// Ported from duckdb/src/main/capi/v1/cast_function-c.cpp and the `duckdb_register_logical_type` of
// duckdb/src/main/capi/v1/logical_types-c.cpp @ rust-port-baseline

//! User-defined casts (`duckdb_cast_function`) and custom types (`duckdb_register_logical_type`).
//!
//! A cast handle holds what the setters set; registering it gives the engine a [`CCastUdf`], which calls the C
//! function on the rows of each vector the cast runs on (`CAPICastFunction`).

use crate::database::ConnectionWrapper;
use crate::ffi::{self, CPointer, Handle};
use crate::function_info::*;
use crate::scalar_function::logical_type;
use crate::types::*;
use crate::vector::{contains_invalid_or_any, prepare_vector};
use duckrs::udf::{CastUdf, UserCast};
use duckrs::{Error, ErrorKind, LogicalType, Result};
use duckrs_common::vector::Vector;
use std::ffi::c_char;
use std::sync::Arc;

/// `CCastFunction`: what `duckdb_create_cast_function` makes.
pub struct CastFunctionHandle {
    source: Option<LogicalType>,
    target: Option<LogicalType>,
    implicit_cast_cost: i64,
    function: duckdb_cast_function_t,
    extra: ExtraInfo,
}

impl Handle for _duckdb_cast_function {
    type Target = CastFunctionHandle;
}

/// A registered cast (`CCastFunctionData`).
struct CCastUdf {
    function: unsafe extern "C" fn(duckdb_function_info, idx_t, duckdb_vector, duckdb_vector) -> bool,
    extra: ExtraInfo,
}

impl CastUdf for CCastUdf {
    /// `CAPICastFunction`.
    fn cast(&self, source: &Vector, result: &mut Vector, count: usize, try_cast: bool) -> Result<()> {
        let mut input = source.clone();
        prepare_vector(result, count);
        prepare_vector(&mut input, count);
        let mut info = FunctionInfo::Cast(CastFunctionInfo {
            extra: CPointer(extra_ptr(&self.extra)),
            try_cast,
            error: None,
        });
        // SAFETY: the function C registered, with an info and vectors that live for the call.
        let success = unsafe {
            (self.function)(
                ffi::borrowed_handle(&mut info),
                count as idx_t,
                ffi::borrowed_handle(&mut input),
                ffi::borrowed_handle(result),
            )
        };
        let FunctionInfo::Cast(info) = info else {
            unreachable!("a cast function info")
        };
        // `HandleCastError::AssignError`: a strict cast throws, a TRY_CAST keeps the message
        if !success && !try_cast {
            return Err(Error::new(ErrorKind::Conversion, info.error.unwrap_or_default()));
        }
        Ok(())
    }
}

/// The cast behind a handle C passes, to change.
///
/// # Safety
///
/// `cast` is null or a live cast function.
unsafe fn cast_mut<'a>(cast: duckdb_cast_function) -> Option<&'a mut CastFunctionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(cast) }
}

/// The cast info behind a handle C passes to a cast function.
///
/// # Safety
///
/// `info` is null or the info of the cast function that runs.
unsafe fn cast_info<'a>(info: duckdb_function_info) -> Option<&'a mut CastFunctionInfo> {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get_mut(info) } {
        Some(FunctionInfo::Cast(info)) => Some(info),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_cast_function() -> duckdb_cast_function {
    ffi::into_handle(CastFunctionHandle {
        source: None,
        target: None,
        implicit_cast_cost: -1,
        function: None,
        extra: None,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_source_type(cast: duckdb_cast_function, ty: duckdb_logical_type) {
    // SAFETY: C passes a live cast function and a live logical type.
    if let (Some(cast), Some(ty)) = (unsafe { cast_mut(cast) }, unsafe { logical_type(ty) }) {
        cast.source = Some(ty.clone());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_target_type(cast: duckdb_cast_function, ty: duckdb_logical_type) {
    // SAFETY: C passes a live cast function and a live logical type.
    if let (Some(cast), Some(ty)) = (unsafe { cast_mut(cast) }, unsafe { logical_type(ty) }) {
        cast.target = Some(ty.clone());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_implicit_cast_cost(cast: duckdb_cast_function, cost: i64) {
    // SAFETY: C passes a live cast function.
    if let Some(cast) = unsafe { cast_mut(cast) } {
        cast.implicit_cast_cost = cost;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_function(
    cast: duckdb_cast_function,
    function: duckdb_cast_function_t,
) {
    // SAFETY: C passes a live cast function.
    if let (Some(cast), Some(_)) = (unsafe { cast_mut(cast) }, function) {
        cast.function = function;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_get_cast_mode(info: duckdb_function_info) -> duckdb_cast_mode {
    // SAFETY: C passes the info of the cast that runs.
    match unsafe { cast_info(info) } {
        Some(info) if info.try_cast => DUCKDB_CAST_TRY,
        _ => DUCKDB_CAST_NORMAL,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_error(info: duckdb_function_info, error: *const c_char) {
    // SAFETY: C passes the info of the cast that runs, and a NUL-terminated error.
    if let Some(info) = unsafe { cast_info(info) } {
        // SAFETY: as above.
        unsafe { set_error(&mut info.error, error) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_row_error(
    info: duckdb_function_info,
    error: *const c_char,
    row: idx_t,
    output: duckdb_vector,
) {
    // SAFETY: C passes the info of the cast that runs, and a NUL-terminated error.
    let Some(info) = (unsafe { cast_info(info) }) else {
        return;
    };
    // SAFETY: as above.
    unsafe { set_error(&mut info.error, error) };
    // SAFETY: C passes the output vector of the cast, or none.
    if let Some(output) = unsafe { ffi::get_mut(output) } {
        // `FlatVector::SetNull`
        if (row as usize) < output.capacity().max(1) {
            output.set_null(row as usize, true);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_set_extra_info(
    cast: duckdb_cast_function,
    extra_info: *mut std::ffi::c_void,
    destroy: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a live cast function.
    let Some(cast) = (unsafe { cast_mut(cast) }) else {
        return;
    };
    if extra_info.is_null() {
        return;
    }
    cast.extra = Some(Arc::new(OwnedData::new(extra_info, destroy)));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_cast_function_get_extra_info(info: duckdb_function_info) -> *mut std::ffi::c_void {
    // SAFETY: C passes the info of the cast that runs.
    unsafe { cast_info(info) }.map_or(std::ptr::null_mut(), |info| info.extra.0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_cast_function(
    connection: duckdb_connection,
    cast: duckdb_cast_function,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a live cast function.
    let (Some(connection), Some(cast)) = (unsafe { ffi::get::<_duckdb_connection>(connection) }, unsafe {
        ffi::get(cast)
    }) else {
        return DuckDBError;
    };
    let (Some(source), Some(target), Some(function)) = (&cast.source, &cast.target, cast.function) else {
        return DuckDBError;
    };
    if contains_invalid_or_any(source) || contains_invalid_or_any(target) {
        return DuckDBError;
    }
    let ConnectionWrapper { connection, .. } = connection;
    let user_cast = UserCast {
        source: source.clone(),
        target: target.clone(),
        implicit_cast_cost: cast.implicit_cast_cost,
        function: Arc::new(CCastUdf {
            function,
            extra: cast.extra.clone(),
        }),
    };
    match ffi::guard(None, || Some(connection.lock().register_cast(user_cast))) {
        Some(()) => DuckDBSuccess,
        None => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_cast_function(cast: *mut duckdb_cast_function) {
    // SAFETY: C passes a cast function `duckdb_create_cast_function` made.
    drop(unsafe { ffi::take(cast) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_logical_type(
    connection: duckdb_connection,
    ty: duckdb_logical_type,
    _info: duckdb_create_type_info,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a live logical type.
    let (Some(connection), Some(ty)) = (unsafe { ffi::get::<_duckdb_connection>(connection) }, unsafe {
        logical_type(ty)
    }) else {
        return DuckDBError;
    };
    let Some(name) = ty.alias() else {
        return DuckDBError;
    };
    if contains_invalid_or_any(ty) {
        return DuckDBError;
    }
    let ConnectionWrapper { connection, .. } = connection;
    match ffi::guard(None, || Some(connection.lock().register_type(name, ty.clone()))) {
        Some(Ok(())) => DuckDBSuccess,
        _ => DuckDBError,
    }
}
