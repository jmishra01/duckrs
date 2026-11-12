// Ported from duckdb/src/optimizer/deliminator.cpp @ rust-port-baseline
//
//! `Deliminator`: a flattened subquery reads the distinct values of its correlated columns through
//! `DelimGet`, and an equality join with those keeps the rows of the subquery that the join around
//! it can match. That join only filters, so it can go: the rows it would have dropped match nothing
//! there, and the columns of `DelimGet` are the other side's. Without any `DelimGet` left the
//! `DelimJoin` is an ordinary join.
//!
//! When the query filters the rows of a `DelimJoin`, the deepest such join stays, since the
//! distinct values then cut down what the subquery has to read. An existence test in a WHERE clause
//! is the exception: it ends up a SEMI or ANTI join, whose join filter does that.
//!
//! This differs from C++ in three ways. Inequalities are predicates here, and
//! `RemoveInequalityJoinWithDelimGet` moves them to the predicate of a MARK `DelimJoin` only, in a
//! form that is never NULL. A SINGLE join stays a `DelimJoin`, as
//! that is the operator that raises the error for more than one row. And `DelimGet`s are told apart
//! by what they read, so one that belongs to this join but sits in a place this pass does not look
//! keeps the `DelimJoin` in place.

use super::conjunction_of;
use super::rewriter::split_conjuncts;
use super::statistics::flip;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::Arc;

type Delim = Arc<Mutex<ColumnDataCollection>>;
/// The children to go through from some operator to reach another.
type Path = Vec<usize>;

/// `Deliminator::Optimize`.
pub fn deliminate(plan: &mut LogicalOperator) {
    let mut delim_joins = Vec::new();
    find_delim_joins(plan, &mut Vec::new(), &mut delim_joins);
    // a join is removed after the ones below it, so the paths of the others stay right
    for path in delim_joins {
        deliminate_join(plan, &path);
    }
}

/// `Deliminator::FindCandidates`, the joins below first.
fn find_delim_joins(op: &LogicalOperator, path: &mut Path, found: &mut Vec<Path>) {
    for (index, child) in op.children.iter().enumerate() {
        path.push(index);
        find_delim_joins(child, path, found);
        path.pop();
    }
    if matches!(op.kind, K::DelimJoin { .. }) {
        found.push(path.clone());
    }
}

fn operator_at<'a>(mut op: &'a mut LogicalOperator, path: &[usize]) -> &'a mut LogicalOperator {
    for index in path {
        op = &mut op.children[*index];
    }
    op
}

fn deliminate_join(root: &mut LogicalOperator, path: &[usize]) {
    let filtered = marker_is_filtered(root, path);
    let op = operator_at(root, path);
    let K::DelimJoin { delim, .. } = &op.kind else {
        unreachable!("a path of a delim join")
    };
    let delim = delim.clone();
    let mut joins = Vec::new();
    find_joins_with_delim_get(&op.children[1], &delim, &mut vec![1], &mut joins);
    let delim_gets = count_delim_gets(op, &delim);
    joins.sort_by_key(|join| Reverse(join.len()));
    let join_count = joins.len();
    let mut all_removed = true;
    // without its `DelimGet`s an existence test is a SEMI or ANTI join, which gives the scan of the
    // subquery a filter of the keys that does what the distinct values did
    let as_semi = filtered && joins.len() == 1 && is_straight(op, &joins[0]);
    if !joins.is_empty() && has_selection(op) && !as_semi {
        joins.remove(0);
        all_removed = false;
    }
    for join in joins {
        match remove_join_with_delim_get(root, path, &join, &delim) {
            Some(replacements) => replace_columns(root, &replacements),
            None => all_removed = false,
        }
    }
    let op = operator_at(root, path);
    try_switch_single_to_left(op);
    if all_removed && join_count == delim_gets {
        to_ordinary_join(op);
    }
}

/// Whether the `DelimJoin` at `path` is a MARK join under a filter that keeps the rows whose marker
/// is true, or those whose marker is false: `EXISTS` and `NOT EXISTS` in a WHERE clause.
fn marker_is_filtered(root: &mut LogicalOperator, path: &[usize]) -> bool {
    let Some((_, parent)) = path.split_last() else {
        return false;
    };
    let K::DelimJoin {
        mark_index: Some(mark_index),
        ..
    } = operator_at(root, path).kind
    else {
        return false;
    };
    let marker = ColumnBinding::new(mark_index, 0);
    let K::Filter { expressions } = &operator_at(root, parent).kind else {
        return false;
    };
    expressions.iter().any(|expr| match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::Not,
            children,
        } => column_of(&children[0]) == Some(marker),
        _ => column_of(expr) == Some(marker),
    })
}

/// Whether only filters and projections are between the right side of `op` and the join at `join`.
fn is_straight(op: &LogicalOperator, join: &[usize]) -> bool {
    let mut current = &op.children[1];
    for step in &join[1..] {
        if *step != 0 || !matches!(current.kind, K::Filter { .. } | K::Projection { .. }) {
            return false;
        }
        current = &current.children[0];
    }
    true
}

/// What column of the join at `join` the column `binding` of the right side of `op` is.
fn trace(op: &LogicalOperator, join: &[usize], mut binding: ColumnBinding) -> Option<ColumnBinding> {
    let mut current = &op.children[1];
    for _ in &join[1..] {
        if let K::Projection {
            table_index,
            expressions,
        } = &current.kind
        {
            if binding.table_index != *table_index {
                return None;
            }
            binding = column_of(expressions.get(binding.column_index)?)?;
        }
        current = &current.children[0];
    }
    Some(binding)
}

/// `Deliminator::FindJoinWithDelimGet`. The right side of a nested `DelimJoin` has its own.
fn find_joins_with_delim_get(op: &LogicalOperator, delim: &Delim, path: &mut Path, found: &mut Vec<Path>) {
    let children = if matches!(op.kind, K::DelimJoin { .. }) {
        &op.children[..1]
    } else {
        &op.children[..]
    };
    for (index, child) in children.iter().enumerate() {
        path.push(index);
        find_joins_with_delim_get(child, delim, path, found);
        path.pop();
    }
    if matches!(op.kind, K::Join { .. }) && op.children.iter().any(|child| is_delim_get(child, delim)) {
        found.push(path.clone());
    }
}

fn count_delim_gets(op: &LogicalOperator, delim: &Delim) -> usize {
    let own = matches!(&op.kind, K::DelimGet { delim: other, .. } if Arc::ptr_eq(other, delim));
    let nested = match &op.kind {
        K::RecursiveCte { recursive: inner, .. } | K::LateralJoin { right: inner, .. } => {
            count_delim_gets(inner, delim)
        }
        _ => 0,
    };
    let below: usize = op.children.iter().map(|child| count_delim_gets(child, delim)).sum();
    usize::from(own) + nested + below
}

/// `OperatorIsDelimGet`.
fn is_delim_get(op: &LogicalOperator, delim: &Delim) -> bool {
    let reads_delim =
        |op: &LogicalOperator| matches!(&op.kind, K::DelimGet { delim: other, .. } if Arc::ptr_eq(other, delim));
    reads_delim(op) || matches!(op.kind, K::Filter { .. }) && reads_delim(&op.children[0])
}

/// `Deliminator::HasSelection`. A filter that only drops NULLs from a table drops next to nothing.
fn has_selection(op: &LogicalOperator) -> bool {
    if let K::Filter { expressions } = &op.kind {
        let only_not_null = matches!(op.children[0].kind, K::Get { .. }) && expressions.iter().all(is_not_null);
        if !only_not_null {
            return true;
        }
    }
    op.children.iter().any(has_selection)
}

fn is_not_null(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Operator {
            op: BoundOperator::IsNotNull,
            ..
        }
    )
}

/// `Deliminator::RemoveJoinWithDelimGet`: gives the columns of `DelimGet` and what they are now.
/// The join is at `join` from the `DelimJoin` at `path`.
///
/// `RemoveInequalityJoinWithDelimGet`: a comparison other than an equality between a column of
/// `DelimGet` and one of the other side is one between the outer value and that column, so it
/// becomes a predicate of the `DelimJoin` in place of the condition on that value.
fn remove_join_with_delim_get(
    root: &mut LogicalOperator,
    path: &[usize],
    join: &[usize],
    delim: &Delim,
) -> Option<Vec<(ColumnBinding, ColumnBinding)>> {
    let straight = is_straight(operator_at(root, path), join);
    let absolute: Path = path.iter().chain(join).copied().collect();
    let op = operator_at(root, &absolute);
    let K::Join {
        join_type,
        conditions,
        predicate,
    } = &op.kind
    else {
        return None;
    };
    let delim_side = usize::from(!is_delim_get(&op.children[0], delim));
    // a SEMI join with `DelimGet` on its left gives one row per value, and the other side many
    if !matches!((join_type, delim_side), (JoinType::Inner, _) | (JoinType::Semi, 1)) {
        return None;
    }
    let (mut filters, get) = match &op.children[delim_side].kind {
        K::Filter { expressions } => (expressions.clone(), &op.children[delim_side].children[0]),
        _ => (Vec::new(), &op.children[delim_side]),
    };
    let K::DelimGet {
        types,
        table_index: delim_index,
        ..
    } = &get.kind
    else {
        return None;
    };
    let is_not_null = |expr: &BoundExpr| {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::IsNotNull,
                children: vec![expr.clone()],
            },
            LogicalType::BOOLEAN,
        )
    };
    let mut replacements = Vec::new();
    let mut seen = HashSet::new();
    for condition in conditions {
        let (from, to) = if delim_side == 0 {
            (&condition.left, &condition.right)
        } else {
            (&condition.right, &condition.left)
        };
        let (Some(from_column), Some(to_column)) = (column_of(from), column_of(to)) else {
            return None;
        };
        if from.return_type != to.return_type || !seen.insert(from_column) {
            return None;
        }
        replacements.push((from_column, to_column));
        // the join dropped NULLs
        if !condition.null_safe {
            filters.push(is_not_null(to));
        }
    }
    // the columns of `DelimGet` that something other than an equality compares, as `other op column`
    let mut inequalities = Vec::new();
    if let Some(predicate) = predicate {
        if *join_type != JoinType::Inner || !straight {
            return None;
        }
        let mut conjuncts = Vec::new();
        split_conjuncts(predicate.clone(), &mut conjuncts);
        for conjunct in conjuncts {
            let BoundExprKind::Comparison { op, left, right } = &conjunct.kind else {
                return None;
            };
            let (Some(first), Some(second)) = (column_of(left), column_of(right)) else {
                return None;
            };
            let (from, to, op) = match (first.table_index == *delim_index, second.table_index == *delim_index) {
                (true, false) => (first, right, flip(*op)),
                (false, true) => (second, left, *op),
                _ => return None,
            };
            let compares_nulls = matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom);
            if compares_nulls || left.return_type != right.return_type || !seen.insert(from) {
                return None;
            }
            replacements.push((from, column_of(to)?));
            filters.push(is_not_null(to));
            inequalities.push((from, op));
        }
    }
    // a column that nothing compares adds rows
    if seen.len() != types.len() {
        return None;
    }
    // which conditions of the `DelimJoin` the inequalities take the place of
    let mut moved = Vec::new();
    if !inequalities.is_empty() {
        let K::DelimJoin {
            conditions,
            mark_index: Some(_),
            ..
        } = &operator_at(root, path).kind
        else {
            return None;
        };
        let count = conditions.len();
        let rights: Option<Vec<ColumnBinding>> = conditions.iter().map(|c| column_of(&c.right)).collect();
        let rights = rights?;
        for (column, op) in inequalities {
            let delim_join = operator_at(root, path);
            let at = rights
                .iter()
                .position(|right| trace(delim_join, join, *right) == Some(column))?;
            moved.push((at, op));
        }
        // a hash join needs an equality
        if moved.len() >= count {
            return None;
        }
    }
    let op = operator_at(root, &absolute);
    let other = op.children.swap_remove(1 - delim_side);
    *op = if filters.is_empty() {
        other
    } else {
        LogicalOperator::new(K::Filter { expressions: filters }, vec![other])
    };
    if moved.is_empty() {
        return Some(replacements);
    }
    let K::DelimJoin {
        conditions, predicate, ..
    } = &mut operator_at(root, path).kind
    else {
        unreachable!("looked at above")
    };
    let mut conjuncts: Vec<BoundExpr> = predicate.take().into_iter().collect();
    moved.sort_by_key(|(at, _)| Reverse(*at));
    for (at, op) in moved {
        let condition = conditions.remove(at);
        // no row of the subquery compares with a NULL, and the filter above took its own NULLs out
        conjuncts.push(is_not_null(&condition.left));
        let op = if op == ComparisonOp::NotEqual {
            ComparisonOp::DistinctFrom
        } else {
            op
        };
        conjuncts.push(BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(condition.right),
                right: Box::new(condition.left),
            },
            LogicalType::BOOLEAN,
        ));
    }
    *predicate = Some(conjunction_of(conjuncts));
    Some(replacements)
}

fn column_of(expr: &BoundExpr) -> Option<ColumnBinding> {
    match expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(binding),
        _ => None,
    }
}

/// `ColumnBindingReplacer`.
fn replace_columns(plan: &mut LogicalOperator, replacements: &[(ColumnBinding, ColumnBinding)]) {
    plan.visit_expressions(&mut |expr| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut expr.kind
            && let Some((_, to)) = replacements.iter().find(|(from, _)| from == binding)
        {
            *binding = *to;
        }
    });
}

/// `Deliminator::TrySwitchSingleToLeft`: with the join keys among the groups of an aggregate, there
/// is at most one right row for each left row.
fn try_switch_single_to_left(op: &mut LogicalOperator) {
    let K::DelimJoin {
        join_type: JoinType::Single,
        conditions,
        ..
    } = &op.kind
    else {
        return;
    };
    let Some(mut keys) = conditions
        .iter()
        .map(|condition| column_of(&condition.right))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let mut current = &op.children[1];
    let (group_index, group_count) = loop {
        match &current.kind {
            K::Aggregate {
                group_index,
                groups,
                grouping_sets,
                grouping_functions,
                ..
            } => {
                if !grouping_functions.is_empty() || grouping_sets.len() > 1 {
                    return;
                }
                break (*group_index, groups.len());
            }
            K::Projection {
                table_index,
                expressions,
            } => {
                for key in &mut keys {
                    let source = (key.table_index == *table_index)
                        .then(|| expressions.get(key.column_index))
                        .flatten()
                        .and_then(column_of);
                    let Some(source) = source else {
                        return;
                    };
                    *key = source;
                }
            }
            K::Filter { .. } => {}
            _ => return,
        }
        current = &current.children[0];
    };
    if (0..group_count).all(|group| keys.contains(&ColumnBinding::new(group_index, group))) {
        let K::DelimJoin { join_type, .. } = &mut op.kind else {
            unreachable!()
        };
        *join_type = JoinType::Left;
    }
}

/// What a `DelimJoin` without `DelimGet` is. See the top of this file for SINGLE.
fn to_ordinary_join(op: &mut LogicalOperator) {
    let K::DelimJoin {
        join_type,
        conditions,
        predicate,
        mark_index,
        ..
    } = &mut op.kind
    else {
        return;
    };
    let join_type = *join_type;
    if join_type == JoinType::Single {
        return;
    }
    let (conditions, predicate) = (std::mem::take(conditions), predicate.take());
    op.kind = match *mark_index {
        Some(table_index) => K::MarkJoin {
            table_index,
            conditions,
            predicate,
        },
        None => K::Join {
            join_type,
            conditions,
            predicate,
        },
    };
}
