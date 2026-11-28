# pintail

`pintail` is the public Rust API of pintail, a Rust port of DuckDB: `Database`, `Connection`,
`query`, `prepare`/`execute`, the `Appender`, Arrow export, the `Relation` API, and results a chunk
at a time. It is a thin facade (a crate that re-exports and wraps others): each call goes to
[`pintail-main`](../pintail-main/README.md)'s `ClientContext`, which runs the SQL with
`pintail-parser` and `pintail-core`. It corresponds to DuckDB's C++ `DuckDB` and `Connection`
classes (`src/main/database.cpp`, `connection.cpp`), `src/main/relation`, `src/main/appender.cpp`
and `src/common/arrow`. It depends on `pintail-common`, `pintail-core` and `pintail-main`; the shell
(`pintail-cli`) and the test runner (`tools/sqllogictest`) depend on it.

## At a glance

| | |
|---|---|
| Lines of Rust | 998 in `src/` (`lib.rs` 488, `arrow_export.rs` 320, `relation.rs` 190); about 4,600 more in `tests/`, `benches/` and `examples/` |
| Main entry points | `Database::{open_in_memory, open, open_with_storage_version, connect}`, `Connection::{query, prepare, execute, query_arrow, stream, appender, appender_in, add_replacement_scan}`, `Appender`, `ChunkedResult`, `Relation`, `record_batches` |
| C++ it ports | `src/main/relation.cpp` and `src/main/relation/*.cpp`, `src/common/arrow/arrow_converter.cpp` and `arrow_appender.cpp`; the `Appender` and `QueryResult::Fetch` behaviour of `src/main` |

## Dependencies

| Crate | Used for |
|---|---|
| `pintail-main` | `DatabaseInstance`, `ClientContext`, `PreparedStatementData`, and `MaterializedQueryResult` (re-exported as `QueryResult`) |
| `pintail-common` | re-exported `Value`, `LogicalType`, `LogicalTypeId`, `DataChunk`, `Error`, `ErrorKind`, `Result`; `Vector` and `STANDARD_VECTOR_SIZE` for the appender's chunks |
| `pintail-core` | `execution::parquet::try_cast_value`, which casts appended values to the column type |
| `arrow` | `RecordBatch`, arrays and schemas for the Arrow export; re-exported as `pintail::arrow` |
| `criterion` (dev) | the `kernels` bench |
| `libc` (dev) | `tests/common/mod.rs`: `atexit` to remove the temporary directory, `kill(pid, 0)` to see whether an earlier test process is still alive |

## Layout

| File | What it does | Ported from |
|---|---|---|
| [`src/lib.rs`](src/lib.rs) | `Database`, `Connection`, `PreparedStatement`, `Appender`, `ChunkedResult`; the re-exports; unit tests | `DuckDB`, `Connection`, `Appender`, `QueryResult::Fetch` (no port header) |
| [`src/arrow_export.rs`](src/arrow_export.rs) | `arrow_type`, `arrow_schema`, `record_batches`: results as Arrow record batches with C++'s default type mapping | `src/common/arrow/arrow_converter.cpp`, `arrow_appender.cpp` |
| [`src/relation.rs`](src/relation.rs) | `Relation` and `JoinKind`: queries built step by step, kept as SQL text | `src/main/relation.cpp`, `src/main/relation/*.cpp` |

## Key types and APIs

All signatures below are from `src/lib.rs`, `src/relation.rs` and `src/arrow_export.rs`.

| Item | Signature | What it is for |
|---|---|---|
| `Database::open_in_memory` | `() -> Result<Database>` | an in-memory database |
| `Database::open` | `(path: impl AsRef<Path>, read_only: bool) -> Result<Database>` | a database file; created if missing and not `read_only`. Commits go to the write-ahead log, which is checkpointed into the file on `CHECKPOINT`, when it gets large and on drop |
| `Database::open_with_storage_version` | `(path, read_only, version: &str)` | a new file in a storage version such as `"v1.0.0"` |
| `Database::connect` | `(&self) -> Result<Connection>` | a connection; connections share the database |
| `Connection::query` | `(&mut self, sql: &str) -> Result<QueryResult>` | runs `;`-separated statements, returns the last result |
| `Connection::prepare` | `(&mut self, sql: &str) -> Result<PreparedStatement>` | one statement with `?` or `$1` parameters |
| `Connection::execute` | `(&mut self, &PreparedStatement, values: &[Value]) -> Result<QueryResult>` | runs a prepared statement |
| `Connection::query_arrow` | `(&mut self, sql: &str, batch_rows: usize) -> Result<Vec<RecordBatch>>` | rows as Arrow batches of up to `batch_rows` rows |
| `Connection::stream` | `(&mut self, sql: &str) -> Result<ChunkedResult>` | rows a `DataChunk` at a time |
| `Connection::appender` / `appender_in` | `(&mut self, table: &str)` / `(&mut self, schema: Option<&str>, table: &str) -> Result<Appender<'_>>` | bulk inserts |
| `Connection::add_replacement_scan` | `(&mut self, scan: impl Fn(&str) -> Option<Result<(Vec<String>, Vec<LogicalType>, Arc<Vec<DataChunk>>)>> + Send + Sync + 'static)` | rows for a table name the catalog does not have (the shell's `_` uses it) |
| `QueryResult` | fields `names`, `types`, `collection`; `row_count()`, `column_count()`, `value(column, row)` | a fully materialized result |
| `Appender` | `types()`, `append(impl Into<Value>)`, `end_row()`, `append_row(impl IntoIterator<Item = Value>)`, `flush()`, `close(self)` | rows kept in memory, inserted on flush |
| `ChunkedResult` | `names()`, `types()`, `fetch() -> Option<DataChunk>`; implements `Iterator<Item = DataChunk>` | chunked reading of a result |
| `Relation` | `table`, `table_in`, `from_sql`, `values`, `filter`, `project`, `aggregate`, `order`, `limit`, `distinct`, `alias`, `join`, `cross_product`, `union`, `except`, `intersect`, `sql`, `execute`, `insert_into`, `create_table`, `create_view`, `explain` | a query built a step at a time |
| `record_batches`, `arrow_schema`, `arrow_type` | `(&QueryResult, usize) -> Result<Vec<RecordBatch>>`, `(&QueryResult) -> Schema`, `(&LogicalType) -> DataType` | Arrow conversion of an existing result |

### Queries and prepared statements

```rust
use pintail::{Database, Result, Value};

fn main() -> Result<()> {
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    con.query("CREATE TABLE t (i INTEGER, s VARCHAR); INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;

    let result = con.query("SELECT i, s FROM t ORDER BY i")?;
    assert_eq!(result.names, ["i", "s"]);
    for row in 0..result.row_count() {
        println!("{} {}", result.value(0, row).try_to_string()?, result.value(1, row).try_to_string()?);
    }

    let prepared = con.prepare("SELECT s FROM t WHERE i = $1")?;
    let result = con.execute(&prepared, &[Value::integer(2)])?;
    assert_eq!(result.value(0, 0), Value::varchar("b"));
    Ok(())
}
```

`Database::open("my.db", false)?` instead of `open_in_memory` keeps the tables in a file.

### Appender

```rust
use pintail::{Database, Result, Value};

fn main() -> Result<()> {
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    con.query("CREATE TABLE t (i INTEGER, s VARCHAR)")?;
    let mut appender = con.appender("t")?;
    for i in 0..10_000 {
        appender.append(Value::bigint(i))?; // cast to INTEGER, the column's type
        appender.append(Value::varchar(format!("row {i}")))?;
        appender.end_row()?;
    }
    appender.append_row([Value::integer(-1), Value::varchar("last")])?;
    appender.close()?;
    assert_eq!(con.query("SELECT count(*) FROM t")?.value(0, 0), Value::bigint(10_001));
    Ok(())
}
```

A value is cast to its column's type; a value that does not cast is an `ErrorKind::Conversion`
error. Too many values in a row is `Too many appends for chunk!`, and `end_row` before every column
is `Call to EndRow before all columns have been appended to!` (C++'s messages). Rows are kept until
`flush`, which also runs after 204,800 rows (`APPENDER_FLUSH_ROWS`, C++'s `FLUSH_COUNT`), on
`close`, and on drop (where an error is lost). A flush is one INSERT: constraints, triggers and the
WAL apply, and a failing flush inserts nothing.

### Arrow export

```rust
use pintail::arrow::array::AsArray;
use pintail::arrow::datatypes::Int32Type;
use pintail::{Database, Result};

fn main() -> Result<()> {
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    let batches = con.query_arrow("SELECT i::INTEGER AS i FROM range(5) t(i)", 3)?;
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].num_rows(), 3);
    let ints = batches[1].column(0).as_primitive::<Int32Type>();
    assert_eq!(ints.values().to_vec(), vec![3, 4]);
    Ok(())
}
```

The types follow C++'s defaults (`ArrowConverter::ToArrowSchema`): integers to the Arrow integer
of the same width, HUGEINT to `Decimal128(38, 0)`, DECIMAL to `Decimal128`, DATE to `Date32`, TIME
to `Time64(Microsecond)`, the TIMESTAMP kinds to `Timestamp` of their unit (TIMESTAMPTZ with
`"UTC"`), INTERVAL to `Interval(MonthDayNano)`, BLOB to `Binary`, LIST and ARRAY to `List`,
STRUCT to `Struct`, MAP to `Map`, and VARCHAR, UUID and types Arrow has no equivalent of to
`Utf8`. An empty result is one empty batch.

### Chunked results

```rust
use pintail::{Database, Result};

fn main() -> Result<()> {
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    let mut result = con.stream("SELECT i FROM range(5000) t(i)")?;
    let mut rows = 0;
    while let Some(chunk) = result.fetch() {
        rows += chunk.len(); // at most STANDARD_VECTOR_SIZE (2048) rows each
    }
    assert_eq!(rows, 5000);
    Ok(())
}
```

`stream` runs the whole query before it returns; `fetch` hands out the chunks of the materialized
result one by one and skips empty ones.

### Relations

```rust
use pintail::{Database, JoinKind, Relation, Result};

fn main() -> Result<()> {
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    con.query("CREATE TABLE t AS SELECT i, i % 3 AS g FROM range(10) r(i)")?;

    let per_group = Relation::table("t")
        .filter("i >= 2")
        .aggregate(&["g", "count(*) AS n"], &["g"])
        .order(&["g"]);
    let result = per_group.execute(&mut con)?;
    assert_eq!(result.row_count(), 3);

    let names = Relation::values(&[&["0", "'zero'"], &["1", "'one'"]], &["k", "name"]);
    let joined = Relation::table("t")
        .join(&names, "a", "b", "a.g = b.k", JoinKind::Inner)
        .project(&["name", "i"])
        .limit(2, 1);
    println!("{}", joined.sql());
    joined.create_table(&mut con, "joined")?;
    Ok(())
}
```

A `Relation` is a SQL string. Each step wraps the previous query as a subquery
(`SELECT ... FROM (<previous>) AS __relation ...`); `join` names its two sides by the aliases
given, and `union` is `UNION ALL`. Expressions and conditions are SQL text pasted in as given;
table and column names passed to `table`, `values`, `insert_into`, `create_table` and
`create_view` are quoted. Nothing runs until `execute`, `insert_into`, `create_table`,
`create_view` or `explain`.

## How it works

`Database` holds an `Arc<DatabaseInstance>`, plus the path and read-only flag it was opened with.
`connect` makes a `ClientContext` over it, so several connections (also on several threads, see
`tests/concurrent_transactions.rs`) share one catalog and storage. `Connection::query`,
`prepare` and `execute` call `ClientContext::query`, `prepare` and `execute_prepared` directly;
[`pintail-main`'s README](../pintail-main/README.md#what-clientcontextquery-does-with-a-sql-string)
traces what happens to the SQL.

The `Appender` keeps rows as `Vec<Value>`. On `flush` it builds `DataChunk`s of up to
`STANDARD_VECTOR_SIZE` rows with `Vector::set_value`, and passes them to `ClientContext::append`,
which inserts them with `INSERT INTO t (...) SELECT * FROM __internal_appender_chunks()`.

`record_batches` walks the `ColumnDataCollection` of a `QueryResult` in slices of `batch_rows`
rows and builds one Arrow array per column with `arrow_array`, value by value, then a
`RecordBatch` with `arrow_schema`.

## Status and gaps

From `../../docs/REMAINING_WORK.md`, item 43 (Rust API) and item 41 (C API):

- Done: `Appender`; Arrow `RecordBatch` export with C++'s default types (`query_arrow`,
  `record_batches`); `Relation` (table, values, filter, project, aggregate, order, limit, distinct,
  joins, set operations), as SQL; results a chunk at a time (`stream` / `fetch`), from the
  materialized result.
- Not done:
  - True streaming: results are not streamed while the query runs, because the executor
    materializes them. `stream` gives no memory saving over `query`.
  - Appending Arrow batches (Arrow goes out, not in).
  - `Relation` from Rust closures, or with its own query plans rather than SQL text.
  - A C API: `pintail-capi`, implementing `duckdb.h` and passing `test/api/capi/*`, has no code yet
    (item 41).
- Smaller points seen in the code: `PreparedStatement` has no way to bind by name or ask for its
  parameter count; `arrow_array` is `pub` in a private module, so it is not reachable from outside;
  `QueryResult` has no typed getters (values come out as `Value`).

## Testing

```sh
cargo test -p pintail                         # unit and integration tests
cargo test -p pintail --test read_database_file
cargo bench -p pintail --bench kernels [filter]
```

### Unit tests (`src/lib.rs`)

Five tests: queries, shared connections and prepared statements; Arrow types and batch sizes;
relations; chunked results; and appenders (casts, row errors, a constraint failing a flush).

### Integration tests (`tests/`)

[`tests/common/mod.rs`](tests/common/mod.rs) gives each test process a temporary directory,
removed when it exits (and on the next start, if a killed process left one behind).

| Group | Tests | What they cover |
|---|---|---|
| Database files: reading | [`read_database_file.rs`](tests/read_database_file.rs) (25 tests) | files written by real DuckDB: every compression codec, nested types, deletes, NOT NULL, views and stored SQL rendered as DuckDB does, defaults, generated columns, sequences, constraints, scans of large tables in place, WAL replay (also torn and damaged logs, a log of another database), read-only files |
| Database files: writing | [`write_database_file.rs`](tests/write_database_file.rs) (40 tests) | writing, checkpointing and reopening; keys and indexes after checkpoint and log replay; failed commits; attached databases; `storage_info`; forced and chosen codecs; shredded variants; lazy column loading; appends written ahead; CSV loads on many threads; and `commits_survive_kill_9`, which kills a child process (the `crash_child` test, `#[ignore]`d on its own) |
| Database files: robustness and features | [`damaged_database_file.rs`](tests/damaged_database_file.rs) (7), [`encryption.rs`](tests/encryption.rs) (3), [`row_ids.rs`](tests/row_ids.rs) (3), [`incremental_statistics.rs`](tests/incremental_statistics.rs) (3) | damaged files give errors, not panics (bytes changed, checksums fixed); encrypted files under both ciphers, C++'s key errors, and files DuckDB 1.4 encrypted (read from `../duckdb/data/attach_test` when present); row ids stay stable through changes, the file and the log; statistics follow writes |
| Transactions and databases | [`concurrent_transactions.rs`](tests/concurrent_transactions.rs) (7), [`cross_database.rs`](tests/cross_database.rs) (7) | threads against one database, checked by invariants; statements that write one attached database and read others |
| Optimizer and execution results | [`exists_joins.rs`](tests/exists_joins.rs) (3), [`join_filter.rs`](tests/join_filter.rs), [`filtered_scan.rs`](tests/filtered_scan.rs), [`index_scans.rs`](tests/index_scans.rs) (5), [`unused_columns.rs`](tests/unused_columns.rs), [`common_subexpressions.rs`](tests/common_subexpressions.rs), [`aggregate_spill.rs`](tests/aggregate_spill.rs) | each compares a rewritten or optimized plan with one that avoids the rewrite: EXISTS as SEMI/ANTI joins, hash-join filters pushed into scans, filters applied while reading a file, index scans, column pruning, common subexpressions computed once, GROUP BY spilling past the memory limit |
| Functions | [`format_functions.rs`](tests/format_functions.rs) (7) | `format` and `printf`, with text from DuckDB v1.5.6 |

### Fixtures (`tests/fixtures`)

Made by real DuckDB with [`tests/fixtures/generate.sh`](tests/fixtures/generate.sh)
(`DUCKDB=/path/to/duckdb crates/pintail/tests/fixtures/generate.sh`), so tests compare pintail with
DuckDB and not with itself.

- Database files: `compatible.db`, `compressed.db`, `bitpacked.db`, `floats.db`, `modern.db`,
  `nested.db`, `deletes.db` (types, codecs, nested columns, deletes); `parsed_v100.db`,
  `parsed_v150.db`, `parsed_v200.db` (objects stored as SQL, in three storage versions);
  `scan_v4096.db`, `scan_v122880.db` (large tables, two row group sizes; also used by
  `pintail-main`'s test); `wal_v100.db`, `wal_v150.db`, `wal_v200.db` with their `.wal` files.
- What DuckDB read from them, one `.expected` file each: per table (`alp`, `rle`, `fsst`,
  `structs`, `maps`, ...), `q1`–`q50.expected` (the views built from `stored_queries.txt`),
  `scan_4096_*` and `scan_122880_*` (the 56 queries of `scan_queries.txt` on each file), and the
  `wal_*` results.
- Inputs: `stored_queries.txt`, `stored_schema.sql`, `scan_queries.txt`, `scan_tables.sql`,
  `wal_logged.sql`, `wal_checkpointed.sql`.

### Benches and examples

- [`benches/kernels.rs`](benches/kernels.rs) (criterion): scans and filters, hash aggregates with
  few and many groups, the hash join, sorting, top-N, windows, string matching and casts, each on
  a million rows through SQL. A quick loop for one kernel; `tools/benchmark` runs DuckDB's own
  `.benchmark` files.
- [`examples/tpch.rs`](examples/tpch.rs): the 22 TPC-H queries against DuckDB's answers
  (`cargo run --release -p pintail --example tpch -- <dir with .tbl files> <extension/tpch/dbgen> <sf> [queries]`).
- [`examples/tpcds.rs`](examples/tpcds.rs): the 99 TPC-DS queries, likewise.
- [`examples/parallel_aggregate.rs`](examples/parallel_aggregate.rs),
  [`examples/parallel_join.rs`](examples/parallel_join.rs): time aggregates and joins on one
  thread and on all, and compare answers (`-- <rows>`).
- [`examples/out_of_core.rs`](examples/out_of_core.rs): sorts, aggregates and joins without and
  with a small memory limit, which makes them spill (`-- <rows> <memory_limit> [only]`).

The main check of the SQL surface is the sqllogictest corpus (`tools/sqllogictest`, which runs
through this crate); see the top-level [`README.md`](../../README.md).

## Notes for contributors

- Keep this crate thin: new behaviour belongs in `pintail-main` or `pintail-core`, with the facade
  only wrapping it. Nothing below it may depend on it (`../../../CLAUDE.md`, item 1).
- Mirror C++ names in doc comments (`` `Appender::EndRow` ``, `` `QueryResult::FetchArrowTable` ``,
  `` `TableRelation` ``) and keep C++'s error messages verbatim.
- New files get a `// Ported from duckdb/src/<path> @ rust-port-baseline` header where there is a
  C++ source.
- No `unsafe` in `src/` (`CLAUDE.md`, item 3); the only `unsafe` is the `libc` calls in `tests/common/mod.rs`.
- Integration tests that compare with DuckDB read fixtures made by `generate.sh`; regenerate them
  with a real DuckDB rather than editing `.expected` files by hand.
- `cargo clippy --workspace --all-targets -- -D warnings` is the CI gate.
