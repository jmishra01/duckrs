// Ported from duckdb/src/main/capi/v1 @ rust-port-baseline

//! DuckDB's C API (`duckdb.h`, v1) over duckrs.
//!
//! Every function has the name and the signature `duckdb.h` gives it, so a C program built against the header
//! links with `libduckrs_capi` as it would with `libduckdb`. The types of the header are in [`types`]; the
//! handles C holds point to the Rust objects of this crate (a [`database::DatabaseWrapper`], a
//! `duckrs::LogicalType`, a `duckrs::DataChunk`, ...), as C++'s point to its own.
//!
//! `unsafe` is used at the boundary only, where a pointer C passes becomes a reference and where a result is
//! written through a pointer C passes; [`ffi`] has the helpers that do it. What is behind the boundary is safe
//! Rust over duckrs's API.

#![allow(clippy::missing_safety_doc)]

pub mod aggregate_function;
pub mod appender;
pub mod arrow_c;
pub mod cast_function;
pub mod catalog;
pub mod copy_function;
pub mod database;
pub mod datetime;
pub mod error;
mod ffi;
pub mod file_system;
pub mod function_info;
pub mod helper;
pub mod logical_type;
pub mod logging;
pub mod pending;
pub mod prepared;
pub mod profiling_info;
pub mod result;
pub mod scalar_function;
pub mod table_function;
pub mod threading;
pub mod types;
pub mod value;
pub mod vector;

pub use types::*;

pub mod replacement_scan;
pub mod table_description;
