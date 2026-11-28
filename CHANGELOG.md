# Changelog

## 1.0.0

The first release of PintailDB, a port of DuckDB to Rust. It follows DuckDB commit `d4eea3f208d`
(see [`docs/UPSTREAM_SYNC.md`](docs/UPSTREAM_SYNC.md)) and is measured against that commit's test
suite.

### Name

- The project is PintailDB. While it was being ported it was called duckrs; the crates are now
  `pintail-*`, the shell is `pintail` and the C library is `libpintail_capi`. The C API keeps
  DuckDB's `duckdb_*` functions and `duckdb.h`.

### SQL

- 5618 of the 6,090 test files that run pass (**92.2%**; 6,367 in all, 277 skipped), against DuckDB's test suite at commit `d4eea3f208d`. The v1 target was 90%. The phase gates are met: `test/parquet` at 87.4% of its files (gate 80%), `test/sql/cte` at 84.5%.
- DuckDB's PEG parser runs DuckDB's own grammar: 99.99% of the statements the corpus expects to
  parse are accepted, and every one that parses also transforms and renders back like DuckDB's
  `ToString`.
- The binder, the logical planner, DuckDB's optimizer rules (filter pushdown, join order,
  statistics propagation, common subplans, CTE inlining, ...) and a vectorized executor with
  hash joins, hash aggregation (with a perfect hash for small integer groups), window functions,
  sorting and out-of-core spilling to the temporary directory.
- DuckDB's database file format and write-ahead log: files written by DuckDB can be read, and
  checkpoints, compression (including ZSTD, ALP, Roaring and dictionary) and recovery follow C++.
  Corrupt files are reported as C++ reports them.
- Parquet, CSV (with DuckDB's sniffer) and JSON reading and writing.
- TPC-H and TPC-DS: `dbgen` / `dsdgen` (as `CALL` and in a FROM clause), `PRAGMA tpch(n)` /
  `PRAGMA tpcds(n)`, and their queries and answers. TPC-H at scale factor 10 ran at 0.58× of
  C++ DuckDB's time in the last measurement.

### Interfaces

- The `pintail` shell, ported from DuckDB's CLI (dot commands, output modes, highlighting).
- The Rust API (`pintail::Database`, `Connection`, prepared statements, appenders, user-defined
  scalar, aggregate and table functions, replacement scans).
- DuckDB's C API (`duckdb.h`, v1): all 548 functions, including prepared statements, appenders
  (also over a query), data chunks and vectors, Arrow, user-defined functions, casts, types, copy
  functions, config options, profiling, logging, the instance cache, the catalog and threading.
  167 of the 179 test cases of DuckDB's C API tests pass.

### Not in 1.0.0

- Extensions (`LOAD`, ICU, httpfs, ...), an event-driven executor and streaming CTEs, and the
  C API v2. See [`docs/REMAINING_WORK.md`](docs/REMAINING_WORK.md).
