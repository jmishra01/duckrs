// Ported from duckdb/src/optimizer/projection_pullup.cpp @ rust-port-baseline
//
//! `ProjectionPullup`: a projection below a run of joins and filters goes away when it only
//! passes columns on, and otherwise moves above the run, so that the join order optimizer sees
//! the joins together.
//!
//! C++ walks the plan once with a stack of parents. Here each step finds one projection that can
//! move, by its path, and the plan is searched again until none can.

use super::common_subplan::{node, node_mut, visit_bindings};
use super::rewriter::can_throw;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_parser::ast::JoinType;
use std::collections::{HashMap, HashSet};

type Path = Vec<usize>;

/// `ProjectionPullup::Optimize`.
pub fn pull_up(mut plan: LogicalOperator) -> LogicalOperator {
    // each step removes a projection, or moves one above everything it can pass
    for _ in 0..100_000 {
        let Some(step) = find(&plan, &plan, &mut Vec::new(), &[], false) else {
            break;
        };
        apply(&mut plan, step);
    }
    plan
}

/// What to do with the projection at `projection`.
enum Step {
    /// it only passes columns on: drop it (`PullUpColrefProjection`)
    Remove { projection: Path },
    /// put it above the operator at `top` (`PullUpNonColrefProjection`)
    Move { projection: Path, top: Path },
}

/// Whether a projection can be pulled through `op`, coming from its child `at`.
fn pull_through(op: &LogicalOperator, at: usize) -> bool {
    match &op.kind {
        K::Filter { .. } => true,
        K::Join { join_type, .. } => match join_type {
            JoinType::Mark | JoinType::Single => false,
            // the right side of a semi or anti join is a barrier
            JoinType::Semi | JoinType::Anti => at == 0,
            _ => true,
        },
        _ => false,
    }
}

/// `LogicalOperatorVisitor::EnumerateExpressions` of the operators a projection passes.
fn parent_expressions(op: &LogicalOperator) -> Vec<&BoundExpr> {
    match &op.kind {
        K::Filter { expressions } => expressions.iter().collect(),
        K::Join {
            conditions, predicate, ..
        } => conditions
            .iter()
            .flat_map(|c| [&c.left, &c.right])
            .chain(predicate.iter())
            .collect(),
        _ => Vec::new(),
    }
}

fn references(expr: &BoundExpr, f: &mut impl FnMut(&ColumnBinding)) {
    if let BoundExprKind::ColumnRef { binding, .. } = &expr.kind {
        f(binding);
    }
    for child in expr.children() {
        references(child, f);
    }
}

fn is_column_ref(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::ColumnRef { depth: 0, .. })
}

/// `HasJoinBelow`.
fn has_join_below(op: &LogicalOperator) -> bool {
    let mut current = op;
    while current.children.len() == 1 {
        current = &current.children[0];
    }
    current.children.len() > 1
}

/// Whether the columns of child `at` of `op` may change their order and number: whether what
/// reads them goes by binding, up to an operator that makes columns of its own. A plan's root,
/// a set operation and the like read their input by position (C++ keeps projections under those).
pub(super) fn child_layout_free(op: &LogicalOperator, at: usize, free: bool) -> bool {
    match &op.kind {
        K::Projection { .. } | K::Aggregate { .. } => true,
        K::Filter { .. }
        | K::Order { .. }
        | K::TopN { .. }
        | K::Limit { .. }
        | K::LimitPercent { .. }
        | K::Sample { .. }
        | K::Window { .. }
        | K::Unnest { .. }
        | K::CrossProduct
        | K::PositionalJoin
        | K::MarkJoin { .. }
        | K::AsOfJoin { .. } => free,
        K::Join { join_type, .. } => match (join_type, at) {
            (JoinType::Semi | JoinType::Anti | JoinType::Mark, 1) | (JoinType::RightSemi | JoinType::RightAnti, 0) => {
                true
            }
            _ => free,
        },
        K::MaterializedCte { .. } => at == 1 && free,
        _ => false,
    }
}

/// Finds the first projection that can move, depth first. `chain` holds the paths of the
/// operators directly above `op` that a projection can be pulled through, nearest last; with
/// `free`, the columns of `op` may change their order and number.
fn find(root: &LogicalOperator, op: &LogicalOperator, path: &mut Path, chain: &[Path], free: bool) -> Option<Step> {
    if free && let Some(step) = consider(root, op, path, chain) {
        return Some(step);
    }
    // a projection that stays is a barrier, as is any operator a projection cannot pass
    for at in 0..op.children.len() {
        let child_chain = if pull_through(op, at) {
            let mut chain = chain.to_vec();
            chain.push(path.clone());
            chain
        } else {
            Vec::new()
        };
        path.push(at);
        let step = find(
            root,
            &op.children[at],
            path,
            &child_chain,
            child_layout_free(op, at, free),
        );
        path.pop();
        if step.is_some() {
            return step;
        }
    }
    None
}

/// The projection case of `ProjectionPullup::VisitOperator`.
fn consider(root: &LogicalOperator, op: &LogicalOperator, path: &Path, chain: &[Path]) -> Option<Step> {
    let K::Projection {
        table_index,
        expressions,
    } = &op.kind
    else {
        return None;
    };
    if chain.is_empty() || expressions.iter().any(|e| e.is_volatile() || can_throw(e)) {
        return None;
    }
    let outputs: HashMap<ColumnBinding, &BoundExpr> = expressions
        .iter()
        .enumerate()
        .map(|(i, e)| (ColumnBinding::new(*table_index, i), e))
        .collect();
    // `CanPullThrough`: the operators above read only the outputs that are column references,
    // and above a join, no expression that can throw reads the projection (filter pushdown kept
    // it above the projection, away from the join order optimizer)
    let throwing_filters_stay = has_join_below(op);
    for parent in chain {
        for expr in parent_expressions(node(root, parent)) {
            let mut reads_projection = false;
            let mut reads_computed = false;
            references(expr, &mut |binding| {
                if let Some(output) = outputs.get(binding) {
                    reads_projection = true;
                    reads_computed |= !is_column_ref(output);
                }
            });
            if reads_computed || (throwing_filters_stay && reads_projection && can_throw(expr)) {
                return None;
            }
        }
    }
    // the projection above an UNNEST takes only the fields it needs out of what it unnests
    if matches!(op.children[0].kind, K::Unnest { .. }) {
        return None;
    }
    if expressions.iter().all(is_column_ref) {
        // above a DISTINCT, which keeps all of its columns, it keeps the columns it drops apart
        if matches!(op.children[0].kind, K::Distinct { .. }) {
            return None;
        }
        return Some(Step::Remove {
            projection: path.clone(),
        });
    }
    // computed expressions do not go through outer joins, where they would see the NULLs of rows
    // without a match
    let outer = chain.iter().any(|parent| {
        matches!(
            node(root, parent).kind,
            K::Join {
                join_type: JoinType::Left | JoinType::Right | JoinType::Outer,
                ..
            }
        )
    });
    if outer {
        return None;
    }
    Some(Step::Move {
        projection: path.clone(),
        top: chain[0].clone(),
    })
}

/// `ColumnBindingReplacer` on the plan at `op`, except under `skip`.
fn replace(
    op: &mut LogicalOperator,
    path: &mut Path,
    skip: Option<&[usize]>,
    map: &HashMap<ColumnBinding, ColumnBinding>,
) {
    if skip == Some(path.as_slice()) || map.is_empty() {
        return;
    }
    for at in 0..op.children.len() {
        path.push(at);
        replace(&mut op.children[at], path, skip, map);
        path.pop();
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}

fn apply(plan: &mut LogicalOperator, step: Step) {
    match step {
        Step::Remove { projection } => {
            let slot = node_mut(plan, &projection);
            let K::Projection {
                table_index,
                expressions,
            } = &slot.kind
            else {
                unreachable!("a projection")
            };
            let map: HashMap<ColumnBinding, ColumnBinding> = expressions
                .iter()
                .enumerate()
                .filter_map(|(i, e)| match &e.kind {
                    BoundExprKind::ColumnRef { binding, .. } => Some((ColumnBinding::new(*table_index, i), *binding)),
                    _ => None,
                })
                .collect();
            let child = slot.children.remove(0);
            *slot = child;
            replace(plan, &mut Vec::new(), None, &map);
        }
        Step::Move { projection, top } => {
            let (table_index, mut expressions) = match &node(plan, &projection).kind {
                K::Projection {
                    table_index,
                    expressions,
                } => (*table_index, expressions.clone()),
                _ => unreachable!("a projection"),
            };
            // the operators it passes read its column references' sources
            let passed: HashMap<ColumnBinding, ColumnBinding> = expressions
                .iter()
                .enumerate()
                .filter_map(|(i, e)| match &e.kind {
                    BoundExprKind::ColumnRef { binding, .. } => Some((ColumnBinding::new(table_index, i), *binding)),
                    _ => None,
                })
                .collect();
            replace(node_mut(plan, &top), &mut Vec::new(), None, &passed);
            let top_op = node(plan, &top);
            let (top_bindings, top_types) = (top_op.column_bindings(), top_op.types());
            let own: HashSet<ColumnBinding> = (0..expressions.len())
                .map(|i| ColumnBinding::new(table_index, i))
                .collect();
            let slot = node_mut(plan, &projection);
            let child = slot.children.remove(0);
            *slot = child;
            // the columns of `top` the projection did not make pass through it
            let mut above = HashMap::new();
            for (binding, ty) in top_bindings.into_iter().zip(top_types) {
                if own.contains(&binding) {
                    continue;
                }
                above.insert(binding, ColumnBinding::new(table_index, expressions.len()));
                expressions.push(BoundExpr::new(BoundExprKind::ColumnRef { binding, depth: 0 }, ty));
            }
            replace(plan, &mut Vec::new(), Some(&top), &above);
            let slot = node_mut(plan, &top);
            let below = std::mem::replace(slot, LogicalOperator::new(K::CrossProduct, Vec::new()));
            *slot = LogicalOperator::new(
                K::Projection {
                    table_index,
                    expressions,
                },
                vec![below],
            );
        }
    }
}
