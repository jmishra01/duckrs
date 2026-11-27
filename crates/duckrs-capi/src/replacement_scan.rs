// Ported from duckdb/src/main/capi/v1/replacement_scan-c.cpp @ rust-port-baseline

//! Replacement scans (`duckdb_add_replacement_scan`): a C callback that, for a table name the catalog does not
//! have, names a table function to call in its place, with its parameters.
//!
//! The scan is registered on the database (`DBConfig::replacement_scans`), so every connection to it sees it. The
//! engine calls it as a [`duckrs::Replacement`] scan: the C callback gets the table name and a
//! [`ReplacementScanInfo`] to fill, and what it set becomes a call of the table function, as
//! `duckdb_capi_replacement_callback` makes a `TableFunctionRef`.

use crate::database::DatabaseWrapper;
use crate::ffi::{self, Handle};
use crate::function_info::OwnedData;
use crate::types::*;
use duckrs::{Error, ErrorKind, Replacement, Value};
use std::ffi::{CString, c_char, c_void};
use std::sync::Arc;

/// `CAPIReplacementScanInfo`: what the callback says to read for the table.
#[derive(Debug, Default)]
pub struct ReplacementScanInfo {
    function_name: String,
    parameters: Vec<Value>,
    error: String,
}

impl Handle for _duckdb_replacement_scan_info {
    type Target = ReplacementScanInfo;
}

type ReplacementCallback = unsafe extern "C" fn(duckdb_replacement_scan_info, *const c_char, *mut c_void);

/// `CAPIReplacementScanData`: the callback with its extra data, which the delete callback frees when the database
/// lets go of the scan.
struct CReplacementScan {
    callback: ReplacementCallback,
    extra_data: OwnedData,
}

impl CReplacementScan {
    /// `duckdb_capi_replacement_callback`: what the callback says to read in place of `table_name`, if anything.
    fn replace(&self, table_name: &str) -> Option<duckrs::Result<Replacement>> {
        // a table name with a NUL in it has no C string; C++ passes the part before the NUL
        let table_name = CString::new(table_name.split('\0').next().unwrap_or_default()).ok()?;
        let mut info = ReplacementScanInfo::default();
        // SAFETY: the callback C registered, with the extra data it gave for it, the table name as a C string and
        // an info that live for the call.
        unsafe {
            (self.callback)(
                ffi::borrowed_handle(&mut info),
                table_name.as_ptr(),
                self.extra_data.data.0,
            )
        };
        if !info.error.is_empty() {
            return Some(Err(Error::new(
                ErrorKind::Binder,
                format!("Error in replacement scan: {}\n", info.error),
            )));
        }
        if info.function_name.is_empty() {
            // no function provided: bail-out
            return None;
        }
        Some(Ok(Replacement::TableFunction {
            name: info.function_name,
            parameters: info.parameters,
        }))
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_add_replacement_scan(
    db: duckdb_database,
    replacement: duckdb_replacement_callback_t,
    extra_data: *mut c_void,
    delete_callback: duckdb_delete_callback_t,
) {
    // SAFETY: C passes a database `duckdb_open` made, or none.
    let Some(wrapper) = (unsafe { ffi::get(db) }) else {
        return;
    };
    let Some(callback) = replacement else {
        return;
    };
    let scan = Arc::new(CReplacementScan {
        callback,
        extra_data: OwnedData::new(extra_data, delete_callback),
    });
    ffi::guard((), || add_scan(wrapper, scan));
}

fn add_scan(wrapper: &DatabaseWrapper, scan: Arc<CReplacementScan>) {
    wrapper
        .database()
        .add_replacement_scan(move |table_name| scan.replace(table_name));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_replacement_scan_set_function_name(
    info: duckdb_replacement_scan_info,
    function_name: *const c_char,
) {
    // SAFETY: C passes the info of the callback that runs now, or none.
    let Some(info) = (unsafe { ffi::get_mut(info) }) else {
        return;
    };
    // SAFETY: C passes a NUL-terminated name, or none.
    let Some(name) = (unsafe { ffi::c_str(function_name) }) else {
        return;
    };
    info.function_name = String::from_utf8_lossy(name).into_owned();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_replacement_scan_add_parameter(
    info: duckdb_replacement_scan_info,
    parameter: duckdb_value,
) {
    // SAFETY: C passes the info of the callback that runs now, or none.
    let Some(info) = (unsafe { ffi::get_mut(info) }) else {
        return;
    };
    // SAFETY: C passes a live value, or none; it is copied.
    let Some(value) = (unsafe { ffi::get(parameter) }) else {
        return;
    };
    info.parameters.push(value.clone());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_replacement_scan_set_error(info: duckdb_replacement_scan_info, error: *const c_char) {
    // SAFETY: C passes the info of the callback that runs now, or none.
    let Some(info) = (unsafe { ffi::get_mut(info) }) else {
        return;
    };
    // SAFETY: C passes a NUL-terminated message, or none.
    let Some(error) = (unsafe { ffi::c_str(error) }) else {
        return;
    };
    info.error = String::from_utf8_lossy(error).into_owned();
}
