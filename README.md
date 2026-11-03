# duckrs

An analytical SQL database engine: a port of [DuckDB](https://github.com/duckdb/duckdb) to Rust.

**Status: Phase 1 (common core) in progress.** The engine runs no queries yet. What exists today:

- `crates/duckrs-common`: the core building blocks every other crate uses:
  - the type system (`LogicalType`, `PhysicalType`, with DuckDB's ids and names), `string_t`
    (`StringT`, the 16-byte inline/prefix+pointer string) and its arena, `Interval`;
  - `Vector` (flat, constant, dictionary and sequence representations, nested LIST/STRUCT/
    ARRAY/MAP children, copy-on-write buffers), `ValidityMask`, `SelectionVector`,
    `UnifiedFormat`, and `DataChunk`;
  - the vectorized executors (unary/binary, nullable and fallible variants, string variants,
    and selection for filters);
  - `Value` and the DuckDB-exact VARCHAR rendering of numbers, decimals and blobs.
- `crates/duckrs`: the public API surface (`Database`, `Connection`). Every query returns
  `Not implemented Error`.
- `tools/sqllogictest`: a port of DuckDB's sqllogictest runner. It runs the DuckDB `.test` corpus
  against duckrs, or against the C++ DuckDB library as a reference.

Still to do in Phase 1: date/time/timestamp types and their formatting, hashing, the row layout
(`TupleDataCollection`), `ColumnDataCollection`, sorting, and the binary serializer.

The full roadmap is in [`../RUST_PORT_PLAN.md`](../RUST_PORT_PLAN.md).

## Layout

```
duckrs/
├── crates/duckrs/          # public API (facade crate)
├── crates/duckrs-common/   # types, vectors, executors, values
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
