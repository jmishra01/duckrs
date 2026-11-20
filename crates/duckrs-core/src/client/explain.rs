// Ported from duckdb/src/common/render_tree.cpp, duckdb/src/common/tree_renderer/
// {text_tree_renderer,json_tree_renderer}.cpp and the ParamsToString methods of the operators
// @ rust-port-baseline
//
//! What EXPLAIN shows. A plan becomes a tree of [`ExplainNode`]s, which render as the boxes of
//! `TextTreeRenderer` (in [`text_tree_renderer`]) or as JSON. A physical plan takes its details from the logical plan it came from, whose
//! expressions still have their names.

use crate::execution::physical::PhysicalOperator;
use crate::optimizer::join_order::PlanEstimate;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundWindow};
use crate::optimizer::range_joins::{range_order, reorder_conditions};
use crate::planner::logical::{
    BoundOrderByNode, JoinAlgorithm, JoinCondition, LogicalOperator, LogicalOperatorKind as K,
};
use duckrs_common::sort_key::OrderType;

use super::text_tree_renderer::{self, TextTreeConfig, TreeNode, split_lines, strip_outer_parens};

/// `RenderTreeNode`.
pub(super) struct ExplainNode {
    name: String,
    /// keys and values; a value can have several lines
    info: Vec<(String, String)>,
    estimate: Option<u64>,
    children: Vec<ExplainNode>,
}

impl ExplainNode {
    /// `PhysicalCTE::ParamsToString` of a CTE whose body runs to completion before the query.
    pub(super) fn dependent_cte(name: &str, body: ExplainNode, query: ExplainNode) -> ExplainNode {
        ExplainNode {
            name: "CTE".to_string(),
            info: vec![
                ("CTE Name".to_string(), name.to_string()),
                ("Execution Mode".to_string(), "PIPELINE_DEPENDENT".to_string()),
                ("Run To Completion".to_string(), "true".to_string()),
            ],
            estimate: None,
            children: vec![body, query],
        }
    }
}

/// `LogicalOperatorToString`.
fn logical_name(op: &LogicalOperator) -> &'static str {
    match &op.kind {
        K::Get { .. } | K::Range { .. } | K::ExpressionGet { .. } | K::ChunkGet { .. } => "SEQ_SCAN",
        K::EmptyResult { .. } => "EMPTY_RESULT",
        K::DummyScan { .. } => "DUMMY_SCAN",
        K::Filter { .. } => "FILTER",
        K::Window { .. } => "WINDOW",
        K::MatchRecognize { .. } => "MATCH_RECOGNIZE",
        K::Aggregate { .. } => "AGGREGATE",
        K::Order { .. } => "ORDER_BY",
        K::TopN { .. } => "TOP_N",
        K::Limit { .. } | K::LimitPercent { .. } => "LIMIT",
        K::Sample { .. } => "SAMPLE",
        K::SecureView { .. } => "SECURE_VIEW",
        K::Distinct { .. } => "DISTINCT",
        K::Unnest { .. } => "UNNEST",
        K::Union { .. } => "UNION",
        K::ExceptIntersect { except: true, .. } => "EXCEPT",
        K::ExceptIntersect { .. } => "INTERSECT",
        K::RecursiveCte { .. } => "REC_CTE",
        K::WorkingTable { .. } | K::CteRef { .. } => "CTE_SCAN",
        K::MaterializedCte { .. } => "CTE",
        K::DelimGet { .. } => "DELIM_GET",
        K::DelimJoin { .. } => "DELIM_JOIN",
        K::LateralJoin { .. } => "DEPENDENT_JOIN",
        K::Projection { .. } | K::ColumnSelect { .. } => "PROJECTION",
        K::CrossProduct => "CROSS_PRODUCT",
        K::AsOfJoin { .. } => "ASOF_JOIN",
        K::PositionalJoin => "POSITIONAL_JOIN",
        K::MarkJoin { .. } | K::Join { .. } => "COMPARISON_JOIN",
        K::Insert { .. } => "INSERT",
        K::Delete { .. } => "DELETE",
        K::Update { .. } => "UPDATE",
        K::MergeInto { .. } => "MERGE_INTO",
        K::CreateTableAs { .. } | K::CreateTable { .. } => "CREATE_TABLE",
        K::CsvScan { .. } => "READ_CSV",
        K::ParquetScan { .. } => "READ_PARQUET",
        K::JsonScan { .. } => "READ_JSON",
        K::CopyToFile { .. } | K::CopyToParquet { .. } => "COPY_TO_FILE",
    }
}

/// The `File Filters` and `Scanning Files` of a multi-file scan whose filters left out files.
fn file_filter_info(file_filters: &str, files: usize, total: Option<usize>) -> Vec<(String, String)> {
    let mut info = Vec::new();
    if !file_filters.is_empty() {
        info.push(("File Filters".to_string(), file_filters.to_string()));
        if let Some(total) = total {
            info.push(("Scanning Files".to_string(), format!("{files}/{total}")));
        }
    }
    info
}

fn lines(values: impl IntoIterator<Item = String>) -> String {
    values.into_iter().collect::<Vec<_>>().join("\n")
}

fn conditions_text(conditions: &[JoinCondition], predicate: Option<String>) -> String {
    let conditions = conditions
        .iter()
        .map(|c| format!("{} {} {}", c.left, c.comparison.operator(), c.right));
    lines(conditions.chain(predicate))
}

fn aggregate_text(aggregate: &BoundAggregate) -> String {
    // the arguments of an aggregate duckrs computes from its rows are packed into one (`a0`, `a1`, ...)
    let children = match aggregate.children.as_slice() {
        [only] if matches!(aggregate.function.kind, crate::function::aggregates::AggregateKind::Holistic(_)) => {
            match &only.kind {
                crate::planner::expression::BoundExprKind::Function { function, children, .. }
                    if function.name == "struct_pack"
                        && children.len() > 1
                        && only.return_type.id() == duckrs_common::types::LogicalTypeId::Struct
                        && only
                            .return_type
                            .struct_children()
                            .iter()
                            .enumerate()
                            .all(|(i, (name, _))| *name == format!("a{i}")) =>
                {
                    children.as_slice()
                }
                _ => aggregate.children.as_slice(),
            }
        }
        _ => aggregate.children.as_slice(),
    };
    let arguments: Vec<String> = children.iter().map(|c| c.to_string()).collect();
    let name = &aggregate.function.name;
    let distinct = if aggregate.distinct { "DISTINCT " } else { "" };
    let text = format!("{name}({distinct}{})", arguments.join(", "));
    match &aggregate.filter {
        Some(filter) => format!("{text} Filter: {filter}"),
        None => text,
    }
}

/// `ExpressionFilter::ToString`: without the parentheses around all of it, if there are any.
/// The filters of a scan as `GetFilterInfo` shows them: a BETWEEN of a column is its two
/// comparisons (`FilterCombiner::GenerateTableScanFilters`), in one conjunction.
fn scan_filter_lines(filters: &[BoundExpr]) -> String {
    use crate::planner::expression::BoundExprKind as E;
    lines(filters.iter().map(|filter| match &filter.kind {
        E::Between { input, lower, upper } if matches!(input.kind, E::ColumnRef { .. }) => {
            format!("({input} >= {lower}) AND ({input} <= {upper})")
        }
        _ => filter_text(filter),
    }))
}

pub(crate) fn filter_text(filter: &BoundExpr) -> String {
    let text = filter.to_string();
    let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return text;
    };
    // (a) OR (b) starts and ends with them too, but those are not one pair
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return text;
        }
    }
    inner.to_string()
}

/// `BoundOrderByNode::ToString`.
fn order_node_text(expr: &BoundExpr, modifiers: duckrs_common::sort_key::OrderModifiers) -> String {
    let order = match modifiers.order {
        OrderType::Descending => "DESC",
        _ => "ASC",
    };
    let nulls = match modifiers.nulls {
        duckrs_common::sort_key::NullOrder::NullsFirst => "NULLS FIRST",
        duckrs_common::sort_key::NullOrder::NullsLast => "NULLS LAST",
    };
    format!("{expr} {order} {nulls}")
}

/// `BoundWindowExpression::ToString` (`WindowExpression::ToString`).
fn window_text(window: &BoundWindow) -> String {
    use crate::planner::expression::WindowFunction as F;
    use duckrs_parser::ast::{WindowBoundary as B, WindowExclude};
    let name = match window.function {
        F::Aggregate => window.aggregate.as_ref().map_or("", |a| a.name.as_str()),
        F::RowNumber => "row_number",
        F::Rank => "rank",
        F::DenseRank => "dense_rank",
        F::PercentRank => "percent_rank",
        F::CumeDist => "cume_dist",
        F::Ntile => "ntile",
        F::Lag => "lag",
        F::Lead => "lead",
        F::FirstValue => "first_value",
        F::LastValue => "last_value",
        F::NthValue => "nth_value",
        F::Fill => "fill",
    };
    let mut result = format!("{name}(");
    let arguments: Vec<String> = window
        .children
        .iter()
        .enumerate()
        .map(|(at, child)| {
            if at == 0 && window.distinct { format!("DISTINCT {child}") } else { child.to_string() }
        })
        .collect();
    result += &arguments.join(", ");
    if !window.arg_orders.is_empty() {
        let orders: Vec<String> = window.arg_orders.iter().map(|(e, m)| order_node_text(e, *m)).collect();
        result += &format!(" ORDER BY {}", orders.join(", "));
    }
    if window.ignore_nulls {
        result += " IGNORE NULLS";
    }
    if let Some(filter) = &window.filter {
        result += &format!(") FILTER (WHERE {filter}");
    }
    result += ") OVER (";
    let mut separator = "";
    if !window.partitions.is_empty() {
        let partitions: Vec<String> = window.partitions.iter().map(|p| p.to_string()).collect();
        result += &format!("PARTITION BY {}", partitions.join(", "));
        separator = " ";
    }
    if !window.orders.is_empty() {
        let orders: Vec<String> = window.orders.iter().map(|(e, m)| order_node_text(e, *m)).collect();
        result += &format!("{separator}ORDER BY {}", orders.join(", "));
        separator = " ";
    }
    let units_of = |boundary: B| match boundary {
        B::CurrentRowRange | B::ExprPrecedingRange | B::ExprFollowingRange => "RANGE",
        B::CurrentRowGroups | B::ExprPrecedingGroups | B::ExprFollowingGroups => "GROUPS",
        _ => "ROWS",
    };
    let expr_text = |expr: &Option<BoundExpr>| expr.as_ref().map(|e| e.to_string()).unwrap_or_default();
    let mut units = "ROWS";
    let mut from = String::new();
    match window.start {
        B::CurrentRowRange | B::CurrentRowRows | B::CurrentRowGroups => {
            from = "CURRENT ROW".to_string();
            units = units_of(window.start);
        }
        B::UnboundedPreceding => {
            if window.end != B::CurrentRowRange {
                from = "UNBOUNDED PRECEDING".to_string();
            }
        }
        B::ExprPrecedingRows | B::ExprPrecedingRange | B::ExprPrecedingGroups => {
            from = format!("{} PRECEDING", expr_text(&window.start_expr));
            units = units_of(window.start);
        }
        B::ExprFollowingRows | B::ExprFollowingRange | B::ExprFollowingGroups => {
            from = format!("{} FOLLOWING", expr_text(&window.start_expr));
            units = units_of(window.start);
        }
        _ => {}
    }
    let mut to = String::new();
    match window.end {
        B::CurrentRowRange => {
            if window.start != B::UnboundedPreceding {
                to = "CURRENT ROW".to_string();
                units = "RANGE";
            }
        }
        B::CurrentRowRows | B::CurrentRowGroups => {
            to = "CURRENT ROW".to_string();
            units = units_of(window.end);
        }
        B::UnboundedPreceding => to = "UNBOUNDED PRECEDING".to_string(),
        B::UnboundedFollowing => to = "UNBOUNDED FOLLOWING".to_string(),
        B::ExprPrecedingRows | B::ExprPrecedingRange | B::ExprPrecedingGroups => {
            to = format!("{} PRECEDING", expr_text(&window.end_expr));
            units = units_of(window.end);
        }
        B::ExprFollowingRows | B::ExprFollowingRange | B::ExprFollowingGroups => {
            to = format!("{} FOLLOWING", expr_text(&window.end_expr));
            units = units_of(window.end);
        }
        B::Invalid => {}
    }
    if window.exclude != WindowExclude::NoOther {
        if from.is_empty() {
            from = "UNBOUNDED PRECEDING".to_string();
        }
        if to.is_empty() {
            to = "CURRENT ROW".to_string();
            units = "RANGE";
        }
    }
    if !from.is_empty() || !to.is_empty() {
        result += &format!("{separator}{units}");
    }
    match (from.is_empty(), to.is_empty()) {
        (false, false) => result += &format!(" BETWEEN {from} AND {to}"),
        (false, true) => result += &format!(" {from}"),
        (true, false) => result += &format!(" {to}"),
        (true, true) => {}
    }
    result += match window.exclude {
        WindowExclude::CurrentRow => " EXCLUDE CURRENT ROW",
        WindowExclude::Group => " EXCLUDE GROUP",
        WindowExclude::Ties => " EXCLUDE TIES",
        WindowExclude::NoOther => "",
    };
    result += ")";
    result
}

fn order_text(orders: &[BoundOrderByNode]) -> String {
    lines(orders.iter().map(|order| {
        let direction = match order.modifiers.order {
            OrderType::Descending => "DESC",
            _ => "ASC",
        };
        format!("{} {direction}", order.expr)
    }))
}

/// A physical aggregate reads its child by position: a column without a name shows as one
/// (`BoundReferenceExpression::ToString`).
fn resolve_unnamed(op: &LogicalOperator) -> Option<LogicalOperator> {
    let K::Aggregate { .. } = &op.kind else {
        return None;
    };
    let bindings = op.children.first()?.column_bindings();
    let mut resolved = LogicalOperator::new(op.kind.clone(), Vec::new());
    let mut changed = false;
    for expr in resolved.expressions_mut() {
        expr.walk_mut(&mut |e| {
            if let crate::planner::expression::BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind
                && e.alias.is_empty()
                && let Some(at) = bindings.iter().position(|b| b == binding)
            {
                e.kind = crate::planner::expression::BoundExprKind::Reference(at);
                changed = true;
            }
        });
    }
    changed.then_some(resolved)
}

/// `ParamsToString`, where physical operators differ from logical ones in some keys.
fn operator_info(op: &LogicalOperator, physical: bool) -> Vec<(String, String)> {
    if physical && let Some(resolved) = resolve_unnamed(op) {
        return operator_info(&resolved, physical);
    }
    let entry = |key: &str, value: String| vec![(key.to_string(), value)];
    let join = |join_type: String, conditions: &[JoinCondition], predicate: Option<String>| {
        let mut info = entry("Join Type", join_type);
        if !conditions.is_empty() || predicate.is_some() {
            info.extend(entry("Conditions", conditions_text(conditions, predicate)));
        }
        info
    };
    match &op.kind {
        K::MaterializedCte { name, table_index, .. } => {
            let mut info = entry("CTE Name", name.clone());
            info.extend(entry("Table Index", table_index.to_string()));
            info
        }
        K::CteRef { cte_index, .. } => entry("CTE Index", cte_index.to_string()),
        // `PhysicalRecursiveCTE::ParamsToString`
        K::RecursiveCte { name, table_index, .. } => {
            let mut info = entry("CTE Name", name.clone());
            info.extend(entry("Table Index", table_index.to_string()));
            info
        }
        K::WorkingTable { working, .. } => match recursive_scan(working) {
            Some((_, cte_index)) => entry("CTE Index", cte_index.to_string()),
            None => Vec::new(),
        },
        // `PhysicalTableScan::ParamsToString` of `read_csv`
        K::CsvScan { scan, .. } => {
            let mut info = entry("Function", "READ_CSV".to_string());
            if let Some(columns) = &scan.projection {
                let mut names = scan.names.clone();
                if scan.filename {
                    names.push("filename".to_string());
                }
                names.extend(scan.hive.iter().map(|(name, _)| name.clone()));
                let projected = columns.iter().map(|&c| names.get(c).cloned().unwrap_or_default());
                info.extend(entry("Projections", lines(projected)));
            }
            info.extend(file_filter_info(&scan.file_filters, scan.files.len(), scan.total_files));
            info
        }
        K::JsonScan { scan, .. } => {
            let mut info = entry("Function", scan.function_name.to_uppercase());
            if let Some(columns) = &scan.projection {
                let names = scan.output_names();
                let projected = columns.iter().map(|&c| names.get(c).cloned().unwrap_or_default());
                info.extend(entry("Projections", lines(projected)));
            }
            info.extend(file_filter_info(&scan.file_filters, scan.files.len(), scan.total_files));
            info
        }
        K::Get {
            table,
            table_index,
            column_ids,
            filters,
            optional_filters,
            projection,
            sample,
            extracts,
            ..
        } => {
            // `PhysicalTableScan::GetFilterInfo`: a filter of a column by the column's name
            let rename = |filter: &BoundExpr| {
                let mut filter = filter.clone();
                filter.walk_mut(&mut |e| {
                    if let crate::planner::expression::BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind
                        && binding.table_index == *table_index
                        && let Some(column) = column_ids.get(binding.column_index)
                        && let Some(definition) = table.columns.get(*column)
                    {
                        e.alias = definition.name.clone();
                    }
                });
                filter
            };
            let renamed: Vec<BoundExpr> = filters.iter().map(rename).collect();
            let filters = &renamed;
            // `FormatOptionalFilterString`
            let filter_text = {
                let mut text: Vec<String> = Vec::new();
                if !filters.is_empty() {
                    text.push(scan_filter_lines(filters));
                }
                text.extend(
                    optional_filters
                        .iter()
                        .map(|f| format!("optional: {}", scan_filter_lines(&[rename(f)]))),
                );
                lines(text)
            };
            // `LogicalGet::ParamsToString` has the filters, then what `TableScanToString` adds
            if !physical {
                let mut info = Vec::new();
                if !filter_text.is_empty() {
                    info.extend(entry("Filters", filter_text));
                }
                info.extend(entry("Table", table.qualified_name()));
                info.extend(entry("Type", "Sequential Scan".to_string()));
                return info;
            }
            let mut info = entry("Table", table.qualified_name());
            info.extend(entry("Type", "Sequential Scan".to_string()));
            let name = |&column: &usize| {
                if column == crate::planner::logical::COLUMN_IDENTIFIER_ROW_NUMBER {
                    return "row_number".to_string();
                }
                let name = table.columns.get(column).map(|c| c.name.as_str());
                name.unwrap_or("rowid").to_string()
            };
            // a pushed field by its path, `column.a.b`
            let name_at = |p: usize| match extracts.get(p) {
                Some(Some(extract)) => format!("{}.{}", name(&column_ids[p]), extract.names.join(".")),
                _ => name(&column_ids[p]),
            };
            let projected: Vec<String> = match projection {
                Some(positions) => positions.iter().map(|&p| name_at(p)).collect(),
                None => (0..column_ids.len()).map(name_at).collect(),
            };
            info.extend(entry("Projections", lines(projected)));
            if !filter_text.is_empty() {
                info.extend(entry("Filters", filter_text));
            }
            // `PhysicalTableScan::ParamsToString`
            if let Some(sample) = sample {
                let text = if sample.options.is_percentage {
                    let value = sample.options.sample_size.parse::<f64>().unwrap_or(0.0);
                    let value = if value.fract() == 0.0 { format!("{value:.1}") } else { value.to_string() };
                    format!("System: {value}%")
                } else {
                    format!("System: {} rows", sample.options.sample_size)
                };
                info.extend(entry("Sample Method", text));
            }
            info
        }
        K::ParquetScan { scan, .. } => {
            // `PhysicalTableScan::ParamsToString` of a function without `to_string`
            let mut info = entry("Function", scan.function_name.to_uppercase());
            let mut names: Vec<String> = scan.names.clone();
            if scan.file_row_number {
                names.push("file_row_number".to_string());
            }
            if scan.filename {
                names.push("filename".to_string());
            }
            names.extend(scan.hive.iter().map(|(name, _)| name.clone()));
            names.extend(scan.virtual_columns.iter().map(|c| c.name().to_string()));
            let positions: Vec<usize> = (0..names.len())
                .filter(|at| {
                    scan.projected
                        .as_ref()
                        .is_none_or(|p| p.get(*at).copied().unwrap_or(true))
                })
                .collect();
            // `column_ids` are in the order the binder read the columns
            let projected = scan.column_order.arrange(&positions).into_iter().map(|at| names[at].clone());
            info.extend(entry("Projections", lines(projected)));
            if !scan.filters.is_empty() {
                info.extend(entry("Filters", lines(scan.filters.iter().map(filter_text))));
            }
            if !scan.file_filters.is_empty() {
                info.extend(entry("File Filters", scan.file_filters.clone()));
                if let Some(total) = scan.total_files {
                    info.extend(entry("Scanning Files", format!("{}/{total}", scan.files.len())));
                }
            }
            info
        }
        K::Filter { expressions } => {
            let key = if physical { "Expression" } else { "Expressions" };
            entry(key, lines(expressions.iter().map(filter_text)))
        }
        K::Projection { expressions, .. } => {
            if physical {
                entry("Projections", lines(expressions.iter().map(|e| e.to_string())))
            } else {
                // `LogicalOperator::ParamsToString`: `Expression::GetName`, the alias where there is one
                let name = |e: &BoundExpr| if e.alias.is_empty() { e.to_string() } else { e.alias.clone() };
                entry("Expressions", lines(expressions.iter().map(name)))
            }
        }
        K::Aggregate { groups, aggregates, .. } => {
            let mut info = Vec::new();
            if !groups.is_empty() {
                info.extend(entry("Groups", lines(groups.iter().map(|g| g.to_string()))));
            }
            if !aggregates.is_empty() {
                let key = if physical { "Aggregates" } else { "Expressions" };
                info.extend(entry(key, lines(aggregates.iter().map(aggregate_text))));
            }
            info
        }
        K::Order { orders } => entry("Order By", order_text(orders)),
        K::TopN { orders, limit, offset } => {
            let mut info = entry("Top", limit.to_string());
            if *offset > 0 {
                info.extend(entry("Offset", offset.to_string()));
            }
            info.extend(entry("Order By", order_text(orders)));
            info
        }
        K::Limit { limit, offset, .. } => {
            let mut info = Vec::new();
            if let Some(limit) = limit {
                info.extend(entry("Limit", limit.to_string()));
            }
            if *offset != 0 {
                info.extend(entry("Offset", offset.to_string()));
            }
            info
        }
        // `PhysicalComparisonJoin` orders its conditions: equalities first, and for range joins the
        // ranges first (`ReorderConditions`, `PhysicalRangeJoin`)
        K::Join {
            join_type,
            conditions,
            predicate,
            algorithm,
        } if physical && !conditions.is_empty() => {
            let mut conditions = conditions.clone();
            match algorithm {
                JoinAlgorithm::Hash | JoinAlgorithm::NestedLoop => reorder_conditions(&mut conditions),
                JoinAlgorithm::PiecewiseMerge | JoinAlgorithm::IeJoin => range_order(&mut conditions),
            }
            join(
                join_type.name().to_string(),
                &conditions,
                predicate.as_ref().map(|p| p.to_string()),
            )
        }
        K::Join {
            join_type,
            conditions,
            predicate,
            ..
        }
        | K::DelimJoin {
            join_type,
            conditions,
            predicate,
            ..
        } => join(
            join_type.name().to_string(),
            conditions,
            predicate.as_ref().map(|p| p.to_string()),
        ),
        K::MarkJoin {
            conditions, predicate, ..
        } => join(
            "MARK".to_string(),
            conditions,
            predicate.as_ref().map(|p| p.to_string()),
        ),
        K::AsOfJoin { join_type, .. } => entry("Join Type", join_type.name().to_string()),
        // `PhysicalStreamingSample::ParamsToString` and `PhysicalReservoirSample::ParamsToString`
        K::Sample { options } => {
            use duckrs_parser::ast::SampleMethod;
            let size = options.sample_size.parse::<f64>().unwrap_or(0.0);
            match options.method {
                SampleMethod::Reservoir => {
                    let value = if size.fract() == 0.0 && options.is_percentage {
                        format!("{size:.1}")
                    } else {
                        options.sample_size.clone()
                    };
                    let unit = if options.is_percentage { "%" } else { " rows" };
                    entry("Sample Size", format!("{value}{unit}"))
                }
                method => {
                    let name = if method == SampleMethod::System { "System" } else { "Bernoulli" };
                    if options.is_percentage {
                        entry("Sample Method", format!("{name}: {size:.6}%"))
                    } else {
                        entry("Sample Method", format!("{name}: {} rows", options.sample_size))
                    }
                }
            }
        }
        // `PhysicalWindow::ParamsToString`
        K::Window { windows, .. } => entry("Projections", lines(windows.iter().map(window_text))),
        _ => Vec::new(),
    }
}

/// The tree of a logical plan.
pub(super) fn logical_tree(op: &LogicalOperator, estimate: Option<&PlanEstimate>) -> ExplainNode {
    let aligned = estimate.filter(|e| e.children.len() == op.children.len());
    // C++ has this inside of the operators, so plans do not show it
    if matches!(op.kind, K::ColumnSelect { .. }) {
        return logical_tree(&op.children[0], aligned.map(|e| &e.children[0]));
    }
    ExplainNode {
        name: logical_name(op).to_string(),
        info: operator_info(op, false),
        estimate: estimate.map(|e| e.cardinality),
        children: op
            .children
            .iter()
            .enumerate()
            .map(|(at, child)| logical_tree(child, aligned.map(|e| &e.children[at])))
            .collect(),
    }
}

thread_local! {
    /// The working and recurring tables of the recursive CTEs being rendered, and their table
    /// indexes.
    static RECURSIVE_TABLES: std::cell::RefCell<Vec<(usize, usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// What a scan of `table` is: `REC_CTE_SCAN` of a working table or `REC_REC_CTE_SCAN` of a
/// recurring one, with the index of its CTE.
fn recursive_scan(table: &std::sync::Arc<parking_lot::Mutex<duckrs_common::column_data::ColumnDataCollection>>) -> Option<(&'static str, usize)> {
    let pointer = std::sync::Arc::as_ptr(table) as *const () as usize;
    RECURSIVE_TABLES.with(|tables| {
        tables.borrow().iter().rev().find_map(|&(working, recurring, index)| {
            if pointer == working {
                Some(("REC_CTE_SCAN", index))
            } else if pointer == recurring {
                Some(("REC_REC_CTE_SCAN", index))
            } else {
                None
            }
        })
    })
}

/// The tree of a physical plan, and the logical plan it came from if they have the same shape.
pub(super) fn physical_tree(
    op: &dyn PhysicalOperator,
    logical: Option<&LogicalOperator>,
    estimate: Option<&PlanEstimate>,
    analyze: bool,
) -> ExplainNode {
    let children = op.children();
    // `PhysicalRecursiveCTE`: its recursive part is planned for each round, so it is planned here
    // once more to show
    if let Some(
        cte @ LogicalOperator {
            kind:
                K::RecursiveCte {
                    working,
                    recurring,
                    table_index,
                    recursive,
                    unresolved,
                    ..
                },
            children: logical_children,
        },
    ) = logical
        && op.name() == "REC_CTE"
        && children.len() == 1
        && logical_children.len() == 1
    {
        let child_estimate = |at: usize| estimate.and_then(|e| e.children.get(at));
        let anchor = physical_tree(children[0].as_ref(), Some(&logical_children[0]), child_estimate(0), analyze);
        let pointer = |table: &std::sync::Arc<_>| std::sync::Arc::as_ptr(table) as *const () as usize;
        RECURSIVE_TABLES.with(|t| t.borrow_mut().push((pointer(working), pointer(recurring), *table_index)));
        let plan = crate::execution::create_plan((**recursive).clone());
        let rounds = physical_tree(plan.as_ref(), Some(unresolved), child_estimate(1), analyze);
        RECURSIVE_TABLES.with(|t| t.borrow_mut().pop());
        return ExplainNode {
            name: op.name().to_string(),
            info: operator_info(cte, true),
            estimate: estimate.map(|e| e.cardinality),
            children: vec![anchor, rounds],
        };
    }
    // a column select that went into the projection below it has no physical operator
    if let Some(LogicalOperator {
        kind: K::ColumnSelect { .. },
        children: logical_children,
    }) = logical
        && op.name() != "COLUMN_SELECT"
        && logical_children.len() == 1
    {
        let estimate = estimate.filter(|e| e.children.len() == 1).map(|e| &e.children[0]);
        return physical_tree(op, Some(&logical_children[0]), estimate, analyze);
    }
    // a projection that only passes its child's columns on has no physical operator
    if let Some(LogicalOperator {
        kind: K::Projection { .. },
        children: logical_children,
    }) = logical
        && op.name() != "PROJECTION"
        && logical_children.len() == 1
    {
        let estimate = estimate.filter(|e| e.children.len() == 1).map(|e| &e.children[0]);
        return physical_tree(op, Some(&logical_children[0]), estimate, analyze);
    }
    // duckrs runs DISTINCT aggregates as a grouping by the argument below the aggregate, which C++
    // does inside the one operator
    if !analyze
        && let Some(
            aggregate @ LogicalOperator {
                kind:
                    K::Aggregate {
                        groups,
                        aggregates,
                        grouping_sets,
                        grouping_functions,
                        ..
                    },
                children: logical_children,
            },
        ) = logical
        && logical_children.len() == 1
        && children.len() == 1
        && children[0].name() == op.name()
        && children[0].children().len() == 1
        && crate::execution::distinct_in_two_steps(groups, aggregates, grouping_sets, grouping_functions).is_some()
    {
        let estimate = estimate.filter(|e| e.children.len() == 1);
        return ExplainNode {
            name: op.name().to_string(),
            info: operator_info(aggregate, true),
            estimate: estimate.map(|e| e.cardinality),
            children: vec![physical_tree(
                children[0].children()[0].as_ref(),
                Some(&logical_children[0]),
                estimate.map(|e| &e.children[0]),
                analyze,
            )],
        };
    }
    // `CreatePlan(LogicalSample)` puts a LIMIT over a SYSTEM sample of rows
    if let Some(LogicalOperator {
        kind: K::Sample { options },
        ..
    }) = logical
        && op.name() == "LIMIT"
        && children.len() == 1
    {
        return ExplainNode {
            name: op.name().to_string(),
            info: vec![("Limit".to_string(), options.sample_size.clone())],
            estimate: estimate.map(|e| e.cardinality),
            children: vec![physical_tree(children[0].as_ref(), logical, estimate, analyze)],
        };
    }
    // `CreatePlan(LogicalWindow)` puts a projection over the window operators it makes, one per
    // group of windows, all of which come from the one logical window
    if let Some(
        window @ LogicalOperator {
            kind: K::Window { .. }, ..
        },
    ) = logical
        && children.len() == 1
        && (op.name() == "PROJECTION" || matches!(children[0].name(), "WINDOW" | "STREAMING_WINDOW"))
    {
        let info = if op.name() == "PROJECTION" {
            let references = (0..op.types().len()).map(|i| format!("#{i}"));
            vec![("Projections".to_string(), lines(references))]
        } else {
            operator_info(window, true)
        };
        return ExplainNode {
            name: op.name().to_string(),
            info,
            estimate: estimate.map(|e| e.cardinality),
            children: vec![physical_tree(children[0].as_ref(), Some(window), estimate, analyze)],
        };
    }
    let logical = logical.filter(|l| l.children.len() == children.len());
    let estimate = estimate.filter(|e| e.children.len() == children.len());
    if op.name() == "COLUMN_SELECT" {
        return physical_tree(
            children[0].as_ref(),
            logical.map(|l| &l.children[0]),
            estimate.map(|e| &e.children[0]),
            analyze,
        );
    }
    let name = match logical.map(|l| &l.kind) {
        Some(K::Get { .. }) => "SEQ_SCAN".to_string(),
        // `PhysicalTableScan::GetName`
        Some(K::ParquetScan { scan, .. }) => scan.function_name.to_uppercase(),
        Some(K::JsonScan { scan, .. }) => scan.function_name.to_uppercase(),
        Some(K::WorkingTable { working, .. }) => match recursive_scan(working) {
            Some((name, _)) => name.to_string(),
            None => op.name().to_string(),
        },
        _ => op.name().to_string(),
    };
    let mut info: Vec<(String, String)> = match logical {
        Some(logical) => operator_info(logical, true),
        None => op.explain_details().into_iter().map(|d| (String::new(), d)).collect(),
    };
    // C++ only knows when the scan has started
    if analyze
        && let Some(K::Get {
            index_rows: Some(_), ..
        }) = logical.map(|l| &l.kind)
        && let Some((_, kind)) = info.iter_mut().find(|(key, _)| key == "Type")
    {
        *kind = "Index Scan".to_string();
    }
    if analyze && let Some((scanned, total)) = op.row_groups_scanned() {
        info.push(("Row Groups Scanned".to_string(), format!("{scanned} / {total}")));
    }
    ExplainNode {
        name,
        info,
        estimate: estimate.map(|e| e.cardinality),
        children: children
            .iter()
            .enumerate()
            .map(|(at, child)| {
                physical_tree(
                    child.as_ref(),
                    logical.map(|l| &l.children[at]),
                    estimate.map(|e| &e.children[at]),
                    analyze,
                )
            })
            .collect(),
    }
}

/// `Dynamic Filters` of the table scans, from a run of the statement, in the order of the scans.
pub(super) fn add_dynamic_filters(node: &mut ExplainNode, recorded: &mut impl Iterator<Item = Option<String>>) {
    if matches!(node.name.as_str(), "SEQ_SCAN" | "TABLE_SCAN")
        && let Some(Some(filters)) = recorded.next()
    {
        let at = node
            .info
            .iter()
            .position(|(key, _)| key == "Row Groups Scanned")
            .unwrap_or(node.info.len());
        node.info.insert(at, ("Dynamic Filters".to_string(), filters));
    }
    for child in &mut node.children {
        add_dynamic_filters(child, recorded);
    }
}

/// `BuildExplainTree`: what the text renderer shows of a node. A detail without a key is `Text`.
fn text_tree(node: &ExplainNode) -> TreeNode {
    let details = node
        .info
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| {
            let values = split_lines(value).into_iter().map(strip_outer_parens).collect();
            let key = if key.is_empty() {
                "Text".to_string()
            } else {
                key.clone()
            };
            (key, values)
        })
        .collect();
    TreeNode {
        name: node.name.clone(),
        details,
        rows: None,
        estimated_rows: node.estimate,
        children: node.children.iter().map(text_tree).collect(),
    }
}

fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn render_json_node(node: &ExplainNode, depth: usize, out: &mut String) {
    let pad = "    ".repeat(depth);
    let inner = "    ".repeat(depth + 1);
    out.push_str(&pad);
    out.push_str("{\n");
    out.push_str(&inner);
    out.push_str("\"name\": ");
    json_string(&node.name, out);
    out.push_str(",\n");
    out.push_str(&inner);
    if node.children.is_empty() {
        out.push_str("\"children\": [],\n");
    } else {
        out.push_str("\"children\": [\n");
        for (at, child) in node.children.iter().enumerate() {
            if at > 0 {
                out.push_str(",\n");
            }
            render_json_node(child, depth + 2, out);
        }
        out.push('\n');
        out.push_str(&inner);
        out.push_str("],\n");
    }
    out.push_str(&inner);
    out.push_str("\"extra_info\": {");
    let estimate = node
        .estimate
        .map(|e| ("Estimated Cardinality".to_string(), e.to_string()));
    let entries: Vec<&(String, String)> = node.info.iter().chain(estimate.iter()).collect();
    for (at, (key, value)) in entries.iter().enumerate() {
        out.push_str(if at == 0 { "\n" } else { ",\n" });
        out.push_str(&inner);
        out.push_str("    ");
        json_string(if key.is_empty() { "Details" } else { key }, out);
        out.push_str(": ");
        let split: Vec<&str> = value.lines().collect();
        if split.len() > 1 {
            out.push('[');
            for (at, line) in split.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                json_string(line, out);
            }
            out.push(']');
        } else {
            json_string(value, out);
        }
    }
    if !entries.is_empty() {
        out.push('\n');
        out.push_str(&inner);
    }
    out.push_str("}\n");
    out.push_str(&pad);
    out.push('}');
}

/// How EXPLAIN shows a plan: `format` is that of the statement, and `upper` is `operator_casing`.
pub(super) fn render(node: &ExplainNode, format: &str, upper: bool) -> String {
    let mut out = String::new();
    if format == "json" {
        out.push_str("[\n");
        render_json_node(node, 1, &mut out);
        out.push_str("\n]");
    } else {
        let config = TextTreeConfig {
            upper_case: upper,
            ..TextTreeConfig::default()
        };
        out = text_tree_renderer::render(&text_tree(node), &config);
    }
    out
}
