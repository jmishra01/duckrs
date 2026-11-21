// Ported from duckdb/src/optimizer/partitioned_execution.cpp @ rust-port-baseline
//
//! `PartitionedExecution` (`PARTITIONED_EXECUTION`): a large aggregate, ORDER BY or window over a
//! table scan runs once per range of one of its columns, the ranges cut where the row groups'
//! bounds of that column overlap little, and a UNION ALL of the parts replaces it. An ORDER BY's
//! parts come in the order of their ranges; for the others, each part sees whole groups or
//! partitions of its own.

use super::common_subplan::{max_table_index, visit_bindings};
use super::cte_inlining::deep_copy;
use super::rewriter::expressions_equal;
use super::statistics::compare_values;
use crate::execution::zone_map::{ROW_GROUP_SIZE, zones_of};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::sort_key::OrderType;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::collections::HashMap;

/// `MAXIMUM_COLUMNS`: the most columns whose row groups' statistics are read.
const MAXIMUM_COLUMNS: usize = 3;
/// `MIN_ROW_GROUPS_PER_THREAD_PER_PARTITION`.
const MIN_ROW_GROUPS_PER_THREAD_PER_PARTITION: usize = 32;
/// `MAX_OVERLAP_RATIO`: of the rows of a part, how many may be in row groups that reach into the next.
const MAX_OVERLAP_RATIO: f64 = 0.1;
/// `MINIMUM_RESULT_CARDINALITY`.
const MINIMUM_RESULT_CARDINALITY: u64 = 4_194_304;

/// `PartitionedExecutionColumn`.
#[derive(Clone)]
struct Column {
    original: usize,
    binding: ColumnBinding,
    order: OrderType,
    /// the table column, once traced to the scan
    table_column: usize,
}

/// `PartitionedExecution::Optimize`.
pub fn optimize(mut plan: LogicalOperator, threads: usize) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    let mut replacements = Replacements::default();
    visit(&mut plan, threads.max(1), &mut next, &mut replacements);
    if !replacements.map.is_empty() {
        replace(&mut plan, &replacements);
    }
    plan
}

#[derive(Default)]
struct Replacements {
    map: HashMap<ColumnBinding, ColumnBinding>,
    /// the unions, whose parts read the bindings as they were
    stops: Vec<usize>,
}

fn replace(op: &mut LogicalOperator, replacements: &Replacements) {
    if let K::Union { table_index, .. } = &op.kind
        && replacements.stops.contains(table_index)
    {
        return;
    }
    for child in &mut op.children {
        replace(child, replacements);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(to) = replacements.map.get(binding)
        {
            *binding = *to;
        }
    });
}

fn column_of(expr: &BoundExpr) -> Option<ColumnBinding> {
    match expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(binding),
        _ => None,
    }
}

/// `PartitionedExecutionGetColumns`.
fn get_columns(op: &LogicalOperator) -> Option<Vec<Column>> {
    let column = |original, binding, order| Column {
        original,
        binding,
        order,
        table_column: 0,
    };
    let mut columns = Vec::new();
    match &op.kind {
        K::Aggregate {
            groups,
            grouping_sets,
            grouping_functions,
            ..
        } => {
            if grouping_sets.len() > 1 || !grouping_functions.is_empty() || groups.is_empty() {
                return None;
            }
            for group in groups {
                if let Some(binding) = column_of(group) {
                    columns.push(column(columns.len(), binding, OrderType::Ascending));
                }
            }
        }
        K::Order { orders } => {
            for order in orders {
                let Some(binding) = column_of(&order.expr) else {
                    break;
                };
                columns.push(column(columns.len(), binding, order.modifiers.order));
            }
        }
        K::Window { windows, .. } => {
            let first = windows.first()?;
            for window in &windows[1..] {
                let same = window.partitions.len() == first.partitions.len()
                    && window.partitions.iter().all(|p| first.partitions.iter().any(|q| expressions_equal(p, q)));
                if !same {
                    return None;
                }
            }
            for partition in &first.partitions {
                if let Some(binding) = column_of(partition) {
                    columns.push(column(columns.len(), binding, OrderType::Ascending));
                }
            }
        }
        _ => return None,
    }
    Some(columns)
}

/// `PartitionedExecutionHandleColumnRemoval`: an ORDER BY keeps its columns up to the first that
/// goes, the others any of them.
fn remove(op: &LogicalOperator, columns: &mut Vec<Column>, at: usize) {
    if matches!(op.kind, K::Order { .. }) {
        columns.truncate(at);
    } else {
        columns.remove(at);
    }
}

/// `PartitionedExecutionTraceColumns`: the columns down to the table scan, through projections of
/// columns and filters.
fn trace_columns<'a>(op: &'a LogicalOperator, columns: &mut Vec<Column>) -> Option<&'a LogicalOperator> {
    if op.children.len() != 1 {
        return None;
    }
    let mut child = &op.children[0];
    loop {
        match &child.kind {
            K::Get { .. } => break,
            K::Projection { expressions, .. } => {
                let mut at = 0;
                while at < columns.len() {
                    match expressions.get(columns[at].binding.column_index).and_then(column_of) {
                        Some(below) => {
                            columns[at].binding = below;
                            at += 1;
                        }
                        None => remove(op, columns, at),
                    }
                }
            }
            K::Filter { .. } => {}
            _ => return None,
        }
        child = &child.children[0];
    }
    let K::Get {
        column_ids,
        index_rows: None,
        ..
    } = &child.kind
    else {
        return None;
    };
    if !child.children.is_empty() {
        return None;
    }
    let mut at = 0;
    while at < columns.len() {
        match column_ids.get(columns[at].binding.column_index) {
            Some(&c) if c != COLUMN_IDENTIFIER_ROW_ID => {
                columns[at].table_column = c;
                at += 1;
            }
            _ => remove(op, columns, at),
        }
    }
    Some(child)
}

/// `PartitionedExecutionCanUseStats`, for a zone: bounds and no NULL, of a number or a VARCHAR.
fn can_use(zone: &crate::catalog::ColumnZone, ty: &LogicalType) -> bool {
    let supported = ty.is_numeric() || ty.id() == LogicalTypeId::Varchar;
    supported && !zone.unknown && !zone.has_null && zone.min.is_some() && zone.max.is_some()
}

/// A bound of a row group: where it starts (`count > 0`) or ends (`count < 0`)
/// (`PartitionedExecutionStatsNode`).
#[derive(Clone)]
struct Node {
    value: Value,
    count: i64,
}

/// `PartitionedExecutionRange`.
#[derive(Clone)]
struct Range {
    min: Option<Value>,
    max: Option<Value>,
}

/// `PartitionedExecutionGrowPartition`.
fn grow(
    nodes: &[Node],
    i: &mut usize,
    overlap: &mut i64,
    count: &mut u64,
    ratio: &mut f64,
    stop: &mut dyn FnMut(usize, u64, f64) -> bool,
    update: &mut dyn FnMut(usize, f64),
) {
    while *i < nodes.len() && !stop(*i, *count, *ratio) {
        let node = &nodes[*i];
        *i += 1;
        *overlap += node.count;
        if node.count > 0 {
            *count += node.count as u64;
        }
        *ratio = if *count == 0 { 1.0 } else { *overlap as f64 / *count as f64 };
        update(*i, *ratio);
    }
}

/// `PartitionedExecutionComputeRanges` for one column's nodes.
fn compute_ranges(mut nodes: Vec<Node>, threads: usize) -> Vec<Range> {
    let min_row_groups = threads * MIN_ROW_GROUPS_PER_THREAD_PER_PARTITION;
    let min_count = (min_row_groups * ROW_GROUP_SIZE) as u64;
    nodes.sort_by(|a, b| {
        if compare_values(ComparisonOp::Equal, &a.value, &b.value) == Some(true) {
            // the starts of row groups first
            b.count.cmp(&a.count)
        } else if compare_values(ComparisonOp::LessThan, &a.value, &b.value) == Some(true) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let mut ranges = Vec::new();
    let mut overlap: i64 = 0;
    let mut i = 0;
    let mut nothing = |_: usize, _: f64| {};
    while i < nodes.len() {
        let start = i;
        let mut count = 0u64;
        let mut ratio = 1.0;
        // at least `min_count` rows
        grow(&nodes, &mut i, &mut overlap, &mut count, &mut ratio, &mut |_, c, _| c >= min_count, &mut nothing);
        // until the overlap is small enough
        grow(&nodes, &mut i, &mut overlap, &mut count, &mut ratio, &mut |_, _, r| r <= MAX_OVERLAP_RATIO, &mut nothing);
        if ratio != 0.0 {
            // look ahead for at most half of `min_count` for a point with less overlap
            let (mut temp_i, mut temp_overlap, mut temp_count, mut temp_ratio) = (i, overlap, count, ratio);
            let (mut lowest_ratio, mut lowest_i) = (ratio, i);
            let end = count + min_count / 2;
            grow(
                &nodes,
                &mut temp_i,
                &mut temp_overlap,
                &mut temp_count,
                &mut temp_ratio,
                &mut |_, c, _| c >= end,
                &mut |at, r| {
                    if r < lowest_ratio {
                        lowest_ratio = r;
                        lowest_i = at;
                    }
                },
            );
            if lowest_i != i {
                grow(&nodes, &mut i, &mut overlap, &mut count, &mut ratio, &mut |at, _, _| at == lowest_i, &mut nothing);
            }
            // until the next node starts a row group
            if ratio != 0.0 {
                grow(
                    &nodes,
                    &mut i,
                    &mut overlap,
                    &mut count,
                    &mut ratio,
                    &mut |at, _, _| nodes[at].count > 0,
                    &mut nothing,
                );
            }
        }
        // no remainder of less than half of `min_row_groups`
        if nodes.len() - i < min_row_groups / 2 {
            grow(&nodes, &mut i, &mut overlap, &mut count, &mut ratio, &mut |_, _, _| false, &mut nothing);
        }
        ranges.push(Range {
            min: (start != 0).then(|| nodes[start].value.clone()),
            max: (i != nodes.len()).then(|| nodes[i].value.clone()),
        });
    }
    ranges
}

fn visit(op: &mut LogicalOperator, threads: usize, next: &mut usize, replacements: &mut Replacements) {
    for child in &mut op.children {
        visit(child, threads, next, replacements);
    }
    if super::join_order::estimated_cardinality(op) < MINIMUM_RESULT_CARDINALITY {
        return;
    }
    let Some(mut columns) = get_columns(op).filter(|c| !c.is_empty()) else {
        return;
    };
    let Some(get) = trace_columns(op, &mut columns) else {
        return;
    };
    if columns.is_empty() {
        return;
    }
    let K::Get { table, extracts, .. } = &get.kind else {
        unreachable!()
    };
    // a position that outputs a field of a column has no zones of its own
    if extracts.iter().any(Option::is_some) {
        return;
    }
    let table = table.clone();
    let rows = table.row_count();
    // `PartitionedExecutionHandleGlobalStats`: of the whole table, a column without NULL whose
    // values differ
    let mut at = 0;
    while at < columns.len() {
        let column = columns[at].table_column;
        let ty = &table.columns[column].ty;
        let zones = zones_of(&table, column);
        let usable = !zones.is_empty() && zones.iter().all(|z| can_use(z, ty));
        if !usable {
            remove(op, &mut columns, at);
            continue;
        }
        let constant = zones.iter().all(|z| {
            compare_values(ComparisonOp::Equal, z.min.as_ref().expect("can_use"), zones[0].min.as_ref().expect("can_use"))
                == Some(true)
                && compare_values(ComparisonOp::Equal, z.max.as_ref().expect("can_use"), zones[0].min.as_ref().expect("can_use"))
                    == Some(true)
        });
        if constant {
            columns.remove(at);
        } else {
            at += 1;
        }
    }
    if columns.is_empty() {
        return;
    }
    // an ORDER BY has to keep its order: one column
    if matches!(op.kind, K::Order { .. }) {
        columns.truncate(1);
    }
    let groups = crate::execution::zone_map::row_group_count(&table, rows);
    if groups <= 1 {
        return;
    }
    // `PartitionedExecutionCollectStatsNodes`: the bounds of each row group, with its rows
    columns.truncate(MAXIMUM_COLUMNS);
    let size = table.row_group_size();
    let mut best: Option<(usize, Vec<Range>)> = None;
    for (index, column) in columns.iter().enumerate() {
        let zones = zones_of(&table, column.table_column);
        let mut nodes = Vec::with_capacity(groups * 2);
        for (group, zone) in zones.iter().enumerate().take(groups) {
            let count = (rows - group * size).min(size) as i64;
            nodes.push(Node {
                value: zone.min.clone().expect("can_use"),
                count,
            });
            nodes.push(Node {
                value: zone.max.clone().expect("can_use"),
                count: -count,
            });
        }
        let ranges = compute_ranges(nodes, threads);
        if best.as_ref().is_none_or(|(_, r)| ranges.len() > r.len()) {
            best = Some((index, ranges));
        }
    }
    let Some((range_column, mut ranges)) = best else {
        return;
    };
    if columns[range_column].order == OrderType::Descending {
        ranges.reverse();
    }
    if ranges.len() < 2 {
        return;
    }
    split(op, &columns[range_column], &ranges, next, replacements);
}

/// `PartitionedExecutionSplitPipeline`.
fn split(op: &mut LogicalOperator, column: &Column, ranges: &[Range], next: &mut usize, replacements: &mut Replacements) {
    let mut parts = Vec::new();
    for range in ranges {
        let mut copy = deep_copy(op, next);
        let copy_columns = get_columns(&copy).expect("as the original");
        let binding = copy_columns[column.original].binding;
        let ty = copy.children[0]
            .column_bindings()
            .iter()
            .position(|b| *b == binding)
            .map(|at| copy.children[0].types()[at].clone())
            .expect("a column of the child");
        let compare = |op, value: &Value| {
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(BoundExpr::column_ref(binding, ty.clone(), "")),
                    right: Box::new(BoundExpr::constant(value.clone())),
                },
                LogicalType::BOOLEAN,
            )
        };
        let mut filters = Vec::new();
        if let Some(min) = &range.min {
            filters.push(compare(ComparisonOp::GreaterThanOrEqual, min));
        }
        if let Some(max) = &range.max {
            filters.push(compare(ComparisonOp::LessThan, max));
        }
        let child = copy.children.pop().expect("one child");
        copy.children.push(LogicalOperator::new(K::Filter { expressions: filters }, vec![child]));
        let copy = super::filter_pushdown::push_down(copy);
        // a range the filters there leave nothing of
        let mut leaf = &copy;
        while let Some(child) = leaf.children.first() {
            leaf = child;
        }
        if matches!(leaf.kind, K::EmptyResult { .. }) || matches!(copy.kind, K::EmptyResult { .. }) {
            continue;
        }
        parts.push(copy);
    }
    if parts.len() < 2 {
        return;
    }
    let old_bindings = op.column_bindings();
    let types = op.types();
    let table_index = *next;
    *next += 1;
    *op = LogicalOperator::new(K::Union { table_index, types }, parts);
    for (i, old) in old_bindings.into_iter().enumerate() {
        replacements.map.insert(old, ColumnBinding::new(table_index, i));
    }
    replacements.stops.push(table_index);
}
