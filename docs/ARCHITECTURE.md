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
digits), no table options, no collations. The expression executor evaluates a filter's
condition to a BOOLEAN vector rather than using DuckDB's specialized `Select`, and casts to
VARCHAR and from or to DECIMAL, CASE and COALESCE write row by row through `Value`. Tables and
blocking operators are capped at about 2 GiB and fail as not yet ported beyond that, until
out-of-core operators (Phase 8) and storage (Phase 9) exist. Errors for SQL beyond the slice
say it `is not yet ported to duckrs`, wording DuckDB never uses, so that no test expecting an
error passes because a feature is missing.

## Phase 6

- **Temporary objects** live in a `temp` schema ahead of the search path. DuckDB has a `temp`
  catalog per connection; this one is shared until Phase 10 isolates connections.
- **PIVOT** is always a set of filtered aggregates. C++ switches to a `PhysicalPivot` over `list()`
  past `pivot_filter_threshold` values, which is only faster. **UNPIVOT** is `unpivot_list` and
  `unnest`, as in C++.
- **Generated columns** are in `TableCatalogEntry::generated`, apart from the stored `columns`, each
  with its position among all columns. A `Binding` carries their expressions, and a reference binds
  the expression instead of a column; renamed tables refer to columns by position
  (`POSITIONAL_COLUMN`).
- **DESCRIBE, SHOW, SUMMARIZE** and `duckdb_{databases,schemas,tables,views,columns}` are rows computed
  while binding, in an `ExpressionGet`.
- **EXPLAIN** renders an indented list of operator names, not C++'s box tree. `EXPLAIN (SQL)` and
  JSON or YAML output are not ported.
- **MERGE INTO** binds a join and gathers its rows; `PhysicalMergeInto` picks an action per row and
  rewrites the table once at the end. A WHEN NOT MATCHED BY SOURCE marker is a cross product of
  the source with one row, which leaves the source's bindings alone.
- **ASOF, POSITIONAL and MARK joins** are in `execution/special_joins.rs` and work row by row.
  NEAREST is a lateral join with ORDER BY and LIMIT, as in C++.
- **USING KEY** keeps one row per key. Aggregates are limited to `min`, `max`, `first`, `last` and
  `any_value`; `sum`, `avg`, `approx_count_distinct` and the rest are not ported.
- **Samples** are seeded and repeatable, but not the rows C++ picks: its random engine is not ported.
- **Aggregates of outer columns in a subquery** bind in the outer query, and reach the subquery as a
  correlated column.

Not ported from the Phase 6 list: data-modifying statements in WITH. Not in the plan's list but
blocking its gate directories: COLLATE (Phase 6 has no collation work) and `SET schema`.

## Phase 7

The optimizer (`crates/duckrs-core/src/optimizer`) runs between the binder and the
`ColumnBindingResolver`. Subquery plans and the recursive part of a recursive CTE are resolved
while binding, so the binder optimizes those itself. `PRAGMA disable_optimizer` turns it off, and
`disabled_optimizers` turns off single passes.

- **Expression rewriter:** constant folding, distributivity, arithmetic, CASE, conjunction,
  comparison with NULL, NOT over comparisons and conjunctions, moving integer constants across
  comparisons, constant order, LIKE as a comparison or `prefix`, `suffix`, `contains`, and empty
  needles.
- **Filter pushdown:** through aggregates, cross products (which become joins), inner, left, semi
  and anti joins, projections, set operations, DISTINCT and UNNEST. A filter that is always false
  empties the plan, and **empty result pull-up** carries that upward.
- **Subquery flattening** (`planner/subquery.rs`) runs at the end of binding, before the optimizer.
  A correlated subquery in a filter or a projection becomes a `DelimJoin`: the left side is
  gathered, its distinct correlated values go into the right side through `DelimGet`, and a hash
  join on those values follows. It is SINGLE for a scalar and MARK for EXISTS and ANY, and keeps the
  left rows in order. `BoundSubquery::raw` keeps the plan as it was bound for this, next to the
  resolved `plan`. What cannot be flattened stays an expression that runs per combination of
  correlated values: subqueries in other operators or under TRY, volatile ones, ones reading a
  column of a query further out, and plans with an operator `push` has no rule for (VALUES with
  correlated columns, FULL joins, grouping sets with an empty set, LIMIT with a percentage, SAMPLE,
  ASOF, recursive CTEs). C++ decorrelates nearly all of those. LATERAL joins are not flattened yet.
- **Statistics** (`optimizer/statistics.rs`) follow C++ in what they decide, not in where they come
  from. Each table has min, max and whether it has NULLs per column, not per row group, worked out
  with aggregates when a plan first needs them after a write, and thrown away by
  `TableCatalogEntry::write_storage`. A version keeps a scan that overlaps a write from storing what
  it saw. Plans do not outlive a statement, so what they fold cannot go stale. A plan in which a
  table index appears twice is left alone, as narrowing one use of a binding would apply to all.
  Decided comparisons become constants, or `constant_or_null` where a side can be NULL.
- **Join order** takes trees of inner joins and cross products apart into relations and conjuncts,
  and DPhyp, or Greedy Operator Ordering from 12 relations on, picks the order with the lowest sum
  of estimated join sizes. Each conjunct goes on the lowest join that has its relations, and of
  each join the side with the fewest rows is on the right, which is the side a hash join builds.
  Column positions change, so a tree stays as it is under what reads its columns by position
  (set operations, DML, `DISTINCT`, the root); those have a projection below them in practice.
  Estimates use the total domain of each equivalence class and DuckDB's selectivities, from
  `approx_count_distinct` on the first 100,000 rows of a table, per state of its rows, except that
  integer and date columns are counted in all rows with a sketch, as a sample says little of keys. Those counts
  only steer the order, so being stale or uncommitted can make a plan slower but not wrong.
- **`x IN (subquery)`** without correlated columns becomes a MARK join in `planner::subquery`, on a
  hash of the one key; the other uncorrelated subqueries stay expressions, which run once. A filter
  on only the marker makes it a SEMI join, if nothing else reads the marker. That is a pass of its
  own after filter pushdown, which counts the reads, where C++ flags the joins up front. `NOT IN`
  stays MARK, since a NULL on either side changes its answer. In a tree of joins, a SEMI or ANTI
  join that reads one relation goes down to it.
- **Scan filters** move a conjunct from a `FILTER` directly on a scan into it, if it reads one
  column of that scan, is not volatile and cannot fail. It runs last, so the other passes see plain
  filters.
- **Zone maps** (`execution::zone_map`) keep the bounds of a column per 122,880-row group. They are
  worked out per column when a filter on a table of two groups or more first asks, and dropped by
  `write_storage`. A scan skips a group only if no row of it can pass: comparisons with constants,
  `BETWEEN`, NULL checks, AND, OR, and casts and functions that keep the order of values. Anything
  else keeps the group. `EXPLAIN ANALYZE` works out `Row Groups Scanned` with the same check.
- **EXPLAIN** (`client::explain`) turns a plan into a tree that renders as text or JSON. Names are
  in title case unless `operator_casing` says `upper`, and JSON keeps them as they are. The text is
  one line per fact, not boxes. A physical plan takes its details and estimates from the logical
  plan it came from wherever the shapes line up, as resolved expressions have lost their names.
  `~N rows` comes from the join order's statistics, per operator, so an estimate is only as good
  as those.
- **Unused columns** become typed NULL constants instead of going away, which keeps column
  bindings stable: subquery plans refer to outer columns by binding. Scans keep all their columns,
  because in-memory scans clone `Arc`s and pruning them would gain nothing.

A filter stays above a projection or an aggregate if it, or an entry it would be substituted with,
can fail: below, `a::int < b::int` would run ahead of the filter that guards those casts.
`can_throw` allows only what is known not to fail, so some filters stay higher than they do in C++.

`JoinDependentFilterRule` adds, next to an OR of ANDs, what each branch says of a table on its own,
so that it can go down to that table. It leaves out what can fail, where C++ does not.
`EqualOrNullSimplification` runs only at the root of a filter, where NULL counts as false, and the
conjuncts of a filter are split before rewriting so that each is such a root. `DatePartSimplificationRule`
leaves `date_part` alone where the function would need a cast, as `TIMESTAMP_NS` to microseconds rounds.

Most of DuckDB's pruning tests start with `ATTACH ... (ROW_GROUP_SIZE 2048)`, so they cannot run
until Phase 9; the others want statistics per function (`contains`, `substr`, `length`, ...).

TPC-H gives all 22 answers right at SF 0.01, 0.1 and 1, with `examples/tpch.rs` against DuckDB's
answer files. Data comes from `tools/tpch/build_dbgen.sh`, which builds DuckDB's generators with text
output. At SF 1 it is 14 times slower than DuckDB on one thread, by the geometric mean, and no
query is more than 45 times slower. Phase 8 brings parallelism. TPC-DS does not run yet.

Not ported yet: the box rendering of EXPLAIN, `Projections` of scans, moving LEFT joins in the join order, and SEMI and ANTI joins that read several
relations (those stay on top of the tree), the estimator's caps for composite keys, estimated rows in EXPLAIN, flipping
LEFT to RIGHT joins for the build side, statistics per row group or for functions (`stats()`, `length`, `abs`,
`greatest`, `lower`, ...), aggregates and windows, `ANALYZE`, `FilterCombiner` beyond splitting,
deduplicating and deciding constants, transitive filters, MoveConstants of floats and DECIMALs,
`x * 0`,
pushdown through outer, right, ASOF and window operators, the deliminator,
and flattening of the cases above, the extremum rewrite of `> ANY` among them. MARK joins with a
predicate check each pair of a key's rows, where C++ hashes on the comparison. The gate is not met: `test/optimizer` is at 41.3% against 80%, and TPC-DS does
not run yet.

## Testing

The DuckDB sqllogictest corpus is the acceptance oracle. `tools/sqllogictest` runs it against
duckrs, or against the C++ library (`--engine reference`, behind the `reference` cargo feature).
CI fails when a file in `tools/sqllogictest/known_passing.txt` stops passing.

`--diff-against` is the plan's differential tester, as a mode of the runner and not a crate of its
own: every statement also runs on a second engine, either duckrs with the optimizer off
(`unoptimized`) or C++ (`reference`, which is v2.0.0 and not the baseline), and differences go to
`--diff-out`. It needs `--isolate`. Rows are sorted unless the query has ORDER BY, LIMIT without
ORDER BY is skipped, and floats compare within 1e-6. Tests with `random()` or concurrent loops
differ between any two runs, so read those as noise.
