# Architecture

duckrs keeps DuckDB's design: columnar storage, vectorized execution over 2048-row vectors,
push-based pipelines with morsel-driven parallelism, MVCC transactions, and a single-file database.

## Query path

```
SQL ─► parser (PEG grammar) ─► AST ─► binder ─► logical plan ─► optimizer
    ─► physical plan generator ─► pipelines ─► task scheduler ─► result
```

## Crates

Today the workspace has five members. Following the plan, the engine starts as one crate,
`duckrs-core`, with a module per DuckDB `src/` directory; each module becomes its own crate
once its boundaries settle. The target layout follows DuckDB's `src/` modules, so any C++ file
has an obvious Rust home:

| Crate (target) | C++ source | Phase | Today |
|---|---|---|---|
| `duckrs-common` | `src/common` | 1 | crate |
| `duckrs-parser` | `src/parser` | 2 | crate |
| `duckrs-catalog` | `src/catalog` | 3 | `duckrs-core::catalog` |
| `duckrs-planner` | `src/planner` | 3, 6 | `duckrs-core::planner` |
| `duckrs-execution` | `src/execution` | 3 | `duckrs-core::execution` |
| `duckrs-parallel` | `src/parallel` | 3, 8 | `duckrs-core::execution` (pipelines) |
| `duckrs-function` | `src/function`, `extension/core_functions` | 4–5 | `duckrs-core::function` |
| `duckrs-optimizer` | `src/optimizer` | 7 | |
| `duckrs-storage` | `src/storage` | 9 | |
| `duckrs-transaction` | `src/transaction` | 10 | |
| `duckrs-main` | `src/main` | 3, 12 | `duckrs-core::client` |
| `duckrs` | public API facade | 0 | crate |
| `duckrs-capi` | `src/main/capi`, `duckdb.h` | 12 | |
| `duckrs-cli` | `tools/shell` | 12 | |
| `sqllogictest` | `test/sqlite` | 0 | crate |

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
semantics (ordered choice, greedy repetition, case-insensitive literals). Like C++, it memoizes
(packrat) only the rules DuckDB marks — Expression, TableRef, Identifier, ColId and the catalog and
schema qualifications — and shares memoized subtrees (`Arc`) instead of copying them. Parsing
runs on a small pool of reused threads with 512 MB stacks. Instead of a recursion count, the
matcher and the transformer measure the stack they use and stop at a 448 MB budget, reporting
DuckDB's `Max expression depth` error (input that deep is far past any depth limit); parse trees
deeper than a few hundred levels drop iteratively, so returning one to a small-stack thread is
safe. Like DuckDB, the transformer limits expression depth (`ParserOptions::max_expression_depth`,
default 1000): nested expressions, types and subqueries add one level each, every link of an
operator chain one more. On the corpus (`--dump` it, then run the `bench` example) a statement
takes about 54 µs to match and 75 µs to become an AST on one thread, against 24.5 µs for
DuckDB's release-built `Parser::ParseQuery` on the same statements. Most of the gap is matching:
C++ skips choice alternatives, optionals and repetitions whose FIRST set rules out the next
token (`matcher_first_set.cpp`), which the Rust matcher does not do yet.

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
uses unordered sets, and WITH, ATTACH and CREATE SECRET options and EXECUTE parameters keep
input order (COPY sorts its options, as DuckDB does). A PIVOT whose values come from the data is
preceded by a `CREATE TYPE` of an ENUM; DuckDB names it with a random UUID, duckrs with a
process-wide counter formatted as one, so renderings are reproducible. Statement transformers
live in `transformer/statements.rs` (ALTER, COMMENT and SHOW in `statements/alter.rs`, COPY and the other option-driven statements
in `statements/utility.rs`, PIVOT, MERGE and NEAREST in `statements/pivot.rs`, MATCH_RECOGNIZE in
`statements/match_recognize.rs`); every statement the grammar accepts is transformed.

## Engine (`duckrs-core`)

A statement goes `parse_ast` → `Binder` (bound expressions, `LogicalOperator` tree) →
`ColumnBindingResolver` (column references become input positions) → `create_plan`
(physical operators) → pipelines → `MaterializedQueryResult`. The pieces mirror the C++ classes:

- **Types while binding.** Constants bind with literal types (`INTEGER_LITERAL` carries its
  value, `STRING_LITERAL`), so `tinyint_col + 1` stays TINYINT and `int_col = '5'` compares as
  INTEGER. `cast_rules` is `CastRules::ImplicitCast` (the cost table), `combine_types` is
  `TryGetMaxLogicalType`/`ForceMaxLogicalType` with the built-in combine rules, and the function
  binder picks the cheapest overload, folds calls with a NULL argument into a NULL constant and
  casts the arguments to the parameter types, like `FunctionBinder`.
- **Bound expressions and logical operators** are enums (`BoundExprKind`,
  `LogicalOperatorKind`); each operator reports its types and column bindings.
- **Physical operators** implement a trait with DuckDB's source/operator/sink roles. Pipelines
  are built from those roles as in `PhysicalOperator::BuildPipelines` and run in dependency order
  on the calling thread; the task scheduler and parallelism arrive with Phase 8.
- **The catalog** is in memory and has no transactions yet: tables hold their rows in a
  `ColumnDataCollection` until storage (Phase 9) and MVCC (Phase 10) land. In place of a
  transaction's snapshot, a table scan reads the rows the table had when it started, so
  `INSERT INTO t FROM t` ends.
- **Result modifiers** bind like `BindSelectNode`: ORDER BY and DISTINCT targets resolve to
  select list entries (by position, alias, unaliased column name or equal expression, after
  qualifying column names), entries ORDER BY needs are appended and pruned by a final
  projection, and the modifiers apply in the order they were written. ORDER BY gathers and
  sorts with the byte-comparable sort keys of `duckrs-common`; DISTINCT keeps the first row
  per sort key. Select list entries and WHERE may use select list aliases.
- **Aggregates** bind like `BaseSelectBinder`: groups naming a select list entry swap places with
  it, expressions matching a group or an aggregate become column references of the aggregate
  operator, equal aggregates are computed once, and a column outside of them is DuckDB's "must
  appear in the GROUP BY clause" error. `PhysicalAggregate` finds groups by sort key and keeps a
  state per group and aggregate (`function::aggregates`); min, max and DISTINCT compare sort
  keys. This is simple rather than fast: DuckDB's vectorized hash table comes with Phase 8.
- **Settings** are generated from DuckDB's `settings.json` (`tools/generate_settings.py`) with
  their aliases, types, scopes and defaults, and kept per database and per connection.
  Settings that only tune performance, storage, extensions or logging are stored; the
  default sort order and the expression depth limit take effect; the settings that would
  change results in ways duckrs does not implement yet (`integer_division`, `search_path`,
  `default_collation`, ...) fail as not yet ported when set to anything but their default.
  Pragmas that only switch DuckDB's debug verification, profiling or progress output are
  no-ops.

Deliberate gaps so far: no DECIMAL arithmetic (a DECIMAL meets a number in DOUBLE, which loses
digits), no constraints beyond NOT NULL, no table options, no joins, no transactions, no DISTINCT ON, LIMIT PERCENT or
non-constant LIMIT, no ORDER BY in aggregates, no collations, no
SET VARIABLE or settings of MAP and list types. The expression executor evaluates a filter's
condition to a BOOLEAN vector rather than using DuckDB's specialized `Select`, and casts to
VARCHAR and from or to DECIMAL, CASE and COALESCE write row by row through `Value`. Tables and
blocking operators are capped at about 2 GiB and fail as not yet ported beyond that, until
out-of-core operators (Phase 8) and storage (Phase 9) exist. There is no optimizer yet, so
errors DuckDB's rewrites avoid (an overflow in a filter it removes) still surface; only
`LIMIT 0` skips its input. Errors for SQL beyond the slice
say it `is not yet ported to duckrs`, wording DuckDB never uses, so that no test expecting an
error passes because a feature is missing.

## Testing

The DuckDB sqllogictest corpus is the acceptance oracle. `tools/sqllogictest` runs it against
duckrs, or against the C++ library (`--engine reference`, behind the `reference` cargo feature).
CI fails when a file in `tools/sqllogictest/known_passing.txt` stops passing.
