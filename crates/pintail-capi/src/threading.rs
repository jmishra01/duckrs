// Ported from duckdb/src/main/capi/v1/threading-c.cpp @ rust-port-baseline

//! Running the tasks of a database on threads of the client (`duckdb_execute_tasks`, `duckdb_task_state`).
//!
//! pintail runs a statement to its end on its own threads when it is executed, and keeps no queue of tasks for other
//! threads to take: a thread the client gives finds no task to run. `duckdb_execute_tasks_state` waits, as C++'s
//! `TaskScheduler::ExecuteForever` does, until `duckdb_finish_execution` ends it.

use crate::ffi;
use crate::types::*;
use std::sync::{Condvar, Mutex};

/// `CAPITaskState`: whether the threads given to the database should keep running (`marker`).
#[derive(Default)]
pub struct TaskState {
    finished: Mutex<bool>,
    wake: Condvar,
}

/// The task state behind a pointer C passes.
///
/// # Safety
///
/// `state` is null or a state `duckdb_create_task_state` made and not destroyed.
unsafe fn task_state<'a>(state: duckdb_task_state) -> Option<&'a TaskState> {
    // SAFETY: by the contract of this function.
    unsafe { state.cast::<TaskState>().as_ref() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_tasks(database: duckdb_database, _max_tasks: idx_t) {
    // SAFETY: C passes a live database.
    let _ = unsafe { ffi::get(database) };
    // no task waits for a thread of the client
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_task_state(database: duckdb_database) -> duckdb_task_state {
    // SAFETY: C passes a live database.
    if unsafe { ffi::get(database) }.is_none() {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(TaskState::default())).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_tasks_state(state: duckdb_task_state) {
    // SAFETY: C passes a live task state.
    let Some(state) = (unsafe { task_state(state) }) else {
        return;
    };
    // `ExecuteForever`: until the execution is finished
    let mut finished = state.finished.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while !*finished {
        finished = state
            .wake
            .wait(finished)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_n_tasks_state(state: duckdb_task_state, _max_tasks: idx_t) -> idx_t {
    // SAFETY: C passes a live task state; there is no task to run.
    let _ = unsafe { task_state(state) };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_finish_execution(state: duckdb_task_state) {
    // SAFETY: C passes a live task state.
    let Some(state) = (unsafe { task_state(state) }) else {
        return;
    };
    *state.finished.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    state.wake.notify_all();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_task_state_is_finished(state: duckdb_task_state) -> bool {
    // SAFETY: C passes a live task state.
    unsafe { task_state(state) }
        .is_some_and(|state| *state.finished.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_task_state(state: duckdb_task_state) {
    if state.is_null() {
        return;
    }
    // SAFETY: C passes a state `duckdb_create_task_state` made, which it destroys once.
    drop(unsafe { Box::from_raw(state.cast::<TaskState>()) });
}

/// `ClientContext::ExecutionIsFinished`: a statement runs to its end in the call that executes it, so there is no
/// execution of the connection left to finish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execution_is_finished(connection: duckdb_connection) -> bool {
    // SAFETY: C passes a live connection.
    unsafe { ffi::get(connection) }.is_some()
}
