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
out-of-core operators (Phase 8) and the rest of storage (Phase 9) exist. Errors for SQL beyond the slice
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
  When the query filters the outer rows the deepest such join stays, as DuckDB does, so most of
  TPC-H keeps its delim joins: the distinct values make the subquery read less. A join with no
  `DelimGet` left is an ordinary one, except SINGLE, which stays a `DelimJoin` because that operator
  raises the error for two rows. SINGLE becomes LEFT when an aggregate proves one row per key.
  Joins with an inequality stay, since those are predicates here. With a scalar subquery whose
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

- ZSTD, Chimp and Patas;
- VARIANT and GEOMETRY columns;
- triggers;
- `CREATE TABLE ... AS` queries, partition and sort keys, table options, and schemas with options;
- in a stored query: INSERT, UPDATE, DELETE, MERGE, COPY, SHOW and MATCH_RECOGNIZE;
- `ALTER` and updates of a field of a STRUCT in a write-ahead log, encrypted files and logs.

These stop writes, or fail when they are made, as a file can't hold them yet:

- keys and indexes in a file: C++ keeps the ART of each in the file, and with none it takes the table
  for one without data and stops enforcing the keys. That has to wait for Phase 10. A file that has
  them can be read, and takes no writes;
- files from before v2.0.0, which can be read but not checkpointed into, as checkpoints write v2.0.0;
- VARIANT and GEOMETRY columns, and in a stored query what can't be read from one either. These fail
  at the commit, which leaves the database in memory as it is, until what can't be stored is dropped.

Next, in order: ALP and ALPRD when writing, then FSST and the rest of the error messages of the
decoders, which `test/sql/storage/compression` compares with those of C++; then keys in files, with
the ART of Phase 10. ZSTD takes a new
dependency, and Chimp and Patas are only in old files.

**Indexes**: the ART of each index is parsed past and not read, whichever way the file points to it
(`index_storage_infos`, or `index_pointers` and `root_block_pointer` in old files). C++ doesn't read
it at load either: an `UnboundIndex` holds on to it until something needs it. Here nothing ever does,
as the engine keeps its keys unique from the rows (`TableCatalogEntry::unique_keys`, built when first
needed) until Phase 10 ports the ART. A `CREATE INDEX` entry goes through `bind_create_index` and
joins the table without a scan, as the file has these rows under the index already. An index of a
type that isn't ART came with an extension; as nothing here reads it, it is treated like the others.

**Checkpoints** (`writer`, `to_stored`): the whole database goes into blocks that the last checkpoint
doesn't use, and a new header makes it count, so that a crash leaves the last checkpoint as it was.
Afterwards every table is back in the file, and the blocks of the last checkpoint are free. Tables
are read through `TableCursor` as they are written, so that doesn't take them into memory either.

Files are v2.0.0, where constants are literals and casts name parsed types, so the AST goes into a
file nearly as it is. Each column of a row group takes the smallest of what `encode` has, unless
`force_compression` says otherwise: RLE and BitPacking for numbers, Roaring for validity, and the
dictionary mode of DICT_FSST for strings. C++ picks with an analysis of its own, so its choice can
differ. A segment of one value is Constant, whatever was chosen, as in C++. BitPacking uses frames of
reference and constants, not deltas, and not HUGEINT; Roaring uses arrays and bitsets, not runs. Those
are smaller or faster at times, but what is written is valid either way. ALP, ALPRD, FSST, ZSTD,
Chimp and Patas are read and not written. Segments share blocks, as a block per column would make small tables
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

The gate is not met: `test/sql/storage` is at 45.8% against 85%. About 40 of the files that fail need
keys in a file, which can't come before Phase 10.

**Writing the log** (`log_writer`): the engine has no undo log and tables change by being rewritten,
so a commit doesn't know what it did. It compares the catalog with what is durable, and writes the
difference as the records of C++: `Arc`s tell which entries are new, and `TableLog` what is durable of
the rows of a table. That comes to the same few records for all statements, and what a transaction undid
never shows up. Everything that can fail is checked before any of it is written, as `TableLog` moves on.

Adding rows logs those rows. Anything else that changes a table logs the deletion of all its rows and
inserts them again, so a small UPDATE of a large table writes all of it to the log. Ids stay when rows
go, so `TableLog` keeps what they are; rows that came from a file with deletes have ids with gaps.

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
zone maps come from reading the column. Row ids are positions, as they were in memory: after deletes
that isn't the id in C++.

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

Of the 25 database files under DuckDB's `data` and `test`, 12 open. The rest fail on Patas and Chimp
(3) and encryption (2), and 8 are refused as DuckDB refuses them: damaged, of another vector size, or
too old.

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
