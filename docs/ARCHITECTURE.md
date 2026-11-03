# Architecture

duckrs keeps DuckDB's design: columnar storage, vectorized execution over 2048-row vectors,
push-based pipelines with morsel-driven parallelism, MVCC transactions, and a single-file database.

## Query path

```
SQL ─► parser (PEG grammar) ─► AST ─► binder ─► logical plan ─► optimizer
    ─► physical plan generator ─► pipelines ─► task scheduler ─► result
```

## Crates

Today the workspace has three members. More crates are split out as each phase lands; the target
layout follows DuckDB's `src/` modules, so any C++ file has an obvious Rust home:

| Crate (target) | C++ source | Phase |
|---|---|---|
| `duckrs-common` (exists) | `src/common` | 1 |
| `duckrs-parser` | `src/parser` | 2 |
| `duckrs-catalog` | `src/catalog` | 3 |
| `duckrs-planner` | `src/planner` | 3, 6 |
| `duckrs-execution` | `src/execution` | 3 |
| `duckrs-parallel` | `src/parallel` | 3, 8 |
| `duckrs-function` | `src/function`, `extension/core_functions` | 4–5 |
| `duckrs-optimizer` | `src/optimizer` | 7 |
| `duckrs-storage` | `src/storage` | 9 |
| `duckrs-transaction` | `src/transaction` | 10 |
| `duckrs-main` | `src/main` | 3, 12 |
| `duckrs` (exists) | public API facade | 0 |
| `duckrs-capi` | `src/main/capi`, `duckdb.h` | 12 |
| `duckrs-cli` | `tools/shell` | 12 |
| `sqllogictest` (exists) | `test/sqlite` | 0 |

Dependencies point downward only:
`common → parser → catalog → function → planner → optimizer → storage/transaction → execution → parallel → main → duckrs`.
Where the C++ modules reference each other (for example, a table catalog entry owning its
storage), the lower crate defines a trait and the higher crate implements it.

## Core data structures (`duckrs-common`)

- `Vector` is a column of up to 2048 rows. It is flat (one value per row), constant (one value
  for all rows), dictionary (a `SelectionVector` over a child vector) or sequence. Buffers are
  shared through `Arc` and copied on write, so cloning a vector is cheap and clones never see
  each other's writes.
- `StringT` is DuckDB's 16-byte `string_t`. Reading a non-inlined string is `unsafe` because the
  bytes live in a `StringBuffer` owned by a vector; the vector accessors (`str_at`,
  `UnifiedFormat::str_at`) tie the bytes to the vector's lifetime and are safe. Writing raw
  strings is `unsafe` (`string_data_mut`), so safe code cannot store a dangling pointer.
- Executors take closures (`|a: i32, b: i32| a + b`) and handle vector representations and
  NULLs; the closures are monomorphized and vectorize.

## Testing

The DuckDB sqllogictest corpus is the acceptance oracle. `tools/sqllogictest` runs it against
duckrs, or against the C++ library (`--engine reference`, behind the `reference` cargo feature).
CI fails when a file in `tools/sqllogictest/known_passing.txt` stops passing.
