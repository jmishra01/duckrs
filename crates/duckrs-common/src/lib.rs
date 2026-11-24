//! Core building blocks shared by every duckrs crate (port of `duckdb/src/common`):
//! the type system, `string_t`, validity masks, selection vectors, vectors, data chunks, the
//! vectorized executors, and scalar values.

pub mod column_data;
pub mod comparison;
mod error;
pub mod executor;
pub mod geojson;
mod fmt_float;
pub mod geometry;
pub mod geometry_crs;
pub mod hash;
pub mod json;
pub mod path;
pub mod render_width;
pub mod row;
pub mod serializer;
pub mod sort_key;
pub mod storage_version;
pub mod string_cast;
pub mod string_util;
pub mod types;
pub mod value;
pub mod variant;
pub mod vector;

pub use error::{Error, ErrorKind, Result};

/// The number of rows in a vector (`STANDARD_VECTOR_SIZE`).
pub const STANDARD_VECTOR_SIZE: usize = 2048;

/// Asks for the memory of `value` to be brought into the cache, without waiting for it (a prefetch never faults).
#[inline(always)]
pub fn prefetch<T: ?Sized>(value: &T) {
    let address = std::ptr::from_ref(value).cast::<u8>();
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `prfm` only hints at a read of a valid address; it reads no register and writes no memory
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) address, options(nostack, readonly, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch only hints at a read; it never faults
    unsafe {
        std::arch::x86_64::_mm_prefetch(address.cast::<i8>(), std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = address;
}
