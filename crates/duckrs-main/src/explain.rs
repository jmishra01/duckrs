// Ported from duckdb/src/common/render_tree.cpp, duckdb/src/common/tree_renderer/
// {text_tree_renderer,json_tree_renderer}.cpp and the ParamsToString methods of the operators
// @ rust-port-baseline
//
//! What EXPLAIN shows. A plan becomes a tree of [`ExplainNode`]s, which render as the boxes of
//! `TextTreeRenderer` (in [`text_tree_renderer`]) or as JSON. A physical plan takes its details from the logical plan it came from, whose
//! expressions still have their names.

use duckrs_core::planner::expression::filter_text;
use duckrs_core::execution::physical::PhysicalOperator;
use duckrs_core::optimizer::join_order::PlanEstimate;
use duckrs_core::planner::expression::{BoundAggregate, BoundExpr, BoundWindow};
use duckrs_core::optimizer::range_joins::{range_order, reorder_conditions};
use duckrs_core::planner::logical::{
    BoundOrderByNode, JoinAlgorithm, JoinCondition, LogicalOperator, LogicalOperatorKind as K,
};
use duckrs_common::sort_key::OrderType;

use super::text_tree_renderer::{self, TextTreeConfig, TreeNode, split_lines, strip_outer_parens};

/// `RenderTreeNode`.
#[derive(Debug)]
pub(super) struct ExplainNode {
    name: String,
    /// keys and values; a value can have several lines
    info: Vec<(String, String)>,
    estimate: Option<u64>,
    /// the rows the operator gave on, of a run of EXPLAIN ANALYZE
    rows: Option<u64>,
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
            rows: None,
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
        [only] if matches!(aggregate.function.kind, duckrs_core::function::aggregates::AggregateKind::Holistic(_)) => {
            match &only.kind {
                duckrs_core::planner::expression::BoundExprKind::Function { function, children, .. }
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

/// The filters of a scan as `GetFilterInfo` shows them (`ExpressionFilter::ToString`, without the parentheses
/// around all of it): a BETWEEN of a column is its two comparisons (`FilterCombiner::GenerateTableScanFilters`), in
/// one conjunction, and some of the filters may be optional (`OptionalFilterScalarFun::ToString`).
fn scan_filter_lines_marked(filters: &[(BoundExpr, bool)]) -> String {
    use duckrs_core::planner::expression::BoundExprKind as E;
    let text = |filter: &BoundExpr| match &filter.kind {
        E::Between { input, lower, upper } if matches!(input.kind, E::ColumnRef { .. }) => {
            format!("({input} >= {lower}) AND ({input} <= {upper})")
        }
        _ => filter_text(filter),
    };
    // `TableFilterSet`: one filter per column, the filters of a column a conjunction of them
    let column = |filter: &BoundExpr| {
        let mut columns = std::collections::BTreeSet::new();
        filter.clone().walk_mut(&mut |e| {
            if let E::ColumnRef { binding, depth: 0 } = &e.kind {
                columns.insert(*binding);
            }
        });
        (columns.len() == 1).then(|| columns.into_iter().next()).flatten()
    };
    // an OR among them keeps its parentheses; the same filter twice is pushed once
    let whole = |(filter, optional): &(BoundExpr, bool)| {
        if *optional {
            format!("optional: {}", text(filter))
        } else {
            text(filter)
        }
    };
    let part = |(filter, optional): &(BoundExpr, bool)| match (&filter.kind, optional) {
        (_, true) => format!("optional: {}", text(filter)),
        (E::Conjunction { and: false, .. }, false) => format!("({})", text(filter)),
        _ => text(filter),
    };
    #[allow(clippy::type_complexity, reason = "per column, its filters")]
    let mut groups: Vec<(Option<duckrs_core::planner::expression::ColumnBinding>, Vec<&(BoundExpr, bool)>)> = Vec::new();
    for filter in filters {
        let key = column(&filter.0);
        match groups.iter_mut().find(|(k, _)| key.is_some() && *k == key) {
            Some((_, members)) => {
                if !members.iter().any(|m| part(m) == part(filter)) {
                    members.push(filter);
                }
            }
            None => groups.push((key, vec![filter])),
        }
    }
    lines(groups.into_iter().map(|(_, members)| match members.as_slice() {
        [only] => whole(only),
        _ => members.iter().map(|m| part(m)).collect::<Vec<_>>().join(" AND "),
    }))
}

/// The fields of STRUCT columns a filter reads, as C++ shows them: of a column whose field the scan reads in its
/// place (`IsPushdownExtract`), the filter is one on that field, named by its path (`s.a`); otherwise the key is the
/// field's name (`struct_extract(s, 'a')`).
fn with_field_names(filter: &BoundExpr, extracted: &dyn Fn(&duckrs_core::planner::expression::ColumnBinding) -> bool) -> BoundExpr {
    use duckrs_core::planner::expression::BoundExprKind as E;
    fn path(expr: &BoundExpr) -> Option<(duckrs_core::planner::expression::ColumnBinding, String)> {
        match &expr.kind {
            E::ColumnRef { binding, depth: 0 } => Some((*binding, expr.alias.clone())),
            E::Function { function, children, .. } if function.name == "struct_extract" && children.len() == 2 => {
                let field = field_name(&children[0], &children[1])?;
                let (binding, base) = path(&children[0])?;
                (!base.is_empty()).then(|| (binding, format!("{base}.{field}")))
            }
            _ => None,
        }
    }
    fn field_name(child: &BoundExpr, key: &BoundExpr) -> Option<String> {
        let E::Constant(index) = &key.kind else {
            return None;
        };
        let index = usize::try_from(index.try_to_string().ok()?.parse::<i64>().ok()?).ok()?;
        let field = child.return_type.struct_children().get(index)?.0.clone();
        (!field.is_empty()).then_some(field)
    }
    let mut copy = filter.clone();
    copy.walk_mut(&mut |e| {
        let E::Function { function, children, .. } = &mut e.kind else {
            return;
        };
        if function.name != "struct_extract" || children.len() != 2 {
            return;
        }
        match path(e) {
            Some((binding, name)) if extracted(&binding) => {
                *e = BoundExpr::column_ref(binding, e.return_type.clone(), name);
            }
            _ => {
                if let E::Function { children, .. } = &mut e.kind
                    && let Some(field) = field_name(&children[0], &children[1])
                {
                    children[1] = BoundExpr::constant(duckrs_common::value::Value::varchar(field));
                }
            }
        }
    });
    copy
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
    use duckrs_core::planner::expression::WindowFunction as F;
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
        B::UnboundedPreceding if window.end != B::CurrentRowRange => {
            from = "UNBOUNDED PRECEDING".to_string();
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
            if let duckrs_core::planner::expression::BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind
                && e.alias.is_empty()
                && let Some(at) = bindings.iter().position(|b| b == binding)
            {
                e.kind = duckrs_core::planner::expression::BoundExprKind::Reference(at);
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
        // `PhysicalSecureView::ParamsToString`
        K::SecureView {
            view_name,
            pushed_filters,
            ..
        } => {
            let mut info = entry("View", view_name.clone());
            if !pushed_filters.is_empty() {
                info.extend(entry("Filters", pushed_filters.join("\n")));
            }
            info
        }
        // `PhysicalTableScan::ParamsToString` of a table function without `to_string`
        K::ExpressionGet {
            function: Some(function), ..
        } if physical => entry("Function", function.to_uppercase()),
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
                    if let duckrs_core::planner::expression::BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind
                        && binding.table_index == *table_index
                        && let Some(column) = column_ids.get(binding.column_index)
                        && let Some(definition) = table.columns.get(*column)
                    {
                        e.alias = definition.name.clone();
                    }
                });
                // the scan reads fields of the column, not all of it
                let extracted = |binding: &duckrs_core::planner::expression::ColumnBinding| {
                    let Some(column) = column_ids.get(binding.column_index).filter(|_| binding.table_index == *table_index)
                    else {
                        return false;
                    };
                    let positions: Vec<usize> = (0..column_ids.len()).filter(|&at| column_ids[at] == *column).collect();
                    let is_extract = |at: &usize| extracts.get(*at).is_some_and(|extract| extract.is_some());
                    positions.iter().any(is_extract)
                        && positions.iter().all(|at| is_extract(at) || *at == binding.column_index)
                };
                with_field_names(&filter, &extracted)
            };
            let renamed: Vec<BoundExpr> = filters.iter().map(rename).collect();
            let filters = &renamed;
            // `FormatOptionalFilterString`
            let filter_text = {
                let marked: Vec<(BoundExpr, bool)> = filters
                    .iter()
                    .map(|f| (f.clone(), false))
                    .chain(optional_filters.iter().map(|f| (rename(f), true)))
                    .collect();
                scan_filter_lines_marked(&marked)
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
                if column == duckrs_core::planner::logical::COLUMN_IDENTIFIER_ROW_NUMBER {
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
            // the row id is no column of the table, which C++ does not list
            let positions: Vec<usize> = match projection {
                Some(positions) => positions.clone(),
                None => (0..column_ids.len()).collect(),
            };
            let projected: Vec<String> = positions
                .into_iter()
                .filter(|&p| column_ids[p] != duckrs_core::planner::logical::COLUMN_IDENTIFIER_ROW_ID)
                .map(name_at)
                .collect();
            if !projected.is_empty() {
                info.extend(entry("Projections", lines(projected)));
            }
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
            let base_count = names.len();
            // a pushed field by its path, `column.a.b`, where its column is
            for (base, extract) in &scan.extracts {
                names.push(format!("{}.{}", names[*base], extract.names.join(".")));
            }
            let is_projected =
                |at: usize| scan.projected.as_ref().is_none_or(|p| p.get(at).copied().unwrap_or(true));
            let positions: Vec<usize> = (0..base_count)
                .filter(|&at| is_projected(at) || scan.extracts.iter().any(|(base, _)| *base == at))
                .collect();
            // `column_ids` are in the order the binder read the columns
            let mut order = Vec::new();
            for at in scan.column_order.arrange(&positions) {
                if is_projected(at) {
                    order.push(at);
                }
                for (i, (base, _)) in scan.extracts.iter().enumerate() {
                    if *base == at && is_projected(base_count + i) {
                        order.push(base_count + i);
                    }
                }
            }
            let projected = order.into_iter().map(|at| names[at].clone());
            info.extend(entry("Projections", lines(projected)));
            if !scan.filters.is_empty() || !scan.optional_filters.is_empty() {
                let marked: Vec<(BoundExpr, bool)> = scan
                    .filters
                    .iter()
                    .map(|f| (f.clone(), false))
                    .chain(scan.optional_filters.iter().map(|f| (f.clone(), true)))
                    .collect();
                info.extend(entry("Filters", scan_filter_lines_marked(&marked)));
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
        K::TopN {
            orders, limit, offset, ..
        } => {
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
    // `HidesChildren`: nothing of what is inside a secure view is shown
    let hidden = matches!(op.kind, K::SecureView { .. });
    let mut children: Vec<ExplainNode> = op
        .children
        .iter()
        .enumerate()
        .filter(|_| !hidden)
        .map(|(at, child)| logical_tree(child, aligned.map(|e| &e.children[at])))
        .collect();
    // `LogicalRecursiveCTE`: the recursive part is its second child, which duckrs keeps apart
    if let K::RecursiveCte { recursive, .. } = &op.kind {
        children.push(logical_tree(recursive, estimate.and_then(|e| e.children.get(1))));
    }
    ExplainNode {
        name: logical_name(op).to_string(),
        info: operator_info(op, false),
        estimate: estimate.map(|e| e.cardinality),
        rows: None,
        children,
    }
}

thread_local! {
    /// The details of each operator of the plan being rendered, by its address, while a profile wants them as its
    /// `extra_info`.
    #[allow(clippy::type_complexity, reason = "the extra info of each operator, by its id")]
    static INFOS: std::cell::RefCell<Option<std::collections::HashMap<usize, Vec<(String, String)>>>> =
        const { std::cell::RefCell::new(None) };
    /// The working and recurring tables of the recursive CTEs being rendered, and their table
    /// indexes.
    static RECURSIVE_TABLES: std::cell::RefCell<Vec<(usize, usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// What the operators of the plan being rendered did when EXPLAIN ANALYZE ran it.
    static ANALYZED: std::cell::RefCell<Option<std::sync::Arc<duckrs_core::execution::profiler::OperatorProfiler>>> =
        const { std::cell::RefCell::new(None) };
}

/// [`physical_tree`] of a plan that ran with `profiler`, whose operators show the rows they gave on.
pub(super) fn analyzed_tree(
    op: &dyn PhysicalOperator,
    logical: Option<&LogicalOperator>,
    estimate: Option<&PlanEstimate>,
    profiler: std::sync::Arc<duckrs_core::execution::profiler::OperatorProfiler>,
) -> ExplainNode {
    let previous = ANALYZED.with(|a| a.borrow_mut().replace(profiler));
    let tree = physical_tree(op, logical, estimate, true);
    ANALYZED.with(|a| *a.borrow_mut() = previous);
    tree
}

/// [`analyzed_tree`], and the details it shows of each operator, by its address.
pub(super) fn analyzed_tree_with_infos(
    op: &dyn PhysicalOperator,
    logical: Option<&LogicalOperator>,
    estimate: Option<&PlanEstimate>,
    profiler: std::sync::Arc<duckrs_core::execution::profiler::OperatorProfiler>,
) -> (ExplainNode, std::collections::HashMap<usize, Vec<(String, String)>>) {
    INFOS.with(|i| *i.borrow_mut() = Some(std::collections::HashMap::new()));
    let tree = analyzed_tree(op, logical, estimate, profiler);
    let infos = INFOS.with(|i| i.borrow_mut().take()).unwrap_or_default();
    (tree, infos)
}

/// `OperatorProfiler`'s `operator_cardinality` of `op`, when the plan ran.
fn analyzed_rows(op: &dyn PhysicalOperator) -> Option<u64> {
    ANALYZED.with(|a| a.borrow().as_ref().map(|profiler| profiler.metrics(op).rows))
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
/// Whether a physical operator is the one a logical operator becomes, by the names EXPLAIN gives them.
fn physical_matches(op: &dyn PhysicalOperator, logical: &LogicalOperator) -> bool {
    let physical = op.name();
    let logical = logical_name(logical);
    physical == logical || (matches!(physical, "TABLE_SCAN" | "SEQ_SCAN") && matches!(logical, "SEQ_GET" | "TABLE_SCAN" | "SEQ_SCAN" | "GET"))
}

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
        let plan = duckrs_core::execution::create_plan((**recursive).clone());
        let rounds = physical_tree(plan.as_ref(), Some(unresolved), child_estimate(1), analyze);
        RECURSIVE_TABLES.with(|t| t.borrow_mut().pop());
        return ExplainNode {
            name: op.name().to_string(),
            info: operator_info(cte, true),
            estimate: estimate.map(|e| e.cardinality),
            rows: analyzed_rows(op),
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
        && duckrs_core::execution::distinct_in_two_steps(groups, aggregates, grouping_sets, grouping_functions).is_some()
    {
        let estimate = estimate.filter(|e| e.children.len() == 1);
        return ExplainNode {
            name: op.name().to_string(),
            info: operator_info(aggregate, true),
            estimate: estimate.map(|e| e.cardinality),
            rows: analyzed_rows(op),
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
            rows: analyzed_rows(op),
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
            rows: analyzed_rows(op),
            children: vec![physical_tree(children[0].as_ref(), Some(window), estimate, analyze)],
        };
    }
    // a projection the physical plan adds where the logical plan has none (the keys of an ORDER BY
    // over a scan's columns, say): its own references, above what the logical operator becomes
    if let Some(l) = logical
        && op.name() == "PROJECTION"
        && children.len() == 1
        && !matches!(l.kind, K::Projection { .. } | K::ColumnSelect { .. })
        && children[0].name() != "PROJECTION"
        && physical_matches(children[0].as_ref(), l)
    {
        let references = (0..op.types().len()).map(|i| format!("#{i}"));
        return ExplainNode {
            name: op.name().to_string(),
            info: vec![("Projections".to_string(), lines(references))],
            estimate: estimate.map(|e| e.cardinality),
            rows: analyzed_rows(op),
            children: vec![physical_tree(children[0].as_ref(), logical, estimate, analyze)],
        };
    }
    let logical = logical.filter(|l| l.children.len() == children.len());
    let estimate = estimate.filter(|e| e.children.len() == children.len());
    if op.name() == "COLUMN_SELECT" {
        // the column select of a logical one, or one the physical plan adds above the operator
        let selected = logical.filter(|l| matches!(l.kind, K::ColumnSelect { .. }));
        return match selected {
            Some(l) => physical_tree(children[0].as_ref(), Some(&l.children[0]), estimate.map(|e| &e.children[0]), analyze),
            None => physical_tree(children[0].as_ref(), logical, estimate, analyze),
        };
    }
    let ran = ANALYZED.with(|a| a.borrow().is_some());
    let name = match logical.map(|l| &l.kind) {
        // the profiler names an operator by its `PhysicalOperatorType`, and a scan of a table or a table
        // function is a TABLE_SCAN
        Some(
            K::Get { .. }
            | K::ParquetScan { .. }
            | K::JsonScan { .. }
            | K::CsvScan { .. }
            | K::ExpressionGet { function: Some(_), .. },
        ) if ran => "TABLE_SCAN".to_string(),
        _ if ran && duckrs_core::execution::profiler::operator_type(op) == "TABLE_SCAN" => "TABLE_SCAN".to_string(),
        Some(K::Get { .. }) => "SEQ_SCAN".to_string(),
        // `PhysicalTableScan::GetName`
        Some(K::ParquetScan { scan, .. }) => scan.function_name.to_uppercase(),
        Some(K::JsonScan { scan, .. }) => scan.function_name.to_uppercase(),
        Some(K::ExpressionGet { function: Some(function), .. }) => function.to_uppercase(),
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
    // the filters the plan gave a scan once it was planned (a Top-N's), after those it had
    let pushed = op.planned_filters();
    if !pushed.is_empty() {
        match info.iter_mut().find(|(key, _)| key == "Filters") {
            Some((_, filters)) => {
                for line in pushed {
                    filters.push('\n');
                    filters.push_str(&line);
                }
            }
            None => {
                let at = info
                    .iter()
                    .position(|(key, _)| !matches!(key.as_str(), "Table" | "Type" | "Function" | "Projections"))
                    .unwrap_or(info.len());
                info.insert(at, ("Filters".to_string(), pushed.join("\n")));
            }
        }
    }
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
    INFOS.with(|infos| {
        if let Some(infos) = infos.borrow_mut().as_mut() {
            // the profile shows the estimate with the other details (`ParamsToString`)
            let mut details = info.clone();
            if let Some(estimate) = estimate {
                details.push(("Estimated Cardinality".to_string(), estimate.cardinality.to_string()));
            }
            infos.insert(duckrs_core::execution::profiler::key(op), details);
        }
    });
    ExplainNode {
        name,
        info,
        estimate: estimate.map(|e| e.cardinality),
        rows: analyzed_rows(op),
        // `HidesChildren`: nothing of what is inside a secure view is shown
        children: children
            .iter()
            .enumerate()
            .filter(|_| op.name() != "SECURE_VIEW")
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
    // of the scans of tables and Parquet files, not of other table functions, which are TABLE_SCANs too once
    // the plan ran
    let scan = node.info.iter().any(|(key, value)| {
        key == "Table" || (key == "Function" && matches!(value.as_str(), "READ_PARQUET" | "PARQUET_SCAN"))
    });
    if scan && let Some(Some(filters)) = recorded.next()
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
        rows: node.rows,
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

/// `YAMLTreeRenderer::RenderRecursive`.
fn render_yaml_node(node: &ExplainNode, indent: usize, out: &mut String) {
    out.push_str(&" ".repeat(indent));
    out.push_str("- name: ");
    json_string(&node.name, out);
    out.push('\n');
    let indent = indent + 2;
    let estimate = node
        .estimate
        .map(|e| ("Estimated Cardinality".to_string(), e.to_string()));
    for (key, value) in node.info.iter().chain(estimate.iter()) {
        out.push_str(&" ".repeat(indent));
        json_string(if key.is_empty() { "Details" } else { key }, out);
        out.push(':');
        let split: Vec<&str> = value.split('\n').collect();
        if split.len() > 1 {
            out.push('\n');
            for line in split {
                out.push_str(&" ".repeat(indent + 2));
                out.push_str("- ");
                json_string(line, out);
                out.push('\n');
            }
        } else {
            out.push(' ');
            json_string(value, out);
            out.push('\n');
        }
    }
    if !node.children.is_empty() {
        out.push_str(&" ".repeat(indent));
        out.push_str("children: \n");
        for child in &node.children {
            render_yaml_node(child, indent + 2, out);
        }
    }
}

/// How EXPLAIN shows a plan: `format` is that of the statement, and `upper` is `operator_casing`.
pub(super) fn render(node: &ExplainNode, format: &str, upper: bool) -> String {
    render_with_query(node, format, upper, None)
}

/// The `Summary` box `QueryProfiler::RenderQueryTree` puts above the operator tree.
pub(super) fn summary_box(time: std::time::Duration) -> String {
    let seconds = time.as_secs_f64();
    let timing = if seconds >= 1.0 {
        format!("{seconds:.2}s")
    } else if seconds >= 0.1 {
        format!("{seconds:.3}s")
    } else {
        format!("{seconds:.4}s")
    };
    let title = "Summary";
    let key = "Total Time: ";
    let content = (title.len() + 2).max(key.len() + timing.len());
    let width = content + 4;
    format!(
        "╭─ {title} {}╮\n│ {key}{}{timing} │\n╰{}╯\n",
        "─".repeat(width - 5 - title.len()),
        " ".repeat(content - key.len() - timing.len()),
        "─".repeat(width - 2)
    )
}

/// `RenderTree::CreateNode(ProfilingNode)` over the profile of an EXPLAIN ANALYZE run, under its `QUERY` root.
pub(super) fn profile_tree(root: Option<duckrs_core::execution::profiler::ProfileNode>) -> ExplainNode {
    fn node(profile: &duckrs_core::execution::profiler::ProfileNode) -> ExplainNode {
        let mut info: Vec<(String, String)> = profile
            .extra_info
            .iter()
            .map(|(key, value)| {
                let key = if key == "Estimated Cardinality" { ESTIMATED_CARDINALITY } else { key };
                (key.to_string(), value.clone())
            })
            .collect();
        if profile.row_groups.1 > 0 && !info.iter().any(|(key, _)| key == "Row Groups Scanned") {
            info.push((
                "Row Groups Scanned".to_string(),
                format!("{} / {}", profile.row_groups.0, profile.row_groups.1),
            ));
        }
        info.push((CARDINALITY.to_string(), profile.metrics.rows.to_string()));
        info.push((TIMING.to_string(), format!("{:.2}s", profile.metrics.time.as_secs_f64())));
        ExplainNode {
            name: profile.op_type.clone(),
            info,
            estimate: None,
            rows: None,
            children: profile.children.iter().map(node).collect(),
        }
    }
    let children: Vec<ExplainNode> = root.iter().map(node).collect();
    ExplainNode {
        name: "QUERY".to_string(),
        info: Vec::new(),
        estimate: None,
        rows: None,
        children,
    }
}

const CARDINALITY: &str = "__cardinality__";
const ESTIMATED_CARDINALITY: &str = "__estimated_cardinality__";
const TIMING: &str = "__timing__";

/// A node of a `RenderTree`: its name, its `extra_text`, and where its children are.
struct GridNode {
    x: usize,
    y: usize,
    name: String,
    extra: Vec<(String, String)>,
    children: Vec<(usize, usize)>,
}

/// `RenderTree::CreateTree`: each node at its column and depth, a node taking the column of its first child.
fn grid(root: &ExplainNode, raw_keys: bool) -> Vec<GridNode> {
    fn place(node: &ExplainNode, x: usize, y: usize, raw_keys: bool, out: &mut Vec<GridNode>) -> usize {
        let estimate = node.estimate.map(|e| (ESTIMATED_CARDINALITY.to_string(), e.to_string()));
        let extra = node
            .info
            .iter()
            .chain(estimate.iter())
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| {
                let key = if key.is_empty() {
                    "Details".to_string()
                } else if !raw_keys && key.starts_with("__") {
                    sanitize_key(key)
                } else {
                    key.clone()
                };
                (key, value.clone())
            })
            .collect();
        let at = out.len();
        out.push(GridNode {
            x,
            y,
            name: node.name.clone(),
            extra,
            children: Vec::new(),
        });
        if node.children.is_empty() {
            return 1;
        }
        let mut width = 0;
        for child in &node.children {
            let child_x = x + width;
            out[at].children.push((child_x, y + 1));
            width += place(child, child_x, y + 1, raw_keys, out);
        }
        width
    }
    let mut out = Vec::new();
    place(root, 0, 0, raw_keys, &mut out);
    out.sort_by_key(|node| (node.y, node.x));
    out
}

/// `RenderTree::SanitizeKeyNames`: `__estimated_cardinality__` is shown as `Estimated Cardinality`.
fn sanitize_key(key: &str) -> String {
    key.replace("__", "")
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars.flat_map(|c| c.to_lowercase())).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// `QueryProfiler::JSONSanitize`.
fn json_sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

/// `GRAPHVIZTreeRenderer::ToStreamInternal`.
fn render_graphviz(root: &ExplainNode) -> String {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for node in grid(root, false) {
        let mut body = vec![node.name.clone()];
        for (key, value) in &node.extra {
            body.push(format!("{key}:\\n{}", json_sanitize(value)));
        }
        nodes.push(format!("    node_{}_{} [label=\"{}\"];", node.x, node.y, body.join("\\n───\\n")));
        for (cx, cy) in &node.children {
            edges.push(format!("    node_{}_{} -> node_{cx}_{cy};", node.x, node.y));
        }
    }
    format!(
        "\ndigraph G {{\n    node [shape=box, style=rounded, fontname=\"Courier New\", fontsize=10];\n{}\n{}\n}}\n\t",
        nodes.join("\n"),
        edges.join("\n")
    )
}

/// `CompactTreeRenderer`: an operator a line, indented by its depth. Of a profile, a line of the query's
/// time comes first (`RenderProfiler`).
fn render_compact(root: &ExplainNode, query: Option<std::time::Duration>) -> String {
    let mut out = String::new();
    if let Some(time) = query {
        out.push_str(&format!("QUERY (time={:.4}s)\n", time.as_secs_f64()));
    }
    let nodes = grid(root, true);
    fn line(nodes: &[GridNode], at: (usize, usize), depth: usize, out: &mut String) {
        let Some(node) = nodes.iter().find(|node| (node.x, node.y) == at) else {
            return;
        };
        let mut stats = Vec::new();
        let mut properties = Vec::new();
        for (key, value) in &node.extra {
            if key == CARDINALITY {
                stats.push(format!("rows={value}"));
            } else if key == ESTIMATED_CARDINALITY {
                stats.push(format!("est={value}"));
            } else if key == TIMING {
                stats.push(format!("time={value}"));
            } else {
                let key = if key.starts_with("__") { sanitize_key(key) } else { key.clone() };
                let value: Vec<&str> = value.split('\n').map(str::trim).filter(|v| !v.is_empty()).collect();
                properties.push(format!("{key}: {}", value.join(", ")));
            }
        }
        out.push_str(&" ".repeat(depth * 2));
        out.push_str(&node.name);
        if !stats.is_empty() {
            out.push_str(&format!(" ({})", stats.join(", ")));
        }
        if !properties.is_empty() {
            out.push(' ');
            out.push_str(&properties.join("; "));
        }
        out.push('\n');
        for child in &node.children {
            line(nodes, *child, depth + 1, out);
        }
    }
    line(&nodes, (0, 0), 0, &mut out);
    out
}

/// `SanitizeMermaidLabel`: backticks and quotes are escaped, and a literal `\n` becomes a line break.
fn mermaid_label(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => out.push_str("\\`"),
            '"' => out.push_str("\\\""),
            '\\' if chars.peek() == Some(&'n') => {
                chars.next();
                out.push_str("\n\t");
            }
            c => out.push(c),
        }
    }
    out
}

/// `MermaidTreeRenderer::ToStreamInternal`.
fn render_mermaid(root: &ExplainNode) -> String {
    let mut out = String::from("flowchart TD\n");
    let mut edges = Vec::new();
    for node in grid(root, false) {
        let mut extra = String::new();
        for (key, value) in &node.extra {
            extra.push_str(&format!("\n\t{key}: {}", mermaid_label(&json_sanitize(value))));
        }
        out.push_str(&format!(
            "    node_{}_{}[\"`**{}**{extra}`\"]\n\n",
            node.x,
            node.y,
            mermaid_label(node.name.trim())
        ));
        for (cx, cy) in &node.children {
            edges.push(format!("    node_{}_{} --> node_{cx}_{cy}\n", node.x, node.y));
        }
    }
    out.extend(edges);
    out
}

/// `ClassifyKind` of the HTML renderer: the colour of a node.
fn html_kind(name: &str, leaf: bool, extra: &[(String, String)]) -> &'static str {
    let has = |part: &str| name.contains(part);
    if has("CTE") {
        "cte"
    } else if has("SCAN") || has("GET") {
        "scan"
    } else if has("JOIN") || name == "CROSS_PRODUCT" {
        "join"
    } else if has("WINDOW") {
        "window"
    } else if has("AGGREGATE") || has("GROUP_BY") || has("DISTINCT") {
        "aggregate"
    } else if has("ORDER_BY") || has("TOP_N") {
        "order"
    } else if has("PROJECTION") {
        "projection"
    } else if has("UNION") {
        "union"
    } else if has("FILTER") {
        "filter"
    } else if leaf && extra.iter().any(|(key, _)| key == "Table" || key == "Function") {
        "scan"
    } else {
        "generic"
    }
}

/// A double as yyjson writes it.
fn json_double(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// `BuildNodeJSON` of the HTML renderer: the node at `at`, its children after it.
fn html_node(nodes: &[GridNode], at: (usize, usize), total: &mut f64, timed: &mut bool, out: &mut String) {
    let Some(node) = nodes.iter().find(|node| (node.x, node.y) == at) else {
        return;
    };
    out.push_str("{\"name\":");
    json_string_compact(&node.name, out);
    out.push_str(",\"kind\":");
    json_string_compact(html_kind(&node.name, node.children.is_empty(), &node.extra), out);
    let mut details = Vec::new();
    for (key, value) in &node.extra {
        if key == CARDINALITY || key == ESTIMATED_CARDINALITY {
            let field = if key == CARDINALITY { "cardinality" } else { "estimated_cardinality" };
            out.push_str(&format!(",\"{field}\":{}", value.parse::<i64>().unwrap_or(0)));
        } else if key == TIMING {
            let seconds: f64 = value.trim_end_matches('s').parse().unwrap_or(0.0);
            out.push_str(&format!(",\"timing\":{}", json_double(seconds)));
            *total += seconds;
            *timed = true;
        } else {
            let key = if key.starts_with("__") { sanitize_key(key) } else { key.clone() };
            let mut detail = String::from("{\"key\":");
            json_string_compact(&key, &mut detail);
            detail.push_str(",\"values\":[");
            for (i, line) in value.split('\n').enumerate() {
                if i > 0 {
                    detail.push(',');
                }
                json_string_compact(line, &mut detail);
            }
            detail.push_str("]}");
            details.push(detail);
        }
    }
    out.push_str(",\"details\":[");
    out.push_str(&details.join(","));
    out.push_str("],\"children\":[");
    for (i, child) in node.children.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        html_node(nodes, *child, total, timed, out);
    }
    out.push_str("]}");
}

fn json_string_compact(text: &str, out: &mut String) {
    json_string(text, out);
}

/// HTML-escaped text (`AppendEscapedHTML`).
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `HTMLTreeRenderer::ToStreamInternal`: the plan as JSON, in the viewer page. `query` is the run of
/// EXPLAIN ANALYZE: how long it took, and its SQL.
fn render_html(root: &ExplainNode, query: Option<(std::time::Duration, &str)>) -> String {
    let nodes = grid(root, true);
    let mut node_json = String::new();
    let mut total = 0.0;
    let mut timed = false;
    html_node(&nodes, (0, 0), &mut total, &mut timed, &mut node_json);
    let mut doc = format!("{{\"analyze\":{timed},\"total_time\":{}", json_double(total));
    if let Some((time, sql)) = query {
        let seconds = json_double(time.as_secs_f64());
        doc.push_str(&format!(
            ",\"query\":{{\"real_time\":{seconds},\"cpu_time\":{},\"bytes_read\":0,\"bytes_written\":0,\"sql\":",
            json_double(total)
        ));
        json_string_compact(&escape_html(sql), &mut doc);
        doc.push_str(",\"timings\":[]}");
    }
    doc.push_str(",\"root\":");
    doc.push_str(&node_json);
    doc.push('}');
    // the JSON must not end the <script> it is in
    let doc = doc.replace('<', "\\u003c").replace('>', "\\u003e");
    include_str!("html_template.html").replace("__PLAN_JSON__", &doc)
}

/// [`render`], with the run of EXPLAIN ANALYZE the tree is the profile of.
pub(super) fn render_with_query(
    node: &ExplainNode,
    format: &str,
    upper: bool,
    query: Option<(std::time::Duration, &str)>,
) -> String {
    let mut out = String::new();
    if format == "no_output" {
        return out;
    }
    if format == "compact" {
        return render_compact(node, query.map(|(time, _)| time));
    }
    if format == "graphviz" {
        return render_graphviz(node);
    } else if format == "mermaid" {
        return render_mermaid(node);
    } else if format == "html" {
        return render_html(node, query);
    }
    if format == "json" {
        out.push_str("[\n");
        render_json_node(node, 1, &mut out);
        out.push_str("\n]");
    } else if format == "yaml" {
        render_yaml_node(node, 0, &mut out);
    } else {
        let config = TextTreeConfig {
            upper_case: upper,
            ..TextTreeConfig::default()
        };
        out = text_tree_renderer::render(&text_tree(node), &config);
    }
    out
}
