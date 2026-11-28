// Ported from duckdb/src/optimizer/partial_aggregate_pushdown.cpp @ rust-port-baseline
//
//! `PartialAggregatePushdown`: an aggregate over a join can aggregate what the join reads first.
//!
//! - One side (`TryPushdownAggregate`): when the aggregates read one side only, that side is grouped
//!   by its join keys and the groups of the query on it, exporting the states, and the aggregate
//!   above the join combines them. It pays when that side has many rows per key and the join keeps
//!   most of them.
//! - Both sides (`TryDoubleEagerPushdown`, "eager group-by-count"): both sides are grouped by the
//!   join keys, each with a count of its rows, and a state above counts as often as the other side
//!   has rows for its key.
//! - The count of an outer join (`TryOwnerOuterCountPushdown`): a count by the key of the side a
//!   LEFT or RIGHT join keeps is a sum of the counts of the other side by its key.
//!
//! The estimates are duckrs's of the plan as it is, where C++ reads the ones the join order
//! optimizer left on the operators. The aggregates with constant parameters (`string_agg`,
//! `quantile_cont`, ...) stay where they are: duckrs packs those into the argument.

use super::common_subplan::{max_table_index, visit_bindings};
use super::join_order::{estimated_cardinality, relation_stats};
use super::relation_statistics::CountSource;
use crate::function::BuiltinFunctions;
use crate::function::aggregates::{AggregateKind, export_state, resolve_state};
use crate::function::binder::bind_aggregate_function;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `PartialAggregatePushdownHeuristics`.
const MIN_AGGREGATE_TO_DIMENSION_RATIO: f64 = 4.0;
const MAX_EXTRA_LOWER_GROUPS: usize = 1;
/// Avoid eager aggregation when the join discards much of the aggregate side.
const MIN_JOIN_RETENTION: f64 = 0.7;
/// `DoubleEagerHeuristics`.
const MAX_SMALL_GROUP_COUNT: u64 = 2 * 2048;
const MIN_SMALL_COLLAPSE: f64 = 2.0;
const MIN_LARGE_COLLAPSE: f64 = 8.0;

type Replacements = HashMap<ColumnBinding, ColumnBinding>;
type Sides = [HashSet<usize>; 2];

/// Runs the pass over a plan.
pub fn push_down(mut plan: LogicalOperator) -> LogicalOperator {
    let mut pass = Pass {
        next: max_table_index(&plan) + 1,
        replacements: Replacements::new(),
    };
    pass.visit(&mut plan);
    plan
}

struct Pass {
    next: usize,
    replacements: Replacements,
}

impl Pass {
    fn fresh(&mut self) -> usize {
        let index = self.next;
        self.next += 1;
        index
    }

    /// `VisitOperator`: the children first, then the columns this one reads, then this one.
    fn visit(&mut self, op: &mut LogicalOperator) {
        self.visit_children(op);
        fuse_intervening_projections(op);
        let rewritten = self.try_double_eager(op) || self.try_owner_outer_count(op) || self.try_one_side(op);
        if rewritten {
            // what the rewrite put below can go further down
            self.visit_children(op);
        }
    }

    /// `LogicalOperatorVisitor::VisitOperator`.
    fn visit_children(&mut self, op: &mut LogicalOperator) {
        for child in &mut op.children {
            self.visit(child);
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
    }

    /// `TryPushdownAggregate`.
    fn try_one_side(&mut self, op: &mut LogicalOperator) -> bool {
        if !pushdown_operators(op) {
            return false;
        }
        let join = &op.children[0];
        let sides = side_tables(join);
        let K::Aggregate { groups, aggregates, .. } = &op.kind else {
            unreachable!()
        };
        let Some(aggregate_side) = find_aggregate_side(aggregates, &sides) else {
            return false;
        };
        let dimension_side = 1 - aggregate_side;
        let K::Join {
            join_type, conditions, ..
        } = &join.kind
        else {
            unreachable!()
        };
        // `JoinPreservesAggregateSide`
        let preserved = match join_type {
            JoinType::Inner => true,
            JoinType::Left => aggregate_side == 0,
            JoinType::Right => aggregate_side == 1,
            _ => false,
        };
        if !preserved
            || contains_aggregate(&join.children[aggregate_side])
            || sides[dimension_side].is_empty()
            || !passes_cardinality(join, aggregate_side)
        {
            return false;
        }
        // `ValidateJoinConditions`: equalities of a column of the aggregate side
        let mut keys = Vec::new();
        for condition in conditions {
            let Some((aggregate_expr, _)) = join_side_expressions(condition, &sides, aggregate_side) else {
                return false;
            };
            let Some(key) = column_of(aggregate_expr) else {
                return false;
            };
            keys.push((key, aggregate_expr.return_type.clone()));
        }
        // `HasDimensionGroup`
        let mut dimension_groups = 0;
        for group in groups {
            if column_of(group).is_none() {
                return false;
            }
            match expression_side(group, &sides) {
                Some(side) if side == dimension_side => dimension_groups += 1,
                Some(_) => {}
                None => return false,
            }
        }
        if dimension_groups == 0 {
            return false;
        }
        let lower_group_index = self.fresh();
        let lower_aggregate_index = self.fresh();
        let (lower_groups, lower_map, key_count) =
            collect_lower_groups(lower_group_index, &keys, groups, aggregate_side, &sides);
        // `PassesLowerGroupHeuristic`
        if lower_groups.len() >= groups.len() || lower_groups.len() > key_count + MAX_EXTRA_LOWER_GROUPS {
            return false;
        }
        // `BindPushdownAggregates`
        let Some(combine) = BuiltinFunctions::get().aggregate("combine_aggr") else {
            return false;
        };
        let mut lower_aggregates = Vec::new();
        let mut upper_aggregates = Vec::new();
        let mut finalized_types = Vec::new();
        for (i, aggregate) in aggregates.iter().enumerate() {
            let Some(lower) = exported(aggregate) else {
                return false;
            };
            let state = BoundExpr::column_ref(
                ColumnBinding::new(lower_aggregate_index, i),
                lower.return_type.clone(),
                "",
            );
            let Ok(upper) = bind_aggregate_function(combine, vec![state], None, false) else {
                return false;
            };
            if !upper.return_type.is_aggregate_state() {
                return false;
            }
            // the state does not come back to the type of the aggregate
            if finalized_type(&upper.return_type).as_ref() != Some(&aggregate.return_type) {
                return false;
            }
            finalized_types.push(None);
            lower_aggregates.push(lower);
            upper_aggregates.push(upper);
        }
        // from here on the rewrite does not fail
        let upper_groups: Vec<BoundExpr> = groups
            .iter()
            .map(|group| match column_of(group).and_then(|b| lower_map.get(&b)) {
                Some(binding) => BoundExpr::column_ref(*binding, group.return_type.clone(), group.alias.clone()),
                None => group.clone(),
            })
            .collect();
        let (groupings_index, grouping_sets) = (self.fresh(), grouping_sets_of(op));
        let mut join = op.children.pop().expect("checked above");
        let K::Join { conditions, .. } = &mut join.kind else {
            unreachable!()
        };
        // `CreateJoin`: the keys of the aggregate side are the groups of the lower aggregate now
        for condition in conditions.iter_mut() {
            let key = if aggregate_side == 0 {
                &mut condition.left
            } else {
                &mut condition.right
            };
            let binding = lower_map[&column_of(key).expect("checked above")];
            *key = BoundExpr::column_ref(binding, key.return_type.clone(), key.alias.clone());
        }
        let input = std::mem::replace(&mut join.children[aggregate_side], placeholder());
        join.children[aggregate_side] = aggregate_operator(
            lower_group_index,
            lower_aggregate_index,
            groupings_index,
            lower_groups,
            lower_aggregates,
            Vec::new(),
            input,
        );
        let (upper_group_index, upper_aggregate_index, upper_groupings_index) =
            (self.fresh(), self.fresh(), self.fresh());
        let upper = aggregate_operator(
            upper_group_index,
            upper_aggregate_index,
            upper_groupings_index,
            upper_groups,
            upper_aggregates,
            grouping_sets,
            join,
        );
        self.final_projection(op, upper, &finalized_types);
        true
    }

    /// `TryDoubleEagerPushdown`.
    fn try_double_eager(&mut self, op: &mut LogicalOperator) -> bool {
        if !pushdown_operators(op) {
            return false;
        }
        let join = &op.children[0];
        let K::Join {
            join_type: JoinType::Inner,
            conditions,
            ..
        } = &join.kind
        else {
            return false;
        };
        let K::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        if grouping_sets.len() > 1 {
            return false;
        }
        if estimate_collapse(join).is_none() {
            return false;
        }
        let sides = side_tables(join);
        // `DEClassifyAggregates`: the side whose rows an aggregate reads, the first for COUNT(*)
        let mut aggregate_sides = Vec::new();
        for aggregate in aggregates {
            if !is_supported(aggregate) || !can_repeat_state(aggregate) {
                return false;
            }
            let side = match aggregate.children.first() {
                Some(child) => match expression_side(child, &sides) {
                    Some(side) => side,
                    None => return false,
                },
                None => 0,
            };
            aggregate_sides.push(side);
        }
        // `DECollectJoinKeys`
        let mut keys: [Vec<(ColumnBinding, LogicalType)>; 2] = [Vec::new(), Vec::new()];
        for condition in conditions {
            if condition.comparison != ComparisonOp::Equal {
                return false;
            }
            let (Some(left_side), Some(right_side)) = (
                expression_side(&condition.left, &sides),
                expression_side(&condition.right, &sides),
            ) else {
                return false;
            };
            if left_side == right_side {
                return false;
            }
            let (first, second) = if left_side == 0 {
                (&condition.left, &condition.right)
            } else {
                (&condition.right, &condition.left)
            };
            let (Some(first_key), Some(second_key)) = (column_of(first), column_of(second)) else {
                return false;
            };
            keys[0].push((first_key, first.return_type.clone()));
            keys[1].push((second_key, second.return_type.clone()));
        }
        // `DEValidateGroups`
        if !groups
            .iter()
            .all(|group| column_of(group).is_some() && expression_side(group, &sides).is_some())
        {
            return false;
        }
        // `DEBuildLowerSides`: a COUNT(*) first on each side, then the states of its aggregates
        let Some(count_star) = BuiltinFunctions::get()
            .aggregate("count_star")
            .and_then(|set| bind_aggregate_function(set, Vec::new(), None, false).ok())
        else {
            return false;
        };
        let mut lower: Vec<LowerSide> = Vec::with_capacity(2);
        for (side, side_keys) in keys.iter().enumerate() {
            let group_index = self.fresh();
            let aggregate_index = self.fresh();
            let (groups, map, _) = collect_lower_groups(group_index, side_keys, groups, side, &sides);
            lower.push(LowerSide {
                group_index,
                aggregate_index,
                groups,
                map,
                aggregates: vec![count_star.clone()],
            });
        }
        let mut partial_positions = Vec::new();
        for (aggregate, &side) in aggregates.iter().zip(&aggregate_sides) {
            let Some(partial) = exported(aggregate) else {
                return false;
            };
            partial_positions.push(lower[side].aggregates.len());
            lower[side].aggregates.push(partial);
        }
        // `DECreateUpperAggregates`: a state counts as often as the other side has rows
        let Some(combine) = BuiltinFunctions::get().aggregate("combine_aggr") else {
            return false;
        };
        let mut upper_aggregates = Vec::new();
        let mut finalized_types = Vec::new();
        for ((aggregate, &side), &position) in aggregates.iter().zip(&aggregate_sides).zip(&partial_positions) {
            let partial = &lower[side].aggregates[position];
            let state = BoundExpr::column_ref(
                ColumnBinding::new(lower[side].aggregate_index, position),
                partial.return_type.clone(),
                "",
            );
            let other = &lower[1 - side];
            let count = BoundExpr::column_ref(
                ColumnBinding::new(other.aggregate_index, 0),
                count_star.return_type.clone(),
                "",
            );
            let Ok(upper) = bind_aggregate_function(combine, vec![state, count], None, false) else {
                return false;
            };
            if !upper.return_type.is_aggregate_state() {
                return false;
            }
            let Some(finalized) = finalized_type(&upper.return_type) else {
                return false;
            };
            finalized_types.push((finalized != aggregate.return_type).then(|| aggregate.return_type.clone()));
            upper_aggregates.push(upper);
        }
        // from here on the rewrite does not fail
        let upper_groups: Vec<BoundExpr> = groups
            .iter()
            .map(|group| {
                let side = expression_side(group, &sides).expect("checked above");
                let binding = lower[side].map[&column_of(group).expect("checked above")];
                BoundExpr::column_ref(binding, group.return_type.clone(), group.alias.clone())
            })
            .collect();
        let grouping_sets = grouping_sets_of(op);
        let mut join = op.children.pop().expect("checked above");
        let mut lower_operators = Vec::with_capacity(2);
        for (side, input) in join.children.drain(..).enumerate() {
            let side_info = std::mem::take(&mut lower[side]);
            let groupings_index = self.fresh();
            lower_operators.push(aggregate_operator(
                side_info.group_index,
                side_info.aggregate_index,
                groupings_index,
                side_info.groups,
                side_info.aggregates,
                Vec::new(),
                input,
            ));
        }
        let conditions = keys[0]
            .iter()
            .zip(&keys[1])
            .map(|((left, left_type), (right, right_type))| JoinCondition {
                bound: None,
                left: BoundExpr::column_ref(lower_binding(&lower_operators[0], *left), left_type.clone(), ""),
                right: BoundExpr::column_ref(lower_binding(&lower_operators[1], *right), right_type.clone(), ""),
                comparison: ComparisonOp::Equal,
                filter: None,
            })
            .collect();
        let new_join = LogicalOperator::new(
            K::Join {
                join_type: JoinType::Inner,
                conditions,
                predicate: None,
                algorithm: Default::default(),
            },
            lower_operators,
        );
        let (upper_group_index, upper_aggregate_index, upper_groupings_index) =
            (self.fresh(), self.fresh(), self.fresh());
        let upper = aggregate_operator(
            upper_group_index,
            upper_aggregate_index,
            upper_groupings_index,
            upper_groups,
            upper_aggregates,
            grouping_sets,
            new_join,
        );
        self.final_projection(op, upper, &finalized_types);
        true
    }

    /// `TryOwnerOuterCountPushdown`.
    fn try_owner_outer_count(&mut self, op: &mut LogicalOperator) -> bool {
        if !pushdown_operators(op) {
            return false;
        }
        let K::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let join = &op.children[0];
        let K::Join {
            join_type,
            conditions,
            predicate: None,
            ..
        } = &join.kind
        else {
            return false;
        };
        let dimension_side = match join_type {
            JoinType::Left => 0,
            JoinType::Right => 1,
            _ => return false,
        };
        if conditions.len() != 1
            || groups.len() != 1
            || aggregates.len() != 1
            || grouping_sets.len() > 1
            || grouping_sets.first().is_some_and(|set| !set.contains(&0))
        {
            return false;
        }
        let aggregate_side = 1 - dimension_side;
        let sides = side_tables(join);
        let Some((aggregate_key_expr, owner_key_expr)) = join_side_expressions(&conditions[0], &sides, aggregate_side)
        else {
            return false;
        };
        let (Some(owner_key), Some(group_key), Some(aggregate_key)) = (
            column_of(owner_key_expr),
            column_of(&groups[0]),
            column_of(aggregate_key_expr),
        ) else {
            return false;
        };
        if group_key != owner_key {
            return false;
        }
        let aggregate = &aggregates[0];
        let count_star = aggregate.function.name == "count_star";
        if (!count_star && aggregate.function.name != "count")
            || aggregate.distinct
            || aggregate.filter.is_some()
            || !aggregate.order_by.is_empty()
            || aggregate.children.len() != usize::from(!count_star)
            || aggregate.function.export.is_some()
        {
            return false;
        }
        if !count_star {
            let Some(argument) = column_of(&aggregate.children[0]) else {
                return false;
            };
            if !sides[aggregate_side].contains(&argument.table_index) {
                return false;
            }
        }
        let stats = relation_stats(&join.children[aggregate_side]);
        let key_stats = stats.column(&aggregate_key);
        if key_stats.total.source != CountSource::Hll {
            return false;
        }
        let cardinality = stats.cardinality.max(0.0).floor();
        let key_count = key_stats.total.count.floor().min(cardinality);
        if cardinality == 0.0
            || key_count == 0.0
            || cardinality < MIN_AGGREGATE_TO_DIMENSION_RATIO * key_count
            || (estimated_cardinality(join) as f64) < MIN_JOIN_RETENTION * cardinality
        {
            return false;
        }
        let count_type = aggregate.return_type.clone();
        let value = BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::Coalesce,
                children: vec![
                    BoundExpr::column_ref(ColumnBinding::new(0, 0), count_type.clone(), ""),
                    BoundExpr::constant(Value::bigint(i64::from(count_star))),
                ],
            },
            count_type.clone(),
        );
        let Some(sum) = BuiltinFunctions::get()
            .aggregate("sum")
            .and_then(|set| bind_aggregate_function(set, vec![value], None, false).ok())
        else {
            return false;
        };
        // from here on the rewrite does not fail
        let aggregate = aggregate.clone();
        let groups = groups.clone();
        let grouping_sets = grouping_sets_of(op);
        let (lower_group_index, lower_aggregate_index, lower_groupings_index) =
            (self.fresh(), self.fresh(), self.fresh());
        let mut join = op.children.pop().expect("checked above");
        let K::Join { conditions, .. } = &mut join.kind else {
            unreachable!()
        };
        let key = if aggregate_side == 0 {
            &mut conditions[0].left
        } else {
            &mut conditions[0].right
        };
        let lower_group = std::mem::replace(
            key,
            BoundExpr::column_ref(
                ColumnBinding::new(lower_group_index, 0),
                key.return_type.clone(),
                key.alias.clone(),
            ),
        );
        let input = std::mem::replace(&mut join.children[aggregate_side], placeholder());
        join.children[aggregate_side] = aggregate_operator(
            lower_group_index,
            lower_aggregate_index,
            lower_groupings_index,
            vec![lower_group],
            vec![aggregate],
            Vec::new(),
            input,
        );
        // the sum reads the count of the lower aggregate
        let mut sum = sum;
        sum.children[0].walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind {
                *binding = ColumnBinding::new(lower_aggregate_index, 0);
            }
        });
        let (upper_group_index, upper_aggregate_index, upper_groupings_index) =
            (self.fresh(), self.fresh(), self.fresh());
        let sum_type = sum.return_type.clone();
        let upper = aggregate_operator(
            upper_group_index,
            upper_aggregate_index,
            upper_groupings_index,
            groups.clone(),
            vec![sum],
            grouping_sets,
            join,
        );
        let projection_index = self.fresh();
        let K::Aggregate {
            group_index,
            aggregate_index,
            ..
        } = op.kind
        else {
            unreachable!()
        };
        self.replacements.insert(
            ColumnBinding::new(group_index, 0),
            ColumnBinding::new(projection_index, 0),
        );
        self.replacements.insert(
            ColumnBinding::new(aggregate_index, 0),
            ColumnBinding::new(projection_index, 1),
        );
        let expressions = vec![
            BoundExpr::column_ref(
                ColumnBinding::new(upper_group_index, 0),
                groups[0].return_type.clone(),
                groups[0].alias.clone(),
            ),
            BoundExpr::column_ref(ColumnBinding::new(upper_aggregate_index, 0), sum_type, "")
                .cast_to(&count_type, false),
        ];
        *op = LogicalOperator::new(
            K::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![upper],
        );
        true
    }

    /// `BuildFinalProjection`: the groups, then the finalized states, cast to `cast` where given.
    fn final_projection(&mut self, op: &mut LogicalOperator, upper: LogicalOperator, cast: &[Option<LogicalType>]) {
        let K::Aggregate {
            group_index,
            aggregate_index,
            groups,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let (group_index, aggregate_index) = (*group_index, *aggregate_index);
        let K::Aggregate {
            group_index: upper_group_index,
            aggregate_index: upper_aggregate_index,
            aggregates: upper_aggregates,
            ..
        } = &upper.kind
        else {
            unreachable!()
        };
        let projection_index = self.fresh();
        let mut expressions = Vec::new();
        for (i, group) in groups.iter().enumerate() {
            self.replacements.insert(
                ColumnBinding::new(group_index, i),
                ColumnBinding::new(projection_index, i),
            );
            expressions.push(BoundExpr::column_ref(
                ColumnBinding::new(*upper_group_index, i),
                group.return_type.clone(),
                group.alias.clone(),
            ));
        }
        let group_count = groups.len();
        for (j, upper_aggregate) in upper_aggregates.iter().enumerate() {
            let state = BoundExpr::column_ref(
                ColumnBinding::new(*upper_aggregate_index, j),
                upper_aggregate.return_type.clone(),
                "",
            );
            let mut value = crate::function::extras::bind_function("finalize", &[], vec![state])
                .expect("checked when the states were bound");
            if let Some(ty) = &cast[j] {
                value = value.cast_to(ty, false);
            }
            self.replacements.insert(
                ColumnBinding::new(aggregate_index, j),
                ColumnBinding::new(projection_index, group_count + j),
            );
            expressions.push(value);
        }
        *op = LogicalOperator::new(
            K::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![upper],
        );
    }
}

/// What a side of the double-eager rewrite aggregates (`DoubleEagerSide`).
#[derive(Default)]
struct LowerSide {
    group_index: usize,
    aggregate_index: usize,
    groups: Vec<BoundExpr>,
    map: HashMap<ColumnBinding, ColumnBinding>,
    aggregates: Vec<BoundAggregate>,
}

fn placeholder() -> LogicalOperator {
    LogicalOperator::new(K::CrossProduct, Vec::new())
}

fn grouping_sets_of(op: &LogicalOperator) -> Vec<std::collections::BTreeSet<usize>> {
    match &op.kind {
        K::Aggregate { grouping_sets, .. } => grouping_sets.clone(),
        _ => Vec::new(),
    }
}

fn aggregate_operator(
    group_index: usize,
    aggregate_index: usize,
    groupings_index: usize,
    groups: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    grouping_sets: Vec<std::collections::BTreeSet<usize>>,
    child: LogicalOperator,
) -> LogicalOperator {
    LogicalOperator::new(
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index,
            groups,
            aggregates,
            grouping_sets,
            grouping_functions: Vec::new(),
            perfect_hash: None,
            domain_cap: None,
        },
        vec![child],
    )
}

/// The binding of the lower aggregate `op` that groups by `original`.
fn lower_binding(op: &LogicalOperator, original: ColumnBinding) -> ColumnBinding {
    let K::Aggregate {
        group_index, groups, ..
    } = &op.kind
    else {
        unreachable!()
    };
    let at = groups
        .iter()
        .position(|group| column_of(group) == Some(original))
        .expect("a join key is a group");
    ColumnBinding::new(*group_index, at)
}

/// `GetPushdownOperators`: an aggregate with groups and aggregates over a join on equalities.
fn pushdown_operators(op: &LogicalOperator) -> bool {
    let K::Aggregate {
        groups,
        aggregates,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return false;
    };
    if op.children.len() != 1 || !grouping_functions.is_empty() || groups.is_empty() || aggregates.is_empty() {
        return false;
    }
    let join = &op.children[0];
    let K::Join {
        join_type,
        conditions,
        predicate,
        ..
    } = &join.kind
    else {
        return false;
    };
    matches!(join_type, JoinType::Inner | JoinType::Left | JoinType::Right)
        && predicate.is_none()
        && !conditions.is_empty()
        && join.children.len() == 2
        && !contains_delim_get(join)
}

fn contains_delim_get(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::DelimGet { .. }) || op.children.iter().any(contains_delim_get)
}

fn contains_aggregate(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::Aggregate { .. }) || op.children.iter().any(contains_aggregate)
}

/// `LogicalJoin::GetTableReferences` of both sides.
fn side_tables(join: &LogicalOperator) -> Sides {
    let tables = |op: &LogicalOperator| op.column_bindings().iter().map(|b| b.table_index).collect();
    [tables(&join.children[0]), tables(&join.children[1])]
}

fn expression_tables(expr: &BoundExpr, tables: &mut HashSet<usize>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        tables.insert(binding.table_index);
    }
    for child in expr.children() {
        expression_tables(child, tables);
    }
}

/// `GetExpressionSide`: the side all the columns of `expr` come from.
fn expression_side(expr: &BoundExpr, sides: &Sides) -> Option<usize> {
    let mut tables = HashSet::new();
    expression_tables(expr, &mut tables);
    if tables.is_empty() {
        return None;
    }
    (0..2).find(|&side| tables.is_subset(&sides[side]))
}

fn column_of(expr: &BoundExpr) -> Option<ColumnBinding> {
    match expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(binding),
        _ => None,
    }
}

/// `GetJoinSideExpressions`: of an equality, the expression of the aggregate side and the other.
fn join_side_expressions<'a>(
    condition: &'a JoinCondition,
    sides: &Sides,
    aggregate_side: usize,
) -> Option<(&'a BoundExpr, &'a BoundExpr)> {
    if condition.comparison != ComparisonOp::Equal {
        return None;
    }
    let mut left = HashSet::new();
    let mut right = HashSet::new();
    expression_tables(&condition.left, &mut left);
    expression_tables(&condition.right, &mut right);
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let dimension_side = 1 - aggregate_side;
    if left.is_subset(&sides[aggregate_side]) && right.is_subset(&sides[dimension_side]) {
        return Some((&condition.left, &condition.right));
    }
    if right.is_subset(&sides[aggregate_side]) && left.is_subset(&sides[dimension_side]) {
        return Some((&condition.right, &condition.left));
    }
    None
}

/// `CollectLowerSideGroups`: the join keys, then the groups of the query on `side`. Gives the
/// groups, where each column is among them, and how many are join keys.
fn collect_lower_groups(
    group_index: usize,
    keys: &[(ColumnBinding, LogicalType)],
    groups: &[BoundExpr],
    side: usize,
    sides: &Sides,
) -> (Vec<BoundExpr>, HashMap<ColumnBinding, ColumnBinding>, usize) {
    let mut out = Vec::new();
    let mut map = HashMap::new();
    let mut add =
        |map: &mut HashMap<ColumnBinding, ColumnBinding>, binding: ColumnBinding, ty: &LogicalType, alias: &str| {
            if map.contains_key(&binding) {
                return;
            }
            map.insert(binding, ColumnBinding::new(group_index, out.len()));
            out.push(BoundExpr::column_ref(binding, ty.clone(), alias));
        };
    for (key, ty) in keys {
        add(&mut map, *key, ty, "");
    }
    let key_count = map.len();
    for group in groups {
        if let Some(binding) = column_of(group)
            && expression_side(group, sides) == Some(side)
        {
            add(&mut map, binding, &group.return_type, &group.alias);
        }
    }
    (out, map, key_count)
}

fn is_decimal_average_state(ty: &LogicalType) -> bool {
    ty.is_aggregate_state() && resolve_state(ty).is_ok_and(|(function, _)| function.kind == AggregateKind::AvgDecimal)
}

/// `IsSupportedAggregate`.
fn is_supported(aggregate: &BoundAggregate) -> bool {
    !aggregate.distinct
        && aggregate.filter.is_none()
        && aggregate.order_by.is_empty()
        && aggregate.function.kind != AggregateKind::AvgDecimal
        && !aggregate.children.iter().any(|c| is_decimal_average_state(&c.return_type))
        && aggregate.function.export.is_none()
        // it has no state of its own (`HasGetStateTypeCallback`)
        && aggregate.function.name != "combine_aggr"
        && aggregate.children.len() <= 1
        && exported(aggregate).is_some()
}

/// `ExportAggregateFunction::Bind`: the aggregate giving its state.
fn exported(aggregate: &BoundAggregate) -> Option<BoundAggregate> {
    use crate::function::holistic::Holistic;
    // their constant parameters are packed into the argument here, which a state bound without them
    // does not lay out
    if let AggregateKind::Holistic(
        Holistic::StringAgg | Holistic::QuantileCont | Holistic::QuantileDisc | Holistic::ApproxQuantile,
    ) = aggregate.function.kind
    {
        return None;
    }
    let types: Vec<LogicalType> = aggregate.children.iter().map(|c| c.return_type.clone()).collect();
    let (function, ty) = export_state(&aggregate.function, &aggregate.return_type, &types, Vec::new()).ok()?;
    if !ty.is_aggregate_state() {
        return None;
    }
    let mut copy = aggregate.clone();
    copy.function = Arc::new(function);
    copy.return_type = ty;
    Some(copy)
}

fn finalized_type(state: &LogicalType) -> Option<LogicalType> {
    resolve_state(state).ok().map(|(_, ty)| ty)
}

/// `DECanRepeatAggregateState`: a sum of 128-bit integers repeated could overflow where the
/// rows themselves do not.
fn can_repeat_state(aggregate: &BoundAggregate) -> bool {
    let name = aggregate.function.name.as_str();
    if (name != "sum" && name != "avg") || aggregate.children.is_empty() {
        return true;
    }
    aggregate.children[0].return_type.physical_type() != PhysicalType::Int128
}

/// `FindAggregateSide`: the one side all the aggregates read.
fn find_aggregate_side(aggregates: &[BoundAggregate], sides: &Sides) -> Option<usize> {
    let mut found = None;
    for aggregate in aggregates {
        if !is_supported(aggregate) {
            return None;
        }
        let Some(child) = aggregate.children.first() else {
            continue;
        };
        let side = expression_side(child, sides)?;
        match found {
            None => found = Some(side),
            Some(other) if other != side => return None,
            Some(_) => {}
        }
    }
    found
}

/// `PassesCardinalityHeuristic`.
fn passes_cardinality(join: &LogicalOperator, aggregate_side: usize) -> bool {
    let aggregate_rows = estimated_cardinality(&join.children[aggregate_side]) as f64;
    let join_rows = estimated_cardinality(join) as f64;
    if join_rows > aggregate_rows {
        return true;
    }
    if join_rows < MIN_JOIN_RETENTION * aggregate_rows {
        return false;
    }
    let dimension_rows = estimated_cardinality(&join.children[1 - aggregate_side]) as f64;
    aggregate_rows >= MIN_AGGREGATE_TO_DIMENSION_RATIO * dimension_rows
}

/// `DEEstimateCollapse`: the distinct keys the join sees, if both sides have enough rows per key.
fn estimate_collapse(join: &LogicalOperator) -> Option<u64> {
    let left = estimated_cardinality(&join.children[0]).max(1) as f64;
    let right = estimated_cardinality(&join.children[1]).max(1) as f64;
    let joined = estimated_cardinality(join).max(1) as f64;
    let ndv = (left * right / joined).max(1.0).min(left.min(right));
    let effective = (ndv as u64).max(1);
    let min_collapse = if effective <= MAX_SMALL_GROUP_COUNT {
        MIN_SMALL_COLLAPSE
    } else {
        MIN_LARGE_COLLAPSE
    };
    (left >= min_collapse * ndv && right >= min_collapse * ndv).then_some(effective)
}

/// `FuseInterveningProjections`: an aggregate over projections over a join reads the join.
fn fuse_intervening_projections(op: &mut LogicalOperator) {
    if !matches!(op.kind, K::Aggregate { .. }) || op.children.len() != 1 {
        return;
    }
    let mut projections: Vec<(usize, Vec<BoundExpr>)> = Vec::new();
    let mut current = &op.children[0];
    while let K::Projection {
        table_index,
        expressions,
    } = &current.kind
        && current.children.len() == 1
    {
        projections.push((*table_index, expressions.clone()));
        current = &current.children[0];
    }
    if projections.is_empty() || !matches!(&current.kind, K::Join { conditions, .. } if !conditions.is_empty()) {
        return;
    }
    // inlining would run a volatile expression more often
    if projections
        .iter()
        .any(|(_, exprs)| exprs.iter().any(BoundExpr::is_volatile))
    {
        return;
    }
    let inline = |expr: &mut BoundExpr| {
        for (table_index, expressions) in &projections {
            inline_projection(expr, *table_index, expressions);
        }
    };
    let K::Aggregate { groups, aggregates, .. } = &mut op.kind else {
        unreachable!()
    };
    for group in groups.iter_mut() {
        inline(group);
    }
    for aggregate in aggregates.iter_mut() {
        for child in &mut aggregate.children {
            inline(child);
        }
        if let Some(filter) = &mut aggregate.filter {
            inline(filter);
        }
        for (expr, _) in &mut aggregate.order_by {
            inline(expr);
        }
        if let Some(key) = &mut aggregate.distinct_key {
            inline(key);
        }
    }
    let mut below = op.children.pop().expect("checked above");
    for _ in 0..projections.len() {
        below = below.children.pop().expect("a projection has a child");
    }
    op.children.push(below);
}

/// `DEInlineProjection`.
fn inline_projection(expr: &mut BoundExpr, table_index: usize, expressions: &[BoundExpr]) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind
        && binding.table_index == table_index
    {
        *expr = expressions[binding.column_index].clone();
        return;
    }
    for child in expr.children_mut() {
        inline_projection(child, table_index, expressions);
    }
}
