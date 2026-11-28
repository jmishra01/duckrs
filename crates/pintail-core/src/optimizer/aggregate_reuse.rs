// Ported from duckdb/src/optimizer/aggregate_reuse.cpp @ rust-port-baseline
//
//! `AggregateReuseOptimizer::TryReuseSemiAggregate`: an aggregate of a table joined by its key,
//! where the rows are filtered by a SEMI join with the same aggregate of the same table grouped by
//! that key (`WHERE k IN (SELECT k FROM fact GROUP BY k HAVING sum(v) > 25)`), takes the states of
//! the aggregate of the SEMI join instead of reading the table again. The SEMI join becomes an
//! INNER join that brings the state along, and the aggregate above combines the states.
//!
//! `TryReuseMaterializedAggregate` is in `materialized_aggregate_reuse`.
//!
//! pintail turns `sum` into `sum_no_overflow` before this pass, where C++ does it later: the two
//! are the same aggregate here, and the reused one is a `sum` again.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::function::BuiltinFunctions;
use crate::function::aggregates::export_state;
use crate::function::binder::bind_aggregate_function;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use pintail_common::types::LogicalType;
use pintail_parser::ast::{ComparisonOp, JoinType};
use std::collections::HashMap;
use std::sync::Arc;

type Replacements = HashMap<ColumnBinding, ColumnBinding>;

/// Runs the pass over a plan.
pub fn reuse(mut plan: LogicalOperator) -> LogicalOperator {
    let mut pass = Pass {
        next: max_table_index(&plan) + 1,
        replacements: Replacements::new(),
        ctes: HashMap::new(),
    };
    pass.visit(&mut plan);
    plan
}

struct Pass {
    next: usize,
    replacements: Replacements,
    /// `cte_definitions`: what each materialized CTE runs, with its table
    ctes: HashMap<
        usize,
        (
            LogicalOperator,
            Arc<parking_lot::Mutex<pintail_common::column_data::ColumnDataCollection>>,
        ),
    >,
}

/// `TableColumnOrigin`: a column of a table.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(super) struct Origin {
    pub table: usize,
    pub column: usize,
}

impl Pass {
    fn fresh(&mut self) -> usize {
        let index = self.next;
        self.next += 1;
        index
    }

    fn visit(&mut self, op: &mut LogicalOperator) {
        for (at, child) in op.children.iter_mut().enumerate() {
            self.visit(child);
            // `CollectCTEs`: the definition as the query after it sees it
            if at == 0
                && let K::MaterializedCte { table_index, data, .. } = &op.kind
            {
                self.ctes.insert(*table_index, (child.clone(), data.clone()));
            }
        }
        if !self.replacements.is_empty() {
            let replacements = &self.replacements;
            visit_bindings(&mut op.kind, &mut |binding, depth| {
                if depth == 0
                    && let Some(new) = replacements.get(binding)
                {
                    *binding = *new;
                }
            });
        }
        let reused = super::materialized_aggregate_reuse::try_reuse(op, &self.ctes, &mut self.next);
        if !reused {
            self.try_reuse_semi_aggregate(op);
        }
    }

    /// `TryReuseSemiAggregate`.
    fn try_reuse_semi_aggregate(&mut self, op: &mut LogicalOperator) -> bool {
        let Some(candidate) = candidate(op) else {
            return false;
        };
        let K::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            group_index,
            aggregate_index,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let outer = &aggregates[0];
        let Some(exported_outer) = exported(outer) else {
            return false;
        };
        let state_type = exported_outer.return_type.clone();
        let join = &op.children[0];
        let owner = &join.children[candidate.owner_child];
        let probe = &join.children[candidate.probe_child];
        let Some(key_origin) = origin_at(probe, candidate.probe_key) else {
            return false;
        };
        let owner_key = owner.column_bindings()[candidate.owner_key];
        // validated on a copy, which takes the place of the owner where it works out
        let mut promoted = owner.clone();
        let reuse = Reuse {
            probe,
            key_origin: &key_origin,
            outer,
            state_type: &state_type,
        };
        let Some(payload) = reuse.promote(&mut promoted, owner_key) else {
            return false;
        };
        let Some(combine) = BuiltinFunctions::get().aggregate("combine_aggr") else {
            return false;
        };
        let Ok(combined) = bind_aggregate_function(
            combine,
            vec![BoundExpr::column_ref(payload, state_type.clone(), "")],
            None,
            false,
        ) else {
            return false;
        };
        if combined.return_type != state_type {
            return false;
        }
        let upper_groups: Vec<BoundExpr> = candidate
            .owner_groups
            .iter()
            .zip(groups)
            .map(|(binding, group)| BoundExpr::column_ref(*binding, group.return_type.clone(), group.alias.clone()))
            .collect();
        let (upper_group_index, upper_aggregate_index, upper_groupings_index, projection_index) =
            (self.fresh(), self.fresh(), self.fresh(), self.fresh());
        let state = BoundExpr::column_ref(ColumnBinding::new(upper_aggregate_index, 0), state_type.clone(), "");
        let Ok(finalized) = crate::function::extras::bind_function("finalize", &[], vec![state]) else {
            return false;
        };
        if finalized.return_type != outer.return_type {
            return false;
        }
        // from here on the rewrite does not fail
        let mut expressions = Vec::with_capacity(groups.len() + 1);
        for (i, group) in groups.iter().enumerate() {
            expressions.push(BoundExpr::column_ref(
                ColumnBinding::new(upper_group_index, i),
                group.return_type.clone(),
                group.alias.clone(),
            ));
            self.replacements.insert(
                ColumnBinding::new(*group_index, i),
                ColumnBinding::new(projection_index, i),
            );
        }
        expressions.push(finalized);
        self.replacements.insert(
            ColumnBinding::new(*aggregate_index, 0),
            ColumnBinding::new(projection_index, groups.len()),
        );
        let upper = LogicalOperator::new(
            K::Aggregate {
                group_index: upper_group_index,
                aggregate_index: upper_aggregate_index,
                groupings_index: upper_groupings_index,
                groups: upper_groups,
                aggregates: vec![combined],
                grouping_sets: grouping_sets.clone(),
                grouping_functions: Vec::new(),
                perfect_hash: None,
                domain_cap: None,
            },
            vec![promoted],
        );
        *op = LogicalOperator::new(
            K::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![upper],
        );
        true
    }
}

/// `AggregateJoinCandidate`.
struct Candidate {
    owner_child: usize,
    probe_child: usize,
    owner_key: usize,
    probe_key: usize,
    owner_groups: Vec<ColumnBinding>,
}

/// `GetCandidate`: one aggregate of a column of one side of an INNER join on one key, grouped by
/// columns of the other side.
fn candidate(op: &LogicalOperator) -> Option<Candidate> {
    let K::Aggregate { groups, aggregates, .. } = &op.kind else {
        return None;
    };
    let [join] = op.children.as_slice() else {
        return None;
    };
    let K::Join {
        join_type: JoinType::Inner,
        conditions,
        predicate: None,
        ..
    } = &join.kind
    else {
        return None;
    };
    if join.children.len() != 2 || conditions.len() != 1 || aggregates.len() != 1 || !has_complete_grouping(op) {
        return None;
    }
    let condition = &conditions[0];
    if condition.comparison != ComparisonOp::Equal || condition.left.return_type != condition.right.return_type {
        return None;
    }
    let left_key = reference_index(&condition.left, &join.children[0])?;
    let right_key = reference_index(&condition.right, &join.children[1])?;
    let outer = &aggregates[0];
    if outer.distinct
        || outer.filter.is_some()
        || !outer.order_by.is_empty()
        || outer.children.len() != 1
        || outer.function.export.is_some()
        || outer.children[0].is_volatile()
        || exported(outer).is_none()
    {
        return None;
    }
    for owner_child in 0..2 {
        let probe_child = 1 - owner_child;
        if reference_index(&outer.children[0], &join.children[probe_child]).is_none() {
            continue;
        }
        let owner_bindings = join.children[owner_child].column_bindings();
        let owner_groups: Option<Vec<ColumnBinding>> = groups
            .iter()
            .map(|group| reference_index(group, &join.children[owner_child]).map(|at| owner_bindings[at]))
            .collect();
        if let Some(owner_groups) = owner_groups {
            let (owner_key, probe_key) = if owner_child == 0 {
                (left_key, right_key)
            } else {
                (right_key, left_key)
            };
            return Some(Candidate {
                owner_child,
                probe_child,
                owner_key,
                probe_key,
                owner_groups,
            });
        }
    }
    None
}

/// `HasCompleteGrouping`: groups, all of them in the one grouping set.
pub(super) fn has_complete_grouping(op: &LogicalOperator) -> bool {
    let K::Aggregate {
        groups,
        grouping_sets,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return false;
    };
    if groups.is_empty() || !grouping_functions.is_empty() || grouping_sets.len() > 1 {
        return false;
    }
    grouping_sets
        .first()
        .is_none_or(|set| set.len() == groups.len() && (0..groups.len()).all(|g| set.contains(&g)))
}

/// `GetDirectReferenceIndex`: the output of `input` that `expr` is.
pub(super) fn reference_index(expr: &BoundExpr, input: &LogicalOperator) -> Option<usize> {
    let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind else {
        return None;
    };
    input.column_bindings().iter().position(|b| b == binding)
}

/// `GetTableColumnOrigin` without filters: the column of a table that output `at` of `op` is.
fn origin_at(op: &LogicalOperator, at: usize) -> Option<Origin> {
    match &op.kind {
        K::Projection { expressions, .. } => {
            let [child] = op.children.as_slice() else {
                return None;
            };
            origin_at(child, reference_index(expressions.get(at)?, child)?)
        }
        K::Get {
            table,
            column_ids,
            filters,
            zone_filters,
            index_rows: None,
            sample: None,
            ..
        } if filters.is_empty() && zone_filters.is_empty() && op.children.is_empty() => {
            let binding = *op.column_bindings().get(at)?;
            Some(Origin {
                table: Arc::as_ptr(table) as usize,
                column: *column_ids.get(binding.column_index)?,
            })
        }
        _ => None,
    }
}

fn origin_of(op: &LogicalOperator, expr: &BoundExpr) -> Option<Origin> {
    origin_at(op, reference_index(expr, op)?)
}

/// `FindTracedOutput`: the output of `op` that is `target` of an operator below.
fn traced_output(op: &LogicalOperator, target: ColumnBinding) -> Option<usize> {
    if let Some(at) = op.column_bindings().iter().position(|b| *b == target) {
        return Some(at);
    }
    let [child] = op.children.as_slice() else {
        return None;
    };
    match &op.kind {
        K::Filter { .. } => traced_output(child, target),
        K::Projection { expressions, .. } => {
            let traced = traced_output(child, target)?;
            expressions
                .iter()
                .position(|expr| reference_index(expr, child) == Some(traced))
        }
        _ => None,
    }
}

/// `FindUnaryAggregate`: the aggregate below projections and filters.
pub(super) fn unary_aggregate_path(op: &LogicalOperator) -> Option<usize> {
    let mut depth = 0;
    let mut current = op;
    while !matches!(current.kind, K::Aggregate { .. }) {
        let [child] = current.children.as_slice() else {
            return None;
        };
        if !matches!(current.kind, K::Projection { .. } | K::Filter { .. }) {
            return None;
        }
        current = child;
        depth += 1;
    }
    Some(depth)
}

fn nth_below(op: &mut LogicalOperator, depth: usize) -> &mut LogicalOperator {
    let mut current = op;
    for _ in 0..depth {
        current = &mut current.children[0];
    }
    current
}

/// The aggregate as it is reused: a `sum_no_overflow` is a `sum` (see the top of this file).
fn normalized(aggregate: &BoundAggregate) -> Option<BoundAggregate> {
    if aggregate.function.name != "sum_no_overflow" {
        return Some(aggregate.clone());
    }
    let set = BuiltinFunctions::get().aggregate("sum")?;
    let bound = bind_aggregate_function(set, aggregate.children.clone(), None, false).ok()?;
    (bound.return_type == aggregate.return_type).then_some(bound)
}

/// `ExportAggregateFunction::Bind`.
fn exported(aggregate: &BoundAggregate) -> Option<BoundAggregate> {
    let aggregate = normalized(aggregate)?;
    let types: Vec<LogicalType> = aggregate.children.iter().map(|c| c.return_type.clone()).collect();
    let (function, ty) = export_state(&aggregate.function, &aggregate.return_type, &types, Vec::new()).ok()?;
    if !ty.is_aggregate_state() {
        return None;
    }
    let mut copy = aggregate;
    copy.function = Arc::new(function);
    copy.return_type = ty;
    Some(copy)
}

/// What the owner side is searched for.
struct Reuse<'a> {
    probe: &'a LogicalOperator,
    key_origin: &'a Origin,
    outer: &'a BoundAggregate,
    state_type: &'a LogicalType,
}

impl Reuse<'_> {
    /// `SameReusableAggregate`: the aggregate of `outer` over the same column of the same table.
    fn same_aggregate(&self, inner: &BoundAggregate, inner_input: &LogicalOperator) -> bool {
        let (Some(outer), Some(inner_normalized)) = (normalized(self.outer), normalized(inner)) else {
            return false;
        };
        if outer.distinct
            || inner.distinct
            || outer.function.name != inner_normalized.function.name
            || outer.function.kind != inner_normalized.function.kind
            || outer.return_type != inner.return_type
            || outer.function.export.is_some()
            || inner.function.export.is_some()
            || outer.filter.is_some()
            || inner.filter.is_some()
            || !outer.order_by.is_empty()
            || !inner.order_by.is_empty()
            || outer.children.len() != 1
            || inner.children.len() != 1
        {
            return false;
        }
        match (
            origin_of(self.probe, &outer.children[0]),
            origin_of(inner_input, &inner.children[0]),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// `PromoteReusableAggregate`: below `op`, a SEMI join on `key` with an aggregate that is the
    /// outer one grouped by the key, made an INNER join that brings the state up to the output of
    /// `op`, whose binding is the result.
    fn promote(&self, op: &mut LogicalOperator, key: ColumnBinding) -> Option<ColumnBinding> {
        match &op.kind {
            K::Projection {
                table_index,
                expressions,
            } => {
                let table_index = *table_index;
                if op.children.len() != 1 || key.table_index != table_index {
                    return None;
                }
                let child_at = reference_index(expressions.get(key.column_index)?, &op.children[0])?;
                let child_key = op.children[0].column_bindings()[child_at];
                let payload = self.promote(&mut op.children[0], child_key)?;
                let K::Projection { expressions, .. } = &mut op.kind else {
                    unreachable!()
                };
                expressions.push(BoundExpr::column_ref(payload, self.state_type.clone(), ""));
                Some(ColumnBinding::new(table_index, expressions.len() - 1))
            }
            K::Filter { .. } => {
                if op.children.len() != 1 || !op.column_bindings().contains(&key) {
                    return None;
                }
                self.promote(&mut op.children[0], key)
            }
            K::Join {
                join_type: JoinType::Semi,
                ..
            } => self.promote_semi(op, key),
            K::Join {
                join_type: JoinType::Inner,
                ..
            } => {
                for child in &mut op.children {
                    if !child.column_bindings().contains(&key) {
                        continue;
                    }
                    if let Some(payload) = self.promote(child, key) {
                        return Some(payload);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn promote_semi(&self, op: &mut LogicalOperator, key: ColumnBinding) -> Option<ColumnBinding> {
        let K::Join {
            conditions, predicate, ..
        } = &op.kind
        else {
            unreachable!()
        };
        if op.children.len() != 2
            || conditions.len() != 1
            || predicate.is_some()
            || conditions[0].comparison != ComparisonOp::Equal
            || !matches!(op.children[1].kind, K::Projection { .. })
        {
            return None;
        }
        let left_key = reference_index(&conditions[0].left, &op.children[0])?;
        let right_key = reference_index(&conditions[0].right, &op.children[1])?;
        if op.children[0].column_bindings()[left_key] != key {
            return None;
        }
        let right_projection = &op.children[1];
        let K::Projection {
            table_index: right_index,
            expressions: right_expressions,
        } = &right_projection.kind
        else {
            unreachable!()
        };
        let right_index = *right_index;
        let below = &right_projection.children[0];
        let depth = unary_aggregate_path(below)?;
        let mut inner = below;
        for _ in 0..depth {
            inner = &inner.children[0];
        }
        let K::Aggregate {
            group_index,
            aggregate_index,
            groups,
            aggregates,
            grouping_sets,
            grouping_functions,
            ..
        } = &inner.kind
        else {
            unreachable!()
        };
        if inner.children.len() != 1
            || groups.len() != 1
            || grouping_sets.len() > 1
            || !grouping_functions.is_empty()
            || grouping_sets
                .first()
                .is_some_and(|set| set.len() != 1 || !set.contains(&0))
        {
            return None;
        }
        // the SEMI join reads the group of the aggregate
        let traced = traced_output(below, ColumnBinding::new(*group_index, 0))?;
        if reference_index(&right_expressions[right_key], below) != Some(traced) {
            return None;
        }
        if origin_of(&inner.children[0], &groups[0]).as_ref() != Some(self.key_origin) {
            return None;
        }
        let mut found = None;
        for (at, aggregate) in aggregates.iter().enumerate() {
            if !self.same_aggregate(aggregate, &inner.children[0]) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(at);
        }
        let at = found?;
        let exported = exported(&aggregates[at])?;
        if exported.return_type != *self.state_type {
            return None;
        }
        let aggregate_binding = ColumnBinding::new(*aggregate_index, at);
        // the operators between read the state now, finalized (`FinalizeAggregateConsumers`)
        let below = &mut op.children[1].children[0];
        finalize_consumers(below, depth, aggregate_binding, self.state_type)?;
        let K::Aggregate { aggregates, .. } = &mut nth_below(below, depth).kind else {
            unreachable!()
        };
        aggregates[at] = exported;
        // `PromoteAggregateBinding`
        let payload = promote_binding(below, depth, aggregate_binding, self.state_type);
        let K::Projection { expressions, .. } = &mut op.children[1].kind else {
            unreachable!()
        };
        expressions.push(BoundExpr::column_ref(payload, self.state_type.clone(), ""));
        let result = ColumnBinding::new(right_index, expressions.len() - 1);
        let K::Join { join_type, .. } = &mut op.kind else {
            unreachable!()
        };
        *join_type = JoinType::Inner;
        Some(result)
    }
}

/// `FinalizeAggregateConsumers`: the projections and filters above the aggregate `depth` below
/// `op` read `binding` finalized.
fn finalize_consumers(
    op: &mut LogicalOperator,
    depth: usize,
    binding: ColumnBinding,
    state: &LogicalType,
) -> Option<()> {
    if depth == 0 {
        return Some(());
    }
    finalize_consumers(&mut op.children[0], depth - 1, binding, state)?;
    let mut failed = false;
    for expr in op.expressions_mut() {
        finalize_references(expr, binding, state, &mut failed);
    }
    (!failed).then_some(())
}

fn finalize_references(expr: &mut BoundExpr, binding: ColumnBinding, state: &LogicalType, failed: &mut bool) {
    if let BoundExprKind::ColumnRef { binding: b, depth: 0 } = &expr.kind
        && *b == binding
    {
        let state_ref = BoundExpr::column_ref(binding, state.clone(), expr.alias.clone());
        match crate::function::extras::bind_function("finalize", &[], vec![state_ref]) {
            Ok(finalized) => *expr = finalized,
            Err(_) => *failed = true,
        }
        return;
    }
    for child in expr.children_mut() {
        finalize_references(child, binding, state, failed);
    }
}

/// `PromoteAggregateBinding`: `binding` of the aggregate `depth` below `op` in the output of `op`.
fn promote_binding(
    op: &mut LogicalOperator,
    depth: usize,
    binding: ColumnBinding,
    state: &LogicalType,
) -> ColumnBinding {
    if depth == 0 {
        return binding;
    }
    let child_binding = promote_binding(&mut op.children[0], depth - 1, binding, state);
    match &mut op.kind {
        K::Projection {
            table_index,
            expressions,
        } => {
            expressions.push(BoundExpr::column_ref(child_binding, state.clone(), ""));
            ColumnBinding::new(*table_index, expressions.len() - 1)
        }
        _ => child_binding,
    }
}
