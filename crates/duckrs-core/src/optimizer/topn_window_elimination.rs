// Ported from duckdb/src/optimizer/topn_window_elimination.cpp @ rust-port-baseline
//
//! `TopNWindowElimination` (`TOP_N_WINDOW_ELIMINATION`): the first `k` rows of each partition by
//! `row_number()` are a grouped `arg_min`/`arg_max` of `k` values. `row_number() OVER (PARTITION BY
//! g ORDER BY v) <= 3` becomes `GROUP BY g` with `arg_min_nulls_last(payload, v, 3)`, whose lists an
//! UNNEST turns back into rows, with their positions for the row number.
//!
//! A payload of several columns of one table comes back through late materialization rather than
//! a struct: the aggregate keeps the row ids, and a join with a second scan of the table on them
//! reads the columns (`TryPrepareLateMaterialization`).
//!
//! A projection between the filter and the window must pass columns through as they are.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding, WindowFunction};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::sort_key::{NullOrder, OrderType};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// `MIN_MAX_N_MAX_VALUE`: the `n` of the min/max/arg_min/arg_max "n" aggregates stays below it.
const MIN_MAX_N_MAX_VALUE: i64 = 1_000_000;

/// `TopNWindowElimination::Optimize`. Says whether the plan changed.
pub fn optimize(plan: &mut LogicalOperator) -> bool {
    let mut next = max_table_index(plan) + 1;
    let mut replacements = Replacements::default();
    // the columns the plan reads: one that nothing reads stays out of the payload, as C++ has removed
    // it before this runs
    let mut read = HashMap::new();
    collect_reads(plan, &mut read);
    // the result's columns, which are read by position
    for binding in plan.column_bindings() {
        *read.entry(binding).or_default() += 1;
    }
    replacements.read = read;
    let changed = visit(plan, &mut next, &mut replacements);
    if !replacements.map.is_empty() {
        replace(plan, &replacements);
    }
    changed
}

/// The bindings of the filters that went, and the projections that took their place, whose plans
/// read the old bindings of the window's input (`ColumnBindingReplacer::stop_operator`).
#[derive(Default)]
struct Replacements {
    map: HashMap<ColumnBinding, ColumnBinding>,
    stops: BTreeSet<usize>,
    /// how often the plan reads each column
    read: HashMap<ColumnBinding, usize>,
}

fn collect_reads(op: &LogicalOperator, read: &mut HashMap<ColumnBinding, usize>) {
    let mut kind = op.kind.clone();
    visit_bindings(&mut kind, &mut |binding, _| {
        *read.entry(*binding).or_default() += 1;
    });
    for child in &op.children {
        collect_reads(child, read);
    }
}

fn replace(op: &mut LogicalOperator, replacements: &Replacements) {
    if let K::Projection { table_index, .. } = &op.kind
        && replacements.stops.contains(table_index)
    {
        return;
    }
    let map = &replacements.map;
    for child in &mut op.children {
        replace(child, replacements);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(to) = map.get(binding)
        {
            *binding = *to;
        }
    });
}

fn take(next: &mut usize) -> usize {
    let index = *next;
    *next += 1;
    index
}

fn column_of(expr: &BoundExpr) -> Option<ColumnBinding> {
    match expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(binding),
        _ => None,
    }
}

/// `CanOptimize`: the limit of a filter on the row number of a window, through projections. `Some(0)`
/// keeps no row.
fn can_optimize(op: &LogicalOperator) -> Option<i64> {
    let K::Filter { expressions } = &op.kind else {
        return None;
    };
    let [filter] = expressions.as_slice() else {
        return None;
    };
    let BoundExprKind::Comparison {
        op: comparison,
        left,
        right,
    } = &filter.kind
    else {
        return None;
    };
    let BoundExprKind::Constant(value) = &right.kind else {
        return None;
    };
    // C++ has the constant as a BIGINT; here it can keep a narrower integer type
    use duckrs_common::value::ValueData as V;
    let value = match *value.data() {
        V::Int8(v) => i64::from(v),
        V::Int16(v) => i64::from(v),
        V::Int32(v) => i64::from(v),
        V::Int64(v) => v,
        _ => return None,
    };
    let limit = match comparison {
        ComparisonOp::LessThanOrEqual if value < 1 => 0,
        ComparisonOp::LessThanOrEqual if value >= MIN_MAX_N_MAX_VALUE => return None,
        ComparisonOp::LessThanOrEqual => value,
        ComparisonOp::LessThan if value < 2 => 0,
        ComparisonOp::LessThan if value - 1 >= MIN_MAX_N_MAX_VALUE => return None,
        ComparisonOp::LessThan => value - 1,
        ComparisonOp::Equal if value < 1 => 0,
        ComparisonOp::Equal if value != 1 => return None,
        ComparisonOp::Equal => 1,
        _ => return None,
    };
    // the filter names the window column itself, through projections that pass it on
    let mut binding = column_of(left)?;
    let mut child = &op.children[0];
    while let K::Projection {
        table_index,
        expressions,
    } = &child.kind
    {
        if binding.table_index != *table_index {
            return None;
        }
        binding = column_of(expressions.get(binding.column_index)?)?;
        child = &child.children[0];
    }
    let K::Window { table_index, windows } = &child.kind else {
        return None;
    };
    if binding.table_index != *table_index {
        return None;
    }
    let first = windows.first()?;
    let same = |w: &crate::planner::expression::BoundWindow| format!("{w:?}") == format!("{first:?}");
    if !windows.iter().all(same) || first.function != WindowFunction::RowNumber {
        return None;
    }
    if first.partitions.iter().any(|p| column_of(p).is_none()) || !first.arg_orders.is_empty() {
        return None;
    }
    let [(_, modifiers)] = first.orders.as_slice() else {
        return None;
    };
    if modifiers.nulls != NullOrder::NullsLast {
        return None;
    }
    Some(limit)
}

fn visit(op: &mut LogicalOperator, next: &mut usize, replacements: &mut Replacements) -> bool {
    match can_optimize(op) {
        None => {
            let mut changed = false;
            for child in &mut op.children {
                changed |= visit(child, next, replacements);
            }
            changed
        }
        Some(0) => {
            *op = super::filter_pushdown::empty_result(op);
            true
        }
        Some(limit) => {
            // the window's input first
            let mut window = &mut op.children[0];
            while matches!(window.kind, K::Projection { .. }) {
                window = &mut window.children[0];
            }
            // the window's input changed bindings first: what reads them above it follows now, as
            // the rewrite copies those operators (`child_replacer`)
            let mut inner = Replacements {
                read: replacements.read.clone(),
                ..Default::default()
            };
            let below = visit(&mut window.children[0], next, &mut inner);
            if !inner.map.is_empty() {
                replace(op, &inner);
                replacements.map.extend(inner.map);
                replacements.stops.extend(inner.stops);
            }
            rewrite(op, limit, next, replacements) || below
        }
    }
}

/// The rewrite of `filter -> projections -> window` into `projection -> [unnest ->] aggregate`.
fn rewrite(
    op: &mut LogicalOperator,
    limit: i64,
    next: &mut usize,
    replacements: &mut Replacements,
) -> bool {
    let topmost_bindings = op.column_bindings();
    let topmost_types = op.types();
    // `TraverseProjectionBindings`: the bindings as the window outputs them
    let mut bindings = topmost_bindings.clone();
    let mut child = &op.children[0];
    while let K::Projection {
        table_index,
        expressions,
    } = &child.kind
    {
        for binding in &mut bindings {
            if binding.table_index != *table_index {
                return false;
            }
            match expressions.get(binding.column_index).and_then(column_of) {
                Some(below) => *binding = below,
                None => return false,
            }
        }
        child = &child.children[0];
    }
    let K::Window {
        table_index: window_index,
        windows,
    } = &child.kind
    else {
        return false;
    };
    let window_index = *window_index;
    let window = windows[0].clone();
    let mut input = child.children[0].clone();
    let input_bindings = input.column_bindings();
    let input_types = input.types();
    let partitions: Vec<ColumnBinding> = window.partitions.iter().filter_map(column_of).collect();

    // `GenerateAggregatePayload`: which topmost column is which group, and the payload columns
    let mut group_of: BTreeMap<usize, usize> = BTreeMap::new();
    let mut payload: Vec<BoundExpr> = Vec::new();
    // a column that only the filter, the window or its input read (its order, a join condition) is
    // no payload: the removal of unused columns has taken it out of the window's output in C++
    let mut inside = HashMap::new();
    collect_reads(op, &mut inside);
    let unread: BTreeSet<usize> = (0..bindings.len())
        .filter(|&i| {
            let binding = &topmost_bindings[i];
            let total = replacements.read.get(binding).copied().unwrap_or(0);
            total <= inside.get(binding).copied().unwrap_or(0)
        })
        .collect();
    for (i, binding) in bindings.iter().enumerate() {
        if unread.contains(&i) {
            continue;
        }
        if let Some(group) = partitions.iter().position(|p| p == binding) {
            group_of.insert(i, group);
            continue;
        }
        if binding.table_index == window_index {
            continue;
        }
        let Some(at) = input_bindings.iter().position(|b| b == binding) else {
            return false;
        };
        payload.push(BoundExpr::column_ref(*binding, input_types[at].clone(), format!("c{}", payload.len())));
    }
    let (order, modifiers) = window.orders[0].clone();
    // only the value of the order itself: no payload needed
    if payload.len() == 1 && column_of(&order).is_some_and(|b| column_of(&payload[0]) == Some(b)) {
        payload.clear();
    }
    let include_row_number = bindings
        .iter()
        .enumerate()
        .any(|(i, b)| b.table_index == window_index && !unread.contains(&i));
    let mut struct_pack = payload.len() > 1;
    // `TryPrepareLateMaterialization`: the row ids in place of the struct
    let late = if struct_pack {
        try_late_materialization(&mut input, &partitions, &payload, next)
    } else {
        None
    };
    let lhs_payload = std::mem::take(&mut payload);
    match &late {
        Some(late) => {
            payload = vec![late.row_id.clone()];
            struct_pack = false;
        }
        None => payload = lhs_payload,
    }

    // `CreateAggregateExpression`; the order can be NULL, so the NULL-preserving variants
    let requires_arg = !payload.is_empty();
    let mut arguments: Vec<BoundExpr> = Vec::new();
    match payload.len() {
        0 => {}
        1 => arguments.push(payload[0].clone()),
        _ => {
            let names = payload.iter().map(|p| p.alias.clone()).collect();
            let Ok(packed) = crate::function::nested::bind_nested_function("struct_pack", names, payload.clone()) else {
                return false;
            };
            arguments.push(packed);
        }
    }
    arguments.push(order.clone());
    if limit > 1 {
        arguments.push(BoundExpr::constant(Value::bigint(limit)));
    }
    let change_to_arg = !requires_arg && limit > 1;
    if change_to_arg {
        arguments.insert(1, order.clone());
    }
    let ascending = modifiers.order == OrderType::Ascending;
    let mut name = String::new();
    if requires_arg || change_to_arg {
        name.push_str("arg_");
    }
    name.push_str(if ascending { "min" } else { "max" });
    if requires_arg || change_to_arg {
        name.push_str("_nulls_last");
    }
    let functions = BuiltinFunctions::get();
    let Some(set) = functions.aggregate(&name) else {
        return false;
    };
    let Ok(aggregate) = bind_aggregate_function(set, arguments, None, false) else {
        return false;
    };
    let aggregate_type = aggregate.return_type.clone();

    // `CreateAggregateOperator`
    let mut groups = window.partitions.clone();
    if groups.is_empty() && limit == 1 {
        // a constant group keeps the window's no rows for no input
        groups.push(BoundExpr::constant(Value::boolean(true)));
    }
    let group_types: Vec<LogicalType> = groups.iter().map(|g| g.return_type.clone()).collect();
    let group_index = take(next);
    let aggregate_index = take(next);
    let mut plan = LogicalOperator::new(
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index: take(next),
            groups,
            aggregates: vec![aggregate],
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
            perfect_hash: false,
            domain_cap: None,
        },
        vec![input],
    );
    let aggregate_ref = BoundExpr::column_ref(ColumnBinding::new(aggregate_index, 0), aggregate_type.clone(), "");

    // `TryCreateUnnestOperator`
    let (value, row_number) = if limit > 1 {
        let mut expressions = vec![aggregate_ref.clone()];
        if include_row_number {
            let generic = crate::function::generic::bind_generic_function;
            let Ok(length) = generic("array_length", vec![aggregate_ref.clone(), BoundExpr::constant(Value::bigint(1))])
            else {
                return false;
            };
            let Ok(series) = generic("generate_series", vec![BoundExpr::constant(Value::bigint(1)), length]) else {
                return false;
            };
            expressions.push(series);
        }
        let unnest_index = take(next);
        let element_type = aggregate_type.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
        plan = LogicalOperator::new(
            K::Unnest {
                table_index: unnest_index,
                expressions,
            },
            vec![plan],
        );
        let value = BoundExpr::column_ref(ColumnBinding::new(unnest_index, 0), element_type, "");
        let row_number = BoundExpr::column_ref(ColumnBinding::new(unnest_index, 1), LogicalType::BIGINT, "row_number");
        (value, row_number)
    } else {
        (aggregate_ref, BoundExpr::constant(Value::bigint(1)))
    };

    // `CreateProjectionOperator`: [the groups read, in order][the payload][the row number]
    let ordered_groups: BTreeSet<usize> = group_of.values().copied().collect();
    let mut columns: Vec<BoundExpr> = ordered_groups
        .iter()
        .map(|&g| BoundExpr::column_ref(ColumnBinding::new(group_index, g), group_types[g].clone(), ""))
        .collect();
    let payload_start = columns.len();
    if struct_pack {
        for field in &payload {
            let extract = crate::function::nested::bind_nested_function(
                "struct_extract",
                Vec::new(),
                vec![value.clone(), BoundExpr::constant(Value::varchar(field.alias.clone()))],
            );
            let Ok(extract) = extract else {
                return false;
            };
            columns.push(extract);
        }
    } else {
        columns.push(value);
    }
    let row_number_at = columns.len();
    if include_row_number {
        columns.push(row_number);
    }
    let projection_index = take(next);
    let column_types: Vec<LogicalType> = columns.iter().map(|c| c.return_type.clone()).collect();
    plan = LogicalOperator::new(
        K::Projection {
            table_index: projection_index,
            expressions: columns,
        },
        vec![plan],
    );
    // `ConstructJoin`: the rows of the second scan whose row ids the aggregate kept; an INNER join
    // where a row id can be there more than once, or the row number is read
    if let Some(late) = late {
        let join_type = if late.duplicates || include_row_number {
            duckrs_parser::ast::JoinType::Inner
        } else {
            duckrs_parser::ast::JoinType::Semi
        };
        let condition = crate::planner::logical::JoinCondition {
            bound: None,
            left: BoundExpr::column_ref(
                ColumnBinding::new(late.lhs_index, late.lhs_types.len() - 1),
                LogicalType::BIGINT,
                "rowid",
            ),
            right: BoundExpr::column_ref(
                ColumnBinding::new(projection_index, payload_start),
                LogicalType::BIGINT,
                "rowid",
            ),
            comparison: ComparisonOp::Equal,
            filter: None,
        };
        plan = LogicalOperator::new(
            K::Join {
                join_type,
                conditions: vec![condition],
                predicate: None,
                algorithm: Default::default(),
            },
            vec![late.lhs, plan],
        );
        let partition_count = partitions.len();
        let final_index = take(next);
        let mut finals = Vec::with_capacity(topmost_bindings.len());
        let mut payload_at = partition_count;
        for (i, binding) in bindings.iter().enumerate() {
            if unread.contains(&i) {
                finals.push(BoundExpr::constant(Value::null(topmost_types[i].clone())));
                replacements.map.insert(topmost_bindings[i], ColumnBinding::new(final_index, i));
                continue;
            }
            let (column, ty) = if let Some(&group) = group_of.get(&i) {
                (ColumnBinding::new(late.lhs_index, group), late.lhs_types[group].clone())
            } else if binding.table_index == window_index {
                (
                    ColumnBinding::new(projection_index, row_number_at),
                    column_types[row_number_at].clone(),
                )
            } else {
                let at = payload_at;
                payload_at += 1;
                (ColumnBinding::new(late.lhs_index, at), late.lhs_types[at].clone())
            };
            let mut expr = BoundExpr::column_ref(column, ty.clone(), "");
            if ty != topmost_types[i] {
                expr = expr.cast_to(&topmost_types[i], false);
            }
            finals.push(expr);
            replacements.map.insert(topmost_bindings[i], ColumnBinding::new(final_index, i));
        }
        replacements.stops.insert(final_index);
        *op = LogicalOperator::new(
            K::Projection {
                table_index: final_index,
                expressions: finals,
            },
            vec![plan],
        );
        return true;
    }

    // `UpdateTopmostBindings`: a projection with the columns the filter had, under new bindings
    let compact: BTreeMap<usize, usize> = ordered_groups.iter().enumerate().map(|(at, &g)| (g, at)).collect();
    let mut payload_at = payload_start;
    let final_index = take(next);
    let mut finals = Vec::with_capacity(topmost_bindings.len());
    for (i, binding) in bindings.iter().enumerate() {
        if unread.contains(&i) {
            finals.push(BoundExpr::constant(Value::null(topmost_types[i].clone())));
            replacements.map.insert(topmost_bindings[i], ColumnBinding::new(final_index, i));
            continue;
        }
        let at = if let Some(group) = group_of.get(&i) {
            compact[group]
        } else if binding.table_index == window_index {
            row_number_at
        } else {
            let at = payload_at;
            payload_at += 1;
            at
        };
        let mut expr = BoundExpr::column_ref(ColumnBinding::new(projection_index, at), column_types[at].clone(), "");
        if column_types[at] != topmost_types[i] {
            expr = expr.cast_to(&topmost_types[i], false);
        }
        finals.push(expr);
        replacements.map.insert(topmost_bindings[i], ColumnBinding::new(final_index, i));
    }
    replacements.stops.insert(final_index);
    *op = LogicalOperator::new(
        K::Projection {
            table_index: final_index,
            expressions: finals,
        },
        vec![plan],
    );
    true
}

/// What late materialization made of the window's input.
struct LateMaterialization {
    /// the second scan, under a projection of the partitions, the payload and the row id
    lhs: LogicalOperator,
    lhs_index: usize,
    lhs_types: Vec<LogicalType>,
    /// the row id, as the window's input outputs it now
    row_id: BoundExpr,
    /// an INNER join below can repeat a row of the table
    duplicates: bool,
}

/// `IsRecreatableByLateMaterialization`: a column, or one non-TRY cast of one. The column under it.
fn recreatable(expr: &BoundExpr) -> Option<(ColumnBinding, bool)> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some((*binding, false)),
        BoundExprKind::Cast { child, try_cast: false } => column_of(child).map(|b| (b, true)),
        _ => None,
    }
}

/// `CanUseLateMaterialization` and `TryPrepareLateMaterialization`: the partitions and the payload
/// come from one table scan, through projections, filters and joins on equal columns. The scan in
/// `input` then outputs its row id up to the top.
fn try_late_materialization(
    input: &mut LogicalOperator,
    partitions: &[ColumnBinding],
    payload: &[BoundExpr],
    next: &mut usize,
) -> Option<LateMaterialization> {
    let mut projections: Vec<ColumnBinding> = partitions.to_vec();
    projections.extend(payload.iter().map(|p| column_of(p).expect("a payload column")));
    // only a single cast per column can be applied again
    let mut cast = vec![false; projections.len()];
    let mut path = Vec::new();
    let mut duplicates = false;
    let mut op: &LogicalOperator = input;
    loop {
        match &op.kind {
            K::Get { .. } if op.children.is_empty() => break,
            K::Projection {
                table_index,
                expressions,
            } => {
                for (i, projection) in projections.iter_mut().enumerate() {
                    if projection.table_index != *table_index {
                        return None;
                    }
                    let (below, is_cast) = recreatable(expressions.get(projection.column_index)?)?;
                    if is_cast {
                        if cast[i] {
                            return None;
                        }
                        cast[i] = true;
                    }
                    *projection = below;
                }
                path.push(0);
            }
            K::Filter { .. } => path.push(0),
            // a condition on both sides that is no comparison of one with the other (the predicate)
            // has no columns to stand for each other
            K::Join {
                join_type, conditions, ..
            } if matches!(
                join_type,
                duckrs_parser::ast::JoinType::Inner
                    | duckrs_parser::ast::JoinType::Semi
                    | duckrs_parser::ast::JoinType::Anti
            ) =>
            {
                // an INNER join can give a row of the table more than once
                duplicates |= *join_type == duckrs_parser::ast::JoinType::Inner;
                // the columns of a condition are equal, so either side's can stand for the other's
                let mut equal: HashMap<ColumnBinding, ColumnBinding> = HashMap::new();
                for condition in conditions {
                    if condition.comparison != ComparisonOp::Equal {
                        return None;
                    }
                    let (left, right) = (column_of(&condition.left)?, column_of(&condition.right)?);
                    equal.insert(left, right);
                    equal.insert(right, left);
                }
                let left_side = op.children[0].column_bindings();
                let right_side = op.children[1].column_bindings();
                let (mut all_left, mut all_right) = (true, true);
                for projection in &projections {
                    if left_side.contains(projection) {
                        all_left &= equal.contains_key(projection);
                    } else if right_side.contains(projection) {
                        all_right &= equal.contains_key(projection);
                    } else {
                        return None;
                    }
                }
                if !all_left && !all_right {
                    return None;
                }
                let (replaced, child) = if all_right { (&right_side, 0) } else { (&left_side, 1) };
                for projection in &mut projections {
                    if replaced.contains(projection) {
                        *projection = equal[projection];
                    }
                }
                path.push(child);
            }
            _ => return None,
        }
        op = &op.children[*path.last().expect("pushed")];
    }
    let K::Get {
        table_index: get_index,
        index_rows,
        ..
    } = &op.kind
    else {
        return None;
    };
    if index_rows.is_some() || projections.iter().any(|p| p.table_index != *get_index) {
        return None;
    }
    let get_index = *get_index;
    // `ConstructLHS`: a copy of the scan whose filters only skip row groups, with the row id last
    let lhs_get_index = take(next);
    let mut lhs = op.clone();
    if let K::Get {
        table_index,
        filters,
        optional_filters,
        projection,
        ..
    } = &mut lhs.kind
    {
        *table_index = lhs_get_index;
        optional_filters.extend(std::mem::take(filters));
        *projection = None;
    }
    visit_bindings(&mut lhs.kind, &mut |binding, depth| {
        if depth == 0 && binding.table_index == get_index {
            binding.table_index = lhs_get_index;
        }
    });
    let lhs_row_id = super::late_materialization::get_or_insert_row_id(&mut lhs);
    let lhs_scan_types = lhs.types();
    let mut columns: Vec<BoundExpr> = projections
        .iter()
        .map(|p| {
            BoundExpr::column_ref(
                ColumnBinding::new(lhs_get_index, p.column_index),
                lhs_scan_types[p.column_index].clone(),
                "",
            )
        })
        .collect();
    columns.push(BoundExpr::column_ref(
        ColumnBinding::new(lhs_get_index, lhs_row_id),
        LogicalType::BIGINT,
        "rowid",
    ));
    let lhs_types = columns.iter().map(|c| c.return_type.clone()).collect();
    let lhs_index = take(next);
    let lhs = LogicalOperator::new(
        K::Projection {
            table_index: lhs_index,
            expressions: columns,
        },
        vec![lhs],
    );
    // the RHS: the row id from the scan up through the operators on the path
    fn add_row_id(op: &mut LogicalOperator, path: &[usize]) -> ColumnBinding {
        let Some((&child, rest)) = path.split_first() else {
            let K::Get { table_index, .. } = &op.kind else {
                unreachable!("the path ends at the scan")
            };
            let table_index = *table_index;
            return ColumnBinding::new(table_index, super::late_materialization::get_or_insert_row_id(op));
        };
        let below = add_row_id(&mut op.children[child], rest);
        match &mut op.kind {
            K::Projection {
                table_index,
                expressions,
            } => {
                expressions.push(BoundExpr::column_ref(below, LogicalType::BIGINT, "rowid"));
                ColumnBinding::new(*table_index, expressions.len() - 1)
            }
            _ => below,
        }
    }
    let row_id = add_row_id(input, &path);
    Some(LateMaterialization {
        lhs,
        lhs_index,
        lhs_types,
        row_id: BoundExpr::column_ref(row_id, LogicalType::BIGINT, "rowid"),
        duplicates,
    })
}
