// Ported from duckdb/src/main/capi/v1/result-c.cpp, stream-c.cpp, value-c.cpp and cast/*.cpp @ rust-port-baseline

//! `duckdb_result`: the result of a query (`DuckDBResultData`), the deprecated column arrays it materializes on
//! demand, the `duckdb_value_*` accessors that read them, and its data chunks.

use crate::error::c_string;
use crate::ffi;
use crate::helper::{c_type_size, error_type_to_c, return_type_to_c, statement_type_to_c, type_id_to_c};
use crate::types::*;
use crate::vector::prepare_chunk;
use duckrs::{ErrorKind, LogicalType, LogicalTypeId, QueryResult, StatementProperties, Value};
use duckrs_common::types::{Interval, NativeType, PhysicalType};
use duckrs_common::value::ValueData;
use std::ffi::{CString, c_char, c_void};

/// `CAPIResultSetType`: a result is read with the deprecated column arrays or with data chunks, not both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultSetType {
    None,
    Materialized,
    Streaming,
    Deprecated,
}

/// `DuckDBResultData`: what `duckdb_result::internal_data` points to.
pub(crate) struct ResultData {
    result: Option<QueryResult>,
    /// `ErrorData::Message` (with the type of the error) and the type.
    error: Option<(CString, ErrorKind)>,
    properties: StatementProperties,
    names: Vec<CString>,
    set_type: ResultSetType,
    /// the chunks of the result that hold rows, by their index in its collection
    chunks: Vec<usize>,
    /// the next of `chunks` that `duckdb_fetch_chunk` gives
    next_chunk: usize,
    /// a result C reads as a stream (`QueryResultStream`): duckrs has all of its rows already, but C sees no row
    /// count and reads it a chunk at a time, as it would a stream
    streaming: bool,
}

impl ResultData {
    fn column_count(&self) -> usize {
        self.result.as_ref().map_or(0, QueryResult::column_count)
    }

    fn types(&self) -> &[LogicalType] {
        self.result.as_ref().map_or(&[], |r| r.types.as_slice())
    }

    fn row_count(&self) -> usize {
        self.result.as_ref().map_or(0, QueryResult::row_count)
    }
}

/// `DuckDBTranslateResult`: writes `result` to `out`, if there is an `out`.
pub(crate) fn translate_result(
    result: duckrs::Result<QueryResult>,
    properties: Option<StatementProperties>,
    out: Option<&mut duckdb_result>,
) -> duckdb_state {
    translate(result, properties, out, false)
}

/// `StreamsByDefault`: whether a statement executed to stream gives a stream; only a query does.
pub(crate) fn streams_by_default(properties: &StatementProperties) -> bool {
    matches!(
        properties.statement_type,
        duckrs::StatementType::Select | duckrs::StatementType::Pragma
    ) && properties.return_type == duckrs::StatementReturnType::QueryResult
}

/// [`translate_result`], or `DuckDBTranslateStreamResult` when `streaming` is set and the statement streams.
pub(crate) fn translate(
    result: duckrs::Result<QueryResult>,
    properties: Option<StatementProperties>,
    out: Option<&mut duckdb_result>,
    streaming: bool,
) -> duckdb_state {
    let state = if result.is_ok() { DuckDBSuccess } else { DuckDBError };
    let Some(out) = out else {
        return state;
    };
    *out = duckdb_result::zeroed();
    let properties = properties.unwrap_or(StatementProperties {
        statement_type: duckrs::StatementType::Invalid,
        return_type: duckrs::StatementReturnType::QueryResult,
    });
    let mut data = Box::new(match result {
        Ok(result) => {
            let chunks = (0..result.collection.chunk_count())
                .filter(|&i| !result.collection.chunk(i).is_empty())
                .collect();
            ResultData {
                names: result.names.iter().map(|name| c_string(name)).collect(),
                result: Some(result),
                error: None,
                properties,
                set_type: ResultSetType::None,
                chunks,
                next_chunk: 0,
                streaming: streaming && streams_by_default(&properties),
            }
        }
        Err(error) => ResultData {
            result: None,
            error: Some((c_string(&error.to_string()), error.kind)),
            properties,
            names: Vec::new(),
            set_type: ResultSetType::None,
            chunks: Vec::new(),
            next_chunk: 0,
            streaming: false,
        },
    });
    if let Some((message, _)) = &data.error {
        out.deprecated_error_message = message.as_ptr().cast_mut();
    } else {
        out.deprecated_column_count = data.column_count() as idx_t;
    }
    out.internal_data = std::ptr::from_mut(data.as_mut()).cast();
    std::mem::forget(data);
    state
}

/// The data behind a result: what its `internal_data` points to.
///
/// # Safety
///
/// `internal_data` is null or the `ResultData` `translate_result` made for a result, not yet destroyed, which nothing
/// else references while the result is in use (C reads a result from one thread at a time).
unsafe fn data_at<'a>(internal_data: *mut c_void) -> Option<&'a mut ResultData> {
    // SAFETY: by the contract of this function.
    unsafe { internal_data.cast::<ResultData>().as_mut() }
}

/// The `duckdb_result` C points to and its data.
///
/// # Safety
///
/// `result` is null or points to a result `translate_result` wrote (or one zeroed), not yet destroyed.
unsafe fn result_and_data<'a>(result: *mut duckdb_result) -> Option<(&'a mut duckdb_result, &'a mut ResultData)> {
    // SAFETY: by the contract of this function.
    let result = unsafe { ffi::reference_mut(result) }?;
    // SAFETY: the result's `internal_data` is its data, by the contract of this function.
    let data = unsafe { data_at(result.internal_data) }?;
    Some((result, data))
}

/// Whether `result` holds the result of a query that succeeded (`duckdb_result_get_arrow_options`).
///
/// # Safety
///
/// As for [`result_and_data`].
pub(crate) unsafe fn has_query_result(result: *mut duckdb_result) -> bool {
    // SAFETY: by the contract of this function.
    unsafe { result_and_data(result) }.is_some_and(|(_, data)| data.result.is_some())
}

/// The deprecated array of the values of one column, written by [`translate_column`].
fn write_values<S: NativeType, D: Copy>(
    result: &QueryResult,
    column: usize,
    target: &mut [D],
    null: D,
    convert: impl Fn(S) -> D,
) {
    let mut row = 0;
    for chunk in result.collection.chunks() {
        let unified = chunk.column(column).to_unified(chunk.len());
        let data = unified.data::<S>();
        for i in 0..chunk.len() {
            target[row] = if unified.is_valid(i) {
                convert(data[unified.sel.get(i)])
            } else {
                null
            };
            row += 1;
        }
    }
}

/// [`write_values`] of a VARCHAR or BLOB column.
fn write_strings<D: Copy>(result: &QueryResult, column: usize, target: &mut [D], null: D, convert: impl Fn(&[u8]) -> D) {
    let mut row = 0;
    for chunk in result.collection.chunks() {
        let unified = chunk.column(column).to_unified(chunk.len());
        for i in 0..chunk.len() {
            target[row] = if unified.is_valid(i) {
                convert(unified.str_at(i))
            } else {
                null
            };
            row += 1;
        }
    }
}

/// An array of `count` zeroed `T` for a deprecated column, as the slice to fill and the pointer C reads.
fn column_array<'a, T: Copy>(count: usize) -> Option<(&'a mut [T], *mut c_void)> {
    ffi::malloc_zeroed::<T>(count).map(|(slice, ptr)| (slice, ptr.cast()))
}

/// `deprecated_duckdb_translate_column`.
fn translate_column(result: &QueryResult, column: &mut duckdb_column, col: usize) -> bool {
    let rows = result.row_count();
    let Some((nullmask, nullmask_ptr)) = ffi::malloc_zeroed::<bool>(rows) else {
        return false;
    };
    column.deprecated_nullmask = nullmask_ptr;
    let type_size = c_type_size(column.deprecated_type);
    if type_size == 0 {
        // a type the deprecated result leaves out: its values are NULL, but not marked as such
        return true;
    }
    let mut row = 0;
    for chunk in result.collection.chunks() {
        let unified = chunk.column(col).to_unified(chunk.len());
        for i in 0..chunk.len() {
            nullmask[row] = !unified.is_valid(i);
            row += 1;
        }
    }
    let ty = &result.types[col];
    macro_rules! direct {
        ($s:ty, $d:ty, $convert:expr) => {{
            let Some((target, ptr)) = column_array::<$d>(rows) else {
                return false;
            };
            column.deprecated_data = ptr;
            write_values::<$s, $d>(result, col, target, <$d>::default(), $convert);
        }};
    }
    let finite = |v: i64| v != i64::MAX && v != -i64::MAX;
    use LogicalTypeId as L;
    match ty.id() {
        L::SqlNull => {
            let Some((_, ptr)) = column_array::<i32>(rows) else {
                return false;
            };
            column.deprecated_data = ptr;
        }
        L::Boolean => direct!(bool, bool, |v| v),
        L::TinyInt => direct!(i8, i8, |v| v),
        L::SmallInt => direct!(i16, i16, |v| v),
        L::Integer => direct!(i32, i32, |v| v),
        L::BigInt => direct!(i64, i64, |v| v),
        L::UTinyInt => direct!(u8, u8, |v| v),
        L::USmallInt => direct!(u16, u16, |v| v),
        L::UInteger => direct!(u32, u32, |v| v),
        L::UBigInt => direct!(u64, u64, |v| v),
        L::Float => direct!(f32, f32, |v| v),
        L::Double => direct!(f64, f64, |v| v),
        L::Date => direct!(i32, i32, |v| v),
        L::Time | L::TimeNs | L::TimeTz | L::Timestamp | L::TimestampTz => direct!(i64, i64, |v| v),
        L::TimestampNs | L::TimestampTzNs => direct!(i64, i64, |v| if finite(v) { v / 1000 } else { v }),
        L::TimestampMs => direct!(i64, i64, |v| if finite(v) { v.saturating_mul(1000) } else { v }),
        L::TimestampSec => direct!(i64, i64, |v| if finite(v) { v.saturating_mul(1_000_000) } else { v }),
        L::HugeInt => direct!(i128, duckdb_hugeint, duckdb_hugeint::from_i128),
        L::UHugeInt => direct!(u128, duckdb_uhugeint, duckdb_uhugeint::from_u128),
        L::Interval => direct!(Interval, duckdb_interval, |v: Interval| duckdb_interval {
            months: v.months,
            days: v.days,
            micros: v.micros,
        }),
        // `CDecimalConverter`: the value in `lower`, `upper` left 0
        L::Decimal => match ty.physical_type() {
            PhysicalType::Int16 => direct!(i16, duckdb_hugeint, |v| duckdb_hugeint {
                lower: i64::from(v) as u64,
                upper: 0
            }),
            PhysicalType::Int32 => direct!(i32, duckdb_hugeint, |v| duckdb_hugeint {
                lower: i64::from(v) as u64,
                upper: 0
            }),
            PhysicalType::Int64 => direct!(i64, duckdb_hugeint, |v| duckdb_hugeint {
                lower: v as u64,
                upper: 0
            }),
            _ => direct!(i128, duckdb_hugeint, duckdb_hugeint::from_i128),
        },
        L::Varchar | L::Char => {
            let Some((target, ptr)) = column_array::<*mut c_char>(rows) else {
                return false;
            };
            column.deprecated_data = ptr;
            write_strings(result, col, target, std::ptr::null_mut(), |s| ffi::strdup(s));
        }
        L::Blob => {
            let null = duckdb_blob {
                data: std::ptr::null_mut(),
                size: 0,
            };
            let Some((target, ptr)) = column_array::<duckdb_blob>(rows) else {
                return false;
            };
            column.deprecated_data = ptr;
            write_strings(result, col, target, null, |s| duckdb_blob {
                data: ffi::malloc_copy(s, false).cast(),
                size: s.len() as idx_t,
            });
        }
        // a type with a size that has no deprecated form (UUID)
        _ => return false,
    }
    true
}

/// `DeprecatedMaterializeResult`: fills the deprecated columns of `result`, once, unless chunks were read from it.
fn materialize(result: &mut duckdb_result, data: &mut ResultData) -> bool {
    if data.error.is_some() {
        return false;
    }
    match data.set_type {
        ResultSetType::Deprecated => return true,
        ResultSetType::Materialized | ResultSetType::Streaming => return false,
        ResultSetType::None => {}
    }
    data.set_type = ResultSetType::Deprecated;
    // the deprecated result reads every row at once: a stream is drained into it
    data.streaming = false;
    let Some(query_result) = &data.result else {
        return false;
    };
    let column_count = query_result.column_count();
    let Some((columns, columns_ptr)) = ffi::malloc_zeroed::<duckdb_column>(column_count) else {
        return false;
    };
    result.deprecated_columns = columns_ptr;
    for (i, column) in columns.iter_mut().enumerate() {
        column.deprecated_type = type_id_to_c(query_result.types[i].id());
        column.deprecated_name = data.names[i].as_ptr().cast_mut();
    }
    result.deprecated_row_count = query_result.row_count() as idx_t;
    if result.deprecated_row_count > 0 && data.properties.return_type == duckrs::StatementReturnType::ChangedRows {
        let changes = query_result.value(0, 0);
        if !changes.is_null()
            && let Ok(changes) = duckrs::cast_value(&changes, &LogicalType::BIGINT)
            && let ValueData::Int64(changes) = changes.data()
        {
            result.deprecated_rows_changed = *changes as idx_t;
        }
    }
    for (i, column) in columns.iter_mut().enumerate() {
        if !translate_column(query_result, column, i) {
            return false;
        }
    }
    true
}

/// `duckdb_destroy_result`'s `DuckdbDestroyColumn`.
fn destroy_column(column: &duckdb_column, rows: usize) {
    if !column.deprecated_data.is_null() {
        if column.deprecated_type == DUCKDB_TYPE_VARCHAR {
            // SAFETY: a VARCHAR column's data is the array of `rows` strings `translate_column` allocated.
            let strings = unsafe { ffi::slice(column.deprecated_data.cast::<*mut c_char>().cast_const(), rows as idx_t) };
            strings.unwrap_or_default().iter().for_each(|s| ffi::free(*s));
        } else if column.deprecated_type == DUCKDB_TYPE_BLOB {
            // SAFETY: a BLOB column's data is the array of `rows` blobs `translate_column` allocated.
            let blobs = unsafe { ffi::slice(column.deprecated_data.cast::<duckdb_blob>().cast_const(), rows as idx_t) };
            blobs.unwrap_or_default().iter().for_each(|b| ffi::free(b.data));
        }
        ffi::free(column.deprecated_data);
    }
    ffi::free(column.deprecated_nullmask);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_result(result: *mut duckdb_result) {
    // SAFETY: C passes a result a query wrote.
    let Some(result) = (unsafe { ffi::reference_mut(result) }) else {
        return;
    };
    if !result.deprecated_columns.is_null() {
        // SAFETY: the deprecated columns are the array of `deprecated_column_count` columns `materialize` made.
        let columns = unsafe { ffi::slice(result.deprecated_columns.cast_const(), result.deprecated_column_count) };
        for column in columns.unwrap_or_default() {
            destroy_column(column, result.deprecated_row_count as usize);
        }
        ffi::free(result.deprecated_columns);
    }
    if !result.internal_data.is_null() {
        // SAFETY: `internal_data` is the `ResultData` box `translate_result` leaked, destroyed only here.
        drop(unsafe { Box::from_raw(result.internal_data.cast::<ResultData>()) });
    }
    *result = duckdb_result::zeroed();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_name(result: *mut duckdb_result, col: idx_t) -> *const c_char {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        Some((_, data)) if (col as usize) < data.names.len() => data.names[col as usize].as_ptr(),
        _ => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_type(result: *mut duckdb_result, col: idx_t) -> duckdb_type {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        Some((_, data)) if (col as usize) < data.column_count() => type_id_to_c(data.types()[col as usize].id()),
        _ => DUCKDB_TYPE_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_logical_type(result: *mut duckdb_result, col: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        Some((_, data)) if (col as usize) < data.column_count() => {
            ffi::into_handle(data.types()[col as usize].clone())
        }
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_count(result: *mut duckdb_result) -> idx_t {
    // SAFETY: C passes a result a query wrote.
    unsafe { result_and_data(result) }.map_or(0, |(_, data)| data.column_count() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_row_count(result: *mut duckdb_result) -> idx_t {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        // the rows of a stream are not known before it is read
        Some((_, data)) if !data.streaming => data.row_count() as idx_t,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_rows_changed(result: *mut duckdb_result) -> idx_t {
    // SAFETY: C passes a result a query wrote.
    let Some((result, data)) = (unsafe { result_and_data(result) }) else {
        return 0;
    };
    if data.set_type == ResultSetType::Deprecated {
        return result.deprecated_rows_changed;
    }
    if data.streaming {
        return 0;
    }
    let Some(query_result) = &data.result else {
        return 0;
    };
    if data.properties.return_type != duckrs::StatementReturnType::ChangedRows
        || query_result.row_count() != 1
        || query_result.column_count() != 1
    {
        return 0;
    }
    match duckrs::cast_value(&query_result.value(0, 0), &LogicalType::UBIGINT).map(|v| v.data().clone()) {
        Ok(ValueData::UInt64(changed)) => changed,
        _ => 0,
    }
}

/// `duckdb_column_data` and `duckdb_nullmask_data`: the deprecated column `col`, materialized.
///
/// # Safety
///
/// As for [`result_and_data`].
unsafe fn deprecated_column<'a>(result: *mut duckdb_result, col: idx_t) -> Option<&'a duckdb_column> {
    // SAFETY: by the contract of this function.
    let (result, data) = unsafe { result_and_data(result) }?;
    if col >= result.deprecated_column_count || !materialize(result, data) {
        return None;
    }
    // SAFETY: `materialize` made `deprecated_columns` an array of `deprecated_column_count` columns.
    let columns = unsafe { ffi::slice(result.deprecated_columns.cast_const(), result.deprecated_column_count) }?;
    columns.get(col as usize)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_data(result: *mut duckdb_result, col: idx_t) -> *mut c_void {
    // SAFETY: C passes a result a query wrote.
    unsafe { deprecated_column(result, col) }.map_or(std::ptr::null_mut(), |column| column.deprecated_data)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_nullmask_data(result: *mut duckdb_result, col: idx_t) -> *mut bool {
    // SAFETY: C passes a result a query wrote.
    unsafe { deprecated_column(result, col) }.map_or(std::ptr::null_mut(), |column| column.deprecated_nullmask)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_error(result: *mut duckdb_result) -> *const c_char {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        Some((_, ResultData { error: Some((message, _)), .. })) => message.as_ptr(),
        _ => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_error_type(result: *mut duckdb_result) -> duckdb_error_type {
    // SAFETY: C passes a result a query wrote.
    match unsafe { result_and_data(result) } {
        Some((_, ResultData { error: Some((_, kind)), .. })) => error_type_to_c(*kind),
        _ => DUCKDB_ERROR_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_chunk_count(result: duckdb_result) -> idx_t {
    // SAFETY: C passes a result a query wrote.
    match unsafe { data_at(result.internal_data) } {
        Some(data) if data.set_type != ResultSetType::Deprecated && !data.streaming => data.chunks.len() as idx_t,
        _ => 0,
    }
}

/// A copy of chunk `index` of `data`'s collection, flat, as C reads chunks.
fn chunk_handle(data: &ResultData, index: usize) -> duckdb_data_chunk {
    let Some(result) = &data.result else {
        return std::ptr::null_mut();
    };
    let mut chunk = result.collection.chunk(index).clone();
    prepare_chunk(&mut chunk);
    ffi::into_handle(chunk)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_get_chunk(result: duckdb_result, chunk_index: idx_t) -> duckdb_data_chunk {
    // SAFETY: C passes a result a query wrote.
    let Some(data) = (unsafe { data_at(result.internal_data) }) else {
        return std::ptr::null_mut();
    };
    // random access is for retained results only
    if data.set_type == ResultSetType::Deprecated || data.streaming {
        return std::ptr::null_mut();
    }
    data.set_type = ResultSetType::Materialized;
    let Some(&index) = data.chunks.get(chunk_index as usize) else {
        return std::ptr::null_mut();
    };
    ffi::guard(std::ptr::null_mut(), || chunk_handle(data, index))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_fetch_chunk(result: duckdb_result) -> duckdb_data_chunk {
    // SAFETY: C passes a result a query wrote.
    let Some(data) = (unsafe { data_at(result.internal_data) }) else {
        return std::ptr::null_mut();
    };
    if data.set_type == ResultSetType::Deprecated {
        return std::ptr::null_mut();
    }
    data.set_type = ResultSetType::Streaming;
    let Some(&index) = data.chunks.get(data.next_chunk) else {
        return std::ptr::null_mut();
    };
    data.next_chunk += 1;
    ffi::guard(std::ptr::null_mut(), || chunk_handle(data, index))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_stream_fetch_chunk(result: duckdb_result) -> duckdb_data_chunk {
    // SAFETY: C passes a result a query wrote.
    unsafe { duckdb_fetch_chunk(result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_is_streaming(result: duckdb_result) -> bool {
    // SAFETY: C passes a result a query wrote.
    match unsafe { data_at(result.internal_data) } {
        Some(data) => data.error.is_none() && data.streaming,
        None => false,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_return_type(result: duckdb_result) -> duckdb_result_type {
    // SAFETY: C passes a result a query wrote.
    match unsafe { data_at(result.internal_data) } {
        Some(data) if data.error.is_none() => return_type_to_c(data.properties.return_type),
        _ => DUCKDB_RESULT_TYPE_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_statement_type(result: duckdb_result) -> duckdb_statement_type {
    // SAFETY: C passes a result a query wrote.
    match unsafe { data_at(result.internal_data) } {
        Some(data) if data.error.is_none() => statement_type_to_c(data.properties.statement_type),
        _ => DUCKDB_STATEMENT_TYPE_INVALID,
    }
}

// The `duckdb_value_*` accessors (value-c.cpp): a value of the deprecated result, cast to the type asked for.

/// `CanUseDeprecatedFetch`: the result materialized, and `col`, `row` in it.
///
/// # Safety
///
/// As for [`result_and_data`].
unsafe fn deprecated_fetch<'a>(
    result: *mut duckdb_result,
    col: idx_t,
    row: idx_t,
) -> Option<(&'a duckdb_column, &'a ResultData)> {
    // SAFETY: by the contract of this function.
    let (result, data) = unsafe { result_and_data(result) }?;
    if !materialize(result, data) || col >= result.deprecated_column_count || row >= result.deprecated_row_count {
        return None;
    }
    // SAFETY: `materialize` made `deprecated_columns` an array of `deprecated_column_count` columns.
    let columns = unsafe { ffi::slice(result.deprecated_columns.cast_const(), result.deprecated_column_count) }?;
    let column = &columns[col as usize];
    if column.deprecated_nullmask.is_null() {
        return None;
    }
    Some((column, data))
}

/// Whether row `row` of a materialized column is NULL.
fn is_null(column: &duckdb_column, rows: idx_t, row: idx_t) -> bool {
    // SAFETY: a materialized column's nullmask is the array of `rows` flags `translate_column` allocated.
    let nullmask = unsafe { ffi::slice(column.deprecated_nullmask.cast_const(), rows) };
    nullmask.is_some_and(|mask| mask[row as usize])
}

/// `CanFetchValue` and `GetInternalCValue`'s choice of source: the value at `col`, `row` when it is not NULL and of
/// a type the accessors read, with its type in the deprecated result.
///
/// # Safety
///
/// As for [`result_and_data`].
unsafe fn fetch_value(result: *mut duckdb_result, col: idx_t, row: idx_t) -> Option<(Value, duckdb_type)> {
    // SAFETY: by the contract of this function.
    let (column, data) = unsafe { deprecated_fetch(result, col, row) }?;
    if is_null(column, data.row_count() as idx_t, row) {
        return None;
    }
    match column.deprecated_type {
        DUCKDB_TYPE_BOOLEAN
        | DUCKDB_TYPE_TINYINT
        | DUCKDB_TYPE_SMALLINT
        | DUCKDB_TYPE_INTEGER
        | DUCKDB_TYPE_BIGINT
        | DUCKDB_TYPE_UTINYINT
        | DUCKDB_TYPE_USMALLINT
        | DUCKDB_TYPE_UINTEGER
        | DUCKDB_TYPE_UBIGINT
        | DUCKDB_TYPE_FLOAT
        | DUCKDB_TYPE_DOUBLE
        | DUCKDB_TYPE_DATE
        | DUCKDB_TYPE_TIME
        | DUCKDB_TYPE_TIME_NS
        | DUCKDB_TYPE_TIMESTAMP
        | DUCKDB_TYPE_HUGEINT
        | DUCKDB_TYPE_UHUGEINT
        | DUCKDB_TYPE_DECIMAL
        | DUCKDB_TYPE_INTERVAL
        | DUCKDB_TYPE_VARCHAR
        | DUCKDB_TYPE_BLOB => {}
        _ => return None,
    }
    let value = data.result.as_ref()?.value(col as usize, row as usize);
    Some((value, column.deprecated_type))
}

/// `TryCastCInternal`: the value at `col`, `row` cast to `ty`; `None` when it is NULL or does not cast. A BLOB casts
/// only to text (`FromCBlobCastWrapper`).
///
/// # Safety
///
/// As for [`result_and_data`].
unsafe fn fetch_cast(result: *mut duckdb_result, col: idx_t, row: idx_t, ty: &LogicalType) -> Option<ValueData> {
    // SAFETY: by the contract of this function.
    let (value, source) = unsafe { fetch_value(result, col, row) }?;
    if source == DUCKDB_TYPE_BLOB && ty.id() != LogicalTypeId::Varchar {
        return None;
    }
    let cast = ffi::guard(None, || duckrs::cast_value(&value, ty).ok())?;
    if cast.is_null() {
        return None;
    }
    Some(cast.data().clone())
}

macro_rules! value_accessor {
    ($name:ident, $ret:ty, $ty:expr, $variant:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(result: *mut duckdb_result, col: idx_t, row: idx_t) -> $ret {
            // SAFETY: C passes a result a query wrote.
            match unsafe { fetch_cast(result, col, row, &$ty) } {
                Some(ValueData::$variant(v)) => v,
                _ => <$ret>::default(),
            }
        }
    };
}

value_accessor!(duckdb_value_boolean, bool, LogicalType::BOOLEAN, Bool);
value_accessor!(duckdb_value_int8, i8, LogicalType::TINYINT, Int8);
value_accessor!(duckdb_value_int16, i16, LogicalType::SMALLINT, Int16);
value_accessor!(duckdb_value_int32, i32, LogicalType::INTEGER, Int32);
value_accessor!(duckdb_value_int64, i64, LogicalType::BIGINT, Int64);
value_accessor!(duckdb_value_uint8, u8, LogicalType::UTINYINT, UInt8);
value_accessor!(duckdb_value_uint16, u16, LogicalType::USMALLINT, UInt16);
value_accessor!(duckdb_value_uint32, u32, LogicalType::UINTEGER, UInt32);
value_accessor!(duckdb_value_uint64, u64, LogicalType::UBIGINT, UInt64);
value_accessor!(duckdb_value_float, f32, LogicalType::FLOAT, Float);
value_accessor!(duckdb_value_double, f64, LogicalType::DOUBLE, Double);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_hugeint(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_hugeint {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::HUGEINT) } {
        Some(ValueData::Int128(v)) => duckdb_hugeint::from_i128(v),
        _ => duckdb_hugeint::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_uhugeint(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_uhugeint {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::UHUGEINT) } {
        Some(ValueData::UInt128(v)) => duckdb_uhugeint::from_u128(v),
        _ => duckdb_uhugeint::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_decimal(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_decimal {
    // SAFETY: C passes a result a query wrote.
    let Some((value, DUCKDB_TYPE_DECIMAL)) = (unsafe { fetch_value(result, col, row) }) else {
        return duckdb_decimal::default();
    };
    let (width, scale) = value.logical_type().decimal_properties().unwrap_or_default();
    duckdb_decimal {
        width,
        scale,
        value: duckdb_hugeint::from_i128(value.as_i128().unwrap_or_default()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_date(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_date {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::DATE) } {
        Some(ValueData::Int32(days)) => duckdb_date { days },
        _ => duckdb_date::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_time(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_time {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::TIME) } {
        Some(ValueData::Int64(micros)) => duckdb_time { micros },
        _ => duckdb_time::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_timestamp(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_timestamp {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::TIMESTAMP) } {
        Some(ValueData::Int64(micros)) => duckdb_timestamp { micros },
        _ => duckdb_timestamp::default(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_interval(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_interval {
    // SAFETY: C passes a result a query wrote.
    match unsafe { fetch_cast(result, col, row, &LogicalType::INTERVAL) } {
        Some(ValueData::Interval(v)) => duckdb_interval {
            months: v.months,
            days: v.days,
            micros: v.micros,
        },
        _ => duckdb_interval::default(),
    }
}

fn no_string() -> duckdb_string {
    duckdb_string {
        data: std::ptr::null_mut(),
        size: 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_string(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_string {
    // SAFETY: C passes a result a query wrote.
    let Some((value, source)) = (unsafe { fetch_value(result, col, row) }) else {
        return no_string();
    };
    // `StringCast` of the source type; a VARCHAR is read as the deprecated result keeps it, up to a NUL
    let text = match value.data() {
        ValueData::Bytes(bytes) if source == DUCKDB_TYPE_VARCHAR => {
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            bytes[..end].to_vec()
        }
        _ => match ffi::guard(None, || value.try_to_string().ok()) {
            Some(text) => text.into_bytes(),
            None => return no_string(),
        },
    };
    duckdb_string {
        data: ffi::strdup(&text),
        size: text.len() as idx_t,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_varchar(result: *mut duckdb_result, col: idx_t, row: idx_t) -> *mut c_char {
    // SAFETY: C passes a result a query wrote.
    unsafe { duckdb_value_string(result, col, row) }.data
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_string_internal(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_string {
    // SAFETY: C passes a result a query wrote.
    let Some((column, data)) = (unsafe { deprecated_fetch(result, col, row) }) else {
        return no_string();
    };
    if is_null(column, data.row_count() as idx_t, row) || column.deprecated_type != DUCKDB_TYPE_VARCHAR {
        return no_string();
    }
    // SAFETY: a VARCHAR column's data is the array of the strings `translate_column` allocated, one per row.
    let strings = unsafe { ffi::slice(column.deprecated_data.cast::<*mut c_char>().cast_const(), data.row_count() as idx_t) };
    let Some(&text) = strings.and_then(|s| s.get(row as usize)) else {
        return no_string();
    };
    // SAFETY: the string is NUL-terminated, as `strdup` made it.
    let size = unsafe { ffi::c_str(text) }.map_or(0, <[u8]>::len);
    duckdb_string {
        data: text,
        size: size as idx_t,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_varchar_internal(result: *mut duckdb_result, col: idx_t, row: idx_t) -> *mut c_char {
    // SAFETY: C passes a result a query wrote.
    unsafe { duckdb_value_string_internal(result, col, row) }.data
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_blob(result: *mut duckdb_result, col: idx_t, row: idx_t) -> duckdb_blob {
    let none = duckdb_blob {
        data: std::ptr::null_mut(),
        size: 0,
    };
    // SAFETY: C passes a result a query wrote.
    let Some((value, DUCKDB_TYPE_BLOB)) = (unsafe { fetch_value(result, col, row) }) else {
        return none;
    };
    match value.data() {
        ValueData::Bytes(bytes) => duckdb_blob {
            data: ffi::malloc_copy(bytes, false).cast(),
            size: bytes.len() as idx_t,
        },
        _ => none,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_is_null(result: *mut duckdb_result, col: idx_t, row: idx_t) -> bool {
    // SAFETY: C passes a result a query wrote.
    let Some((column, data)) = (unsafe { deprecated_fetch(result, col, row) }) else {
        return false;
    };
    is_null(column, data.row_count() as idx_t, row)
}
