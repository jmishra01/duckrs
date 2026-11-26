//! The FFI boundary: every conversion of a pointer C passes in into a Rust reference, and every write of a result
//! through a pointer C passes in, goes through the helpers here. The rest of the crate is safe Rust.

use crate::types::idx_t;
use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// A handle type of `duckdb.h` and the Rust object its pointer points to.
pub(crate) trait Handle {
    type Target;
}

/// The object behind a handle [`into_handle`] made, or `None` for a null handle.
///
/// # Safety
///
/// `handle` is null, or points to a live `H::Target` (made by [`into_handle`] and not yet given to [`take`], or
/// borrowed from an object that is still alive, as `duckdb_data_chunk_get_vector` hands out), and no mutable
/// reference to that object is in use for as long as the result is.
pub(crate) unsafe fn get<'a, H: Handle>(handle: *mut H) -> Option<&'a H::Target> {
    // SAFETY: by the contract of this function, the pointer is null or points to a live, unaliased `H::Target`.
    unsafe { handle.cast::<H::Target>().as_ref() }
}

/// [`get`], mutably.
///
/// # Safety
///
/// As for [`get`], and no other reference to the object is in use for as long as the result is.
pub(crate) unsafe fn get_mut<'a, H: Handle>(handle: *mut H) -> Option<&'a mut H::Target> {
    // SAFETY: by the contract of this function, the pointer is null or points to a live `H::Target` that nothing
    // else references now.
    unsafe { handle.cast::<H::Target>().as_mut() }
}

/// A new handle that owns `target`; [`take`] gets it back.
pub(crate) fn into_handle<H: Handle>(target: H::Target) -> *mut H {
    Box::into_raw(Box::new(target)).cast()
}

/// A handle for an object that something else owns (`duckdb_data_chunk_get_vector`); C must not destroy it.
pub(crate) fn borrowed_handle<H: Handle>(target: &mut H::Target) -> *mut H {
    std::ptr::from_mut(target).cast()
}

/// A handle for an object that something else owns, which C only reads (`duckdb_get_value_type`).
pub(crate) fn borrowed_const_handle<H: Handle>(target: &H::Target) -> *mut H {
    std::ptr::from_ref(target).cast_mut().cast()
}

/// Takes back the object behind `*slot`, a handle [`into_handle`] made, and sets `*slot` to null (the
/// `duckdb_destroy_*` functions). `None` when `slot` or `*slot` is null.
///
/// # Safety
///
/// `slot` is null or points to a handle that is null or was made by [`into_handle`] and not taken before, and nothing
/// references the object any more.
pub(crate) unsafe fn take<H: Handle>(slot: *mut *mut H) -> Option<Box<H::Target>> {
    // SAFETY: `slot` is null or points to a handle C owns, by the contract of this function.
    let slot = unsafe { slot.as_mut() }?;
    let handle = std::mem::replace(slot, std::ptr::null_mut());
    if handle.is_null() {
        return None;
    }
    // SAFETY: the handle came from `Box::into_raw` of an `H::Target` in `into_handle`, and is taken only once.
    Some(unsafe { Box::from_raw(handle.cast::<H::Target>()) })
}

/// A reference to a struct C owns (`duckdb_result *`), or `None` for a null pointer.
///
/// # Safety
///
/// `ptr` is null or points to a live, initialized `T` that nothing else references while the result is in use.
pub(crate) unsafe fn reference_mut<'a, T>(ptr: *mut T) -> Option<&'a mut T> {
    // SAFETY: by the contract of this function.
    unsafe { ptr.as_mut() }
}

/// Writes `value` to the out-pointer `out`, which may point to uninitialized memory. False for a null pointer.
///
/// # Safety
///
/// `out` is null or valid for a write of a `T`.
pub(crate) unsafe fn write<T>(out: *mut T, value: T) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: `out` is not null, and valid for writes by the contract of this function.
    unsafe { out.write(value) };
    true
}

/// The bytes of a NUL-terminated C string, without the NUL, or `None` for a null pointer.
///
/// # Safety
///
/// `ptr` is null or points to a NUL-terminated string that stays alive and unchanged while the result is in use.
pub(crate) unsafe fn c_str<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is a NUL-terminated string, by the contract of this function.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes())
}

/// `length` bytes at `data`, or `None` when `data` is null and `length` is not 0.
///
/// # Safety
///
/// `data` is null or valid for reads of `length` bytes, which stay alive and unchanged while the result is in use.
pub(crate) unsafe fn bytes<'a>(data: *const c_void, length: idx_t) -> Option<&'a [u8]> {
    if data.is_null() {
        return (length == 0).then_some(&[]);
    }
    // SAFETY: `data` is valid for `length` bytes, by the contract of this function.
    Some(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize) })
}

/// `count` values at `data`, or `None` when `data` is null.
///
/// # Safety
///
/// `data` is null or valid for reads of `count` initialized values of `T`, which stay alive and unchanged while the
/// result is in use.
pub(crate) unsafe fn slice<'a, T>(data: *const T, count: idx_t) -> Option<&'a [T]> {
    if data.is_null() {
        return None;
    }
    if count == 0 {
        return Some(&[]);
    }
    // SAFETY: `data` is valid for `count` values, by the contract of this function.
    Some(unsafe { std::slice::from_raw_parts(data, count as usize) })
}

/// A copy of `bytes` in memory from `duckdb_malloc`, which C frees with `duckdb_free`, followed by a NUL when `nul`
/// is set. Null when the allocation fails.
pub(crate) fn malloc_copy(bytes: &[u8], nul: bool) -> *mut u8 {
    let size = bytes.len() + usize::from(nul);
    // SAFETY: `malloc` may be called with any size; a size of 0 is made 1 so a non-null pointer means success.
    let target = unsafe { libc::malloc(size.max(1)) }.cast::<u8>();
    if target.is_null() {
        return target;
    }
    // SAFETY: `target` was just allocated with room for `size` bytes, and does not overlap `bytes`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        if nul {
            target.add(bytes.len()).write(0);
        }
    }
    target
}

/// `strdup`: a NUL-terminated copy of `text` C frees with `duckdb_free`.
pub(crate) fn strdup(text: impl AsRef<[u8]>) -> *mut c_char {
    malloc_copy(text.as_ref(), true).cast()
}

/// An array of `count` zeroed values of `T` from `duckdb_malloc`, for C to free with `duckdb_free`, as a slice to
/// fill. `None` when the allocation fails.
///
/// `T` is one of the plain C types of [`crate::types`] or a primitive, for which all-zero bytes are a value.
pub(crate) fn malloc_zeroed<'a, T: Copy>(count: usize) -> Option<(&'a mut [T], *mut T)> {
    let size = count.checked_mul(size_of::<T>())?;
    // SAFETY: `calloc` may be called with any size; a size of 0 is made 1 so a non-null pointer means success.
    let target = unsafe { libc::calloc(size.max(1), 1) }.cast::<T>();
    if target.is_null() {
        return None;
    }
    // SAFETY: `target` is a fresh, zeroed allocation of `count` values of `T` (malloc aligns for any C type), all
    // zero bytes are a value of `T`, and nothing else references it.
    let slice = unsafe { std::slice::from_raw_parts_mut(target, count) };
    Some((slice, target))
}

/// Frees memory from `duckdb_malloc` (or `malloc_copy`/`malloc_zeroed`).
pub(crate) fn free<T>(ptr: *mut T) {
    // SAFETY: `ptr` is null or came from `malloc`/`calloc` and is freed only once: the callers free what they
    // allocated for a result when it is destroyed.
    unsafe { libc::free(ptr.cast()) }
}

/// A pointer C gave to be handed back to it (extra info, bind data, a state), which duckrs never reads or writes
/// through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CPointer(pub *mut c_void);

// SAFETY: duckrs only passes the pointer back to C's callbacks, and DuckDB's C API runs those on any of its threads,
// so what the pointer points to is C's to make usable from any thread (as it must be with libduckdb).
unsafe impl Send for CPointer {}

// SAFETY: as for `Send`: duckrs does nothing with the pointer itself but copy it.
unsafe impl Sync for CPointer {}

impl CPointer {
    pub(crate) fn null() -> Self {
        Self(std::ptr::null_mut())
    }
}

/// Runs `body`, giving `fallback` if it panics, so a panic of duckrs does not unwind into C (where it would abort).
pub(crate) fn guard<R>(fallback: R, body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}
