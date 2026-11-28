// Ported from duckdb/src/main/capi/v1/scalar_function-c.cpp @ rust-port-baseline

//! User-defined scalar functions (`duckdb_scalar_function`, `duckdb_scalar_function_set`).
//!
//! A function handle holds what the setters set; registering it gives the engine a [`CScalarUdf`], which calls the
//! C callbacks: the bind when a call is bound, the init once per thread that runs the call, and the function on each
//! chunk.

use crate::database::ConnectionWrapper;
use crate::ffi::{self, CPointer, Handle};
use crate::function_info::*;
use crate::types::*;
use crate::vector::{contains_invalid_or_any, prepare_chunk, prepare_vector};
use duckrs::udf::{ScalarUdf, ScalarUdfBindInput, ScalarUdfExecutor, ScalarUdfOverload, ScalarUdfSet};
use duckrs::{DataChunk, ErrorKind, LogicalType, LogicalTypeId, Result};
use duckrs_common::vector::Vector;
use local_states::LocalStates;
use std::ffi::c_char;
use std::sync::Arc;

/// `ScalarFunction` with its `CScalarFunctionInfo`: what `duckdb_create_scalar_function` makes.
#[derive(Clone, Default)]
pub struct ScalarFunctionHandle {
    name: String,
    arguments: Vec<LogicalType>,
    varargs: Option<LogicalType>,
    return_type: Option<LogicalType>,
    special_handling: bool,
    volatile: bool,
    bind: duckdb_scalar_function_bind_t,
    init: duckdb_scalar_function_init_t,
    function: duckdb_scalar_function_t,
    extra: ExtraInfo,
}

/// `ScalarFunctionSet`.
pub struct ScalarFunctionSetHandle {
    name: String,
    functions: Vec<ScalarFunctionHandle>,
}

impl Handle for _duckdb_scalar_function {
    type Target = ScalarFunctionHandle;
}

impl Handle for _duckdb_scalar_function_set {
    type Target = ScalarFunctionSetHandle;
}

/// The callbacks of a registered function (`CScalarFunctionInfo`).
struct CScalarUdf {
    bind: duckdb_scalar_function_bind_t,
    init: duckdb_scalar_function_init_t,
    function: unsafe extern "C" fn(duckdb_function_info, duckdb_data_chunk, duckdb_vector),
    extra: ExtraInfo,
}

/// A bound call (`CScalarFunctionBindData`) and the local states of the threads that run it
/// (`CScalarFunctionLocalState`).
struct CScalarExecutor {
    udf: Arc<CScalarUdf>,
    bind_data: Option<OwnedData>,
    connection_id: u64,
    locals: LocalStates,
}

/// The per-thread local states of a bound call, made by the function's init when a thread first runs it.
mod local_states {
    use super::OwnedData;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;

    #[derive(Default)]
    pub(super) struct LocalStates(Mutex<HashMap<ThreadId, Arc<Mutex<Option<OwnedData>>>>>);

    impl LocalStates {
        /// The state of the current thread, and whether it is new (its init has not run).
        pub(super) fn current(&self) -> (Arc<Mutex<Option<OwnedData>>>, bool) {
            let mut states = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let id = std::thread::current().id();
            match states.get(&id) {
                Some(state) => (state.clone(), false),
                None => {
                    let state = Arc::new(Mutex::new(None));
                    states.insert(id, state.clone());
                    (state, true)
                }
            }
        }
    }
}

impl ScalarUdf for CScalarUdf {
    /// `CScalarFunctionBind`.
    fn bind(&self, input: &ScalarUdfBindInput<'_>) -> Result<Arc<dyn ScalarUdfExecutor>> {
        let mut bind_data = None;
        if let Some(bind) = self.bind {
            let mut info = BindInfo::Scalar(ScalarBindInfo {
                extra: CPointer(extra_ptr(&self.extra)),
                connection_id: input.connection_id,
                arguments: input.arguments.to_vec(),
                bind_data: None,
                copy: None,
                error: None,
            });
            // SAFETY: the bind callback C registered, with an info that lives for the call.
            unsafe { bind(ffi::borrowed_handle(&mut info)) };
            let BindInfo::Scalar(info) = info else {
                unreachable!("a scalar bind info")
            };
            callback_error(ErrorKind::Binder, info.error)?;
            bind_data = info.bind_data;
        }
        Ok(Arc::new(CScalarExecutor {
            udf: Arc::new(CScalarUdf {
                bind: self.bind,
                init: self.init,
                function: self.function,
                extra: self.extra.clone(),
            }),
            bind_data,
            connection_id: input.connection_id,
            locals: LocalStates::default(),
        }))
    }
}

impl ScalarUdfExecutor for CScalarExecutor {
    /// `CScalarFunctionInit` the first time a thread runs the call, then `CAPIScalarFunction`.
    fn execute(&self, arguments: &DataChunk, result: &mut Vector) -> Result<()> {
        let (local, new) = self.locals.current();
        let mut local = local.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if new && let Some(init) = self.udf.init {
            let mut info = InitInfo::Scalar(ScalarInitInfo {
                extra: CPointer(extra_ptr(&self.udf.extra)),
                bind_data: CPointer(OwnedData::ptr(&self.bind_data)),
                connection_id: self.connection_id,
                state: None,
                error: None,
            });
            // SAFETY: the init callback C registered, with an info that lives for the call.
            unsafe { init(ffi::borrowed_handle(&mut info)) };
            let InitInfo::Scalar(info) = info else {
                unreachable!("a scalar init info")
            };
            callback_error(ErrorKind::InvalidInput, info.error)?;
            *local = info.state;
        }
        let count = arguments.len();
        let mut input = arguments.clone();
        prepare_chunk(&mut input);
        prepare_vector(result, count);
        let mut info = FunctionInfo::Scalar(ScalarFunctionInfo {
            extra: CPointer(extra_ptr(&self.udf.extra)),
            bind_data: CPointer(OwnedData::ptr(&self.bind_data)),
            state: CPointer(OwnedData::ptr(&local)),
            error: None,
        });
        // SAFETY: the function C registered, with an info, a chunk and a vector that live for the call.
        unsafe {
            (self.udf.function)(
                ffi::borrowed_handle(&mut info),
                ffi::borrowed_handle(&mut input),
                ffi::borrowed_handle(result),
            )
        };
        let FunctionInfo::Scalar(info) = info else {
            unreachable!("a scalar function info")
        };
        callback_error(ErrorKind::InvalidInput, info.error)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_scalar_function() -> duckdb_scalar_function {
    ffi::into_handle(ScalarFunctionHandle::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_scalar_function(function: *mut duckdb_scalar_function) {
    // SAFETY: C passes a function `duckdb_create_scalar_function` made.
    drop(unsafe { ffi::take(function) });
}

/// The function behind a handle C passes, to change.
///
/// # Safety
///
/// `function` is null or a live scalar function.
unsafe fn function_mut<'a>(function: duckdb_scalar_function) -> Option<&'a mut ScalarFunctionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(function) }
}

/// The logical type behind a handle C passes.
///
/// # Safety
///
/// `ty` is null or a live logical type.
pub(crate) unsafe fn logical_type<'a>(ty: duckdb_logical_type) -> Option<&'a LogicalType> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get(ty) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_name(function: duckdb_scalar_function, name: *const c_char) {
    // SAFETY: C passes a live function and a NUL-terminated name.
    let (Some(function), Some(name)) = (unsafe { function_mut(function) }, unsafe { ffi::c_str(name) }) else {
        return;
    };
    function.name = String::from_utf8_lossy(name).into_owned();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_varargs(function: duckdb_scalar_function, ty: duckdb_logical_type) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    function.varargs = Some(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_special_handling(function: duckdb_scalar_function) {
    // SAFETY: C passes a live function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.special_handling = true;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_volatile(function: duckdb_scalar_function) {
    // SAFETY: C passes a live function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.volatile = true;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_add_parameter(
    function: duckdb_scalar_function,
    ty: duckdb_logical_type,
) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    // v1 declares no parameter name, so the parameter is positional-only; it goes ahead of any varargs
    function.arguments.push(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_return_type(
    function: duckdb_scalar_function,
    ty: duckdb_logical_type,
) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    function.return_type = Some(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_extra_info(
    function: duckdb_scalar_function,
    extra_info: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a live function.
    let Some(function) = (unsafe { function_mut(function) }) else {
        return;
    };
    if extra_info.is_null() {
        return;
    }
    function.extra = Some(Arc::new(OwnedData::new(extra_info, destroy)));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_bind(
    function: duckdb_scalar_function,
    bind: duckdb_scalar_function_bind_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, bind) {
        function.bind = bind;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_init(
    function: duckdb_scalar_function,
    init: duckdb_scalar_function_init_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, init) {
        function.init = init;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_function(
    function: duckdb_scalar_function,
    execute: duckdb_scalar_function_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, execute) {
        function.function = execute;
    }
}

/// The scalar bind info behind a handle C passes to a bind callback.
///
/// # Safety
///
/// `info` is null or the info of the bind callback that runs.
unsafe fn bind_info<'a>(info: duckdb_bind_info) -> Option<&'a mut ScalarBindInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(BindInfo::Scalar(info)) => Some(info),
        _ => None,
    }
}

/// The scalar init info behind a handle C passes to an init callback.
///
/// # Safety
///
/// `info` is null or the info of the init callback that runs.
unsafe fn init_info<'a>(info: duckdb_init_info) -> Option<&'a mut ScalarInitInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(InitInfo::Scalar(info)) => Some(info),
        _ => None,
    }
}

/// The scalar function info behind a handle C passes to a function callback.
///
/// # Safety
///
/// `info` is null or the info of the function callback that runs.
unsafe fn function_info<'a>(info: duckdb_function_info) -> Option<&'a mut ScalarFunctionInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(FunctionInfo::Scalar(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_get_extra_info(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_bind_get_extra_info(info: duckdb_bind_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { bind_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_get_bind_data(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_get_client_context(
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
pub unsafe extern "C" fn duckdb_scalar_function_set_error(info: duckdb_function_info, error: *const c_char) {
    // SAFETY: C passes the info of the function that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { function_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_bind_set_error(info: duckdb_bind_info, error: *const c_char) {
    // SAFETY: C passes the info of the bind that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { bind_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_bind_get_argument_count(info: duckdb_bind_info) -> idx_t {
    // SAFETY: C passes the info of the bind that runs.
    unsafe { bind_info(info) }.map_or(0, |info| info.arguments.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_bind_get_argument(
    info: duckdb_bind_info,
    index: idx_t,
) -> duckdb_expression {
    // SAFETY: C passes the info of the bind that runs.
    let Some(info) = (unsafe { bind_info(info) }) else {
        return std::ptr::null_mut();
    };
    match info.arguments.get(index as usize) {
        Some(argument) => ffi::into_handle(argument.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_bind_data(
    info: duckdb_bind_info,
    bind_data: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the bind that runs.
    if let Some(info) = unsafe { bind_info(info) } {
        info.bind_data = Some(OwnedData::new(bind_data, destroy));
    }
}

/// duckrs shares the bind data of a call among its copies rather than copying it, so the copy callback is kept but
/// does not run.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_set_bind_data_copy(
    info: duckdb_bind_info,
    copy: duckdb_copy_callback_t,
) {
    // SAFETY: C passes the info of the bind that runs.
    if let Some(info) = unsafe { bind_info(info) } {
        info.copy = copy;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_get_state(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the function that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.state.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_init_set_error(info: duckdb_init_info, error: *const c_char) {
    // SAFETY: C passes the info of the init that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { init_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_init_set_state(
    info: duckdb_init_info,
    state: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes the info of the init that runs.
    if let Some(info) = unsafe { init_info(info) } {
        info.state = Some(OwnedData::new(state, destroy));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_init_get_client_context(
    info: duckdb_init_info,
    out_context: *mut duckdb_client_context,
) {
    // SAFETY: C passes the info of the init that runs.
    let Some(info) = (unsafe { init_info(info) }) else {
        return;
    };
    // SAFETY: C passes where to write the context.
    unsafe { write_client_context(out_context, info.connection_id) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_init_get_bind_data(info: duckdb_init_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.map_or(std::ptr::null_mut(), |info| info.bind_data.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_scalar_function_init_get_extra_info(info: duckdb_init_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the init that runs.
    unsafe { init_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_scalar_function_set(name: *const c_char) -> duckdb_scalar_function_set {
    // SAFETY: C passes a NUL-terminated name.
    match unsafe { ffi::c_str(name) } {
        Some(name) if !name.is_empty() => ffi::into_handle(ScalarFunctionSetHandle {
            name: String::from_utf8_lossy(name).into_owned(),
            functions: Vec::new(),
        }),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_scalar_function_set(set: *mut duckdb_scalar_function_set) {
    // SAFETY: C passes a set `duckdb_create_scalar_function_set` made.
    drop(unsafe { ffi::take(set) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_add_scalar_function_to_set(
    set: duckdb_scalar_function_set,
    function: duckdb_scalar_function,
) -> duckdb_state {
    // SAFETY: C passes a live set and a live function.
    let (Some(set), Some(function)) = (unsafe { ffi::get_mut(set) }, unsafe { ffi::get(function) }) else {
        return DuckDBError;
    };
    set.functions.push(function.clone());
    DuckDBSuccess
}

/// `TypeVisitor::Contains(type, INVALID)`.
fn contains_invalid(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Invalid
        || ty.child_type().is_some_and(contains_invalid)
        || (matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Union)
            && ty.struct_children().iter().any(|(_, child)| contains_invalid(child)))
}

/// The checks of `duckdb_register_scalar_function_set`: an overload the engine can register, or `None`.
fn overload(name: &str, function: &ScalarFunctionHandle) -> Option<ScalarUdfOverload> {
    let execute = function.function?;
    let return_type = function.return_type.clone()?;
    if function.name.is_empty() || contains_invalid_or_any(&return_type) {
        return None;
    }
    if function.arguments.iter().any(contains_invalid) {
        return None;
    }
    Some(ScalarUdfOverload {
        name: name.to_string(),
        arguments: function.arguments.clone(),
        varargs: function.varargs.clone(),
        return_type,
        special_handling: function.special_handling,
        volatile: function.volatile,
        function: Arc::new(CScalarUdf {
            bind: function.bind,
            init: function.init,
            function: execute,
            extra: function.extra.clone(),
        }),
    })
}

/// Registers `functions` under `name` on the database of `connection`.
///
/// # Safety
///
/// `connection` is null or a live connection.
unsafe fn register(connection: duckdb_connection, name: &str, functions: &[ScalarFunctionHandle]) -> duckdb_state {
    // SAFETY: by the contract of this function.
    let Some(connection) = (unsafe { ffi::get::<_duckdb_connection>(connection) }) else {
        return DuckDBError;
    };
    let ConnectionWrapper { connection, .. } = connection;
    let Some(overloads) = functions.iter().map(|f| overload(name, f)).collect::<Option<Vec<_>>>() else {
        return DuckDBError;
    };
    if overloads.is_empty() {
        return DuckDBError;
    }
    let set = ScalarUdfSet {
        name: name.to_string(),
        functions: overloads,
    };
    match ffi::guard(None, || Some(connection.lock().register_scalar_functions(set))) {
        Some(Ok(())) => DuckDBSuccess,
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_scalar_function(
    connection: duckdb_connection,
    function: duckdb_scalar_function,
) -> duckdb_state {
    // SAFETY: C passes a live function.
    let Some(function) = (unsafe { ffi::get(function) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live connection.
    unsafe { register(connection, &function.name, std::slice::from_ref(function)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_scalar_function_set(
    connection: duckdb_connection,
    set: duckdb_scalar_function_set,
) -> duckdb_state {
    // SAFETY: C passes a live set.
    let Some(set) = (unsafe { ffi::get(set) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live connection.
    unsafe { register(connection, &set.name, &set.functions) }
}
