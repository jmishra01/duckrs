// Ported from duckdb/src/common/render_tree.cpp, duckdb/src/common/tree_renderer/
// {text_tree_renderer,json_tree_renderer}.cpp and the ParamsToString methods of the operators
// @ rust-port-baseline
//
//! What EXPLAIN shows. A plan becomes a tree of [`ExplainNode`]s, which render as text or JSON. The
//! text form has one line per fact, not the boxes of `TextTreeRenderer`, as what reads it looks for
//! names and values. A physical plan takes its details from the logical plan it came from, whose
//! expressions still have their names.

use crate::execution::physical::PhysicalOperator;
use crate::optimizer::join_order::PlanEstimate;
use crate::planner::expression::{BoundAggregate, BoundExpr};
use crate::planner::logical::{BoundOrderByNode, JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::sort_key::OrderType;

/// `RenderTreeNode`.
pub(super) struct ExplainNode {
    name: String,
    /// keys and values; a value can have several lines
    info: Vec<(String, String)>,
    estimate: Option<u64>,
    children: Vec<ExplainNode>,
}

/// `LogicalOperatorToString`.
fn logical_name(op: &LogicalOperator) -> &'static str {
    match &op.kind {
        K::Get { .. } | K::Range { .. } | K::ExpressionGet { .. } | K::ChunkGet { .. } => "SEQ_SCAN",
        K::EmptyResult { .. } => "EMPTY_RESULT",
        K::DummyScan { .. } => "DUMMY_SCAN",
        K::Filter { .. } => "FILTER",
        K::Window { .. } => "WINDOW",
        K::Aggregate { .. } => "AGGREGATE",
        K::Order { .. } => "ORDER_BY",
        K::TopN { .. } => "TOP_N",
        K::Limit { .. } | K::LimitPercent { .. } => "LIMIT",
        K::Sample { .. } => "SAMPLE",
        K::Distinct { .. } => "DISTINCT",
        K::Unnest { .. } => "UNNEST",
        K::Union { .. } => "UNION",
        K::ExceptIntersect { except: true, .. } => "EXCEPT",
        K::ExceptIntersect { .. } => "INTERSECT",
        K::RecursiveCte { .. } => "REC_CTE",
        K::WorkingTable { .. } => "CTE_SCAN",
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

fn lines(values: impl IntoIterator<Item = String>) -> String {
    values.into_iter().collect::<Vec<_>>().join("\n")
}

fn conditions_text(conditions: &[JoinCondition], predicate: Option<String>) -> String {
    let conditions = conditions.iter().map(|c| {
        let op = if c.null_safe { "IS NOT DISTINCT FROM" } else { "=" };
        format!("{} {op} {}", c.left, c.right)
    });
    lines(conditions.chain(predicate))
}

fn aggregate_text(aggregate: &BoundAggregate) -> String {
    let arguments: Vec<String> = aggregate.children.iter().map(|c| c.to_string()).collect();
    let name = &aggregate.function.name;
    let distinct = if aggregate.distinct { "DISTINCT " } else { "" };
    let text = format!("{name}({distinct}{})", arguments.join(", "));
    match &aggregate.filter {
        Some(filter) => format!("{text} Filter: {filter}"),
        None => text,
    }
}

/// `ExpressionFilter::ToString`: without the parentheses around all of it, if there are any.
fn filter_text(filter: &BoundExpr) -> String {
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

fn order_text(orders: &[BoundOrderByNode]) -> String {
    lines(orders.iter().map(|order| {
        let direction = match order.modifiers.order {
            OrderType::Descending => "DESC",
            _ => "ASC",
        };
        format!("{} {direction}", order.expr)
    }))
}

/// `ParamsToString`, where physical operators differ from logical ones in some keys.
fn operator_info(op: &LogicalOperator, physical: bool) -> Vec<(String, String)> {
    let entry = |key: &str, value: String| vec![(key.to_string(), value)];
    let join = |join_type: String, conditions: &[JoinCondition], predicate: Option<String>| {
        let mut info = entry("Join Type", join_type);
        if !conditions.is_empty() || predicate.is_some() {
            info.extend(entry("Conditions", conditions_text(conditions, predicate)));
        }
        info
    };
    match &op.kind {
        K::Get {
            table,
            column_ids,
            filters,
            projection,
            ..
        } => {
            let mut info = entry("Table", table.name.clone());
            info.extend(entry("Type", "Sequential Scan".to_string()));
            let name = |&column: &usize| {
                let name = table.columns.get(column).map(|c| c.name.as_str());
                name.unwrap_or("rowid").to_string()
            };
            let projected: Vec<String> = match projection {
                Some(positions) => positions.iter().map(|&p| name(&column_ids[p])).collect(),
                None => column_ids.iter().map(name).collect(),
            };
            info.extend(entry("Projections", lines(projected)));
            if !filters.is_empty() {
                info.extend(entry("Filters", lines(filters.iter().map(filter_text))));
            }
            info
        }
        K::Filter { expressions } => {
            let key = if physical { "Expression" } else { "Expressions" };
            entry(key, lines(expressions.iter().map(|e| e.to_string())))
        }
        K::Projection { expressions, .. } => {
            let key = if physical { "Projections" } else { "Expressions" };
            entry(key, lines(expressions.iter().map(|e| e.to_string())))
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
        K::Limit { limit, offset } => {
            let mut info = Vec::new();
            if let Some(limit) = limit {
                info.extend(entry("Limit", limit.to_string()));
            }
            if *offset != 0 {
                info.extend(entry("Offset", offset.to_string()));
            }
            info
        }
        K::Join {
            join_type,
            conditions,
            predicate,
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

/// The tree of a physical plan, and the logical plan it came from if they have the same shape.
pub(super) fn physical_tree(
    op: &dyn PhysicalOperator,
    logical: Option<&LogicalOperator>,
    estimate: Option<&PlanEstimate>,
    analyze: bool,
) -> ExplainNode {
    let children = op.children();
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

/// `DisplayName`: HASH_JOIN is Hash Join, unless `operator_casing` asks for the name as it is.
fn display_name(name: &str, upper: bool) -> String {
    if upper {
        return name.to_string();
    }
    let title: Vec<String> = name
        .split('_')
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                Some(first) => first.to_string() + &letters.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect();
    title.join(" ").replace("Cte", "CTE").replace("Ie Join", "IE Join")
}

/// `FormatCount`, with the default thousand separator.
fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut result = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

fn render_text_node(node: &ExplainNode, depth: usize, upper: bool, out: &mut String) {
    let indent = "  ".repeat(depth);
    out.push_str(&indent);
    out.push_str(&display_name(&node.name, upper));
    out.push('\n');
    for (key, value) in &node.info {
        for (at, line) in value.lines().enumerate() {
            out.push_str(&indent);
            out.push_str("  ");
            if at == 0 && !key.is_empty() {
                out.push_str(key);
                out.push_str(": ");
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    if let Some(estimate) = node.estimate {
        let unit = if estimate == 1 { "row" } else { "rows" };
        out.push_str(&format!("{indent}  ~{} {unit}\n", format_count(estimate)));
    }
    for child in &node.children {
        render_text_node(child, depth + 1, upper, out);
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
        render_text_node(node, 0, upper, &mut out);
    }
    out
}
