// Ported from duckdb/src/optimizer/common_aggregate_optimizer.cpp @ rust-port-baseline
//
//! `CommonAggregateOptimizer` (`COMMON_AGGREGATE`): an aggregate that computes the same aggregate twice keeps it
//! once, and what read the other reads it. Duplicates come from queries (`sum(x)` and `sum(x) AS total`) and from
//! the rewrites before, as `avg(x)` that the `AggregateFunctionRewriter` made `sum(x) / count(x)` next to a `sum(x)`
//! and a `count(*)` of the query, once statistics made `count(x)` of a column without NULLs `count_star()`.
//!
//! C++ renumbers the aggregates and rebinds what reads them. duckrs plans subqueries and lateral joins with plans
//! of their own, which can read the columns of the plan around them, so a plan with one of those is left as it
//! is, and so is an aggregate whose columns something reads by position (the result, a set operation): taking
//! out a column would change what that reads.

use super::aggregate_rewriter::aggregate_equals;
use super::common_subplan::visit_bindings;
use crate::planner::expression::{BoundAggregate, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use std::collections::HashMap;

/// Runs the pass over a plan, and says whether it took out an aggregate.
pub fn optimize(plan: &mut LogicalOperator) -> bool {
    if has_outer_references(plan) {
        return false;
    }
    let mut changed = false;
    visit(plan, true, &mut changed);
    changed
}

/// Whether a plan of its own inside this one (a subquery, the right side of a lateral join, a recursive CTE, a
/// DELIM join's correlated side) might read its columns.
fn has_outer_references(plan: &mut LogicalOperator) -> bool {
    fn nested(op: &LogicalOperator) -> bool {
        matches!(
            op.kind,
            K::LateralJoin { .. } | K::RecursiveCte { .. } | K::DelimJoin { .. } | K::DelimGet { .. }
        ) || op.children.iter().any(nested)
    }
    if nested(plan) {
        return true;
    }
    let mut found = false;
    plan.visit_expressions(&mut |expr| {
        found |= matches!(expr.kind, BoundExprKind::Subquery { .. })
            || matches!(expr.kind, BoundExprKind::ColumnRef { depth, .. } if depth > 0);
    });
    found
}

/// Takes out the duplicate aggregates at and below `op`, and gives the columns that moved. `by_position` says
/// something above reads the columns of `op` by position, and not through bindings.
fn visit(op: &mut LogicalOperator, by_position: bool, changed: &mut bool) -> HashMap<ColumnBinding, ColumnBinding> {
    // as `RemoveUnusedColumns` passes it on: what reads its child through bindings, and what passes its child's
    // columns on
    let child_by_position = match op.kind {
        K::Projection { .. } | K::Aggregate { .. } => false,
        K::Filter { .. }
        | K::Order { .. }
        | K::TopN { .. }
        | K::Limit { .. }
        | K::CrossProduct
        | K::Join { .. }
        | K::MarkJoin { .. }
        | K::ColumnSelect { .. } => by_position,
        _ => true,
    };
    let mut map = HashMap::new();
    for child in &mut op.children {
        map.extend(visit(child, child_by_position, changed));
    }
    if !map.is_empty() {
        visit_bindings(&mut op.kind, &mut |binding, depth| {
            if depth == 0
                && let Some(new) = map.get(binding)
            {
                *binding = *new;
            }
        });
    }
    // `VisitOperator`: the columns of these are their own
    if matches!(
        op.kind,
        K::Union { .. } | K::ExceptIntersect { .. } | K::MaterializedCte { .. } | K::Projection { .. }
    ) {
        map.clear();
    }
    if !by_position && let K::Aggregate { .. } = op.kind {
        let moved = extract_common_aggregates(op);
        *changed |= !moved.is_empty();
        map.extend(moved);
    }
    map
}

/// `ExtractCommonAggregates`: the aggregates without their duplicates, and where each column of them went.
fn extract_common_aggregates(op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
    let mut map = HashMap::new();
    let K::Aggregate {
        aggregate_index,
        aggregates,
        ..
    } = &mut op.kind
    else {
        return map;
    };
    let mut kept: Vec<BoundAggregate> = Vec::with_capacity(aggregates.len());
    for (original, aggregate) in std::mem::take(aggregates).into_iter().enumerate() {
        // each call of a volatile aggregate is a value of its own
        let existing = (!is_volatile(&aggregate))
            .then(|| {
                kept.iter()
                    .position(|other| !is_volatile(other) && same_aggregate(other, &aggregate))
            })
            .flatten();
        let new = existing.unwrap_or_else(|| {
            kept.push(aggregate);
            kept.len() - 1
        });
        if new != original {
            map.insert(
                ColumnBinding::new(*aggregate_index, original),
                ColumnBinding::new(*aggregate_index, new),
            );
        }
    }
    *aggregates = kept;
    map
}

/// The expression equality C++ maps aggregates by: the same function on the same arguments, filter and order,
/// down to its overload and whether it exports its state.
fn same_aggregate(a: &BoundAggregate, b: &BoundAggregate) -> bool {
    aggregate_equals(a, b)
        && a.function.kind == b.function.kind
        && a.function.export == b.function.export
        && a.distinct_order == b.distinct_order
        && a.distinct_key.as_ref().map(ToString::to_string) == b.distinct_key.as_ref().map(ToString::to_string)
}

fn is_volatile(aggregate: &BoundAggregate) -> bool {
    aggregate.children.iter().any(|c| c.is_volatile())
        || aggregate.filter.as_ref().is_some_and(|f| f.is_volatile())
        || aggregate.order_by.iter().any(|(e, _)| e.is_volatile())
}
