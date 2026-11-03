# Architecture

duckrs keeps DuckDB's design: columnar storage, vectorized execution over 2048-row vectors,
push-based pipelines with morsel-driven parallelism, MVCC transactions, and a single-file database.

## Query path

```
SQL ─► parser (PEG grammar) ─► AST ─► binder ─► logical plan ─► optimizer
    ─► physical plan generator ─► pipelines ─► task scheduler ─► result
```

## Crates

Today the workspace has four members. More crates are split out as each phase lands; the target
layout follows DuckDB's `src/` modules, so any C++ file has an obvious Rust home:

| Crate (target) | C++ source | Phase |
|---|---|---|
| `duckrs-common` (exists) | `src/common` | 1 |
| `duckrs-parser` (exists) | `src/parser` | 2 |
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

## Parser (`duckrs-parser`)

The parser runs DuckDB's grammar unchanged: the `.gram` rules are vendored and interpreted at
run time, so grammar changes upstream are picked up by re-syncing, not re-porting. The C++
matcher is an explicit-stack machine; the Rust one is a recursive interpreter with the same
semantics (ordered choice, greedy repetition, case-insensitive literals, packrat memoization).
It runs on a thread with a 512 MB stack so deeply nested SQL cannot overflow. Known cost: a
thread and a fresh memo table per parse (about 0.3 ms per statement); reusing threads and
memoizing only the rules C++ memoizes are the obvious optimizations.

The transformer (`transformer.rs`) turns parse trees into the AST (`ast.rs`). The C++ version
has one function per rule plus ~20k lines of generated trampolines that unpack each rule's
sequence by position; the Rust one handles each rule by name and finds its parts by sub-rule
name (`sub`/`subs`), so it does not depend on how a rule's sequence nests. The AST is a set of
enums (`ExprKind`, `TableRefKind`, `QueryNodeKind`) instead of a class hierarchy, and `Display`
reproduces DuckDB's `ToString` byte for byte. Rendering is not idempotent in DuckDB either:
VALUES lists gain a subquery level, GROUPING SETS renumber, `UNNEST` is lowercased, and SET
VARIABLE prints its name unquoted (so `"my var"` does not read back). The corpus
tool therefore checks that every rendering parses again and reports identical renderings
separately. Known deviation: STAR EXCLUDE/REPLACE/RENAME lists keep input order, where C++
uses unordered sets, and CREATE TABLE WITH options keep input order. Statement transformers
live in `transformer/statements.rs`; statements not yet ported return `Not implemented Error`.

## Testing

The DuckDB sqllogictest corpus is the acceptance oracle. `tools/sqllogictest` runs it against
duckrs, or against the C++ library (`--engine reference`, behind the `reference` cargo feature).
CI fails when a file in `tools/sqllogictest/known_passing.txt` stops passing.
