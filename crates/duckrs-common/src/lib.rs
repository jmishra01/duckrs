//! Core building blocks shared by every duckrs crate (port of `duckdb/src/common`):
//! the type system, `string_t`, validity masks, selection vectors, vectors, data chunks, the
//! vectorized executors, and scalar values.

mod error;
pub mod executor;
pub mod hash;
pub mod string_cast;
pub mod types;
pub mod value;
pub mod vector;

pub use error::{Error, ErrorKind, Result};

/// The number of rows in a vector (`STANDARD_VECTOR_SIZE`).
pub const STANDARD_VECTOR_SIZE: usize = 2048;
