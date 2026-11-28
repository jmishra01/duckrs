// Ported from duckdb/src/optimizer/window_self_join.cpp @ rust-port-baseline
//
//! `WindowSelfJoinOptimizer` (`WINDOW_SELF_JOIN`): window aggregates whose frame is the whole
//! partition are one value per partition. The window becomes a join of its input with an aggregate
//! over a copy of it, grouped by the partitions: `sum(x) OVER (PARTITION BY k)` is the `sum(x)` of
//! `GROUP BY k` joined on `k IS NOT DISTINCT FROM k`.

use super::common_subplan::{max_table_index, visit_bindings};
use super::cte_inlining::deep_copy;
use super::remove_redundant_order::order_independent;
use super::rewriter::expressions_equal;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundWindow, ColumnBinding, WindowFunction};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use pintail_parser::ast::{ComparisonOp, JoinType, WindowBoundary, WindowExclude};
use std::collections::HashMap;

/// `WindowSelfJoinOptimizer::Optimize`.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    let mut replacements = HashMap::new();
    visit(&mut plan, &mut next, &mut replacements);
    if !replacements.is_empty() {
        replace(&mut plan, &replacements);
    }
    plan
}

fn replace(op: &mut LogicalOperator, map: &HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        replace(child, map);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(to) = map.get(binding)
        {
            *binding = *to;
        }
    });
}

/// `VolatileExpressionCounter::HasVolatiles`, over the operator and what is below it.
fn has_volatiles(op: &LogicalOperator) -> bool {
    let mut found = false;
    let mut probe = LogicalOperator::new(op.kind.clone(), Vec::new());
    for expr in probe.expressions_mut() {
        found |= expr.is_volatile();
    }
    found || op.children.iter().any(has_volatiles)
}

/// `BaseSelectBinder::IsFunctionallyDependent`.
fn is_functionally_dependent(expr: &BoundExpr, dependencies: &[&BoundExpr]) -> bool {
    if expr.is_volatile() {
        return false;
    }
    if expr.is_foldable() || dependencies.iter().any(|d| expressions_equal(expr, d)) {
        return true;
    }
    let children = expr.children();
    !children.is_empty()
        && children
            .into_iter()
            .all(|child| is_functionally_dependent(child, dependencies))
}

fn is_order_dependent(window: &BoundWindow) -> bool {
    window.aggregate.as_ref().is_some_and(|aggregate| {
        !order_independent(&aggregate.name, window.children.first().map(|c| c.return_type.id()))
    })
}

/// `FrameOrderIsRedundant`: rows that tie on the argument ordering have equal arguments.
fn frame_order_is_redundant(window: &BoundWindow) -> bool {
    if window.arg_orders.is_empty() {
        return false;
    }
    let orders: Vec<&BoundExpr> = window.arg_orders.iter().map(|(e, _)| e).collect();
    window
        .children
        .iter()
        .all(|arg| is_functionally_dependent(arg, &orders))
}

/// `IsOrderableDistinctAggregate`.
fn is_orderable_distinct_aggregate(window: &BoundWindow) -> bool {
    if !is_order_dependent(window) || !window.distinct {
        return true;
    }
    let arguments: Vec<&BoundExpr> = window.children.iter().collect();
    let mut in_args = window
        .arg_orders
        .iter()
        .all(|(order, _)| is_functionally_dependent(order, &arguments));
    if frame_order_is_redundant(window) {
        return in_args;
    }
    in_args &= window
        .orders
        .iter()
        .all(|(order, _)| is_functionally_dependent(order, &arguments));
    in_args
}

/// `PartitionsAreEquivalent`: the partitions as sets.
fn partitions_are_equivalent(a: &BoundWindow, b: &BoundWindow) -> bool {
    let mut left: Vec<&BoundExpr> = Vec::new();
    for p in &a.partitions {
        if !left.iter().any(|e| expressions_equal(e, p)) {
            left.push(p);
        }
    }
    let mut right: Vec<&BoundExpr> = Vec::new();
    for p in &b.partitions {
        if !right.iter().any(|e| expressions_equal(e, p)) {
            right.push(p);
        }
    }
    left.len() == right.len() && left.iter().all(|l| right.iter().any(|r| expressions_equal(l, r)))
}

/// `CanOptimize(BoundWindowExpression)`: an aggregate over the whole partition.
fn can_optimize_window(window: &BoundWindow, first: &BoundWindow) -> bool {
    if window.function != WindowFunction::Aggregate || window.aggregate.is_none() {
        return false;
    }
    // only the whole partition: an ORDER BY with a frame that ends at the current row is a
    // running aggregate, and ROWS frames depend on the position of the row
    let bound_ok = |boundary: WindowBoundary, unbounded: WindowBoundary| match boundary {
        b if b == unbounded => true,
        WindowBoundary::CurrentRowRange | WindowBoundary::CurrentRowGroups => window.orders.is_empty(),
        _ => false,
    };
    bound_ok(window.start, WindowBoundary::UnboundedPreceding)
        && bound_ok(window.end, WindowBoundary::UnboundedFollowing)
        && window.exclude == WindowExclude::NoOther
        && partitions_are_equivalent(window, first)
        && is_orderable_distinct_aggregate(window)
}

/// `CanOptimize(LogicalOperator)`: what below the window can be copied.
fn can_optimize_input(op: &LogicalOperator) -> bool {
    match op.kind {
        K::Get { .. }
        | K::Range { .. }
        | K::CsvScan { .. }
        | K::ParquetScan { .. }
        | K::JsonScan { .. }
        | K::ExpressionGet { .. }
        | K::Projection { .. }
        | K::Aggregate { .. }
        | K::DummyScan { .. }
        | K::Filter { .. } => op.children.first().is_none_or(can_optimize_input),
        _ => false,
    }
}

/// `TranslateAggregate`.
fn translate_aggregate(window: &BoundWindow) -> BoundAggregate {
    let function = window.aggregate.clone().expect("checked");
    let mut order_by = Vec::new();
    if is_order_dependent(window) {
        // the frame ordering breaks ties in the argument ordering
        order_by.extend(window.arg_orders.iter().cloned());
        if !frame_order_is_redundant(window) {
            order_by.extend(window.orders.iter().cloned());
        }
    }
    BoundAggregate {
        function,
        return_type: window.return_type.clone(),
        children: window.children.clone(),
        filter: window.filter.clone(),
        distinct: window.distinct,
        order_by,
        distinct_key: None,
        distinct_order: Vec::new(),
        proven_no_overflow: false,
    }
}

fn take(next: &mut usize) -> usize {
    let index = *next;
    *next += 1;
    index
}

/// `OptimizeInternal`.
fn visit(op: &mut LogicalOperator, next: &mut usize, replacements: &mut HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        visit(child, next, replacements);
    }
    let K::Window { table_index, windows } = &op.kind else {
        return;
    };
    if windows.is_empty()
        || has_volatiles(op)
        || !can_optimize_input(&op.children[0])
        || !windows.iter().all(|w| can_optimize_window(w, &windows[0]))
    {
        return;
    }
    let window_index = *table_index;
    let partitions = windows[0].partitions.clone();
    let aggregates: Vec<BoundAggregate> = windows.iter().map(translate_aggregate).collect();
    let original = op.children.pop().expect("a window has a child");
    // the copy has the bindings of the original, under new table indexes
    let copy = deep_copy(&original, next);
    let map: HashMap<ColumnBinding, ColumnBinding> = original
        .column_bindings()
        .into_iter()
        .zip(copy.column_bindings())
        .collect();
    let rebind = |mut expr: BoundExpr| {
        expr.walk_mut(&mut |e| {
            if let crate::planner::expression::BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                && let Some(to) = map.get(binding)
            {
                *binding = *to;
            }
        });
        expr
    };
    let aggregate_index = take(next);
    let group_index = take(next);
    let groups: Vec<BoundExpr> = partitions.iter().cloned().map(rebind).collect();
    let aggregates: Vec<BoundAggregate> = aggregates
        .into_iter()
        .map(|mut aggregate| {
            aggregate.children = aggregate.children.into_iter().map(rebind).collect();
            aggregate.filter = aggregate.filter.map(rebind);
            aggregate.order_by = aggregate.order_by.into_iter().map(|(e, m)| (rebind(e), m)).collect();
            aggregate
        })
        .collect();
    let count = aggregates.len();
    let aggregate = LogicalOperator::new(
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index: take(next),
            groups,
            aggregates,
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
            perfect_hash: None,
            domain_cap: None,
        },
        vec![copy],
    );
    // an inner join on the partitions, or a cross product with the one row of the aggregate
    *op = if partitions.is_empty() {
        LogicalOperator::new(K::CrossProduct, vec![original, aggregate])
    } else {
        let conditions = partitions
            .iter()
            .enumerate()
            .map(|(i, partition)| JoinCondition {
                bound: None,
                left: partition.clone(),
                right: BoundExpr::column_ref(ColumnBinding::new(group_index, i), partition.return_type.clone(), ""),
                comparison: ComparisonOp::NotDistinctFrom,
                filter: None,
            })
            .collect();
        LogicalOperator::new(
            K::Join {
                join_type: JoinType::Inner,
                conditions,
                predicate: None,
                algorithm: Default::default(),
            },
            vec![original, aggregate],
        )
    };
    for i in 0..count {
        replacements.insert(
            ColumnBinding::new(window_index, i),
            ColumnBinding::new(aggregate_index, i),
        );
    }
}
