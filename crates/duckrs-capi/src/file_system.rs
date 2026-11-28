// Ported from duckdb/src/main/capi/v1/file_system-c.cpp @ rust-port-baseline

//! The file system of a client context (`duckdb_file_system`), its open options and its file handles.
//!
//! duckrs reads and writes local files, so the file system is the local one (`LocalFileSystem`): a handle is an
//! open `std::fs::File` with the errors of DuckDB's local file system.

use crate::error::ErrorData;
use crate::ffi::{self, Handle};
use crate::function_info::ClientContextWrapper;
use crate::types::*;
use duckrs::ErrorKind;
use std::ffi::{c_char, c_void};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

/// `CFileSystem`: the file system and the error of its last open.
#[derive(Default)]
pub struct CFileSystem {
    error: ErrorData,
}

/// `CFileOpenOptions`: the `FileOpenFlags` set so far.
#[derive(Debug, Default, Clone, Copy)]
pub struct CFileOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
}

/// `CFileHandle`: an open file and the error of its last operation.
pub struct CFileHandle {
    file: Option<File>,
    path: String,
    error: ErrorData,
}

impl Handle for _duckdb_file_system {
    type Target = CFileSystem;
}

impl Handle for _duckdb_file_open_options {
    type Target = CFileOpenOptions;
}

impl Handle for _duckdb_file_handle {
    type Target = CFileHandle;
}

/// `strerror` of an I/O error: its message without Rust's " (os error N)".
fn describe(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_string(),
        None => text,
    }
}

/// `LocalFileSystem::OpenFile`.
fn open(path: &str, flags: CFileOpenOptions) -> Result<File, String> {
    if !flags.read && !flags.write {
        return Err("READ, WRITE or both should be specified when opening a file".to_string());
    }
    if (flags.create || flags.create_new || flags.append) && !flags.write {
        return Err("Cannot open a file for creating or appending without opening it for writing".to_string());
    }
    let mut options = OpenOptions::new();
    options.read(flags.read).write(flags.write).append(flags.append);
    if flags.create_new {
        options.create_new(true);
    } else if flags.create {
        options.create(true);
    }
    options
        .open(path)
        .map_err(|error| format!("Cannot open file \"{path}\": {}", describe(&error)))
}

impl CFileHandle {
    /// Runs `body` on the open file; an error of it is the handle's error.
    fn run<R>(&mut self, fallback: R, body: impl FnOnce(&mut File) -> std::io::Result<R>) -> R {
        let Some(file) = self.file.as_mut() else {
            self.error = ErrorData::new(ErrorKind::Io, format!("File \"{}\" is closed", self.path));
            return fallback;
        };
        match body(file) {
            Ok(result) => result,
            Err(error) => {
                self.error = ErrorData::new(ErrorKind::Io, describe(&error));
                fallback
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_client_context_get_file_system(context: duckdb_client_context) -> duckdb_file_system {
    // SAFETY: C passes a live client context.
    match unsafe { ffi::get::<_duckdb_client_context>(context) } {
        Some(ClientContextWrapper { .. }) => ffi::into_handle(CFileSystem::default()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_file_system(file_system: *mut duckdb_file_system) {
    // SAFETY: C passes a file system `duckdb_client_context_get_file_system` made.
    drop(unsafe { ffi::take(file_system) });
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_file_open_options() -> duckdb_file_open_options {
    ffi::into_handle(CFileOpenOptions::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_open_options_set_flag(
    options: duckdb_file_open_options,
    flag: duckdb_file_flag,
    value: bool,
) -> duckdb_state {
    // SAFETY: C passes live open options.
    let Some(options) = (unsafe { ffi::get_mut(options) }) else {
        return DuckDBError;
    };
    // as C++'s `flags |= ...`, a flag is only ever added: `value` is not looked at
    let _ = value;
    match flag {
        DUCKDB_FILE_FLAG_READ => options.read = true,
        DUCKDB_FILE_FLAG_WRITE => options.write = true,
        DUCKDB_FILE_FLAG_APPEND => options.append = true,
        DUCKDB_FILE_FLAG_CREATE => options.create = true,
        DUCKDB_FILE_FLAG_CREATE_NEW => options.create_new = true,
        _ => return DuckDBError,
    }
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_file_open_options(options: *mut duckdb_file_open_options) {
    // SAFETY: C passes options `duckdb_create_file_open_options` made.
    drop(unsafe { ffi::take(options) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_system_open(
    fs: duckdb_file_system,
    path: *const c_char,
    options: duckdb_file_open_options,
    out_file: *mut duckdb_file_handle,
) -> duckdb_state {
    // SAFETY: C passes a live file system, or none.
    let Some(fs) = (unsafe { ffi::get_mut(fs) }) else {
        // SAFETY: C passes where to write the handle, or no place.
        unsafe { ffi::write(out_file, std::ptr::null_mut()) };
        return DuckDBError;
    };
    if out_file.is_null() {
        fs.error = ErrorData::new(ErrorKind::Io, "Invalid out file to duckdb_file_system_open");
        return DuckDBError;
    }
    // SAFETY: C passes a NUL-terminated path and live options, or none.
    let (path, options) = (unsafe { ffi::c_str(path) }, unsafe { ffi::get(options) });
    let (Some(path), Some(options)) = (path, options) else {
        fs.error = ErrorData::new(ErrorKind::Io, "Invalid input to duckdb_file_system_open");
        // SAFETY: `out_file` is not null, and C passes it to write the handle to.
        unsafe { ffi::write(out_file, std::ptr::null_mut()) };
        return DuckDBError;
    };
    let path = String::from_utf8_lossy(path).into_owned();
    let opened = ffi::guard(Err("Unknown error occurred during file open".to_string()), || {
        open(&path, *options)
    });
    match opened {
        Ok(file) => {
            let handle = ffi::into_handle(CFileHandle {
                file: Some(file),
                path,
                error: ErrorData::default(),
            });
            // SAFETY: `out_file` is not null, and C passes it to write the handle to.
            unsafe { ffi::write(out_file, handle) };
            DuckDBSuccess
        }
        Err(message) => {
            fs.error = ErrorData::new(ErrorKind::Io, message);
            // SAFETY: `out_file` is not null, and C passes it to write the handle to.
            unsafe { ffi::write(out_file, std::ptr::null_mut()) };
            DuckDBError
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_system_error_data(fs: duckdb_file_system) -> duckdb_error_data {
    // SAFETY: C passes a live file system, or none.
    let error = unsafe { ffi::get(fs) }.map(|fs| fs.error.clone()).unwrap_or_default();
    ffi::into_handle(error)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_file_handle(file: *mut duckdb_file_handle) {
    // SAFETY: C passes a handle `duckdb_file_system_open` made; dropping it closes the file.
    drop(unsafe { ffi::take(file) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_error_data(file: duckdb_file_handle) -> duckdb_error_data {
    // SAFETY: C passes a live file handle, or none.
    let error = unsafe { ffi::get(file) }.map(|file| file.error.clone()).unwrap_or_default();
    ffi::into_handle(error)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_read(file: duckdb_file_handle, buffer: *mut c_void, size: i64) -> i64 {
    if buffer.is_null() || size < 0 {
        return -1;
    }
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return -1;
    };
    // SAFETY: C passes a buffer with room for `size` bytes, which nothing else uses during the call.
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), size as usize) };
    file.run(-1, |file| {
        // `LocalFileSystem::Read`: as many bytes as there are, up to `size`
        let mut read = 0;
        while read < buffer.len() {
            match file.read(&mut buffer[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(read as i64)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_write(file: duckdb_file_handle, buffer: *const c_void, size: i64) -> i64 {
    if buffer.is_null() || size < 0 {
        return -1;
    }
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return -1;
    };
    // SAFETY: C passes `size` bytes at `buffer`.
    let Some(bytes) = (unsafe { ffi::bytes(buffer, size as idx_t) }) else {
        return -1;
    };
    file.run(-1, |file| file.write_all(bytes).map(|()| size))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_tell(file: duckdb_file_handle) -> i64 {
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return -1;
    };
    file.run(-1, |file| file.stream_position().map(|p| p as i64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_size(file: duckdb_file_handle) -> i64 {
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return -1;
    };
    file.run(-1, |file| file.metadata().map(|m| m.len() as i64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_seek(file: duckdb_file_handle, position: i64) -> duckdb_state {
    if position < 0 {
        return DuckDBError;
    }
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return DuckDBError;
    };
    file.run(DuckDBError, |file| {
        file.seek(SeekFrom::Start(position as u64)).map(|_| DuckDBSuccess)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_sync(file: duckdb_file_handle) -> duckdb_state {
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return DuckDBError;
    };
    file.run(DuckDBError, |file| file.sync_all().map(|()| DuckDBSuccess))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_file_handle_close(file: duckdb_file_handle) -> duckdb_state {
    // SAFETY: C passes a live file handle, or none.
    let Some(file) = (unsafe { ffi::get_mut(file) }) else {
        return DuckDBError;
    };
    // closing a closed file does nothing, as `FileHandle::Close` of a closed one
    drop(file.file.take());
    DuckDBSuccess
}
