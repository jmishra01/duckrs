// Ported from duckdb/src/optimizer/limit_pushdown.cpp and duckdb/src/optimizer/topn_optimizer.cpp
// @ rust-port-baseline
//
//! A small LIMIT goes below projections, so that they only compute what comes out, and a LIMIT over
//! an ORDER BY becomes a Top-N, which sorts only the rows that can still be among the first ones.

use super::join_order::estimate_plan;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};

/// Above this a LIMIT stays where it is. C++ has it for the parallelism that a limit ends.
const LIMIT_PUSHDOWN_THRESHOLD: u64 = 8192;
/// When a limit is more than this, and more than `TOP_N_MAX_FRACTION` of the rows, a sort is faster.
const TOP_N_LARGE_LIMIT: f64 = 5000.0;
const TOP_N_MAX_FRACTION: f64 = 0.007;

/// `LimitPushdown::Optimize`.
pub fn push_limits(op: &mut LogicalOperator) {
    if can_push_limit(op) {
        let mut projection = op.children.remove(0);
        std::mem::swap(&mut op.children, &mut projection.children);
        std::mem::swap(op, &mut projection);
        op.children.push(projection);
    }
    for child in &mut op.children {
        push_limits(child);
    }
}

fn can_push_limit(op: &LogicalOperator) -> bool {
    let K::Limit {
        limit: Some(limit),
        offset,
    } = op.kind
    else {
        return false;
    };
    let K::Projection { expressions, .. } = &op.children[0].kind else {
        return false;
    };
    // the offset would skip rows before a volatile expression runs on them
    limit < LIMIT_PUSHDOWN_THRESHOLD && (offset == 0 || !expressions.iter().any(|e| e.is_volatile()))
}

/// `TopN::Optimize`.
pub fn top_n(op: &mut LogicalOperator) {
    if can_make_top_n(op) {
        let K::Limit {
            limit: Some(limit),
            offset,
        } = op.kind
        else {
            unreachable!("checked")
        };
        // the projections stay above it, in the order they have
        let mut below = op.children.remove(0);
        let mut projections = Vec::new();
        while matches!(below.kind, K::Projection { .. }) {
            let child = below.children.remove(0);
            projections.push(below);
            below = child;
        }
        let K::Order { orders } = below.kind else {
            unreachable!("checked")
        };
        let mut result = LogicalOperator::new(K::TopN { orders, limit, offset }, below.children);
        while let Some(mut projection) = projections.pop() {
            projection.children.push(result);
            result = projection;
        }
        *op = result;
    }
    for child in &mut op.children {
        top_n(child);
    }
}

/// `TopN::CanOptimize`.
fn can_make_top_n(op: &LogicalOperator) -> bool {
    let K::Limit {
        limit: Some(limit),
        offset,
    } = op.kind
    else {
        return false;
    };
    let mut child = &op.children[0];
    let total = limit.saturating_add(offset) as f64;
    if total > TOP_N_LARGE_LIMIT && total > estimate_plan(child).cardinality as f64 * TOP_N_MAX_FRACTION {
        return false;
    }
    while matches!(child.kind, K::Projection { .. }) {
        child = &child.children[0];
    }
    matches!(child.kind, K::Order { .. })
}
