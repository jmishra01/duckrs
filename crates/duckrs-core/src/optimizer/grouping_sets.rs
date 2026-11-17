// Ported from duckdb/src/optimizer/grouping_sets_optimizer.cpp @ rust-port-baseline
//
//! `GroupingSetsOptimizer`: an aggregate over several grouping sets (ROLLUP, CUBE, GROUPING SETS)
//! becomes a UNION ALL of one aggregate per set. When its aggregates can combine their states,
//! the sets cascade: the finest set aggregates the input and exports the states, each coarser set
//! combines the states of the smallest finer set computed before it, from a materialized CTE.
//! Otherwise each set aggregates the input again, from a CTE of it.
//!
//! Not ported: staging volatile aggregate inputs into the CTE (`StageVolatileAggregateInputs`); an
//! aggregate with volatile groups or arguments is left as it is.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// `MAX_REWRITE_GROUPING_SETS`.
const MAX_REWRITE_GROUPING_SETS: usize = 1024;

type Replacements = HashMap<ColumnBinding, ColumnBinding>;

/// Runs the pass over a plan.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    visit(&mut plan, &mut next);
    plan
}

fn fresh(next: &mut usize) -> usize {
    let index = *next;
    *next += 1;
    index
}

fn visit(op: &mut LogicalOperator, next: &mut usize) -> Replacements {
    let mut replacements = Replacements::new();
    for child in &mut op.children {
        replacements.extend(visit(child, next));
    }
    if !replacements.is_empty() {
        visit_bindings(&mut op.kind, &mut |binding, depth| {
            if depth == 0
                && let Some(new) = replacements.get(binding)
            {
                *binding = *new;
            }
        });
    }
    if let Some(moved) = try_rewrite(op, next) {
        replacements.extend(moved);
    }
    replacements
}

/// One grouping set of the cascade, in the order it is computed (`GroupingSetLevel`).
struct Level {
    set: usize,
    source: Option<usize>,
    materialized: bool,
    cte_index: usize,
    output_types: Vec<LogicalType>,
    output_names: Vec<String>,
    group_positions: HashMap<usize, usize>,
    data: Arc<Mutex<ColumnDataCollection>>,
    aggregate: Option<LogicalOperator>,
}

/// `GetGroupingValue`: a bit for each group of the GROUPING call that the set leaves out.
fn grouping_value(set: &BTreeSet<usize>, function: &[usize]) -> i64 {
    let mut value = 0i64;
    for (i, group) in function.iter().enumerate() {
        if !set.contains(group) {
            value += 1i64 << (function.len() - (i + 1));
        }
    }
    value
}

/// `CanRewriteAggregate`: whether states of finer groups combine into the result.
fn can_rewrite_aggregate(aggregate: &BoundAggregate) -> bool {
    if aggregate.distinct || !aggregate.order_by.is_empty() || aggregate.function.export.is_some() {
        return false;
    }
    super::common_subplan::can_combine_aggregate(aggregate)
}

/// `FindCascade`: the sets by size, each computed from the smallest computed superset.
fn find_cascade(sets: &[BTreeSet<usize>]) -> Option<Vec<(usize, Option<usize>, bool)>> {
    let mut order: Vec<usize> = (0..sets.len()).collect();
    order.sort_by(|a, b| sets[*b].len().cmp(&sets[*a].len()));
    let mut levels: Vec<(usize, Option<usize>, bool)> = order.into_iter().map(|set| (set, None, false)).collect();
    for at in 1..levels.len() {
        let set = &sets[levels[at].0];
        let mut found = None;
        for source in (0..at).rev() {
            let source_set = &sets[levels[source].0];
            if set.is_subset(source_set) {
                let mut source_level = source;
                // an identical set shares its source, which keeps the cascade shallow
                if source_set.len() == set.len()
                    && let Some(shared) = levels[source].1
                {
                    source_level = shared;
                }
                found = Some(source_level);
                break;
            }
        }
        let source = found?;
        levels[at].1 = Some(source);
        levels[source].2 = true;
    }
    Some(levels)
}

fn column(binding: ColumnBinding, ty: &LogicalType) -> BoundExpr {
    BoundExpr::column_ref(binding, ty.clone(), "")
}

fn rebind(expr: &mut BoundExpr, map: &Replacements) {
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}

fn rebind_aggregate(aggregate: &mut BoundAggregate, map: &Replacements) {
    for child in &mut aggregate.children {
        rebind(child, map);
    }
    if let Some(filter) = &mut aggregate.filter {
        rebind(filter, map);
    }
    for (expr, _) in &mut aggregate.order_by {
        rebind(expr, map);
    }
    if let Some(key) = &mut aggregate.distinct_key {
        rebind(key, map);
    }
}

fn aggregate_operator(
    group_index: usize,
    aggregate_index: usize,
    groupings_index: usize,
    groups: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    child: LogicalOperator,
) -> LogicalOperator {
    LogicalOperator::new(
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index,
            groups,
            aggregates,
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
            perfect_hash: false,
            domain_cap: None,
        },
        vec![child],
    )
}

/// `LogicalProjection::CreateIdentity`.
fn identity(table_index: usize, child: LogicalOperator) -> LogicalOperator {
    let expressions = child
        .column_bindings()
        .into_iter()
        .zip(child.types())
        .map(|(binding, ty)| column(binding, &ty))
        .collect();
    LogicalOperator::new(
        K::Projection {
            table_index,
            expressions,
        },
        vec![child],
    )
}

fn cte_ref(
    table_index: usize,
    cte_index: usize,
    types: &[LogicalType],
    names: &[String],
    data: &Arc<Mutex<ColumnDataCollection>>,
) -> LogicalOperator {
    LogicalOperator::new(
        K::CteRef {
            table_index,
            cte_index,
            types: types.to_vec(),
            names: names.to_vec(),
            data: data.clone(),
        },
        Vec::new(),
    )
}

/// What the operators above read the aggregate's columns as: the union's.
fn replacements_of(op: &LogicalOperator, union_index: usize) -> Replacements {
    let K::Aggregate {
        group_index,
        aggregate_index,
        groupings_index,
        groups,
        aggregates,
        grouping_functions,
        ..
    } = &op.kind
    else {
        unreachable!()
    };
    let (group_count, aggregate_count) = (groups.len(), aggregates.len());
    let mut map = Replacements::new();
    for at in 0..group_count {
        map.insert(ColumnBinding::new(*group_index, at), ColumnBinding::new(union_index, at));
    }
    for at in 0..aggregate_count {
        map.insert(
            ColumnBinding::new(*aggregate_index, at),
            ColumnBinding::new(union_index, group_count + at),
        );
    }
    for at in 0..grouping_functions.len() {
        map.insert(
            ColumnBinding::new(*groupings_index, at),
            ColumnBinding::new(union_index, group_count + aggregate_count + at),
        );
    }
    map
}

fn union_types(op: &LogicalOperator) -> Vec<LogicalType> {
    let K::Aggregate {
        groups,
        aggregates,
        grouping_functions,
        ..
    } = &op.kind
    else {
        unreachable!()
    };
    groups
        .iter()
        .map(|g| g.return_type.clone())
        .chain(aggregates.iter().map(|a| a.return_type.clone()))
        .chain(grouping_functions.iter().map(|_| LogicalType::BIGINT))
        .collect()
}

/// `TryRewriteGroupingSets`.
fn try_rewrite(op: &mut LogicalOperator, next: &mut usize) -> Option<Replacements> {
    let K::Aggregate {
        groups,
        aggregates,
        grouping_sets,
        ..
    } = &op.kind
    else {
        return None;
    };
    if op.children.len() != 1 || grouping_sets.len() < 2 || aggregates.is_empty() {
        return None;
    }
    if grouping_sets.len() > MAX_REWRITE_GROUPING_SETS {
        return None;
    }
    let volatile = groups.iter().any(BoundExpr::is_volatile)
        || aggregates.iter().any(|a| {
            a.children.iter().any(BoundExpr::is_volatile) || a.filter.as_ref().is_some_and(BoundExpr::is_volatile)
        });
    if volatile {
        return None;
    }
    let can_cascade = aggregates.iter().all(can_rewrite_aggregate);
    let cascade = if can_cascade { find_cascade(grouping_sets) } else { None };
    match cascade {
        Some(levels) => cascade_rewrite(op, levels, next).or_else(|| expand(op, next)),
        None => expand(op, next),
    }
}

/// `TryExpandGroupingSets`: one aggregate of the input per set, from a CTE of the input.
fn expand(op: &mut LogicalOperator, next: &mut usize) -> Option<Replacements> {
    let K::Aggregate {
        groups,
        aggregates,
        grouping_sets,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return None;
    };
    let input = &op.children[0];
    let input_types = input.types();
    let input_bindings = input.column_bindings();
    let input_names: Vec<String> = (0..input_types.len()).map(|i| format!("__grouping_sets_input_{i}")).collect();
    let cte_index = fresh(next);
    let data = Arc::new(Mutex::new(ColumnDataCollection::new(input_types.clone())));
    let mut branches = Vec::with_capacity(grouping_sets.len());
    for set in grouping_sets {
        let ref_index = fresh(next);
        let map: Replacements = input_bindings
            .iter()
            .enumerate()
            .map(|(i, b)| (*b, ColumnBinding::new(ref_index, i)))
            .collect();
        let child = cte_ref(ref_index, cte_index, &input_types, &input_names, &data);
        let mut positions = HashMap::new();
        let mut branch_groups = Vec::with_capacity(set.len());
        for &group in set {
            positions.insert(group, branch_groups.len());
            let mut expr = groups[group].clone();
            rebind(&mut expr, &map);
            branch_groups.push(expr);
        }
        let branch_aggregates: Vec<BoundAggregate> = aggregates
            .iter()
            .map(|a| {
                let mut a = a.clone();
                rebind_aggregate(&mut a, &map);
                a
            })
            .collect();
        let (group_index, aggregate_index) = (fresh(next), fresh(next));
        let groupings_index = fresh(next);
        let aggregate = aggregate_operator(
            group_index,
            aggregate_index,
            groupings_index,
            branch_groups,
            branch_aggregates,
            child,
        );
        let mut expressions = Vec::new();
        for (at, group) in groups.iter().enumerate() {
            expressions.push(match positions.get(&at) {
                Some(&position) => column(ColumnBinding::new(group_index, position), &group.return_type),
                None => BoundExpr::constant(Value::null(group.return_type.clone())),
            });
        }
        for (at, aggregate) in aggregates.iter().enumerate() {
            expressions.push(column(ColumnBinding::new(aggregate_index, at), &aggregate.return_type));
        }
        for function in grouping_functions {
            expressions.push(BoundExpr::constant(Value::bigint(grouping_value(set, function))));
        }
        branches.push(LogicalOperator::new(
            K::Projection {
                table_index: fresh(next),
                expressions,
            },
            vec![aggregate],
        ));
    }
    let union_index = fresh(next);
    let types = union_types(op);
    let replacements = replacements_of(op, union_index);
    let union = LogicalOperator::new(
        K::Union {
            table_index: union_index,
            types,
        },
        branches,
    );
    let input = op.children.pop().expect("an aggregate has a child");
    let definition = identity(fresh(next), input);
    *op = LogicalOperator::new(
        K::MaterializedCte {
            name: format!("__grouping_sets_input_cte_{cte_index}"),
            table_index: cte_index,
            data,
            always: false,
        },
        vec![definition, union],
    );
    Some(replacements)
}

/// The cascade of `levels`: what combines the states of each, and the union of their results.
fn cascade_rewrite(
    op: &mut LogicalOperator,
    order: Vec<(usize, Option<usize>, bool)>,
    next: &mut usize,
) -> Option<Replacements> {
    let K::Aggregate {
        groups,
        aggregates,
        grouping_sets,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return None;
    };
    let combine = BuiltinFunctions::get().aggregate("combine_aggr")?;
    let mut levels: Vec<Level> = order
        .into_iter()
        .map(|(set, source, materialized)| Level {
            set,
            source,
            materialized,
            cte_index: 0,
            output_types: Vec::new(),
            output_names: Vec::new(),
            group_positions: HashMap::new(),
            data: Arc::new(Mutex::new(ColumnDataCollection::new(Vec::new()))),
            aggregate: None,
        })
        .collect();
    let mut state_types: Vec<LogicalType> = Vec::new();
    let mut finest_parts: Option<(usize, usize, usize, Vec<BoundExpr>, Vec<BoundAggregate>)> = None;
    for at in 0..levels.len() {
        let set = &grouping_sets[levels[at].set];
        let (group_index, aggregate_index, groupings_index) = (fresh(next), fresh(next), fresh(next));
        if at == 0 {
            // the finest set aggregates the input and exports the states
            let level_groups: Vec<BoundExpr> = set.iter().map(|&g| groups[g].clone()).collect();
            let mut exported = Vec::with_capacity(aggregates.len());
            for aggregate in aggregates {
                let types: Vec<LogicalType> = aggregate.children.iter().map(|c| c.return_type.clone()).collect();
                let (function, ty) = crate::function::aggregates::export_state(
                    &aggregate.function,
                    &aggregate.return_type,
                    &types,
                    Vec::new(),
                )
                .ok()?;
                if !ty.is_aggregate_state() {
                    return None;
                }
                state_types.push(ty.clone());
                let mut copy = aggregate.clone();
                copy.function = Arc::new(function);
                copy.return_type = ty;
                exported.push(copy);
            }
            finest_parts = Some((group_index, aggregate_index, groupings_index, level_groups, exported));
        } else {
            // a coarser set combines the states of its source
            let source = &levels[levels[at].source.expect("a coarser level has a source")];
            let source_set = &grouping_sets[source.set];
            let ref_index = fresh(next);
            let child = cte_ref(ref_index, source.cte_index, &source.output_types, &source.output_names, &source.data);
            let level_groups: Vec<BoundExpr> = set
                .iter()
                .map(|g| {
                    let position = source.group_positions[g];
                    BoundExpr::column_ref(
                        ColumnBinding::new(ref_index, position),
                        source.output_types[position].clone(),
                        source.output_names[position].clone(),
                    )
                })
                .collect();
            let mut combined = Vec::with_capacity(aggregates.len());
            for (i, ty) in state_types.iter().enumerate() {
                let state = BoundExpr::column_ref(
                    ColumnBinding::new(ref_index, source_set.len() + i),
                    ty.clone(),
                    format!("state_{i}"),
                );
                let bound = bind_aggregate_function(combine, vec![state], None, false).ok()?;
                if bound.return_type != *ty {
                    return None;
                }
                combined.push(bound);
            }
            levels[at].aggregate = Some(aggregate_operator(
                group_index,
                aggregate_index,
                groupings_index,
                level_groups,
                combined,
                child,
            ));
        }
        // the groups of the set, then the states
        let level = &mut levels[at];
        for &g in set {
            level.group_positions.insert(g, level.output_types.len());
            level.output_types.push(groups[g].return_type.clone());
            level.output_names.push(format!("group_{g}"));
        }
        for (i, ty) in state_types.iter().enumerate() {
            level.output_types.push(ty.clone());
            level.output_names.push(format!("state_{i}"));
        }
        if level.materialized {
            level.cte_index = fresh(next);
            level.data = Arc::new(Mutex::new(ColumnDataCollection::new(level.output_types.clone())));
        }
    }

    // the branches, in the order of the sets: from a CTE of the level, or its aggregate inlined
    let finalize_type = |state: &LogicalType| crate::function::aggregates::resolve_state(state).ok().map(|(_, ty)| ty);
    for (i, aggregate) in aggregates.iter().enumerate() {
        if finalize_type(&state_types[i]).as_ref() != Some(&aggregate.return_type) {
            return None;
        }
    }
    // from here on the rewrite does not fail
    let input = op.children.pop().expect("an aggregate has a child");
    let (group_index, aggregate_index, groupings_index, level_groups, exported) =
        finest_parts.expect("the finest level");
    levels[0].aggregate = Some(aggregate_operator(
        group_index,
        aggregate_index,
        groupings_index,
        level_groups,
        exported,
        input,
    ));
    let mut branches: Vec<Option<LogicalOperator>> = (0..levels.len()).map(|_| None).collect();
    let mut definitions: Vec<(usize, LogicalOperator)> = Vec::new();
    for at in 0..levels.len() {
        let set = &grouping_sets[levels[at].set];
        let level_aggregate = levels[at].aggregate.take().expect("built above");
        let (child, binding_of): (LogicalOperator, Box<dyn Fn(usize) -> ColumnBinding>) = if levels[at].materialized {
            let ref_index = fresh(next);
            definitions.push((at, level_aggregate));
            let level = &levels[at];
            (
                cte_ref(ref_index, level.cte_index, &level.output_types, &level.output_names, &level.data),
                Box::new(move |position| ColumnBinding::new(ref_index, position)),
            )
        } else {
            let K::Aggregate {
                group_index,
                aggregate_index,
                ..
            } = level_aggregate.kind
            else {
                unreachable!()
            };
            let group_count = set.len();
            (
                level_aggregate,
                Box::new(move |position| {
                    if position < group_count {
                        ColumnBinding::new(group_index, position)
                    } else {
                        ColumnBinding::new(aggregate_index, position - group_count)
                    }
                }),
            )
        };
        let level = &levels[at];
        let mut expressions = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            expressions.push(match level.group_positions.get(&g) {
                Some(&position) => {
                    BoundExpr::column_ref(binding_of(position), group.return_type.clone(), level.output_names[position].clone())
                }
                None => BoundExpr::constant(Value::null(group.return_type.clone())),
            });
        }
        for (i, state) in state_types.iter().enumerate() {
            let state_ref = BoundExpr::column_ref(binding_of(set.len() + i), state.clone(), format!("state_{i}"));
            let finalized = crate::function::extras::bind_function("finalize", &[], vec![state_ref]).ok()?;
            expressions.push(finalized);
        }
        for function in grouping_functions {
            expressions.push(BoundExpr::constant(Value::bigint(grouping_value(set, function))));
        }
        branches[level.set] = Some(LogicalOperator::new(
            K::Projection {
                table_index: fresh(next),
                expressions,
            },
            vec![child],
        ));
    }
    let union_index = fresh(next);
    let replacements = replacements_of(op, union_index);
    let mut result = LogicalOperator::new(
        K::Union {
            table_index: union_index,
            types: union_types(op),
        },
        branches.into_iter().map(|b| b.expect("a branch per set")).collect(),
    );
    // the CTEs around the union, the finest outermost, so that the coarser ones can read it
    for (at, definition) in definitions.into_iter().rev() {
        let level = &levels[at];
        result = LogicalOperator::new(
            K::MaterializedCte {
                name: format!("__grouping_sets_cte_{}", level.cte_index),
                table_index: level.cte_index,
                data: level.data.clone(),
                always: false,
            },
            vec![identity(fresh(next), definition), result],
        );
    }
    *op = result;
    Some(replacements)
}
