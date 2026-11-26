// Ported from duckdb/src/main/capi/v1/duckdb_value-c.cpp @ rust-port-baseline

//! `duckdb_value`: a handle to a `Value`.

use crate::ffi::{self, Handle};
use crate::logical_type::is_unusable;
use crate::types::*;
use duckrs::{LogicalType, LogicalTypeId, Value};
use duckrs_common::types::{Date, Interval, Time, TimeTz, Timestamp, bignum};
use duckrs_common::value::ValueData;
use std::ffi::c_char;

impl Handle for _duckdb_value {
    type Target = Value;
}

fn new_value(value: Value) -> duckdb_value {
    ffi::into_handle(value)
}

/// A value of `id` holding `data`, which fits its physical type.
fn typed(id: LogicalTypeId, data: ValueData) -> duckdb_value {
    match Value::new(LogicalType::new(id), data) {
        Ok(value) => new_value(value),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `CAPIGetValue`: the value behind `value` cast to `ty` (`DefaultTryCastAs`), or `None` when it is NULL or does not
/// cast.
///
/// # Safety
///
/// `value` is null or a live value.
unsafe fn cast(value: duckdb_value, ty: &LogicalType) -> Option<ValueData> {
    // SAFETY: by the contract of this function.
    let value = unsafe { ffi::get(value) }?;
    let cast = ffi::guard(None, || duckrs::cast_value(value, ty).ok())?;
    (!cast.is_null()).then(|| cast.data().clone())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_value(value: *mut duckdb_value) {
    // SAFETY: C passes a value it owns.
    drop(unsafe { ffi::take(value) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_varchar_length(text: *const c_char, length: idx_t) -> duckdb_value {
    // SAFETY: C passes `length` bytes.
    let Some(bytes) = (unsafe { ffi::bytes(text.cast(), length) }) else {
        return std::ptr::null_mut();
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => new_value(Value::varchar(text)),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_varchar(text: *const c_char) -> duckdb_value {
    // SAFETY: C passes a NUL-terminated string.
    let Some(bytes) = (unsafe { ffi::c_str(text) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `bytes` is a live slice.
    unsafe { duckdb_create_varchar_length(bytes.as_ptr().cast(), bytes.len() as idx_t) }
}

macro_rules! scalar {
    ($create:ident, $get:ident, $t:ty, $make:expr, $ty:expr, $variant:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $create(input: $t) -> duckdb_value {
            new_value($make(input))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $get(value: duckdb_value) -> $t {
            // SAFETY: C passes a live value.
            match unsafe { cast(value, &$ty) } {
                Some(ValueData::$variant(v)) => v,
                _ => <$t>::default(),
            }
        }
    };
}

scalar!(duckdb_create_bool, duckdb_get_bool, bool, Value::boolean, LogicalType::BOOLEAN, Bool);
scalar!(duckdb_create_int8, duckdb_get_int8, i8, Value::tinyint, LogicalType::TINYINT, Int8);
scalar!(duckdb_create_uint8, duckdb_get_uint8, u8, Value::utinyint, LogicalType::UTINYINT, UInt8);
scalar!(duckdb_create_int16, duckdb_get_int16, i16, Value::smallint, LogicalType::SMALLINT, Int16);
scalar!(duckdb_create_uint16, duckdb_get_uint16, u16, Value::usmallint, LogicalType::USMALLINT, UInt16);
scalar!(duckdb_create_int32, duckdb_get_int32, i32, Value::integer, LogicalType::INTEGER, Int32);
scalar!(duckdb_create_uint32, duckdb_get_uint32, u32, Value::uinteger, LogicalType::UINTEGER, UInt32);
scalar!(duckdb_create_int64, duckdb_get_int64, i64, Value::bigint, LogicalType::BIGINT, Int64);
scalar!(duckdb_create_uint64, duckdb_get_uint64, u64, Value::ubigint, LogicalType::UBIGINT, UInt64);
scalar!(duckdb_create_float, duckdb_get_float, f32, Value::float, LogicalType::FLOAT, Float);
scalar!(duckdb_create_double, duckdb_get_double, f64, Value::double, LogicalType::DOUBLE, Double);

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_hugeint(input: duckdb_hugeint) -> duckdb_value {
    new_value(Value::hugeint(input.to_i128()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_hugeint(value: duckdb_value) -> duckdb_hugeint {
    // SAFETY: C passes a live value.
    match unsafe { cast(value, &LogicalType::HUGEINT) } {
        Some(ValueData::Int128(v)) => duckdb_hugeint::from_i128(v),
        _ => duckdb_hugeint::default(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_uhugeint(input: duckdb_uhugeint) -> duckdb_value {
    new_value(Value::uhugeint(input.to_u128()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_uhugeint(value: duckdb_value) -> duckdb_uhugeint {
    // SAFETY: C passes a live value.
    match unsafe { cast(value, &LogicalType::UHUGEINT) } {
        Some(ValueData::UInt128(v)) => duckdb_uhugeint::from_u128(v),
        _ => duckdb_uhugeint::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_bignum(input: duckdb_bignum) -> duckdb_value {
    // SAFETY: C passes `size` bytes of the magnitude, most significant first.
    let Some(magnitude) = (unsafe { ffi::bytes(input.data.cast_const().cast(), input.size) }) else {
        return std::ptr::null_mut();
    };
    typed(LogicalTypeId::Bignum, ValueData::Bytes(bignum::from_magnitude(magnitude, input.is_negative)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_bignum(value: duckdb_value) -> duckdb_bignum {
    // SAFETY: C passes a live value.
    let bytes = match unsafe { cast(value, &LogicalType::new(LogicalTypeId::Bignum)) } {
        Some(ValueData::Bytes(bytes)) => bytes,
        _ => bignum::from_i128(0),
    };
    let magnitude = bignum::magnitude(&bytes);
    duckdb_bignum {
        data: ffi::malloc_copy(&magnitude, false),
        size: magnitude.len() as idx_t,
        is_negative: bignum::is_negative(&bytes),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_decimal(input: duckdb_decimal) -> duckdb_value {
    if input.width < 1 || input.width > 38 || input.scale > input.width {
        return std::ptr::null_mut();
    }
    match Value::decimal(input.value.to_i128(), input.width, input.scale) {
        Ok(value) => new_value(value),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_decimal(value: duckdb_value) -> duckdb_decimal {
    // SAFETY: C passes a live value.
    let Some(value) = (unsafe { ffi::get(value) }) else {
        return duckdb_decimal::default();
    };
    match value.logical_type().decimal_properties() {
        Some((width, scale)) if value.logical_type().id() == LogicalTypeId::Decimal => duckdb_decimal {
            width,
            scale,
            value: duckdb_hugeint::from_i128(value.as_i128().unwrap_or_default()),
        },
        _ => duckdb_decimal::default(),
    }
}

/// The `Int64` (or `Int32`) data of a value cast to a temporal type `id`.
///
/// # Safety
///
/// `value` is null or a live value.
unsafe fn get_i64(value: duckdb_value, id: LogicalTypeId) -> i64 {
    // SAFETY: by the contract of this function.
    match unsafe { cast(value, &LogicalType::new(id)) } {
        Some(ValueData::Int64(v)) => v,
        Some(ValueData::Int32(v)) => v.into(),
        Some(ValueData::UInt64(v)) => v as i64,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_date(input: duckdb_date) -> duckdb_value {
    new_value(Value::date(Date(input.days)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_date(value: duckdb_value) -> duckdb_date {
    // SAFETY: C passes a live value.
    duckdb_date {
        days: unsafe { get_i64(value, LogicalTypeId::Date) } as i32,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_time(input: duckdb_time) -> duckdb_value {
    new_value(Value::time(Time(input.micros)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_time(value: duckdb_value) -> duckdb_time {
    // SAFETY: C passes a live value.
    duckdb_time {
        micros: unsafe { get_i64(value, LogicalTypeId::Time) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_time_tz_value(input: duckdb_time_tz) -> duckdb_value {
    new_value(Value::time_tz(TimeTz(input.bits)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_time_tz(value: duckdb_value) -> duckdb_time_tz {
    // SAFETY: C passes a live value.
    duckdb_time_tz {
        bits: unsafe { get_i64(value, LogicalTypeId::TimeTz) } as u64,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_time_ns(input: duckdb_time_ns) -> duckdb_value {
    typed(LogicalTypeId::TimeNs, ValueData::Int64(input.nanos))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_time_ns(value: duckdb_value) -> duckdb_time_ns {
    // SAFETY: C passes a live value.
    duckdb_time_ns {
        nanos: unsafe { get_i64(value, LogicalTypeId::TimeNs) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp(input: duckdb_timestamp) -> duckdb_value {
    new_value(Value::timestamp(Timestamp(input.micros)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp(value: duckdb_value) -> duckdb_timestamp {
    // SAFETY: C passes a live value.
    duckdb_timestamp {
        micros: unsafe { get_i64(value, LogicalTypeId::Timestamp) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp_tz(input: duckdb_timestamp) -> duckdb_value {
    new_value(Value::timestamp_tz(Timestamp(input.micros)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp_tz(value: duckdb_value) -> duckdb_timestamp {
    // SAFETY: C passes a live value.
    duckdb_timestamp {
        micros: unsafe { get_i64(value, LogicalTypeId::TimestampTz) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp_tz_ns(input: duckdb_timestamp_ns) -> duckdb_value {
    typed(LogicalTypeId::TimestampTzNs, ValueData::Int64(input.nanos))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp_tz_ns(value: duckdb_value) -> duckdb_timestamp_ns {
    // SAFETY: C passes a live value.
    duckdb_timestamp_ns {
        nanos: unsafe { get_i64(value, LogicalTypeId::TimestampTzNs) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp_s(input: duckdb_timestamp_s) -> duckdb_value {
    typed(LogicalTypeId::TimestampSec, ValueData::Int64(input.seconds))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp_s(value: duckdb_value) -> duckdb_timestamp_s {
    // SAFETY: C passes a live value.
    duckdb_timestamp_s {
        seconds: unsafe { get_i64(value, LogicalTypeId::TimestampSec) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp_ms(input: duckdb_timestamp_ms) -> duckdb_value {
    typed(LogicalTypeId::TimestampMs, ValueData::Int64(input.millis))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp_ms(value: duckdb_value) -> duckdb_timestamp_ms {
    // SAFETY: C passes a live value.
    duckdb_timestamp_ms {
        millis: unsafe { get_i64(value, LogicalTypeId::TimestampMs) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_timestamp_ns(input: duckdb_timestamp_ns) -> duckdb_value {
    typed(LogicalTypeId::TimestampNs, ValueData::Int64(input.nanos))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_timestamp_ns(value: duckdb_value) -> duckdb_timestamp_ns {
    // SAFETY: C passes a live value.
    duckdb_timestamp_ns {
        nanos: unsafe { get_i64(value, LogicalTypeId::TimestampNs) },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_interval(input: duckdb_interval) -> duckdb_value {
    new_value(Value::interval(Interval {
        months: input.months,
        days: input.days,
        micros: input.micros,
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_interval(value: duckdb_value) -> duckdb_interval {
    // SAFETY: C passes a live value.
    match unsafe { cast(value, &LogicalType::INTERVAL) } {
        Some(ValueData::Interval(v)) => duckdb_interval {
            months: v.months,
            days: v.days,
            micros: v.micros,
        },
        _ => duckdb_interval::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_blob(data: *const u8, length: idx_t) -> duckdb_value {
    // SAFETY: C passes `length` bytes.
    match unsafe { ffi::bytes(data.cast(), length) } {
        Some(bytes) => new_value(Value::blob(bytes)),
        None => std::ptr::null_mut(),
    }
}

/// The bytes of a value cast to `ty` (a BLOB or BIT), in memory C frees with `duckdb_free`.
///
/// # Safety
///
/// `value` is null or a live value.
unsafe fn cast_bytes(value: duckdb_value, ty: &LogicalType) -> (*mut u8, idx_t) {
    // SAFETY: by the contract of this function.
    match unsafe { cast(value, ty) } {
        Some(ValueData::Bytes(bytes)) => (ffi::malloc_copy(&bytes, false), bytes.len() as idx_t),
        _ => (std::ptr::null_mut(), 0),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_blob(value: duckdb_value) -> duckdb_blob {
    // SAFETY: C passes a live value.
    let (data, size) = unsafe { cast_bytes(value, &LogicalType::BLOB) };
    duckdb_blob {
        data: data.cast(),
        size,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_bit(input: duckdb_bit) -> duckdb_value {
    // SAFETY: C passes `size` bytes of a BIT: its padding byte, then its bits.
    match unsafe { ffi::bytes(input.data.cast_const().cast(), input.size) } {
        Some(bytes) => typed(LogicalTypeId::Bit, ValueData::Bytes(bytes.to_vec())),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_bit(value: duckdb_value) -> duckdb_bit {
    // SAFETY: C passes a live value.
    let (data, size) = unsafe { cast_bytes(value, &LogicalType::new(LogicalTypeId::Bit)) };
    duckdb_bit { data, size }
}

/// `UUID::FromUHugeint` / `ToUHugeint`: a UUID is stored with its first bit flipped, so that it sorts as text does.
const UUID_FLIP: u128 = 1 << 127;

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_uuid(input: duckdb_uhugeint) -> duckdb_value {
    new_value(Value::uuid((input.to_u128() ^ UUID_FLIP) as i128))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_uuid(value: duckdb_value) -> duckdb_uhugeint {
    // SAFETY: C passes a live value.
    match unsafe { cast(value, &LogicalType::UUID) } {
        Some(ValueData::Int128(stored)) => duckdb_uhugeint::from_u128(stored as u128 ^ UUID_FLIP),
        _ => duckdb_uhugeint::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_value_type(value: duckdb_value) -> duckdb_logical_type {
    // SAFETY: C passes a live value.
    match unsafe { ffi::get(value) } {
        Some(value) => ffi::borrowed_const_handle(value.logical_type()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_varchar(value: duckdb_value) -> *mut c_char {
    // SAFETY: C passes a live value.
    let Some(value) = (unsafe { ffi::get(value) }) else {
        return std::ptr::null_mut();
    };
    let text = ffi::guard(None, || {
        if value.is_null() {
            return Some(Vec::new());
        }
        match duckrs::cast_value(value, &LogicalType::VARCHAR).ok()?.data() {
            ValueData::Bytes(bytes) => Some(bytes.clone()),
            _ => None,
        }
    });
    text.map_or(std::ptr::null_mut(), ffi::strdup)
}

/// The `count` values C passes, cast to `types` in turn where they differ.
///
/// # Safety
///
/// `values` is null or an array of `count` live values.
unsafe fn values_of(values: *mut duckdb_value, count: idx_t, types: impl Fn(usize) -> LogicalType) -> Option<Vec<Value>> {
    // SAFETY: by the contract of this function.
    let handles = unsafe { ffi::slice(values.cast_const(), count) }?;
    let mut result = Vec::with_capacity(handles.len());
    for (i, handle) in handles.iter().enumerate() {
        // SAFETY: each value is live, by the contract of this function.
        let value = unsafe { ffi::get(*handle) }?;
        let ty = types(i);
        let value = if value.logical_type() == &ty {
            value.clone()
        } else if value.is_null() {
            Value::null(ty)
        } else {
            duckrs::cast_value(value, &ty).ok()?
        };
        result.push(value);
    }
    Some(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_struct_value(ty: duckdb_logical_type, values: *mut duckdb_value) -> duckdb_value {
    // SAFETY: C passes a live logical type.
    let Some(ty) = (unsafe { ffi::get(ty) }) else {
        return std::ptr::null_mut();
    };
    if !matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple) || is_unusable(ty) {
        return std::ptr::null_mut();
    }
    let children = ty.struct_children();
    // SAFETY: C passes one value per member.
    let Some(fields) = (unsafe { values_of(values, children.len() as idx_t, |i| children[i].1.clone()) }) else {
        return std::ptr::null_mut();
    };
    Value::new(ty.clone(), ValueData::Struct(fields)).map_or(std::ptr::null_mut(), new_value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_list_value(
    ty: duckdb_logical_type,
    values: *mut duckdb_value,
    value_count: idx_t,
) -> duckdb_value {
    // SAFETY: C passes a live logical type.
    let Some(ty) = (unsafe { ffi::get(ty) }) else {
        return std::ptr::null_mut();
    };
    if is_unusable(ty) {
        return std::ptr::null_mut();
    }
    // SAFETY: C passes `value_count` values.
    let Some(items) = (unsafe { values_of(values, value_count, |_| ty.clone()) }) else {
        return std::ptr::null_mut();
    };
    Value::list(ty.clone(), items).map_or(std::ptr::null_mut(), new_value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_array_value(
    ty: duckdb_logical_type,
    values: *mut duckdb_value,
    value_count: idx_t,
) -> duckdb_value {
    // SAFETY: C passes a live logical type.
    let Some(ty) = (unsafe { ffi::get(ty) }) else {
        return std::ptr::null_mut();
    };
    if value_count >= 100_000 || is_unusable(ty) {
        return std::ptr::null_mut();
    }
    // SAFETY: C passes `value_count` values.
    let Some(items) = (unsafe { values_of(values, value_count, |_| ty.clone()) }) else {
        return std::ptr::null_mut();
    };
    Value::new(LogicalType::array(ty.clone(), value_count as u32), ValueData::List(items))
        .map_or(std::ptr::null_mut(), new_value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_map_value(
    map_type: duckdb_logical_type,
    keys: *mut duckdb_value,
    values: *mut duckdb_value,
    entry_count: idx_t,
) -> duckdb_value {
    // SAFETY: C passes a live logical type.
    let Some(ty) = (unsafe { ffi::get(map_type) }) else {
        return std::ptr::null_mut();
    };
    if ty.id() != LogicalTypeId::Map || is_unusable(ty) {
        return std::ptr::null_mut();
    }
    let Some(entry) = ty.child_type() else {
        return std::ptr::null_mut();
    };
    let (key_type, value_type) = (entry.struct_children()[0].1.clone(), entry.struct_children()[1].1.clone());
    // SAFETY: C passes `entry_count` keys and values.
    let (Some(keys), Some(values)) = (unsafe { values_of(keys, entry_count, |_| key_type.clone()) }, unsafe {
        values_of(values, entry_count, |_| value_type.clone())
    }) else {
        return std::ptr::null_mut();
    };
    let mut entries = Vec::with_capacity(keys.len());
    for (key, value) in keys.into_iter().zip(values) {
        match Value::new(entry.clone(), ValueData::Struct(vec![key, value])) {
            Ok(entry) => entries.push(entry),
            Err(_) => return std::ptr::null_mut(),
        }
    }
    Value::new(ty.clone(), ValueData::List(entries)).map_or(std::ptr::null_mut(), new_value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_union_value(
    union_type: duckdb_logical_type,
    tag_index: idx_t,
    value: duckdb_value,
) -> duckdb_value {
    // SAFETY: C passes a live logical type and value.
    let (Some(ty), Some(value)) = (unsafe { ffi::get(union_type) }, unsafe { ffi::get(value) }) else {
        return std::ptr::null_mut();
    };
    if ty.id() != LogicalTypeId::Union {
        return std::ptr::null_mut();
    }
    let children = ty.struct_children();
    let tag = tag_index as usize;
    if tag + 1 >= children.len() || value.logical_type() != &children[tag + 1].1 {
        return std::ptr::null_mut();
    }
    let mut fields = vec![Value::utinyint(tag as u8)];
    fields.extend(children[1..].iter().enumerate().map(|(i, (_, member))| {
        if i == tag { value.clone() } else { Value::null(member.clone()) }
    }));
    Value::new(ty.clone(), ValueData::Struct(fields)).map_or(std::ptr::null_mut(), new_value)
}

/// The children of a LIST or MAP (`id`) value, when it is one and not NULL.
///
/// # Safety
///
/// `value` is null or a live value.
unsafe fn list_children<'a>(value: duckdb_value, id: LogicalTypeId) -> Option<&'a [Value]> {
    // SAFETY: by the contract of this function.
    let value = unsafe { ffi::get(value) }?;
    match value.data() {
        ValueData::List(items) if value.logical_type().id() == id => Some(items),
        _ => None,
    }
}

/// Member `index` of the STRUCT entry `entry` of a MAP.
fn map_entry_member(entry: &Value, index: usize) -> duckdb_value {
    match entry.data() {
        ValueData::Struct(fields) if index < fields.len() => new_value(fields[index].clone()),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_map_size(value: duckdb_value) -> idx_t {
    // SAFETY: C passes a live value.
    unsafe { list_children(value, LogicalTypeId::Map) }.map_or(0, |entries| entries.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_map_key(value: duckdb_value, index: idx_t) -> duckdb_value {
    // SAFETY: C passes a live value.
    match unsafe { list_children(value, LogicalTypeId::Map) }.and_then(|e| e.get(index as usize)) {
        Some(entry) => map_entry_member(entry, 0),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_map_value(value: duckdb_value, index: idx_t) -> duckdb_value {
    // SAFETY: C passes a live value.
    match unsafe { list_children(value, LogicalTypeId::Map) }.and_then(|e| e.get(index as usize)) {
        Some(entry) => map_entry_member(entry, 1),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_is_null_value(value: duckdb_value) -> bool {
    // SAFETY: C passes a live value.
    unsafe { ffi::get(value) }.is_some_and(Value::is_null)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_null_value() -> duckdb_value {
    new_value(Value::sql_null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_list_size(value: duckdb_value) -> idx_t {
    // SAFETY: C passes a live value.
    unsafe { list_children(value, LogicalTypeId::List) }.map_or(0, |items| items.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_list_child(value: duckdb_value, index: idx_t) -> duckdb_value {
    // SAFETY: C passes a live value.
    match unsafe { list_children(value, LogicalTypeId::List) }.and_then(|items| items.get(index as usize)) {
        Some(item) => new_value(item.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_enum_value(ty: duckdb_logical_type, value: u64) -> duckdb_value {
    // SAFETY: C passes a live logical type.
    match unsafe { ffi::get(ty) } {
        Some(ty) if ty.id() == LogicalTypeId::Enum && (value as usize) < ty.enum_values().len() => {
            new_value(Value::enum_value(ty, value as usize))
        }
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_enum_value(value: duckdb_value) -> u64 {
    // SAFETY: C passes a live value.
    match unsafe { ffi::get(value) } {
        Some(value) if value.logical_type().id() == LogicalTypeId::Enum => match value.data() {
            ValueData::UInt8(v) => (*v).into(),
            ValueData::UInt16(v) => (*v).into(),
            ValueData::UInt32(v) => (*v).into(),
            _ => 0,
        },
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_struct_child(value: duckdb_value, index: idx_t) -> duckdb_value {
    // SAFETY: C passes a live value.
    let Some(value) = (unsafe { ffi::get(value) }) else {
        return std::ptr::null_mut();
    };
    match value.data() {
        ValueData::Struct(fields) if matches!(value.logical_type().id(), LogicalTypeId::Struct | LogicalTypeId::Tuple) => {
            fields.get(index as usize).map_or(std::ptr::null_mut(), |f| new_value(f.clone()))
        }
        _ => std::ptr::null_mut(),
    }
}

/// `Value::ToSQLString`: the value as a SQL literal.
pub(crate) fn to_sql_string(value: &Value) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
    if value.is_null() {
        return "NULL".to_string();
    }
    let ty = value.logical_type();
    let text = || value.try_to_string().unwrap_or_default();
    use LogicalTypeId as L;
    match ty.id() {
        L::Boolean
        | L::TinyInt
        | L::SmallInt
        | L::Integer
        | L::BigInt
        | L::HugeInt
        | L::UTinyInt
        | L::USmallInt
        | L::UInteger
        | L::UBigInt
        | L::UHugeInt
        | L::Decimal => text(),
        L::Float | L::Double => {
            let text = text();
            match text.as_str() {
                "nan" | "inf" | "-inf" => format!("{}::{ty}", quote(&text)),
                _ => text,
            }
        }
        L::Varchar | L::Char => quote(&text()),
        L::Struct | L::Tuple => match value.data() {
            ValueData::Struct(fields) => {
                let members: Vec<String> = ty
                    .struct_children()
                    .iter()
                    .zip(fields)
                    .map(|((name, _), field)| format!("{}: {}", quote(name), to_sql_string(field)))
                    .collect();
                format!("{{{}}}", members.join(", "))
            }
            _ => text(),
        },
        L::List | L::Array => match value.data() {
            ValueData::List(items) => {
                let items: Vec<String> = items.iter().map(to_sql_string).collect();
                format!("[{}]", items.join(", "))
            }
            _ => text(),
        },
        _ => format!("{}::{ty}", quote(&text())),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_to_string(value: duckdb_value) -> *mut c_char {
    // SAFETY: C passes a live value.
    match unsafe { ffi::get(value) } {
        Some(value) => ffi::strdup(ffi::guard(String::new(), || to_sql_string(value))),
        None => std::ptr::null_mut(),
    }
}
