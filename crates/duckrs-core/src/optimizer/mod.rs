// Ported from duckdb/src/optimizer/optimizer.cpp @ rust-port-baseline
//
//! The optimizer: passes that rewrite a logical plan into one with the same result. Each pass can
//! be turned off with `disabled_optimizers`, under DuckDB's name for it, and all of them with
//! `PRAGMA disable_optimizer`.
//!
//! Plans reach the optimizer before `resolve_column_bindings`. A subquery has a plan of its own,
//! which the binder resolves, so it runs the optimizer on that plan itself.

pub mod deliminator;
pub mod empty_result_pullup;
pub mod filter_pushdown;
mod index_scan;
pub mod join_order;
pub mod rewriter;
pub mod scan_filters;
pub mod statistics;
pub mod top_n;
pub mod unused_columns;

use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::LogicalOperator;
use duckrs_common::types::LogicalType;

/// The passes that run (`enable_optimizer` and `disabled_optimizers`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptimizerOptions {
    pub expression_rewriter: bool,
    pub filter_pushdown: bool,
    pub deliminator: bool,
    pub join_order: bool,
    pub statistics_propagation: bool,
    pub empty_result_pullup: bool,
    pub unused_columns: bool,
    /// `index_scan_percentage`
    pub index_scan_percentage: f64,
    /// `index_scan_max_count`
    pub index_scan_max_count: u64,
    /// `dynamic_or_filter_threshold`
    pub dynamic_or_filter_threshold: usize,
}

impl Default for OptimizerOptions {
    fn default() -> Self {
        Self {
            expression_rewriter: true,
            filter_pushdown: true,
            deliminator: true,
            join_order: true,
            statistics_propagation: true,
            empty_result_pullup: true,
            unused_columns: true,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
        }
    }
}

impl OptimizerOptions {
    /// No pass runs.
    pub fn none() -> Self {
        Self {
            expression_rewriter: false,
            filter_pushdown: false,
            deliminator: false,
            join_order: false,
            statistics_propagation: false,
            empty_result_pullup: false,
            unused_columns: false,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
        }
    }

    /// From the value of `disabled_optimizers`: names separated by commas.
    pub fn from_disabled(disabled: &str) -> Self {
        let off = |name: &str| disabled.split(',').any(|d| d.trim().eq_ignore_ascii_case(name));
        Self {
            expression_rewriter: !off("expression_rewriter"),
            filter_pushdown: !off("filter_pushdown"),
            deliminator: !off("deliminator"),
            join_order: !off("join_order"),
            statistics_propagation: !off("statistics_propagation"),
            empty_result_pullup: !off("empty_result_pullup"),
            unused_columns: !off("unused_columns"),
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
        }
    }
}

/// `Optimizer::Optimize`, in DuckDB's order of these passes.
pub fn optimize(plan: LogicalOperator, options: OptimizerOptions) -> LogicalOperator {
    optimize_timed(plan, options, &mut Vec::new())
}

/// [`optimize`], with how long each optimizer took (`Optimizer::RunOptimizer`'s profiling), by
/// the name C++ gives it.
pub fn optimize_timed(
    mut plan: LogicalOperator,
    options: OptimizerOptions,
    timings: &mut Vec<(&'static str, std::time::Duration)>,
) -> LogicalOperator {
    let mut run =
        |name: &'static str, plan: LogicalOperator, pass: &mut dyn FnMut(LogicalOperator) -> LogicalOperator| {
            let start = std::time::Instant::now();
            let plan = pass(plan);
            timings.push((name, start.elapsed()));
            plan
        };
    if options.expression_rewriter {
        plan = run("expression_rewriter", plan, &mut |mut plan| {
            rewriter::rewrite_plan(&mut plan);
            plan
        });
    }
    if options.filter_pushdown {
        plan = run("filter_pushdown", plan, &mut filter_pushdown::push_down);
    }
    if options.deliminator {
        plan = run("deliminator", plan, &mut |mut plan| {
            deliminator::deliminate(&mut plan);
            plan
        });
    }
    if options.filter_pushdown {
        filter_pushdown::convert_mark_joins(&mut plan);
    }
    if options.join_order {
        plan = run("join_order", plan, &mut join_order::reorder_joins);
    }
    if options.statistics_propagation {
        plan = run("statistics_propagation", plan, &mut |mut plan| {
            statistics::propagate_statistics(&mut plan);
            plan
        });
    }
    if options.empty_result_pullup {
        plan = run("empty_result_pullup", plan, &mut empty_result_pullup::pull_up);
    }
    plan = run("top_n", plan, &mut |mut plan| {
        top_n::push_limits(&mut plan);
        top_n::top_n(&mut plan);
        plan
    });
    // a column only a scan's filter reads need not leave the scan
    if options.filter_pushdown {
        scan_filters::push_into_scans(&mut plan);
    }
    if options.unused_columns {
        plan = run("unused_columns", plan, &mut |mut plan| {
            unused_columns::remove_unused_columns(&mut plan);
            plan
        });
    }
    index_scan::plan_index_scans(&mut plan, &options);
    plan
}

/// The conjunction of `expressions`, of which there is at least one.
pub(crate) fn conjunction_of(mut expressions: Vec<BoundExpr>) -> BoundExpr {
    if expressions.len() == 1 {
        return expressions.remove(0);
    }
    BoundExpr::new(
        BoundExprKind::Conjunction {
            and: true,
            children: expressions,
        },
        LogicalType::BOOLEAN,
    )
}
