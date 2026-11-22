// Ported from duckdb/src/optimizer/cse_optimizer.cpp @ rust-port-baseline
//
//! `CommonSubExpressionOptimizer`: an expression that a projection or an aggregate computes more than
//! once goes into a new projection below it, computed once there, and is read from it. The new
//! projection also passes on every column the operator reads.
//!
//! Expressions are told apart by their whole structure, types included, with their aliases left
//! out (`Expression::Equals`). An operator whose expressions reach outside of it (subqueries,
//! correlated columns, lambdas, sequences) or whose aggregates tell rows apart by a collated key
//! is left as it is.

use super::common_subplan::max_table_index;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use std::collections::{HashMap, HashSet};

/// Runs the pass over a plan.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    let mut next_index = max_table_index(&plan) + 1;
    visit(&mut plan, &mut next_index);
    plan
}

/// `CSENode`: how often an expression occurs, and its column in the new projection once it has one.
#[derive(Default)]
struct Node {
    count: usize,
    column: Option<usize>,
}

/// `CSEReplacementState`.
#[derive(Default)]
struct State {
    projection_index: usize,
    counts: HashMap<String, Node>,
    /// the column of the new projection of each column the operator reads
    columns: HashMap<ColumnBinding, usize>,
    expressions: Vec<BoundExpr>,
    short_circuited: bool,
    /// in the child of a TRY or HAS_ERROR, whose errors must stay inside it
    inside_try: bool,
    /// ungrouped aggregates evaluate their filters themselves, so each filter's root stays there
    keep_aggregate_filters: bool,
    counted_filters: HashSet<String>,
}

/// What tells an expression apart: all of it but its aliases.
fn key(expr: &BoundExpr) -> String {
    let mut expr = expr.clone();
    expr.walk_mut(&mut |e| e.alias.clear());
    format!("{expr:?}")
}

fn visit(op: &mut LogicalOperator, next_index: &mut usize) {
    if matches!(op.kind, K::Projection { .. } | K::Aggregate { .. }) && op.children.len() == 1 {
        extract(op, next_index);
    }
    for child in &mut op.children {
        visit(child, next_index);
    }
    if let K::RecursiveCte { recursive, .. } | K::LateralJoin { right: recursive, .. } = &mut op.kind {
        visit(recursive, next_index);
    }
}

/// Whether an expression reads only columns of the operator's child, by binding.
fn movable(expr: &BoundExpr) -> bool {
    let here = match &expr.kind {
        BoundExprKind::ColumnRef { depth, .. } => *depth == 0,
        BoundExprKind::Reference(_)
        | BoundExprKind::Subquery { .. }
        | BoundExprKind::ListLambda { .. }
        | BoundExprKind::Sequence { .. }
        | BoundExprKind::JoinFilter { .. } => false,
        _ => true,
    };
    here && expr.children().into_iter().all(movable)
}

fn aggregate_expressions(aggregate: &BoundAggregate) -> impl Iterator<Item = &BoundExpr> {
    aggregate
        .children
        .iter()
        .chain(aggregate.filter.iter())
        .chain(aggregate.order_by.iter().map(|(expr, _)| expr))
}

/// `ExtractCommonSubExpressions`.
fn extract(op: &mut LogicalOperator, next_index: &mut usize) {
    let mut state = State::default();
    match &op.kind {
        K::Projection { expressions, .. } => {
            if !expressions.iter().all(movable) {
                return;
            }
            for expr in expressions {
                count(expr, &mut state, false);
            }
        }
        K::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            ..
        } => {
            if aggregates.iter().any(|a| a.distinct_key.is_some())
                || !groups.iter().all(movable)
                || !aggregates.iter().flat_map(aggregate_expressions).all(movable)
            {
                return;
            }
            state.keep_aggregate_filters = groups.is_empty() && grouping_sets.len() <= 1;
            for group in groups {
                count(group, &mut state, false);
            }
            for aggregate in aggregates {
                count_aggregate(aggregate, &mut state);
            }
        }
        _ => return,
    }
    if !state.counts.values().any(|node| node.count > 1) {
        return;
    }
    state.projection_index = *next_index;
    *next_index += 1;
    match &mut op.kind {
        K::Projection { expressions, .. } => {
            for expr in expressions {
                replace(expr, &mut state, false);
            }
        }
        K::Aggregate { groups, aggregates, .. } => {
            for group in groups {
                replace(group, &mut state, false);
            }
            for aggregate in aggregates {
                let kept = state.keep_aggregate_filters && aggregate.filter.as_ref().is_some_and(|f| !f.is_volatile());
                for child in &mut aggregate.children {
                    replace(child, &mut state, false);
                }
                if let Some(filter) = &mut aggregate.filter {
                    replace(filter, &mut state, kept);
                }
                for (expr, _) in &mut aggregate.order_by {
                    replace(expr, &mut state, false);
                }
            }
        }
        _ => unreachable!("only projections and aggregates have common subexpressions extracted"),
    }
    let child = op.children.pop().expect("one child");
    let projection = LogicalOperator {
        kind: K::Projection {
            table_index: state.projection_index,
            expressions: state.expressions,
        },
        children: vec![child],
    };
    op.children.push(projection);
}

/// The children of an aggregate, its filter and its ORDER BY keys, as `ExpressionIterator` gives them:
/// the aggregate itself can't move into a projection.
fn count_aggregate(aggregate: &BoundAggregate, state: &mut State) {
    for child in &aggregate.children {
        count(child, state, false);
    }
    if let Some(filter) = &aggregate.filter {
        let kept = state.keep_aggregate_filters && !filter.is_volatile();
        count(filter, state, kept);
    }
    for (expr, _) in &aggregate.order_by {
        count(expr, state, false);
    }
}

fn is_try(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Operator {
            op: BoundOperator::Try | BoundOperator::HasError,
            ..
        }
    )
}

/// `CountExpressions`; `kept_filter_root` when `expr` is an aggregate's filter that stays with it.
fn count(expr: &BoundExpr, state: &mut State, kept_filter_root: bool) {
    // only expressions with children are worth computing once
    if matches!(expr.kind, BoundExprKind::ColumnRef { .. } | BoundExprKind::Constant(_)) {
        return;
    }
    let expr_key = key(expr);
    if kept_filter_root && !state.counted_filters.insert(expr_key.clone()) {
        return;
    }
    if !expr.is_volatile() && !state.inside_try && !kept_filter_root {
        match state.counts.get_mut(&expr_key) {
            Some(node) => node.count += 1,
            // an inner argument of an expression that short-circuits might not run for every row
            None if !state.short_circuited => {
                state.counts.insert(expr_key, Node { count: 1, column: None });
            }
            None => {}
        }
    }
    match &expr.kind {
        // COALESCE too evaluates an operand only on the rows the ones before it left NULL; C++ keeps a
        // short IN as one expression, where the binder here spells it out as ORs of its probe
        BoundExprKind::Conjunction { .. }
        | BoundExprKind::Case { .. }
        | BoundExprKind::Operator {
            op: BoundOperator::Coalesce,
            ..
        } => {
            let saved = state.short_circuited;
            for child in expr.children() {
                count(child, state, false);
                state.short_circuited = true;
            }
            state.short_circuited = saved;
        }
        _ => {
            let saved = state.inside_try;
            state.inside_try |= is_try(expr);
            for child in expr.children() {
                count(child, state, false);
            }
            state.inside_try = saved;
        }
    }
}

/// `PerformCSEReplacement`.
fn replace(expr: &mut BoundExpr, state: &mut State, kept_filter_root: bool) {
    if let BoundExprKind::ColumnRef { binding, .. } = &mut expr.kind {
        let column = match state.columns.get(binding) {
            Some(&column) => column,
            None => {
                let column = state.expressions.len();
                state.expressions.push(BoundExpr::column_ref(
                    *binding,
                    expr.return_type.clone(),
                    expr.alias.clone(),
                ));
                state.columns.insert(*binding, column);
                column
            }
        };
        *binding = ColumnBinding::new(state.projection_index, column);
        return;
    }
    if !state.inside_try && !kept_filter_root {
        let expr_key = key(expr);
        if let Some(node) = state.counts.get_mut(&expr_key)
            && node.count > 1
        {
            let column = match node.column {
                Some(column) => column,
                None => {
                    let column = state.expressions.len();
                    state.expressions.push(expr.clone());
                    node.column = Some(column);
                    column
                }
            };
            let binding = ColumnBinding::new(state.projection_index, column);
            *expr = BoundExpr::column_ref(binding, expr.return_type.clone(), expr.alias.clone());
            return;
        }
    }
    let saved = state.inside_try;
    state.inside_try |= is_try(expr);
    for child in expr.children_mut() {
        replace(child, state, false);
    }
    state.inside_try = saved;
}
