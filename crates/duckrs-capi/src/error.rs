// Ported from duckdb/src/main/capi/v1/error_data-c.cpp @ rust-port-baseline

//! `duckdb_error_data` (`ErrorDataWrapper`).

use crate::ffi::{self, Handle};
use crate::helper::{error_type_from_c, error_type_to_c};
use crate::types::*;
use duckrs::ErrorKind;
use std::ffi::{CString, c_char};

/// `ErrorData`: an error's type and its message without the type (`RawMessage`), or no error.
#[derive(Debug, Clone, Default)]
pub struct ErrorData {
    error: Option<(ErrorKind, CString)>,
}

impl ErrorData {
    pub fn new(kind: ErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            error: Some((kind, c_string(message.as_ref()))),
        }
    }

    pub fn from_error(error: &duckrs::Error) -> Self {
        Self::new(error.kind, &error.message)
    }

    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    pub fn kind(&self) -> Option<ErrorKind> {
        self.error.as_ref().map(|(kind, _)| *kind)
    }

    /// `RawMessage`, as C reads it, or null without an error.
    pub fn raw_message(&self) -> *const c_char {
        self.error
            .as_ref()
            .map_or(std::ptr::null(), |(_, message)| message.as_ptr())
    }
}

/// `text` as a C string; a NUL inside it ends it, as C would read it anyway.
pub(crate) fn c_string(text: &str) -> CString {
    let end = text.find('\0').unwrap_or(text.len());
    CString::new(&text[..end]).unwrap_or_default()
}

impl Handle for _duckdb_error_data {
    type Target = ErrorData;
}

pub(crate) fn new_error_data(kind: ErrorKind, message: &str) -> duckdb_error_data {
    ffi::into_handle(ErrorData::new(kind, message))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_error_data(ty: duckdb_error_type, message: *const c_char) -> duckdb_error_data {
    // SAFETY: C passes a NUL-terminated message.
    let message = unsafe { ffi::c_str(message) }.unwrap_or_default();
    new_error_data(error_type_from_c(ty), &String::from_utf8_lossy(message))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_error_data(error_data: *mut duckdb_error_data) {
    // SAFETY: C passes the handle `duckdb_create_error_data` (or another function) gave it.
    drop(unsafe { ffi::take(error_data) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_error_data_error_type(error_data: duckdb_error_data) -> duckdb_error_type {
    // SAFETY: C passes a live error data handle.
    let Some(error) = (unsafe { ffi::get(error_data) }) else {
        return DUCKDB_ERROR_INVALID_TYPE;
    };
    error.kind().map_or(DUCKDB_ERROR_INVALID, error_type_to_c)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_error_data_message(error_data: duckdb_error_data) -> *const c_char {
    // SAFETY: C passes a live error data handle.
    let Some(error) = (unsafe { ffi::get(error_data) }) else {
        return std::ptr::null();
    };
    if error.has_error() {
        error.raw_message()
    } else {
        c"".as_ptr()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_error_data_has_error(error_data: duckdb_error_data) -> bool {
    // SAFETY: C passes a live error data handle.
    unsafe { ffi::get(error_data) }.is_some_and(ErrorData::has_error)
}
