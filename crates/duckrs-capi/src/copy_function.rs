// Ported from duckdb/src/main/capi/v1/copy_function-c.cpp @ rust-port-baseline

//! User-defined copy functions (`duckdb_copy_function`): what `COPY ... TO ... (FORMAT name)` and
//! `COPY ... FROM ... (FORMAT name)` run.
//!
//! A copy function handle holds what the setters set; registering it gives the engine a [`CCopyToUdf`] for COPY TO,
//! which calls the C bind when the COPY is bound and the global init, sink and finalize when it runs, and the table
//! function of `duckdb_copy_function_set_copy_from_function` for COPY FROM.

use crate::database::ConnectionWrapper;
use crate::ffi::{self, CPointer, Handle};
use crate::function_info::*;
use crate::table_function::TableFunctionHandle;
use crate::types::*;
use crate::vector::prepare_chunk;
use duckrs::udf::{CopyToBindInput, CopyToBound, CopyToSink, CopyToUdf, TableUdfOverload, UserCopyFunction};
use duckrs::{DataChunk, Error, ErrorKind, LogicalType, Result, Value};
use std::ffi::{CString, c_char};
use std::sync::Arc;

/// `CopyFunction` with its `CCopyFunctionInfo`: what `duckdb_create_copy_function` makes.
#[derive(Default)]
pub struct CopyFunctionHandle {
    name: String,
    callbacks: CopyCallbacks,
    copy_from: Option<TableUdfOverload>,
}

/// The callbacks and the extra info of a copy function (`CCopyFunctionInfo`).
#[derive(Clone, Default)]
struct CopyCallbacks {
    bind: duckdb_copy_function_bind_t,
    global_init: duckdb_copy_function_global_init_t,
    sink: duckdb_copy_function_sink_t,
    finalize: duckdb_copy_function_finalize_t,
    extra: ExtraInfo,
}

impl Handle for _duckdb_copy_function {
    type Target = CopyFunctionHandle;
}

/// `CCopyFunctionToInternalBindInfo`.
pub(crate) struct CopyBindInfo {
    extra: CPointer,
    connection_id: u64,
    types: Vec<LogicalType>,
    options: Vec<(String, Vec<Value>)>,
    bind_data: Option<OwnedData>,
    error: CallbackError,
}

/// `CCopyToGlobalInitInfo`.
pub(crate) struct CopyGlobalInitInfo {
    extra: CPointer,
    bind_data: CPointer,
    connection_id: u64,
    file_path: CString,
    global_state: Option<OwnedData>,
    error: CallbackError,
}

/// `CCopyToSinkInfo` and `CCopyToFinalizeInfo`.
pub(crate) struct CopySinkInfo {
    extra: CPointer,
    bind_data: CPointer,
    global_state: CPointer,
    connection_id: u64,
    error: CallbackError,
}

impl Handle for _duckdb_copy_function_bind_info {
    type Target = CopyBindInfo;
}

impl Handle for _duckdb_copy_function_global_init_info {
    type Target = CopyGlobalInitInfo;
}

impl Handle for _duckdb_copy_function_sink_info {
    type Target = CopySinkInfo;
}

impl Handle for _duckdb_copy_function_finalize_info {
    type Target = CopySinkInfo;
}

/// The COPY TO of a registered copy function.
struct CCopyToUdf(CopyCallbacks);

/// A bound COPY TO (`CCopyToBindInfo`).
struct BoundCopy {
    callbacks: CopyCallbacks,
    bind_data: Option<OwnedData>,
    connection_id: u64,
}

/// [`BoundCopy`], shared with the running COPY.
struct CCopyToBound(Arc<BoundCopy>);

/// A running COPY TO (`CCopyToGlobalState`).
struct CCopyToRun {
    bound: Arc<BoundCopy>,
    global_state: Option<OwnedData>,
}

impl CopyToUdf for CCopyToUdf {
    /// `CCopyToBind`.
    fn bind(&self, input: CopyToBindInput) -> Result<Arc<dyn CopyToBound>> {
        let mut bind_data = None;
        if let Some(bind) = self.0.bind {
            let mut info = CopyBindInfo {
                extra: CPointer(extra_ptr(&self.0.extra)),
                connection_id: input.connection_id,
                types: input.types,
                options: input.options,
                bind_data: None,
                error: None,
            };
            // SAFETY: the bind callback C registered, with an info that lives for the call.
            unsafe { bind(ffi::borrowed_handle(&mut info)) };
            callback_error(ErrorKind::Binder, info.error)?;
            bind_data = info.bind_data;
        }
        Ok(Arc::new(CCopyToBound(Arc::new(BoundCopy {
            callbacks: self.0.clone(),
            bind_data,
            connection_id: input.connection_id,
        }))))
    }
}

impl CopyToBound for CCopyToBound {
    /// `CCopyToGlobalInit`.
    fn initialize(&self, path: &str) -> Result<Box<dyn CopyToSink>> {
        let bound = &self.0;
        let mut global_state = None;
        if let Some(global_init) = bound.callbacks.global_init {
            let mut info = CopyGlobalInitInfo {
                extra: CPointer(extra_ptr(&bound.callbacks.extra)),
                bind_data: CPointer(OwnedData::ptr(&bound.bind_data)),
                connection_id: bound.connection_id,
                file_path: crate::error::c_string(path),
                global_state: None,
                error: None,
            };
            // SAFETY: the global init callback C registered, with an info that lives for the call.
            unsafe { global_init(ffi::borrowed_handle(&mut info)) };
            callback_error(ErrorKind::InvalidInput, info.error)?;
            global_state = info.global_state;
        }
        Ok(Box::new(CCopyToRun {
            bound: bound.clone(),
            global_state,
        }))
    }
}

impl CCopyToRun {
    fn info(&self) -> CopySinkInfo {
        CopySinkInfo {
            extra: CPointer(extra_ptr(&self.bound.callbacks.extra)),
            bind_data: CPointer(OwnedData::ptr(&self.bound.bind_data)),
            global_state: CPointer(OwnedData::ptr(&self.global_state)),
            connection_id: self.bound.connection_id,
            error: None,
        }
    }
}

impl CopyToSink for CCopyToRun {
    /// `CCopyToSink`: the chunk, flattened.
    fn sink(&mut self, chunk: &DataChunk) -> Result<()> {
        let Some(sink) = self.bound.callbacks.sink else {
            return Err(Error::new(ErrorKind::Internal, "a copy function without a sink"));
        };
        let mut input = chunk.clone();
        prepare_chunk(&mut input);
        let mut info = self.info();
        // SAFETY: the sink callback C registered, with an info and a chunk that live for the call.
        unsafe { sink(ffi::borrowed_handle(&mut info), ffi::borrowed_handle(&mut input)) };
        callback_error(ErrorKind::InvalidInput, info.error)
    }

    /// `CCopyToFinalize`.
    fn finalize(&mut self) -> Result<()> {
        let Some(finalize) = self.bound.callbacks.finalize else {
            return Ok(());
        };
        let mut info = self.info();
        // SAFETY: the finalize callback C registered, with an info that lives for the call.
        unsafe { finalize(ffi::borrowed_handle(&mut info)) };
        callback_error(ErrorKind::InvalidInput, info.error)
    }
}

/// `MakeValueFromCopyOptions`: a STRUCT of the options, by their names in upper case; an option without a value is
/// NULL, one with several values a LIST of them (or a STRUCT of them when their types differ). NULL without options.
fn options_value(options: &[(String, Vec<Value>)]) -> Value {
    let mut fields = Vec::with_capacity(options.len());
    for (name, values) in options {
        let name = name.to_uppercase();
        let value = match values.as_slice() {
            [] => Value::sql_null(),
            [value] => value.clone(),
            [first, ..] => {
                let ty = first.logical_type().clone();
                if values.iter().all(|v| *v.logical_type() == ty) {
                    Value::list(ty, values.clone()).unwrap_or_else(|_| Value::sql_null())
                } else {
                    Value::struct_value(values.iter().map(|v| (String::new(), v.clone())).collect())
                }
            }
        };
        fields.push((name, value));
    }
    if fields.is_empty() {
        return Value::sql_null();
    }
    Value::struct_value(fields)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_copy_function() -> duckdb_copy_function {
    ffi::into_handle(CopyFunctionHandle::default())
}

/// The copy function behind a handle C passes, to change.
///
/// # Safety
///
/// `function` is null or a live copy function.
unsafe fn function_mut<'a>(function: duckdb_copy_function) -> Option<&'a mut CopyFunctionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(function) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_name(function: duckdb_copy_function, name: *const c_char) {
    // SAFETY: C passes a live copy function and a NUL-terminated name.
    if let (Some(function), Some(name)) = (unsafe { function_mut(function) }, unsafe { ffi::c_str(name) }) {
        function.name = String::from_utf8_lossy(name).into_owned();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_copy_function(function: *mut duckdb_copy_function) {
    // SAFETY: C passes a copy function `duckdb_create_copy_function` made.
    drop(unsafe { ffi::take(function) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_extra_info(
    function: duckdb_copy_function,
    extra_info: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a live copy function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.callbacks.extra = Some(Arc::new(OwnedData::new(extra_info, destroy)));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_bind(function: duckdb_copy_function, bind: duckdb_copy_function_bind_t) {
    // SAFETY: C passes a live copy function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, bind) {
        function.callbacks.bind = bind;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_global_init(
    function: duckdb_copy_function,
    init: duckdb_copy_function_global_init_t,
) {
    // SAFETY: C passes a live copy function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, init) {
        function.callbacks.global_init = init;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_sink(function: duckdb_copy_function, sink: duckdb_copy_function_sink_t) {
    // SAFETY: C passes a live copy function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, sink) {
        function.callbacks.sink = sink;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_finalize(
    function: duckdb_copy_function,
    finalize: duckdb_copy_function_finalize_t,
) {
    // SAFETY: C passes a live copy function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, finalize) {
        function.callbacks.finalize = finalize;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_set_copy_from_function(
    function: duckdb_copy_function,
    table_function: duckdb_table_function,
) {
    // SAFETY: C passes a live copy function and a live table function.
    let (Some(function), Some(table_function)) = (unsafe { function_mut(function) }, unsafe {
        ffi::get::<_duckdb_table_function>(table_function)
    }) else {
        return;
    };
    let table_function: &TableFunctionHandle = table_function;
    // the table function takes the name of the copy function if it has none
    let name = if table_function.name().is_empty() {
        function.name.clone()
    } else {
        table_function.name().to_string()
    };
    if let Some(overload) = crate::table_function::overload(table_function, &name) {
        function.copy_from = Some(overload);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_copy_function(
    connection: duckdb_connection,
    function: duckdb_copy_function,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a live copy function.
    let (Some(connection), Some(function)) = (unsafe { ffi::get::<_duckdb_connection>(connection) }, unsafe {
        ffi::get(function)
    }) else {
        return DuckDBError;
    };
    if function.name.is_empty() {
        return DuckDBError;
    }
    let copy_to = function
        .callbacks
        .sink
        .map(|_| Arc::new(CCopyToUdf(function.callbacks.clone())) as Arc<dyn CopyToUdf>);
    if copy_to.is_none() && function.copy_from.is_none() {
        return DuckDBError;
    }
    let ConnectionWrapper { connection } = connection;
    let copy = UserCopyFunction {
        name: function.name.clone(),
        copy_to,
        copy_from: function.copy_from.clone().map(Arc::new),
    };
    match ffi::guard(None, || Some(connection.lock().register_copy_function(copy))) {
        Some(()) => DuckDBSuccess,
        None => DuckDBError,
    }
}

// --------------------------------------------------------------------------------------------------------------
// Bind
// --------------------------------------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_set_error(info: duckdb_copy_function_bind_info, error: *const c_char) {
    // SAFETY: C passes the info of the bind that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_get_extra_info(
    info: duckdb_copy_function_bind_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_get_client_context(
    info: duckdb_copy_function_bind_info,
) -> duckdb_client_context {
    // SAFETY: C passes the info of the bind that runs.
    match unsafe { ffi::get(info) } {
        Some(info) => new_client_context(info.connection_id),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_get_column_count(info: duckdb_copy_function_bind_info) -> idx_t {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { ffi::get(info) }.map_or(0, |info| info.types.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_get_column_type(
    info: duckdb_copy_function_bind_info,
    col_idx: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes the info of the bind that runs.
    match unsafe { ffi::get(info) }.and_then(|info| info.types.get(col_idx as usize)) {
        Some(ty) => ffi::into_handle(ty.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_get_options(info: duckdb_copy_function_bind_info) -> duckdb_value {
    // SAFETY: C passes the info of the bind that runs.
    match unsafe { ffi::get(info) } {
        Some(info) => ffi::into_handle::<_duckdb_value>(options_value(&info.options)),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_bind_set_bind_data(
    info: duckdb_copy_function_bind_info,
    bind_data: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the bind that runs.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        info.bind_data = Some(OwnedData::new(bind_data, destroy));
    }
}

/// A new client context of `connection_id` (`new CClientContextWrapper(context)`), which C destroys.
fn new_client_context(connection_id: u64) -> duckdb_client_context {
    ffi::into_handle(ClientContextWrapper { connection_id })
}

// --------------------------------------------------------------------------------------------------------------
// Global init
// --------------------------------------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_set_error(
    info: duckdb_copy_function_global_init_info,
    error: *const c_char,
) {
    // SAFETY: C passes the info of the global init that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_get_extra_info(
    info: duckdb_copy_function_global_init_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the global init that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_get_client_context(
    info: duckdb_copy_function_global_init_info,
) -> duckdb_client_context {
    // SAFETY: C passes the info of the global init that runs.
    match unsafe { ffi::get(info) } {
        Some(info) => new_client_context(info.connection_id),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_get_bind_data(
    info: duckdb_copy_function_global_init_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the global init that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_set_global_state(
    info: duckdb_copy_function_global_init_info,
    global_state: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the global init that runs.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        info.global_state = Some(OwnedData::new(global_state, destroy));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_global_init_get_file_path(
    info: duckdb_copy_function_global_init_info,
) -> *const c_char {
    // SAFETY: C passes the info of the global init that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null(), |info| info.file_path.as_ptr())
}

// --------------------------------------------------------------------------------------------------------------
// Sink and finalize
// --------------------------------------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_sink_set_error(info: duckdb_copy_function_sink_info, error: *const c_char) {
    // SAFETY: C passes the info of the sink that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_sink_get_extra_info(
    info: duckdb_copy_function_sink_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the sink that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_sink_get_client_context(
    info: duckdb_copy_function_sink_info,
) -> duckdb_client_context {
    // SAFETY: C passes the info of the sink that runs.
    match unsafe { ffi::get(info) } {
        Some(info) => new_client_context(info.connection_id),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_sink_get_bind_data(
    info: duckdb_copy_function_sink_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the sink that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_sink_get_global_state(
    info: duckdb_copy_function_sink_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the sink that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.global_state.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_finalize_set_error(
    info: duckdb_copy_function_finalize_info,
    error: *const c_char,
) {
    // SAFETY: C passes the info of the finalize that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { ffi::get_mut(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_finalize_get_extra_info(
    info: duckdb_copy_function_finalize_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the finalize that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_finalize_get_client_context(
    info: duckdb_copy_function_finalize_info,
) -> duckdb_client_context {
    // SAFETY: C passes the info of the finalize that runs.
    match unsafe { ffi::get(info) } {
        Some(info) => new_client_context(info.connection_id),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_finalize_get_bind_data(
    info: duckdb_copy_function_finalize_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the finalize that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_copy_function_finalize_get_global_state(
    info: duckdb_copy_function_finalize_info,
) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the finalize that runs.
    unsafe { ffi::get(info) }.map_or(std::ptr::null_mut(), |info| info.global_state.0)
}
