// Ported from duckdb/src/main/capi/v1/arrow-c.cpp (duckdb_arrow_scan, duckdb_arrow_array_scan,
// arrow_array_stream_wrapper) and src/function/table/arrow.cpp (arrow_scan) @ rust-port-baseline

//! Arrow streams as views: `duckdb_arrow_scan` makes a temporary view that reads a stream C keeps.
//!
//! C++ binds its `arrow_scan` table function to the stream's factory and makes the view of that call. duckrs makes
//! the view of a user table function (`function/udf.rs`, the machinery of C API table functions), which every
//! connection of the database has: its one parameter names the stream in a registry of the streams views were made
//! of. Its bind reads the stream's schema; its scan reads the stream's arrays, `STANDARD_VECTOR_SIZE` rows to a chunk.

use super::import::{self, ConvertedSchema, OwnedArray};
use super::raw::{ArrayView, ArrowArray, ArrowArrayStream, ArrowSchema, SchemaView};
use super::duckdb_arrow_schema;
use super::{duckdb_arrow_array, duckdb_arrow_stream};
use crate::ffi::{self, CPointer};
use crate::types::*;
use duckrs::udf::{TableUdf, TableUdfBindInput, TableUdfBinding, TableUdfOverload, TableUdfRun, TableUdfScan, TableUdfSet};
use duckrs::{DataChunk, Error, ErrorKind, LogicalType, Result};
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::value::ValueData;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// The table function the views of Arrow streams call.
const SCAN_FUNCTION: &str = "__duckrs_capi_arrow_scan";

/// The streams views were made of, by the number the view's call passes. C keeps each stream alive for as long as
/// it uses the view (as with C++), so an entry stays.
static STREAMS: LazyLock<Mutex<HashMap<u64, CPointer>>> = LazyLock::new(Mutex::default);
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// The message of the stream's last error (`get_last_error`), or `fallback`.
///
/// # Safety
///
/// `stream` is a live stream.
unsafe fn last_error(stream: *mut ArrowArrayStream, fallback: &str) -> String {
    // SAFETY: `stream` is live, by the contract of this function.
    let Some(get_last_error) = (unsafe { (*stream).get_last_error }) else {
        return fallback.to_string();
    };
    // SAFETY: the stream's callback, called on the stream; it gives null or a NUL-terminated message.
    let message = unsafe { get_last_error(stream) };
    if message.is_null() {
        return fallback.to_string();
    }
    // SAFETY: a non-null message is NUL-terminated and valid until the next call on the stream.
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

/// `ArrowScanFactory::GetSchema`: the stream's schema, as columns. Like C++'s `FactoryGetSchema`, the schema is not
/// released: a stream does not let the consumer release the schema its factory gives (the caller owns it).
fn stream_schema(stream: CPointer) -> Result<ConvertedSchema> {
    let stream = stream.0.cast::<ArrowArrayStream>();
    // SAFETY: the stream of a view is alive while the view is in use, as C keeps it.
    let Some(get_schema) = (unsafe { (*stream).get_schema }) else {
        return Err(invalid("arrow_scan: the Arrow stream has no get_schema"));
    };
    let mut schema = ArrowSchema::empty();
    // SAFETY: the stream's callback, called on the stream with a schema to write.
    if unsafe { get_schema(stream, &mut schema) } != 0 {
        // SAFETY: the stream is live.
        return Err(invalid(unsafe { last_error(stream, "arrow_scan: get_schema failed") }));
    }
    // SAFETY: the stream gave a valid schema, which lives as long as the stream.
    import::convert_schema(unsafe { SchemaView::new(&schema) })
}

/// The table function of the views (`arrow_scan` of a `CArrowScanFactory`).
struct ArrowScanFunction;

impl TableUdf for ArrowScanFunction {
    fn bind(&self, input: TableUdfBindInput) -> Result<TableUdfBinding> {
        let id = match input.parameters.first().map(|v| v.data()) {
            Some(ValueData::UInt64(id)) => *id,
            _ => return Err(invalid("arrow_scan: no Arrow stream")),
        };
        let stream = STREAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .copied()
            .ok_or_else(|| invalid("arrow_scan: the Arrow stream of this view is not registered"))?;
        let schema = stream_schema(stream)?;
        Ok(TableUdfBinding {
            names: schema.names.clone(),
            types: schema.types(),
            cardinality: None,
            scan: Arc::new(ArrowScan {
                stream,
                schema: Arc::new(schema),
            }),
        })
    }
}

/// A bound scan of a stream (`ArrowScanFunctionData`).
struct ArrowScan {
    stream: CPointer,
    schema: Arc<ConvertedSchema>,
}

impl TableUdfScan for ArrowScan {
    fn init(&self, _column_ids: &[usize]) -> Result<Box<dyn TableUdfRun>> {
        Ok(Box::new(ArrowScanRun {
            stream: self.stream,
            schema: self.schema.clone(),
            current: None,
            done: false,
        }))
    }
}

/// A running scan (`ArrowScanGlobalState`/`ArrowScanLocalState`): the array it reads and the next row of it.
struct ArrowScanRun {
    stream: CPointer,
    schema: Arc<ConvertedSchema>,
    current: Option<(OwnedArray, usize)>,
    done: bool,
}

impl ArrowScanRun {
    /// `ArrowArrayStreamWrapper::GetNextChunk`: the next array of the stream, `None` at its end.
    fn next_array(&mut self) -> Result<Option<OwnedArray>> {
        let stream = self.stream.0.cast::<ArrowArrayStream>();
        // SAFETY: the stream is alive while the view is in use, as C keeps it.
        let Some(get_next) = (unsafe { (*stream).get_next }) else {
            return Err(invalid("arrow_scan: the Arrow stream has no get_next"));
        };
        let mut array = ArrowArray::empty();
        // SAFETY: the stream's callback, called on the stream with an array to write.
        if unsafe { get_next(stream, &mut array) } != 0 {
            // SAFETY: the stream is live.
            return Err(invalid(unsafe { last_error(stream, "arrow_scan: get_next failed") }));
        }
        // a released array ends the stream
        Ok(array.release.is_some().then_some(OwnedArray(array)))
    }
}

impl TableUdfRun for ArrowScanRun {
    fn next(&mut self, output: &mut DataChunk) -> Result<()> {
        loop {
            if let Some((array, next_row)) = &mut self.current {
                // SAFETY: the stream gave a valid array, which this scan owns until it drops it.
                let view = unsafe { ArrayView::new(&array.0) };
                if *next_row < view.len() {
                    let count = (view.len() - *next_row).min(STANDARD_VECTOR_SIZE);
                    *output = import::import_chunk(&self.schema, &view, *next_row, count, None)?;
                    *next_row += count;
                    return Ok(());
                }
                // read to its end: released
                self.current = None;
            }
            if self.done {
                return Ok(());
            }
            let Some(array) = self.next_array()? else {
                self.done = true;
                return Ok(());
            };
            // SAFETY: as above.
            let view = unsafe { ArrayView::new(&array.0) };
            if view.len() == 0 {
                continue;
            }
            import::validate_shape(&self.schema, &view)?;
            self.current = Some((array, 0));
        }
    }
}

/// `EmptySchemaRelease`: a release that releases nothing.
unsafe extern "C" fn empty_schema_release(schema: *mut ArrowSchema) {
    // SAFETY: C++'s callers pass the schema being released.
    unsafe { (*schema).release = None };
}

/// `EmptyArrayRelease`.
unsafe extern "C" fn empty_array_release(array: *mut ArrowArray) {
    // SAFETY: the array being released.
    unsafe { (*array).release = None };
}

/// `Ingest`: the temporary view `table_name` of the stream.
fn ingest(connection: &crate::database::ConnectionWrapper, table_name: &str, stream: CPointer) -> Result<()> {
    let mut connection = connection.connection.lock();
    connection.register_table_functions(TableUdfSet {
        name: SCAN_FUNCTION.to_string(),
        functions: vec![Arc::new(TableUdfOverload {
            name: SCAN_FUNCTION.to_string(),
            arguments: vec![LogicalType::UBIGINT],
            named_parameters: Vec::new(),
            projection_pushdown: false,
            function: Arc::new(ArrowScanFunction),
        })],
    })?;
    let id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, stream);
    let name = table_name.replace('"', "\"\"");
    let created = connection.query(&format!(
        "CREATE OR REPLACE TEMPORARY VIEW \"{name}\" AS SELECT * FROM {SCAN_FUNCTION}({id}::UBIGINT)"
    ));
    if let Err(error) = created {
        STREAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&id);
        return Err(error);
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_arrow_scan(
    connection: duckdb_connection,
    table_name: *const c_char,
    arrow: duckdb_arrow_stream,
) -> duckdb_state {
    let stream = arrow.cast::<ArrowArrayStream>();
    // SAFETY: C passes a live connection and a NUL-terminated name.
    let (Some(connection), Some(table_name)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(table_name) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live stream, or none.
    let Some(get_schema) = (unsafe { stream.as_ref() }).and_then(|s| s.get_schema) else {
        return DuckDBError;
    };
    let mut schema = ArrowSchema::empty();
    // SAFETY: the stream's callback, called on the stream with a schema to write.
    if unsafe { get_schema(stream, &mut schema) } != 0 {
        return DuckDBError;
    }
    // The children's release functions are made no-ops while the view is made, so nothing downstream releases them
    // on the caller's behalf; they are put back after (Arrow's releases are not recursive, so only the immediate
    // children are).
    let children: Vec<*mut ArrowSchema> = if schema.n_children > 0 && !schema.children.is_null() {
        // SAFETY: the schema has `n_children` children.
        unsafe { std::slice::from_raw_parts(schema.children, schema.n_children as usize) }
            .iter()
            .copied()
            .filter(|child| !child.is_null())
            .collect()
    } else {
        Vec::new()
    };
    let releases: Vec<_> = children
        .iter()
        .map(|&child| {
            // SAFETY: each child is a live schema of the stream's.
            unsafe { std::mem::replace(&mut (*child).release, Some(empty_schema_release)) }
        })
        .collect();
    let name = String::from_utf8_lossy(table_name);
    let ingested = ffi::guard(None, || Some(ingest(connection, &name, CPointer(stream.cast()))));
    for (&child, release) in children.iter().zip(releases) {
        // SAFETY: as above.
        unsafe { (*child).release = release };
    }
    match ingested {
        Some(Ok(())) => DuckDBSuccess,
        _ => DuckDBError,
    }
}

/// The private data of a stream around one array (`arrow_array_stream_wrapper::PrivateData`).
struct ArrayStreamData {
    schema: *mut ArrowSchema,
    array: *mut ArrowArray,
    done: bool,
}

/// `arrow_array_stream_wrapper::GetSchema`: the caller's schema, which the caller keeps owning.
unsafe extern "C" fn array_stream_get_schema(stream: *mut ArrowArrayStream, out: *mut ArrowSchema) -> c_int {
    // SAFETY: the stream is one `duckdb_arrow_array_scan` made, whose private data is an `ArrayStreamData`.
    let data = unsafe { &*(*stream).private_data.cast::<ArrayStreamData>() };
    if data.schema.is_null() {
        return DuckDBError as c_int;
    }
    // SAFETY: the caller's schema is live; the copy C++ makes is a shallow one, with a release that does nothing.
    unsafe {
        out.write(data.schema.read());
        (*out).release = Some(empty_schema_release);
    }
    DuckDBSuccess as c_int
}

/// `arrow_array_stream_wrapper::GetNext`: the caller's array once, then the end.
unsafe extern "C" fn array_stream_get_next(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> c_int {
    // SAFETY: as for `array_stream_get_schema`.
    let data = unsafe { &mut *(*stream).private_data.cast::<ArrayStreamData>() };
    // SAFETY: C passes where the array goes; the caller's array is live (or null, which ends the stream).
    unsafe {
        if data.array.is_null() || data.done {
            out.write(ArrowArray::empty());
        } else {
            out.write(data.array.read());
            (*out).release = Some(empty_array_release);
        }
    }
    data.done = true;
    DuckDBSuccess as c_int
}

/// `arrow_array_stream_wrapper::GetLastError`: the stream gives no errors.
unsafe extern "C" fn array_stream_get_last_error(_stream: *mut ArrowArrayStream) -> *const c_char {
    std::ptr::null()
}

/// `arrow_array_stream_wrapper::Release`.
unsafe extern "C" fn array_stream_release(stream: *mut ArrowArrayStream) {
    // SAFETY: the stream is one `duckdb_arrow_array_scan` made; its private data came from `Box::into_raw` and is
    // freed once, as release sets it to null.
    unsafe {
        let data = (*stream).private_data.cast::<ArrayStreamData>();
        if !data.is_null() {
            drop(Box::from_raw(data));
        }
        (*stream).private_data = std::ptr::null_mut();
        (*stream).release = None;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_arrow_array_scan(
    connection: duckdb_connection,
    table_name: *const c_char,
    arrow_schema: duckdb_arrow_schema,
    arrow_array: duckdb_arrow_array,
    out_stream: *mut duckdb_arrow_stream,
) -> duckdb_state {
    if out_stream.is_null() {
        return DuckDBError;
    }
    let data = Box::new(ArrayStreamData {
        schema: arrow_schema.cast(),
        array: arrow_array.cast(),
        done: false,
    });
    let stream = Box::into_raw(Box::new(ArrowArrayStream {
        get_schema: Some(array_stream_get_schema),
        get_next: Some(array_stream_get_next),
        get_last_error: Some(array_stream_get_last_error),
        release: Some(array_stream_release),
        private_data: Box::into_raw(data).cast(),
    }));
    // SAFETY: C passes where to write the stream, which it destroys with `duckdb_destroy_arrow_stream`; it is not
    // null.
    unsafe { out_stream.write(stream.cast()) };
    // SAFETY: the stream was just made, and lives until C destroys it.
    unsafe { duckdb_arrow_scan(connection, table_name, stream.cast()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_arrow_stream(stream_p: *mut duckdb_arrow_stream) {
    // SAFETY: C passes where its stream handle is, or nothing.
    let Some(slot) = (unsafe { stream_p.as_mut() }) else {
        return;
    };
    let stream = std::mem::replace(slot, std::ptr::null_mut()).cast::<ArrowArrayStream>();
    if stream.is_null() {
        return;
    }
    // SAFETY: the stream is one `duckdb_arrow_array_scan` made (C++ deletes what it is given just as well), live
    // until now; it is released and freed once.
    unsafe {
        if let Some(release) = (*stream).release {
            release(stream);
        }
        drop(Box::from_raw(stream));
    }
}
