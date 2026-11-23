// Ported from duckdb/src/planner/binder/tableref/plan_joinref.cpp (ExtractJoinConditions),
// duckdb/src/planner/operator/logical_comparison_join.cpp (HasEquality) and
// duckdb/src/execution/physical_plan/plan_comparison_join.cpp (PlanComparisonJoin) @ rust-port-baseline
//
//! The comparisons of joins. C++ keeps every comparison between the two sides of a join as a join
//! condition from the binder on; duckrs keeps only the equalities there while the optimizer runs,
//! and the others in the join's predicate. Once the optimizer is done, this moves them to the
//! conditions and picks the operator the join becomes, as `PlanComparisonJoin` does: a hash join
//! with an equality, else an IE join, a piecewise merge join or a nested loop join, by the number
//! of range comparisons, the estimated rows of the two sides and the settings.

use super::join_order::estimated_cardinality;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{JoinAlgorithm, JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::HashSet;

/// The settings `PlanComparisonJoin` reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangeJoinOptions {
    /// `nested_loop_join_threshold`
    pub nested_loop_join_threshold: u64,
    /// `merge_join_threshold`
    pub merge_join_threshold: u64,
    /// `prefer_range_joins`
    pub prefer_range_joins: bool,
    /// the plan is the recursive part of a recursive CTE (`recursive_cte_tables` is not empty)
    pub in_recursive_cte: bool,
}

impl Default for RangeJoinOptions {
    fn default() -> Self {
        Self {
            nested_loop_join_threshold: 5,
            merge_join_threshold: 1000,
            prefer_range_joins: false,
            in_recursive_cte: false,
        }
    }
}

/// Where a part of a condition reads from (`JoinSide`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    None,
    Left,
    Right,
    Both,
}

fn combine(a: Side, b: Side) -> Side {
    match (a, b) {
        (Side::None, other) | (other, Side::None) => other,
        (a, b) if a == b => a,
        _ => Side::Both,
    }
}

/// `JoinSide::GetJoinSide`: the sides whose columns `expr` reads. Correlated columns are of neither.
fn side_of(expr: &BoundExpr, left: &HashSet<usize>, right: &HashSet<usize>) -> Side {
    let mut side = Side::None;
    walk(expr, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            let here = if left.contains(&binding.table_index) {
                Side::Left
            } else if right.contains(&binding.table_index) {
                Side::Right
            } else {
                Side::Both
            };
            side = combine(side, here);
        }
    });
    side
}

fn walk(expr: &BoundExpr, f: &mut impl FnMut(&BoundExpr)) {
    f(expr);
    for child in expr.children() {
        walk(child, f);
    }
}

fn tables(op: &LogicalOperator) -> HashSet<usize> {
    op.column_bindings().iter().map(|b| b.table_index).collect()
}

/// `FlipComparisonExpression`.
pub fn flip(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// `x < y`, `x <= y`, `x > y` or `x >= y`.
pub fn is_range(op: ComparisonOp) -> bool {
    matches!(
        op,
        ComparisonOp::LessThan
            | ComparisonOp::LessThanOrEqual
            | ComparisonOp::GreaterThan
            | ComparisonOp::GreaterThanOrEqual
    )
}

fn split_conjuncts(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr.kind {
        BoundExprKind::Conjunction { and: true, children } => {
            for child in children {
                split_conjuncts(child, out);
            }
        }
        kind => out.push(BoundExpr { kind, ..expr }),
    }
}

fn comparison(op: ComparisonOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        LogicalType::BOOLEAN,
    )
}

/// `JoinCondition::CreateExpression` of one condition.
pub fn condition_expression(condition: &JoinCondition) -> BoundExpr {
    comparison(condition.comparison, condition.left.clone(), condition.right.clone())
}

/// The join types whose comparisons can be conditions of a range join here: those the piecewise
/// merge join and the IE join run (`ResolveComplexJoin` and `ResolveSimpleJoin`). MARK, SINGLE and
/// the RIGHT SEMI and ANTI of a build side keep their predicates.
fn takes_conditions(join_type: JoinType) -> bool {
    matches!(
        join_type,
        JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Outer | JoinType::Semi | JoinType::Anti
    )
}

/// `CreateJoinCondition`: `expr` as a condition, if it compares the two sides.
fn create_condition(expr: &BoundExpr, left: &HashSet<usize>, right: &HashSet<usize>) -> Option<JoinCondition> {
    let BoundExprKind::Comparison { op, left: a, right: b } = &expr.kind else {
        return None;
    };
    let (a_side, b_side) = (side_of(a, left, right), side_of(b, left, right));
    // a NONE side is a constant: C++ takes those too, but then the comparison is not BOTH
    let normal = a_side == Side::Left && b_side == Side::Right;
    let reversed = a_side == Side::Right && b_side == Side::Left;
    if !(normal || reversed) {
        return None;
    }
    let (l, r, op) = if normal {
        ((**a).clone(), (**b).clone(), *op)
    } else {
        ((**b).clone(), (**a).clone(), flip(*op))
    };
    Some(JoinCondition {
        bound: None,
        left: l,
        right: r,
        comparison: op,
        filter: None,
    })
}

/// `ExtractJoinConditions` for the predicate of a join whose equalities are conditions already.
/// `BoundBetweenExpression` of a column is two comparisons in C++ (`BindBetweenExpression`).
fn extract(op: &mut LogicalOperator) {
    let left = tables(&op.children[0]);
    let right = tables(&op.children[1]);
    let K::Join {
        conditions, predicate, ..
    } = &mut op.kind
    else {
        return;
    };
    let Some(expr) = predicate.take() else {
        return;
    };
    let mut conjuncts = Vec::new();
    split_conjuncts(expr, &mut conjuncts);
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        let pieces = match conjunct.kind {
            BoundExprKind::Between { input, lower, upper } if matches!(input.kind, BoundExprKind::ColumnRef { .. }) => {
                let pieces = vec![
                    comparison(ComparisonOp::GreaterThanOrEqual, (*input).clone(), *lower.clone()),
                    comparison(ComparisonOp::LessThanOrEqual, (*input).clone(), *upper.clone()),
                ];
                // only when the two of them are conditions: otherwise it stays as it was
                if pieces.iter().all(|p| create_condition(p, &left, &right).is_some()) {
                    pieces
                } else {
                    residual.push(BoundExpr {
                        kind: BoundExprKind::Between { input, lower, upper },
                        ..conjunct
                    });
                    continue;
                }
            }
            kind => vec![BoundExpr { kind, ..conjunct }],
        };
        for piece in pieces {
            match create_condition(&piece, &left, &right) {
                Some(condition) => conditions.push(condition),
                None => residual.push(piece),
            }
        }
    }
    *predicate = (!residual.is_empty()).then(|| super::conjunction_of(residual));
}

/// `LogicalComparisonJoin::HasEquality`: whether a condition is an equality, and how many are ranges.
fn has_equality(conditions: &[JoinCondition]) -> (bool, usize) {
    let equality = conditions.iter().any(JoinCondition::is_equality);
    let ranges = conditions.iter().filter(|c| is_range(c.comparison)).count();
    (equality, ranges)
}

/// `PhysicalNestedLoopJoin::IsSupported`.
fn nested_loop_supported(conditions: &[JoinCondition], join_type: JoinType) -> bool {
    if join_type == JoinType::Mark {
        return true;
    }
    for condition in conditions {
        let physical = condition.left.return_type.physical_type();
        if matches!(
            physical,
            PhysicalType::Struct | PhysicalType::List | PhysicalType::Array
        ) {
            return false;
        }
    }
    if conditions.is_empty() {
        return false;
    }
    if matches!(join_type, JoinType::Semi | JoinType::Anti) {
        return conditions.len() == 1;
    }
    true
}

/// `PlanComparisonJoin`: the operator of a join with conditions.
fn choose(op: &LogicalOperator, options: &RangeJoinOptions, after_cte: bool) -> JoinAlgorithm {
    let K::Join {
        join_type, conditions, ..
    } = &op.kind
    else {
        return JoinAlgorithm::Hash;
    };
    let join_type = *join_type;
    if conditions.is_empty() {
        return JoinAlgorithm::Hash;
    }
    let (equality, ranges) = has_equality(conditions);
    let mut can_merge = ranges > 0;
    let mut can_iejoin = ranges >= 2 && !after_cte && !options.in_recursive_cte;
    match join_type {
        JoinType::Semi | JoinType::Anti | JoinType::Mark => can_merge = can_merge && conditions.len() == 1,
        JoinType::RightAnti | JoinType::RightSemi => {
            can_merge = can_merge && conditions.len() == 1;
            can_iejoin = false;
        }
        _ => {}
    }
    let prefer_range_joins = options.prefer_range_joins && can_iejoin;
    if equality && !prefer_range_joins {
        return JoinAlgorithm::Hash;
    }
    let left_rows = estimated_cardinality(&op.children[0]);
    let right_rows = estimated_cardinality(&op.children[1]);
    let threshold = options.nested_loop_join_threshold;
    if left_rows < threshold || right_rows < threshold {
        can_iejoin = false;
        can_merge = false;
    }
    if can_merge && can_iejoin {
        let threshold = options.merge_join_threshold;
        if left_rows < threshold || right_rows < threshold {
            can_iejoin = false;
        }
    }
    if can_iejoin {
        JoinAlgorithm::IeJoin
    } else if can_merge {
        JoinAlgorithm::PiecewiseMerge
    } else if nested_loop_supported(conditions, join_type) {
        JoinAlgorithm::NestedLoop
    } else {
        JoinAlgorithm::Hash
    }
}

/// Moves the comparisons that are not equalities of a join back to its predicate.
fn restore(op: &mut LogicalOperator) {
    let K::Join {
        conditions,
        predicate,
        algorithm,
        ..
    } = &mut op.kind
    else {
        return;
    };
    *algorithm = JoinAlgorithm::Hash;
    if conditions.iter().all(JoinCondition::is_equality) {
        return;
    }
    let (kept, moved): (Vec<_>, Vec<_>) = std::mem::take(conditions)
        .into_iter()
        .partition(JoinCondition::is_equality);
    *conditions = kept;
    let mut expressions: Vec<BoundExpr> = moved.iter().map(condition_expression).collect();
    expressions.extend(predicate.take());
    *predicate = Some(super::conjunction_of(expressions));
}

/// Undoes [`plan_range_joins`], for a plan that goes through the optimizer again.
pub fn restore_predicates(op: &mut LogicalOperator) {
    for child in &mut op.children {
        restore_predicates(child);
    }
    restore(op);
}

/// The comparisons of the joins of `plan` as their conditions, and their operators. Joins come
/// in the order `PhysicalPlanGenerator` plans them, children first, as a CTE before them rules
/// out IE joins (`recursive_cte_tables`).
pub fn plan_range_joins(plan: &mut LogicalOperator, options: &RangeJoinOptions) {
    let mut after_cte = false;
    visit(plan, options, &mut after_cte);
}

fn visit(op: &mut LogicalOperator, options: &RangeJoinOptions, after_cte: &mut bool) {
    if matches!(op.kind, K::MaterializedCte { .. } | K::RecursiveCte { .. }) {
        *after_cte = true;
    }
    for child in &mut op.children {
        visit(child, options, after_cte);
    }
    let K::Join { join_type, .. } = &op.kind else {
        return;
    };
    if !takes_conditions(*join_type) {
        return;
    }
    extract(op);
    let algorithm = choose(op, options, *after_cte);
    if algorithm == JoinAlgorithm::Hash && !has_equality(join_conditions(op)).0 {
        // `PhysicalBlockwiseNLJoin` has them all as one expression
        restore(op);
        return;
    }
    if let K::Join { algorithm: slot, .. } = &mut op.kind {
        *slot = algorithm;
    }
}

fn join_conditions(op: &LogicalOperator) -> &[JoinCondition] {
    match &op.kind {
        K::Join { conditions, .. } => conditions,
        _ => &[],
    }
}

/// `PhysicalComparisonJoin::ReorderConditions`: equalities first, then the other comparisons.
pub fn reorder_conditions(conditions: &mut [JoinCondition]) {
    conditions.sort_by_key(|c| !c.is_equality());
}

/// `PhysicalRangeJoin::LessThan` without the distinct counts of the sides' statistics, which
/// duckrs does not keep on conditions: ranges first, narrower constant-size types among them first.
pub fn range_order(conditions: &mut [JoinCondition]) {
    let rank = |c: &JoinCondition| -> (u8, u8, usize) {
        let physical = c.right.return_type.physical_type();
        let width = (u8::from(!physical.is_constant_size()), physical.size());
        if is_range(c.comparison) {
            (0, width.0, if width.0 == 0 { width.1 } else { 0 })
        } else {
            (1, 0, 0)
        }
    };
    conditions.sort_by_key(rank);
}
