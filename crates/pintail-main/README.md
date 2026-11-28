# pintail-main

`pintail-main` is the client layer of pintail: the database instance, the per-connection state
(`ClientContext`), materialized results, prepared statements, transactions, `ATTACH`/`DETACH`,
settings, EXPLAIN and profiler output, `EXPORT`/`IMPORT DATABASE`, data-modifying CTEs, triggers
and query logging. It ports DuckDB's `src/main` and a few binder and renderer files that sit close
to it. It takes SQL text, sends each statement through the parser (`pintail-parser`) and the
binder, optimizer and executor (`pintail-core`), and collects the rows. It sits on top of
`pintail-common`, `pintail-parser` and `pintail-core`. The only workspace crate that depends on it is
the public facade, [`pintail`](../pintail/README.md). It was split out of `pintail-core`
(`pintail-core/src/client`) on 2026-11-23, in merge commit `1fdc0fe`; `client/mod.rs` became
`src/lib.rs`, and the old `pintail_core::client::X` is now `pintail_main::X`.

## At a glance

| | |
|---|---|
| Lines of Rust | 18,269 (`src/lib.rs` is 9,409 of them, about 4,400 of those unit tests; `src/settings_table.rs`, 2,410, is generated) |
| Main entry points | `DatabaseInstance::{new, open_file, open_file_with_version, open_with}`, `ClientContext::{new, query, prepare, execute_prepared, append, table_columns, add_replacement_scan, setting}`, `MaterializedQueryResult` |
| C++ it ports | `src/main` (`database.cpp`, `client_context.cpp`, `materialized_query_result.cpp`, `config.cpp`, `settings/custom_settings.cpp`), `src/common/tree_renderer`, `src/common/render_tree.cpp`, `src/common/settings.json`, and the binder files for EXPORT, CTEs and triggers listed under Layout |

## Dependencies

| Crate | Used for |
|---|---|
| `pintail-common` | `Value`, `LogicalType`, `Error`/`ErrorKind`, `ColumnDataCollection` (the rows of a result), `DataChunk`, storage versions |
| `pintail-parser` | `parse_ast_with_text` and `parse_ast_with` turn SQL into `Statement`s; the AST types the client matches on (`Statement`, `CreateInfo`, `AttachInfo`, `ExplainType`, ...) |
| `pintail-core` | the catalog (`Catalog`, `Fork`, `DatabaseList`), the `Binder`, `optimize`, `create_plan`, `execute_plan_with`, `PhysicalResultCollector`, storage (`BufferManager`, `WriteAheadLog`, encryption), the profiler and the log manager |
| `parking_lot` | `Mutex` and `RwLock` around the open transactions, the statement lock, settings and the attached databases |

There are no other external crates.

## Layout

| File | What it does | Ported from |
|---|---|---|
| [`src/lib.rs`](src/lib.rs) | `DatabaseInstance`, `AttachOptions`, `ClientContext`, `MaterializedQueryResult`, `PreparedStatementData`; the statement loop, transactions (`BEGIN`/`COMMIT`/`ROLLBACK`, forks of the catalog), routing to attached databases, `ATTACH`/`DETACH`/`USE`, search paths, `CREATE`/`DROP` of views, macros, types, sequences and schemas, `PRAGMA`, `CHECKPOINT`, the commit and write-ahead log path, profiling output; and the unit tests | `src/main/database.cpp`, `client_context.cpp`, `materialized_query_result.cpp` |
| [`src/settings.rs`](src/settings.rs) | `SET`, `RESET`, `PRAGMA x = y`, `current_setting`; global and session values (`SettingValues`), `find_setting`, `list_settings` (for `duckdb_settings()`), the `UNHONORED` list, allowed paths and directories | `src/main/config.cpp`, `src/main/settings/custom_settings.cpp`, `execution/operator/helper/physical_{set,reset}.cpp` |
| [`src/settings_table.rs`](src/settings_table.rs) | `SETTINGS`: 197 `SettingDefinition`s (name, aliases, input type, scope, default, enum values, description). Generated, do not edit | `src/common/settings.json`, the `DUCKDB_SETTING_ALIAS` entries of `src/main/config.cpp` |
| [`src/explain.rs`](src/explain.rs) | `ExplainNode`, the tree EXPLAIN and the profiler show; logical, physical and analyzed trees; renderers for JSON, YAML, graphviz, mermaid, compact and HTML; the `Summary` box of the profiler | `src/common/render_tree.cpp`, `src/common/tree_renderer/*.cpp`, the `ParamsToString` methods of the operators |
| [`src/text_tree_renderer.rs`](src/text_tree_renderer.rs) | the box drawing of the default EXPLAIN output (each operator a box, its first child under it, the others to the right, joined by `├────╮`) | `src/common/tree_renderer/text_tree_renderer.cpp` |
| [`src/html_template.html`](src/html_template.html) | the page `FORMAT html` fills in (`__PLAN_JSON__` is replaced by the plan); a byte-for-byte copy of the C++ file | `src/common/tree_renderer/html_template.html` |
| [`src/export.rs`](src/export.rs) | `EXPORT DATABASE` (a COPY of each table into a directory, plus `schema.sql` and `load.sql`) and `IMPORT DATABASE` (runs the two; `load.sql` may only hold COPY statements) | `planner/binder/statement/bind_export.cpp`, `execution/operator/persistent/physical_export.cpp`, `PragmaImportDatabase` |
| [`src/dml_cte.rs`](src/dml_cte.rs) | INSERT, UPDATE, DELETE, MERGE and COPY as CTE bodies of a top-level query: each runs as its own statement, in the query's transaction, and later CTEs and the query read its rows as a table | `planner/binder/query_node/bind_cte_node.cpp` |
| [`src/triggers.rs`](src/triggers.rs) | `CREATE TRIGGER`, `DROP TRIGGER`, and running BEFORE / AFTER, FOR EACH ROW / STATEMENT bodies around an INSERT, UPDATE or DELETE, with the rows as transition tables | `bind_create.cpp` (`BindCreateTriggerInfo`), `bind_trigger_expansion.cpp`, `trigger_catalog_entry.cpp` |
| [`src/logging.rs`](src/logging.rs) | a logger per statement, the query log, the logging settings, `enable_logging`, `disable_logging`, `truncate_duckdb_logs` | `client_context_execution.cpp` (`RefreshLoggerInternal`), `custom_settings.cpp`, `function/table/system/logging_utils.cpp` |
| [`src/secret/mod.rs`](src/secret/mod.rs) | `SecretManager` (one per `DatabaseInstance`): the `memory`, `local_file` and `connection` storages, lookups by the longest matching scope, CREATE/DROP SECRET errors, the extension hints of types and providers C++ core does not have, and the `.duckdb_secret` files (escaped names, private files written under a temporary name) | `src/main/secret/secret_manager.cpp`, `secret_storage.cpp` |
| [`src/secret/key_value.rs`](src/secret/key_value.rs) | `KeyValueSecret`: scope matching, the redacted `secret_string`, and its binary serialization | `src/main/secret/secret.cpp` |
| [`src/secret/default_secrets.rs`](src/secret/default_secrets.rs) | the `http` secret type with its `config` and `env` providers | `src/main/secret/default_secrets.cpp` |
| [`src/secret/statements.rs`](src/secret/statements.rs) | `CREATE SECRET` (expressions evaluated as constants), `DROP SECRET`, rollback of secret changes, and `SecretView`, what `duckdb_secrets()`, `duckdb_secret_types()`, `duckdb_secret_type_parameters()` and `which_secret()` read through `pintail_core::planner::binder::SecretTables` | `bind_create.cpp`, `physical_create_secret.cpp`, `function/table/system/duckdb_secret*.cpp`, `duckdb_which_secret.cpp` |

`settings` is the only public module; the others are private and add `impl ClientContext` blocks
(`pub(super)` functions) that `lib.rs` calls.

## Key types and APIs

- **`DatabaseInstance`** (`DatabaseInstance` in C++). Holds the `Catalog` (public field
  `catalog`), the global settings, the set of open transactions, the statement lock, the database
  file and its write-ahead log, the databases `ATTACH` added (`attached`, by lower-case name), the
  log manager and the query and transaction counters. Constructors return `Arc<DatabaseInstance>`:
  - `new()`: in memory.
  - `open_file(path, read_only)`: a database file; it is created if missing and `read_only` is
    false.
  - `open_file_with_version(path, read_only, version)`: as `open_file`, with a storage version
    such as `"v1.0.0"` for a new file.
  - `open_with(path, &AttachOptions)`: with the options `ATTACH` can give (block size, row group
    size, storage version, `HIDDEN`, `DEFAULT_TABLE`, encryption, ...).
  It also implements `pintail_core::catalog::DatabaseList`, which lets the engine find the catalog
  of another database by name (`with_catalog`) and list databases (`databases`, for
  `duckdb_databases()`). Hidden databases are left out of the listing.
- **`ClientContext`** (`ClientContext`). One per connection. It holds the current database, the
  connection's open transaction, session settings, `PREPARE`d statements, `SET VARIABLE` values,
  the search path, temporary objects, replacement scans and profiler state.
  - `new(db)`: a connection to `db`.
  - `query(sql) -> Result<MaterializedQueryResult>`: runs all `;`-separated statements and returns
    the result of the last one.
  - `prepare(sql) -> Result<Arc<PreparedStatementData>>`: exactly one statement, with `?` or `$n`
    parameters. More than one statement is the error `Cannot prepare multiple statements at once!`.
  - `execute_prepared(&prepared, &[Value])`: runs it with values for `$1`, `$2`, ...; too many or
    missing values are C++'s `Parameter argument/count mismatch` errors.
  - `append(schema, table, columns, chunks)`: rows of the table's types, inserted as
    `INSERT INTO t (...) SELECT * FROM __internal_appender_chunks()`, so constraints, triggers and
    the WAL apply as for any INSERT. `pintail::Appender` uses it.
  - `table_columns(schema, table)`: names and types of a table's columns (for the appender).
  - `add_replacement_scan(scan)`: rows for a table name the catalog does not have.
  - `setting(name) -> Option<Value>`: the current value, session before global.
- **`MaterializedQueryResult`**: public fields `names`, `types` and `collection` (a
  `ColumnDataCollection`), and `row_count()`, `column_count()`, `value(column, row)`.
  `pintail` re-exports it as `QueryResult`.
- **`AttachOptions`**: what `ATTACH` can say of a file; also used by `open_with`.
- **`settings`**: `SettingDefinition`, `SettingScope`, `SettingValues`, `find_setting`,
  `get_setting`, `current_value`, `list_settings`, `set_variable`, `reset_variable`, `check_lock`,
  `file_access`, `external_access_enabled`.

Most users should go through [`pintail`](../pintail/README.md). Using the crate directly looks like
this (the same calls as [`tests/file_scans.rs`](tests/file_scans.rs)):

```rust
use pintail_main::{ClientContext, DatabaseInstance};

fn main() -> pintail_common::Result<()> {
    let db = DatabaseInstance::new();
    let mut context = ClientContext::new(db.clone());
    context.query("CREATE TABLE t (i INTEGER); INSERT INTO t VALUES (1), (2), (3)")?;
    let result = context.query("SELECT sum(i) AS total FROM t")?;
    assert_eq!(result.names, ["total"]);
    println!("{}", result.value(0, 0).try_to_string()?);

    let prepared = context.prepare("SELECT i FROM t WHERE i > $1")?;
    let rows = context.execute_prepared(&prepared, &[pintail_common::value::Value::integer(1)])?;
    assert_eq!(rows.row_count(), 2);
    Ok(())
}
```

## How it works

### What `ClientContext::query` does with a SQL string

1. **Parse.** It enters the connection's temporary schema (`SessionTemp::enter`) and calls
   `pintail_parser::parse_ast_with_text(sql, &parser_options)`, which gives each `Statement` with
   its own text. A parse error inside a `BEGIN` transaction goes through the transaction's
   invalidation policy first (`apply_invalidation_policy`), then is returned.
2. **Per statement**, in order, with `current_query` set to the whole text:
   1. Logging starts for the statement (`begin_statement_logging`, in `logging.rs`).
   2. If profiling is on (`enable_profiling`), a fresh `OperatorProfiler` is installed for the
      statement (`with_profiler`), and the file logger is installed (`with_file_logging`).
   3. `old_implicit_casting` is applied for the duration of the statement.
   4. `verification_statement`: with `debug_verify_statement` set to `explain_sql` or
      `explain_sql_strict`, a SELECT is rendered back to SQL from its optimized plan, parsed
      again, and that statement runs instead.
   5. `run_statement`, below.
   6. Logging ends. Unless the statement was `EXPLAIN ANALYZE`, a profiled statement writes its
      profile (`write_profile`, to `profiling_output` or the result).
   7. The first error stops the loop and is returned. Otherwise the last statement's result is
      the result.
3. **`run_statement`** decides where and how the statement runs:
   - It sets the file system's home directory and file search path from the settings, and
     refuses `CREATE` in the `system` catalog, `pg_catalog` or `information_schema`.
   - `route_to_database`: a statement whose table names start with an attached database's name
     runs again against that database, without the prefix.
   - It sets the search path for the database, and fails with C++'s `Catalog ... does not exist!`
     if the default database has gone (for example a rolled-back ATTACH).
   - Some statements are handled here: `Multi` (what PIVOT rewrites into) runs as one implicit
     transaction; `BEGIN`/`COMMIT`/`ROLLBACK` go to `run_transaction_statement`;
     `PREPARE`/`EXECUTE`/`DEALLOCATE` go to `execute_statement`; a SELECT, COPY, INSERT, UPDATE or
     DELETE with data-modifying CTEs goes to `run_with_dml_ctes` (`dml_cte.rs`).
   - In an invalidated transaction only ROLLBACK is accepted (`INVALIDATED_TRANSACTION`).
   - `run_with_triggers` (`triggers.rs`): if the target table has triggers, the chain of BEFORE
     bodies, the statement, and AFTER bodies runs and its result is returned.
   - For writes: the database must be writable (`check_writable`), and a transaction may write
     only one database besides the temporary one (`MetaTransaction::ModifyDatabase`'s error).
     A read-only transaction refuses writes.
   - Locking and isolation. Inside a transaction the statement runs on the transaction's `Fork`
     of the catalog (`start_on_database` makes one per database it touches). Outside one, a
     SELECT takes the database's `statements` lock for reading; other statements take it for
     writing, so UPDATE and DELETE cannot rewrite rows under a running scan. A failed INSERT
     truncates the table back to its earlier row count; catalog changes are checked with
     `claim_changes` against a snapshot and restored on conflict.
   - After a successful auto-commit statement it checks deferred keys and forced commit failures
     (debug settings), restoring the snapshot on failure, and then calls `log_commit`, which
     writes the WAL and checkpoints when the log passes `checkpoint_threshold` or
     `wal_autocheckpoint_entries`.
4. **`execute_statement`** handles the statements that do not go through the planner (EXPORT,
   VACUUM/ANALYZE checks, `SET`/`RESET`, `USE`, `ATTACH`, `DETACH`, `PRAGMA`, `EXPLAIN`, `CALL
   checkpoint`, `CREATE VIEW/MACRO/TYPE/SEQUENCE/SCHEMA/TRIGGER`, `DROP`, ...). Everything else
   reaches the common path at its end:
   1. `Binder::with_options(&catalog, bind_options)` with the prepared parameters and the session
      values, then `binder.bind(statement)`.
   2. `optimize_timed(bound.plan, optimizer_options, ..)`, timing each optimizer pass; then
      `join_order::take_error()`.
   3. For `EXPLAIN ANALYZE`, a copy of the optimized plan and its estimates is kept.
   4. `note_probe_estimates`, `resolve_column_bindings`, then `create_plan` builds the physical
      plan, wrapped in a `PhysicalResultCollector`.
   5. The query memory is set up (`memory_limit`, threads, block memory, transaction start time
      for `now()`), and `execute_plan_with(&collector, threads)` runs the pipelines.
   6. Profiling phases (planner, optimizer per pass, physical planner) and the operator tree are
      recorded; for `EXPLAIN ANALYZE` the analyzed `ExplainNode` tree is built.
   7. The collector's `ColumnDataCollection` becomes the `MaterializedQueryResult`, with the
      bound names and types.

Every result is fully materialized. There is no streaming result in this crate.

### Transactions

`BEGIN` creates an `ActiveTransaction` (read-only if asked) and registers the connection in the
database's open transactions. With `immediate_transaction_mode` it forks the catalog at once;
otherwise on the first statement that touches a database. `COMMIT` commits each fork under that
database's write lock, then writes each database's log; an invalidated transaction rolls back
instead. `ROLLBACK` drops the forks and detaches what the transaction attached. A statement error
invalidates the transaction unless `default_transaction_invalidation_policy` spares that kind of
error.

### ATTACH, DETACH and the database list

`attach` (`PhysicalAttach`) evaluates the path and options, opens the file with
`DatabaseInstance::open_with` (or makes an in-memory database, which shares the home database's
settings), and adds it to the home database's `attached` map. `detach`
(`PhysicalDetach`) removes it; detaching the database the connection was made to only hides it by
name. `ATTACH ... TO EXTERNAL RESOURCE` is unported.

### Settings

Each setting is a `SettingDefinition` in `SETTINGS` (generated) or in `EXTENSION_SETTINGS` (7
extension settings, hand-written in `settings.rs`). `find_setting` looks up a name or alias without
case. Values are stored by index in `SettingValues`, once per database (global) and once per
connection (session); a session value wins. The few settings that change query results are either
honored or listed in `UNHONORED`, where setting a value other than the default fails as not yet
ported. An unknown name gives C++'s `unrecognized configuration parameter` error with suggestions.

### EXPLAIN and the profiler

`explain.rs` turns a logical, physical or analyzed plan into a tree of `ExplainNode`s (name plus
key/value details). `render` picks the output: the text boxes of `text_tree_renderer.rs` by
default, or `json`, `yaml`, `graphviz`, `mermaid`, `compact` or `html` (which fills in
`html_template.html`). `profile_tree` turns the profiler's `ProfileNode`s into the same tree.

## Status and gaps

Evidence is the `pintail_core::unported(...)` calls in `src/`, which fail with an error saying the
feature `is not yet ported to pintail`, and `../../docs/REMAINING_WORK.md`.

- Unported here: `ATTACH ... TO EXTERNAL RESOURCE`, `INSTALL`, some `PRAGMA`s
  (`PRAGMA {name}`), some `CREATE` kinds, macro dependencies, several forms of `DROP` (including
  `DROP SCHEMA` of another catalog), settings of an input type the client cannot store, writing to
  a database file of a storage version from before the supported ones, and settings in
  `UNHONORED`.
- The text tree renderer has no grouped boxes for fast operators and no colour; only full boxes
  are ported (see the header of `text_tree_renderer.rs`).
- `REMAINING_WORK.md` items for this area: 37 (settings, generated in full from `settings.json`:
  `sql/settings` 35 failing), 39 (SQL export and DDL rendering; data-modifying CTEs are open
  there), 40 (logging and error messages), 40a (the profiler: `query_tree` text output,
  `duckdb_available_metrics()`, memory metrics are open), 44 (connection tests).

## Testing

- Unit tests: 88 in `src/lib.rs` (`mod tests`: joins, views, sequences, macros, prepared
  statements, transactions, spilling past the memory limit, EXPLAIN output, optimizer results
  that must match DuckDB, checkpoints, logging, MERGE, ...), 2 in `src/settings.rs` and 2 in
  `src/text_tree_renderer.rs`.
- Integration test: [`tests/file_scans.rs`](tests/file_scans.rs). It opens
  `../pintail/tests/fixtures/scan_v122880.db` (written by C++ DuckDB) read-only and checks that
  queries leave the tables in the file rather than loading them, and that a filter skips row
  groups (read from `EXPLAIN ANALYZE`'s `Row Groups Scanned`).

```sh
cargo test -p pintail-main
cargo test -p pintail-main --test file_scans
```

Most of the behaviour of this crate is checked through the sqllogictest corpus (`tools/sqllogictest`,
see the top-level [`README.md`](../../README.md)), and through the integration tests of
[`pintail`](../pintail/README.md).

## Notes for contributors

- Layering: this crate is the top of the engine. `pintail-core` must not use it; where engine code
  needs something from the client (for example the database list), it goes through a trait the
  engine defines (`DatabaseList`) or a callback. See `../../../CLAUDE.md`, item 1.
- Each file starts with a `// Ported from duckdb/src/<path> @ rust-port-baseline` header; doc
  comments name the C++ function a Rust function stands for (`` `PhysicalAttach` ``,
  `` `ClientContext::Append` ``).
- Error messages are C++'s text, verbatim, so the sqllogictest expectations match.
- `src/settings_table.rs` is generated: change `tools/generate_settings.py` and run
  `python3 tools/generate_settings.py [../duckdb]`.
- `src/html_template.html` is a copy of the C++ file; keep it identical.
- `Value` per row is fine here (this is a cold path), but not in operators and kernels.
- No `unsafe` in this crate; new `unsafe` is not allowed here (`CLAUDE.md`, item 3).
- `cargo clippy --workspace --all-targets -- -D warnings` is the CI gate.
