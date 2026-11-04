// Ported from duckdb/extension/core_functions/scalar/list/{array_slice,list_sort}.cpp,
// duckdb/extension/core_functions/scalar/map/{map_keys_values,map_entries,map_from_entries,
// cardinality}.cpp, duckdb/extension/core_functions/scalar/string/{repeat,hex}.cpp,
// duckdb/extension/core_functions/scalar/date/to_interval.cpp and
// duckdb/src/function/scalar/string/{length,concat,concat_ws,like,caseconvert}.cpp
// @ rust-port-baseline
//
//! The helpers that the type tests lean on, ahead of the rest of Phase 5's function library.
//! They run row by row over values.

use super::bit_functions::strict_rows;
use super::combine_types::{force_max_logical_type, normalize_type};
use super::dates::{bind_date_function, is_date_function};
use super::nested::{bound_function, row_wise};
use super::strings::format_callback;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use crate::planner::binder::range_parameters;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::hash::{combine_hash, hash_vector};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys};
use duckrs_common::types::{Interval, LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use parking_lot::Mutex;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn no_match(name: &str, children: &[BoundExpr]) -> Error {
    let types: Vec<String> = children.iter().map(|c| c.return_type.to_string()).collect();
    binder_error(format!(
        "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
         casts.",
        types.join(", ")
    ))
}

fn text_of(value: &Value) -> String {
    match value.data() {
        ValueData::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => value.try_to_string().unwrap_or_default(),
    }
}

fn is_list(ty: &LogicalType) -> bool {
    matches!(ty.id(), LogicalTypeId::List | LogicalTypeId::Array)
}

fn items_of(value: &Value) -> &[Value] {
    match value.data() {
        ValueData::List(items) => items,
        _ => &[],
    }
}

/// The value at the end of an ARRAY-to-LIST cast.
fn as_list(child: BoundExpr) -> BoundExpr {
    if child.return_type.id() == LogicalTypeId::Array {
        let list = LogicalType::list(child.return_type.child_type().expect("a child type").clone());
        child.cast_to(&list, false)
    } else {
        child
    }
}

/// `ClampIndex`: a position from the end becomes one from the start, inside of the value.
fn clamp_index(index: i64, length: i64, is_min: bool) -> i64 {
    if index < 0 {
        let index = if is_min { index } else { index + 1 };
        return (length + index).max(0);
    }
    index.min(length)
}

/// The bound of a slice: `None` if it was left out.
fn slice_bound(value: &Value) -> Option<i64> {
    value
        .as_i128()
        .map(|v| v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// The 0-based `[begin, end)` of a slice of `length`, or `None` if it is empty.
fn clamp_slice(begin: Option<i64>, end: Option<i64>, length: i64) -> (i64, i64) {
    let begin = begin.map_or(0, |b| if b > 0 { b - 1 } else { b });
    let (begin, end) = (
        clamp_index(begin, length, true),
        clamp_index(end.unwrap_or(length), length, false),
    );
    (begin, end.max(begin))
}

fn array_slice(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        // a bound that was left out is an empty list, and a NULL bound makes the result NULL
        if row.iter().any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let step = row.get(3).and_then(|s| s.as_i128()).unwrap_or(1) as i64;
        if step == 0 {
            return Err(invalid_input("Slice step cannot be zero"));
        }
        if ty.id() == LogicalTypeId::List {
            let items = items_of(&row[0]);
            // a negative step walks from `begin` down to `end`
            let (lower, upper) = (slice_bound(&row[1]), slice_bound(&row[2]));
            let (lower, upper) = if step < 0 { (upper, lower) } else { (lower, upper) };
            let (begin, end) = clamp_slice(lower, upper, items.len() as i64);
            let (begin, end) = (begin as usize, end as usize);
            let mut picked: Vec<Value> = items[begin..end].to_vec();
            if step < 0 {
                picked.reverse();
            }
            let picked = picked.into_iter().step_by(step.unsigned_abs() as usize).collect();
            return Value::new(ty.clone(), ValueData::List(picked));
        }
        if step != 1 {
            return Err(Error::new(
                ErrorKind::Internal,
                "Slicing with steps is not supported for strings",
            ));
        }
        let characters: Vec<char> = text_of(&row[0]).chars().collect();
        let (begin, end) = clamp_slice(slice_bound(&row[1]), slice_bound(&row[2]), characters.len() as i64);
        Ok(Value::varchar(
            characters[begin as usize..end as usize].iter().collect::<String>(),
        ))
    })
}

fn bind_array_slice(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if !(3..=4).contains(&children.len()) {
        return Err(no_match("array_slice", &children));
    }
    let mut children = children.into_iter();
    let input = as_list(children.next().expect("checked"));
    let ty = input.return_type.clone();
    let return_type = match ty.id() {
        LogicalTypeId::List => ty,
        LogicalTypeId::Varchar => LogicalType::VARCHAR,
        LogicalTypeId::SqlNull => return Ok(BoundExpr::constant(Value::sql_null())),
        _ => return Err(binder_error("ARRAY_SLICE can only operate on LISTs and VARCHARs")),
    };
    let mut arguments = vec![input];
    for bound in children {
        // an empty list stands for a bound that was left out
        if is_list(&bound.return_type) {
            arguments.push(bound);
        } else if bound.return_type.is_numeric() || bound.return_type.id() == LogicalTypeId::SqlNull {
            arguments.push(bound.cast_to(&LogicalType::BIGINT, false));
        } else {
            return Err(binder_error("The upper and lower bounds of the slice must be a BIGINT"));
        }
    }
    Ok(bound_function("array_slice", arguments, return_type, array_slice))
}

fn value_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::bigint(match row[0].data() {
            ValueData::List(items) => items.len() as i64,
            ValueData::Bytes(_) => text_of(&row[0]).chars().count() as i64,
            _ => 0,
        }))
    })
}

fn concat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let is_blob = result.logical_type().id() == LogicalTypeId::Blob;
    row_wise(args, result, |row| {
        let mut out = Vec::new();
        for value in row.iter().filter(|v| !v.is_null()) {
            match value.data() {
                ValueData::Bytes(bytes) if is_blob || value.logical_type().id() == LogicalTypeId::Varchar => {
                    out.extend_from_slice(bytes)
                }
                _ => out.extend_from_slice(value.try_to_string()?.as_bytes()),
            }
        }
        Ok(if is_blob {
            Value::blob(out)
        } else {
            Value::varchar(String::from_utf8_lossy(&out).into_owned())
        })
    })
}

fn concat_ws(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let parts = row[1..]
            .iter()
            .flat_map(|v| match v.data() {
                ValueData::List(items) => items.as_slice(),
                _ => std::slice::from_ref(v),
            })
            .filter(|v| !v.is_null())
            .map(Value::try_to_string)
            .collect::<Result<Vec<_>>>()?;
        Ok(Value::varchar(parts.join(&text_of(&row[0]))))
    })
}

fn list_concat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().all(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        Value::new(
            ty.clone(),
            ValueData::List(row.iter().flat_map(|v| items_of(v).to_vec()).collect()),
        )
    })
}

fn array_to_string(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let parts = items_of(&row[0])
            .iter()
            .filter(|v| !v.is_null())
            .map(Value::try_to_string)
            .collect::<Result<Vec<_>>>()?;
        Ok(Value::varchar(parts.join(&text_of(&row[1]))))
    })
}

fn list_sort(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let child = ty.child_type().expect("a child type").clone();
    strict_rows(args, result, |row| {
        let option = |index: usize, default: &str| row.get(index).map_or(default.to_string(), text_of).to_uppercase();
        let modifiers = OrderModifiers {
            order: if option(1, "ASC") == "DESC" {
                OrderType::Descending
            } else {
                OrderType::Ascending
            },
            nulls: if option(2, "NULLS FIRST") == "NULLS LAST" {
                NullOrder::NullsLast
            } else {
                NullOrder::NullsFirst
            },
        };
        let items = items_of(&row[0]);
        let mut column = Vector::new(child.clone(), items.len().max(1));
        for (i, item) in items.iter().enumerate() {
            column.set_value(i, item)?;
        }
        let keys = create_sort_keys(&[(&column, modifiers)], items.len())?;
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
        Value::new(
            ty.clone(),
            ValueData::List(order.into_iter().map(|i| items[i].clone()).collect()),
        )
    })
}

fn map_part(entry: &Value, index: usize) -> Value {
    match entry.data() {
        ValueData::Struct(fields) => fields[index].clone(),
        _ => unreachable!("a map entry is a STRUCT"),
    }
}

fn map_keys(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let keys = items_of(&row[0]).iter().map(|e| map_part(e, 0)).collect();
        Value::new(ty.clone(), ValueData::List(keys))
    })
}

fn map_values(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let values = items_of(&row[0]).iter().map(|e| map_part(e, 1)).collect();
        Value::new(ty.clone(), ValueData::List(values))
    })
}

fn map_entries(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        Value::new(ty.clone(), ValueData::List(items_of(&row[0]).to_vec()))
    })
}

fn map_from_entries(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let entry = ty.child_type().expect("an entry type").clone();
    strict_rows(args, result, |row| {
        let mut entries: Vec<Value> = Vec::with_capacity(items_of(&row[0]).len());
        for item in items_of(&row[0]) {
            if item.is_null() {
                return Err(invalid_input("Map keys can not be NULL."));
            }
            let ValueData::Struct(fields) = item.data() else {
                unreachable!("cast to the entry type");
            };
            if fields[0].is_null() {
                return Err(invalid_input("Map keys can not be NULL."));
            }
            if entries.iter().any(|e| map_part(e, 0) == fields[0]) {
                return Err(invalid_input("Map keys must be unique."));
            }
            entries.push(Value::new(entry.clone(), ValueData::Struct(fields.clone()))?);
        }
        Value::new(ty.clone(), ValueData::List(entries))
    })
}

/// The NULL a map function gives for an untyped NULL.
fn null_map_argument(children: &[BoundExpr]) -> bool {
    matches!(children, [child] if child.return_type.id() == LogicalTypeId::SqlNull)
}

fn map_type_of(child: &BoundExpr, name: &str) -> Result<(LogicalType, LogicalType)> {
    if child.return_type.id() != LogicalTypeId::Map {
        return Err(no_match(name, std::slice::from_ref(child)));
    }
    let fields = child.return_type.child_type().expect("an entry type").struct_children();
    Ok((fields[0].1.clone(), fields[1].1.clone()))
}

fn bind_map_from_entries(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let [input] = children.as_slice() else {
        return Err(no_match("map_from_entries", &children));
    };
    let entry = input.return_type.child_type().filter(|_| is_list(&input.return_type));
    let fields = entry.map(LogicalType::struct_children).unwrap_or(&[]);
    let [(_, key), (_, value)] = fields else {
        return Err(binder_error(
            "The provided argument is not a list of key-value pairs: expected STRUCT(key, value)[]",
        ));
    };
    let map = LogicalType::map(key.clone(), value.clone());
    let input = as_list(children.into_iter().next().expect("one child"));
    Ok(bound_function("map_from_entries", vec![input], map, map_from_entries))
}

fn hash_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let count = args.len();
    let mut hashes = hash_vector(args.column(0), count)?;
    for column in &args.columns()[1..] {
        combine_hash(&mut hashes, column, count)?;
    }
    *result = hashes;
    Ok(())
}

fn repeat_list(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let times = row[1].as_i128().unwrap_or(0).max(0) as usize;
        let items = items_of(&row[0]);
        let mut out = Vec::with_capacity(items.len() * times);
        for _ in 0..times {
            out.extend_from_slice(items);
        }
        Value::new(ty.clone(), ValueData::List(out))
    })
}

fn chr(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let code = row[0].as_i128().expect("an INTEGER");
        let character = u32::try_from(code).ok().and_then(char::from_u32);
        let character =
            character.ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("Invalid UTF8 Codepoint {code}")))?;
        Ok(Value::varchar(character.to_string()))
    })
}

fn contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::boolean(
            row[0].try_to_string()?.contains(&row[1].try_to_string()?),
        ))
    })
}

/// What a pad can ask for (`NumericLimits<int32_t>::Maximum` in characters, 4 bytes each, fits
/// a string).
const MAX_PAD_LENGTH: i128 = (u32::MAX / 4) as i128;

/// `PadFunction`: `text` cut or filled up to `length` characters.
fn pad(row: &[Value], left: bool) -> Result<Value> {
    let text: Vec<char> = row[0].try_to_string()?.chars().collect();
    let length = row[1].as_i128().expect("an INTEGER");
    if length > MAX_PAD_LENGTH {
        return Err(Error::new(
            ErrorKind::OutOfRange,
            format!("Padding length {length} is larger than the maximum of {MAX_PAD_LENGTH}"),
        ));
    }
    let length = length.max(0) as usize;
    if length <= text.len() {
        return Ok(Value::varchar(text[..length].iter().collect::<String>()));
    }
    let fill: Vec<char> = row[2].try_to_string()?.chars().collect();
    if fill.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            if left {
                "Insufficient padding in LPAD."
            } else {
                "Insufficient padding in RPAD."
            },
        ));
    }
    let padding = fill.iter().cycle().take(length - text.len());
    Ok(Value::varchar(if left {
        padding.chain(text.iter()).collect::<String>()
    } else {
        text.iter().chain(padding).collect::<String>()
    }))
}

fn lpad(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| pad(row, true))
}

fn rpad(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| pad(row, false))
}

/// `LeastGreatestFunction`: NULLs don't count, unless there is nothing else.
fn extreme(args: &DataChunk, result: &mut Vector, greatest: bool) -> Result<()> {
    let modifiers = OrderModifiers::new(OrderType::Ascending, NullOrder::NullsLast);
    let keys = args
        .columns()
        .iter()
        .map(|column| create_sort_keys(&[(column, modifiers)], args.len()))
        .collect::<Result<Vec<_>>>()?;
    for row in 0..args.len() {
        let mut best: Option<usize> = None;
        for (column, column_keys) in keys.iter().enumerate() {
            if args.column(column).get_value(row).is_null() {
                continue;
            }
            let wins = best.is_none_or(|current| {
                let (key, other) = (&column_keys[row], &keys[current][row]);
                if greatest { key > other } else { key < other }
            });
            if wins {
                best = Some(column);
            }
        }
        let value = match best {
            Some(column) => args.column(column).get_value(row),
            None => Value::null(result.logical_type().clone()),
        };
        result.set_value(row, &value)?;
    }
    Ok(())
}

fn greatest(args: &DataChunk, result: &mut Vector) -> Result<()> {
    extreme(args, result, true)
}

fn least(args: &DataChunk, result: &mut Vector) -> Result<()> {
    extreme(args, result, false)
}

fn signbit(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::Double(value) = row[0].data() else {
            unreachable!("cast to DOUBLE");
        };
        Ok(Value::boolean(value.is_sign_negative()))
    })
}

/// The state of `random`, shared by the connections. C++ has one per connection; a test that
/// relies on the numbers of a seed needs its pcg32, which isn't ported.
static RANDOM_STATE: Mutex<u64> = Mutex::new(0x9E37_79B9_7F4A_7C15);

fn next_random() -> u64 {
    // xorshift64*
    let mut state = RANDOM_STATE.lock();
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn random(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        let value = (next_random() >> 11) as f64 / (1u64 << 53) as f64;
        result.set_value(row, &Value::double(value))?;
    }
    Ok(())
}

fn setseed(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        let value = args.column(0).get_value(row);
        let ValueData::Double(seed) = value.data() else {
            continue;
        };
        if !(-1.0..=1.0).contains(seed) {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                "SETSEED accepts seed values between -1.0 and 1.0, inclusive",
            ));
        }
        // xorshift never leaves zero
        *RANDOM_STATE.lock() = seed.to_bits() | 1;
    }
    let _ = result;
    Ok(())
}

fn uuid(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        let high = u128::from(next_random());
        let bits = (high << 64 | u128::from(next_random())) as i128;
        // version 4, variant 1
        let bits = bits & !(0xF000_i128 << 64) | (0x4000_i128 << 64);
        let bits = bits & !(0xC000_i128 << 48) | (0x8000_i128 << 48);
        result.set_value(row, &Value::uuid(bits))?;
    }
    Ok(())
}

/// `ListRangeFunction`: the numbers a `range` of the same arguments gives as rows.
fn range_list(args: &DataChunk, result: &mut Vector, generate_series: bool) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let values: Vec<i128> = row.iter().filter_map(Value::as_i128).collect();
        let (start, count, increment) = range_parameters(&values, generate_series)?;
        if count >= 1 << 32 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Lists larger than 2^32 elements are not supported",
            ));
        }
        let items = (0..i128::from(count))
            .map(|i| Value::bigint((start + i * increment) as i64))
            .collect();
        Value::new(ty.clone(), ValueData::List(items))
    })
}

fn list_range(args: &DataChunk, result: &mut Vector) -> Result<()> {
    range_list(args, result, false)
}

fn list_generate_series(args: &DataChunk, result: &mut Vector) -> Result<()> {
    range_list(args, result, true)
}

/// Whether `name` is the `unnest` of a select list.
pub fn is_unnest(name: &str) -> bool {
    name.eq_ignore_ascii_case("unnest")
}

/// Whether `expr` is an unnest the select node has yet to take out of its expressions.
pub fn is_unnest_call(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Function { function, .. } if function.name == "unnest")
}

fn unnest_outside_of_select(_: &DataChunk, _: &mut Vector) -> Result<()> {
    Err(binder_error("UNNEST not supported here"))
}

/// `BindUnnest`: the select node turns this into a `LogicalUnnest`.
pub fn bind_unnest(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let [child] = children.as_slice() else {
        return Err(binder_error("UNNEST() requires a single argument"));
    };
    let element = match child.return_type.id() {
        LogicalTypeId::List | LogicalTypeId::Array => child.return_type.child_type().expect("a child type").clone(),
        LogicalTypeId::SqlNull => LogicalType::SQLNULL,
        _ => return Err(binder_error("UNNEST() can only be applied to lists, structs and NULL")),
    };
    Ok(bound_function("unnest", children, element, unnest_outside_of_select))
}

/// Whether [`bind_generic_function`] binds the function `name`.
pub fn is_generic_function(name: &str) -> bool {
    is_date_function(name)
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "array_slice"
                | "list_slice"
                | "len"
                | "array_length"
                | "cardinality"
                | "concat"
                | "concat_ws"
                | "list_concat"
                | "array_concat"
                | "list_cat"
                | "array_cat"
                | "array_to_string"
                | "list_sort"
                | "array_sort"
                | "map_keys"
                | "map_values"
                | "map_entries"
                | "map_from_entries"
                | "hash"
                | "repeat"
                | "range"
                | "generate_series"
                | "length"
                | "chr"
                | "contains"
                | "lpad"
                | "rpad"
                | "greatest"
                | "least"
                | "signbit"
                | "random"
                | "setseed"
                | "uuid"
                | "gen_random_uuid"
                | "printf"
                | "format"
        )
}

/// Binds a function of [`is_generic_function`].
pub fn bind_generic_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if is_date_function(name) {
        return bind_date_function(name, children);
    }
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("map_") && null_map_argument(&children)
        || lower == "cardinality" && null_map_argument(&children)
    {
        return Ok(BoundExpr::constant(Value::sql_null()));
    }
    let single = |children: &[BoundExpr]| match children {
        [child] => Ok(child.clone()),
        other => Err(no_match(&lower, other)),
    };
    match lower.as_str() {
        "array_slice" | "list_slice" => bind_array_slice(children),
        "len" | "array_length" | "length" => {
            let child = as_list(single(&children)?);
            let id = child.return_type.id();
            if !matches!(
                id,
                LogicalTypeId::List | LogicalTypeId::Varchar | LogicalTypeId::SqlNull
            ) {
                return Err(no_match(&lower, &children));
            }
            Ok(bound_function(&lower, vec![child], LogicalType::BIGINT, value_length))
        }
        "cardinality" => {
            let child = single(&children)?;
            map_type_of(&child, &lower)?;
            Ok(bound_function(&lower, vec![child], LogicalType::UBIGINT, cardinality))
        }
        "concat" => {
            let all_blob = !children.is_empty() && children.iter().all(|c| c.return_type.id() == LogicalTypeId::Blob);
            let ty = if all_blob {
                LogicalType::BLOB
            } else {
                LogicalType::VARCHAR
            };
            Ok(bound_function("concat", children, ty, concat))
        }
        "concat_ws" => {
            if children.len() < 2 {
                return Err(binder_error("concat_ws requires at least two arguments"));
            }
            let mut children = children;
            let separator = children.remove(0).cast_to(&LogicalType::VARCHAR, false);
            children.insert(0, separator);
            Ok(bound_function("concat_ws", children, LogicalType::VARCHAR, concat_ws))
        }
        "list_concat" | "array_concat" | "list_cat" | "array_cat" => {
            let mut element = LogicalType::SQLNULL;
            for child in &children {
                if is_list(&child.return_type) {
                    let child_type = child.return_type.child_type().expect("a child type");
                    element = super::combine_types::force_max_logical_type(&element, child_type);
                } else if child.return_type.id() != LogicalTypeId::SqlNull {
                    return Err(no_match(&lower, &children));
                }
            }
            let list = LogicalType::list(element);
            let children = children.into_iter().map(|c| c.cast_to(&list, false)).collect();
            Ok(bound_function(&lower, children, list, list_concat))
        }
        "array_to_string" => {
            if children.len() != 2 || !is_list(&children[0].return_type) {
                return Err(no_match(&lower, &children));
            }
            let mut children = children.into_iter();
            let list = as_list(children.next().expect("checked"));
            let separator = children.next().expect("checked").cast_to(&LogicalType::VARCHAR, false);
            Ok(bound_function(
                &lower,
                vec![list, separator],
                LogicalType::VARCHAR,
                array_to_string,
            ))
        }
        "list_sort" | "array_sort" => {
            let Some(first) = children.first().filter(|_| children.len() <= 3).cloned() else {
                return Err(no_match(&lower, &children));
            };
            let child = as_list(first);
            if child.return_type.id() != LogicalTypeId::List {
                return Err(no_match(&lower, &children));
            }
            let ty = child.return_type.clone();
            let mut arguments = vec![child];
            arguments.extend(
                children[1..]
                    .iter()
                    .map(|c| c.clone().cast_to(&LogicalType::VARCHAR, false)),
            );
            Ok(bound_function(&lower, arguments, ty, list_sort))
        }
        "map_keys" => {
            let child = single(&children)?;
            let (key, _) = map_type_of(&child, &lower)?;
            Ok(bound_function(&lower, vec![child], LogicalType::list(key), map_keys))
        }
        "map_values" => {
            let child = single(&children)?;
            let (_, value) = map_type_of(&child, &lower)?;
            Ok(bound_function(
                &lower,
                vec![child],
                LogicalType::list(value),
                map_values,
            ))
        }
        "map_entries" => {
            let child = single(&children)?;
            map_type_of(&child, &lower)?;
            let entry = child.return_type.child_type().expect("an entry type").clone();
            Ok(bound_function(
                &lower,
                vec![child],
                LogicalType::list(entry),
                map_entries,
            ))
        }
        "map_from_entries" => bind_map_from_entries(children),
        "hash" => {
            if children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            Ok(bound_function("hash", children, LogicalType::UBIGINT, hash_function))
        }
        "repeat" => {
            let [input, times] = children.as_slice() else {
                return Err(no_match("repeat", &children));
            };
            let times = times.clone().cast_to(&LogicalType::BIGINT, false);
            if input.return_type.id() == LogicalTypeId::List {
                let ty = input.return_type.clone();
                return Ok(bound_function("repeat", vec![input.clone(), times], ty, repeat_list));
            }
            let ty = if input.return_type.id() == LogicalTypeId::Blob {
                LogicalType::BLOB
            } else {
                LogicalType::VARCHAR
            };
            let input = input.clone().cast_to(&ty, false);
            Ok(bound_function("repeat", vec![input, times], ty, repeat))
        }
        "range" | "generate_series" => {
            if children.is_empty() || children.len() > 3 {
                return Err(no_match(&lower, &children));
            }
            let children = children
                .iter()
                .map(|c| c.clone().cast_to(&LogicalType::BIGINT, false))
                .collect();
            let list = LogicalType::list(LogicalType::BIGINT);
            if lower == "range" {
                Ok(bound_function("range", children, list, list_range))
            } else {
                Ok(bound_function("generate_series", children, list, list_generate_series))
            }
        }
        "chr" => {
            let child = single(&children)?.cast_to(&LogicalType::INTEGER, false);
            Ok(bound_function("chr", vec![child], LogicalType::VARCHAR, chr))
        }
        "contains" => {
            let [text, needle] = children.as_slice() else {
                return Err(no_match("contains", &children));
            };
            if !matches!(
                text.return_type.id(),
                LogicalTypeId::Varchar | LogicalTypeId::StringLiteral | LogicalTypeId::SqlNull
            ) {
                return Err(no_match("contains", &children));
            }
            let args = vec![
                text.clone().cast_to(&LogicalType::VARCHAR, false),
                needle.clone().cast_to(&LogicalType::VARCHAR, false),
            ];
            Ok(bound_function("contains", args, LogicalType::BOOLEAN, contains))
        }
        "lpad" | "rpad" => {
            let [text, length, fill] = children.as_slice() else {
                return Err(no_match(&lower, &children));
            };
            let args = vec![
                text.clone().cast_to(&LogicalType::VARCHAR, false),
                length.clone().cast_to(&LogicalType::INTEGER, false),
                fill.clone().cast_to(&LogicalType::VARCHAR, false),
            ];
            let callback: ScalarCallback = if lower == "lpad" { lpad } else { rpad };
            Ok(bound_function(&lower, args, LogicalType::VARCHAR, callback))
        }
        "greatest" | "least" => {
            if children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            let mut ty = children[0].return_type.clone();
            for child in &children[1..] {
                ty = force_max_logical_type(&ty, &child.return_type);
            }
            let ty = normalize_type(&ty);
            let children = children.into_iter().map(|c| c.cast_to(&ty, false)).collect();
            let callback: ScalarCallback = if lower == "greatest" { greatest } else { least };
            Ok(bound_function(&lower, children, ty, callback))
        }
        "signbit" => {
            let child = single(&children)?.cast_to(&LogicalType::DOUBLE, false);
            Ok(bound_function("signbit", vec![child], LogicalType::BOOLEAN, signbit))
        }
        "random" => {
            if !children.is_empty() {
                return Err(no_match("random", &children));
            }
            Ok(bound_function("random", children, LogicalType::DOUBLE, random))
        }
        "setseed" => {
            let child = single(&children)?.cast_to(&LogicalType::DOUBLE, false);
            Ok(bound_function("setseed", vec![child], LogicalType::SQLNULL, setseed))
        }
        "uuid" | "gen_random_uuid" => {
            if !children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            Ok(bound_function(&lower, children, LogicalType::UUID, uuid))
        }
        "printf" | "format" => {
            if children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            let mut children = children;
            children[0] = children[0].clone().cast_to(&LogicalType::VARCHAR, false);
            Ok(bound_function(
                &lower,
                children,
                LogicalType::VARCHAR,
                format_callback(&lower),
            ))
        }
        other => unreachable!("{other} is not a generic function"),
    }
}

fn cardinality(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::ubigint(items_of(&row[0]).len() as u64)))
}

fn upper(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::varchar(text_of(&row[0]).to_uppercase())))
}

fn lower(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::varchar(text_of(&row[0]).to_lowercase())))
}

/// The bytes of a string, where `length` counts its characters.
fn strlen(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| match row[0].data() {
        ValueData::Bytes(bytes) => Ok(Value::bigint(bytes.len() as i64)),
        _ => unreachable!("a string"),
    })
}

fn repeat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let times = row[1].as_i128().unwrap_or(0).max(0) as usize;
        let ValueData::Bytes(bytes) = row[0].data() else {
            unreachable!("a string");
        };
        Value::new(ty.clone(), ValueData::Bytes(bytes.repeat(times)))
    })
}

fn octet_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| match row[0].data() {
        ValueData::Bytes(bytes) => Ok(Value::bigint(bytes.len() as i64)),
        _ => unreachable!("bytes"),
    })
}

/// `-interval`.
fn interval_negate(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::Interval(v) = row[0].data() else {
            unreachable!("an INTERVAL");
        };
        let negated = (v.months.checked_neg(), v.days.checked_neg(), v.micros.checked_neg());
        let (Some(months), Some(days), Some(micros)) = negated else {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                format!("Overflow in negation of interval - interval value {v} is out of range"),
            ));
        };
        Ok(Value::interval(Interval::new(months, days, micros)))
    })
}

fn double_of(value: &Value) -> f64 {
    match value.data() {
        ValueData::Double(v) => *v,
        _ => unreachable!("a DOUBLE"),
    }
}

fn trunc(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::double(double_of(&row[0]).trunc())))
}

fn floor(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::double(double_of(&row[0]).floor())))
}

fn ceil(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::double(double_of(&row[0]).ceil())))
}

fn round(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::double(double_of(&row[0]).round())))
}

/// The overload of `-` for an INTERVAL.
pub(super) fn interval_negation() -> ScalarFunction {
    ScalarFunction::new("-", vec![LogicalType::INTERVAL], LogicalType::INTERVAL, interval_negate)
}

/// `LikeOperatorFunction`: `%` is any characters, `_` is one, and `escape` quotes the next one.
fn like_matches(text: &[char], pattern: &[char], escape: Option<char>) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    if Some(first) == escape {
        return match rest.split_first() {
            Some((&literal, rest)) => text.first() == Some(&literal) && like_matches(&text[1..], rest, escape),
            None => false,
        };
    }
    match first {
        '%' => (0..=text.len()).any(|skip| like_matches(&text[skip..], rest, escape)),
        '_' => !text.is_empty() && like_matches(&text[1..], rest, escape),
        c => text.first() == Some(&c) && like_matches(&text[1..], rest, escape),
    }
}

fn like_row(row: &[Value], ignore_case: bool) -> bool {
    let fold = |value: &Value| {
        let text = text_of(value);
        if ignore_case { text.to_lowercase() } else { text }
    };
    let (text, pattern): (Vec<char>, Vec<char>) = (fold(&row[0]).chars().collect(), fold(&row[1]).chars().collect());
    like_matches(&text, &pattern, None)
}

fn like(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(like_row(row, false))))
}

fn not_like(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(!like_row(row, false))))
}

fn ilike(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(like_row(row, true))))
}

fn not_ilike(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(!like_row(row, true))))
}

fn sign(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let number = row[0]
            .as_i128()
            .map(|v| v.signum() as f64)
            .unwrap_or_else(|| match row[0].data() {
                ValueData::Double(v) if v.is_nan() => 0.0,
                ValueData::Double(v) if *v == 0.0 => 0.0,
                ValueData::Double(v) => v.signum(),
                _ => 0.0,
            });
        Ok(Value::tinyint(number as i8))
    })
}

fn interval_out_of_range() -> Error {
    Error::new(ErrorKind::OutOfRange, "Interval value out of range")
}

/// `to_years` and the like, for a unit of `months`, `days` or `micros`.
fn to_interval(args: &DataChunk, result: &mut Vector, months: i64, days: i64, micros: i64) -> Result<()> {
    strict_rows(args, result, |row| {
        let number = |factor: i64| -> Result<i64> {
            match row[0].data() {
                ValueData::Double(v) => {
                    let scaled = (v * factor as f64).round();
                    if scaled.is_finite() && scaled.abs() < i64::MAX as f64 {
                        Ok(scaled as i64)
                    } else {
                        Err(interval_out_of_range())
                    }
                }
                _ => {
                    let value = row[0].as_i128().expect("an integer argument") * factor as i128;
                    i64::try_from(value).map_err(|_| interval_out_of_range())
                }
            }
        };
        let (m, d, us) = (number(months)?, number(days)?, number(micros)?);
        let narrow = |v: i64| i32::try_from(v).map_err(|_| interval_out_of_range());
        Ok(Value::interval(Interval::new(narrow(m)?, narrow(d)?, us)))
    })
}

macro_rules! interval_function {
    ($($name:ident: $months:expr, $days:expr, $micros:expr;)*) => {$(
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            to_interval(args, result, $months, $days, $micros)
        }
    )*};
}

interval_function! {
    to_millennia: 12_000, 0, 0;
    to_centuries: 1_200, 0, 0;
    to_decades: 120, 0, 0;
    to_years: 12, 0, 0;
    to_quarters: 3, 0, 0;
    to_months: 1, 0, 0;
    to_weeks: 0, 7, 0;
    to_days: 0, 1, 0;
    to_hours: 0, 0, 3_600_000_000;
    to_minutes: 0, 0, 60_000_000;
    to_seconds: 0, 0, 1_000_000;
    to_milliseconds: 0, 0, 1_000;
    to_microseconds: 0, 0, 1;
}

/// The functions of a plain signature.
pub(super) fn generic_functions() -> Vec<ScalarFunctionSet> {
    let (varchar, boolean, bigint, integer, double, interval) = (
        LogicalType::VARCHAR,
        LogicalType::BOOLEAN,
        LogicalType::BIGINT,
        LogicalType::INTEGER,
        LogicalType::DOUBLE,
        LogicalType::INTERVAL,
    );
    let one = |name: &str, argument: &LogicalType, ret: &LogicalType, f: ScalarCallback| {
        (name.to_string(), vec![argument.clone()], ret.clone(), f)
    };
    let two = |name: &str, f: ScalarCallback| {
        (
            name.to_string(),
            vec![varchar.clone(), varchar.clone()],
            boolean.clone(),
            f,
        )
    };
    let mut definitions = vec![
        one("upper", &varchar, &varchar, upper),
        one("ucase", &varchar, &varchar, upper),
        one("lower", &varchar, &varchar, lower),
        one("lcase", &varchar, &varchar, lower),
        one("strlen", &varchar, &bigint, strlen),
        one("octet_length", &LogicalType::BLOB, &bigint, octet_length),
        one("trunc", &double, &double, trunc),
        one("floor", &double, &double, floor),
        one("ceil", &double, &double, ceil),
        one("ceiling", &double, &double, ceil),
        one("round", &double, &double, round),
        two("~~", like),
        two("!~~", not_like),
        two("~~*", ilike),
        two("!~~*", not_ilike),
        one("to_millennia", &integer, &interval, to_millennia),
        one("to_centuries", &integer, &interval, to_centuries),
        one("to_decades", &integer, &interval, to_decades),
        one("to_years", &integer, &interval, to_years),
        one("to_quarters", &integer, &interval, to_quarters),
        one("to_months", &integer, &interval, to_months),
        one("to_weeks", &integer, &interval, to_weeks),
        one("to_days", &integer, &interval, to_days),
        one("to_hours", &bigint, &interval, to_hours),
        one("to_minutes", &bigint, &interval, to_minutes),
        one("to_seconds", &double, &interval, to_seconds),
        one("to_milliseconds", &double, &interval, to_milliseconds),
        one("to_microseconds", &bigint, &interval, to_microseconds),
    ];
    for ty in [
        LogicalType::TINYINT,
        LogicalType::SMALLINT,
        LogicalType::INTEGER,
        LogicalType::BIGINT,
        LogicalType::HUGEINT,
        LogicalType::DOUBLE,
    ] {
        definitions.push(one("sign", &ty, &LogicalType::TINYINT, sign));
    }
    let mut sets: Vec<ScalarFunctionSet> = Vec::new();
    for (name, arguments, return_type, callback) in definitions {
        if sets.last().is_none_or(|set| set.name != name) {
            sets.push(ScalarFunctionSet::new(name.clone()));
        }
        sets.last_mut()
            .expect("just pushed")
            .add(ScalarFunction::new(name, arguments, return_type, callback));
    }
    sets
}
