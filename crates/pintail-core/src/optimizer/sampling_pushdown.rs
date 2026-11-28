// Ported from duckdb/src/optimizer/sampling_pushdown.cpp @ rust-port-baseline
//
//! `SamplingPushdown`: a SYSTEM sample of a table scan without filters is taken by the scan, which
//! skips whole vectors. A sample of a number of rows keeps rows at the rate of that number to
//! the estimated rows of the table, under a LIMIT to that number.

use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K, TableSample};
use pintail_parser::ast::SampleMethod;

/// Runs the pass over a plan.
pub fn push_down(mut plan: LogicalOperator) -> LogicalOperator {
    optimize(&mut plan);
    plan
}

fn optimize(op: &mut LogicalOperator) {
    if let K::Sample { options } = &op.kind
        && options.method == SampleMethod::System
        && let [child] = op.children.as_slice()
        && let K::Get {
            filters,
            zone_filters,
            index_rows: None,
            sample: None,
            ..
        } = &child.kind
        && filters.is_empty()
        && zone_filters.is_empty()
        && child.children.is_empty()
    {
        let options = options.clone();
        let rows = (!options.is_percentage).then(|| options.sample_size.parse::<f64>().unwrap_or(0.0) as u64);
        let rate = match rows {
            Some(rows) => {
                let estimated = super::join_order::estimate_plan(child).cardinality;
                if estimated > 0 {
                    rows as f64 / estimated as f64
                } else {
                    1.0
                }
            }
            None => 0.0,
        };
        let mut get = op.children.pop().expect("checked above");
        if let K::Get { sample, .. } = &mut get.kind {
            *sample = Some(TableSample {
                options,
                rate: rate.clamp(0.0, 1.0),
            });
        }
        *op = match rows {
            // the scan can take more rows than asked for: the LIMIT stops it at them
            Some(rows) => LogicalOperator::new(
                K::Limit {
                    limit: Some(rows),
                    offset: 0,
                    failure: None,
                },
                vec![get],
            ),
            None => get,
        };
    }
    for child in &mut op.children {
        optimize(child);
    }
}
