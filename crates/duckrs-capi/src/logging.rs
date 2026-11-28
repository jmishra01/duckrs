// Ported from duckdb/src/main/capi/v1/logging-c.cpp @ rust-port-baseline

//! Log storages a client writes (`duckdb_log_storage`): each log entry of the database goes to its callback once
//! `SET logging_storage` selects it.

use crate::database::DatabaseWrapper;
use crate::error::c_string;
use crate::ffi::{self, Handle};
use crate::function_info::OwnedData;
use crate::types::*;
use duckrs::logging::{LogLevel, LogStorageCallback};
use std::ffi::c_char;
use std::sync::Arc;

/// `LogStorageWrapper`: what `duckdb_create_log_storage` makes.
#[derive(Default)]
pub struct LogStorageHandle {
    name: String,
    write_log_entry: duckdb_logger_write_log_entry_t,
    /// the extra data, until a registration takes it over
    extra_data: Option<OwnedData>,
}

impl Handle for _duckdb_log_storage {
    type Target = LogStorageHandle;
}

/// `CallbackLogStorage`.
struct CLogStorage {
    write_log_entry:
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut duckdb_timestamp, *const c_char, *const c_char, *const c_char),
    /// freed when the database lets go of the storage (`~CallbackLogStorage`)
    extra_data: Option<OwnedData>,
}

impl LogStorageCallback for CLogStorage {
    /// `CallbackLogStorage::WriteLogEntry`.
    fn write_entry(&self, timestamp: i64, level: LogLevel, log_type: &str, message: &str) {
        let mut timestamp = duckdb_timestamp { micros: timestamp };
        let (level, log_type, message) = (c_string(level.name()), c_string(log_type), c_string(message));
        let extra = OwnedData::ptr(&self.extra_data);
        // SAFETY: the callback C registered, with its extra data and strings that live for the call.
        unsafe {
            (self.write_log_entry)(
                extra,
                &mut timestamp,
                level.as_ptr(),
                log_type.as_ptr(),
                message.as_ptr(),
            )
        };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_log_storage() -> duckdb_log_storage {
    ffi::into_handle(LogStorageHandle::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_log_storage(log_storage: *mut duckdb_log_storage) {
    // SAFETY: C passes a log storage `duckdb_create_log_storage` made; its extra data, if no registration took it,
    // goes with it.
    drop(unsafe { ffi::take(log_storage) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_log_storage_set_write_log_entry(
    log_storage: duckdb_log_storage,
    function: duckdb_logger_write_log_entry_t,
) {
    // SAFETY: C passes a live log storage.
    if let (Some(storage), Some(_)) = (unsafe { ffi::get_mut(log_storage) }, function) {
        storage.write_log_entry = function;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_log_storage_set_extra_data(
    log_storage: duckdb_log_storage,
    extra_data: *mut std::ffi::c_void,
    delete_callback: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a live log storage.
    if let Some(storage) = unsafe { ffi::get_mut(log_storage) } {
        storage.extra_data = Some(OwnedData::new(extra_data, delete_callback));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_log_storage_set_name(log_storage: duckdb_log_storage, name: *const c_char) {
    // SAFETY: C passes a live log storage and a NUL-terminated name.
    if let (Some(storage), Some(name)) = (unsafe { ffi::get_mut(log_storage) }, unsafe { ffi::c_str(name) }) {
        storage.name = String::from_utf8_lossy(name).into_owned();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_log_storage(
    database: duckdb_database,
    log_storage: duckdb_log_storage,
) -> duckdb_state {
    // SAFETY: C passes a live database and a live log storage.
    let (Some(database), Some(storage)) = (unsafe { ffi::get::<_duckdb_database>(database) }, unsafe {
        ffi::get_mut(log_storage)
    }) else {
        return DuckDBError;
    };
    let database: &DatabaseWrapper = database;
    let Some(write_log_entry) = storage.write_log_entry else {
        return DuckDBError;
    };
    if storage.name.is_empty() {
        return DuckDBError;
    }
    let callback = Arc::new(CLogStorage {
        write_log_entry,
        extra_data: storage.extra_data.take(),
    });
    let registered = ffi::guard(false, || {
        database
            .database()
            .register_log_storage(&storage.name, callback.clone())
    });
    if !registered {
        // the storage was not taken: the extra data stays with the handle
        if let Ok(callback) = Arc::try_unwrap(callback) {
            let CLogStorage { extra_data, .. } = callback;
            storage.extra_data = extra_data;
        }
        return DuckDBError;
    }
    DuckDBSuccess
}
