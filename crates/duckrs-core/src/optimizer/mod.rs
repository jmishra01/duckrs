// Ported from duckdb/src/optimizer/optimizer.cpp @ rust-port-baseline
//
//! The optimizer: passes that rewrite a logical plan into one with the same result. Each pass can
//! be turned off with `disabled_optimizers`, under DuckDB's name for it, and all of them with
//! `PRAGMA disable_optimizer`.
//!
//! Plans reach the optimizer before `resolve_column_bindings`. A subquery has a plan of its own,
//! which the binder resolves, so it runs the optimizer on that plan itself.

pub mod build_side;
mod aggregate_rewriter;
mod common_subplan;
mod cte_inlining;
mod join_elimination;
mod outer_join_simplification;
mod projection_pullup;
mod projection_placement;
mod deliminator;
mod disjunctive_join;
pub mod distinct_statistics;
pub mod empty_result_pullup;
mod filter_combiner;
mod filter_pullup;
mod grouping_sets;
mod partial_aggregate_pushdown;
mod aggregate_reuse;
mod cte_filter_pusher;
mod materialized_aggregate_reuse;
pub mod filter_pushdown;
pub mod in_clause_rewriter;
mod index_scan;
pub mod join_order;
pub mod range_joins;
mod redundant_order_keys;
mod relation_statistics;
mod sampling_pushdown;
mod remove_redundant_order;
pub mod rewriter;
pub mod scan_filters;
pub mod statistics;
pub mod top_n;
pub mod row_number_rewriter;
pub mod type_pushdown;
pub mod monotone_preimage;
pub mod or_range_merge;
pub mod reorder_filter;
mod late_materialization;
mod scalar_aggregate_merger;
mod self_join_aggregate;
mod topn_window_elimination;
mod window_join_pushdown;
mod window_self_join;
pub mod unnest_rewriter;
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
    pub row_number_rewriter: bool,
    pub type_pushdown: bool,
    pub unnest_rewriter: bool,
    /// `REORDER_FILTER`
    pub reorder_filter: bool,
    pub common_subplan: bool,
    pub projection_pullup: bool,
    pub projection_placement: bool,
    pub join_elimination: bool,
    pub in_clause: bool,
    pub disjunctive_join_rewriter: bool,
    pub cte_inlining: bool,
    pub redundant_order: bool,
    pub redundant_order_keys: bool,
    pub aggregate_function_rewriter: bool,
    pub common_aggregate: bool,
    pub outer_join_simplification: bool,
    pub filter_pullup: bool,
    pub grouping_sets: bool,
    pub sampling_pushdown: bool,
    pub partial_aggregate_pushdown: bool,
    pub aggregate_reuse: bool,
    pub join_filter_pushdown: bool,
    pub cte_filter_pusher: bool,
    /// `SCALAR_AGGREGATE_MERGE`
    /// `SELF_JOIN_AGGREGATE`
    pub self_join_aggregate: bool,
    pub scalar_aggregate_merge: bool,
    /// `WINDOW_SELF_JOIN`
    pub window_self_join: bool,
    /// `WINDOW_JOIN_PUSHDOWN`
    pub window_join_pushdown: bool,
    /// `LATE_MATERIALIZATION`
    pub late_materialization: bool,
    /// `TOP_N_WINDOW_ELIMINATION`
    pub topn_window_elimination: bool,
    /// `late_materialization_max_rows` and `preserve_insertion_order`
    pub late_materialization_options: late_materialization::LateMaterializationOptions,
    /// `scalar_aggregate_merge_threshold`
    pub scalar_aggregate_merge_threshold: u64,
    /// `index_scan_percentage`
    pub index_scan_percentage: f64,
    /// `index_scan_max_count`
    pub index_scan_max_count: u64,
    /// `dynamic_or_filter_threshold`
    pub dynamic_or_filter_threshold: usize,
    /// `perfect_ht_threshold`
    pub perfect_ht_threshold: u64,
    /// `approximate_join_order_threshold`
    pub approximate_join_order_threshold: usize,
    /// `debug_force_no_cross_product`
    pub force_no_cross_product: bool,
    /// what picks the operators of joins on comparisons
    pub range_joins: range_joins::RangeJoinOptions,
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
            row_number_rewriter: true,
            type_pushdown: true,
            unnest_rewriter: true,
            reorder_filter: true,
            common_subplan: true,
            projection_pullup: true,
            projection_placement: true,
            join_elimination: true,
            in_clause: true,
            disjunctive_join_rewriter: true,
            cte_inlining: true,
            redundant_order: true,
            redundant_order_keys: true,
            aggregate_function_rewriter: true,
            common_aggregate: true,
            outer_join_simplification: true,
            filter_pullup: true,
            grouping_sets: true,
            sampling_pushdown: true,
            partial_aggregate_pushdown: true,
            aggregate_reuse: true,
            join_filter_pushdown: true,
            cte_filter_pusher: true,
            self_join_aggregate: true,
            scalar_aggregate_merge: true,
            window_self_join: true,
            window_join_pushdown: true,
            late_materialization: true,
            topn_window_elimination: true,
            late_materialization_options: late_materialization::LateMaterializationOptions {
                max_rows: 50,
                preserve_insertion_order: true,
            },
            scalar_aggregate_merge_threshold: 0,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
            approximate_join_order_threshold: 12,
            force_no_cross_product: false,
            range_joins: range_joins::RangeJoinOptions::default(),
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
            row_number_rewriter: false,
            type_pushdown: false,
            unnest_rewriter: false,
            reorder_filter: false,
            common_subplan: false,
            projection_pullup: false,
            projection_placement: false,
            join_elimination: false,
            in_clause: false,
            disjunctive_join_rewriter: false,
            cte_inlining: false,
            redundant_order: false,
            redundant_order_keys: false,
            aggregate_function_rewriter: false,
            common_aggregate: false,
            outer_join_simplification: false,
            filter_pullup: false,
            grouping_sets: false,
            sampling_pushdown: false,
            partial_aggregate_pushdown: false,
            aggregate_reuse: false,
            join_filter_pushdown: false,
            cte_filter_pusher: false,
            self_join_aggregate: false,
            scalar_aggregate_merge: false,
            window_self_join: false,
            window_join_pushdown: false,
            late_materialization: false,
            topn_window_elimination: false,
            late_materialization_options: late_materialization::LateMaterializationOptions {
                max_rows: 50,
                preserve_insertion_order: true,
            },
            scalar_aggregate_merge_threshold: 0,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
            approximate_join_order_threshold: 12,
            force_no_cross_product: false,
            range_joins: range_joins::RangeJoinOptions::default(),
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
            row_number_rewriter: !off("window_rewriter"),
            type_pushdown: !off("type_pushdown"),
            unnest_rewriter: !off("unnest_rewriter"),
            reorder_filter: !off("reorder_filter"),
            common_subplan: !off("common_subplan"),
            projection_pullup: !off("projection_pullup"),
            projection_placement: !off("projection_placement"),
            join_elimination: !off("join_elimination"),
            in_clause: !off("in_clause"),
            disjunctive_join_rewriter: !off("disjunctive_join_rewriter"),
            cte_inlining: !off("cte_inlining"),
            redundant_order: !off("redundant_order"),
            redundant_order_keys: !off("redundant_order_keys"),
            aggregate_function_rewriter: !off("aggregate_function_rewriter"),
            common_aggregate: !off("common_aggregate"),
            outer_join_simplification: !off("outer_join_simplification"),
            filter_pullup: !off("filter_pullup"),
            grouping_sets: !off("grouping_sets"),
            sampling_pushdown: !off("sampling_pushdown"),
            partial_aggregate_pushdown: !off("partial_aggregate_pushdown"),
            aggregate_reuse: !off("aggregate_reuse"),
            join_filter_pushdown: !off("join_filter_pushdown"),
            cte_filter_pusher: !off("cte_filter_pusher"),
            self_join_aggregate: !off("self_join_aggregate"),
            scalar_aggregate_merge: !off("scalar_aggregate_merge"),
            window_self_join: !off("window_self_join"),
            window_join_pushdown: !off("window_join_pushdown"),
            late_materialization: !off("late_materialization"),
            topn_window_elimination: !off("top_n_window_elimination"),
            late_materialization_options: late_materialization::LateMaterializationOptions {
                max_rows: 50,
                preserve_insertion_order: true,
            },
            scalar_aggregate_merge_threshold: 0,
            index_scan_percentage: 0.001,
            index_scan_max_count: 2048,
            dynamic_or_filter_threshold: 50,
            perfect_ht_threshold: 12,
            approximate_join_order_threshold: 12,
            force_no_cross_product: false,
            range_joins: range_joins::RangeJoinOptions::default(),
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
    // a plan that has been through here before has its comparisons back in the predicates
    range_joins::restore_predicates(&mut plan);
    if options.expression_rewriter {
        plan = run("expression_rewriter", plan, &mut |mut plan| {
            rewriter::rewrite_plan(&mut plan);
            plan
        });
    }
    // `CTE_INLINING` runs here too, so that what is inlined goes through the filter pushdown
    if options.cte_inlining {
        plan = run("cte_inlining", plan, &mut cte_inlining::inline);
    }
    // before the filter pushdown, which would move the join's conditions into the scans
    if options.self_join_aggregate {
        plan = run("self_join_aggregate", plan, &mut self_join_aggregate::optimize);
    }
    if options.scalar_aggregate_merge {
        let threshold = options.scalar_aggregate_merge_threshold;
        plan = run("scalar_aggregate_merge", plan, &mut |plan| scalar_aggregate_merger::merge(plan, threshold));
    }
    if options.aggregate_function_rewriter {
        let (common, statistics) = (options.common_aggregate, options.statistics_propagation);
        plan = run("aggregate_function_rewriter", plan, &mut |plan| {
            aggregate_rewriter::rewrite(plan, common, statistics)
        });
    }
    if options.redundant_order {
        plan = run("redundant_order", plan, &mut remove_redundant_order::remove);
    }
    if options.filter_pullup {
        plan = run("filter_pullup", plan, &mut filter_pullup::pull_up);
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
    // `CTE_FILTER_PUSHER`: the filters of the scans of a materialized CTE go into its definition
    if options.cte_filter_pusher {
        plan = run("cte_filter_pusher", plan, &mut cte_filter_pusher::push);
    }
    // `UNNEST_REWRITER`, before the DELIMINATOR
    if options.unnest_rewriter {
        plan = run("unnest_rewriter", plan, &mut unnest_rewriter::rewrite);
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
    if options.grouping_sets {
        plan = run("grouping_sets", plan, &mut grouping_sets::optimize);
    }
    if options.disjunctive_join_rewriter {
        plan = run("disjunctive_join_rewriter", plan, &mut disjunctive_join::rewrite);
    }
    if options.cte_inlining {
        plan = run("cte_inlining", plan, &mut cte_inlining::inline);
    }
    if options.empty_result_pullup {
        plan = run("empty_result_pullup", plan, &mut empty_result_pullup::pull_up);
    }
    if options.window_self_join {
        plan = run("window_self_join", plan, &mut window_self_join::optimize);
    }
    if options.projection_pullup {
        plan = run("projection_pullup", plan, &mut projection_pullup::pull_up);
    }
    if options.window_join_pushdown {
        plan = run("window_join_pushdown", plan, &mut window_join_pushdown::optimize);
    }
    if options.outer_join_simplification {
        plan = run("outer_join_simplification", plan, &mut outer_join_simplification::simplify);
    }
    if options.join_order {
        build_side::choose_build_sides(&mut plan, options.range_joins.prefer_range_joins);
        join_order::configure(options.approximate_join_order_threshold, options.force_no_cross_product);
        plan = run("join_order", plan, &mut join_order::reorder_joins);
    }
    if options.aggregate_reuse {
        plan = run("aggregate_reuse", plan, &mut aggregate_reuse::reuse);
    }
    if options.partial_aggregate_pushdown {
        plan = run("partial_aggregate_pushdown", plan, &mut partial_aggregate_pushdown::push_down);
    }
    if options.join_elimination {
        plan = run("join_elimination", plan, &mut join_elimination::eliminate);
    }
    if options.redundant_order_keys {
        plan = run("redundant_order_keys", plan, &mut redundant_order_keys::remove);
    }
    let mut statistics_map = None;
    if options.statistics_propagation {
        plan = run("statistics_propagation", plan, &mut |mut plan| {
            statistics_map = statistics::propagate_statistics(&mut plan, options.perfect_ht_threshold);
            plan
        });
    }
    if options.sampling_pushdown {
        plan = run("sampling_pushdown", plan, &mut sampling_pushdown::push_down);
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
        // C++ runs it after `TOP_N`, with the unused columns gone, and removes those again after
        if options.late_materialization {
            let late = options.late_materialization_options;
            plan = run("late_materialization", plan, &mut |mut plan| {
                if late_materialization::optimize(&mut plan, late) {
                    unused_columns::remove_unused_columns(&mut plan);
                }
                plan
            });
        }
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
    // `TOP_N_WINDOW_ELIMINATION`, and the unused columns removed again after it
    if options.topn_window_elimination {
        let unused = options.unused_columns;
        plan = run("topn_window_elimination", plan, &mut |mut plan| {
            if topn_window_elimination::optimize(&mut plan) && unused {
                unused_columns::remove_unused_columns(&mut plan);
            }
            plan
        });
    }
    if options.join_order && options.unused_columns {
        build_side::choose_by_width(&mut plan);
    }
    // cheapest filters first
    if options.reorder_filter {
        plan = run("reorder_filter", plan, &mut reorder_filter::reorder_filters);
    }
    // shared subplans run once, as materialized CTEs
    if options.common_subplan {
        plan = run("common_subplan", plan, &mut common_subplan::optimize);
    }
    index_scan::plan_index_scans(&mut plan, &options);
    // `ROW_NUMBER() OVER ()` over a table becomes its `row_number` column
    if options.row_number_rewriter {
        plan = run("row_number_rewriter", plan, &mut row_number_rewriter::rewrite);
    }
    // casts in SELECT pushed into file readers, e.g. `SELECT num::UHUGEINT FROM read_csv(...)`
    if options.type_pushdown {
        plan = run("type_pushdown", plan, &mut |mut plan| {
            type_pushdown::push_down_types(&mut plan);
            plan
        });
    }
    // what the binder does in C++, which has them as conditions all the way, and the planner
    range_joins::plan_range_joins(&mut plan, &options.range_joins);
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
