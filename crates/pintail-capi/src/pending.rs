// Ported from duckdb/src/main/capi/v1/pending-c.cpp @ rust-port-baseline

//! `duckdb_pending_result` (`PendingStatementWrapper`). pintail runs a statement to its end in one call, so a pending
//! result has run when it is made: each task it is asked to execute finds it ready.

use crate::error::c_string;
use crate::ffi::{self, Handle};
use crate::prepared::prepared_wrapper;
use crate::result::{streams_by_default, translate};
use crate::types::*;
use std::ffi::{CString, c_char};

/// `PendingStatementWrapper`: the result of the statement, until `duckdb_execute_pending` takes it.
pub struct PendingWrapper {
    result: Option<(pintail::Result<pintail::QueryResult>, pintail::StatementProperties)>,
    /// `ErrorData::Message` of the statement's error
    error: Option<CString>,
    allow_streaming: bool,
}

impl Handle for _duckdb_pending_result {
    type Target = PendingWrapper;
}

/// `duckdb_pending_prepared_internal`.
///
/// # Safety
///
/// `prepared_statement` is null or a live prepared statement; `out_result` is null or valid for a write.
unsafe fn pending(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_pending_result,
    allow_streaming: bool,
) -> duckdb_state {
    // SAFETY: by the contract of this function.
    let Some(wrapper) = (unsafe { prepared_wrapper(prepared_statement) }) else {
        return DuckDBError;
    };
    if out_result.is_null() {
        return DuckDBError;
    }
    let Some((result, properties)) = wrapper.run() else {
        return DuckDBError;
    };
    let error = result.as_ref().err().map(|error| c_string(&error.to_string()));
    let state = if error.is_none() { DuckDBSuccess } else { DuckDBError };
    let pending = PendingWrapper {
        allow_streaming: allow_streaming && streams_by_default(&properties),
        result: Some((result, properties)),
        error,
    };
    // SAFETY: by the contract of this function; it is not null.
    unsafe { ffi::write(out_result, ffi::into_handle(pending)) };
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_pending_prepared(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_pending_result,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement and where to write the pending result.
    unsafe { pending(prepared_statement, out_result, false) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_pending_prepared_streaming(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_pending_result,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement and where to write the pending result.
    unsafe { pending(prepared_statement, out_result, true) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_pending(pending_result: *mut duckdb_pending_result) {
    // SAFETY: C passes a pending result `duckdb_pending_prepared` made.
    drop(unsafe { ffi::take(pending_result) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_pending_error(pending_result: duckdb_pending_result) -> *const c_char {
    // SAFETY: C passes a live pending result.
    match unsafe { ffi::get(pending_result) }.and_then(|p| p.error.as_ref()) {
        Some(error) => error.as_ptr(),
        None => std::ptr::null(),
    }
}

/// The state of a pending result: ready, unless its statement failed (or its result was taken).
///
/// # Safety
///
/// `pending_result` is null or a live pending result.
unsafe fn state(pending_result: duckdb_pending_result) -> duckdb_pending_state {
    // SAFETY: by the contract of this function.
    match unsafe { ffi::get(pending_result) } {
        Some(pending) if pending.error.is_none() && pending.result.is_some() => DUCKDB_PENDING_RESULT_READY,
        _ => DUCKDB_PENDING_ERROR,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_pending_execute_task(pending_result: duckdb_pending_result) -> duckdb_pending_state {
    // SAFETY: C passes a live pending result.
    unsafe { state(pending_result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_pending_execute_check_state(
    pending_result: duckdb_pending_result,
) -> duckdb_pending_state {
    // SAFETY: C passes a live pending result.
    unsafe { state(pending_result) }
}

/// `IsObservable`: a state C stops polling at.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_pending_execution_is_finished(pending_state: duckdb_pending_state) -> bool {
    !matches!(
        pending_state,
        DUCKDB_PENDING_RESULT_NOT_READY | DUCKDB_PENDING_NO_TASKS_AVAILABLE
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_pending(
    pending_result: duckdb_pending_result,
    out_result: *mut duckdb_result,
) -> duckdb_state {
    // SAFETY: C passes a live pending result and where to write the result.
    let (Some(pending), Some(out)) = (unsafe { ffi::get_mut(pending_result) }, unsafe {
        ffi::reference_mut(out_result)
    }) else {
        return DuckDBError;
    };
    *out = duckdb_result::zeroed();
    let Some((result, properties)) = pending.result.take() else {
        return DuckDBError;
    };
    translate(result, Some(properties), Some(out), pending.allow_streaming)
}
