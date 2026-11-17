// Ported from duckdb/src/optimizer/optimizer.cpp @ rust-port-baseline
//
//! The optimizer: passes that rewrite a logical plan into one with the same result. Each pass can
//! be turned off with `disabled_optimizers`, under DuckDB's name for it, and all of them with
//! `PRAGMA disable_optimizer`.
//!
//! Plans reach the optimizer before `resolve_column_bindings`. A subquery has a plan of its own,
//! which the binder resolves, so it runs the optimizer on that plan itself.

pub mod build_side;
mod common_subplan;
mod join_elimination;
mod projection_pullup;
mod projection_placement;
mod deliminator;
pub mod distinct_statistics;
pub mod empty_result_pullup;
pub mod filter_pushdown;
pub mod in_clause_rewriter;
mod index_scan;
pub mod join_order;
mod relation_statistics;
pub mod rewriter;
pub mod scan_filters;
pub mod statistics;
pub mod top_n;
pub mod type_pushdown;
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
    pub type_pushdown: bool,
    pub common_subplan: bool,
    pub projection_pullup: bool,
    pub projection_placement: bool,
    pub join_elimination: bool,
    pub in_clause: bool,
    /// `index_scan_percentage`
    pub index_scan_percentage: f64,
    /// `index_scan_max_count`
    pub index_scan_max_count: u64,
    /// `dynamic_or_filter_threshold`
    pub dynamic_or_filter_threshold: usize,
    /// `perfect_ht_threshold`
    pub perfect_ht_threshold: u64,
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
            type_pushdown: true,
            common_subplan: true,
            projection_pullup: true,
            projection_placement: true,
            join_elimination: true,
            in_clause: true,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
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
            type_pushdown: false,
            common_subplan: false,
            projection_pullup: false,
            projection_placement: false,
            join_elimination: false,
            in_clause: false,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
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
            type_pushdown: !off("type_pushdown"),
            common_subplan: !off("common_subplan"),
            projection_pullup: !off("projection_pullup"),
            projection_placement: !off("projection_placement"),
            join_elimination: !off("join_elimination"),
            in_clause: !off("in_clause"),
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
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
    // `FilterStatisticsOptimizer`
    if options.filter_pushdown && options.statistics_propagation && filter_statistics_applies(&plan) {
        let mut changed = false;
        plan = run("statistics_propagation", plan, &mut |mut plan| {
            changed = statistics::simplify_filters(&mut plan);
            plan
        });
        if changed {
            plan = run("filter_pushdown", plan, &mut filter_pushdown::push_down);
        }
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
    // C++ runs this before the DELIMINATOR, after `CheckMarkToSemi`: its MARK joins stay
    if options.in_clause {
        plan = run("in_clause", plan, &mut in_clause_rewriter::rewrite);
    }
    if options.projection_pullup {
        plan = run("projection_pullup", plan, &mut projection_pullup::pull_up);
    }
    if options.join_order {
        build_side::choose_build_sides(&mut plan);
        plan = run("join_order", plan, &mut join_order::reorder_joins);
    }
    if options.join_elimination {
        plan = run("join_elimination", plan, &mut join_elimination::eliminate);
    }
    let mut statistics_map = None;
    if options.statistics_propagation {
        plan = run("statistics_propagation", plan, &mut |mut plan| {
            statistics_map = statistics::propagate_statistics(&mut plan, options.perfect_ht_threshold);
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
    // `PROJECTION_PLACEMENT` runs where the statistics have been propagated, once the unused columns are gone
    if options.projection_placement
        && let Some(statistics_map) = statistics_map.as_mut()
    {
        plan = run("projection_placement", plan, &mut |mut plan| {
            projection_placement::place(&mut plan, statistics_map);
            plan
        });
    }
    if options.join_order && options.unused_columns {
        build_side::choose_by_width(&mut plan);
    }
    // shared subplans run once, as materialized CTEs
    if options.common_subplan {
        plan = run("common_subplan", plan, &mut common_subplan::optimize);
    }
    index_scan::plan_index_scans(&mut plan, &options);
    // casts in SELECT pushed into file readers, e.g. `SELECT num::UHUGEINT FROM read_csv(...)`
    if options.type_pushdown {
        plan = run("type_pushdown", plan, &mut |mut plan| {
            type_pushdown::push_down_types(&mut plan);
            plan
        });
    }
    plan
}

/// `FilterStatisticsOptimizer::Optimize`: a plan without DELIM joins with a filter that reads more
/// than one table (here also the condition of an inner join, which C++ has as a filter).
fn filter_statistics_applies(plan: &LogicalOperator) -> bool {
    use crate::planner::logical::LogicalOperatorKind as K;
    fn tables(expr: &BoundExpr, found: &mut std::collections::HashSet<usize>) {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
            found.insert(binding.table_index);
        }
        for child in expr.children() {
            tables(child, found);
        }
    }
    fn multi(expr: &BoundExpr) -> bool {
        let mut found = std::collections::HashSet::new();
        tables(expr, &mut found);
        found.len() > 1
    }
    fn has_delim(op: &LogicalOperator) -> bool {
        matches!(op.kind, K::DelimJoin { .. }) || op.children.iter().any(has_delim)
    }
    fn has_multi(op: &LogicalOperator) -> bool {
        let own = match &op.kind {
            K::Filter { expressions } => expressions.iter().any(multi),
            K::Join {
                join_type: duckrs_parser::ast::JoinType::Inner,
                predicate: Some(predicate),
                ..
            } => multi(predicate),
            _ => false,
        };
        own || op.children.iter().any(has_multi)
    }
    // `CTEContainsDML`
    fn has_dml_below(op: &LogicalOperator) -> bool {
        op.children.iter().any(|child| {
            matches!(child.kind, K::Insert { .. } | K::Update { .. } | K::Delete { .. }) || has_dml_below(child)
        })
    }
    !has_delim(plan) && has_multi(plan) && !has_dml_below(plan)
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
