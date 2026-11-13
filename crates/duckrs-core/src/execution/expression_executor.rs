// Ported from duckdb/src/execution/expression_executor.cpp and
// duckdb/src/execution/expression_executor/execute_{cast,comparison,conjunction,constant,
// function,operator,reference}.cpp @ rust-port-baseline
//
//! Evaluates bound expressions over a chunk of rows, a vector at a time. Filters evaluate
//! their condition and keep the rows where it is true (DuckDB's specialized `Select` paths
//! are a later optimization).

use crate::catalog::SequenceCatalogEntry;
use crate::execution::run_plan;
use crate::function::cast::{CastMode, cast_vector};
use crate::function::operators::DIVISION_BY_ZERO;
use crate::planner::expression::{
    BoundExpr, BoundExprKind, BoundOperator, BoundSubquery, LambdaKind, SequenceOp, SubqueryKind, SubqueryResult,
};
use duckrs_common::comparison::{ComparisonType, DuckOrd};
use duckrs_common::executor;
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys};
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType, PhysicalType};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector, VectorType};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::ComparisonOp;
use std::collections::HashSet;
use std::sync::Arc;

fn comparison_type(op: ComparisonOp) -> ComparisonType {
    match op {
        ComparisonOp::Equal => ComparisonType::Equal,
        ComparisonOp::NotEqual => ComparisonType::NotEqual,
        ComparisonOp::LessThan => ComparisonType::LessThan,
        ComparisonOp::GreaterThan => ComparisonType::GreaterThan,
        ComparisonOp::LessThanOrEqual => ComparisonType::LessThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonType::GreaterThanOrEqual,
        ComparisonOp::DistinctFrom => ComparisonType::DistinctFrom,
        ComparisonOp::NotDistinctFrom => ComparisonType::NotDistinctFrom,
    }
}

/// Whether both inputs are constant, so the result is a single constant row.
fn all_constant(vectors: &[&Vector]) -> bool {
    vectors.iter().all(|v| v.vector_type() == VectorType::Constant)
}

/// A comparison of two vectors of the same type, row by row. DISTINCT FROM compares NULLs
/// too; the other comparisons are NULL when either side is.
fn compare_typed<T: NativeType + DuckOrd>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    op: ComparisonType,
) {
    let null_aware = matches!(op, ComparisonType::DistinctFrom | ComparisonType::NotDistinctFrom);
    if !null_aware {
        executor::binary::<T, T, bool>(left, right, result, count, |a, b| {
            op.evaluate(false, false, || a.duck_cmp(&b))
        });
        return;
    }
    let rows = if all_constant(&[left, right]) { 1 } else { count };
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    let (ld, rd) = (lu.data::<T>(), ru.data::<T>());
    result.prepare_flat(rows);
    let out = result.data_mut::<bool>();
    for (i, slot) in out.iter_mut().enumerate().take(rows) {
        let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
        let (lnull, rnull) = (!lu.validity.is_valid(li), !ru.validity.is_valid(ri));
        *slot = op.evaluate(lnull, rnull, || ld[li].duck_cmp(&rd[ri]));
    }
    if rows == 1 && count != 1 {
        result.set_constant();
    }
}

fn compare_strings(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) {
    let null_aware = matches!(op, ComparisonType::DistinctFrom | ComparisonType::NotDistinctFrom);
    if !null_aware {
        executor::binary_str::<bool>(left, right, result, count, |a, b| {
            op.evaluate(false, false, || a.cmp(b))
        });
        return;
    }
    let rows = if all_constant(&[left, right]) { 1 } else { count };
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    result.prepare_flat(rows);
    let out = result.data_mut::<bool>();
    for (i, slot) in out.iter_mut().enumerate().take(rows) {
        let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
        let (lnull, rnull) = (!lu.validity.is_valid(li), !ru.validity.is_valid(ri));
        *slot = op.evaluate(lnull, rnull, || lu.str_at(i).cmp(ru.str_at(i)));
    }
    if rows == 1 && count != 1 {
        result.set_constant();
    }
}

/// A comparison of a dictionary vector with a constant, on the entries of the dictionary rather than
/// on every row (`ExpressionExecutor` on dictionary vectors): the result is a dictionary of booleans.
fn compare_dictionary(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    op: ComparisonType,
) -> Result<bool> {
    use duckrs_common::vector::VectorType;
    let constant = |v: &Vector| v.vector_type() == VectorType::Constant;
    let (dictionary, left_is_dictionary) = match (left.dictionary(), right.dictionary()) {
        (Some(d), None) if constant(right) => (d, true),
        (None, Some(d)) if constant(left) => (d, false),
        _ => return Ok(false),
    };
    let (sel, child) = dictionary;
    if child.vector_type() != VectorType::Flat {
        return Ok(false);
    }
    let entries = child.capacity();
    if entries >= count {
        return Ok(false);
    }
    let mut on_entries = Vector::new(LogicalType::BOOLEAN, entries);
    if left_is_dictionary {
        compare(child, right, &mut on_entries, entries, op)?;
    } else {
        compare(left, child, &mut on_entries, entries, op)?;
    }
    on_entries.flatten(entries);
    on_entries.slice(sel, count);
    *result = on_entries;
    Ok(true)
}

fn compare(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) -> Result<()> {
    if compare_dictionary(left, right, result, count, op)? {
        return Ok(());
    }
    match left.physical_type() {
        PhysicalType::Bool => compare_typed::<bool>(left, right, result, count, op),
        PhysicalType::Int8 => compare_typed::<i8>(left, right, result, count, op),
        PhysicalType::Int16 => compare_typed::<i16>(left, right, result, count, op),
        PhysicalType::Int32 => compare_typed::<i32>(left, right, result, count, op),
        PhysicalType::Int64 => compare_typed::<i64>(left, right, result, count, op),
        PhysicalType::Int128 => compare_typed::<i128>(left, right, result, count, op),
        PhysicalType::UInt8 => compare_typed::<u8>(left, right, result, count, op),
        PhysicalType::UInt16 => compare_typed::<u16>(left, right, result, count, op),
        PhysicalType::UInt32 => compare_typed::<u32>(left, right, result, count, op),
        PhysicalType::UInt64 => compare_typed::<u64>(left, right, result, count, op),
        PhysicalType::UInt128 => compare_typed::<u128>(left, right, result, count, op),
        PhysicalType::Float => compare_typed::<f32>(left, right, result, count, op),
        PhysicalType::Double => compare_typed::<f64>(left, right, result, count, op),
        PhysicalType::Interval => compare_typed::<duckrs_common::types::Interval>(left, right, result, count, op),
        // BIT by its bits, VARIANT by what it holds (`variant_comparator`)
        PhysicalType::Varchar if matches!(left.logical_type().id(), LogicalTypeId::Bit | LogicalTypeId::Variant) => {
            return compare_nested(left, right, result, count, op);
        }
        PhysicalType::Varchar => compare_strings(left, right, result, count, op),
        PhysicalType::List | PhysicalType::Struct | PhysicalType::Array => {
            return compare_nested(left, right, result, count, op);
        }
        other => {
            return Err(crate::unported(format!("comparisons of {other} values")));
        }
    }
    Ok(())
}

/// `VectorOperations::NestedEquals` and friends: inside of a nested value NULLs equal each other
/// and sort last, which is what the bytes of a sort key compare like.
fn compare_nested(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) -> Result<()> {
    let null_aware = matches!(op, ComparisonType::DistinctFrom | ComparisonType::NotDistinctFrom);
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    if !null_aware && left.logical_type().id() == LogicalTypeId::Union && !JOIN_CONDITION.with(|c| c.get()) {
        return compare_unions(left, right, result, count, op);
    }
    let left_keys = create_sort_keys(&[(left, modifiers)], count)?;
    let right_keys = create_sort_keys(&[(right, modifiers)], count)?;
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    result.prepare_flat(count.max(1));
    for i in 0..count {
        let (lnull, rnull) = (
            !lu.validity.is_valid(lu.sel.get(i)),
            !ru.validity.is_valid(ru.sel.get(i)),
        );
        if !null_aware && (lnull || rnull) {
            result.set_null(i, true);
            continue;
        }
        result.data_mut::<bool>()[i] = op.evaluate(lnull, rnull, || left_keys[i].cmp(&right_keys[i]));
    }
    Ok(())
}

thread_local! {
    /// Whether a join's condition is being evaluated: joins match UNIONs as the row matcher does,
    /// by the whole value, not with `UnionComparator`'s NULL members.
    static JOIN_CONDITION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` as the evaluation of a join condition.
pub fn as_join_condition<T>(f: impl FnOnce() -> T) -> T {
    let before = JOIN_CONDITION.with(|c| c.replace(true));
    let result = f();
    JOIN_CONDITION.with(|c| c.set(before));
    result
}

/// `UnionComparator` of an SQL comparison: different tags compare as the tags, and the same tag
/// as that member, which is NULL when the member is.
fn compare_unions(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) -> Result<()> {
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    let (mut left, mut right) = (left.clone(), right.clone());
    left.flatten(count);
    right.flatten(count);
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    let (lchildren, rchildren) = (left.struct_children(), right.struct_children());
    let (ltags, rtags) = (lchildren[0].to_unified(count), rchildren[0].to_unified(count));
    // the sort keys of each member on either side, made when a row first needs them
    type MemberKeys = Option<(Vec<Vec<u8>>, Vec<Vec<u8>>)>;
    let mut member_keys: Vec<MemberKeys> = vec![None; lchildren.len()];
    result.prepare_flat(count.max(1));
    for i in 0..count {
        if !lu.validity.is_valid(lu.sel.get(i)) || !ru.validity.is_valid(ru.sel.get(i)) {
            result.set_null(i, true);
            continue;
        }
        let ltag = ltags.data::<u8>()[ltags.sel.get(i)];
        let rtag = rtags.data::<u8>()[rtags.sel.get(i)];
        if ltag != rtag {
            result.data_mut::<bool>()[i] = op.evaluate(false, false, || ltag.cmp(&rtag));
            continue;
        }
        let member = usize::from(ltag) + 1;
        let (lm, rm) = (lchildren[member].to_unified(count), rchildren[member].to_unified(count));
        if !lm.validity.is_valid(lm.sel.get(i)) || !rm.validity.is_valid(rm.sel.get(i)) {
            result.set_null(i, true);
            continue;
        }
        if member_keys[member].is_none() {
            member_keys[member] = Some((
                create_sort_keys(&[(&lchildren[member], modifiers)], count)?,
                create_sort_keys(&[(&rchildren[member], modifiers)], count)?,
            ));
        }
        let (lk, rk) = member_keys[member].as_ref().expect("computed");
        result.data_mut::<bool>()[i] = op.evaluate(false, false, || lk[i].cmp(&rk[i]));
    }
    Ok(())
}

/// Three-valued AND/OR over BOOLEAN vectors (`ExecuteConjunction`).
fn conjunction(children: &[Vector], and: bool, count: usize) -> Vector {
    let refs: Vec<&Vector> = children.iter().collect();
    let rows = if all_constant(&refs) { 1 } else { count };
    let unified: Vec<_> = children.iter().map(|c| c.to_unified(count)).collect();
    let mut result = Vector::new(LogicalType::BOOLEAN, rows.max(1));
    let mut nulls = Vec::new();
    {
        let out = result.data_mut::<bool>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            // AND: false wins over NULL, NULL over true; OR: true wins over NULL, NULL over false
            let mut saw_null = false;
            let mut decided = false;
            for u in &unified {
                let idx = u.sel.get(i);
                if !u.validity.is_valid(idx) {
                    saw_null = true;
                } else if u.data::<bool>()[idx] != and {
                    decided = true;
                    break;
                }
            }
            if decided {
                *slot = !and;
            } else if saw_null {
                nulls.push(i);
            } else {
                *slot = and;
            }
        }
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
    if rows == 1 && count != 1 {
        result.set_constant();
    }
    result
}

/// An OR of `x = constant` with the same `x` (how `x IN (...)` binds): `x` is evaluated once and
/// compared with each constant, instead of once per comparison (C++ evaluates `IN` with its input once).
fn execute_in_list(children: &[BoundExpr], input: &DataChunk, count: usize) -> Result<Option<Vector>> {
    use duckrs_parser::ast::ComparisonOp;
    if children.len() < 2 {
        return Ok(None);
    }
    let mut left_text: Option<String> = None;
    let mut constants = Vec::with_capacity(children.len());
    for child in children {
        let BoundExprKind::Comparison {
            op: ComparisonOp::Equal,
            left,
            right,
        } = &child.kind
        else {
            return Ok(None);
        };
        let BoundExprKind::Constant(_) = right.kind else {
            return Ok(None);
        };
        if left.is_volatile() || matches!(left.kind, BoundExprKind::Reference(_) | BoundExprKind::Constant(_)) {
            // nothing to save on a column itself
            return Ok(None);
        }
        let text = format!("{left:?}");
        match &left_text {
            Some(first) if *first != text => return Ok(None),
            Some(_) => {}
            None => left_text = Some(text),
        }
        constants.push((right.as_ref(), child.return_type.clone()));
    }
    let BoundExprKind::Comparison { left, .. } = &children[0].kind else {
        return Ok(None);
    };
    let value = execute(left, input)?;
    if value.logical_type().id() == LogicalTypeId::SqlNull {
        return Ok(None);
    }
    if value.physical_type() == PhysicalType::Varchar && value.logical_type().id() == LogicalTypeId::Varchar {
        // strings: each row against the constants, in one pass over the rows
        let mut strings: Vec<Vec<u8>> = Vec::with_capacity(constants.len());
        for (constant, _) in &constants {
            let BoundExprKind::Constant(constant) = &constant.kind else {
                unreachable!("looked at above")
            };
            if constant.is_null() || *constant.logical_type() != *value.logical_type() {
                return Ok(None);
            }
            let vector = Vector::constant(constant);
            match vector.str_at(0) {
                Some(bytes) => strings.push(bytes.to_vec()),
                None => return Ok(None),
            }
        }
        let rows = if all_constant(&[&value]) { 1 } else { count };
        let unified = value.to_unified(rows);
        let mut result = Vector::new(LogicalType::BOOLEAN, rows.max(1));
        let mut nulls = Vec::new();
        {
            let out = result.data_mut::<bool>();
            for (row, slot) in out.iter_mut().enumerate().take(rows) {
                if unified.is_valid(row) {
                    let string = unified.str_at(row);
                    *slot = strings.iter().any(|s| s.as_slice() == string);
                } else {
                    nulls.push(row);
                }
            }
        }
        nulls.into_iter().for_each(|row| result.set_null(row, true));
        if rows == 1 && count != 1 {
            result.set_constant();
        }
        return Ok(Some(result));
    }
    let mut results = Vec::with_capacity(constants.len());
    for (constant, return_type) in constants {
        let constant = execute(constant, input)?;
        if constant.logical_type() != value.logical_type() {
            return Ok(None);
        }
        let mut result = Vector::new(return_type, count.max(1));
        compare(&value, &constant, &mut result, count, ComparisonType::Equal)?;
        results.push(result);
    }
    Ok(Some(conjunction(&results, false, count)))
}

/// `IS NULL` / `IS NOT NULL`: never NULL themselves.
fn null_check(input: &Vector, count: usize, is_null: bool) -> Vector {
    let constant = input.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    let u = input.to_unified(count);
    let mut result = Vector::new(LogicalType::BOOLEAN, rows.max(1));
    {
        let out = result.data_mut::<bool>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            *slot = u.validity.is_valid(u.sel.get(i)) != is_null;
        }
    }
    if constant && count != 1 {
        result.set_constant();
    }
    result
}

/// The rows of `input` listed in `rows`.
fn slice_rows(input: &DataChunk, rows: &[u32]) -> DataChunk {
    let mut chunk = input.clone();
    if rows.len() != input.len() {
        chunk.slice(&SelectionVector::from_vec(rows.to_vec()), rows.len());
    }
    chunk
}

/// Writes `source[i]` to `result[rows[i]]`.
fn scatter(source: &Vector, rows: &[u32], result: &mut Vector) -> Result<()> {
    if source.logical_type() == result.logical_type() && result.vector_type() == duckrs_common::vector::VectorType::Flat
    {
        let u = source.to_unified(rows.len());
        macro_rules! typed {
            ($t:ty) => {{
                let data = u.data::<$t>();
                for (i, &row) in rows.iter().enumerate() {
                    let index = u.sel.get(i);
                    if u.validity.is_valid(index) {
                        result.data_mut::<$t>()[row as usize] = data[index];
                    } else {
                        result.set_null(row as usize, true);
                    }
                }
                return Ok(());
            }};
        }
        match source.physical_type() {
            PhysicalType::Bool => typed!(bool),
            PhysicalType::Int8 => typed!(i8),
            PhysicalType::Int16 => typed!(i16),
            PhysicalType::Int32 => typed!(i32),
            PhysicalType::Int64 => typed!(i64),
            PhysicalType::Int128 => typed!(i128),
            PhysicalType::UInt8 => typed!(u8),
            PhysicalType::UInt16 => typed!(u16),
            PhysicalType::UInt32 => typed!(u32),
            PhysicalType::UInt64 => typed!(u64),
            PhysicalType::UInt128 => typed!(u128),
            PhysicalType::Float => typed!(f32),
            PhysicalType::Double => typed!(f64),
            PhysicalType::Varchar => {
                for (i, &row) in rows.iter().enumerate() {
                    match u.validity.is_valid(u.sel.get(i)) {
                        true => result.set_str(row as usize, u.str_at(i)),
                        false => result.set_null(row as usize, true),
                    }
                }
                return Ok(());
            }
            _ => {}
        }
    }
    for (i, &row) in rows.iter().enumerate() {
        result.set_value(row as usize, &source.get_value(i))?;
    }
    Ok(())
}

/// `ExecuteExpression(BoundCaseExpression)`: rows go to the first check that holds, and each
/// THEN only sees its own rows.
fn execute_case(
    checks: &[(BoundExpr, BoundExpr)],
    else_expr: &BoundExpr,
    return_type: &LogicalType,
    input: &DataChunk,
) -> Result<Vector> {
    let mut result = Vector::new(return_type.clone(), input.len().max(1));
    let mut remaining: Vec<u32> = (0..input.len() as u32).collect();
    for (when, then) in checks {
        if remaining.is_empty() {
            break;
        }
        let rows = slice_rows(input, &remaining);
        let (selected, count) = select(when, &rows)?;
        let mut matched = Vec::with_capacity(count);
        let mut positions = Vec::with_capacity(count);
        for i in 0..count {
            matched.push(remaining[selected.get(i)]);
            positions.push(selected.get(i) as u32);
        }
        if !matched.is_empty() {
            let values = execute(then, &slice_rows(&rows, &positions))?;
            scatter(&values, &matched, &mut result)?;
            // the rows chosen are at `positions` of `remaining`, in order
            let mut keep = vec![true; remaining.len()];
            for &position in &positions {
                keep[position as usize] = false;
            }
            let mut position = 0;
            remaining.retain(|_| {
                position += 1;
                keep[position - 1]
            });
        }
    }
    if !remaining.is_empty() {
        let values = execute(else_expr, &slice_rows(input, &remaining))?;
        scatter(&values, &remaining, &mut result)?;
    }
    Ok(result)
}

/// The result of a subquery for correlated values with this key, which runs the first time
/// something asks. An error stays too: TRY runs a failed chunk again.
fn subquery_result(subquery: &BoundSubquery, key: &[u8], values: &[Value]) -> Arc<Result<SubqueryResult>> {
    if values.is_empty() {
        // an uncorrelated subquery runs once, while the threads of a parallel pipeline that ask for
        // it at the same time wait
        let mut results = subquery.results.lock();
        if let Some(result) = results.get(key) {
            return result.clone();
        }
        let result = Arc::new(run_subquery(subquery, values));
        results.insert(key.to_vec(), result.clone());
        return result;
    }
    let cached = !subquery.volatile;
    if cached && let Some(result) = subquery.results.lock().get(key) {
        return result.clone();
    }
    let result = Arc::new(run_subquery(subquery, values));
    if cached {
        subquery.results.lock().insert(key.to_vec(), result.clone());
    }
    result
}

fn run_subquery(subquery: &BoundSubquery, values: &[Value]) -> Result<SubqueryResult> {
    let mut plan = subquery.plan.clone();
    if !values.is_empty() {
        let columns: Vec<_> = subquery
            .correlated
            .iter()
            .copied()
            .zip(values.iter().cloned())
            .collect();
        plan.substitute_columns(&columns);
    }
    // a subquery that runs once has the threads of the query; one that runs per row does not
    let collection = if values.is_empty() {
        crate::execution::run_plan_on_threads(plan)?
    } else {
        run_plan(plan)?
    };
    let mut result = SubqueryResult {
        rows: collection.len() as u64,
        first: None,
        keys: HashSet::new(),
        has_null: false,
        rows_values: Vec::new(),
    };
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    for index in 0..collection.chunk_count() {
        let chunk = collection.chunk(index);
        let column = chunk.column(0);
        if result.first.is_none() && !chunk.is_empty() {
            result.first = Some(column.get_value(0));
        }
        if !matches!(subquery.kind, SubqueryKind::Any(_)) {
            continue;
        }
        if subquery.row {
            result
                .rows_values
                .extend((0..chunk.len()).map(|row| column.get_value(row)));
        }
        let u = column.to_unified(chunk.len());
        let keys = create_sort_keys(&[(column, modifiers)], chunk.len())?;
        for (row, key) in keys.into_iter().enumerate() {
            if u.validity.is_valid(u.sel.get(row)) {
                result.keys.insert(key);
            } else {
                result.has_null = true;
            }
        }
    }
    Ok(result)
}

/// `MarkJoinRowComparison::CompareEquality`: whether two rows are equal under SQL's three-valued
/// logic, `None` for unknown: a pair of columns that differs decides, else a NULL leaves it open.
fn row_equals(left: &Value, right: &Value) -> Option<bool> {
    let (ValueData::Struct(left), ValueData::Struct(right)) = (left.data(), right.data()) else {
        return None;
    };
    let mut unknown = false;
    for (l, r) in left.iter().zip(right) {
        if l.is_null() || r.is_null() {
            unknown = true;
        } else if l != r {
            return Some(false);
        }
    }
    (!unknown).then_some(true)
}

/// `row op ANY (...)` of a row subquery, for `=` and `<>`: `None` for NULL.
fn any_row_matches(result: &SubqueryResult, op: ComparisonOp, child: &Value) -> Option<bool> {
    let mut unknown = child.is_null();
    for row in &result.rows_values {
        let equal = if row.is_null() || child.is_null() {
            None
        } else {
            row_equals(child, row)
        };
        match equal.map(|equal| equal == (op == ComparisonOp::Equal)) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(false)
}

/// `x op ANY (...)` for a value with sort key `key`: `None` for NULL.
fn any_matches(result: &SubqueryResult, op: ComparisonOp, key: &[u8]) -> Option<bool> {
    let found = match op {
        ComparisonOp::Equal => result.keys.contains(key),
        ComparisonOp::NotEqual => result.keys.iter().any(|k| k != key),
        ComparisonOp::LessThan => result.keys.iter().any(|k| key < k.as_slice()),
        ComparisonOp::LessThanOrEqual => result.keys.iter().any(|k| key <= k.as_slice()),
        ComparisonOp::GreaterThan => result.keys.iter().any(|k| key > k.as_slice()),
        _ => result.keys.iter().any(|k| key >= k.as_slice()),
    };
    if found {
        Some(true)
    } else if result.has_null {
        None
    } else {
        Some(false)
    }
}

pub(super) const MULTIPLE_ROWS: &str = "More than one row returned by a subquery used as an expression - scalar subqueries can \
                             only return a single row.\n\nUse \"SET scalar_subquery_error_on_multiple_rows=false\" to \
                             revert to previous behavior of returning a random row.";

/// What one row of the query gets from a subquery. `key` is the sort key of ANY's child, and
/// `None` if that is NULL.
fn subquery_value(
    subquery: &BoundSubquery,
    result: &SubqueryResult,
    key: Option<&[u8]>,
    return_type: &LogicalType,
) -> Result<Value> {
    Ok(match subquery.kind {
        SubqueryKind::Scalar => {
            if result.rows > 1 && subquery.error_on_multiple_rows {
                return Err(Error::new(ErrorKind::InvalidInput, MULTIPLE_ROWS));
            }
            result.first.clone().unwrap_or_else(|| Value::null(return_type.clone()))
        }
        SubqueryKind::Exists => Value::boolean(result.rows > 0),
        // ANY over nothing is false, even for NULL
        SubqueryKind::Any(_) if result.rows == 0 => Value::boolean(false),
        SubqueryKind::Any(op) => match key.and_then(|key| any_matches(result, op, key)) {
            Some(answer) => Value::boolean(answer),
            None => Value::null(LogicalType::BOOLEAN),
        },
    })
}

/// `PlanSubquery`, for what it computes.
fn execute_subquery(
    subquery: &BoundSubquery,
    child: Option<&BoundExpr>,
    correlated: &[BoundExpr],
    return_type: &LogicalType,
    input: &DataChunk,
) -> Result<Vector> {
    let count = input.len();
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    if correlated.is_empty() && child.is_none() {
        let result = subquery_result(subquery, &[], &[]);
        let result = result.as_ref().as_ref().map_err(Clone::clone)?;
        return Ok(Vector::constant(&subquery_value(subquery, result, None, return_type)?));
    }
    let columns = correlated
        .iter()
        .map(|c| execute(c, input))
        .collect::<Result<Vec<_>>>()?;
    let keyed: Vec<_> = columns.iter().map(|c| (c, modifiers)).collect();
    let row_keys = if keyed.is_empty() {
        vec![Vec::new(); count]
    } else {
        create_sort_keys(&keyed, count)?
    };
    let child_values = child.map(|c| execute(c, input)).transpose()?;
    let child_keys = child_values
        .as_ref()
        .map(|values| create_sort_keys(&[(values, modifiers)], count))
        .transpose()?;
    let child_format = child_values.as_ref().map(|values| values.to_unified(count));
    let mut output = Vector::new(return_type.clone(), count.max(1));
    output.prepare_flat(count.max(1));
    for (row, key) in row_keys.iter().enumerate() {
        let values: Vec<Value> = columns.iter().map(|c| c.get_value(row)).collect();
        let result = subquery_result(subquery, key, &values);
        let result = result.as_ref().as_ref().map_err(Clone::clone)?;
        let child_key = match (&child_keys, &child_format) {
            (Some(keys), Some(format)) if format.validity.is_valid(format.sel.get(row)) => Some(keys[row].as_slice()),
            _ => None,
        };
        let value = match (subquery.kind, &child_values) {
            (SubqueryKind::Any(op @ (ComparisonOp::Equal | ComparisonOp::NotEqual)), Some(values))
                if subquery.row && result.rows > 0 =>
            {
                match any_row_matches(result, op, &values.get_value(row)) {
                    Some(answer) => Value::boolean(answer),
                    None => Value::null(LogicalType::BOOLEAN),
                }
            }
            _ => subquery_value(subquery, result, child_key, return_type)?,
        };
        output.set_value(row, &value)?;
    }
    Ok(output)
}

/// `OPERATOR_TRY`: if a chunk fails, each of its rows runs by itself, and those that fail are NULL.
fn execute_try(child: &BoundExpr, return_type: &LogicalType, input: &DataChunk) -> Result<Vector> {
    match execute(child, input) {
        Ok(result) => Ok(result),
        Err(error) if error.kind == ErrorKind::Internal => Err(error),
        Err(_) => {
            let mut result = Vector::new(return_type.clone(), input.len().max(1));
            for row in 0..input.len() {
                match execute(child, &slice_rows(input, &[row as u32])) {
                    Ok(value) => result.set_value(row, &value.get_value(0))?,
                    Err(error) if error.kind == ErrorKind::Internal => return Err(error),
                    Err(_) => result.set_null(row, true),
                }
            }
            Ok(result)
        }
    }
}

/// `OPERATOR_COALESCE`: each argument only sees the rows that are still NULL.
fn execute_coalesce(children: &[BoundExpr], return_type: &LogicalType, input: &DataChunk) -> Result<Vector> {
    let mut result = Vector::new(return_type.clone(), input.len().max(1));
    let mut remaining: Vec<u32> = (0..input.len() as u32).collect();
    for child in children {
        if remaining.is_empty() {
            break;
        }
        let values = execute(child, &slice_rows(input, &remaining))?;
        let unified = values.to_unified(remaining.len());
        let mut next = Vec::new();
        for (i, &row) in remaining.iter().enumerate() {
            if unified.is_valid(i) {
                result.set_value(row as usize, &values.get_value(i))?;
            } else {
                next.push(row);
            }
        }
        remaining = next;
    }
    for row in remaining {
        result.set_null(row as usize, true);
    }
    Ok(result)
}

/// `ExpressionExecutor::Execute`: the expression's value for each of the `input` rows.
/// `ListLambdaBindData` at work: the body runs over chunks of items.
fn execute_lambda(
    kind: LambdaKind,
    list: &BoundExpr,
    initial: Option<&BoundExpr>,
    captures: &[BoundExpr],
    body: &BoundExpr,
    return_type: &LogicalType,
    input: &DataChunk,
) -> Result<Vector> {
    let count = input.len();
    let lists = execute(list, input)?;
    let initial = initial.map(|i| execute(i, input)).transpose()?;
    let captured = captures.iter().map(|c| execute(c, input)).collect::<Result<Vec<_>>>()?;
    let element = list.return_type.child_type().expect("a list").clone();
    let mut types = if kind == LambdaKind::Reduce {
        vec![return_type.clone(), element, LogicalType::BIGINT]
    } else {
        vec![element, LogicalType::BIGINT]
    };
    types.extend(captures.iter().map(|c| c.return_type.clone()));
    let mut result = Vector::new(return_type.clone(), count.max(1));
    let run = |rows: &[Vec<Value>]| -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(rows.len());
        for batch in rows.chunks(STANDARD_VECTOR_SIZE) {
            let mut chunk = DataChunk::new(&types);
            for row in batch {
                chunk.append_row(row)?;
            }
            let column = execute(body, &chunk)?;
            out.extend((0..batch.len()).map(|at| column.get_value(at)));
        }
        Ok(out)
    };
    for row in 0..count {
        let value = lists.get_value(row);
        let ValueData::List(items) = value.data() else {
            result.set_value(row, &Value::null(return_type.clone()))?;
            continue;
        };
        let capture_values: Vec<Value> = captured.iter().map(|c| c.get_value(row)).collect();
        let parameters = |item: &Value, at: usize, accumulator: Option<&Value>| {
            let mut values = Vec::with_capacity(types.len());
            values.extend(accumulator.cloned());
            values.push(item.clone());
            values.push(Value::bigint(at as i64 + 1));
            values.extend(capture_values.iter().cloned());
            values
        };
        let output = match kind {
            LambdaKind::Reduce => {
                let start = initial.as_ref().map(|i| i.get_value(row));
                let mut rest = items.iter().enumerate();
                let mut accumulator = match start {
                    Some(start) => start,
                    None => match rest.next() {
                        Some((_, first)) => first.clone(),
                        None => {
                            return Err(Error::new(
                                ErrorKind::ParameterNotAllowed,
                                "Cannot perform list_reduce on an empty input list",
                            ));
                        }
                    },
                };
                for (at, item) in rest {
                    let step = parameters(item, at, Some(&accumulator));
                    accumulator = run(std::slice::from_ref(&step))?.remove(0);
                }
                accumulator
            }
            LambdaKind::Transform | LambdaKind::Filter => {
                let rows: Vec<Vec<Value>> = items
                    .iter()
                    .enumerate()
                    .map(|(at, item)| parameters(item, at, None))
                    .collect();
                let outputs = run(&rows)?;
                let values = if kind == LambdaKind::Transform {
                    outputs
                } else {
                    items
                        .iter()
                        .zip(outputs)
                        .filter(|(_, keep)| matches!(keep.data(), ValueData::Bool(true)))
                        .map(|(item, _)| item.clone())
                        .collect()
                };
                Value::new(return_type.clone(), ValueData::List(values))?
            }
        };
        result.set_value(row, &output)?;
    }
    Ok(result)
}

pub fn execute(expr: &BoundExpr, input: &DataChunk) -> Result<Vector> {
    let count = input.len();
    match &expr.kind {
        BoundExprKind::Constant(value) => Ok(Vector::constant(value)),
        BoundExprKind::JoinFilter { child, filter } => {
            let values = execute(child, input)?;
            let Some(kept) = filter.select(&values, count) else {
                return Ok(Vector::constant(&Value::boolean(true)));
            };
            let mut result = Vector::new(LogicalType::BOOLEAN, count);
            result.prepare_flat(count);
            let data = result.data_mut::<bool>();
            for row in kept {
                data[row as usize] = true;
            }
            Ok(result)
        }
        BoundExprKind::ListLambda {
            kind,
            list,
            initial,
            captures,
            body,
        } => execute_lambda(
            *kind,
            list,
            initial.as_deref(),
            captures,
            body,
            &expr.return_type,
            input,
        ),
        BoundExprKind::Reference(index) => Ok(input.column(*index).clone()),
        BoundExprKind::ColumnRef { .. } => Err(Error::new(
            ErrorKind::Internal,
            "Attempted to execute an unresolved column reference",
        )),
        BoundExprKind::Sequence {
            op,
            sequence,
            arguments,
        } => execute_sequence(*op, sequence, arguments, input),
        BoundExprKind::Cast { child, try_cast } => {
            let source = execute(child, input)?;
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            let mode = if *try_cast { CastMode::Try } else { CastMode::Strict };
            cast_vector(&source, &mut result, count, mode)?;
            Ok(result)
        }
        BoundExprKind::Comparison { op, left, right } => {
            let (l, r) = (execute(left, input)?, execute(right, input)?);
            if l.logical_type().id() == LogicalTypeId::SqlNull {
                // comparing two untyped NULLs
                let op = comparison_type(*op);
                let value = match op {
                    ComparisonType::DistinctFrom => Value::boolean(false),
                    ComparisonType::NotDistinctFrom => Value::boolean(true),
                    _ => Value::null(LogicalType::BOOLEAN),
                };
                return Ok(Vector::constant(&value));
            }
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            compare(&l, &r, &mut result, count, comparison_type(*op))?;
            Ok(result)
        }
        BoundExprKind::Conjunction { and, children } => {
            if !*and && let Some(result) = execute_in_list(children, input, count)? {
                return Ok(result);
            }
            let children = children.iter().map(|c| execute(c, input)).collect::<Result<Vec<_>>>()?;
            Ok(conjunction(&children, *and, count))
        }
        BoundExprKind::Between {
            input: value,
            lower,
            upper,
        } => {
            let (v, l, u) = (execute(value, input)?, execute(lower, input)?, execute(upper, input)?);
            if v.logical_type().id() == LogicalTypeId::SqlNull {
                return Ok(Vector::constant(&Value::null(LogicalType::BOOLEAN)));
            }
            let mut at_least = Vector::new(LogicalType::BOOLEAN, count.max(1));
            compare(&v, &l, &mut at_least, count, ComparisonType::GreaterThanOrEqual)?;
            let mut at_most = Vector::new(LogicalType::BOOLEAN, count.max(1));
            compare(&v, &u, &mut at_most, count, ComparisonType::LessThanOrEqual)?;
            Ok(conjunction(&[at_least, at_most], true, count))
        }
        BoundExprKind::Case { checks, else_expr } => execute_case(checks, else_expr, &expr.return_type, input),
        BoundExprKind::Subquery {
            subquery,
            child,
            correlated,
        } => execute_subquery(subquery, child.as_deref(), correlated, &expr.return_type, input),
        BoundExprKind::Operator {
            op: BoundOperator::Coalesce,
            children,
        } => execute_coalesce(children, &expr.return_type, input),
        BoundExprKind::Operator {
            op: BoundOperator::Try,
            children,
        } => execute_try(&children[0], &expr.return_type, input),
        BoundExprKind::Operator { op, children } => {
            let child = execute(&children[0], input)?;
            Ok(match op {
                BoundOperator::Not => {
                    let mut result = Vector::new(expr.return_type.clone(), count.max(1));
                    executor::unary::<bool, bool>(&child, &mut result, count, |b| !b);
                    result
                }
                BoundOperator::IsNull => null_check(&child, count, true),
                BoundOperator::IsNotNull => null_check(&child, count, false),
                BoundOperator::Coalesce | BoundOperator::Try => unreachable!("handled above"),
            })
        }
        BoundExprKind::Function { function, children, .. } => {
            let mut arguments = children.iter().map(|c| execute(c, input)).collect::<Result<Vec<_>>>()?;
            // `BinaryNumericDivideWrapper<true>`: a row divided by zero is NULL
            if (function.null_on_zero || function.error_on_zero) && arguments.len() == 2 {
                let divisor = &arguments[1];
                let ty = divisor.logical_type().clone();
                let zero = crate::execution::parquet::try_cast_value(&Value::integer(0), &ty);
                let mut nulled = Vector::new(ty.clone(), count.max(1));
                let mut changed = false;
                for row in 0..count {
                    let value = divisor.get_value(row);
                    let is_zero = zero.as_ref().is_some_and(|zero| {
                        crate::optimizer::statistics::compare_values(
                            duckrs_parser::ast::ComparisonOp::Equal,
                            &value,
                            zero,
                        ) == Some(true)
                    });
                    if is_zero && function.error_on_zero {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "Division by zero in expression {expr}. Use TRY(...) to return NULL for this \
                                 expression, or SET error_on_division_by_zero=false to return NULL for all divisions \
                                 by zero."
                            ),
                        ));
                    }
                    changed |= is_zero;
                    nulled.set_value(row, &if is_zero { Value::null(ty.clone()) } else { value })?;
                }
                if changed {
                    arguments[1] = nulled;
                }
            }
            let arguments = DataChunk::from_columns(arguments, count);
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            (function.function)(&arguments, &mut result).map_err(|error| {
                if error.message == DIVISION_BY_ZERO {
                    let message = format!(
                        "Division by zero in expression {expr}. Use TRY(...) to return NULL for this expression, or \
                         SET error_on_division_by_zero=false to return NULL for all divisions by zero."
                    );
                    Error::new(ErrorKind::InvalidInput, message)
                } else {
                    error
                }
            })?;
            Ok(result)
        }
    }
}

/// `NextValFunction`: moves the sequence once per row, in order. A NULL argument gives NULL and
/// leaves the sequence alone.
fn execute_sequence(
    op: SequenceOp,
    sequence: &SequenceCatalogEntry,
    arguments: &[BoundExpr],
    input: &DataChunk,
) -> Result<Vector> {
    let count = input.len();
    let vectors = arguments
        .iter()
        .map(|a| execute(a, input))
        .collect::<Result<Vec<_>>>()?;
    let mut result = Vector::new(LogicalType::BIGINT, count.max(1));
    for row in 0..count {
        let values: Vec<Value> = vectors.iter().map(|v| v.get_value(row)).collect();
        if values.iter().any(Value::is_null) {
            result.set_value(row, &Value::null(LogicalType::BIGINT))?;
            continue;
        }
        let value = match (op, values.as_slice()) {
            (SequenceOp::Next, _) => sequence.next_value()?,
            (SequenceOp::Current, _) => sequence.current_value()?,
            (SequenceOp::Set, [value]) => sequence.set_value(value.as_i128().unwrap_or(0) as i64, true)?,
            (SequenceOp::Set, [value, is_called]) => {
                sequence.set_value(value.as_i128().unwrap_or(0) as i64, *is_called == Value::boolean(true))?
            }
            (SequenceOp::Set, _) => unreachable!("setval takes one or two arguments after the name"),
        };
        result.set_value(row, &Value::bigint(value))?;
    }
    Ok(result)
}

/// `ExpressionExecutor::Select`: the rows of `input` for which a BOOLEAN expression is true.
pub fn select(expr: &BoundExpr, input: &DataChunk) -> Result<(SelectionVector, usize)> {
    // `SelectAnd`: each conjunct sees only the rows the ones before it kept, so that one that
    // fails on the others (`json_exists(j, ..) AND json_deserialize_sql(j) ..`) does not
    if let BoundExprKind::Conjunction { and: true, children } = &expr.kind
        && children.len() > 1
    {
        let mut remaining: Vec<u32> = (0..input.len() as u32).collect();
        for child in children {
            if remaining.is_empty() {
                break;
            }
            let (selected, count) = if remaining.len() == input.len() {
                select(child, input)?
            } else {
                select(child, &slice_rows(input, &remaining))?
            };
            if count == remaining.len() {
                continue;
            }
            remaining = (0..count).map(|i| remaining[selected.get(i)]).collect();
        }
        let count = remaining.len();
        return Ok((SelectionVector::from_vec(remaining), count));
    }
    if let BoundExprKind::JoinFilter { child, filter } = &expr.kind {
        let values = execute(child, input)?;
        return Ok(match filter.select(&values, input.len()) {
            Some(kept) => {
                let count = kept.len();
                (SelectionVector::from_vec(kept), count)
            }
            None => (
                SelectionVector::from_vec((0..input.len() as u32).collect()),
                input.len(),
            ),
        });
    }
    if let Some(selected) = select_column_constant(expr, input)? {
        return Ok(selected);
    }
    let result = execute(expr, input)?;
    Ok(executor::select::<bool>(&result, input.len(), |b| b))
}

/// The rows for which a column of integers compares as asked with a constant, or is between two:
/// what most filters of a scan are. The comparison is known outside the loop over the rows, and no
/// vector of booleans is made. `None` for anything else.
fn select_column_constant(expr: &BoundExpr, input: &DataChunk) -> Result<Option<(SelectionVector, usize)>> {
    use duckrs_parser::ast::ComparisonOp as C;
    let constant = |e: &BoundExpr| matches!(e.kind, BoundExprKind::Constant(_));
    let (column, op, low, high) = match &expr.kind {
        BoundExprKind::Comparison { op, left, right } => match (&left.kind, &right.kind) {
            (BoundExprKind::Reference(column), BoundExprKind::Constant(_)) => (*column, *op, right.as_ref(), None),
            (BoundExprKind::Constant(_), BoundExprKind::Reference(column)) => {
                let flipped = match op {
                    C::LessThan => C::GreaterThan,
                    C::GreaterThan => C::LessThan,
                    C::LessThanOrEqual => C::GreaterThanOrEqual,
                    C::GreaterThanOrEqual => C::LessThanOrEqual,
                    other => *other,
                };
                (*column, flipped, left.as_ref(), None)
            }
            _ => return Ok(None),
        },
        BoundExprKind::Between {
            input: value,
            lower,
            upper,
        } => match value.kind {
            BoundExprKind::Reference(column) if constant(lower) && constant(upper) => {
                (column, C::GreaterThanOrEqual, lower.as_ref(), Some(upper.as_ref()))
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let values = input.column(column);
    let count = input.len();
    let low = execute(low, input)?;
    let high = high.map(|high| execute(high, input)).transpose()?;
    let same_type = |v: &Vector| v.logical_type() == values.logical_type();
    if !same_type(&low) || !high.as_ref().is_none_or(same_type) {
        return Ok(None);
    }
    fn run<T: NativeType + PartialOrd>(
        values: &Vector,
        count: usize,
        op: C,
        low: &Vector,
        high: Option<&Vector>,
    ) -> Option<(SelectionVector, usize)> {
        let constant = |v: &Vector| {
            let unified = v.to_unified(1);
            let at = unified.sel.get(0);
            unified.validity.is_valid(at).then(|| unified.data::<T>()[at])
        };
        let low = constant(low)?;
        if let Some(high) = high {
            let high = constant(high)?;
            return Some(executor::select::<T>(values, count, |a| a >= low && a <= high));
        }
        Some(match op {
            C::Equal => executor::select::<T>(values, count, |a| a == low),
            C::NotEqual => executor::select::<T>(values, count, |a| a != low),
            C::LessThan => executor::select::<T>(values, count, |a| a < low),
            C::LessThanOrEqual => executor::select::<T>(values, count, |a| a <= low),
            C::GreaterThan => executor::select::<T>(values, count, |a| a > low),
            C::GreaterThanOrEqual => executor::select::<T>(values, count, |a| a >= low),
            _ => return None,
        })
    }
    let high = high.as_ref();
    Ok(match values.physical_type() {
        PhysicalType::Int8 => run::<i8>(values, count, op, &low, high),
        PhysicalType::Int16 => run::<i16>(values, count, op, &low, high),
        PhysicalType::Int32 => run::<i32>(values, count, op, &low, high),
        PhysicalType::Int64 => run::<i64>(values, count, op, &low, high),
        PhysicalType::Int128 => run::<i128>(values, count, op, &low, high),
        PhysicalType::UInt8 => run::<u8>(values, count, op, &low, high),
        PhysicalType::UInt16 => run::<u16>(values, count, op, &low, high),
        PhysicalType::UInt32 => run::<u32>(values, count, op, &low, high),
        PhysicalType::UInt64 => run::<u64>(values, count, op, &low, high),
        _ => None,
    })
}

/// `ExpressionExecutor::TryEvaluateScalar`: the value of an expression without columns, or
/// `None` if evaluating it fails.
pub fn try_evaluate_scalar(expr: &BoundExpr) -> Option<Value> {
    evaluate_scalar(expr).ok()
}

/// `ExpressionExecutor::EvaluateScalar`.
pub fn evaluate_scalar(expr: &BoundExpr) -> Result<Value> {
    let input = DataChunk::from_columns(Vec::new(), 1);
    let result = execute(expr, &input)?;
    Ok(result.get_value(0))
}
