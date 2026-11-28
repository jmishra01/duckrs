// Ported from duckdb/src/main/capi/v1/arrow-c.cpp, and duckdb-c.cpp / result-c.cpp (the arrow options)
// @ rust-port-baseline

//! The Arrow functions of the C API: results as Arrow arrays, DuckDB types and chunks to Arrow and back, and Arrow
//! streams and arrays registered as views a query can read.
//!
//! What duckrs hands C is built with arrow-rs and passed through `arrow::ffi` (`FFI_ArrowSchema`,
//! `FFI_ArrowArray`), whose release callbacks free it; what C hands duckrs is read through the structs of [`raw`]
//! (see there why). [`export`] and [`import`] convert a whole vector at a time.

pub mod export;
pub mod import;
pub mod options;
pub mod raw;
pub mod result;
pub mod scan;

use crate::error::new_error_data;
use crate::ffi::{self, Handle};
use crate::types::*;
use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use duckrs::ErrorKind;
use import::{ConvertedSchema, OwnedArray};
use options::ArrowOptions;
use raw::{ArrayView, ArrowArray, ArrowSchema, SchemaView};
use std::ffi::{c_char, c_void};
use std::sync::Arc;

// arrow-rs's structs of the C data interface are those of `raw`, so either may be written where C expects one.
const _: () = assert!(size_of::<FFI_ArrowSchema>() == size_of::<ArrowSchema>());
const _: () = assert!(size_of::<FFI_ArrowArray>() == size_of::<ArrowArray>());

/// The Arrow handles of `duckdb.h`: each a pointer to a struct of one pointer C never reads.
macro_rules! arrow_handles {
    ($($handle:ident = $object:ident;)*) => {$(
        #[repr(C)]
        #[allow(non_camel_case_types)]
        pub struct $object {
            pub internal_ptr: *mut c_void,
        }
        #[allow(non_camel_case_types)]
        pub type $handle = *mut $object;
    )*};
}

arrow_handles! {
    duckdb_arrow_options = _duckdb_arrow_options;
    duckdb_arrow = _duckdb_arrow;
    duckdb_arrow_stream = _duckdb_arrow_stream;
    duckdb_arrow_schema = _duckdb_arrow_schema;
    duckdb_arrow_array = _duckdb_arrow_array;
    duckdb_arrow_converted_schema = _duckdb_arrow_converted_schema;
}

impl Handle for _duckdb_arrow_options {
    type Target = ArrowOptions;
}

impl Handle for _duckdb_arrow_converted_schema {
    type Target = ConvertedSchema;
}

/// Writes `schema` to `out`, which may hold anything (it is not released first).
///
/// # Safety
///
/// `out` is valid for a write of an `ArrowSchema`.
pub(crate) unsafe fn write_schema(out: *mut ArrowSchema, schema: FFI_ArrowSchema) {
    // SAFETY: `FFI_ArrowSchema` has the layout of `ArrowSchema` (checked above), and `out` is valid for the write by
    // the contract of this function; C owns the schema now and releases it through its callback.
    unsafe { out.cast::<FFI_ArrowSchema>().write(schema) };
}

/// Writes `array` to `out`, which may hold anything (it is not released first).
///
/// # Safety
///
/// `out` is valid for a write of an `ArrowArray`.
pub(crate) unsafe fn write_array(out: *mut ArrowArray, array: FFI_ArrowArray) {
    // SAFETY: as for `write_schema`, of `FFI_ArrowArray` and `ArrowArray`.
    unsafe { out.cast::<FFI_ArrowArray>().write(array) };
}

fn invalid_input(message: &str) -> duckdb_error_data {
    new_error_data(ErrorKind::InvalidInput, message)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_connection_get_arrow_options(
    connection: duckdb_connection,
    out_arrow_options: *mut duckdb_arrow_options,
) {
    // SAFETY: C passes a live connection.
    let Some(connection) = (unsafe { ffi::get(connection) }) else {
        return;
    };
    if out_arrow_options.is_null() {
        return;
    }
    let options = ffi::guard(None, || Some(ArrowOptions::of(&connection.connection.lock())));
    let handle = options.map_or(std::ptr::null_mut(), ffi::into_handle);
    // SAFETY: C passes where to write the options; it is not null.
    unsafe { ffi::write(out_arrow_options, handle) };
}

/// `duckdb_result_get_arrow_options`: the options of a result that holds rows. duckrs's results do not keep the
/// options of the connection they came from, so these are the default ones.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_get_arrow_options(result: *mut duckdb_result) -> duckdb_arrow_options {
    // SAFETY: C passes a result a query wrote, or none.
    if !unsafe { crate::result::has_query_result(result) } {
        return std::ptr::null_mut();
    }
    ffi::into_handle(ArrowOptions::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_arrow_options(arrow_options: *mut duckdb_arrow_options) {
    // SAFETY: C passes options duckdb made.
    drop(unsafe { ffi::take(arrow_options) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_to_arrow_schema(
    arrow_options: duckdb_arrow_options,
    types: *mut duckdb_logical_type,
    names: *mut *const c_char,
    column_count: idx_t,
    out_schema: *mut ArrowSchema,
) -> duckdb_error_data {
    const INVALID: &str = "Invalid argument(s) to duckdb_to_arrow_schema";
    // SAFETY: C passes live options.
    let Some(options) = (unsafe { ffi::get(arrow_options) }) else {
        return invalid_input(INVALID);
    };
    if out_schema.is_null() {
        return invalid_input(INVALID);
    }
    // types and names may be null without columns
    if column_count > 0 && (types.is_null() || names.is_null()) {
        return invalid_input(INVALID);
    }
    let mut schema_types = Vec::with_capacity(column_count as usize);
    let mut schema_names = Vec::with_capacity(column_count as usize);
    if column_count > 0 {
        // SAFETY: C passes `column_count` types and names; neither is null.
        let (Some(types), Some(names)) = (unsafe { ffi::slice(types.cast_const(), column_count) }, unsafe {
            ffi::slice(names.cast_const(), column_count)
        }) else {
            return invalid_input(INVALID);
        };
        for (&ty, &name) in types.iter().zip(names) {
            // SAFETY: each type is a live logical type, and each name a NUL-terminated string.
            let (Some(ty), Some(name)) = (unsafe { ffi::get(ty) }, unsafe { ffi::c_str(name) }) else {
                return invalid_input(INVALID);
            };
            schema_types.push(ty.clone());
            schema_names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    let converted = ffi::guard(None, || Some(export::schema(&schema_types, &schema_names, options)));
    match converted {
        Some(Ok(schema)) => {
            // SAFETY: C passes where to write the schema; it is not null.
            unsafe { write_schema(out_schema, schema) };
            std::ptr::null_mut()
        }
        Some(Err(error)) => invalid_input(&error.to_string()),
        None => invalid_input("Unknown error occurred during conversion"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_to_arrow(
    arrow_options: duckdb_arrow_options,
    chunk: duckdb_data_chunk,
    out_arrow_array: *mut ArrowArray,
) -> duckdb_error_data {
    // SAFETY: C passes live options and a live chunk.
    let (Some(options), Some(chunk)) = (unsafe { ffi::get(arrow_options) }, unsafe { ffi::get(chunk) }) else {
        return invalid_input("Invalid argument(s) to duckdb_data_chunk_to_arrow");
    };
    if out_arrow_array.is_null() {
        return invalid_input("Invalid argument(s) to duckdb_data_chunk_to_arrow");
    }
    match ffi::guard(None, || Some(export::chunk_to_ffi(chunk, options))) {
        Some(Ok(array)) => {
            // SAFETY: C passes where to write the array; it is not null.
            unsafe { write_array(out_arrow_array, array) };
            std::ptr::null_mut()
        }
        Some(Err(error)) => invalid_input(&error.to_string()),
        None => invalid_input("Unknown error occurred during conversion"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_schema_from_arrow(
    connection: duckdb_connection,
    schema: *mut ArrowSchema,
    out_types: *mut duckdb_arrow_converted_schema,
) -> duckdb_error_data {
    // C++'s message names duckdb_data_chunk_to_arrow here too
    const INVALID: &str = "Invalid argument(s) to duckdb_data_chunk_to_arrow";
    // SAFETY: C passes a live connection.
    if unsafe { ffi::get(connection) }.is_none() || out_types.is_null() {
        return invalid_input(INVALID);
    }
    // SAFETY: C passes a schema that is valid by the Arrow C data interface, which outlives the call.
    let Some(schema) = (unsafe { schema.as_ref() }) else {
        return invalid_input(INVALID);
    };
    // SAFETY: as above.
    let view = unsafe { SchemaView::new(schema) };
    match ffi::guard(None, || Some(import::convert_schema(view))) {
        Some(Ok(converted)) => {
            // SAFETY: C passes where to write the converted schema; it is not null.
            unsafe { ffi::write(out_types, ffi::into_handle(converted)) };
            std::ptr::null_mut()
        }
        Some(Err(error)) => invalid_input(&error.to_string()),
        None => invalid_input("Unknown error occurred during conversion"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_from_arrow(
    connection: duckdb_connection,
    arrow_array: *mut ArrowArray,
    converted_schema: duckdb_arrow_converted_schema,
    out_chunk: *mut duckdb_data_chunk,
) -> duckdb_error_data {
    const INVALID: &str = "Invalid argument(s) to duckdb_data_chunk_to_arrow";
    // SAFETY: C passes a live connection and a live converted schema.
    let (Some(_), Some(schema)) = (unsafe { ffi::get(connection) }, unsafe { ffi::get(converted_schema) }) else {
        return invalid_input(INVALID);
    };
    if out_chunk.is_null() || arrow_array.is_null() {
        return invalid_input(INVALID);
    }
    {
        // SAFETY: C passes an array that is valid by the Arrow C data interface; it is not null.
        let view = unsafe { ArrayView::new(&*arrow_array) };
        if let Err(error) = import::validate_shape(schema, &view) {
            return invalid_input(&error.to_string());
        }
    }
    // The vectors share the array, so it is released once none of them references it any more; C's struct is
    // marked released to hand it over.
    // SAFETY: the array is valid and not null (above); moving an Arrow array struct is allowed, and C gives up its
    // copy by having its release set to null.
    let owned = Arc::new(OwnedArray(unsafe { arrow_array.read() }));
    // SAFETY: as above; C's struct is still there to write to.
    unsafe { (*arrow_array).release = None };
    // SAFETY: the array was valid as C passed it, and `owned` keeps it alive while the view is in use.
    let view = unsafe { ArrayView::new(&owned.0) };
    let count = view.len();
    let imported = ffi::guard(None, || {
        Some(import::import_chunk(schema, &view, 0, count, Some(owned.clone())))
    });
    match imported {
        Some(Ok(chunk)) => {
            // SAFETY: C passes where to write the chunk; it is not null.
            unsafe { ffi::write(out_chunk, ffi::into_handle::<_duckdb_data_chunk>(chunk)) };
            std::ptr::null_mut()
        }
        Some(Err(error)) => invalid_input(&error.to_string()),
        None => invalid_input("Unknown error occurred during conversion"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_arrow_converted_schema(
    arrow_converted_schema: *mut duckdb_arrow_converted_schema,
) {
    // SAFETY: C passes a converted schema `duckdb_schema_from_arrow` made.
    drop(unsafe { ffi::take(arrow_converted_schema) });
}

/// `duckdb_result_arrow_array`: `chunk` of `result` as an Arrow array, with the default options (see
/// [`duckdb_result_get_arrow_options`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_arrow_array(
    _result: duckdb_result,
    chunk: duckdb_data_chunk,
    out_array: *mut duckdb_arrow_array,
) {
    if out_array.is_null() {
        return;
    }
    // SAFETY: C passes a live chunk of the result.
    let Some(chunk) = (unsafe { ffi::get::<_duckdb_data_chunk>(chunk) }) else {
        return;
    };
    // SAFETY: C passes where the array goes: a pointer to it.
    let target = unsafe { *out_array }.cast::<ArrowArray>();
    if target.is_null() {
        return;
    }
    if let Some(Ok(array)) = ffi::guard(None, || Some(export::chunk_to_ffi(chunk, &ArrowOptions::default()))) {
        // SAFETY: `target` is where C wants the array; it is not null.
        unsafe { write_array(target, array) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckrs::{Database, LogicalType, Value};

    /// A query's chunk goes out through the C data interface and comes back with the same values.
    #[test]
    fn chunks_round_trip_through_the_c_data_interface() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        let result = con
            .query(
                "SELECT i::INTEGER AS i, i::VARCHAR || 'x' AS s, [i, NULL] AS l, {'a': i::DOUBLE} AS st, \
                 (i * 1.5)::DECIMAL(9, 2) AS d, CASE WHEN i % 2 = 0 THEN NULL ELSE i END AS n \
                 FROM range(5) t(i)",
            )
            .unwrap();
        let chunk = result.collection.chunk(0).clone();
        let options = ArrowOptions::default();
        let mut schema = ArrowSchema::empty();
        let mut array = ArrowArray::empty();
        // SAFETY: both are written to structs of this test.
        unsafe {
            write_schema(
                &mut schema,
                export::schema(&result.types, &result.names, &options).unwrap(),
            );
            write_array(&mut array, export::chunk_to_ffi(&chunk, &options).unwrap());
        }
        // SAFETY: the schema and the array were just made by arrow-rs, and live to the end of the test.
        let converted = import::convert_schema(unsafe { SchemaView::new(&schema) }).unwrap();
        assert_eq!(converted.names, result.names);
        assert_eq!(converted.types(), result.types);
        let owned = Arc::new(OwnedArray(array));
        // SAFETY: as above.
        let view = unsafe { ArrayView::new(&owned.0) };
        import::validate_shape(&converted, &view).unwrap();
        let imported = import::import_chunk(&converted, &view, 1, 4, Some(owned.clone())).unwrap();
        for column in 0..chunk.column_count() {
            for row in 0..4 {
                assert_eq!(imported.get_value(column, row), chunk.get_value(column, row + 1));
            }
        }
        assert_eq!(imported.get_value(0, 0), Value::integer(1));
        assert_eq!(imported.column(0).logical_type(), &LogicalType::INTEGER);
        // the vectors keep the array alive; it is released with the last of them
        assert_eq!(Arc::strong_count(&owned), 1 + chunk.column_count());
        drop(imported);
        assert_eq!(Arc::strong_count(&owned), 1);
        if let Some(release) = schema.release {
            // SAFETY: the schema arrow-rs made is released once.
            unsafe { release(&mut schema) };
        }
    }
}
