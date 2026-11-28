// Ported from duckdb/extension/tpch/tpch_extension.cpp, include/tpch_extension.hpp and
// dbgen/include/dbgen/dbgen.hpp @ rust-port-baseline

//! TPC-H for pintail: a pure-Rust port of DuckDB's `tpch` extension data generator (dbgen), with
//! the 22 queries and the answers DuckDB embeds.
//!
//! The generator writes exactly the rows DuckDB's `CALL dbgen(sf = ...)` writes (checked byte for
//! byte against the C++ dbgen at SF 0.01 and 0.1, see `tests/oracle.rs`). This crate only
//! generates data; creating the tables and appending the chunks is up to the caller.
//!
//! The TPC code is owned by the Transaction Processing Performance Council and used under the TPC
//! end user license agreement in this crate's `LICENSE`.
//!
//! # API
//!
//! Schema (`DBGenWrapper::CreateTPCHSchema`):
//! - [`TpchTable`]: the 8 tables. [`TpchTable::ALL`] lists them in the order `CreateTPCHSchema`
//!   creates them (region, nation, supplier, customer, part, partsupp, orders, lineitem);
//!   [`TpchTable::name`], [`TpchTable::column_names`], [`TpchTable::types`] and
//!   [`TpchTable::schema`] describe each.
//! - [`tpch_schema()`] `-> Vec<TpchTableSchema>`: every table's [`TpchTableSchema`] (name,
//!   columns of [`TpchColumn`] with name, type and `not_null`, which is true for every column as the
//!   C++ adds a NOT NULL constraint on each). The C++ creates `<name><suffix>` with
//!   `IF NOT EXISTS` semantics and then checks the table with
//!   [`validate_tpch_table_schema`]`(table, table_name, &[(name, &type)]) -> Result<()>`, which
//!   gives its error messages.
//!
//! Data (`tpch::CreateDBGenGenerator`, `DBGenGenerator`):
//! - [`DBGenGenerator::new`]`(sf: f64, children: u32, step: Option<u32>) -> Result<DBGenGenerator>`
//!   (also [`create_dbgen_generator`]), with the checks of `DbgenBind` and the C++ generator:
//!   "Step must be defined when children are defined", a NaN scale factor, or one above 100000.
//!   SF 0 (or less) generates no rows at all, as in C++ (the tables stay empty); so does a `step`
//!   of `children` or more.
//! - [`DBGenGenerator::next_chunk`]`(&mut self) -> Option<(TpchTable, DataChunk)>` (and
//!   `Iterator<Item = (TpchTable, DataChunk)>`): the rows of every table, as chunks of at most
//!   `STANDARD_VECTOR_SIZE` (2048) rows with the table's [`TpchTable::types`], in the order the C++
//!   appends them in its sequential mode. DECIMAL(15, 2) columns hold their unscaled `i64`, DATE
//!   columns `Date`, VARCHAR columns strings in the chunk's own buffer.
//! - [`DBGenGenerator::progress`]`(&self) -> f64`: `DbgenProgress`, in `[0, 100]`.
//! - [`DBGenGenerator::is_finished`]`(&self) -> bool`.
//!
//! Queries and answers (`DBGenWrapper::GetQuery` / `GetAnswer`, `tpch_queries()`,
//! `tpch_answers()`, `PRAGMA tpch`):
//! - [`get_query`]`(query: i32) -> Result<&'static str>`: the stored query (SF 1 parameters).
//! - [`get_query_for_sf`]`(query: i32, sf: f64) -> Result<String>`: the query for scale factor
//!   `sf`; only Q11's FRACTION (`0.0001 / sf`, printed with `%.10f`) changes.
//! - [`get_answer`]`(sf: f64, query: i32) -> Result<&'static str>`, for the scale factors in
//!   [`TPCH_ANSWER_SCALE_FACTORS`] (0.01, 0.1, 1).
//! - [`TPCH_QUERIES_COUNT`] (22).

mod bm_utils;
mod build;
mod constants;
mod dbgen;
mod dbgen_gunk;
mod dss;
mod permute;
mod rnd;
mod rng64;
mod speed_seed;
mod text;
mod text_pool_checkpoints;

pub use constants::TPCH_QUERIES_COUNT;
pub use dbgen::{
    DBGenGenerator, TPCH_ANSWER_SCALE_FACTORS, TpchColumn, TpchTable, TpchTableSchema, create_dbgen_generator,
    get_answer, get_query, get_query_for_sf, tpch_schema, validate_tpch_table_schema,
};
