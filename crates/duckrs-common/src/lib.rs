//! Core building blocks shared by every duckrs crate (port of `duckdb/src/common`):
//! the type system, `string_t`, validity masks, selection vectors, vectors, data chunks, the
//! vectorized executors, and scalar values.

pub mod column_data;
pub mod comparison;
mod error;
pub mod executor;
mod fmt_float;
pub mod geometry;
pub mod geometry_crs;
pub mod hash;
pub mod json;
pub mod path;
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
