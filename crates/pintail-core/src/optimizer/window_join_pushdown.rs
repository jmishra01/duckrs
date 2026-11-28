// Ported from duckdb/src/optimizer/window_join_pushdown.cpp and
// duckdb/src/optimizer/apply_functional_dependencies.cpp (`GetUniqueColumnCombinations`) @ rust-port-baseline
//
//! `WindowJoinPushdown` (`WINDOW_JOIN_PUSHDOWN`): a join of a window with a table on one equality
//! goes below the window when it keeps the partitions as they are. The window side's key must be
//! decided by every window's partitions; an inner or left join must also not repeat rows, so the
//! other side's key must be unique there. SEMI and ANTI joins only drop rows, a partition at a time.

use super::rewriter::expressions_equal;
use crate::catalog::TableConstraint;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use pintail_parser::ast::{ComparisonOp, ConstraintCheckMode, JoinType};
use std::collections::BTreeSet;

/// `WindowJoinPushdown::Optimize`. The root keeps its place, as its columns are the result's.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    for child in &mut plan.children {
        visit(child);
    }
    plan
}

fn visit(op: &mut LogicalOperator) {
    if let Some(window_side) = can_optimize(op) {
        push_down(op, window_side);
        return;
    }
    for child in &mut op.children {
        visit(child);
    }
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

/// `WindowJoinPushdown::CanOptimize`: the side of the window, if the join can go below it.
fn can_optimize(op: &LogicalOperator) -> Option<usize> {
    let K::Join {
        join_type,
        conditions,
        predicate: None,
        ..
    } = &op.kind
    else {
        return None;
    };
    let [condition] = conditions.as_slice() else {
        return None;
    };
    if !matches!(
        condition.comparison,
        ComparisonOp::Equal | ComparisonOp::NotDistinctFrom
    ) {
        return None;
    }
    let is_window = |i: usize| matches!(op.children[i].kind, K::Window { .. });
    let window_side = match join_type {
        JoinType::Inner => {
            let side = if is_window(0) { 0 } else { 1 };
            // stacked windows are not pushed into each other
            if is_window(1 - side) {
                return None;
            }
            side
        }
        JoinType::Left | JoinType::Semi | JoinType::Anti => 0,
        _ => return None,
    };
    let K::Window { windows, .. } = &op.children[window_side].kind else {
        return None;
    };
    let b = if window_side == 0 {
        &condition.left
    } else {
        &condition.right
    };
    // FD P -> b
    for window in windows {
        let partitions: Vec<&BoundExpr> = window.partitions.iter().collect();
        if partitions.is_empty() || !is_functionally_dependent(b, &partitions) {
            return None;
        }
    }
    if matches!(join_type, JoinType::Semi | JoinType::Anti) {
        return Some(window_side);
    }
    // x unique
    let x = if window_side == 1 {
        &condition.left
    } else {
        &condition.right
    };
    let BoundExprKind::ColumnRef { binding, depth: 0 } = &x.kind else {
        return None;
    };
    unique_column_combinations(&op.children[1 - window_side])
        .iter()
        .any(|key| key.len() == 1 && key.contains(binding))
        .then_some(window_side)
}

/// `OptimizeInternal`: `window(r) JOIN s` becomes `window(r JOIN s)`.
fn push_down(op: &mut LogicalOperator, window_side: usize) {
    let mut children = std::mem::take(&mut op.children);
    let mut window = children.remove(window_side);
    let window_input = window.children.pop().expect("a window has a child");
    let other = children.pop().expect("a join has two children");
    let join_children = if window_side == 1 {
        vec![other, window_input]
    } else {
        vec![window_input, other]
    };
    let join = LogicalOperator::new(std::mem::replace(&mut op.kind, K::CrossProduct), join_children);
    window.children = vec![join];
    *op = window;
}

/// `ApplyFunctionalDependencies::GetUniqueColumnCombinations`: the sets of columns of `op` whose
/// values no two of its rows share.
pub(super) fn unique_column_combinations(op: &LogicalOperator) -> Vec<BTreeSet<ColumnBinding>> {
    match &op.kind {
        K::Filter { .. } | K::Limit { .. } | K::Order { .. } | K::Window { .. } => {
            unique_column_combinations(&op.children[0])
        }
        K::Projection {
            table_index,
            expressions,
        } => {
            let child_sets = unique_column_combinations(&op.children[0]);
            let forwarded = |binding: &ColumnBinding| {
                expressions
                    .iter()
                    .position(|e| matches!(&e.kind, BoundExprKind::ColumnRef { binding: b, depth: 0 } if b == binding))
            };
            child_sets
                .into_iter()
                .filter_map(|set| {
                    set.iter()
                        .map(|b| forwarded(b).map(|at| ColumnBinding::new(*table_index, at)))
                        .collect::<Option<BTreeSet<_>>>()
                })
                .collect()
        }
        K::Get {
            table,
            table_index,
            column_ids,
            projection,
            ..
        } => {
            if !op.children.is_empty() || column_ids.is_empty() {
                return Vec::new();
            }
            let binding_of = |column: usize| {
                let position = column_ids.iter().position(|&c| c == column)?;
                if let Some(projected) = projection
                    && !projected.contains(&position)
                {
                    return None;
                }
                Some(ColumnBinding::new(*table_index, position))
            };
            let mut result = Vec::new();
            for constraint in &table.constraints {
                let TableConstraint::Unique {
                    columns,
                    primary_key,
                    check_mode,
                    ..
                } = constraint
                else {
                    continue;
                };
                if *check_mode == ConstraintCheckMode::Deferred {
                    continue;
                }
                let key = columns
                    .iter()
                    .map(|&column| {
                        let not_null = *primary_key || table.columns.get(column).is_some_and(|c| c.not_null);
                        binding_of(column).filter(|_| not_null)
                    })
                    .collect::<Option<BTreeSet<_>>>();
                if let Some(key) = key
                    && !key.is_empty()
                {
                    result.push(key);
                }
            }
            result
        }
        K::Aggregate {
            group_index,
            groups,
            grouping_sets,
            grouping_functions,
            ..
        } => {
            if groups.is_empty() || grouping_sets.len() > 1 || !grouping_functions.is_empty() {
                return Vec::new();
            }
            let key: BTreeSet<ColumnBinding> = match grouping_sets.first() {
                None => (0..groups.len()).map(|i| ColumnBinding::new(*group_index, i)).collect(),
                Some(set) => {
                    if set.iter().any(|&i| i >= groups.len()) {
                        return Vec::new();
                    }
                    set.iter().map(|&i| ColumnBinding::new(*group_index, i)).collect()
                }
            };
            if key.is_empty() { Vec::new() } else { vec![key] }
        }
        // the keys of DISTINCT ON are unique too
        K::Distinct { targets, .. } => {
            let key = targets
                .iter()
                .map(|target| match &target.kind {
                    BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
                    _ => None,
                })
                .collect::<Option<BTreeSet<_>>>();
            match key {
                Some(key) if !key.is_empty() => vec![key],
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}
