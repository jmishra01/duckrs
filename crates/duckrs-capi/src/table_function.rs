// Ported from duckdb/src/main/capi/v1/table_function-c.cpp and
// duckdb/src/include/duckdb/main/capi/capi_internal_table.hpp @ rust-port-baseline

//! User-defined table functions (`duckdb_table_function`).
//!
//! Registering a function gives the engine a [`CTableUdf`]: its bind runs the C bind, which gives the columns and
//! the bind data; the scan runs the C init (and local init) when it starts, and then the C function for each chunk
//! until it gives an empty one.

use crate::database::ConnectionWrapper;
use crate::ffi::{self, CPointer, Handle};
use crate::function_info::*;
use crate::scalar_function::logical_type;
use crate::types::*;
use crate::vector::{contains_invalid_or_any, prepare_vector};
use duckrs::udf::{TableUdf, TableUdfBindInput, TableUdfBinding, TableUdfOverload, TableUdfRun, TableUdfScan, TableUdfSet};
use duckrs::{DataChunk, ErrorKind, LogicalType, LogicalTypeId, Result};
use std::ffi::c_char;
use std::sync::Arc;

/// `TableFunction` with its `CTableFunctionInfo`: what `duckdb_create_table_function` makes.
#[derive(Clone, Default)]
pub struct TableFunctionHandle {
    name: String,
    arguments: Vec<LogicalType>,
    named_parameters: Vec<(String, LogicalType)>,
    projection_pushdown: bool,
    bind: duckdb_table_function_bind_t,
    init: duckdb_table_function_init_t,
    local_init: duckdb_table_function_init_t,
    function: duckdb_table_function_t,
    extra: ExtraInfo,
}

impl Handle for _duckdb_table_function {
    type Target = TableFunctionHandle;
}

type InitCallback = unsafe extern "C" fn(duckdb_init_info);

/// The callbacks of a registered function (`CTableFunctionInfo`).
struct CTableUdf {
    bind: unsafe extern "C" fn(duckdb_bind_info),
    init: InitCallback,
    local_init: Option<InitCallback>,
    function: unsafe extern "C" fn(duckdb_function_info, duckdb_data_chunk),
    extra: ExtraInfo,
}

/// A bound call (`CTableBindData`).
struct CTableScan {
    udf: Arc<CTableUdf>,
    bind_data: Option<OwnedData>,
}

/// A running scan: its global and local init data (`CTableGlobalInitData`, `CTableLocalInitData`).
struct CTableRun {
    scan: Arc<CTableScan>,
    init_data: Option<OwnedData>,
    local_data: Option<OwnedData>,
}

impl CTableScan {
    /// `CTableFunctionInit` or `CTableFunctionLocalInit`: the init data `init` set.
    fn run_init(&self, init: InitCallback, column_ids: &[usize]) -> Result<Option<OwnedData>> {
        let mut info = InitInfo::Table(TableInitInfo {
            extra: CPointer(extra_ptr(&self.udf.extra)),
            bind_data: CPointer(OwnedData::ptr(&self.bind_data)),
            column_ids: column_ids.iter().map(|&c| c as idx_t).collect(),
            init_data: None,
            max_threads: 1,
            error: None,
        });
        // SAFETY: the init callback C registered, with an info that lives for the call.
        unsafe { init(ffi::borrowed_handle(&mut info)) };
        let InitInfo::Table(info) = info else { unreachable!("a table init info") };
        callback_error(ErrorKind::InvalidInput, info.error)?;
        Ok(info.init_data)
    }
}

/// The scan of a bound call, which the engine holds as a [`TableUdfScan`].
struct SharedScan(Arc<CTableScan>);

/// A registered function, which the engine holds as a [`TableUdf`].
struct SharedUdf(Arc<CTableUdf>);

impl TableUdf for SharedUdf {
    /// `CTableFunctionBind`.
    fn bind(&self, input: TableUdfBindInput) -> Result<TableUdfBinding> {
        let mut info = BindInfo::Table(TableBindInfo {
            extra: CPointer(extra_ptr(&self.0.extra)),
            connection_id: input.connection_id,
            parameters: input.parameters,
            named_parameters: input.named_parameters,
            names: Vec::new(),
            c_names: Vec::new(),
            types: Vec::new(),
            bind_data: None,
            cardinality: None,
            error: None,
        });
        // SAFETY: the bind callback C registered, with an info that lives for the call.
        unsafe { (self.0.bind)(ffi::borrowed_handle(&mut info)) };
        let BindInfo::Table(info) = info else { unreachable!("a table bind info") };
        callback_error(ErrorKind::Binder, info.error)?;
        let scan = Arc::new(CTableScan {
            udf: self.0.clone(),
            bind_data: info.bind_data,
        });
        Ok(TableUdfBinding {
            names: info.names,
            types: info.types,
            cardinality: info.cardinality,
            scan: Arc::new(SharedScan(scan)),
        })
    }
}

impl TableUdfScan for SharedScan {
    fn init(&self, column_ids: &[usize]) -> Result<Box<dyn TableUdfRun>> {
        let scan = &self.0;
        let init_data = scan.run_init(scan.udf.init, column_ids)?;
        let local_data = match scan.udf.local_init {
            Some(local_init) => scan.run_init(local_init, column_ids)?,
            None => None,
        };
        Ok(Box::new(CTableRun {
            scan: scan.clone(),
            init_data,
            local_data,
        }))
    }
}

impl TableUdfRun for CTableRun {
    /// `CTableFunction`.
    fn next(&mut self, output: &mut DataChunk) -> Result<()> {
        let count = output.capacity();
        for column in output.columns_mut() {
            prepare_vector(column, count);
        }
        let mut info = FunctionInfo::Table(TableFunctionInfo {
            extra: CPointer(extra_ptr(&self.scan.udf.extra)),
            bind_data: CPointer(OwnedData::ptr(&self.scan.bind_data)),
            init_data: CPointer(OwnedData::ptr(&self.init_data)),
            local_data: CPointer(OwnedData::ptr(&self.local_data)),
            error: None,
        });
        // SAFETY: the function C registered, with an info and a chunk that live for the call.
        unsafe { (self.scan.udf.function)(ffi::borrowed_handle(&mut info), ffi::borrowed_handle(output)) };
        let FunctionInfo::Table(info) = info else { unreachable!("a table function info") };
        callback_error(ErrorKind::InvalidInput, info.error)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_table_function() -> duckdb_table_function {
    ffi::into_handle(TableFunctionHandle::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_table_function(function: *mut duckdb_table_function) {
    // SAFETY: C passes a function `duckdb_create_table_function` made.
    drop(unsafe { ffi::take(function) });
}

/// The function behind a handle C passes, to change.
///
/// # Safety
///
/// `function` is null or a live table function.
unsafe fn function_mut<'a>(function: duckdb_table_function) -> Option<&'a mut TableFunctionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(function) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_name(function: duckdb_table_function, name: *const c_char) {
    // SAFETY: C passes a live function and a NUL-terminated name.
    let (Some(function), Some(name)) = (unsafe { function_mut(function) }, unsafe { ffi::c_str(name) }) else {
        return;
    };
    function.name = String::from_utf8_lossy(name).into_owned();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_add_parameter(function: duckdb_table_function, ty: duckdb_logical_type) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    // v1 declares no parameter name, so the parameter is positional-only
    function.arguments.push(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_add_named_parameter(
    function: duckdb_table_function,
    name: *const c_char,
    ty: duckdb_logical_type,
) {
    // SAFETY: C passes a live function, a NUL-terminated name and a live logical type.
    let (Some(function), Some(name), Some(ty)) =
        (unsafe { function_mut(function) }, unsafe { ffi::c_str(name) }, unsafe { logical_type(ty) })
    else {
        return;
    };
    let name = String::from_utf8_lossy(name).into_owned();
    // an option that is there already keeps its type
    if !function.named_parameters.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
        function.named_parameters.push((name, ty.clone()));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_extra_info(
    function: duckdb_table_function,
    extra_info: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a live function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.extra = Some(Arc::new(OwnedData::new(extra_info, destroy)));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_bind(function: duckdb_table_function, bind: duckdb_table_function_bind_t) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, bind) {
        function.bind = bind;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_init(function: duckdb_table_function, init: duckdb_table_function_init_t) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, init) {
        function.init = init;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_local_init(
    function: duckdb_table_function,
    init: duckdb_table_function_init_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, init) {
        function.local_init = init;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_set_function(
    function: duckdb_table_function,
    execute: duckdb_table_function_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, execute) {
        function.function = execute;
    }
}

/// duckrs gives a table function every column, which a function that supports projection pushdown handles as
/// well: its init reads which columns it gives (all of them).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_supports_projection_pushdown(function: duckdb_table_function, pushdown: bool) {
    // SAFETY: C passes a live function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.projection_pushdown = pushdown;
    }
}

/// `TypeVisitor::Contains(type, INVALID)`.
fn contains_invalid(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Invalid
        || ty.child_type().is_some_and(contains_invalid)
        || (matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Union)
            && ty.struct_children().iter().any(|(_, child)| contains_invalid(child)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_table_function(
    connection: duckdb_connection,
    function: duckdb_table_function,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a live function.
    let (Some(connection), Some(function)) =
        (unsafe { ffi::get::<_duckdb_connection>(connection) }, unsafe { ffi::get(function) })
    else {
        return DuckDBError;
    };
    let ConnectionWrapper { connection } = connection;
    let (Some(bind), Some(init), Some(execute)) = (function.bind, function.init, function.function) else {
        return DuckDBError;
    };
    if function.name.is_empty()
        || function.arguments.iter().any(contains_invalid)
        || function.named_parameters.iter().any(|(_, ty)| contains_invalid(ty))
    {
        return DuckDBError;
    }
    let udf = Arc::new(CTableUdf {
        bind,
        init,
        local_init: function.local_init,
        function: execute,
        extra: function.extra.clone(),
    });
    let set = TableUdfSet {
        name: function.name.clone(),
        functions: vec![Arc::new(TableUdfOverload {
            name: function.name.clone(),
            arguments: function.arguments.clone(),
            named_parameters: function.named_parameters.clone(),
            projection_pushdown: function.projection_pushdown,
            function: Arc::new(SharedUdf(udf)),
        })],
    };
    match ffi::guard(None, || Some(connection.lock().register_table_functions(set))) {
        Some(Ok(())) => DuckDBSuccess,
        _ => DuckDBError,
    }
}

// --------------------------------------------------------------------------------------------------------------
// Bind interface
// --------------------------------------------------------------------------------------------------------------

/// The table bind info behind a handle C passes to a bind callback.
///
/// # Safety
///
/// `info` is null or the info of the bind callback that runs.
unsafe fn bind_info<'a>(info: duckdb_bind_info) -> Option<&'a mut TableBindInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(BindInfo::Table(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_get_extra_info(info: duckdb_bind_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { bind_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_get_client_context(
    info: duckdb_bind_info,
    out_context: *mut duckdb_client_context,
) {
    // SAFETY: C passes the info of the bind that runs.
    let Some(info) = (unsafe { bind_info(info) }) else {
        return;
    };
    // SAFETY: C passes where to write the context.
    unsafe { write_client_context(out_context, info.connection_id) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_add_result_column(info: duckdb_bind_info, name: *const c_char, ty: duckdb_logical_type) {
    // SAFETY: C passes the info of the bind that runs, a NUL-terminated name and a live logical type.
    let (Some(info), Some(name), Some(ty)) =
        (unsafe { bind_info(info) }, unsafe { ffi::c_str(name) }, unsafe { logical_type(ty) })
    else {
        return;
    };
    if contains_invalid_or_any(ty) {
        return;
    }
    info.names.push(String::from_utf8_lossy(name).into_owned());
    info.types.push(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_bind_get_result_column_count(info: duckdb_bind_info) -> idx_t {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { bind_info(info) }.map_or(0, |info| info.types.len() as idx_t)
}

/// The name of result column `col_idx`, which lives as long as the bind runs; null without one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_bind_get_result_column_name(
    info: duckdb_bind_info,
    col_idx: idx_t,
) -> *const c_char {
    // SAFETY: C passes the info of the bind that runs.
    let Some(info) = (unsafe { bind_info(info) }) else {
        return std::ptr::null();
    };
    let Some(name) = info.names.get(col_idx as usize) else {
        return std::ptr::null();
    };
    let index = col_idx as usize;
    if info.c_names.len() <= index {
        info.c_names.resize(index + 1, None);
    }
    info.c_names[index]
        .get_or_insert_with(|| crate::error::c_string(name))
        .as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_function_bind_get_result_column_type(
    info: duckdb_bind_info,
    col_idx: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes the info of the bind that runs.
    let Some(info) = (unsafe { bind_info(info) }) else {
        return std::ptr::null_mut();
    };
    info.types
        .get(col_idx as usize)
        .map_or(std::ptr::null_mut(), |ty| ffi::into_handle(ty.clone()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { bind_info(info) }.map_or(0, |info| info.parameters.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_get_parameter(info: duckdb_bind_info, index: idx_t) -> duckdb_value {
    // SAFETY: C passes the info of the bind that runs.
    let Some(info) = (unsafe { bind_info(info) }) else {
        return std::ptr::null_mut();
    };
    match info.parameters.get(index as usize) {
        Some(value) => ffi::into_handle(value.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_get_named_parameter(info: duckdb_bind_info, name: *const c_char) -> duckdb_value {
    // SAFETY: C passes the info of the bind that runs and a NUL-terminated name.
    let (Some(info), Some(name)) = (unsafe { bind_info(info) }, unsafe { ffi::c_str(name) }) else {
        return std::ptr::null_mut();
    };
    let name = String::from_utf8_lossy(name);
    match info.named_parameters.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
        Some((_, value)) => ffi::into_handle(value.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_set_bind_data(
    info: duckdb_bind_info,
    bind_data: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the bind that runs.
    if let Some(info) = unsafe { bind_info(info) } {
        info.bind_data = Some(OwnedData::new(bind_data, destroy));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_set_cardinality(info: duckdb_bind_info, cardinality: idx_t, is_exact: bool) {
    // SAFETY: C passes the info of the bind that runs.
    if let Some(info) = unsafe { bind_info(info) } {
        info.cardinality = Some((cardinality, is_exact));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char) {
    // SAFETY: C passes the info of the bind that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { bind_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

// --------------------------------------------------------------------------------------------------------------
// Init interface
// --------------------------------------------------------------------------------------------------------------

/// The table init info behind a handle C passes to an init callback.
///
/// # Safety
///
/// `info` is null or the info of the init callback that runs.
unsafe fn init_info<'a>(info: duckdb_init_info) -> Option<&'a mut TableInitInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(InitInfo::Table(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_get_extra_info(info: duckdb_init_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_set_init_data(
    info: duckdb_init_info,
    init_data: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the init that runs.
    if let Some(info) = unsafe { init_info(info) } {
        info.init_data = Some(OwnedData::new(init_data, destroy));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char) {
    // SAFETY: C passes the info of the init that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { init_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.map_or(0, |info| info.column_ids.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_get_column_index(info: duckdb_init_info, column_index: idx_t) -> idx_t {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.and_then(|info| info.column_ids.get(column_index as usize).copied()).unwrap_or(0)
}

/// duckrs scans a table function on one thread; the count is kept for the init data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_init_set_max_threads(info: duckdb_init_info, max_threads: idx_t) {
    // SAFETY: C passes the info of the init that runs.
    if let Some(info) = unsafe { init_info(info) } {
        info.max_threads = max_threads;
    }
}

// --------------------------------------------------------------------------------------------------------------
// Function interface
// --------------------------------------------------------------------------------------------------------------

/// The table function info behind a handle C passes to a function callback.
///
/// # Safety
///
/// `info` is null or the info of the function callback that runs.
unsafe fn function_info<'a>(info: duckdb_function_info) -> Option<&'a mut TableFunctionInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(FunctionInfo::Table(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_function_get_extra_info(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.init_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_function_get_local_init_data(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.local_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_function_set_error(info: duckdb_function_info, error: *const c_char) {
    // SAFETY: C passes the info of the function that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { function_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}
