# duckrs

An analytical SQL database engine: a port of [DuckDB](https://github.com/duckdb/duckdb) to Rust.

**Status: Phase 3 (minimal vertical slice) in progress.** The engine runs CREATE TABLE, INSERT
and single-table SELECT in memory. What exists today:

- `crates/duckrs-common`: the core building blocks every other crate uses:
  - the type system (`LogicalType`, `PhysicalType`, with DuckDB's ids and names), `string_t`
    (`StringT`, the 16-byte inline/prefix+pointer string) and its arena, `Interval`;
  - `Vector` (flat, constant, dictionary and sequence representations, nested LIST/STRUCT/
    ARRAY/MAP children, copy-on-write buffers), `ValidityMask`, `SelectionVector`,
    `UnifiedFormat`, and `DataChunk`;
  - the vectorized executors (unary/binary, nullable and fallible variants, string variants,
    and selection for filters);
  - temporal types (`Date`, `Time`, `TimeTz`, `Timestamp` and the `_S`/`_MS`/`_NS`/`TZ`
    variants), with DuckDB's parsing and formatting, plus interval formatting and UUIDs;
  - DuckDB's hash functions (bit-for-bit, checked against `hash()`), for scalars and vectors;
  - `Value` and the DuckDB-exact VARCHAR rendering of every scalar type;
  - vector copy, `ColumnDataCollection` (chunked columnar row buffer), and DuckDB's row format
    (`TupleDataLayout`, `TupleDataCollection`, `RowMatcher`) for hash tables and aggregation;
  - DuckDB's comparison semantics (NaN ordering, NULL-aware `DISTINCT FROM`);
  - the binary serializer (DuckDB's on-disk/WAL format) with `LogicalType` and `Value`
    serialization, interval parsing, and byte-comparable sort keys (`create_sort_key`) with an
    in-memory sort.
- `crates/duckrs-parser`: a port of DuckDB's PEG parser engine: the tokenizer, keyword tables,
  grammar loader and a memoizing PEG matcher that runs DuckDB's own grammar (vendored from the
  baseline in `grammar/`, re-synced with `sync_grammar.sh`) and produces parse trees. It accepts
  99.99% of the statements the test corpus expects to parse (all except one extension's
  grammar); `cargo run --release -p duckrs-parser --example corpus` measures this, and CI fails
  below 99.9%. The transformer turns parse trees into the AST in `ast.rs`, which renders SQL
  exactly like DuckDB's `ToString`. It covers every statement in DuckDB's grammar (SELECT with
  every expression form, DML, DDL, utility statements, PIVOT, MERGE, MATCH_RECOGNIZE, external
  resources, ...): all 94.8k corpus statements that parse also transform, and their renderings
  parse again (CI gate). It rejects 88.57% of the statements the corpus expects a parser error
  for (CI gate); the rest are raised by DuckDB's binder or at run time, so they arrive with
  later phases. Expression depth is limited like DuckDB (`max_expression_depth`). Parsing a
  corpus statement into the AST takes about 82 µs (`bench` example), with packrat memoization of
  the rules DuckDB memoizes and a pool of reused parser threads. Phase 2 is complete.
- `crates/duckrs-core`: the engine, one module per DuckDB `src/` directory until the boundaries
  settle: an in-memory catalog (`catalog`); scalar functions, the implicit cast cost rules, type
  unification (`MaxLogicalType`), overload resolution and vector casts (`function`); the binder,
  bound expressions, logical operators and column binding resolution (`planner`); the expression
  executor, physical operators and single-threaded pipelines (`execution`); and the client
  context (`client`, DuckDB's `src/main`). It runs CREATE TABLE, INSERT (VALUES or a query) and
  SELECT over one table or VALUES list with WHERE, comparisons, AND/OR/NOT, IS [NOT] NULL, `+`,
  `-`, `*` and casts between the numeric types, BOOLEAN and VARCHAR, with DuckDB's types and
  error messages. Joins, aggregates, ORDER BY and LIMIT are next. SQL it does not run yet fails
  with an error saying it `is not yet ported to duckrs`; the sqllogictest runner never counts
  that error as the one a test expects.
- `crates/duckrs`: the public API (`Database`, `Connection`, `QueryResult`) over the engine.
  Databases live in memory; a database file is accepted but not written until Phase 9.
- `tools/sqllogictest`: a port of DuckDB's sqllogictest runner. It runs the DuckDB `.test` corpus
  against duckrs, or against the C++ DuckDB library as a reference. duckrs passes 82 files so far
  (`tools/sqllogictest/known_passing.txt`, a CI gate), among them
  `test/sql/projection/test_simple_projection.test`.

Deferred to later phases by design: sorting is in-memory (external merge sort comes with Phase
8), and `ColumnDataCollection` and `TupleDataCollection` are in-memory (spilling comes with the
buffer manager in Phase 9). The row format stores LIST/ARRAY values in a per-column side store
rather than C++'s row heap; it is never persisted, so only its behavior has to match.

The full roadmap is in [`../RUST_PORT_PLAN.md`](../RUST_PORT_PLAN.md).

## Layout

```
duckrs/
├── crates/duckrs/          # public API (facade crate)
├── crates/duckrs-common/   # types, vectors, executors, values
├── crates/duckrs-core/     # catalog, functions, binder/planner, execution, client context
├── crates/duckrs-parser/   # tokenizer, PEG grammar engine (+ vendored grammar), AST, transformer
├── tools/sqllogictest/     # test runner + known_passing.txt
└── docs/                   # ARCHITECTURE, PORTING_GUIDE, UPSTREAM_SYNC
```

The runner expects the DuckDB checkout at `../duckdb` (override with `--duckdb-root`). The port
is pinned to the `rust-port-baseline` branch of that checkout.

## Running the test corpus

```bash
cargo build --release -p sqllogictest
./target/release/sqllogictest --allow-failures --status-out docs/STATUS.md   # duckrs, whole corpus
./target/release/sqllogictest test/sql/select -v                            # one directory, verbose
```

Against the C++ reference engine (the first build compiles DuckDB from source, which takes a
while):

```bash
cargo build --release -p sqllogictest --features reference
./target/release/sqllogictest --engine reference test/sql/select
```

Useful flags: `--slow` (include `.test_slow`), `-j N`, `--continue-on-failure` (count every
passing statement instead of stopping a file at its first failure), `--passing-out` / `--known-passing`
(the regression gate CI uses), and `--isolate` (run each file in a child process with a
`--timeout`, so an engine crash or hang fails one file instead of the whole run; use it for full
runs of the reference engine).

### Reference engine notes

- It links the C++ DuckDB that `libduckdb-sys` bundles (currently v2.0.0), not the
  `rust-port-baseline` commit, so a few hundred files fail because of real version differences.
  [`docs/STATUS-reference.md`](docs/STATUS-reference.md) records the current numbers. To compare
  against the exact baseline, build `../duckdb` and point `libduckdb-sys` at it with
  `DUCKDB_LIB_DIR` / `DUCKDB_INCLUDE_DIR` instead of the `bundled` feature.
- Values are read through the C API's row-based result functions. Strings with embedded NUL
  bytes come back truncated (a known C API limit). Nested values (LIST, STRUCT, ...) are rendered
  by re-running the SELECT with a VARCHAR cast, so they show as NULL in non-SELECT statements
  such as `PRAGMA`.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

MIT, like DuckDB. Ported files name their C++ source with a `// Ported from duckdb/<path> @ rust-port-baseline` header.
