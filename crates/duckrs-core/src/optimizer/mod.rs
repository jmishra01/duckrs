// Ported from duckdb/src/optimizer/optimizer.cpp @ rust-port-baseline
//
//! The optimizer: passes that rewrite a logical plan into one with the same result. Each pass can
//! be turned off with `disabled_optimizers`, under DuckDB's name for it, and all of them with
//! `PRAGMA disable_optimizer`.
//!
//! Plans reach the optimizer before `resolve_column_bindings`. A subquery has a plan of its own,
//! which the binder resolves, so it runs the optimizer on that plan itself.

pub mod empty_result_pullup;
pub mod filter_pushdown;
pub mod join_order;
pub mod rewriter;
pub mod statistics;
pub mod unused_columns;

use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::LogicalOperator;
use duckrs_common::types::LogicalType;

/// The passes that run (`enable_optimizer` and `disabled_optimizers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizerOptions {
    pub expression_rewriter: bool,
    pub filter_pushdown: bool,
    pub join_order: bool,
    pub statistics_propagation: bool,
    pub empty_result_pullup: bool,
    pub unused_columns: bool,
}

impl Default for OptimizerOptions {
    fn default() -> Self {
        Self {
            expression_rewriter: true,
            filter_pushdown: true,
            join_order: true,
            statistics_propagation: true,
            empty_result_pullup: true,
            unused_columns: true,
        }
    }
}

impl OptimizerOptions {
    /// No pass runs.
    pub fn none() -> Self {
        Self {
            expression_rewriter: false,
            filter_pushdown: false,
            join_order: false,
            statistics_propagation: false,
            empty_result_pullup: false,
            unused_columns: false,
        }
    }

    /// From the value of `disabled_optimizers`: names separated by commas.
    pub fn from_disabled(disabled: &str) -> Self {
        let off = |name: &str| disabled.split(',').any(|d| d.trim().eq_ignore_ascii_case(name));
        Self {
            expression_rewriter: !off("expression_rewriter"),
            filter_pushdown: !off("filter_pushdown"),
            join_order: !off("join_order"),
            statistics_propagation: !off("statistics_propagation"),
            empty_result_pullup: !off("empty_result_pullup"),
            unused_columns: !off("unused_columns"),
        }
    }
}

/// `Optimizer::Optimize`, in DuckDB's order of these passes.
pub fn optimize(mut plan: LogicalOperator, options: OptimizerOptions) -> LogicalOperator {
    if options.expression_rewriter {
        rewriter::rewrite_plan(&mut plan);
    }
    if options.filter_pushdown {
        plan = filter_pushdown::push_down(plan);
        filter_pushdown::convert_mark_joins(&mut plan);
    }
    if options.join_order {
        plan = join_order::reorder_joins(plan);
    }
    if options.statistics_propagation {
        statistics::propagate_statistics(&mut plan);
    }
    if options.empty_result_pullup {
        plan = empty_result_pullup::pull_up(plan);
    }
    if options.unused_columns {
        unused_columns::remove_unused_columns(&mut plan);
    }
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
