// Ported from duckdb/src/optimizer/remove_redundant_order.cpp @ rust-port-baseline
//
//! `RemoveRedundantOrder`: an ORDER BY whose order nothing above it reads (a hash aggregate, a
//! join, a hash-based set operation, another sort) is left out.

use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::LogicalTypeId;

/// Runs the pass over a plan; the order of the query result is observable.
pub fn remove(mut plan: LogicalOperator) -> LogicalOperator {
    optimize(&mut plan, false);
    plan
}

/// The aggregates C++ marks `AggregateOrderDependent::NOT_ORDER_DEPENDENT`.
fn order_independent(name: &str, input: Option<LogicalTypeId>) -> bool {
    match name {
        "count" | "count_star" | "min" | "max" | "bool_and" | "bool_or" | "histogram" | "mode" | "entropy" | "mad" => {
            true
        }
        // the integer and DECIMAL sums; a sum of floating point values depends on the order
        "sum" | "sum_no_overflow" => !matches!(input, Some(LogicalTypeId::Float | LogicalTypeId::Double)),
        _ => false,
    }
}

/// `AggregateDiscardsOrder`: a hash aggregate sinks in parallel; an ungrouped one sinks in order
/// when one of its aggregates reads the rows in order.
fn aggregate_discards_order(op: &LogicalOperator) -> bool {
    let K::Aggregate { groups, aggregates, .. } = &op.kind else {
        return true;
    };
    if !groups.is_empty() {
        return true;
    }
    aggregates.iter().all(|aggregate| {
        !aggregate.order_by.is_empty()
            || order_independent(
                &aggregate.function.name,
                aggregate.children.first().map(|c| c.return_type.id()),
            )
    })
}

fn optimize(op: &mut LogicalOperator, order_discarded: bool) {
    match &op.kind {
        K::Order { .. } => {
            if order_discarded {
                // nothing above reads the order: the sort goes, and its input is in the same place
                *op = op.children.pop().expect("a sort has a child");
                optimize(op, true);
            } else {
                // a sort replaces whatever order its input had
                optimize(&mut op.children[0], true);
            }
        }
        K::TopN { .. } => optimize(&mut op.children[0], true),
        K::Aggregate { .. } => {
            let discards = aggregate_discards_order(op);
            optimize(&mut op.children[0], discards);
        }
        // `DistinctDiscardsOrder`: DISTINCT ON keeps the first row of each group, plain DISTINCT does not
        K::Distinct { on, .. } => {
            let discards = !*on;
            optimize(&mut op.children[0], discards);
        }
        // UNION ALL concatenates its inputs in order; UNION has a DISTINCT on top here
        K::Union { .. } => {
            for child in &mut op.children {
                optimize(child, order_discarded);
            }
        }
        K::ExceptIntersect { .. } => {
            for child in &mut op.children {
                optimize(child, true);
            }
        }
        // joins do not keep the order of either input
        K::Join { .. } | K::MarkJoin { .. } | K::CrossProduct | K::AsOfJoin { .. } => {
            for child in &mut op.children {
                optimize(child, true);
            }
        }
        // row-wise operators neither read nor change the order
        K::Projection { .. } | K::Filter { .. } | K::Unnest { .. } | K::ColumnSelect { .. } => {
            optimize(&mut op.children[0], order_discarded)
        }
        // LIMIT, window functions, samples, CTEs, what writes, ... may read their input in order
        _ => {
            for child in &mut op.children {
                optimize(child, false);
            }
        }
    }
}
