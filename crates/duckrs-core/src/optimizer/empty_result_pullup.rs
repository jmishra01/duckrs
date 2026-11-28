// Ported from duckdb/src/optimizer/empty_result_pullup.cpp @ rust-port-baseline
//
//! `EmptyResultPullup`: an operator over an input without rows has no rows either, so far as that
//! holds for the operator.

use super::filter_pushdown::empty_result;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_parser::ast::JoinType;

fn is_empty(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::EmptyResult { .. })
}

pub fn pull_up(mut op: LogicalOperator) -> LogicalOperator {
    op.children = std::mem::take(&mut op.children).into_iter().map(pull_up).collect();
    let any_empty = op.children.iter().any(is_empty);
    match &op.kind {
        // not ORDER BY or LIMIT: C++ leaves those over an empty input, so a CTE inlined into a join
        // keeps the join of two empty results (`cte_inlining_empty_result`)
        K::Filter { .. } | K::Projection { .. } | K::Distinct { .. } | K::Window { .. } if any_empty => {
            empty_result(&op)
        }
        // an aggregate without groups gives a row for no rows
        K::Aggregate {
            groups, grouping_sets, ..
        } if any_empty && !groups.is_empty() && grouping_sets.iter().all(|set| !set.is_empty()) => empty_result(&op),
        K::CrossProduct if any_empty => empty_result(&op),
        K::Join { join_type, .. } => {
            let left_empty = is_empty(&op.children[0]);
            let right_empty = is_empty(&op.children[1]);
            match join_type {
                JoinType::Inner | JoinType::Semi if any_empty => empty_result(&op),
                JoinType::Left | JoinType::Anti if left_empty => empty_result(&op),
                // nothing to match: all left rows pass, and the output is the left side's
                JoinType::Anti if right_empty => op.children.remove(0),
                _ => op,
            }
        }
        K::Union { .. } => {
            if op.children.iter().all(is_empty) {
                return empty_result(&op);
            }
            op.children.retain(|child| !is_empty(child));
            op
        }
        K::ExceptIntersect { all: true, .. } if is_empty(&op.children[0]) => empty_result(&op),
        _ => op,
    }
}
