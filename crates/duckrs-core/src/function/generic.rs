// Ported from duckdb/extension/core_functions/scalar/list/{array_slice,list_sort}.cpp,
// duckdb/extension/core_functions/scalar/map/{map_keys_values,map_entries,map_from_entries,
// cardinality}.cpp, duckdb/extension/core_functions/scalar/string/{repeat,hex}.cpp,
// duckdb/extension/core_functions/scalar/date/to_interval.cpp and
// duckdb/src/function/scalar/string/{length,concat,concat_ws,like,caseconvert}.cpp
// @ rust-port-baseline
//
//! The helpers that the type tests lean on, ahead of the rest of Phase 5's function library.
//! They run row by row over values.

use super::bit_functions::{bit_length, strict_rows};
use super::combine_types::normalize_type;
use super::dates::{bind_date_function, is_date_function};
use super::lists::{bind_list_function, is_list_function};
use super::nested::{bind_nested_function, bound_function, row_wise};
use super::printf::format_callback;
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

/// `error`: fails with the given message.
fn error_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Err(Error::new(ErrorKind::InvalidInput, row[0].try_to_string()?))
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

/// `ListConcatFunction`: `list_concat` skips NULLs.
fn list_concat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
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
        // the collated elements the binder gives, which the elements sort by
        let (key_items, key_type) = match row.get(3) {
            Some(keys) => (
                items_of(keys),
                keys.logical_type().child_type().cloned().unwrap_or(child.clone()),
            ),
            None => (items, child.clone()),
        };
        let mut column = Vector::new(key_type, key_items.len().max(1));
        for (i, item) in key_items.iter().enumerate() {
            column.set_value(i, item)?;
        }
        let keys = create_sort_keys(&[(&column, modifiers)], key_items.len())?;
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

/// `DConstants::MAX_VECTOR_SIZE`: the most rows a vector can grow to (`VectorBuffer::GetReserveSize`).
const MAX_VECTOR_SIZE: u64 = 1 << 37;

/// `RepeatListFunction`.
fn repeat_list(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    // the entries of the result's child so far (`ListVector::GetListSize`)
    let child_size = std::cell::Cell::new(0u64);
    strict_rows(args, result, |row| {
        let items = items_of(&row[0]);
        let times = row[1].as_i128().unwrap_or(0);
        let times = if times <= 0 || items.is_empty() {
            0
        } else {
            times as u64
        };
        let length = items.len() as u64;
        let total = length.checked_mul(times).ok_or_else(|| {
            Error::new(
                ErrorKind::OutOfRange,
                format!("Cannot create a list of size: '{length}' * '{times}', the result is too large"),
            )
        })?;
        // `ListVector::Reserve`
        let required = child_size.get().saturating_add(total);
        if required > MAX_VECTOR_SIZE {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                format!(
                    "Cannot resize vector to {required} rows: maximum allowed vector size is {}",
                    crate::execution::temporary::format_bytes(MAX_VECTOR_SIZE)
                ),
            ));
        }
        child_size.set(required);
        let (items, times) = (items, times as usize);
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

pub(super) fn contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
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
            format!(
                "Cannot create a string of size: '{length}' in PAD, the maximum supported string size is: \
                 '{MAX_PAD_LENGTH}'"
            ),
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

/// `SleepFunction`, which doesn't care for NULL.
fn sleep_ms(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        let value = args.column(0).get_value(row);
        if let Some(time) = value.as_i128() {
            std::thread::sleep(std::time::Duration::from_millis(
                time.clamp(0, i128::from(u64::MAX)) as u64
            ));
        }
        result.set_value(row, &Value::sql_null())?;
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
    *result = Vector::constant_null(LogicalType::SQLNULL);
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

/// `UUIDv7::GenerateRandomUUID`: a 48 bit big-endian Unix timestamp in milliseconds, then random
/// bits, with the version and the variant set.
fn uuid_v7(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
        let random = [next_random() >> 32, next_random() >> 32, next_random() >> 32].map(|r| r as u32);
        bytes[6..10].copy_from_slice(&random[0].to_be_bytes());
        bytes[10..14].copy_from_slice(&random[1].to_be_bytes());
        bytes[14..16].copy_from_slice(&random[2].to_be_bytes()[..2]);
        bytes[6] = (bytes[6] & 0x0f) | (7 << 4);
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        // the top bit is flipped, so that the values sort as their bytes
        let stored = (u128::from_be_bytes(bytes) ^ (1 << 127)) as i128;
        result.set_value(row, &Value::uuid(stored))?;
    }
    Ok(())
}

fn list_length_of_dimension(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        if row[1].as_i128() != Some(1) {
            return Err(Error::new(
                ErrorKind::NotImplemented,
                "array_length for lists with dimensions other than 1 not implemented",
            ));
        }
        match row[0].data() {
            ValueData::List(items) => Ok(Value::bigint(items.len() as i64)),
            _ => Ok(Value::null(LogicalType::BIGINT)),
        }
    })
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
        || is_list_function(name)
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
                | "list_append"
                | "array_append"
                | "array_push_back"
                | "list_prepend"
                | "array_prepend"
                | "array_push_front"
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
                | "sleep_ms"
                | "setseed"
                | "uuid"
                | "gen_random_uuid"
                | "uuidv4"
                | "uuidv7"
                | "printf"
                | "format"
        )
}

/// Binds a function of [`is_generic_function`].
pub fn bind_generic_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if is_date_function(name) {
        return bind_date_function(name, children);
    }
    if is_list_function(name) {
        return bind_list_function(name, children);
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
        // `ListLengthBinaryFunction`: the length of a list, of its only dimension
        "array_length"
            if children.len() == 2
                && matches!(
                    children[0].return_type.id(),
                    LogicalTypeId::List | LogicalTypeId::SqlNull
                ) =>
        {
            let args = vec![
                children[0].clone(),
                children[1].clone().cast_to(&LogicalType::BIGINT, false),
            ];
            Ok(bound_function(
                &lower,
                args,
                LogicalType::BIGINT,
                list_length_of_dimension,
            ))
        }
        "len" | "array_length" | "length" => {
            let child = as_list(single(&children)?);
            let id = child.return_type.id();
            if id == LogicalTypeId::Bit && lower == "length" {
                return Ok(bound_function(&lower, vec![child], LogicalType::BIGINT, bit_length));
            }
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
            if children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            if children.iter().any(|c| is_list(&c.return_type)) {
                return bind_generic_function("list_concat", children);
            }
            // blob concat is only supported for the concat operator
            Ok(bound_function("concat", children, LogicalType::VARCHAR, concat))
        }
        "concat_ws" => {
            if children.len() < 2 {
                return Err(binder_error("concat_ws requires at least two arguments"));
            }
            let mut children = children;
            let separator = children.remove(0).cast_to(&LogicalType::VARCHAR, false);
            children.insert(0, separator);
            for child in &mut children[1..] {
                if child.return_type.id() == LogicalTypeId::List {
                    if child
                        .return_type
                        .child_type()
                        .is_some_and(|c| c.id() == LogicalTypeId::List)
                    {
                        return Err(binder_error("concat_ws() does not support nested lists"));
                    }
                    *child = child.clone().cast_to(&LogicalType::list(LogicalType::VARCHAR), false);
                } else {
                    *child = child.clone().cast_to(&LogicalType::VARCHAR, false);
                }
            }
            Ok(bound_function("concat_ws", children, LogicalType::VARCHAR, concat_ws))
        }
        "list_concat" | "array_concat" | "list_cat" | "array_cat" => {
            let mut element = LogicalType::SQLNULL;
            for child in &children {
                if is_list(&child.return_type) {
                    let child_type = child.return_type.child_type().expect("a child type");
                    element = super::combine_types::try_max_logical_type(&element, child_type).ok_or_else(|| {
                        binder_error(format!(
                            "Cannot concatenate lists of types {element}[] and {child_type}[] - an explicit cast is \
                             required"
                        ))
                    })?;
                } else if child.return_type.id() != LogicalTypeId::SqlNull {
                    return Err(no_match(&lower, &children));
                }
            }
            let list = LogicalType::list(element);
            let children = children.into_iter().map(|c| c.cast_to(&list, false)).collect();
            Ok(bound_function(&lower, children, list, list_concat))
        }
        "list_append" | "array_append" | "array_push_back" | "list_prepend" | "array_prepend" | "array_push_front" => {
            let [first, second] = <[BoundExpr; 2]>::try_from(children).map_err(|c| no_match(&lower, &c))?;
            // the element comes first in the prepend functions only
            let (list, element, append) = match lower.as_str() {
                "list_prepend" | "array_prepend" => (second, first, false),
                "array_push_front" => (first, second, false),
                _ => (first, second, true),
            };
            let element = bind_nested_function("list_value", vec![String::new()], vec![element])?;
            let arguments = if append {
                vec![list, element]
            } else {
                vec![element, list]
            };
            bind_generic_function("list_concat", arguments)
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
            let children = match super::extras::bind_temporal_range(&lower, children)? {
                Ok(bound) => return Ok(bound),
                Err(children) => children,
            };
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
            if is_list(&text.return_type) {
                return bind_generic_function("list_contains", children);
            }
            // `ContainsFun`: of a MAP its keys, of an unnamed struct its members
            if text.return_type.id() == LogicalTypeId::Map {
                return super::extras::bind_function("map_contains", &[String::new(), String::new()], children);
            }
            if text.return_type.id() == LogicalTypeId::Tuple {
                return super::extras::bind_function("struct_contains", &[String::new(), String::new()], children);
            }
            if children.iter().all(|c| c.return_type.id() == LogicalTypeId::SqlNull) {
                return Err(binder_error(
                    "Could not choose a best candidate function for the function call \"contains(\"NULL\", \
                     \"NULL\")\". In order to select one, please add explicit type casts.",
                ));
            }
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
                ty = super::combine_types::try_max_logical_type(&ty, &child.return_type).ok_or_else(|| {
                    Error::new(
                        ErrorKind::NotImplemented,
                        format!(
                            "Cannot combine types {ty} and {} - an explicit cast is required",
                            child.return_type
                        ),
                    )
                })?;
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
        "sleep_ms" => {
            let child = single(&children)?.cast_to(&LogicalType::BIGINT, false);
            Ok(bound_function("sleep_ms", vec![child], LogicalType::SQLNULL, sleep_ms))
        }
        "setseed" => {
            let child = single(&children)?.cast_to(&LogicalType::DOUBLE, false);
            Ok(bound_function("setseed", vec![child], LogicalType::SQLNULL, setseed))
        }
        "uuid" | "gen_random_uuid" | "uuidv4" | "uuidv7" => {
            if !children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            let function = if lower == "uuidv7" { uuid_v7 } else { uuid };
            Ok(bound_function(&lower, children, LogicalType::UUID, function))
        }
        "printf" | "format" => {
            if children.is_empty() {
                return Err(no_match(&lower, &children));
            }
            let mut children = children;
            children[0] = children[0].clone().cast_to(&LogicalType::VARCHAR, false);
            for child in &mut children[1..] {
                if let Some(target) = printf_argument_type(&child.return_type) {
                    *child = child.clone().cast_to(&target, false);
                }
            }
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

/// `PrintfResolveTypes`. A parameter is left alone, to be bound later.
fn printf_argument_type(ty: &LogicalType) -> Option<LogicalType> {
    use LogicalTypeId as Id;
    Some(match ty.id() {
        Id::Unknown => return None,
        Id::Boolean => LogicalType::BOOLEAN,
        Id::TinyInt | Id::SmallInt | Id::Integer | Id::BigInt => LogicalType::BIGINT,
        Id::UTinyInt | Id::USmallInt | Id::UInteger | Id::UBigInt => LogicalType::UBIGINT,
        Id::HugeInt => LogicalType::HUGEINT,
        Id::UHugeInt => LogicalType::UHUGEINT,
        Id::Float | Id::Double | Id::Decimal => LogicalType::DOUBLE,
        _ => LogicalType::VARCHAR,
    })
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

/// One position of a LIKE pattern between two `%`: a byte of the text, or with `None` a character.
type LikeSegment = Vec<Option<u8>>;

/// `LikeOperatorFunction`: a pattern as what is between its `%`. Each segment after the first can
/// start anywhere after the one before, and the first match is the one that leaves the most text.
/// UTF-8 lets literal bytes compare without decoding, as no character starts inside another.
struct LikePattern {
    segments: Vec<LikeSegment>,
}

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

impl LikePattern {
    fn new(pattern: &[u8]) -> Self {
        let segment = |part: &[u8]| part.iter().map(|&b| (b != b'_').then_some(b)).collect();
        Self {
            segments: pattern.split(|&b| b == b'%').map(segment).collect(),
        }
    }

    /// Where `segment` ends if it matches at the start of `text`.
    fn match_at(segment: &[Option<u8>], text: &[u8]) -> Option<usize> {
        let mut at = 0;
        for position in segment {
            let byte = *text.get(at)?;
            at += 1;
            match position {
                Some(literal) if *literal != byte => return None,
                Some(_) => {}
                None => {
                    while text.get(at).is_some_and(|b| is_continuation(*b)) {
                        at += 1;
                    }
                }
            }
        }
        Some(at)
    }

    fn matches(&self, text: &[u8]) -> bool {
        let (first, rest) = self.segments.split_first().expect("split gives at least one");
        let Some(mut at) = Self::match_at(first, text) else {
            return false;
        };
        let Some((last, middle)) = rest.split_last() else {
            return at == text.len();
        };
        for segment in middle {
            match Self::find(segment, &text[at..]) {
                Some(end) => at += end,
                None => return false,
            }
        }
        // only the last one has to end where the text ends
        (at..=text.len())
            .filter(|&start| start == text.len() || !is_continuation(text[start]))
            .any(|start| Self::match_at(last, &text[start..]) == Some(text.len() - start))
    }

    /// Where the first match of `segment` in `text` ends.
    fn find(segment: &[Option<u8>], text: &[u8]) -> Option<usize> {
        if segment.is_empty() {
            return Some(0);
        }
        (0..text.len())
            .filter(|&start| !is_continuation(text[start]))
            .find_map(|start| Self::match_at(segment, &text[start..]).map(|end| start + end))
    }
}

fn like_rows(args: &DataChunk, result: &mut Vector, ignore_case: bool, negated: bool) -> Result<()> {
    let count = args.len();
    let (texts, patterns) = (args.column(0).to_unified(count), args.column(1).to_unified(count));
    result.prepare_flat(count.max(1));
    // the pattern is nearly always a constant
    let mut compiled: Option<(Vec<u8>, LikePattern)> = None;
    for row in 0..count {
        if !texts.is_valid(row) || !patterns.is_valid(row) {
            result.set_null(row, true);
            continue;
        }
        let pattern = patterns.str_at(row);
        if compiled.as_ref().is_none_or(|(bytes, _)| bytes != pattern) {
            let folded = if ignore_case {
                String::from_utf8_lossy(pattern).to_lowercase().into_bytes()
            } else {
                pattern.to_vec()
            };
            compiled = Some((pattern.to_vec(), LikePattern::new(&folded)));
        }
        let (_, like) = compiled.as_ref().expect("just compiled");
        let text = texts.str_at(row);
        let matched = if ignore_case {
            like.matches(String::from_utf8_lossy(text).to_lowercase().as_bytes())
        } else {
            like.matches(text)
        };
        result.data_mut::<bool>()[row] = matched != negated;
    }
    Ok(())
}

fn like(args: &DataChunk, result: &mut Vector) -> Result<()> {
    like_rows(args, result, false, false)
}

fn not_like(args: &DataChunk, result: &mut Vector) -> Result<()> {
    like_rows(args, result, false, true)
}

fn ilike(args: &DataChunk, result: &mut Vector) -> Result<()> {
    like_rows(args, result, true, false)
}

fn not_ilike(args: &DataChunk, result: &mut Vector) -> Result<()> {
    like_rows(args, result, true, true)
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
        one("error", &varchar, &LogicalType::SQLNULL, error_function),
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

#[cfg(test)]
mod like_tests {
    use super::LikePattern;
    use proptest::prelude::*;

    /// The definition, on characters: `%` is any characters and `_` is one.
    fn expected(text: &[char], pattern: &[char]) -> bool {
        let Some((&first, rest)) = pattern.split_first() else {
            return text.is_empty();
        };
        match first {
            '%' => (0..=text.len()).any(|skip| expected(&text[skip..], rest)),
            '_' => !text.is_empty() && expected(&text[1..], rest),
            c => text.first() == Some(&c) && expected(&text[1..], rest),
        }
    }

    fn matches(text: &str, pattern: &str) -> bool {
        LikePattern::new(pattern.as_bytes()).matches(text.as_bytes())
    }

    #[test]
    fn like_matches_duckdb() {
        for (text, pattern, result) in [
            ("", "", true),
            ("a", "", false),
            ("", "%", true),
            ("", "_", false),
            ("", "%%", true),
            ("abc", "abc", true),
            ("abc", "ab", false),
            ("abc", "a%", true),
            ("abc", "%c", true),
            ("abc", "%b%", true),
            ("abc", "a_c", true),
            ("abc", "___", true),
            ("abc", "____", false),
            ("abcabc", "%abc", true),
            ("abab", "%ab%ab", true),
            ("aXbXc", "a%b%c", true),
            ("ab", "a%b%c", false),
            ("package requests special", "%special%requests%", false),
            ("special packages requests", "%special%requests%", true),
            // `_` is a character, not a byte
            ("é", "_", true),
            ("é", "__", false),
            ("aéb", "a_b", true),
            ("日本語", "%_語", true),
            ("日本語", "_本%", true),
            ("日本語", "%本%", true),
        ] {
            assert_eq!(matches(text, pattern), result, "{text} LIKE {pattern}");
        }
    }

    proptest! {
        #[test]
        fn like_agrees_with_its_definition(text in "[abé日]{0,8}", pattern in "[abé日%_]{0,8}") {
            let characters = |s: &str| s.chars().collect::<Vec<_>>();
            prop_assert_eq!(matches(&text, &pattern), expected(&characters(&text), &characters(&pattern)));
        }
    }
}
