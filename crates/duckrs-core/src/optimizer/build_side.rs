// Ported in spirit from duckdb/src/optimizer/build_probe_side_optimizer.cpp @ rust-port-baseline
//
//! `BuildProbeSideOptimizer`, for SEMI and ANTI joins: a hash join builds its right side, so one
//! whose right side is the larger becomes a RIGHT SEMI or RIGHT ANTI join with the sides swapped,
//! which gives the same rows. The larger side then only probes, and its scan can take a filter of
//! the keys of the smaller one.
//!
//! For inner joins, join order puts the side with the fewest rows on the right. Once the columns
//! nothing reads are gone, `choose_by_width` looks again with the width of the rows: a hash table
//! of few wide rows can cost more than one of many narrow rows, and the side that probes can take
//! a join filter and read its wide columns for fewer rows. C++ always goes by rows times width;
//! here that only decides where the rows differ in width several times over, as the estimates of
//! joins are too rough to weigh against a small difference (TPC-H Q9 would build on all of orders).
//!
//! LEFT and RIGHT joins are the other joins with an inverse type: join order leaves them where they
//! are, so the side to build on is the one with the fewest rows times width, as C++ goes by. A LEFT
//! join whose right side costs more becomes a RIGHT join with the sides swapped (TPC-H Q13 builds on
//! customer, not orders), and a RIGHT join whose left side costs more becomes a LEFT join.

use super::join_order::{PlanEstimate, estimate_plan};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_parser::ast::{ComparisonOp, JoinType};

/// With `prefer_range_joins`, a SEMI or ANTI join stays as it is: no range join runs RIGHT SEMI or
/// RIGHT ANTI.
pub fn choose_build_sides(plan: &mut LogicalOperator, prefer_range_joins: bool) {
    // an estimate reads the statistics of the tables, which a plan without such a join has no use for
    if prefer_range_joins || !has_semi_join(plan) {
        return;
    }
    let estimate = estimate_plan(plan);
    choose(plan, &estimate);
}

fn has_semi_join(op: &LogicalOperator) -> bool {
    let own = matches!(
        op.kind,
        K::Join {
            join_type: JoinType::Semi | JoinType::Anti,
            ..
        }
    );
    own || op.children.iter().any(has_semi_join)
}

fn choose(op: &mut LogicalOperator, estimate: &PlanEstimate) {
    for (child, estimate) in op.children.iter_mut().zip(&estimate.children) {
        choose(child, estimate);
    }
    let K::Join {
        join_type, conditions, ..
    } = &mut op.kind
    else {
        return;
    };
    let swapped = match join_type {
        JoinType::Semi => JoinType::RightSemi,
        JoinType::Anti => JoinType::RightAnti,
        _ => return,
    };
    if conditions.is_empty() || estimate.children[1].cardinality <= estimate.children[0].cardinality {
        return;
    }
    *join_type = swapped;
    for condition in conditions {
        std::mem::swap(&mut condition.left, &mut condition.right);
    }
    op.children.swap(0, 1);
}

/// `COLUMN_COUNT_PENALTY`.
const COLUMN_COUNT_PENALTY: f64 = 2.0;
/// `PREFER_RIGHT_DEEP_PENALTY`: rows that come out of a join are in flight already.
const PREFER_RIGHT_DEEP_PENALTY: f64 = 0.15;
/// How many times as wide the rows of the right side have to be for their width to count.
const MIN_WIDTH_RATIO: f64 = 3.0;
/// The bytes of an entry of the hash table, of which there are about three for a row.
const ENTRY_SIZE: f64 = 8.0;

/// `BuildProbeSideOptimizer::GetBuildSize`: about how many bytes a hash table of `rows` rows of
/// `types` takes.
fn build_size(types: &[LogicalType], rows: u64) -> f64 {
    // the hash of the row
    let mut width = 8.0 + COLUMN_COUNT_PENALTY;
    for ty in types {
        width += match ty.physical_type() {
            // what points at the string, and something for the string
            PhysicalType::Varchar => 16.0 + 8.0,
            PhysicalType::List | PhysicalType::Array | PhysicalType::Struct => 16.0 + 32.0,
            other => other.size() as f64,
        };
        width += COLUMN_COUNT_PENALTY;
    }
    width += 3.0 * ENTRY_SIZE;
    (width * rows as f64).max(ENTRY_SIZE)
}

/// `BuildProbeSideOptimizer::ChildHasJoins`.
fn joins_below(op: &LogicalOperator) -> usize {
    match (&op.kind, op.children.as_slice()) {
        (_, []) => 0,
        (K::Join { .. } | K::CrossProduct, [left, right]) => 1 + joins_below(left) + joins_below(right),
        (_, [first, ..]) => joins_below(first),
    }
}

/// `BuildProbeSideOptimizer::TryFlipJoinChildren` for inner, LEFT and RIGHT joins, to run when the
/// plan has the columns it will have.
pub fn choose_by_width(plan: &mut LogicalOperator) {
    fn has_flippable_join(op: &LogicalOperator) -> bool {
        let own = matches!(
            op.kind,
            K::Join {
                join_type: JoinType::Inner | JoinType::Left | JoinType::Right,
                ..
            }
        );
        own || op.children.iter().any(has_flippable_join)
    }
    if !has_flippable_join(plan) {
        return;
    }
    let estimate = estimate_plan(plan);
    flip_by_width(plan, &estimate, true);
}

/// `by_position`: whether something above reads the columns of `op` by where they are, and not by
/// what they are, so that its sides have to stay where they are.
fn flip_by_width(op: &mut LogicalOperator, estimate: &PlanEstimate, by_position: bool) {
    let below = match &op.kind {
        K::Projection { .. } | K::Aggregate { .. } | K::ColumnSelect { .. } => false,
        K::Join { .. } | K::CrossProduct | K::Filter { .. } | K::Order { .. } | K::TopN { .. } | K::Limit { .. } => {
            by_position
        }
        _ => true,
    };
    for (child, estimate) in op.children.iter_mut().zip(&estimate.children) {
        flip_by_width(child, estimate, below);
    }
    let K::Join {
        join_type: join_type @ (JoinType::Inner | JoinType::Left | JoinType::Right),
        conditions,
        ..
    } = &mut op.kind
    else {
        return;
    };
    if by_position || conditions.is_empty() {
        return;
    }
    let (left_types, right_types) = (op.children[0].types(), op.children[1].types());
    let inverse = match join_type {
        JoinType::Inner => {
            // estimates of joins are rough, and join order went by them alone: the width only
            // overrules them where the rows to build are several times as wide (strings, mostly)
            if build_size(&right_types, 1) < MIN_WIDTH_RATIO * build_size(&left_types, 1) {
                return;
            }
            JoinType::Inner
        }
        // a hash join, which builds the right side, needs an equality; the others stay as written
        _ if !conditions
            .iter()
            .any(|c| matches!(c.comparison, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom)) =>
        {
            return;
        }
        JoinType::Left => JoinType::Right,
        _ => JoinType::Left,
    };
    let left = build_size(&left_types, estimate.children[0].cardinality);
    let mut right = build_size(&right_types, estimate.children[1].cardinality);
    if joins_below(&op.children[1]) == 0 && joins_below(&op.children[0]) > 0 {
        right *= 1.0 + PREFER_RIGHT_DEEP_PENALTY;
    }
    if right <= left {
        return;
    }
    *join_type = inverse;
    for condition in conditions {
        std::mem::swap(&mut condition.left, &mut condition.right);
        condition.comparison = super::range_joins::flip(condition.comparison);
    }
    op.children.swap(0, 1);
}
