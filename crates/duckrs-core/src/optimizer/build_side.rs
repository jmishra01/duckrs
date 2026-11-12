// Ported in spirit from duckdb/src/optimizer/build_probe_side_optimizer.cpp @ rust-port-baseline
//
//! `BuildProbeSideOptimizer`, for SEMI and ANTI joins: a hash join builds its right side, so one
//! whose right side is the larger becomes a RIGHT SEMI or RIGHT ANTI join with the sides swapped,
//! which gives the same rows. The larger side then only probes, and its scan can take a filter of
//! the keys of the smaller one.

use super::join_order::{PlanEstimate, estimate_plan};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_parser::ast::JoinType;

pub fn choose_build_sides(plan: &mut LogicalOperator) {
    // an estimate reads the statistics of the tables, which a plan without such a join has no use for
    if !has_semi_join(plan) {
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
