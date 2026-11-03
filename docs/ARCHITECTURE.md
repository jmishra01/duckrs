# Architecture

duckrs keeps DuckDB's design: columnar storage, vectorized execution over 2048-row vectors,
push-based pipelines with morsel-driven parallelism, MVCC transactions, and a single-file database.

## Query path

```
SQL ─► parser (PEG grammar) ─► AST ─► binder ─► logical plan ─► optimizer
    ─► physical plan generator ─► pipelines ─► task scheduler ─► result
```

## Crates

Today the workspace has two members. More crates are split out as each phase lands; the target
layout follows DuckDB's `src/` modules, so any C++ file has an obvious Rust home:

| Crate (target) | C++ source | Phase |
|---|---|---|
| `duckrs-common` | `src/common` | 1 |
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

## Testing

The DuckDB sqllogictest corpus is the acceptance oracle. `tools/sqllogictest` runs it against
duckrs, or against the C++ library (`--engine reference`, behind the `reference` cargo feature).
CI fails when a file in `tools/sqllogictest/known_passing.txt` stops passing.
