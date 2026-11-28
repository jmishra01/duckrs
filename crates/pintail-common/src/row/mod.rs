//! DuckDB's row format: rows of mixed-type values stored contiguously, used by hash tables,
//! aggregation and sorting (port of `duckdb/src/common/types/row` and `common/row_operations`).

mod collection;
mod layout;
mod matcher;

pub use collection::{RowPtr, TupleDataCollection};
pub use layout::{RowValidity, TupleDataLayout, align8};
pub use matcher::RowMatcher;

#[cfg(test)]
mod tests;
