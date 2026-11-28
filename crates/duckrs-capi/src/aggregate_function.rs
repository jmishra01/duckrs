// Ported from duckdb/src/main/capi/v1/aggregate_function-c.cpp @ rust-port-baseline

//! User-defined aggregate functions (`duckdb_aggregate_function`, `duckdb_aggregate_function_set`).
//!
//! A state is a buffer of the size the function's `state_size` gives, which its `init` fills, and which its
//! `destructor` cleans up when duckrs drops the state. The engine hands the update a chunk of rows with the state of
//! each, and the combine and the finalize arrays of states, as C++'s `CAPIAggregateUpdate` ... do.

use crate::database::ConnectionWrapper;
use crate::ffi::{self, CPointer, Handle};
use crate::function_info::*;
use crate::scalar_function::logical_type;
use crate::types::*;
use crate::vector::{contains_invalid_or_any, prepare_chunk, prepare_vector};
use duckrs::udf::{AggregateUdf, UdfState, user_aggregate_function, user_aggregate_set};
use duckrs::{DataChunk, ErrorKind, LogicalType, LogicalTypeId, Result};
use duckrs_common::vector::Vector;
use std::ffi::c_char;
use std::sync::{Arc, OnceLock};

/// `AggregateFunction` with its `CAggregateFunctionInfo`: what `duckdb_create_aggregate_function` makes.
#[derive(Clone, Default)]
pub struct AggregateFunctionHandle {
    name: String,
    arguments: Vec<LogicalType>,
    return_type: Option<LogicalType>,
    special_handling: bool,
    state_size: duckdb_aggregate_state_size,
    init: duckdb_aggregate_init_t,
    update: duckdb_aggregate_update_t,
    combine: duckdb_aggregate_combine_t,
    finalize: duckdb_aggregate_finalize_t,
    destroy: duckdb_aggregate_destroy_t,
    extra: ExtraInfo,
}

/// `AggregateFunctionSet`.
pub struct AggregateFunctionSetHandle {
    name: String,
    functions: Vec<AggregateFunctionHandle>,
}

impl Handle for _duckdb_aggregate_function {
    type Target = AggregateFunctionHandle;
}

impl Handle for _duckdb_aggregate_function_set {
    type Target = AggregateFunctionSetHandle;
}

/// The callbacks of a registered function (`CAggregateFunctionInfo`).
struct CAggregateUdf {
    state_size: unsafe extern "C" fn(duckdb_function_info) -> idx_t,
    init: unsafe extern "C" fn(duckdb_function_info, duckdb_aggregate_state),
    update: unsafe extern "C" fn(duckdb_function_info, duckdb_data_chunk, *mut duckdb_aggregate_state),
    combine: unsafe extern "C" fn(duckdb_function_info, *mut duckdb_aggregate_state, *mut duckdb_aggregate_state, idx_t),
    finalize: unsafe extern "C" fn(duckdb_function_info, *mut duckdb_aggregate_state, duckdb_vector, idx_t, idx_t),
    destroy: duckdb_aggregate_destroy_t,
    extra: ExtraInfo,
    /// `AggregateStateSize`, asked once
    size: OnceLock<usize>,
}

/// The state of a group: a buffer C reads and writes as its state struct, aligned for any field of it.
struct CAggregateState {
    buffer: Vec<u64>,
    destroy: duckdb_aggregate_destroy_t,
}

impl CAggregateState {
    fn ptr(&mut self) -> duckdb_aggregate_state {
        self.buffer.as_mut_ptr().cast()
    }
}

impl Drop for CAggregateState {
    /// `CAPIAggregateDestructor`.
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            let mut state = self.ptr();
            // SAFETY: the destructor C registered, given the one state, which init made and nothing uses any more.
            unsafe { destroy(&mut state, 1) };
        }
    }
}

/// The C state of a state the engine holds.
fn c_state(state: &mut UdfState) -> duckdb_aggregate_state {
    state
        .downcast_mut::<CAggregateState>()
        .expect("the state of a C aggregate")
        .ptr()
}

impl CAggregateUdf {
    /// Runs `call` with a function info, and gives the error the callback set (`CAggregateExecuteInfo`).
    fn with_info<R>(&self, call: impl FnOnce(duckdb_function_info) -> R) -> Result<R> {
        let mut info = FunctionInfo::Aggregate(AggregateFunctionInfo {
            extra: CPointer(extra_ptr(&self.extra)),
            error: None,
        });
        let result = call(ffi::borrowed_handle(&mut info));
        let FunctionInfo::Aggregate(info) = info else { unreachable!("an aggregate function info") };
        callback_error(ErrorKind::InvalidInput, info.error)?;
        Ok(result)
    }

    /// `CAPIAggregateStateSize`.
    fn size(&self) -> Result<usize> {
        if let Some(size) = self.size.get() {
            return Ok(*size);
        }
        // SAFETY: the state size callback C registered, with an info that lives for the call.
        let size = self.with_info(|info| unsafe { (self.state_size)(info) })? as usize;
        Ok(*self.size.get_or_init(|| size))
    }
}

impl AggregateUdf for CAggregateUdf {
    /// `CAPIAggregateStateInit` of one state.
    fn new_state(&self) -> Result<UdfState> {
        let words = self.size()?.div_ceil(size_of::<u64>()).max(1);
        let mut state = CAggregateState {
            buffer: vec![0; words],
            destroy: None,
        };
        let ptr = state.ptr();
        // SAFETY: the init callback C registered, with an info that lives for the call and a state of the size C
        // asked for.
        self.with_info(|info| unsafe { (self.init)(info, ptr) })?;
        // only a state that init made is destroyed
        state.destroy = self.destroy;
        Ok(Box::new(state))
    }

    /// `CAPIAggregateUpdate`.
    fn update(&self, input: &DataChunk, states: &mut [&mut UdfState], rows: &[usize]) -> Result<()> {
        let mut input = input.clone();
        prepare_chunk(&mut input);
        let groups: Vec<duckdb_aggregate_state> = states.iter_mut().map(|state| c_state(state)).collect();
        let mut row_states: Vec<duckdb_aggregate_state> = rows.iter().map(|&group| groups[group]).collect();
        // SAFETY: the update callback C registered, with an info and a chunk that live for the call, and a state for
        // each row of the chunk.
        self.with_info(|info| unsafe {
            (self.update)(info, ffi::borrowed_handle(&mut input), row_states.as_mut_ptr())
        })
    }

    /// `CAPIAggregateCombine`.
    fn combine(&self, sources: &mut [&mut UdfState], targets: &mut [&mut UdfState]) -> Result<()> {
        let mut source: Vec<duckdb_aggregate_state> = sources.iter_mut().map(|state| c_state(state)).collect();
        let mut target: Vec<duckdb_aggregate_state> = targets.iter_mut().map(|state| c_state(state)).collect();
        let count = source.len().min(target.len()) as idx_t;
        // SAFETY: the combine callback C registered, with an info that lives for the call and `count` states on
        // each side.
        self.with_info(|info| unsafe { (self.combine)(info, source.as_mut_ptr(), target.as_mut_ptr(), count) })
    }

    /// `CAPIAggregateFinalize`.
    fn finalize(&self, states: &mut [&mut UdfState], result: &mut Vector) -> Result<()> {
        let mut source: Vec<duckdb_aggregate_state> = states.iter_mut().map(|state| c_state(state)).collect();
        let count = source.len();
        prepare_vector(result, count);
        // SAFETY: the finalize callback C registered, with an info and a result vector that live for the call, and
        // `count` states, the result having a row for each.
        self.with_info(|info| unsafe {
            (self.finalize)(info, source.as_mut_ptr(), ffi::borrowed_handle(result), count as idx_t, 0)
        })
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_aggregate_function() -> duckdb_aggregate_function {
    ffi::into_handle(AggregateFunctionHandle::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_aggregate_function(function: *mut duckdb_aggregate_function) {
    // SAFETY: C passes a function `duckdb_create_aggregate_function` made.
    drop(unsafe { ffi::take(function) });
}

/// The function behind a handle C passes, to change.
///
/// # Safety
///
/// `function` is null or a live aggregate function.
unsafe fn function_mut<'a>(function: duckdb_aggregate_function) -> Option<&'a mut AggregateFunctionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(function) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_name(function: duckdb_aggregate_function, name: *const c_char) {
    // SAFETY: C passes a live function and a NUL-terminated name.
    let (Some(function), Some(name)) = (unsafe { function_mut(function) }, unsafe { ffi::c_str(name) }) else {
        return;
    };
    function.name = String::from_utf8_lossy(name).into_owned();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_add_parameter(
    function: duckdb_aggregate_function,
    ty: duckdb_logical_type,
) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    function.arguments.push(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_return_type(
    function: duckdb_aggregate_function,
    ty: duckdb_logical_type,
) {
    // SAFETY: C passes a live function and a live logical type.
    let (Some(function), Some(ty)) = (unsafe { function_mut(function) }, unsafe { logical_type(ty) }) else {
        return;
    };
    function.return_type = Some(ty.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_functions(
    function: duckdb_aggregate_function,
    state_size: duckdb_aggregate_state_size,
    state_init: duckdb_aggregate_init_t,
    update: duckdb_aggregate_update_t,
    combine: duckdb_aggregate_combine_t,
    finalize: duckdb_aggregate_finalize_t,
) {
    // SAFETY: C passes a live function.
    let Some(function) = (unsafe { function_mut(function) }) else {
        return;
    };
    if state_size.is_none() || state_init.is_none() || update.is_none() || combine.is_none() || finalize.is_none() {
        return;
    }
    function.state_size = state_size;
    function.init = state_init;
    function.update = update;
    function.combine = combine;
    function.finalize = finalize;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_destructor(
    function: duckdb_aggregate_function,
    destroy: duckdb_aggregate_destroy_t,
) {
    // SAFETY: C passes a live function.
    if let (Some(function), Some(_)) = (unsafe { function_mut(function) }, destroy) {
        function.destroy = destroy;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_special_handling(function: duckdb_aggregate_function) {
    // SAFETY: C passes a live function.
    if let Some(function) = unsafe { function_mut(function) } {
        function.special_handling = true;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_extra_info(
    function: duckdb_aggregate_function,
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

/// The aggregate function info behind a handle C passes to a callback.
///
/// # Safety
///
/// `info` is null or the info of the callback that runs.
unsafe fn function_info<'a>(info: duckdb_function_info) -> Option<&'a mut AggregateFunctionInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(FunctionInfo::Aggregate(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_get_extra_info(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the callback that runs.
    unsafe { function_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_aggregate_function_set_error(info: duckdb_function_info, error: *const c_char) {
    // SAFETY: C passes the info of the callback that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { function_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_aggregate_function_set(name: *const c_char) -> duckdb_aggregate_function_set {
    // SAFETY: C passes a NUL-terminated name.
    match unsafe { ffi::c_str(name) } {
        Some(name) if !name.is_empty() => ffi::into_handle(AggregateFunctionSetHandle {
            name: String::from_utf8_lossy(name).into_owned(),
            functions: Vec::new(),
        }),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_aggregate_function_set(set: *mut duckdb_aggregate_function_set) {
    // SAFETY: C passes a set `duckdb_create_aggregate_function_set` made.
    drop(unsafe { ffi::take(set) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_add_aggregate_function_to_set(
    set: duckdb_aggregate_function_set,
    function: duckdb_aggregate_function,
) -> duckdb_state {
    // SAFETY: C passes a live set and a live function.
    let (Some(set), Some(function)) = (unsafe { ffi::get_mut(set) }, unsafe { ffi::get(function) }) else {
        return DuckDBError;
    };
    set.functions.push(function.clone());
    DuckDBSuccess
}

/// The checks of `duckdb_register_aggregate_function_set`: the engine's overload, or `None`.
fn overload(name: &str, function: &AggregateFunctionHandle) -> Option<duckrs::udf::AggregateFunction> {
    let (Some(state_size), Some(init), Some(update), Some(combine), Some(finalize)) = (
        function.state_size,
        function.init,
        function.update,
        function.combine,
        function.finalize,
    ) else {
        return None;
    };
    let return_type = function.return_type.clone()?;
    if function.name.is_empty() || contains_invalid_or_any(&return_type) {
        return None;
    }
    if function.arguments.iter().any(|ty| ty.id() == LogicalTypeId::Invalid) {
        return None;
    }
    let udf = CAggregateUdf {
        state_size,
        init,
        update,
        combine,
        finalize,
        destroy: function.destroy,
        extra: function.extra.clone(),
        size: OnceLock::new(),
    };
    Some(user_aggregate_function(
        name,
        function.arguments.clone(),
        return_type,
        Arc::new(udf),
    ))
}

/// Registers `functions` under `name` on the database of `connection`.
///
/// # Safety
///
/// `connection` is null or a live connection.
unsafe fn register(connection: duckdb_connection, name: &str, functions: &[AggregateFunctionHandle]) -> duckdb_state {
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
    let set = user_aggregate_set(name, overloads);
    match ffi::guard(None, || Some(connection.lock().register_aggregate_functions(set))) {
        Some(Ok(())) => DuckDBSuccess,
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_aggregate_function(
    connection: duckdb_connection,
    function: duckdb_aggregate_function,
) -> duckdb_state {
    // SAFETY: C passes a live function.
    let Some(function) = (unsafe { ffi::get(function) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live connection.
    unsafe { register(connection, &function.name, std::slice::from_ref(function)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_aggregate_function_set(
    connection: duckdb_connection,
    set: duckdb_aggregate_function_set,
) -> duckdb_state {
    // SAFETY: C passes a live set.
    let Some(set) = (unsafe { ffi::get(set) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live connection.
    unsafe { register(connection, &set.name, &set.functions) }
}
