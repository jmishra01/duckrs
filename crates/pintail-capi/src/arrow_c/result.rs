// Ported from duckdb/src/main/capi/v1/arrow-c.cpp (duckdb_query_arrow and the deprecated duckdb_arrow functions)
// @ rust-port-baseline

//! `duckdb_arrow`: a query's result read as Arrow arrays (`ArrowResultWrapper`).
//!
//! C++ runs such a query with the Arrow result format, whose arrays hold `STANDARD_VECTOR_SIZE` rows each. pintail
//! materializes the result and cuts the arrays from its chunks when C fetches them, `STANDARD_VECTOR_SIZE` rows at
//! a time, as many rows from as many chunks as that takes.

use super::export;
use super::options::ArrowOptions;
use super::raw::ArrowSchema;
use super::{
    _duckdb_arrow, ArrowArray, duckdb_arrow, duckdb_arrow_array, duckdb_arrow_schema, write_array, write_schema,
};
use crate::error::c_string;
use crate::ffi::{self, Handle};
use crate::types::*;
use arrow::array::{Array, ArrayRef, StructArray};
use pintail::{Error, ErrorKind, LogicalType, QueryResult, StatementProperties, StatementReturnType};
use pintail_common::STANDARD_VECTOR_SIZE;
use pintail_common::value::ValueData;
use std::ffi::{CString, c_char};
use std::sync::Arc;

/// `ArrowResultWrapper`.
pub struct ArrowResult {
    /// the rows, or the message of the error
    result: Result<QueryResult, CString>,
    properties: Option<StatementProperties>,
    options: ArrowOptions,
    /// where the next array starts: a chunk of the result and a row of it
    next_chunk: usize,
    next_row: usize,
}

impl Handle for _duckdb_arrow {
    type Target = ArrowResult;
}

impl ArrowResult {
    pub(crate) fn new(
        result: pintail::Result<QueryResult>,
        properties: Option<StatementProperties>,
        options: ArrowOptions,
    ) -> Self {
        Self {
            result: result.map_err(|error| c_string(&error.to_string())),
            properties,
            options,
            next_chunk: 0,
            next_row: 0,
        }
    }

    fn succeeded(&self) -> bool {
        self.result.is_ok()
    }

    /// `QueryResult::Fetch<ArrowFormat>`: the next rows, up to `STANDARD_VECTOR_SIZE`, as a struct array; `None`
    /// after the last.
    fn next_array(&mut self) -> pintail::Result<Option<StructArray>> {
        let Ok(result) = &self.result else {
            return Ok(None);
        };
        let collection = &result.collection;
        let mut pieces: Vec<ArrayRef> = Vec::new();
        let mut rows = 0;
        while rows < STANDARD_VECTOR_SIZE && self.next_chunk < collection.chunk_count() {
            let chunk = collection.chunk(self.next_chunk);
            let available = chunk.len().saturating_sub(self.next_row);
            if available == 0 {
                self.next_chunk += 1;
                self.next_row = 0;
                continue;
            }
            let count = available.min(STANDARD_VECTOR_SIZE - rows);
            let piece = if self.next_row == 0 && count == chunk.len() {
                export::chunk_array(chunk, &self.options)?
            } else {
                export::chunk_array(&chunk.range(self.next_row, count), &self.options)?
            };
            pieces.push(Arc::new(piece));
            rows += count;
            self.next_row += count;
        }
        let array = match pieces.len() {
            0 => return Ok(None),
            1 => pieces.pop().expect("one piece"),
            _ => {
                let pieces: Vec<&dyn Array> = pieces.iter().map(|p| p.as_ref()).collect();
                arrow::compute::concat(&pieces).map_err(|e| Error::new(ErrorKind::InvalidInput, e.to_string()))?
            }
        };
        Ok(array.as_any().downcast_ref::<StructArray>().cloned())
    }

    /// `ChangedRows`: the count of a statement that changes rows, read from its one row.
    fn rows_changed(&self) -> idx_t {
        let Ok(result) = &self.result else {
            return 0;
        };
        let changes_rows = self
            .properties
            .is_some_and(|p| p.return_type == StatementReturnType::ChangedRows);
        if !changes_rows || result.column_count() != 1 || result.row_count() == 0 {
            return 0;
        }
        match pintail::cast_value(&result.value(0, 0), &LogicalType::UBIGINT).map(|v| v.data().clone()) {
            Ok(ValueData::UInt64(changed)) => changed,
            _ => 0,
        }
    }

    fn names_and_types(&self) -> (Vec<String>, Vec<LogicalType>) {
        match &self.result {
            Ok(result) => (result.names.clone(), result.types.clone()),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }
}

/// Writes a new Arrow result to `out_result`: success unless the query failed (or there is no place to write it).
///
/// # Safety
///
/// `out_result` is null or valid for a write of a handle.
pub(crate) unsafe fn write_result(result: ArrowResult, out_result: *mut duckdb_arrow) -> duckdb_state {
    let state = if result.succeeded() { DuckDBSuccess } else { DuckDBError };
    // SAFETY: by the contract of this function.
    if unsafe { ffi::write(out_result, ffi::into_handle(result)) } {
        state
    } else {
        DuckDBError
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query_arrow(
    connection: duckdb_connection,
    query: *const c_char,
    out_result: *mut duckdb_arrow,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a NUL-terminated query.
    let (Some(connection), Some(query)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(query) }) else {
        return DuckDBError;
    };
    if out_result.is_null() {
        return DuckDBError;
    }
    let query = String::from_utf8_lossy(query);
    let outcome = ffi::guard(None, || {
        let mut connection = connection.connection.lock();
        let options = ArrowOptions::of(&connection);
        let result = connection.query(&query);
        let properties = connection.statement_properties(&query);
        Some(ArrowResult::new(result, properties, options))
    });
    let Some(result) = outcome else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the result; it is not null.
    unsafe { write_result(result, out_result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query_arrow_schema(
    result: duckdb_arrow,
    out_schema: *mut duckdb_arrow_schema,
) -> duckdb_state {
    if out_schema.is_null() {
        return DuckDBSuccess;
    }
    // SAFETY: C passes a live Arrow result.
    let Some(result) = (unsafe { ffi::get(result) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a pointer to where the schema goes.
    let target = unsafe { *out_schema }.cast::<ArrowSchema>();
    if target.is_null() {
        return DuckDBError;
    }
    let (names, types) = result.names_and_types();
    match ffi::guard(None, || Some(export::schema(&types, &names, &result.options))) {
        Some(Ok(schema)) => {
            // SAFETY: `target` is where C wants the schema; it is not null.
            unsafe { write_schema(target, schema) };
            DuckDBSuccess
        }
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query_arrow_array(
    result: duckdb_arrow,
    out_array: *mut duckdb_arrow_array,
) -> duckdb_state {
    if out_array.is_null() {
        return DuckDBSuccess;
    }
    // SAFETY: C passes a live Arrow result, which it reads from one thread at a time.
    let Some(result) = (unsafe { ffi::get_mut(result) }) else {
        return DuckDBError;
    };
    let next = ffi::guard(Err(Error::new(ErrorKind::Internal, "Unknown error in Fetch")), || {
        result.next_array()
    });
    let array = match next {
        Ok(Some(array)) => array,
        Ok(None) => return DuckDBSuccess,
        Err(error) => {
            // `SetError`: the result is an error from now on
            result.result = Err(c_string(&error.to_string()));
            return DuckDBError;
        }
    };
    // SAFETY: C passes a pointer to where the array goes.
    let target = unsafe { *out_array }.cast::<ArrowArray>();
    if target.is_null() {
        return DuckDBError;
    }
    // SAFETY: `target` is where C wants the array; it is not null.
    unsafe { write_array(target, arrow::ffi::FFI_ArrowArray::new(&array.to_data())) };
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_arrow_row_count(result: duckdb_arrow) -> idx_t {
    // SAFETY: C passes a live Arrow result.
    match unsafe { ffi::get(result) }.map(|r| &r.result) {
        Some(Ok(result)) => result.row_count() as idx_t,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_arrow_column_count(result: duckdb_arrow) -> idx_t {
    // SAFETY: C passes a live Arrow result.
    match unsafe { ffi::get(result) }.map(|r| &r.result) {
        Some(Ok(result)) => result.column_count() as idx_t,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_arrow_rows_changed(result: duckdb_arrow) -> idx_t {
    // SAFETY: C passes a live Arrow result.
    unsafe { ffi::get(result) }.map_or(0, |result| ffi::guard(0, || result.rows_changed()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query_arrow_error(result: duckdb_arrow) -> *const c_char {
    // SAFETY: C passes a live Arrow result.
    match unsafe { ffi::get(result) }.map(|r| &r.result) {
        Some(Err(message)) => message.as_ptr(),
        Some(Ok(_)) => c"".as_ptr(),
        None => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_arrow(result: *mut duckdb_arrow) {
    // SAFETY: C passes an Arrow result duckdb made.
    drop(unsafe { ffi::take(result) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_prepared_arrow(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_arrow,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement.
    let Some(wrapper) = (unsafe { crate::prepared::prepared_wrapper(prepared_statement) }) else {
        return DuckDBError;
    };
    if out_result.is_null() {
        return DuckDBError;
    }
    let options = ffi::guard(ArrowOptions::default(), || {
        ArrowOptions::of(&wrapper.connection().lock())
    });
    let Some((result, properties)) = wrapper.run() else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the result; it is not null.
    unsafe { write_result(ArrowResult::new(result, Some(properties), options), out_result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_arrow_schema(
    prepared: duckdb_prepared_statement,
    out_schema: *mut duckdb_arrow_schema,
) -> duckdb_state {
    if out_schema.is_null() {
        return DuckDBSuccess;
    }
    // SAFETY: C passes a live prepared statement.
    let Some(wrapper) = (unsafe { crate::prepared::prepared_wrapper(prepared) }) else {
        return DuckDBError;
    };
    let options = ffi::guard(ArrowOptions::default(), || {
        ArrowOptions::of(&wrapper.connection().lock())
    });
    // Every parameter is of type UNKNOWN, which is NULL in Arrow ('AdbcStatementGetParameterSchema')
    let count = wrapper.parameter_count();
    let types = vec![LogicalType::SQLNULL; count];
    let names: Vec<String> = (0..count).map(|i| i.to_string()).collect();
    // SAFETY: C passes a pointer to where the schema goes.
    let target = unsafe { *out_schema }.cast::<ArrowSchema>();
    if target.is_null() {
        return DuckDBError;
    }
    // SAFETY: `target` is a schema of C's: one released, or one to release before it is written over.
    if let Some(release) = unsafe { (*target).release } {
        // SAFETY: C's schema is not released yet, so it is released once, here.
        unsafe { release(target) };
    }
    match ffi::guard(None, || Some(export::schema(&types, &names, &options))) {
        Some(Ok(schema)) => {
            // SAFETY: `target` is where C wants the schema; it is not null.
            unsafe { write_schema(target, schema) };
            DuckDBSuccess
        }
        _ => DuckDBError,
    }
}
