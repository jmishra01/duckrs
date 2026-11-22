// Ported from duckdb/src/optimizer/filter_pushdown.cpp and duckdb/src/optimizer/pushdown/
// {pushdown_aggregate,pushdown_cross_product,pushdown_distinct,pushdown_filter,pushdown_inner_join,
// pushdown_left_join,pushdown_limit,pushdown_projection,pushdown_semi_anti_join,
// pushdown_set_operation,pushdown_unnest}.cpp @ rust-port-baseline
//
//! `FilterPushdown`: moves filters as far down the plan as they go, and turns cross products with
//! a filter on both sides into joins. Operators it has no rule for keep the filters on top of them.
//!
//! The filters go through a `FilterCombiner` (`optimizer::filter_combiner`) where C++ pushes and
//! generates them: after a filter operator and before each operator they reach.

use super::conjunction_of;
use super::rewriter::{
    can_throw, contains_barrier, expressions_equal, fold_boolean, rewrite_expression, split_conjuncts, window_can_throw,
};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding, WindowFunction};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{BTreeSet, HashMap};

/// `LogicalOperatorKind::EmptyResult` in place of `op`.
pub(crate) fn empty_result(op: &LogicalOperator) -> LogicalOperator {
    LogicalOperator::new(
        LogicalOperatorKind::EmptyResult {
            types: op.types(),
            bindings: op.column_bindings(),
        },
        Vec::new(),
    )
}

/// The table indexes an expression reads at this level (`LogicalJoin::GetExpressionBindings`).
fn expression_tables(expr: &BoundExpr, tables: &mut BTreeSet<usize>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        tables.insert(binding.table_index);
    }
    for child in expr.children() {
        expression_tables(child, tables);
    }
}

/// `LogicalJoin::GetTableReferences`.
fn plan_tables(op: &LogicalOperator) -> BTreeSet<usize> {
    op.column_bindings().iter().map(|b| b.table_index).collect()
}

fn visit_columns(expr: &mut BoundExpr, f: &mut impl FnMut(&mut BoundExpr)) {
    expr.walk_mut(&mut |e| {
        if matches!(e.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
            f(e);
        }
    });
}

/// Replaces the columns of `expr` with `f`, leaving what `f` puts there alone: a replacement refers
/// to the columns below the operator, which mean something else there.
fn replace_columns(expr: &mut BoundExpr, f: &mut impl FnMut(&mut BoundExpr)) {
    if matches!(expr.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
        f(expr);
        return;
    }
    for child in expr.children_mut() {
        replace_columns(child, f);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinSide {
    None,
    Left,
    Right,
    Both,
}

fn join_side(tables: &BTreeSet<usize>, left: &BTreeSet<usize>, right: &BTreeSet<usize>) -> JoinSide {
    if tables.is_empty() {
        JoinSide::None
    } else if tables.is_subset(left) {
        JoinSide::Left
    } else if tables.is_subset(right) {
        JoinSide::Right
    } else {
        JoinSide::Both
    }
}

/// `CreateAuxiliaryFilter`: for a filter `b op c` of a constant `c`, `e(b) op' e(c)` of the partition
/// `e(b)` if `e` keeps the order of its argument, with `op'` not strict where `e` is not.
fn auxiliary_filter(partition: &BoundExpr, filter: &BoundExpr) -> Option<BoundExpr> {
    let BoundExprKind::Comparison { op, left, right } = &filter.kind else {
        return None;
    };
    let (constant, free, op) = if left.is_foldable() {
        (left, right, crate::optimizer::statistics::flip(*op))
    } else if right.is_foldable() {
        (right, left, *op)
    } else {
        return None;
    };
    let BoundExprKind::Function { function, children, .. } = &partition.kind else {
        return None;
    };
    if !expressions_equal(free, &children[0]) {
        return None;
    }
    // `ArgProperties`: increasing, and strictly so for these
    let strict = matches!(function.name.as_str(), "make_date" | "make_timestamp" | "make_timestamp_ns" | "+");
    if !strict && crate::execution::zone_map::unary_monotonicity(&function.name) != Some(true) {
        return None;
    }
    let op = match op {
        ComparisonOp::NotEqual | ComparisonOp::DistinctFrom | ComparisonOp::Equal | ComparisonOp::NotDistinctFrom => {
            if !strict {
                return None;
            }
            op
        }
        ComparisonOp::LessThan if !strict => ComparisonOp::LessThanOrEqual,
        ComparisonOp::GreaterThan if !strict => ComparisonOp::GreaterThanOrEqual,
        other => other,
    };
    let mut of_constant = partition.clone();
    if let BoundExprKind::Function { children, .. } = &mut of_constant.kind {
        children[0] = (**constant).clone();
    }
    Some(BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(partition.clone()),
            right: Box::new(of_constant),
        },
        LogicalType::BOOLEAN,
    ))
}

/// `FilterPushdown::Filter`.
#[derive(Debug, Clone)]
struct Filter {
    filter: BoundExpr,
    tables: BTreeSet<usize>,
}

impl Filter {
    fn new(filter: BoundExpr) -> Self {
        let mut tables = BTreeSet::new();
        expression_tables(&filter, &mut tables);
        Self { filter, tables }
    }
}

/// The filters on their way down to an operator.
#[derive(Debug, Default)]
struct FilterPushdown {
    filters: Vec<Filter>,
}

thread_local! {
    /// The next table index of the plan being rewritten (`Binder::GenerateTableIndex`).
    static NEXT_TABLE_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// `ProjectionMode::PRESERVE_COMPUTED_EXPRESSIONS`: a filter of a column a projection computes
    /// stays above it
    static PRESERVE_COMPUTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn generate_table_index() -> usize {
    NEXT_TABLE_INDEX.with(|next| {
        let index = next.get();
        next.set(index + 1);
        index
    })
}

/// [`push_down`] that leaves above a projection the filters of what it computes
/// (`ProjectionMode::PRESERVE_COMPUTED_EXPRESSIONS`).
pub fn push_down_preserving_computed(plan: LogicalOperator) -> LogicalOperator {
    let before = PRESERVE_COMPUTED.with(|p| p.replace(true));
    let result = push_down(plan);
    PRESERVE_COMPUTED.with(|p| p.set(before));
    result
}

/// Runs the pass over a plan.
pub fn push_down(plan: LogicalOperator) -> LogicalOperator {
    // the pass also runs on parts of the plan it rewrites: the outermost run numbers the new tables
    let outermost = NEXT_TABLE_INDEX.with(|next| next.get()) == 0;
    if outermost {
        let first_free = super::common_subplan::max_table_index(&plan) + 1;
        NEXT_TABLE_INDEX.with(|next| next.set(first_free));
    }
    let result = FilterPushdown::default().rewrite(plan);
    if outermost {
        NEXT_TABLE_INDEX.with(|next| next.set(0));
    }
    result
}

/// `CreateDummyRHS`: one row of NULLs with the columns of `op`, a projection on a dummy scan for
/// each of its tables, side by side.
fn dummy_rhs(op: &LogicalOperator) -> LogicalOperator {
    let mut groups: Vec<(usize, Vec<(usize, duckrs_common::types::LogicalType)>)> = Vec::new();
    for (binding, ty) in op.column_bindings().into_iter().zip(op.types()) {
        match groups.iter_mut().find(|(table, _)| *table == binding.table_index) {
            Some((_, columns)) => columns.push((binding.column_index, ty)),
            None => groups.push((binding.table_index, vec![(binding.column_index, ty)])),
        }
    }
    let mut result: Option<LogicalOperator> = None;
    for (table_index, columns) in groups {
        // the columns keep their bindings: a projection with each at its column index
        let width = columns.iter().map(|(c, _)| c + 1).max().unwrap_or(0);
        let expressions = (0..width)
            .map(|at| {
                let ty = columns
                    .iter()
                    .find(|(c, _)| *c == at)
                    .map_or(duckrs_common::types::LogicalType::INTEGER, |(_, ty)| ty.clone());
                BoundExpr::constant(Value::null(ty.clone())).cast_to(&ty, false)
            })
            .collect();
        let projection = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            },
            vec![LogicalOperator::new(
                LogicalOperatorKind::DummyScan {
                    table_index: generate_table_index(),
                },
                Vec::new(),
            )],
        );
        result = Some(match result {
            None => projection,
            Some(left) => LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![left, projection]),
        });
    }
    result.unwrap_or_else(|| LogicalOperator::new(LogicalOperatorKind::DummyScan { table_index: generate_table_index() }, Vec::new()))
}

/// Whether a condition can hold for no row: it folds to false or NULL (`FilterResult::UNSATISFIABLE`).
fn unsatisfiable(expr: &BoundExpr) -> bool {
    let mut conjuncts = Vec::new();
    split_conjuncts(expr.clone(), &mut conjuncts);
    conjuncts.iter().any(|conjunct| {
        conjunct.is_foldable()
            && crate::execution::expression_executor::try_evaluate_scalar(conjunct)
                .is_some_and(|value| value.is_null() || value == Value::boolean(false))
    })
}

impl FilterPushdown {
    /// `FilterCombiner::AddFilter`: false if the filter can never hold.
    fn add_filter(&mut self, expr: BoundExpr) -> bool {
        let mut conjuncts = Vec::new();
        split_conjuncts(expr, &mut conjuncts);
        for conjunct in conjuncts {
            match fold_boolean(&conjunct) {
                Some(Some(true)) => continue,
                Some(_) => return false,
                None => {}
            }
            if !self.filters.iter().any(|f| expressions_equal(&f.filter, &conjunct)) {
                self.filters.push(Filter::new(conjunct));
            }
        }
        true
    }

    /// `PushFilters` and `GenerateFilters`: the filters through a `FilterCombiner`; false if
    /// together they hold for no row.
    fn combine(&mut self) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        let mut combiner = super::filter_combiner::FilterCombiner::default();
        for filter in std::mem::take(&mut self.filters) {
            if combiner.add(filter.filter) == super::filter_combiner::FilterResult::Unsatisfiable {
                return false;
            }
        }
        self.filters = combiner.generate().into_iter().map(Filter::new).collect();
        true
    }

    fn rewrite(mut self, mut op: LogicalOperator) -> LogicalOperator {
        if !matches!(op.kind, LogicalOperatorKind::Filter { .. }) && !self.combine() {
            return empty_result(&op);
        }
        // `BarrierCanPassThrough`: an operator that can remove rows keeps the barred filters on top of it
        let passes = matches!(
            op.kind,
            LogicalOperatorKind::Filter { .. }
                | LogicalOperatorKind::Projection { .. }
                | LogicalOperatorKind::Order { .. }
                | LogicalOperatorKind::Get { .. }
                | LogicalOperatorKind::SecureView { .. }
        );
        if !passes && self.filters.iter().any(|f| contains_barrier(&f.filter)) {
            let (barred, rest): (Vec<Filter>, Vec<Filter>) = std::mem::take(&mut self.filters)
                .into_iter()
                .partition(|f| contains_barrier(&f.filter));
            self.filters = rest;
            let result = self.rewrite(op);
            return add_filter_operator(result, barred.into_iter().map(|f| f.filter).collect());
        }
        match &op.kind {
            LogicalOperatorKind::Aggregate { .. } => self.pushdown_aggregate(op),
            LogicalOperatorKind::Filter { .. } => {
                let LogicalOperatorKind::Filter { expressions } = &mut op.kind else {
                    unreachable!()
                };
                for expr in std::mem::take(expressions) {
                    if !self.add_filter(expr) {
                        return empty_result(&op);
                    }
                }
                // `PushdownFilter`: `GenerateFilters`
                if !self.combine() {
                    return empty_result(&op);
                }
                self.rewrite(op.children.remove(0))
            }
            LogicalOperatorKind::CrossProduct => self.pushdown_cross_product(op),
            LogicalOperatorKind::Join { join_type, .. } => match join_type {
                JoinType::Inner => self.pushdown_inner_join(op),
                JoinType::Left => self.pushdown_left_join(op),
                JoinType::Outer => self.pushdown_outer_join(op),
                JoinType::Semi | JoinType::Anti => self.pushdown_semi_anti_join(op),
                _ => self.finish_pushdown(op),
            },
            LogicalOperatorKind::DelimJoin { .. } | LogicalOperatorKind::MarkJoin { .. } => {
                self.pushdown_delim_join(op)
            }
            LogicalOperatorKind::Projection { .. } => self.pushdown_projection(op),
            LogicalOperatorKind::Union { .. } | LogicalOperatorKind::ExceptIntersect { .. } => {
                self.pushdown_set_operation(op)
            }
            LogicalOperatorKind::Distinct { .. } => self.pushdown_distinct(op),
            LogicalOperatorKind::Window { .. } => self.pushdown_window(op),
            LogicalOperatorKind::Order { .. } => {
                let child = op.children.remove(0);
                op.children.push(self.rewrite(child));
                op
            }
            LogicalOperatorKind::Limit { limit: Some(0), .. } => empty_result(&op),
            LogicalOperatorKind::Unnest { .. } => self.pushdown_unnest(op),
            LogicalOperatorKind::SecureView { .. } => self.pushdown_secure_view(op),
            _ => self.finish_pushdown(op),
        }
    }

    /// `FilterPushdown::PushdownSecureView`: the filters go into the view, but one that could be
    /// observed from outside (it can throw, or is volatile) goes in a barrier, which no operator
    /// inside that removes rows lets past. The boundary stays.
    fn pushdown_secure_view(self, mut op: LogicalOperator) -> LogicalOperator {
        let mut inside = FilterPushdown::default();
        let mut pushed = Vec::new();
        for f in self.filters {
            let mut expr = f.filter;
            // the operators inside the view are never shown: the boundary reports the filter instead
            pushed.push(expr.to_string());
            if (can_throw(&expr) || expr.is_volatile()) && !contains_barrier(&expr) {
                expr = crate::function::wrap_in_barrier(expr);
            }
            if !inside.add_filter(expr) {
                return empty_result(&op);
            }
        }
        // the estimate of the view is frozen before anything goes in: otherwise the boundary would report what the
        // statistics of the view's contents say about the caller's predicate
        let frozen = (!pushed.is_empty()).then(|| plain_estimate(&op.children[0]));
        if let LogicalOperatorKind::SecureView {
            pushed_filters,
            propagate_statistics,
            estimate,
            ..
        } = &mut op.kind
        {
            if !*propagate_statistics && estimate.is_none() {
                *estimate = frozen;
            }
            pushed_filters.extend(pushed);
        }
        let child = op.children.remove(0);
        op.children.push(inside.rewrite(child));
        op
    }

    /// `PushdownMarkJoin` and `PushdownSingleJoin`: each left row stays, once, so what reads only
    /// the left side can go there, which also makes the domain smaller.
    fn pushdown_delim_join(self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let (left, rest): (Vec<_>, Vec<_>) = self
            .filters
            .into_iter()
            .partition(|f| !f.tables.is_empty() && f.tables.is_subset(&left_tables));
        let right = op.children.pop().expect("a join has two children");
        let left_child = op.children.pop().expect("a join has two children");
        op.children.push(FilterPushdown { filters: left }.rewrite(left_child));
        op.children.push(FilterPushdown::default().rewrite(right));
        FilterPushdown { filters: rest }.push_final_filters(op)
    }

    /// `FilterPushdown::PushFinalFilters`.
    fn push_final_filters(self, op: LogicalOperator) -> LogicalOperator {
        add_filter_operator(op, self.filters.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::FinishPushdown`: the filters stop here, and the children start over.
    fn finish_pushdown(self, mut op: LogicalOperator) -> LogicalOperator {
        op.children = std::mem::take(&mut op.children).into_iter().map(push_down).collect();
        if let LogicalOperatorKind::LateralJoin { right, .. } = &mut op.kind {
            let plan = std::mem::replace(
                &mut **right,
                LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![]),
            );
            **right = push_down(plan);
        }
        self.push_final_filters(op)
    }

    /// `FilterPushdown::PushdownWindow`: a filter on columns that partition every window holds
    /// for all rows of a partition or none, and so does one that compares a partition of every
    /// window with constants when the windows are rankings without arguments. A filter `b op c` of
    /// a constant `c` and an order-preserving partition `e(b)` of every window leaves a weaker copy
    /// `e(b) op' e(c)` below (`CreateAuxiliaryFilter`), and stays.
    fn pushdown_window(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Window { windows, .. } = &op.kind else {
            unreachable!()
        };
        let mut partition_bindings: Vec<BTreeSet<ColumnBinding>> = Vec::new();
        // the partitions `e(b)` of one argument, with how often windows have them
        let mut partition_functions: Vec<(BoundExpr, usize)> = Vec::new();
        for window in windows {
            // all windows need partitions
            if window.partitions.is_empty() {
                return self.finish_pushdown(op);
            }
            for partition in &window.partitions {
                if let BoundExprKind::Function { children, .. } = &partition.kind
                    && children.len() == 1
                    && !partition.is_volatile()
                {
                    match partition_functions.iter_mut().find(|(p, _)| expressions_equal(p, partition)) {
                        Some((_, count)) => *count += 1,
                        None => partition_functions.push((partition.clone(), 1)),
                    }
                }
            }
            partition_bindings.push(
                window
                    .partitions
                    .iter()
                    .filter_map(|p| match p.kind {
                        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(binding),
                        _ => None,
                    })
                    .collect(),
            );
        }
        if partition_bindings.is_empty() {
            return self.finish_pushdown(op);
        }
        let only_ranks = only_ranks_without_arguments(windows);
        let compares_partitions = |filter: &BoundExpr| compares_window_partitions(windows, filter);
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for f in std::mem::take(&mut self.filters) {
            let mut columns = Vec::new();
            let mut probe = f.filter.clone();
            visit_columns(&mut probe, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            let on_partitions = partition_bindings
                .iter()
                .all(|set| columns.iter().all(|b| set.contains(b)));
            if (on_partitions || (only_ranks && compares_partitions(&f.filter))) && !f.filter.is_volatile() {
                below.filters.push(f);
                continue;
            }
            for (partition, count) in &partition_functions {
                // a partition of every window
                if *count != partition_bindings.len() {
                    continue;
                }
                if let Some(auxiliary) = auxiliary_filter(partition, &f.filter) {
                    below.filters.push(Filter::new(auxiliary));
                }
            }
            remaining.push(f);
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        self.filters = remaining;
        self.push_final_filters(op)
    }

    /// `FilterPushdown::PushdownAggregate`: a filter on groups that are in every grouping set
    /// holds for all rows of a group or none.
    fn pushdown_aggregate(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Aggregate {
            group_index,
            groups,
            grouping_sets,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let (group_index, groups, grouping_sets) = (*group_index, groups.clone(), grouping_sets.clone());
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for mut f in std::mem::take(&mut self.filters) {
            let mut columns = Vec::new();
            visit_columns(&mut f.filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            let in_all_sets = columns
                .iter()
                .all(|b| b.table_index == group_index && grouping_sets.iter().all(|set| set.contains(&b.column_index)));
            if groups.is_empty() || columns.is_empty() || !in_all_sets {
                remaining.push(f);
                continue;
            }
            // below the aggregate, a group that can fail would run ahead of the filters there
            if can_throw(&f.filter)
                || f.filter.is_volatile()
                || columns.iter().any(|b| {
                    let group = &groups[b.column_index];
                    group.is_volatile() || can_throw(group)
                })
            {
                remaining.push(f);
                continue;
            }
            let mut filter = f.filter;
            replace_columns(&mut filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    *e = groups[binding.column_index].clone();
                }
            });
            if !below.add_filter(filter) {
                return empty_result(&op);
            }
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownCrossProduct`.
    fn pushdown_cross_product(self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let right_tables = plan_tables(&op.children[1]);
        let (mut left, mut right) = (FilterPushdown::default(), FilterPushdown::default());
        let mut join_expressions = Vec::new();
        let mut above = Vec::new();
        for f in self.filters {
            match join_side(&f.tables, &left_tables, &right_tables) {
                JoinSide::Left => left.filters.push(f),
                JoinSide::Right => right.filters.push(f),
                JoinSide::Both => {
                    // the filter stays on the join, so these only have to keep what it keeps
                    if let Some(filter) = side_of_disjunction(&f.filter, &left_tables) {
                        left.add_filter(filter);
                    }
                    if let Some(filter) = side_of_disjunction(&f.filter, &right_tables) {
                        right.add_filter(filter);
                    }
                    join_expressions.push(f.filter);
                }
                JoinSide::None => above.push(f.filter),
            }
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        let children = vec![left.rewrite(left_child), right.rewrite(right_child)];
        let result = if join_expressions.is_empty() {
            LogicalOperator::new(LogicalOperatorKind::CrossProduct, children)
        } else {
            let (conditions, predicates) = extract_join_conditions(join_expressions, &left_tables, &right_tables);
            let kind = LogicalOperatorKind::Join {
                join_type: JoinType::Inner,
                conditions,
                predicate: (!predicates.is_empty()).then(|| conjunction_of(predicates)),
                algorithm: Default::default(),
            };
            LogicalOperator::new(kind, children)
        };
        add_filter_operator(result, above)
    }

    /// `FilterPushdown::PushdownInnerJoin`: the conditions become filters, and then the join is a
    /// cross product with filters.
    fn pushdown_inner_join(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Join {
            conditions, predicate, ..
        } = &mut op.kind
        else {
            unreachable!()
        };
        let (conditions, predicate) = (std::mem::take(conditions), predicate.take());
        let expressions = conditions.into_iter().map(condition_expression).chain(predicate);
        for expr in expressions {
            if !self.add_filter(expr) {
                return empty_result(&op);
            }
        }
        // `GenerateFilters`: the conditions and the filters together, through the combiner
        if !self.combine() {
            return empty_result(&op);
        }
        self.pushdown_cross_product(LogicalOperator::new(LogicalOperatorKind::CrossProduct, op.children))
    }

    /// `FilterPushdown::PushdownOuterJoin`: a filter `P(coalesce(l, r))` of the keys of a condition `l = r` holds of
    /// the row of each side that makes up a joined row, so `P(l)` goes to the left and `P(r)` to the right
    /// (`PushDownFiltersOnCoalescedEqualJoinKeys`).
    fn pushdown_outer_join(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Join { conditions, .. } = &op.kind else {
            unreachable!()
        };
        let keys: Vec<(BoundExpr, BoundExpr)> = conditions
            .iter()
            .filter(|c| c.comparison == ComparisonOp::Equal)
            .map(|c| (c.left.clone(), c.right.clone()))
            .collect();
        if keys.is_empty() {
            return self.finish_pushdown(op);
        }
        let (mut left, mut right) = (FilterPushdown::default(), FilterPushdown::default());
        let mut applied = false;
        let mut kept = Vec::new();
        for filter in std::mem::take(&mut self.filters) {
            match coalesced_key_filters(&filter.filter, &keys) {
                Some((on_left, on_right)) => {
                    left.add_filter(on_left);
                    right.add_filter(on_right);
                    applied = true;
                }
                None => kept.push(filter),
            }
        }
        self.filters = kept;
        if !applied {
            return self.finish_pushdown(op);
        }
        if !left.combine() || !right.combine() {
            return empty_result(&op);
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        op.children = vec![left.rewrite(left_child), right.rewrite(right_child)];
        self.push_final_filters(op)
    }

    /// `FilterPushdown::PushdownLeftJoin`: filters on the left go below the join. One that
    /// removes the rows without a match makes this an inner join.
    fn pushdown_left_join(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let right_tables = plan_tables(&op.children[1]);
        let mut left = FilterPushdown::default();
        let mut remaining = Vec::new();
        let mut filters = std::mem::take(&mut self.filters);
        let removes_null = filters.iter().position(|f| {
            join_side(&f.tables, &left_tables, &right_tables) != JoinSide::Left
                && filter_removes_null(&f.filter, &right_tables)
        });
        if let Some(at) = removes_null {
            let LogicalOperatorKind::Join { join_type, .. } = &mut op.kind else {
                unreachable!()
            };
            *join_type = JoinType::Inner;
            // C++ goes on from the filter that removes NULLs, then the filters of the left side before it, then
            // the others before it
            let before: Vec<_> = filters.drain(..at).collect();
            let (left_before, others): (Vec<_>, Vec<_>) = before
                .into_iter()
                .partition(|f| join_side(&f.tables, &left_tables, &right_tables) == JoinSide::Left);
            filters.extend(left_before);
            filters.extend(others);
            self.filters = filters;
            return self.pushdown_inner_join(op);
        }
        for f in filters {
            if join_side(&f.tables, &left_tables, &right_tables) == JoinSide::Left {
                left.filters.push(f);
            } else {
                remaining.push(f);
            }
        }
        let mut right = FilterPushdown::default();
        // the join's conditions and the left side's filters tell filters of the right side: with
        // `i = a` and `i = 500`, `a = 500`
        let LogicalOperatorKind::Join {
            conditions, predicate, ..
        } = &op.kind
        else {
            unreachable!()
        };
        let mut combiner = super::filter_combiner::FilterCombiner::default();
        for condition in conditions {
            combiner.add(BoundExpr::new(
                BoundExprKind::Comparison {
                    op: condition.comparison,
                    left: Box::new(condition.left.clone()),
                    right: Box::new(condition.right.clone()),
                },
                duckrs_common::types::LogicalType::BOOLEAN,
            ));
        }
        if let Some(predicate) = predicate {
            let mut conjuncts = Vec::new();
            split_conjuncts(predicate.clone(), &mut conjuncts);
            for conjunct in conjuncts {
                combiner.add(conjunct);
            }
        }
        for f in &left.filters {
            combiner.add(f.filter.clone());
        }
        // only a comparison join (with conditions here) does this, not `LogicalAnyJoin`
        let generated = if conditions.is_empty() { Vec::new() } else { combiner.generate() };
        for generated in generated {
            let generated = Filter::new(generated);
            if join_side(&generated.tables, &left_tables, &right_tables) == JoinSide::Right {
                right.filters.push(generated);
            }
        }
        let LogicalOperatorKind::Join { predicate, .. } = &mut op.kind else {
            unreachable!()
        };
        if let Some(expr) = predicate.take() {
            let mut conjuncts = Vec::new();
            split_conjuncts(expr, &mut conjuncts);
            let (below, stay): (Vec<_>, Vec<_>) = conjuncts.into_iter().map(Filter::new).partition(|f| {
                // an ON condition of the right side alone drops right rows as well below the join,
                // but there it also sees rows without a partner
                join_side(&f.tables, &left_tables, &right_tables) == JoinSide::Right
                    && !f.filter.is_volatile()
                    && !can_throw(&f.filter)
            });
            right.filters = below;
            if !stay.is_empty() {
                *predicate = Some(conjunction_of(stay.into_iter().map(|f| f.filter).collect()));
            }
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        // a LEFT join on a condition that holds for no row: each left row with NULLs on the right
        let LogicalOperatorKind::Join { predicate, .. } = &op.kind else {
            unreachable!()
        };
        if predicate.as_ref().is_some_and(unsatisfiable) {
            let dummy = dummy_rhs(&right_child);
            let product = LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![left.rewrite(left_child), dummy]);
            return add_filter_operator(product, remaining.into_iter().map(|f| f.filter).collect());
        }
        op.children = vec![left.rewrite(left_child), right.rewrite(right_child)];
        add_filter_operator(op, remaining.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownSemiAntiJoin`: the output is the left side's, filters and all.
    fn pushdown_semi_anti_join(self, mut op: LogicalOperator) -> LogicalOperator {
        // a right row that matches a left row passes what the filters tell of the right side
        let mut right = FilterPushdown::default();
        if let LogicalOperatorKind::Join { conditions, .. } = &op.kind
            && !conditions.is_empty()
            && !self.filters.is_empty()
        {
            let filters: Vec<BoundExpr> = self.filters.iter().map(|f| f.filter.clone()).collect();
            for generated in super::filter_pullup::join_filters(conditions, &filters, false) {
                right.filters.push(Filter::new(generated));
            }
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        op.children = vec![self.rewrite(left_child), right.rewrite(right_child)];
        op
    }

    /// `FilterPushdown::PushdownProjection`.
    fn pushdown_projection(self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } = &op.kind
        else {
            unreachable!()
        };
        let table_index = *table_index;
        let projected = expressions.clone();
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        let mut split = Vec::new();
        let mut barred = Vec::new();
        for mut f in self.filters {
            // a barrier goes below only if all the others do, so that it is evaluated after them
            if contains_barrier(&f.filter) {
                barred.push(f);
                continue;
            }
            let mut columns = Vec::new();
            visit_columns(&mut f.filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            // copying a subquery would run it twice, and below the projection an entry that can
            // fail would run ahead of the filters there
            // `stays_on_top`: volatile, or a filter that can throw
            let on_top = can_throw(&f.filter)
                || columns.iter().any(|b| {
                    let entry = projected.get(b.column_index);
                    b.table_index != table_index || entry.is_none_or(|e| e.is_volatile() || contains_subquery(e))
                });
            // `splits_off`: what reads a computed column is evaluated once, below the filter, where
            // the projection computes it as it did
            let preserve = PRESERVE_COMPUTED.with(std::cell::Cell::get);
            if !on_top
                && preserve
                && columns.iter().any(|b| {
                    projected
                        .get(b.column_index)
                        .is_some_and(|e| !matches!(e.kind, BoundExprKind::ColumnRef { .. }))
                })
            {
                split.push(f.filter);
                continue;
            }
            if on_top {
                remaining.push(f.filter);
                continue;
            }
            let mut filter = f.filter;
            replace_columns(&mut filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    // `ReplaceProjectionBindings`: what the projection reads, by the column's name if it has one
                    let mut copy = projected[binding.column_index].clone();
                    if !e.alias.is_empty() {
                        copy.alias = e.alias.clone();
                    }
                    *e = copy;
                }
            });
            if !below.add_filter(filter) {
                return empty_result(&op);
            }
        }
        for mut f in barred {
            let mut columns = Vec::new();
            visit_columns(&mut f.filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            // a projection removes no rows, so what it computes may throw below the barrier too
            let stays = !remaining.is_empty()
                || !split.is_empty()
                || columns.iter().any(|b| {
                    let entry = projected.get(b.column_index);
                    b.table_index != table_index || entry.is_none_or(|e| e.is_volatile() || contains_subquery(e))
                });
            if stays {
                remaining.push(f.filter);
                continue;
            }
            let mut filter = f.filter;
            replace_columns(&mut filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    // the filter reads what the projection reads, by its own name
                    *e = projected[binding.column_index].clone();
                }
            });
            below.filters.push(Filter::new(filter));
        }
        if !split.is_empty() {
            return split_projection(op, below, split, remaining);
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining)
    }

    /// `FilterPushdown::PushdownSetOperation`.
    fn pushdown_set_operation(self, mut op: LogicalOperator) -> LogicalOperator {
        let (table_index, left_only) = match &op.kind {
            LogicalOperatorKind::Union { table_index, .. } => (*table_index, false),
            LogicalOperatorKind::ExceptIntersect { table_index, .. } => (*table_index, true),
            _ => unreachable!(),
        };
        let children = std::mem::take(&mut op.children);
        let mut result = Vec::with_capacity(children.len());
        for (index, child) in children.into_iter().enumerate() {
            let mut below = FilterPushdown::default();
            // a filter on the result of EXCEPT and INTERSECT only applies to their left child
            if index == 0 || !left_only {
                let bindings = child.column_bindings();
                for f in &self.filters {
                    let mut filter = f.filter.clone();
                    replace_set_operation_bindings(&mut filter, table_index, &bindings);
                    below.filters.push(Filter::new(filter));
                }
            }
            result.push(below.rewrite(child));
        }
        op.children = result;
        op
    }

    /// `FilterPushdown::PushdownDistinct`. DISTINCT ON picks a row of each group, so what it picks
    /// changes with a filter below it.
    fn pushdown_distinct(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Distinct { targets, .. } = &op.kind else {
            unreachable!()
        };
        if targets.len() != op.children[0].column_bindings().len() {
            return self.finish_pushdown(op);
        }
        let (volatile, pushed) = std::mem::take(&mut self.filters)
            .into_iter()
            .partition::<Vec<_>, _>(|f| f.filter.is_volatile());
        self.filters = pushed;
        let child = op.children.remove(0);
        op.children.push(self.rewrite(child));
        add_filter_operator(op, volatile.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownUnnest`.
    fn pushdown_unnest(self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Unnest { table_index, .. } = &op.kind else {
            unreachable!()
        };
        let table_index = *table_index;
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for f in self.filters {
            if f.tables.contains(&table_index) {
                remaining.push(f.filter);
            } else if !below.add_filter(f.filter) {
                return empty_result(&op);
            }
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining)
    }
}

/// What `(a1 AND b1) OR (a2 AND b2)` says of the side `a` of a join alone: `a1 OR a2`, which each row
/// has that the whole has. `None` if some alternative says nothing of that side.
/// `FlattenedCoalesce::Of`: the arguments of a COALESCE, those of COALESCEs in it in their place.
fn flattened_coalesce<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::Coalesce,
            children,
        } => children.iter().for_each(|child| flattened_coalesce(child, out)),
        _ => out.push(expr),
    }
}

/// `PushDownFiltersOnCoalescedEqualJoinKeys` of one filter: the filter with its COALESCEs of the keys of one
/// condition `l = r` as `l`, and as `r`, if it is a function of those alone.
fn coalesced_key_filters(filter: &BoundExpr, keys: &[(BoundExpr, BoundExpr)]) -> Option<(BoundExpr, BoundExpr)> {
    if filter.is_volatile() || super::rewriter::can_throw(filter) {
        return None;
    }
    // `Expression::Equals`, which leaves aliases out
    let shape = |expr: &BoundExpr| {
        let mut copy = expr.clone();
        copy.walk_mut(&mut |e| e.alias.clear());
        format!("{copy:?}")
    };
    let same = |a: &[&BoundExpr], b: &[&BoundExpr]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| shape(x) == shape(y))
    };
    let key_of = |expr: &BoundExpr| -> Option<usize> {
        if !matches!(expr.kind, BoundExprKind::Operator { op: BoundOperator::Coalesce, .. }) {
            return None;
        }
        let mut args = Vec::new();
        flattened_coalesce(expr, &mut args);
        keys.iter().position(|(l, r)| {
            let mut lr = Vec::new();
            flattened_coalesce(l, &mut lr);
            flattened_coalesce(r, &mut lr);
            let mut rl = Vec::new();
            flattened_coalesce(r, &mut rl);
            flattened_coalesce(l, &mut rl);
            same(&args, &lr) || same(&args, &rl)
        })
    };
    // the COALESCEs of the keys, all of the same condition
    let mut found: Option<usize> = None;
    let mut several = false;
    fn visit(expr: &BoundExpr, f: &mut impl FnMut(&BoundExpr) -> bool) {
        if f(expr) {
            return;
        }
        for child in expr.children() {
            visit(child, f);
        }
    }
    visit(filter, &mut |expr| match key_of(expr) {
        Some(key) => {
            several |= found.is_some_and(|k| k != key);
            found = Some(key);
            true
        }
        None => false,
    });
    let key = found.filter(|_| !several)?;
    let replaced = |with: &BoundExpr| {
        let mut copy = filter.clone();
        replace_coalesce(&mut copy, &key_of, key, with);
        copy
    };
    // `ExprIsFunctionOnlyOf`: with the COALESCEs as constants, nothing else of the rows is left
    let constant = BoundExpr::constant(Value::null(keys[key].0.return_type.clone()));
    let mut scalar = true;
    replaced(&constant).walk_mut(&mut |e| {
        if matches!(e.kind, BoundExprKind::ColumnRef { .. } | BoundExprKind::Subquery { .. }) {
            scalar = false;
        }
    });
    scalar.then(|| (replaced(&keys[key].0), replaced(&keys[key].1)))
}

/// `ReplaceIn`: the COALESCEs of the keys of condition `key` in `expr` become `with`.
fn replace_coalesce(
    expr: &mut BoundExpr,
    key_of: &impl Fn(&BoundExpr) -> Option<usize>,
    key: usize,
    with: &BoundExpr,
) {
    if key_of(expr) == Some(key) {
        *expr = with.clone();
        return;
    }
    for child in expr.children_mut() {
        replace_coalesce(child, key_of, key, with);
    }
}

fn side_of_disjunction(expr: &BoundExpr, side: &BTreeSet<usize>) -> Option<BoundExpr> {
    let BoundExprKind::Conjunction { and: false, children } = &expr.kind else {
        return None;
    };
    let mut alternatives = Vec::with_capacity(children.len());
    for child in children {
        let mut conjuncts = Vec::new();
        split_conjuncts(child.clone(), &mut conjuncts);
        conjuncts.retain(|conjunct| {
            let mut tables = BTreeSet::new();
            expression_tables(conjunct, &mut tables);
            !tables.is_empty()
                && tables.is_subset(side)
                && !conjunct.is_volatile()
                && !can_throw(conjunct)
                && !contains_subquery(conjunct)
        });
        if conjuncts.is_empty() {
            return None;
        }
        alternatives.push(conjunction_of(conjuncts));
    }
    Some(BoundExpr::new(
        BoundExprKind::Conjunction {
            and: false,
            children: alternatives,
        },
        duckrs_common::types::LogicalType::BOOLEAN,
    ))
}

/// `FilterCombiner::GetComparedExpression`: the one side that is not constant of a comparison,
/// or the first child of BETWEEN and IN.
/// `FilterPushdown::OnlyRanksWithoutArguments`: the windows rank rows by nothing but their partitions and orders.
pub(super) fn only_ranks_without_arguments(windows: &[crate::planner::expression::BoundWindow]) -> bool {
    windows.iter().all(|w| {
        matches!(
            w.function,
            WindowFunction::RowNumber
                | WindowFunction::Rank
                | WindowFunction::DenseRank
                | WindowFunction::PercentRank
                | WindowFunction::CumeDist
                | WindowFunction::Ntile
        ) && !window_can_throw(w)
            && w.children.is_empty()
            && w.filter.is_none()
            && w.start_expr.is_none()
            && w.end_expr.is_none()
    })
}

/// `FilterPushdown::ComparesWindowPartitions`: `filter` compares a partition of every window with constants.
pub(super) fn compares_window_partitions(windows: &[crate::planner::expression::BoundWindow], filter: &BoundExpr) -> bool {
    let Some(compared) = compared_expression(filter) else {
        return false;
    };
    !windows.is_empty()
        && windows.iter().all(|w| {
            w.partitions.iter().any(|p| match (&p.kind, &compared.kind) {
                (BoundExprKind::ColumnRef { binding: a, .. }, BoundExprKind::ColumnRef { binding: b, .. }) => a == b,
                _ => expressions_equal(p, compared),
            })
        })
}

fn compared_expression(filter: &BoundExpr) -> Option<&BoundExpr> {
    if filter.is_volatile() {
        return None;
    }
    let (children, first_only): (Vec<&BoundExpr>, bool) = match &filter.kind {
        BoundExprKind::Comparison { op, left, right }
            if matches!(
                op,
                ComparisonOp::Equal
                    | ComparisonOp::NotEqual
                    | ComparisonOp::LessThan
                    | ComparisonOp::LessThanOrEqual
                    | ComparisonOp::GreaterThan
                    | ComparisonOp::GreaterThanOrEqual
            ) =>
        {
            (vec![&**left, &**right], false)
        }
        BoundExprKind::Between { input, lower, upper } => (vec![&**input, &**lower, &**upper], true),
        BoundExprKind::Operator {
            op: BoundOperator::In,
            children,
        } => (children.iter().collect(), true),
        _ => return None,
    };
    let mut compared = None;
    for (i, child) in children.into_iter().enumerate() {
        if !child.is_foldable() {
            if compared.is_some() || (first_only && i != 0) {
                return None;
            }
            compared = Some(child);
        }
    }
    compared
}

fn contains_subquery(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(contains_subquery)
}

fn replace_set_operation_bindings(filter: &mut BoundExpr, table_index: usize, bindings: &[ColumnBinding]) {
    visit_columns(filter, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind
            && binding.table_index == table_index
        {
            *binding = bindings[binding.column_index];
        }
    });
}

/// `FilterPushdown::AddLogicalFilter`.
/// `FilterPushdown::SplitProjection`: the columns the `split` filters read are computed by a
/// projection of their own below them, which passes on what the others read.
fn split_projection(
    mut op: LogicalOperator,
    below: FilterPushdown,
    mut split: Vec<BoundExpr>,
    remaining: Vec<BoundExpr>,
) -> LogicalOperator {
    let LogicalOperatorKind::Projection {
        table_index,
        expressions,
    } = &mut op.kind
    else {
        unreachable!()
    };
    let table_index = *table_index;
    let mut referenced = vec![false; expressions.len()];
    for filter in &mut split {
        visit_columns(filter, &mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = e.kind
                && binding.table_index == table_index
                && let Some(r) = referenced.get_mut(binding.column_index)
            {
                *r = true;
            }
        });
    }
    if referenced.iter().all(|r| *r) {
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        let mut all = remaining;
        all.extend(split);
        return add_filter_operator(op, all);
    }
    let lower_index = generate_table_index();
    let mut lower_expressions: Vec<BoundExpr> = Vec::new();
    let mut lower_positions = vec![None; expressions.len()];
    for (at, expr) in expressions.iter_mut().enumerate() {
        if !referenced[at] {
            continue;
        }
        lower_positions[at] = Some(lower_expressions.len());
        let column = BoundExpr::column_ref(
            ColumnBinding::new(lower_index, lower_expressions.len()),
            expr.return_type.clone(),
            expr.alias.clone(),
        );
        lower_expressions.push(std::mem::replace(expr, column));
    }
    // `AddPassThroughColumns`
    let mut pass_through: Vec<(ColumnBinding, usize)> = Vec::new();
    for (at, expr) in expressions.iter_mut().enumerate() {
        if referenced[at] {
            continue;
        }
        expr.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind {
                let position = match pass_through.iter().find(|(b, _)| b == binding) {
                    Some((_, position)) => *position,
                    None => {
                        let position = lower_expressions.len();
                        pass_through.push((*binding, position));
                        let mut copy = BoundExpr::column_ref(*binding, e.return_type.clone(), e.alias.clone());
                        copy.alias.clone_from(&e.alias);
                        lower_expressions.push(copy);
                        position
                    }
                };
                *binding = ColumnBinding::new(lower_index, position);
            }
        });
    }
    // `ReplaceFilterBindings`
    for filter in &mut split {
        replace_columns(filter, &mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                && binding.table_index == table_index
                && let Some(Some(position)) = lower_positions.get(binding.column_index)
            {
                *binding = ColumnBinding::new(lower_index, *position);
            }
        });
    }
    let child = op.children.remove(0);
    let lower = LogicalOperator::new(
        LogicalOperatorKind::Projection {
            table_index: lower_index,
            expressions: lower_expressions,
        },
        vec![below.rewrite(child)],
    );
    op.children.push(add_filter_operator(lower, split));
    add_filter_operator(op, remaining)
}

fn add_filter_operator(op: LogicalOperator, mut expressions: Vec<BoundExpr>) -> LogicalOperator {
    // the barred filters run after the others
    expressions.sort_by_key(contains_barrier);
    if expressions.is_empty() {
        return op;
    }
    LogicalOperator::new(LogicalOperatorKind::Filter { expressions }, vec![op])
}

/// `JoinCondition::CreateExpression`.
pub(super) fn condition_expression(condition: JoinCondition) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op: condition.comparison,
            left: Box::new(condition.left),
            right: Box::new(condition.right),
        },
        duckrs_common::types::LogicalType::BOOLEAN,
    )
}

/// `LogicalComparisonJoin::ExtractJoinConditions`: equalities with a side of the join on each
/// side of them become conditions, which a hash join can use. The rest is a predicate.
fn extract_join_conditions(
    expressions: Vec<BoundExpr>,
    left: &BTreeSet<usize>,
    right: &BTreeSet<usize>,
) -> (Vec<JoinCondition>, Vec<BoundExpr>) {
    let mut conditions = Vec::new();
    let mut predicates = Vec::new();
    for expr in expressions {
        if let BoundExprKind::Comparison {
            op: op @ (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom),
            left: first,
            right: second,
        } = &expr.kind
        {
            let tables = |e: &BoundExpr| {
                let mut tables = BTreeSet::new();
                expression_tables(e, &mut tables);
                tables
            };
            let sides = (
                join_side(&tables(first), left, right),
                join_side(&tables(second), left, right),
            );
            let null_safe = *op == ComparisonOp::NotDistinctFrom;
            let sides = match sides {
                (JoinSide::Left, JoinSide::Right) => Some((first, second)),
                (JoinSide::Right, JoinSide::Left) => Some((second, first)),
                _ => None,
            };
            if let Some((l, r)) = sides {
                conditions.push(JoinCondition {
                    bound: None,
                    left: (**l).clone(),
                    right: (**r).clone(),
                    comparison: JoinCondition::equality(null_safe),
                    filter: None,
                });
                continue;
            }
        }
        predicates.push(expr);
    }
    (conditions, predicates)
}

/// How many times the expressions of a plan read each column.
fn count_columns(plan: &mut LogicalOperator, reads: &mut HashMap<ColumnBinding, usize>) {
    plan.visit_expressions(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = e.kind {
            *reads.entry(binding).or_default() += 1;
        }
    });
}

/// `PushdownMarkJoin`: a filter on only the marker of a MARK join makes it a SEMI join, as false
/// and NULL both lose the row. `CheckMarkToSemi`: that takes the marker away, so nothing else may
/// read it. A filter on `NOT marker` makes it an ANTI join where the marker is never NULL.
pub(super) fn convert_mark_joins(plan: &mut LogicalOperator) {
    let mut reads = HashMap::new();
    count_columns(plan, &mut reads);
    mark_to_semi(plan, &reads);
}

/// Whether an expression is true or false for any row: a test for NULL, or such tests together.
fn never_null(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Conjunction { children, .. } => children.iter().all(never_null),
        BoundExprKind::Operator {
            op: BoundOperator::IsNull | BoundOperator::IsNotNull,
            ..
        } => true,
        BoundExprKind::Comparison { op, .. } => {
            matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom)
        }
        _ => false,
    }
}

fn mark_to_semi(op: &mut LogicalOperator, reads: &HashMap<ColumnBinding, usize>) {
    for child in &mut op.children {
        mark_to_semi(child, reads);
    }
    if let LogicalOperatorKind::LateralJoin { right, .. } = &mut op.kind {
        mark_to_semi(right, reads);
    }
    let LogicalOperatorKind::Filter { expressions } = &mut op.kind else {
        return;
    };
    let LogicalOperatorKind::MarkJoin {
        table_index,
        conditions,
        predicate,
    } = &mut op.children[0].kind
    else {
        return;
    };
    let marker = ColumnBinding::new(*table_index, 0);
    let is_marker =
        |e: &BoundExpr| matches!(e.kind, BoundExprKind::ColumnRef { binding, depth: 0 } if binding == marker);
    let is_not_marker = |e: &BoundExpr| matches!(&e.kind, BoundExprKind::Operator { op: BoundOperator::Not, children } if is_marker(&children[0]));
    if conditions.is_empty() || reads.get(&marker) != Some(&1) {
        return;
    }
    let join_type = if expressions.iter().any(is_marker) {
        JoinType::Semi
    } else if expressions.iter().any(is_not_marker)
        && conditions.iter().all(|condition| condition.null_safe())
        && predicate.as_ref().is_none_or(never_null)
    {
        JoinType::Anti
    } else {
        return;
    };
    let (conditions, predicate) = (std::mem::take(conditions), predicate.take());
    expressions.retain(|e| !is_marker(e) && !is_not_marker(e));
    op.children[0].kind = LogicalOperatorKind::Join {
        join_type,
        conditions,
        predicate,
        algorithm: Default::default(),
    };
    if expressions.is_empty() {
        *op = op.children.remove(0);
    }
}

/// `FilterRemovesNull`: whether a filter is false or NULL for a row whose right side is NULL.
fn filter_removes_null(filter: &BoundExpr, right: &BTreeSet<usize>) -> bool {
    let mut copy = filter.clone();
    visit_columns(&mut copy, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = e.kind
            && right.contains(&binding.table_index)
        {
            e.kind = BoundExprKind::Constant(Value::null(e.return_type.clone()));
        }
    });
    rewrite_expression(&mut copy, true);
    let mut conjuncts = Vec::new();
    split_conjuncts(copy, &mut conjuncts);
    for conjunct in conjuncts {
        if !conjunct.is_foldable() {
            return false;
        }
        if let Some(None | Some(false)) = fold_boolean(&conjunct) {
            return true;
        }
    }
    false
}

/// `LogicalOperator::EstimateCardinality` before the join order sets estimates: the rows of a scan, and otherwise the
/// most rows of a child, as an operator reports them without knowing what it removes.
fn plain_estimate(op: &LogicalOperator) -> u64 {
    match &op.kind {
        LogicalOperatorKind::Get { table, .. } => table.row_count() as u64,
        LogicalOperatorKind::ExpressionGet { expressions, .. } => expressions.len() as u64,
        LogicalOperatorKind::SecureView { estimate: Some(rows), .. } => *rows,
        _ if op.children.is_empty() => super::join_order::estimated_cardinality(op),
        _ => op.children.iter().map(plain_estimate).max().unwrap_or(1),
    }
}
