# duckrs

An analytical SQL database engine: a port of [DuckDB](https://github.com/duckdb/duckdb) to Rust.

**Status: 3318 files pass, 56.3% of the files that ran (53.5% of all 6,197).** The Phase 6 feature list is ported apart from data-modifying CTEs and a few binder corners, but its gates are not met yet; see "Phase 6" in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). Phase 7 has started: expression rewriting, filter pushdown, empty result pull-up and unused column removal are in, correlated subqueries are flattened where the port covers them, table statistics decide comparisons ahead of time, and inner joins and cross products are reordered by estimated cost; see "Phase 7" there. Phase 4 (types and casts) has started: the scalar casts, the nested types (LIST, ARRAY, STRUCT, MAP, UNION), `CREATE TYPE`, ENUM, BIT, BIGNUM and VARIANT are in, the last in its own in-memory encoding until Phase 9. What is left of the phase is GEOMETRY, which the plan defers. A first batch of Phase 5's functions is in too, because the type tests need them: `unnest`, `TRY(...)`, list slicing, the `list` aggregate, LIKE, and common string, list, map and interval helpers. Nested casts and functions run row by row for now. The engine runs CREATE TABLE, CREATE VIEW, CREATE SEQUENCE, CREATE MACRO, INSERT, prepared statements
and SELECT (joins, WHERE, GROUP BY, aggregates, ORDER BY, LIMIT, DISTINCT) in memory. What exists today:

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
  executor, physical operators and pipelines, on threads for large aggregates and joins (`execution`); and the client
  context and settings (`client`, DuckDB's `src/main`). It runs CREATE TABLE (with AS, NOT NULL and
  DEFAULT), CREATE VIEW, CREATE SEQUENCE (with `nextval`, `currval`, `setval`), CREATE MACRO (scalar and table, with overloads, defaults and named arguments), CREATE SCHEMA, DROP, INSERT (VALUES or a query) and SELECT over tables,
  views, VALUES lists, subqueries, `range`/`generate_series` and joins of those (CROSS, INNER, LEFT, RIGHT,
  FULL, SEMI, ANTI and their RIGHT forms, with ON, USING or NATURAL; hash joins for equality
  conditions and nested loops otherwise, in memory) with WHERE, GROUP BY (with aliases,
  positions, ALL, GROUPING SETS, ROLLUP and CUBE, and `GROUPING()`), HAVING, aggregates (`count`, `sum`, `avg`, `min`, `max`, `first`, `last`,
  `any_value`, with DISTINCT and FILTER), ORDER BY (positions, aliases, expressions, ORDER BY
  ALL, NULLS FIRST/LAST), LIMIT/OFFSET and DISTINCT; expressions with comparisons, AND/OR/NOT,
  IS [NOT] NULL, BETWEEN, IN, CASE, COALESCE, `+`, `-`, `*`, `/`, `//`, `%`, casts between the numeric types, DECIMAL, BOOLEAN, the temporal types, INTERVAL, UUID, BLOB and VARCHAR,
  and aliases of the select list; and SET/RESET/`PRAGMA x = y` for all 196
  of DuckDB's settings (generated from its `settings.json` by `tools/generate_settings.py`), with
  DuckDB's types and error messages. The optimizer turns a filter over a cross product
  (`FROM a, b WHERE a.x = b.x`) into a join, and Phase 7 reorders inner joins by estimated cost. BEGIN, COMMIT and ROLLBACK work: a transaction sees a snapshot of its own, and write-write conflicts on rows and catalog entries fail as in DuckDB. UNION, EXCEPT and INTERSECT (with ALL, and BY NAME) and non-recursive CTEs work, the CTEs inlined at each reference. Scalar, EXISTS, IN, ANY and ALL subqueries work, correlated too. Correlated ones in a filter or a projection are flattened into duplicate eliminated joins where possible; the others run as plans of their own, once if uncorrelated and otherwise once per combination of correlated values. Recursive CTEs run round by round, and LATERAL joins run their right side per combination of left values. UPDATE and DELETE work, with RETURNING, and rewrite the table at the end of the statement; until Phase 10 a statement that writes runs alone. SQL it does not
  run yet fails with an error saying it `is not yet ported to duckrs`; the sqllogictest runner
  never counts that error as the one a test expects.
- `crates/duckrs`: the public API (`Database`, `Connection`, `QueryResult`) over the engine.
  Phase 9 has started: `Database::open` reads files written by DuckDB and scans their tables in place: tables of scalar and
  nested types, with deleted rows, in all compression but ZSTD and the old Chimp and Patas. Such a
  database takes no writes. Views, macros, sequences, types, defaults, generated columns, constraints and indexes come along, what a write-ahead log committed is replayed, and what isn't ported yet is an error. Files can be written too, in a format that DuckDB reads: commits go to a write-ahead log, and `CHECKPOINT` and closing the database write all of it, compressed with RLE, BitPacking, Roaring and dictionaries, apart from keys and indexes, which wait for Phase 10; see "Phase 9" in
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
- `tools/sqllogictest`: a port of DuckDB's sqllogictest runner. It runs the DuckDB `.test` corpus
  against duckrs, or against the C++ DuckDB library as a reference. duckrs passes 3322 files so far
  (`tools/sqllogictest/known_passing.txt`, a CI gate), among them
  `test/sql/projection/test_simple_projection.test`.

ORDER BY, GROUP BY and hash joins spill to `temp_directory` past `memory_limit` (Phase 8); windows
and the other blocking operators are still in memory, and `ColumnDataCollection` and
`TupleDataCollection` are in-memory (spilling them comes with the buffer manager). The row format stores LIST/ARRAY values in a per-column side store
rather than C++'s row heap; it is never persisted, so only its behavior has to match.

The full roadmap is in [`../RUST_PORT_PLAN.md`](../RUST_PORT_PLAN.md).

## Layout

```
duckrs/
├── crates/duckrs/          # public API (facade crate)
├── crates/duckrs-common/   # types, vectors, executors, values
├── crates/duckrs-core/     # catalog, functions, binder/planner, execution, storage, client context
├── crates/duckrs-parser/   # tokenizer, PEG grammar engine (+ vendored grammar), AST, transformer
├── tools/sqllogictest/     # test runner + known_passing.txt
└── docs/                   # ARCHITECTURE, PORTING_GUIDE, UPSTREAM_SYNC
```

The runner expects the DuckDB checkout at `../duckdb` (override with `--duckdb-root`). The port
is pinned to the `rust-port-baseline` branch of that checkout.

## Running the test corpus

```bash
cargo build --release -p sqllogictest
./target/release/sqllogictest --allow-failures --isolate --timeout 60 --status-out docs/STATUS.md   # duckrs, whole corpus
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
(the regression gate CI uses), `--diff-against unoptimized|reference` with `--diff-out FILE` (run every
statement on a second engine and record where they differ), and `--isolate` (run each file in a child process with a
`--timeout`, so an engine crash or hang fails one file instead of the whole run; use it for full
runs, since some files still build very large intermediate results).

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
