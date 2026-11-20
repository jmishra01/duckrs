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
| `duckrs-storage` | `src/storage` | 9 | `duckrs-core::storage` |
| `duckrs-transaction` | `src/transaction` | 10 | |
| `duckrs-main` | `src/main` | 3, 12 | `duckrs-core::client` |
| `duckrs` | public API facade | 0 | crate |
| `duckrs-capi` | `src/main/capi`, `duckdb.h` | 12 | |
| `duckrs-cli` | `tools/shell` | 12 | crate (the `duckrs` binary) |
| `sqllogictest` | `test/sqlite` | 0 | crate |
| `benchmark` | `benchmark` | 13 | crate (`benchmark_runner`) |

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
- **The catalog** is in memory, and tables hold their rows in a `ColumnDataCollection`. A table
  scan reads the rows the table had when it started, so `INSERT INTO t FROM t` ends. Transactions
  run on a copy of it, see "Transactions".
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
out-of-core operators (Phase 8) and the rest of storage (Phase 9) exist. Errors for SQL beyond the slice
say it `is not yet ported to duckrs`, wording DuckDB never uses, so that no test expecting an
error passes because a feature is missing.

## Phase 6

- **Temporary objects** live in a `temp` schema ahead of the search path. DuckDB has a `temp`
  catalog per connection; this one is shared, so connections see each other's.
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
  and anti joins, projections, set operations, DISTINCT and UNNEST. An ON condition of a LEFT join
  that reads only the right side goes below the join, unless it can fail, which C++ does in the
  binder without that check. A filter that is always false
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
- Hash joins, grouping, DISTINCT and delim joins key their rows with `RowKey`
  (`duckrs-common/src/sort_key.rs`): the bytes of a sort key, built a column at a time, inline up to
  30 bytes, and hashed with `KeyHasher`. Keys are not chosen to attack a hash table here, so it is
  a multiplicative hash and not SipHash. Other operators use the slower `group_keys`.
- The **deliminator** (`optimizer/deliminator.rs`) runs right after filter pushdown. An equality
  join with `DelimGet` only filters, so it goes, and an IS NOT NULL stands in for what `=` dropped.
  When the query filters the outer rows the deepest such join stays, as DuckDB does: the distinct
  values make the subquery read less. That is not so for `EXISTS` and `NOT EXISTS` in a WHERE
  clause with only filters and projections above the join: without `DelimGet` they are SEMI and
  ANTI joins, whose join filter cuts the scan of the subquery the same way, so that join goes too
  (TPC-H Q4, Q21, Q22). A join with no `DelimGet` left is an ordinary one, except SINGLE, which
  stays a `DelimJoin` because that operator raises the error for two rows. SINGLE becomes LEFT when
  an aggregate proves one row per key. An inequality with a column of `DelimGet` (`l2.l_suppkey <>
  l1.l_suppkey` in Q21) is a predicate of the join here; for a MARK `DelimJoin` it moves up in
  place of the condition on that outer value, as `outer IS NOT NULL AND inner IS DISTINCT FROM
  outer` over a subquery filtered to `inner IS NOT NULL`, which is never NULL. With a scalar subquery whose
  result is NULL on no rows anyway, flattening also joins the aggregate with `DelimGet` by INNER
  and not LEFT, which is what lets this pass drop that join.
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
  Columns equal to the same column are equal to each other, so the equalities that follow are
  added before the search (`add_implied_equalities`): with `l_partkey = p_partkey` and
  `l_partkey = ps_partkey`, `partsupp` can join `part` first, as in TPC-H Q9. A join then takes one
  equality of each set of equal columns. That holds only where each column of the set is of
  another relation, so a set with two columns of one relation is left as written.
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
  stays MARK, since a NULL on either side changes its answer; a filter on `NOT marker` makes an
  ANTI join only where every condition is `IS NOT DISTINCT FROM` and the predicate is made of tests
  that are never NULL, which is what the deliminator leaves of `NOT EXISTS`. In a tree of joins, a
  SEMI or ANTI join that reads one relation goes down to it.
- **Build sides** (`optimizer/build_side.rs`): a hash join builds its right side, so before join
  order a SEMI or ANTI join whose right side is estimated larger than its left becomes RIGHT SEMI
  or RIGHT ANTI with the sides swapped. The large side then only probes and can take a join filter.
  The pass estimates, which reads table statistics, only for a plan that has such a join.
  For inner joins, join order puts the side with fewer estimated rows on the right. After unused
  columns are gone, a second look swaps the sides where the right rows are at least three times
  as wide as the left ones and rows times width says so (TPC-H Q10 then probes with `customer`
  and its five strings, 428,000 rows of 1,500,000 after the join filter). C++ always goes by rows
  times width; with these estimates that builds on all of `orders` in Q9. Joins under something
  that reads columns by position stay as they are.
- **Scan filter order**: a scan runs the conjuncts of fewest columns first, and among those a join
  filter that has its keys, then conjuncts on numbers, then those that read strings. A comparison
  of an integer, date or decimal column with a constant, and BETWEEN two constants, select rows
  directly (`select_column_constant`), without a vector of booleans.
- **Uncorrelated subqueries** that stay expressions run once, on the first chunk that needs them,
  and on as many threads as the query has (`run_plan_on_threads`; the count travels with
  `QueryMemory`). The other threads wait for the result. A subquery that runs per row has one
  thread. C++ plans these as joins instead.
- **Aggregate states** are 48 bytes: the value of a `min`, `max`, `first` and the like is behind a
  pointer, so that a sum or a count, which most states are, does not carry room for one. The
  memory an aggregate counts against the limit adds those values, and the rows a holistic
  aggregate keeps at the size of a `Value`.
- **`starts_with`, `ends_with` and `contains`** on strings compare bytes; `LIKE 'x%'`, `'%x'` and
  `'%x%'` are rewritten to them.
- **Merging thread states of an aggregate**: the parts a thread's state is split into carry their
  keys in order and no hash table, and the table of each partition is sized once. Each group is
  still moved three times (thread, part, partition), which is most of TPC-H Q18.
- **An OR across a join**: for a filter on both sides of an inner join that is an OR, each side gets
  the OR of the conjuncts of each alternative that read only that side, if every alternative has
  one (`(p AND l) OR (p' AND l')` gives `p OR p'` and `l OR l'`). The filter itself stays on the
  join. TPC-H Q19 reads 4,754 parts of 2,000,000 that way at SF10.
- **Scan filters** move a conjunct from a `FILTER` directly on a scan into it, if it reads one
  column of that scan, is not volatile and cannot fail. It runs last, so the other passes see plain
  filters.
- **Join filters** (`execution::join_filter`) are added as the physical plan is made, so EXPLAIN,
  which shows the logical plan, leaves them out. An INNER, SEMI, RIGHT SEMI, RIGHT ANTI or RIGHT hash join
  follows each equality on an integer or date column of its left side down through filters,
  projections, column selections and the sides of joins that keep that column's values unchanged, to a scan of a
  table of 65,536 rows or more. There it adds a filter. When the build side is done, the filter
  gets its keys' min and max and a blocked Bloom filter of them; until then, and once it has
  passed more than 80% of a million rows, it lets every row through. A build side that spilled,
  or has more than 16M keys, gives no keys. Plans under a recursive CTE get none, as their
  operators run again.
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
- **Limits** below 8192 go below projections, and a limit over an ORDER BY is a **Top-N**
  (`optimizer/top_n.rs`) unless it is more than 5000 rows and 0.7% of the input. The Top-N is
  `PhysicalOrder` with a limit, which cuts its rows whenever it holds twice what it needs.
- `count(x)` of a column without NULLs is `count_star()`, which reads no column. ANALYZE and VACUUM
  only check that the table exists, as statistics are made when needed.
- **Unused columns** of a projection become typed NULL constants instead of going away, which
  keeps column bindings stable: subquery plans refer to outer columns by binding. A scan keeps its
  `column_ids` and bindings, and `projection` says which it outputs, so a column that only its
  filters read stays behind and joins do not copy what nothing above reads. Before an operator that
  copies rows, `ColumnSelect` drops what only the operators below read, as projection maps do in
  C++, and it stays out of EXPLAIN. A projection does not compute what it drops. A join still
  copies its own right key to its output, since that happens inside of it.

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
output. At SF 1 it is 6.5 times slower than DuckDB on one thread, by the geometric mean, and no
query is more than 15 times slower. Phase 8 brings parallelism.

TPC-DS gives all 99 answers right at SF 0.01 and 1, with `examples/tpcds.rs` against DuckDB's
answer files, within a relative tolerance of 1e-9. Data comes from `tools/tpcds/build_dsdgen.sh`,
which builds DuckDB's generator with text output. At SF 1 the 99 queries take 82 s, and loading
takes 26 minutes through INSERT, as there is no COPY yet.

Not ported yet: the box rendering of EXPLAIN, moving LEFT joins in the join order, and SEMI and ANTI joins that read several
relations (those stay on top of the tree), the estimator's caps for composite keys, estimated rows in EXPLAIN, flipping
LEFT to RIGHT joins for the build side, statistics per row group or for functions (`stats()`, `length`, `abs`,
`greatest`, `lower`, ...), aggregates and windows, `ANALYZE`, `FilterCombiner` beyond splitting,
deduplicating and deciding constants, transitive filters, MoveConstants of floats and DECIMALs,
`x * 0`,
pushdown through outer, right, ASOF and window operators, SEMI and ANTI delim joins,
and flattening of the cases above, the extremum rewrite of `> ANY` among them. MARK joins with a
predicate check each pair of a key's rows, where C++ hashes on the comparison. The gate is not met: `test/optimizer` is at 46.5% against 80%.

## Phase 8

Threads for aggregates and joins, and spilling for sorts, aggregates and hash joins. A pipeline runs on the `threads` setting's threads,
all of the machine's by default, when it has all of these:

- a source of morsels: a table of at least 2 row groups, so that small tables stay in order and their floating
  point sums the same on each run. In memory a morsel is a chunk; a table still in its file has a morsel per row
  group (`parallel_chunks` gives its chunks), when the groups of the file are full but for the last;
- operators that any thread can run in any order: filters, projections, column selections, and the probe of a hash or
  blockwise nested loop join (`PhysicalJoin`). Once its build side is finished it does not change, apart from the flags
  of the build rows that found a match, which are atomic, so threads probe it without a lock;
- a sink with a state per thread:
  - `PhysicalAggregate` with one grouping set, no DISTINCT or ORDER BY, and aggregates that `is_order_independent`:
    `count`, `sum`, `avg`, `min` and `max`. `first` and `list` depend on the order, and the aggregates that keep their
    rows do their work at the end, where threads only add merging (`median` got slower);
  - the build side of `PhysicalJoin`. Each thread keeps its chunks with their keys, and the hash of a key puts it in
    one of 64 partitions. `finalize` looks up the keys of each partition in a table of its own, on as many threads as
    sank, once there are 100,000 build rows. Threads cut the time of a large build side most, as looking up keys takes
    nearly all of it. One thread puts all keys in one partition, so that probing does not hash a key twice;
  - both inputs of `PhysicalDelimJoin` (`parallel_sink_input`): the left rows go into a collection per thread,
    put together in morsel order, and the right side is the build of its join;
  - `PhysicalResultCollector`, as `PhysicalBatchCollector`: `sink_local` is told the morsel of each chunk, and the
    rows of each morsel go into the result in morsel order, which is the order of one thread.

A scan with a filter of a table in its file reads a row group at a time (`scan_group_filtered`): the
conjuncts of the filter go one by one, those of one column first, and each reads its columns only for the
rows the ones before kept (`TableSource::fetch_rows`, C++'s `ColumnData::Select`); the columns of the
output are read last, for the rows left. `DiskTable` reads single rows of constant, uncompressed and
bit-packed segments when at most a quarter of a group's rows are wanted, and decodes the column otherwise.

Expressions that are volatile or run a subquery keep a pipeline on one thread. Threads are `std::thread::scope` per
pipeline, not a scheduler, and there are no events or blocked tasks. `examples/parallel_aggregate.rs` gives 2.9x to 5.5x on
8 threads, once a query has been warmed: its first run builds what later ones find cached, and that is single threaded.
`examples/parallel_join.rs` gives 3.4x to 5.4x on joins of 10 million rows with 1 million and with 1,000.

Floating point sums over large tables can differ in their last digits between runs, as they do in C++. Groups come out
in another order, which SQL allows, and so do the matches of a key in the build side of a join, as threads add their
rows in the order they finish. The piecewise merge, IE and nested loop joins are that join with another lookup, so they
probe on threads too, with the build side sorted once instead of the sorted runs of both sides that C++ merges. Not
ported: cross products, AsOf and the other special joins on threads, sort and window on threads, parallel CSV and
`InterruptState`.

### Spilling

C++ keeps the data of operators in blocks of the buffer manager, which evicts what is not pinned, so any operator can
go past `memory_limit`. The data of operators here is not in blocks, so spilling is up to the operator
(`execution/temporary`):

- `QueryMemory` holds a query's limit and what has been taken of it. A spilling operator asks for the bytes it is about
  to hold (`estimated_bytes`: fixed-size values, strings, a guess for nested values, and for an aggregate what its map
  and states take for each group), and spills when it gets a no. Tables, the query result and the operators that do not
  spill are not counted, so the limit bounds what spilling operators hold, not the process. It reaches each thread of
  a query through a thread-local that the threads of a pipeline inherit.
- `SpillFile` writes chunks in the format of `DataChunk::Serialize` from the log, to `temp_directory`: `.tmp`, or the
  database file's path with `.tmp`. The directory is made with the first file and removed after the query if it was
  made for it, and a file is removed once nothing reads it. `max_temp_directory_size` errors as in C++ when the files
  would pass it; by default there is no limit. GEOMETRY and VARIANT can't be written, so rows with them do not spill.
- `memory_limit` (`max_memory`) defaults to 80% of the machine's memory, takes the sizes and percentages of
  `ParseMemoryLimit`, and shows as `BytesToHumanReadableString` does. The default shows as NULL, not as a size.

What spills:

- **ORDER BY** (`external_sort`): rows past the limit are sorted into a run that goes to a file, of at least 16 vectors
  however little memory there is. The runs and what is left in memory are merged on the way out, the smallest key first
  and of equal keys the one of the earlier run, so ties keep their order as in memory. More than 32 runs are merged into
  fewer first. A Top-N keeps few rows and does not spill.
- **GROUP BY** (`PhysicalAggregate`): once the groups in memory are at the limit, the rows of groups that are not
  there go to one of 64 files by the hash of their group. The groups in memory are output first, then each partition is
  aggregated on its own, and partitioned again with other bits of the hash if it is too large too, three levels deep.
  With one thread a group is either in memory or in a partition, so the rows of a group stay in order for `first`,
  `list` and ORDER BY in an aggregate. Threads can each have a group, so then the groups in memory join their
  partition. This needs one grouping set, groups to group on, and groups that come out the same when computed again.
- **Hash joins** (`PhysicalJoin`, a grace hash join): once the build side is at the limit, the largest of its 64
  partitions, about half its rows with keys, go to files, the rows left in memory are compacted in their order, and the
  later rows of spilled partitions go to their files. One thread keeps all keys in one partition until then, and they
  are split by hash when it first spills. A probe row whose key is in a spilled partition goes to a file of that
  partition too, from any thread. A join that can spill is a source after the probe, as RIGHT and FULL joins are: once
  its own unmatched build rows are out, it reads back each spilled partition, builds it in memory, probes it with its
  rows, and emits its unmatched build rows, with the same code for every join type. MARK joins look at whether the
  whole build side, files included, has rows and NULL keys. The rows of spilled partitions come after the others, and
  a spilled partition is joined in memory whatever its size: a key with very many rows is not split further. This needs
  join keys that come out the same when computed again; nested loop joins do not spill.

`examples/out_of_core.rs` sorts and groups 10 million rows with and without `memory_limit='100MB'`: the answers are
the same, and the time is 0.7x to 1.4x. The process takes about 1.5 GB for a sort that takes 2.2 GB without the limit,
and 1.6 GB for an aggregate of 10 million groups that takes 3.5 GB, of which the table is 0.7 GB. Most of the rest is
memory the allocator keeps after the partitions are done: running the query again in the same process takes no more.
Joining 10 million rows with 5 million takes 1.3 GB instead of 1.7 GB, and the joins are 2.6x to 3.4x faster with the
limit, as each partition's table is small enough to stay in the cache.

Operators that spill no longer stop at 2 GiB, so a query that failed there now runs to `memory_limit`.
`test/sql/optimizer/plan/test_filter_pushdown_large.test` builds a hash table of a 100 million row cross product that
way, and passes in about 7 minutes, past the runner's timeout: building one large table is slow (Phase 13).

Not ported: spilling windows, nested loop joins, the other blocking operators and the query result, which keep the
2 GiB cap, and the `outofcore` tests, which need TPC-H's `dbgen`.

## Phase 9

Files stay byte-compatible with DuckDB, the choice the plan recommends: `storage` reads what DuckDB
writes, and block-level files that it makes open in DuckDB, which went on to store and checkpoint a
table in one. What exists so far reads a database into memory at open, with its views, macros,
sequences and types; tables are not scanned in place yet and nothing but the block layer writes.

- **Block file** (`block_manager`): the main header, the two database headers (the higher iteration
  wins), checksums and blocks of 16 to 256 KiB. `write_header` syncs the blocks first, so a crash
  leaves the last header or this one. A corrupt current or spare header fails the open, as in C++.
- **Buffer manager** (`buffer_manager`): a cache of blocks with a memory limit, least recently used
  first. A pinned block is an `Arc`, and one that is held stays however far over the limit that
  goes. Spilling and `memory_limit` errors come with Phase 8's temporary files.
- **Metadata** (`metadata`): C++ reads a chain of sub-blocks as a stream, but `BinaryDeserializer`
  reads a slice, so a chain is read into memory without its pointers and `MetadataChains` remembers
  where each sub-block landed. The columns of a table are pointers into one chain, which would
  otherwise be read anew from each of them.
- **Catalog** (`checkpoint`): serialized properties have neither a type nor a length, so nothing can
  be skipped without understanding it. Statistics and the sample of a table are in front of the
  row group pointers, so they are decoded just to get past them (`statistics`). Elements that are
  pointers in C++ (`unique_ptr`, `shared_ptr`) have a presence flag in front.
- **Columns** (`table_data`, `compression`): uncompressed, constant, RLE, bitpacking (all four modes
  and hugeint), dictionary, FSST, DICT_FSST, ALP, ALPRD and Roaring (for validity and booleans), plus
  validity, overflow strings and the empty validity segment, which says nothing: DICT_FSST keeps
  NULLs as dictionary index 0. A scan decodes a whole segment, row by row. Packed values are one
  stream of bits, least significant first, whatever fastpforlib does to get there, so one reader
  handles all widths. LIST (with MAP), ARRAY and STRUCT (with UNION) are read column by column into a
  nested `Vector`; a list stores cumulative offsets into its child.
- **Deleted rows**: a row group's first delete pointer leads to one mask per vector, and the rows that
  are left go through a selection.

What is read comes from outside, so it is bounds-checked, and damage is an error, never a panic.
Segment counts must add up to their column's rows before anything is decoded or allocated.

A file opened read-only stays as it is. A file opened for writing, or made because there was none,
takes writes, which go to its log at each commit and to the file with `CHECKPOINT`, when the log
passes `checkpoint_threshold` and when the database goes away.

These fail with an error that says they are not ported, because going on would give wrong answers
or lose data:

- shredded VARIANT columns, and GEOMETRY columns;
- triggers;
- `CREATE TABLE ... AS` queries, partition and sort keys, table options, and schemas with options;
- in a stored query: INSERT, UPDATE, DELETE, MERGE, COPY, SHOW and MATCH_RECOGNIZE;
- `ALTER` and updates of a field of a STRUCT in a write-ahead log, encrypted files and logs.

These stop writes, or fail when they are made, as a file can't hold them yet:

- indexes of an extension: a file that has them can be read, and takes no writes;
- files from before v2.0.0, which can be read but not checkpointed into, as checkpoints write v2.0.0;
- VARIANT and GEOMETRY columns, and in a stored query what can't be read from one either. These fail
  at the commit, which leaves the database in memory as it is, until what can't be stored is dropped.

Next: shredded VARIANT and writing older `STORAGE_VERSION`s are the largest of the rest.

**Indexes**: the ART of each index in a file is parsed past and not read, whichever way the file
points to it (`index_storage_infos`, or `index_pointers` and `root_block_pointer` in old files). C++
doesn't read it at load either: an `UnboundIndex` holds on to it until something needs it. Here nothing
ever does, as keys are checked through an ART built from the rows (`TableCatalogEntry::unique_keys`,
built when first needed and dropped when rows move, with positions in the table for row ids). A
`CREATE INDEX` entry goes through `bind_create_index` and joins the table without a scan, as the file
has these rows under the index already. An index of a type that isn't ART came with an extension: it
can be read, but only the extension can build it again, so its file takes no writes.

Scans use them too (`optimizer/index_scan.rs`). C++ tries this when a scan starts: a key on the one
column that has filters, and it gives up if more rows match than `index_scan_max_count` or
`index_scan_percentage` allow. Planning and running a statement are one step here, so the pass
does the same and leaves the rows in the plan, with filters that still run on them: all it has to
do is find every row that matches. `EXPLAIN ANALYZE` shows "Index Scan". Like C++ this handles one
column, one expression: constants, `IN`, `IS NOT DISTINCT FROM`, and a range with a bound on each side.
With values to look up, whatever else the filters say of the column is left to them.

These trees (`constraints::index_tree`) are not `unique_keys`, which only has the keys that have to be
unique. They grow with rows added at the end. Rows that go don't take them down: a tree has rows by
id, and `RowIds` says where an id is now, if anywhere. Past a quarter of them gone it is built again,
as it is when ids stop meaning what they did (`rows_rewritten`). In-place updates change neither, as
they don't touch a key: an update of a key, or of a column that isn't updated in place, is a delete
and an insert, for UPDATE and `ON CONFLICT DO UPDATE` alike (`update_moves_rows`). A tree needs the rows in memory, so the first use reads a table that is still in a file.

C++ also makes a filter of the keys of a small build side when a join runs, which an ART can answer
(`dynamic_or_filter_threshold`). Here that is decided when planning, as for filters: the other side
runs ahead of the statement if it reads little and gives the same rows twice (`scan_join_keys`). The
join still does the matching. Only INNER and SEMI joins do this, as the rest keep rows without a match.
`test_art_internal_issue_4742` can't pass anyway: its answers depend on the sampler of C++.

Statistics of a key are unknown when they are worked out for a table that lost rows (`table_statistics`). C++ doesn't narrow them
on delete, so it goes to the index for a key that is gone; here that would be proven empty first,
with the same result but another plan. Indexes are catalog entries for conflicts too: a transaction
that creates or drops one holds its name (`changed_entries`). What a key calls can't depend on the
session, and a macro can't go while a key calls it (`index_using_macro`). `COMMENT ON INDEX` makes a
new entry, for a rollback to lose it, and goes to the log as dropping the index and creating it.

What goes into a file is for C++, which enforces keys through these trees and stops doing so without
them. `index_data` builds one for each PRIMARY KEY, UNIQUE, FOREIGN KEY and index from the rows that
a checkpoint writes, by the ids they have in the file, and its buffers go through `place` like segments
do. The tree is built anew each time, as tables change by being rewritten, so a checkpoint costs
as much per key as inserting it. Constraints are named as in `GetIndexInfo`, though C++ takes those by
position; indexes it takes by name. Only the side of a foreign key that has it is written: reading it
gives the referenced table its side.

The log has `CREATE_INDEX` with the buffers in it, built from the rows with the ids that they get when
it is replayed (`TableLog`). There are no records for ALTER, so a table with a new entry goes and
comes back, and with it what references it, which would keep it from being dropped. A replay numbers
those rows without gaps, which is why `TableLog` has the ids of both: records keep working, and only
after a crash do those rows have other ids. Updates of a STRUCT column do the same, as there is no
update record for one yet: the table goes in again.

**Transactions** (`catalog/fork.rs`): C++ keeps versions in rows and catalog sets, with an undo buffer
to get back. Tables here change by being rewritten, so a transaction gets its own copy of the catalog
instead, a `Fork`, whose tables share their buffers with those of the database. It starts when a
transaction first uses a database, or at BEGIN with `immediate_transaction_mode`. Whatever binds or
runs a statement is only given that catalog, which is all the isolation there is to it, and ROLLBACK
is dropping the fork. A statement outside of a transaction still works on the database itself, under
`statements`, so that nothing costs more without transactions.

Conflicts are found as they happen, as in C++, by row id. `RowVersions`, shared by a table and its
forks, says who holds which row and when each last changed. The four places that
rewrite rows (`PhysicalDelete`, `PhysicalUpdate`, `upsert`, `merge`) tell it what they do. A row
that someone else holds, or that changed after the transaction started, fails with "Conflict on
update!" or "Conflict on tuple deletion!". A conflict is on the row, whichever columns change. Catalog
entries do the same through `CatalogVersions`: a statement that may have changed entries looks at
what differs afterwards, and goes back to a snapshot if it lost.

A commit takes `statements`, and brings over what the fork changed. If nobody else changed a table
that is the rows of the fork, or just the rows it added at the end. Otherwise they are merged by id
into what is there now, and the keys are checked over the result, which is how two transactions
that add the same key find out ("Failed to commit: ..."). What can fail comes first, for a commit to
do all or nothing, and the log then writes the difference as for any other statement. With no forks
left all of this goes away again.

**Statistics** of a table (`table_statistics`) only get wider once they exist, as in C++: rows that go
leave them as they are, and new values are added by reading only rows that are new or changed
(`TableStatistics::covered`, `updated`). Whoever calls `write_storage` says what became of the rows
(`rows_removed`, `rows_appended`, `rows_updated_in_place`), and what nobody reports counts as all of
them. Zones stay for the row groups before the first row that changed.

**Temporary objects** are a connection's: each has a schema of its own, which `Catalog::get_schema`
finds through a thread local for as long as a statement of that connection runs (`SessionTemp`).

**Row ids** (`catalog/row_ids.rs`): `rowid` is what a row keeps until it goes, as in C++, not where it
is. `RowIds` lists runs of rows whose ids and positions go up together, with the id of the next row,
so a table without deletes has one run, and ids are given out as somebody asks for them (`fill`):
what is added at the end needs no one to say so. What a statement adds and takes back gives its ids
back (`RevertAppend`). Rows of a transaction count from `MAX_ROW_ID` until the commit numbers them.

A CHECKPOINT next to open transactions leaves free blocks alone, as those read tables from the blocks
they were in. Rows that a transaction added or deleted go along with one: they are in the fork, and
their ids stay as no row groups come together then. One is refused next to a transaction that updated
rows or changed the catalog, and next to one that doesn't see an update, as in C++
(`ModifyDatabase`). FORCE CHECKPOINT waits for those.

A commit doesn't check foreign keys again. C++ doesn't either: a child row added in a transaction
stays when another one deletes its parent first, which was tried on the real thing.

What isn't there: the commit checks keys but not foreign keys against what others did since. Reads
don't take `statements`, as nothing writes to a fork from elsewhere, but `SELECT` outside of a
transaction still excludes writers. A transaction that writes to two databases commits them one after
the other, so a crash in between keeps one (C++ refuses this). The cost is per table at the start of a
transaction, and a table's keys are copied the first time it needs them.

**Checkpoints** (`writer`, `to_stored`): the whole database goes into blocks that the last checkpoint
doesn't use, and a new header makes it count, so that a crash leaves the last checkpoint as it was.
Afterwards every table is back in the file, and the blocks of the last checkpoint are free. Tables
are read through `TableCursor` as they are written, so that doesn't take them into memory either.

Files are v2.0.0, where constants are literals and casts name parsed types, so the AST goes into a
file nearly as it is. Each column of a row group takes the smallest of what `encode` has, unless
`force_compression` says otherwise: RLE and BitPacking for numbers, ALP and ALPRD for floats, Roaring
for validity, and the modes of DICT_FSST for strings. `fsst` trains a table per row group on a sample
of its distinct strings, in the rounds of `third_party/fsst`, and a dictionary with it competes on size
with one without. A segment where nothing repeats and with no NULLs in the row group needs no indices
(`FSST_ONLY`). There is no FSST segment on its own: v2.0.0 doesn't write those, in C++ either, and
they are only read. C++ picks with an analysis of its own, so its choice can
differ. A segment of one value is Constant, whatever was chosen, as in C++, except that one value here means
its bits: DuckDB's statistics hold -0.0 and 0.0 as equal, which must not make a column of both one zero. BitPacking uses frames of
reference and constants, not deltas, and not HUGEINT; Roaring uses arrays and bitsets, not runs. Those
are smaller or faster at times, but what is written is valid either way. ALP takes five pairs of
exponent and factor from a sample of the row group, and has each vector pick
among them on 32 of its values; a value is an exception unless it decodes to the same bits. A row
group has one ALPRD dictionary, so the vectors of its segments share it. ZSTD, Chimp and Patas are read
and not written. Segments share blocks, as a block per column would make small tables
large, which DuckDB has to be told of in `multi_use_blocks`. After the free list a file has the list of
metadata blocks with their free sub-blocks: nothing in `duckrs` reads it, but C++ doesn't find them
otherwise. Numeric columns have bounds; strings say that nothing is known of them.

`tools/check_written.py` has the v2.0.0 reference engine read a file from `write_database_file.rs`,
and then write to it, so that it frees and reuses blocks by what the file says.

**Attached databases**: each is a `DatabaseInstance` of its own, with its catalog, file and log, in
`attached` of the one that a connection was made to. `USE` makes a connection run in another one, and
a statement that names one (`db.t`, `db.main.t`) is routed to it with that name taken off. A statement
runs in one database, so one that reads two isn't ported, and subqueries in expressions aren't looked
at for names. `STORAGE_VERSION` below v2.0.0 and `ROW_GROUP_SIZE` are refused, as is encryption.

The gate is not met: `test/sql/storage` is at 62.2% against 85%. About 40 of the files that fail need
keys in a file, which can't come before Phase 10.

**Failing on purpose.** `debug_checkpoint_abort` stops a checkpoint where a crash could (after the free
list, before the header, before truncating), and `debug_force_commit_failure` and
`debug_force_wal_fsync_failure` fail a commit, with `before_wal_flush` as the crash at a commit. A commit
fails before anything of it reaches the log, so nothing is torn as in C++, only absent, which recovery
treats the same way. Outside a transaction there is nothing to roll back to, so a snapshot is taken only
while one of these is set. A failed CHECKPOINT invalidates the database (`ValidChecker`), or all of them
if it is the one the connection was made to, and a database that is invalid neither runs anything nor
checkpoints at the end, as after a crash. `before_moving_recovery` and
`before_deleting_checkpoint_wal` have no point to stop at, as replay here has no recovery file.

`ATTACH (ROW_GROUP_SIZE)` belongs to the attachment and isn't in the file, as in C++. It only decides
how checkpoints cut rows, so rows that are in memory keep the groups of `execution::zone_map`.

**Writing the log** (`log_writer`): the engine has no undo log and tables change by being rewritten,
so a commit doesn't know what it did. It compares the catalog with what is durable, and writes the
difference as the records of C++: `Arc`s tell which entries are new, and `TableLog` what is durable of
the rows of a table. That comes to the same few records for all statements, and what a transaction undid
never shows up. Everything that can fail is checked before any of it is written, as `TableLog` moves on.

Adding rows logs those rows, and rows that went are `DELETE_TUPLE`s of the ids that aren't there any
more. What changed in place the table keeps a list of (`TableLog::updated`), which goes out as
`UPDATE_TUPLE`s of the columns that changed, for all those rows. A STRUCT takes more than one update
record, so such a table goes into the log again, which is how a table with a new entry goes in too.

A log that exists when a file is opened for writing goes into the file first: it can end in anything
a crash leaves, so nothing is added to it. A checkpoint writes a marker with its root to the log first.
A crash after the new header then leaves a log that replaying knows to skip, and without the marker
it would be replayed onto a file that has it all.

`commits_survive_kill_9` kills a process that commits and checkpoints at random moments, and reopens
the file: what was acknowledged is there, and of a transaction all or nothing is. It writes to a real
file with `fsync`, but it can't lose what the operating system holds, as a power failure could.
`tools/check_written.py` has the reference engine replay a log too.

**Tables in place** (`DiskTable`, `TableCursor`): rows stay in the file, and `TableSource` is how the
engine reaches them without knowing about files. A scan decodes the row group it is at, only the
columns that its projection and filter refer to, through the buffer manager, which keeps 256 MiB of
blocks whatever `memory_limit` says for now. Metadata is read at open, as are deletes, as the number
of rows has to be known. Decoding is what fails on damage or ZSTD, so those errors come from the
query that reads the rows and not from opening the file; bad metadata still fails the open.

`storage` of a table is private, so what wants all rows goes through `rows` and `write_storage`,
which read them into memory for good: a write, or a log that changes the table. A snapshot for ROLLBACK
keeps the source and reads nothing. Everything else stays in the file, with what it knows: statistics
of the table for the optimizer and those of each row group as zone maps, so that a filter skips a
group without reading it. Those are the bounds of numeric and temporal columns. A file only has a
prefix of strings, so VARCHAR columns have no zones here, and the bounds can be wider than the values,
after deletes, which is safe as they are only used to rule rows out.

The engine numbers chunks as in memory: all but the last are full, and 60 of them make a group. When a
file's groups match that, a scan jumps to the group it wants. Otherwise (deletes, or another
`ROW_GROUP_SIZE`) `TableCursor` goes through the groups in order and a buffer makes up the chunks, so
zone maps come from reading the column. Ids come from `row_start` and the deletes of each group.

A checkpoint writes the row groups that the file has, new ones of `ROW_GROUP_SIZE` for what came
after them, and none for one whose rows all went: that is the gap that `next_row_id` is there for.
Rows that went from a row group that stays are NULL with a delete vector (`deletes_of`), as the
engine doesn't have them any more where C++ keeps writing them. `vacuum_groups` is
`ScheduleVacuumTasks`: row groups that fit in fewer come together, whose rows get new ids from the
first one's start, and indexes follow as they are built from what is written. Never with a transaction
open, which goes by the ids it knows. Only v2.0.0 files are written, so what C++ does for storage
that can't hold gaps isn't here.

**Write-ahead log** (`wal`): all records of a log are read before any is applied, which is what the
first pass of C++ does. What follows the last WAL_FLUSH was never committed and a log that stops
inside an entry is what a crash leaves, so neither is an error; a wrong checksum is, and so is a log
of another database or checkpoint. Files are only opened read-only, so this is the read-only path
of C++: the log and the file stay as they are, and a checkpoint that didn't finish is read around, with
`.wal.checkpoint` if there is one. Records go through what loads a checkpoint.

Records name rows by row id, and rows that were deleted before the last checkpoint leave gaps, so
`LiveTable` keeps the id of each position while a log is replayed. Only a table that a log changes is
read into memory for that. Deletes and updates are collected
and the tables they touch are rewritten at the end. C++ logs an update twice, the values and a
validity update on the path `[column, 0]`, as the value of a NULL is whatever the vector held; a
cell keeps both. Large inserts write row groups to the file ahead of the commit, past the blocks the
last checkpoint knows of, and the log says where (`ROW_GROUP_DATA`).

`ALTER` is an error for now: it takes the `ParseInfo` specs, and replaying it has to change rows.

**Parsed objects** (`serialization`): `tools/generate_serialization.py` is the counterpart of
DuckDB's generator. It reads the JSON specs and the enums of the C++ headers and writes
`generated.rs`, so field ids stay in step with upstream by running it again. C++ deserializes into
the classes of its parser, but the AST here has a shape of its own, so the generated structs mirror
the specs and `convert.rs` turns them into the AST by hand. `custom.rs` has what C++ reads by hand
too: casts, functions and windows, which read the fields of older versions, and lambdas, whose
syntax depends on the version of the file. Only reading is generated.

Entries stay in the order of the file, in which each comes after what it depends on. A table goes
through the binder like `CREATE TABLE`, for its defaults, generated columns and constraints; views
and macros aren't bound until they are used, as nothing in a file needs that. Types that a file has
already are `CastType::Resolved`. A type that is only named, like a cast to a type of the catalog,
is `LogicalType::unbound`, which keeps the SQL of the name and is resolved when bound. Parsed
expressions are beyond `duckrs-common`, so reading one inside a `LogicalType` goes through
`BinaryDeserializer::with_type_expressions`.

Where this differs from C++: `lead` and `lag` from before v2.0.0 keep their offset and default,
which the C++ reader drops. A constant from before v2.0.0 is a value and not text, so a type that
no literal reads back as, like DOUBLE, becomes a cast of its text.

`crates/duckrs/tests/read_database_file.rs` compares every row of files from the real DuckDB with what
DuckDB reads from them; `tests/fixtures/generate.sh` makes both. `damaged_database_file.rs` changes
bytes of a block and fixes its checksum, to get past that to the decoders. Layout checks match C++
exactly and not just bounds: an FSST symbol table in the wrong place is corruption even if it is in
bounds, as otherwise strings decode to garbage without an error.

Of the 25 database files under DuckDB's `data` and `test`, 15 open, the Patas and Chimp ones (`float_codecs`)
among them. The rest fail on encryption (2), and 8 are refused as DuckDB refuses them: damaged, of another
vector size, or too old.

`stored_queries.txt` is one query per line, stored as a view in files for v1.0.0, v1.5.0 and v2.0.0.
Every view gives the rows that DuckDB gives, and on the v1.5.0 file `duckdb_views()` renders each
query as DuckDB does, apart from the differences between the `ToString` of the 1.5.6 CLI that writes
the fixtures and the baseline, which the test lists. The v2.0.0 file comes from the reference engine
of the test runner, as the CLI is too old for it; its rendering isn't compared. Writes aren't ported, so
`storage::tests` looks at the catalog for keys, foreign keys and indexes.

`scan_tables.sql` makes tables of two row groups, or of many small ones, and `scan_queries.txt` asks
what zone maps and statistics could get wrong: every type of bound, NULLs, NaN, empty tables and
deleted rows. On a file of 1.1 GB, 40 million rows of four columns, `sum` of an integer column took
0.7 s and 180 MB of memory, and a filter on the first column 5 ms and 13 MB.

`wal_checkpointed.sql` and `wal_logged.sql` make files whose changes are partly only in a log, for
the same three versions, and every table has to come out as DuckDB has it after replaying that log.
Replay only needs a log, so these are `.wal` files in `tests/fixtures`, which `.gitignore` lets through.
DuckDB 1.5.6 can't replay a logged index read-only, so `generate.sh` has it read a copy.

The tests compare every column as the text that DuckDB prints, FLOAT and DOUBLE included.

## Phase 5

The scalar functions that came last are in `function::extras`, and JSON's in `function::json`. They bind by name, as
their types depend on their arguments, so they are not overloads and `duckdb_functions()` wouldn't know them. New
aggregates are `Holistic`, which keep their rows: `approx_top_k` and `bitstring_agg` are exact, and `min(x, n)` sorts.
Named arguments of aggregates are read by position.

**EXPORT_STATE** gives an `AGGREGATE_STATE`: a type that names the aggregate, its arguments and its result, whose
values are the bytes of an `AggregateState` (`LegacyAggregateState` is the id, as nothing else used it). `finalize`,
`combine` and `combine_aggr` work on those, and a state survives a table. In C++ a state is a STRUCT with a layout per
aggregate, and most of `test/sql/aggregate/aggregate_state_export` prints it or casts it. Those tests fail here, as does
`to_aggregate_state`, which builds a state from that layout. The bytes are ours, so a state means nothing to C++, and
a state with a sketch in C++ holds its rows here. Matching C++ means a state per aggregate: `Holistic` keeps rows, and
can't turn into Welford's fields or a t-digest and back.

The gates are not met: see `docs/STATUS.md`. What holds the most files back is that layout; the box rendering of EXPLAIN, which the `explain_value`
tests read; `test_vector_types`; COLLATE; and VARIANT columns in database files.

## Phase 11

CSV is in, on one thread: C++'s state machine and sniffer are ported, its buffers and parallel scan are not.
Parquet is in as the plan's v1, through the `parquet` crate. JSON has a first slice. EXPORT and IMPORT have not started.

- **Files** (`execution::file_system`): globbing with `*`, `?`, `[...]` and `**`, and reading a whole
  file. Compressed files fail as not ported, by extension or by magic number, instead of reading as garbage.
  Gzip goes through `flate2`, read and written, by extension or `compression`. Zstd is not ported.
- **The state machine** (`execution::csv_state_machine`) is C++'s transition table, byte by byte: quotes,
  escapes, comments, multi-byte delimiters, strict mode and all three newlines. `Machine::process` is
  `BaseScanner::Process` over a `Sink`, so `ColumnCountScanner`, `SkipScanner` and `ValueScanner` get the same
  events in the same order as in C++. A file is in memory whole, so there are no boundaries between buffers.
- **The sniffer** (`execution::csv_sniffer`) follows `CSVSniffer` step by step: the search space of dialects and
  `AnalyzeDialectCandidate`, refinement over `sample_size` rows, date and timestamp formats, type candidates and
  how they fall back, header detection, `normalize_names`. Its error messages are C++'s.
- **read_csv** (`execution::csv`, `binder::copy`): rows come from `ValueScanner`, and are typed a chunk at a time
  through casts, a date format, `thousands` or `decimal_separator`. Errors carry C++'s text: the line, the fixes and
  `CSVReaderOptions::ToString`. `read_csv_auto` is the same function. A file named like a table (`FROM 'a.csv'`)
  scans it once the name is a file, and `glob()` is in. `encoding` takes utf-8, latin-1 and utf-16, by converting
  the whole file.
- **Several files**: the first gives the schema. Each later file is sniffed as it opens, with its columns found by
  name, and a column it misses is a schema mismatch unless both lack a header. Extra columns are dropped.
  `union_by_name` sniffs every file while binding. `filename` adds a column. `hive_partitioning`, which is detected
  when all files have the same `key=value` directories, adds DATE, TIMESTAMP, BIGINT or VARCHAR columns after it.
- **COPY FROM** is an INSERT from `read_csv`, with the table's columns fixed, as C++ plans it.
  **COPY TO** is `PhysicalCopyToFile`, which opens the file with its first row or at the end, so that
  planning (EXPLAIN) leaves it alone. It writes a header unless told not to, and TSV is CSV with tabs.
  `dateformat` and `timestampformat` go through `strftime`.
- **sniff_csv** reports what the sniffer found, as a row of constants with the `Prompt` of C++.
- **store_rejects** fills temporary `reject_scans` and `reject_errors` tables when the scan ends, one
  row per failed value, missing column or extra column.
- **A name given to a table function** binds as its text, as in `TableFunctionBinder`: `read_csv("a.csv")`,
  `types=[INT]`.

Not like C++: scans are single threaded, so `buffer_size` only caps a line's length; the rows a rejected line
reports can differ where C++ finds several errors on one line; `max_line_size` is measured from the start of a row.

Not ported, each an error and never ignored: `hive_types`, partitioned, per-thread and size-limited writes,
`read_text`, the `blob` format.

**Parquet** (`execution::parquet`, `binder::parquet`) reads and writes through the crate's Arrow reader and
writer, and `LOAD parquet` is a no-op. Rows pass as `Value`s between Arrow and vectors, so it is complete rather
than fast, and neither filters nor projections reach the file, so every column of every row group is decoded.

- **read_parquet**, `parquet_scan` and `FROM 'a.parquet'` take `binary_as_string`, `filename`, `file_row_number`,
  `hive_partitioning` and `union_by_name`. Later files are matched by name and cast to the first file's types.
- **COPY** with FORMAT PARQUET takes `compression`, `compression_level`, `row_group_size`, `parquet_version`,
  `kv_metadata`, `data_page_size_limit`, the bloom filter options and `PARTITION_BY`, with `overwrite`,
  `overwrite_or_ignore`, `write_partition_columns`, `filename_pattern` and `file_extension`. HUGEINT goes out as
  DOUBLE as in C++. TIME WITH TIME ZONE and BIGNUM go out as text, which is not what C++ writes.
- **parquet_metadata**, `parquet_schema`, `parquet_file_metadata` and `parquet_kv_metadata` are rows of constants.
  Their geometry, encoding stats and page index columns are not ported.
- **Corrupt files**: the crate panics on some, so its calls run in `catch_unwind` and a panic is that query's error.
  The panic still prints to stderr, because a library must not change the process's panic hook.

Not like C++: the crate picks page encodings and row group boundaries, so tests of `parquet_metadata` that name
encodings fail, and the checks of corrupt files are the crate's rather than C++'s, so some bad files read without
an error. Not ported, each an error: VARIANT, GEOMETRY, INTERVAL writes, `field_ids`, encryption, `bound_schema`,
`schema`, `order_by`, `return_stats`, `parquet_bloom_probe`, virtual columns (`filename` without the option),
CSV `PARTITION_BY`, and a directory as a path.

**JSON** (`function::json`, `execution::json_scan`, `binder::json`) is a first slice. The type is VARCHAR with the
alias `JSON`, as in C++, and `LOAD json` is a no-op. Text is parsed per row into `Json` by a parser of our own that
takes yyjson's flags, instead of yyjson.

- **Functions**: `json_extract` and `->`, `json_extract_string` and `->>`, `json_value`, `json_exists`, `json_type`,
  `json_valid`, `json_keys`, `json_array_length`, `json_contains`, `json_structure`, `json_transform` and its strict
  form, `to_json`, `json_object`, `json_array`, `json_merge_patch`, `json_pretty`, `json_strip_nulls`, `json_set`,
  `json_insert`, `json_replace`, `json_remove`, `json_group_array`, `json_group_object` and `json_group_structure`.
  They bind by name rather than through overloads, so a wrong argument type fails late or not at all, and they are
  not in `duckdb_functions()`. `a -> b` outside a function that takes a lambda is the JSON operator. `j.a` extracts.
- **read_json**, `read_ndjson`, `read_json_objects`, their `_auto` forms and `FROM 'a.json'` read a file whole. The
  format is decided from the file, and types from a sample: objects become STRUCT, or MAP past
  `map_inference_threshold`, and strings become DATE, TIMESTAMP, TIMESTAMP WITH TIME ZONE or UUID if they all fit.
- **COPY**: TO writes one object per line, or an array with ARRAY. FROM is an INSERT from `read_json`.

Not ported: `json_serialize_sql` and its kin, `json_serialize_plan`, GeoJSON, `json_deep_merge`, `json_normalize`,
`json_merge_patch_diff`, `field_appearance_threshold`, `hive_types`, `PARTITION_BY` for JSON. Nesting is limited to
4096 levels, which C++ doesn't limit, and the parser recurses.

The gate is not met: `test/sql/copy` is at about 51% against 85%, now that the Parquet tests in it run.

## Shell (`duckrs-cli`)

The `duckrs` binary ports `tools/shell`. `main.rs` holds `ShellState`: the command-line options
(two passes, as C++: those needed to open the database, then the rest after the init file),
`ProcessInput` (lines are gathered until a statement ends in a `;` outside quotes and comments,
and a line starting with `.` is a dot command), and `ExecuteStatement`. Each statement runs on its
own through `Connection::query`. The statement's kind (from `duckrs_parser`) decides whether its
result is rows to show: DDL, `SET` and the like answer with an empty `Success` column, and INSERT,
UPDATE and DELETE with a `Count` that only `.changes` reports.

- `commands.rs`: the dot commands, in the C++ table order, found by prefix with the same minimum
  lengths (`.sep` is `.separator`).
- `renderers.rs`: the output modes other than duckbox (`ColumnRenderer` and `RowRenderer`): column,
  table, markdown, box, latex, line, list, tabs, csv, html, tcl, ascii, quote, json, jsonlines, insert.
- `box_renderer.rs`: `src/common/box_renderer.cpp`, the default duckbox mode.
- `table_metadata.rs`: `shell_render_table_metadata.cpp`, used by `.tables`, `.databases` and DESCRIBE.
- `highlight.rs`: `shell_highlight.cpp`: colours for errors and results.

Two engine hooks exist for the shell. `Connection::add_replacement_scan` gives rows for a table
name the catalog does not have; the shell uses it for `_`, the last result (`ShellScanLastResult`).
Opening or attaching a Parquet, CSV or JSON file makes an in-memory database with the views `file`
and the file's stem (`OpenFileStorageExtension`), in `DatabaseInstance::open_with`.

Interactive input goes through `rustyline`, with the history in `~/.duckdb_history` (or
`$DUCKDB_HISTORY`). The shell's pytest suite (`duckdb/tools/shell/tests`) runs against the binary:

```
cd ../duckdb && PYTHONPATH=. pytest tools/shell/tests --shell-binary ../duckrs/target/release/duckrs
```

## Benchmarks (`tools/benchmark`)

`benchmark_runner` ports `benchmark/benchmark_runner.cpp` and `interpreted_benchmark.cpp`. It runs in a
DuckDB checkout (`--root-dir`, `../duckdb` by default), finds the `.benchmark` files whose path or group
matches the pattern, and for each: opens the database (or its cache under `duckdb_benchmark_data/<engine>/`),
runs `load`, a cold run and `--timed-runs` timed runs of `run`, and checks each result. It drives the
engines of the sqllogictest crate; `Connection::query_timed` times the engine and not the conversion of
the values to text. With `--features reference --engine=both` it also times the C++ library and prints
the ratios, their geometric mean and the v1 gate.

Neither engine here has the `tpch` or `tpcds` extension, so `CALL dbgen(sf=N)` and `CALL dsdgen(sf=N)`
in a load are done by the runner: `tools/tpch/build_dbgen.sh` and `tools/tpcds/build_dsdgen.sh` build
the generators of the checkout, whose `.tbl` files are copied into tables of the extensions' columns.

```
cargo run --release -p benchmark -- 'benchmark/tpch/sf1/.*'
cargo run --release -p benchmark --features reference -- --engine=both --threads=8 'benchmark/micro/.*'
cargo bench -p duckrs --bench kernels
```

Larger scale factors go through `--sf N`. The benchmark measures SF10 on the file C++ writes: run the reference
engine once (`--engine=reference --sf 10`), copy `duckdb_benchmark_data/reference/tpch_sf10.duckdb` to
`duckdb_benchmark_data/duckrs/`, then `--engine=both --sf 10`. duckrs can load SF10 into a file of its own too
(CREATE TABLE and `COPY ... FROM` the `.tbl` files, about 2.5 minutes, under 2.2GB at the most, as appends write
their full row groups to blocks as they fill), and `TPCH_DATABASE=<file> cargo run --release -p duckrs --example
tpch -- - ../duckdb/extension/tpch/dbgen 10` checks the 22 queries on it against `answers/sf10`.

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
