# Remaining work

What is still missing from the roadmap in [`RUST_PORT_PLAN.md`](../../RUST_PORT_PLAN.md) §6.
The test numbers come from [`STATUS.md`](STATUS.md) at baseline `d4eea3f208d`.

- Overall: 4832 of 6367 test files pass (**79.9%** of the 6044 that run). The v1 target is ≥ 90%.
- Phases 0–11 have code but are below their pass targets.
- Phase 12 is partly done. Phases 13 and 14 have not started.
- In git, the branches named "Phase 13"–"Phase 21" are rounds of fixes against the test suite.
  They are not the plan's Phase 13 (Performance) or Phase 14 (Extensions).

Each item gives the number of failing files in `test/` that it should fix, where known.

## Phase 11: File formats and I/O

- [ ] 1. COPY TO/FROM failures (`sql/copy`: 162 failing, 74.1% pass; target ≥ 85%)
  - [x] `PARTITION_BY` for CSV and JSON (Parquet had it), `PARTITION_BY *`, `PARTITIONED_BY` alias
  - [x] the `ORDER BY` option of COPY TO, and its checks against `USE_TMP_FILE` / `PER_THREAD_OUTPUT`
  - [x] `ORDER_BY` as C++ writes it: each run of buffered rows sorted (with its terms' collations) into a file of its
    own, with or without `PARTITION_BY`; `PRESERVE_ORDER` and `WRITE_EMPTY_FILE false` refused with it
  - [x] COPY as the body of a CTE, and a COPY of a query with one (see item 23)
  - [x] COPY over the file (or a WAL file) of an attached database refused; a prepared parameter as the file name,
    and parameters in COPY options, bound when the statement is executed
  - [x] `read_text` / `read_blob`, with hive partition columns
  - [x] the `filename`, `file_index`, `file_row_number` virtual columns of `read_csv`, `read_json`, `read_parquet`
  - [ ] `file_row_group_number` and the other Parquet-only virtual columns; `filename=1` name conflicts
  - [ ] `read_text` reads content even when only `filename`/`size` is selected, so binary files fail
  - [x] `APPEND`, `HIVE_FILE_PATTERN`, `PARTITION_PATH` (with `hive_partition_component`), `RETURN_STATS`
  - [x] `partitioned_write_max_open_files` and `partitioned_write_flush_threshold`: rows buffered by partition
  - [x] COPY `FORMAT BLOB`; a file extension no COPY function has (`.tbl`) is CSV; reading and writing zstd
  - [x] `read_single_csv_file` / `read_single_parquet_file`
  - [x] hive partitions as C++ finds them: keys as a set and sorted, values unescaped, `hive_types`,
    `hive_types_autocast`, a partition that is also a column of a Parquet file overrides it
  - [ ] `RETURN_STATS` of VARIANT columns, and with `SHREDDING`
  - [ ] gzip output: miniz writes a stored block for tiny inputs, so a 3-byte file is 26 bytes in C++ and 23 here
  - [ ] streamed reads of compressed CSV (`compressed_read_size`); `read_csv`'s `schema` option
  - [x] schema discovery over up to `files_to_sniff` files (`MergeSchemas`), empty files and one-row files in a glob,
    the column mapper's error with hive partitions or `filename`, `dtypes` with `union_by_name`, `filename=1`,
    deduplicated header names in COPY TO, C++'s maximum line size (2,000,000, or the buffer size) and its errors
  - [x] `TypePushdown` of casts into the CSV scan, with the projection `ResolveInputProjection` puts over an INSERT's query
  - [ ] `TypePushdown` into Parquet scans; trivial projections C++ removes (`optimizer/pushdown/csv_type_pushdown`)
  - [x] `enable_external_access`, `allowed_paths` and `allowed_directories`: checked where the binder takes a path
    (readers, `glob`, COPY, ATTACH), EXPORT / IMPORT, the profiler's output, LOAD and INSTALL, `temp_directory`
  - [ ] the checks C++'s opener file system makes elsewhere: secrets, `home_directory`, files opened during execution
  - [x] a name that says the file is compressed in a way nothing here reads (`.bz2`, `.xz`, `.zip`, ...) is an error;
    `glob` of a list of patterns, `\` as a separator, a file whose name has glob characters in it
  - [x] the agreement of `columns` with `types`, unnamed after named arguments, subqueries, aggregates and windows
    in table function arguments; rejects-table lines made valid UTF-8 as `Utf8Proc::MakeValid` does
  - [ ] the other CSV sniffer, rejects-table and error-message differences (about 40 files under `sql/copy/csv` fail)
- [ ] 2. JSON: `read_json`/`read_json_auto`, the JSON type, JSON functions (`sql/json`: 41 failing, 71.7%)
  - [x] `json_normalize`, `json_deep_merge`, `json_merge_patch_diff`
  - [x] `json_each` / `json_tree`, with columns as arguments (LATERAL) and the hidden `json`, `root`, `rowid`
  - [x] escaped quotes and backslashes in quoted JSON path keys; no wildcards in `json_set`/`json_remove`/...
  - [x] documents nested to any depth: parsing, writing, copying, comparing and dropping a `Json` take no stack per
    level, nor do `json_merge_patch`, `json_deep_merge`, `json_merge_patch_diff`, `json_normalize`, `json_strip_nulls`
  - [x] `json_set`/`json_insert`/`json_replace` as `ModifyDocumentElements`: positions up to the end of an array, the
    missing containers on the way made only if the value goes in; malformed values are errors
  - [x] `read_json_objects` gives each record's bytes as they are; strings are UUIDs only in the strict form
  - [x] numbers as yyjson reads them: integers beyond 64 bits and overflowing reals kept as text (`BIGNUM_AS_RAW`),
    every number as text where a DECIMAL reads them (`NUMBER_AS_RAW`), read again for the other columns
  - [x] JSON to UNION and BLOB; `JSON[]` written as its documents; empty objects are `MAP(VARCHAR, JSON)` (or JSON
    with `map_inference_threshold = -1`)
  - [x] `read_json` errors as C++'s reader: transform errors with file, record and hint; the array format's errors of
    the file; with `ignore_errors` a broken line is a row of NULLs, elsewhere it is still an error
  - [x] COPY (FORMAT JSON) options checked as `GetSingleJSONCopyValue` does; column types that don't parse suggest some
  - [x] `json_serialize_sql` / `json_deserialize_sql` / `json_execute_serialized_sql` (and its PRAGMA): the serializer of the
    file format has a JSON mode (`JsonSerializer`/`JsonDeserializer`: properties by name, enums by their `EnumUtil`
    names, nulls/empties/defaults left out as asked); C++'s `VerifyDeserialized` checks, the type and value checks of
    `LogicalType::Deserialize`/`Value::Deserialize`, no POINTERs, a nesting limit of 256
  - [x] each conjunct of a filter's AND sees only the rows the ones before it kept (`SelectAnd`)
  - [ ] `json_serialize_plan`: needs a serializer of logical plans
  - [ ] VALUES lists with expected types (`ExpressionListRef.expected_types`), UNION constants from serialized values
  - [ ] GeoJSON (`read_json` geojson, `ST_AsGeoJSON`, `json_geometry_format`): needs the GEOMETRY type
  - [x] `json_group_object`/`json_group_array` as window functions (`UnfoldWindowMacroExpression`)
  - [x] `case_insensitive_keys`, duplicate keys as errors, `RemoveDuplicateStructKeys`, keys that differ in case made
    unique column names; `read_single_json_file`; the format detected from the first line (`DetectFormatAndRecordType`)
  - [ ] map inference (`map_inference_threshold`, `field_appearance_threshold`): objects with many or inconsistent keys
- [ ] 3. Parquet reader/writer: missing types, filter pushdown, delta encoding (`test/parquet`: 22 failing, `corrupt`: 5)
  - [x] `utf8_validation` (`strict`, `replace`, `ignore`): strings read as bytes and repaired, their bounds not used;
    exact VARCHAR bounds that are not UTF-8 ignored; `decode(BLOB, error_option)`
  - [x] TIME values outside a day are errors, TIMETZ wraps into the day; INT96 timestamps read as microseconds (no
    overflow before 1677); INTERVALs read with their months; UNKNOWN-annotated columns are `"NULL"`
  - [x] `parquet_metadata`: encodings in file order, deprecated and current bounds apart, TIME(NANOS) bounds;
    `parquet_schema`'s `duckdb_type`; `parquet_full_metadata`; invalid DECIMAL precision/scale errors as C++
  - [x] COPY ... FROM passes reader options on and reads into the table's types, with C++'s count and cast
    errors; multi-file cast errors as C++; VARCHAR read as BLOB by its bytes (`union_by_name`)
  - [x] `allow_empty`; `schema` turns off hive detection; hive columns that are file columns aren't cast from it;
    unknown COPY options of Parquet and CSV with C++'s suggestions; signed zeros apart in dictionaries
  - [ ] INT96 out of range (the crate wraps), `timestamp_ns` saturation, legacy `ConvertedType` 24, recursive
    `UNNEST` of structs (`metadata_full`), `WITH ORDINALITY`
  - [x] `stats()` of Parquet VARCHAR / BLOB columns: string statistics from the footers (exact bounds, a constant's
    lengths), unknown ones where none; JSON the file marks as such keeps them under a repairing `utf8_validation`
  - [ ] float statistics of Parquet columns by `nan_count` and `can_have_nan` (`parquet_stats_function`,
    `parquet_nan_setting`)
  - [x] `parquet_metadata` stats in the column's DuckDB type (decimals, UUID, TIMETZ, INT96, blobs; NULL for infinities and out-of-range values)
  - [x] `int96_as` (`timestamp` by default, `timestamp_ns`, `struct`)
  - [x] the native Parquet writer (`execution/parquet_writer`): DuckDB's encodings, dictionaries, pages, statistics, bloom filters and footer, written with our own Thrift; TIMETZ as TIME in UTC
  - [x] NOT NULL columns (by the optimizer's statistics) written REQUIRED; uncompressed sizes now match C++ byte for byte
  - [x] fallback bloom filters (sortedness, distinct estimate, 64 MiB budget, folding), interval bloom hashes, `parquet_bloom_probe`
  - [x] the page index (`write_page_index`), and `parquet_page_index` / the index columns of `parquet_metadata`
  - [x] `FILE_SIZE_BYTES`, `ROW_GROUPS_PER_FILE`, `PER_THREAD_OUTPUT`, `RETURN_FILES`, `WRITE_EMPTY_FILE`, `FIELD_IDS`, TUPLE columns
  - [x] `RETURN_STATS`: the stats unifiers of the writer, nested and NaN counts, a VARIANT `extra_info`
  - [x] `read_parquet(..., schema)`: by field id or name, defaults, nested STRUCT/LIST/MAP children
  - [x] only the columns a query reads are decoded (projection pushdown into the scan)
  - [x] statistics of Parquet scans from the footers (bounds where exact; not floats, INT96, deprecated bounds)
  - [x] C++'s errors for corrupt files (footer length, short decompression, RLE widths, DBP miniblocks, ...)
  - [x] `TIME(NANOS)` reads as `TIME_NS`; Thrift texts of logical types in `parquet_schema`
  - [x] VARIANT columns: reading the binary encoding and shredded `typed_value`s, writing with auto-shredding and
    `SHREDDING`, `variant_to_parquet_variant` / `variant_bytes_to_variant`
  - [ ] encryption, UNION and GEOMETRY columns
  - [ ] rotation inside partitions groups rows into files differently from C++ (`partitioned_file_rotation`)
  - [x] reading a directory path, as `FALLBACK_GLOB` globs `dir/**/*.ext` (also for CSV and JSON)
  - [x] filters pushed into the scan (`table_filters`), run as each batch is read; row groups their statistics
    or bloom filters rule out are skipped (the zone map checks of tables), shown as `Row Groups Scanned` in EXPLAIN
    ANALYZE; the scan shown as C++'s `PhysicalTableScan` (`Function`, `Projections`, `Filters`); its estimate
    from the footers (`GetCardinality`)
  - [x] hive partitions and file names pruned before the scan (`ApplyFiltersToFileList`): `File Filters`,
    `Scanning Files`
  - [ ] reader side: pruning by page indexes and by bloom filters of nested values, filters through functions with
    statistics of their own (`list_contains`, `map_extract`, ...), `bound_schema` (needs struct field
    projection and `EXPLAIN (SQL)`), `maximum_files_for_stats`; the same pushdown into CSV and JSON scans
  - [ ] corrupt files C++ rejects that the crate reads (INT96 out of range, WKB types, page index row counts)
  - [ ] prefetch / read-ahead metrics and EXPLAIN shapes of Parquet scans (~20 files)
  - [x] duplicate column names (`_1`, `_2`, ... when read, deduplicated when written) and nameless columns (`C0`, ...)
  - [ ] `write_timestamp_as_int96`, `parquet_metadata` logical type names (`Float16Type()`)
- [ ] 4. Parquet VARIANT, including shredding (`parquet/variant`: 15 failing, 68%)
  - [x] reading, writing, auto-shredding, the `SHREDDING` option, VARIANT to JSON
  - [ ] the key dictionary in insertion order (`VariantNode` keeps keys sorted, so bytes differ from C++)
  - [x] `vector_type`, VARIANT ordering and comparison (`variant_comparator`)
  - [ ] `stats()` of VARIANT, `vector_type` of shredded and unnested vectors as C++ has them, case-sensitive shredded
    extraction, `debug_delta_only_variant_encoding_enabled`
- [ ] 5. `EXPORT DATABASE` / `IMPORT DATABASE` (`sql/export`: 4 failing, 1 skipped for ICU)
  - [x] a COPY of each table, `schema.sql` and `load.sql` as C++ writes them, IMPORT of the two, an attached source database
  - [ ] macro dependencies (`export_macros`, `export_generated_columns`), `enable_external_access`
  - [x] HUGEINT and UHUGEINT written as VARCHAR to Parquet, as `ParquetWriteSelect` does for an EXPORT
  - [ ] UNION columns in Parquet
- [ ] 6. `COPY FROM DATABASE` (`sql/copy_database`: 11 of 12 pass)
  - [x] `(SCHEMA)`, `(DATA)` and `PRAGMA copy_database`, read in one database and appended in the other
  - [ ] `currval('db.schema.seq')` of a sequence in another database
- [ ] 7. MultiFileReader: hive partitioning, `union_by_name`, filename column (`sql/multi_file`: 4 failing)
- [ ] 8. GeoParquet (`geoparquet`: 7 failing)

## Phase 9: Storage engine

- [ ] 9. Remaining storage failures (`sql/storage`: 142 failing, 76.3%; target ≥ 85%)
  - [x] files in older storage versions: `ATTACH (STORAGE_VERSION ...)`, `storage_compatibility_version`, the
    header numbers, C++'s version gates in the generated and hand-written serializers, compressions by version
  - [x] DICTIONARY and FSST segments for files before v1.3.0; reading and writing ZSTD segments
  - [x] compression chosen as `DetectBestCompressionMethod` does, by the `final_analyze` score of each method in
    C++'s order (UNCOMPRESSED, RLE, BITPACKING, ROARING for validity and booleans), logged as C++ logs it;
    bitpacking with all of C++'s modes (CONSTANT_DELTA, DELTA_FOR) and types (BOOL, HUGEINT, UHUGEINT)
  - [x] strings from v1.3.0 on: UNCOMPRESSED, ZSTD and DICT_FSST by their scores (a port of DictFSSTAnalyzeState),
    and EMPTY validity where DICT_FSST covers the NULLs
  - [x] DICT_FSST segments as C++ fills them (`DictFSSTCompressionState`: a plain dictionary until the block is
    full, then FSST encoded if that saves enough, without indices where all strings are unique), with libfsst
    ported exactly (the sample, `buildSymbolTable` with its counters, `finalize`, `compressBulk`): segments are
    those of C++ byte for byte, but for a padding byte C++ leaves as it was
  - [ ] before v1.3.0, DICTIONARY and FSST by score (their analyze samples at random, so C++ itself differs from
    run to run; the smaller encoding is taken), the analyze of ALP and ALPRD (scored by their encoded size)
  - [x] blocks as C++ hands them out at a checkpoint: metadata sub-blocks the lowest free first, in the file's
    metadata blocks before new ones (`MetadataManager::AllocateHandle`), those the last checkpoint took freed
    unless kept (`MarkBlocksAsModified`, `ClearModifiedBlocks`), the free list's room taken as
    `GetFreeListBlocks` does; an unchanged table keeps its metadata whole (`WriteUnchangedTable`); deletes in
    a writer of their own and kept when they didn't change, as `ValidityMask::Write` packs them; segments into
    the fullest partly filled block they fit (`PartialBlockManager`)
  - [x] large appends written ahead of a checkpoint (`OptimisticDataWriter`): an INSERT, COPY or CREATE TABLE AS
    writes its row groups to blocks as they fill, five at a time a column at a time, into blocks that get their
    ids as they fill and stay open between those writes until the commit (`APPEND_TO_TABLE`), and keeps only
    the rest in memory; the commit writes the rest of a bulk append the same way and logs where its row groups
    are (`ROW_GROUP_DATA`). A table is then its rows in the file, those row groups and rows in memory, one after
    another (`AppendedTable`), read where they are; an append to a table in a file doesn't read its rows. A
    checkpoint keeps the row groups' blocks, under a DELETE too, and a replay keeps those of a log of C++ in
    theirs (`MarkBlockAsUsed`). An append of 80M rows takes 90MB rather than refusing to keep more than 2 GiB.
    `enable_optimistic_write` turns it off. Not for tables with a key or an index, nor in an explicit
    transaction, whose appends go in at the commit
  - [ ] a one-row VARCHAR column: C++ stores it as DICT_FSST, duckrs uncompressed
  - [x] the ART layouts of files before v2.0.0, in checkpoints and the WAL: from v1.2.0 the current nodes with
    the 264-byte `Node256Leaf` (`GetNode256LeafSegmentSize`), before it `ART::TransformToDeprecated` (chains of
    `Leaf` of 4 row ids, 15-byte prefixes, 6 allocators); the `storage_version` option is the file's
  - [x] dense row id rewriting of files before v2.0.0: row groups without rows go and the ids after them move
    up, unless indexes keep the ids (`KEEP_ROW_IDS`: then only the trailing ones go)
  - [x] GEOMETRY in files and logs before v1.5.0: the spatial extension's BLOB format and type, the keys of
    an index of one (`ART::ConvertKeyInput`), the `storage_version` each index keeps, and no vacuum remap of an
    index with such keys (`KEEP_ROW_IDS`)
  - [x] ATTACH's and the setting `vacuum_rebuild_indexes`: indexes of tables up to that many rows are built
    again after a vacuum that moves their rows (`VacuumIndexStrategy::REBUILD`)
  - [x] `storage.total_vacuum_time` of the checkpoint profiling; under `KEEP_ROW_IDS` row groups without deletes
    whose ids follow each other still come together
  - [x] unshredded VARIANT columns, in the file and the WAL
  - [x] shredded VARIANT columns: read from files C++ writes, and written at a checkpoint by
    `variant_minimum_shredding_size` and `force_variant_shredding`, with the type inferred as C++ does (object fields
    in name order where C++ has its hash order) and the statistics of the typed part
  - [x] `stats()` of a VARIANT: its shredding state and the statistics of the typed part, from files, through
    casts to and from VARIANT and `variant_extract`; row groups merge their typed parts (`MergeShredding`), and
    one a checkpoint leaves as it is keeps how it was shredded
  - [x] pushdown extract into table and parquet scans: fields of STRUCT and VARIANT columns read only through
    extracts (and casts of them) are columns of the scan (`s.y.z` in EXPLAIN), also in SQL export
  - [x] reading a pushed field of a VARIANT by keys from the typed part of the row groups that have it there,
    in parallel, serial and filtered scans of a file (7-10x faster over 1M rows)
  - [x] the statistics of the pushed fields, those of their extracts, for compressed materialization
  - [x] `SHREDDED_VECTOR`: what a scan reads of a shredded VARIANT row group or Parquet column
  - [x] the statistics of the unshredded part, a row group the checkpoint does not shred as `UNINITIALIZED`
    (which merges into what the others are), and the row groups a delete emptied dropped in files before
    v2.0.0, the rows after them taking new ids where no index keeps them
  - [x] Patas and Chimp segments of the files before v0.9, with C++'s checks of damaged ones
  - [ ] shredded GEOMETRY columns, `read_duckdb` (8)
  - [x] list-length statistics: the shortest, longest and total length of a table's lists, kept through appends,
    checkpoints (v2.1.0, with the lists' `ListPersistentColumnData`) and WAL replay; checkpoints write v2.1.0 when
    asked; a table's statistics keep those of what is in its nested columns
  - [ ] string statistics for `stats()` from database files, per-column `USING COMPRESSION`
  - [x] shredded VARIANT columns in the file: read and written as C++ does (`variant_minimum_shredding_size`,
    `force_variant_shredding`, the type the values suggest), with their statistics
  - [x] GEOMETRY in a VARIANT; `vector_type` of a shredded scan (SHREDDED_VECTOR); `stats()` of a VARIANT column:
    its validity and `shredding_state`
  - [x] the statistics of the shredded part in `stats()` (`fully_shredded`, `partially_shredded`), merged across
    row groups as `MergeShredding` does, also of casts to VARIANT, of casts of a VARIANT, and through extracts;
    nested (STRUCT, LIST, ARRAY) child statistics in the file; the empty bounds of an all-NULL numeric segment,
    which C++ scans a constant segment with
  - [ ] of shredded VARIANTs: pushdown of extracts into the shredded part; C++'s order of the fields of a shredded
    object (that of an `unordered_map`); shredded GEOMETRY
  - [x] a crash between the end of a checkpoint and the removal of its log: a log that its marker says is in the
    file goes on open (`IsCheckpointClean`), and the checkpoint of a replayed log marks it where what it committed
    ends, before the new header goes in
  - [x] a checkpoint keeps the row groups whose rows are as the last one left them
    (`REUSE_EXISTING_ROW_GROUP_METADATA`): their column metadata, blocks and statistics stay, and only the row group
    and its deletes are written again. Whether rows changed is found by comparing them with the file's.
  - [x] columns load when something reads them (`RowGroup::is_loaded`, `LoadColumn`), and `pragma_storage_info`'s
    `loaded_segments_only` lists only those; a checkpoint leaves loaded what it wrote and what a kept row group had
  - [x] `PARTIALLY_REUSE_COLUMN_METADATA` (from v2.0.0, or v1.4.0 with `force_column_metadata_reuse`): a row group
    says which metadata blocks each column takes (`PerColumnMetadataBlocks`), and a checkpoint writes again only
    the columns whose values changed; an ALTER keeps the columns it leaves as they were (ADD, DROP, ALTER TYPE).
    Before v2.0.0 a row group lists the blocks of all its columns together (`extra_metadata_blocks`).
  - [x] ALTER TABLE in the WAL (`ALTER_INFO`, `WriteAlter` and `ReplayAlter`): ADD, DROP and RENAME COLUMN, ALTER
    TYPE, SET and DROP DEFAULT and NOT NULL, RENAME TO are read, and an ALTER of a column is logged as such unless
    the transaction changed the rows first, the table has indexes, or an ALTER TYPE gives the rows new ids; a replay
    keeps the ids the log has the rows under. SET DEFAULT keeps the text of the default too.
  - [x] columns that stay in the file through UPDATE and ALTER (`LayeredTable`): an UPDATE in place of a table in a
    file reads and changes only its columns in the row groups it updates, and ADD, DROP and ALTER TYPE COLUMN only
    compute the column they make; a commit takes the table's columns as they are, the log reads only the updated
    columns, and a checkpoint writes the columns in memory and keeps the others unread. An UPDATE with RETURNING,
    of a table with a CHECK or that a foreign key references, and an ALTER that checks the rows read the table.
  - [x] DELETE of a table in a file (`DataTable::Delete`): the rows go from which rows of each row group are left,
    and nothing but the columns of the WHERE is read; a later UPDATE or ALTER keeps that, a commit and the log
    take the ids, a replay of deletes leaves the rows in the file too, and a checkpoint keeps the columns and
    writes the deletes. A DELETE with RETURNING, of a table with a key or an index, or that a foreign key
    references reads the table.
    columns, `read_duckdb` (8)
  - [x] string statistics for `stats()`: a minimum length too long to keep is left out, a checkpoint leaves a table the
    statistics it computed until the file is opened again (a file before v2.0.0 keeps 8 bytes of a string bound),
    and Parquet strings have the bounds of the file
  - [x] list-length statistics (`ListStatsData`): the shortest, longest and total of a column's lists, in a row
    group's column (`ListPersistentColumnData`) and the table's statistics from v2.1.0, which checkpoints now write;
    `length` of lists all as long is a `constant_or_null`, `list_concat`, `flatten`, list constants and ARRAY to
    LIST casts give their lengths, and joins, cross products and UNNEST drop the totals of strings and lists
    (`ResetAdditiveStatistics`)
  - [x] per-column `USING COMPRESSION`: CREATE TABLE refuses methods the file's storage version has no more or not
    yet and those that can't store the column's type (`VerifyCompressionType`), ADD COLUMN keeps one unchecked,
    the catalog, the log and `sql` keep it, and a checkpoint stores the column, its validity and children with
    only that method and UNCOMPRESSED in place of `force_compression` (`ColumnCheckpointInfo::GetCompressionType`)
  - [x] `read_duckdb` and the replacement scan of `.db` files; `enable_view_dependencies` (DROP ... CASCADE,
    refused DROP and ALTER, CREATE OR REPLACE of a view that reads itself)
  - [ ] `vector_type` (~20 files) and `duckdb_memory` (~17) report engine internals duckrs does not model
- [ ] 10. `ATTACH`/`DETACH` and multi-database features (`sql/attach`: 47 failing, 52.5%)
  - [x] the search path (`CatalogSearchPath`): `SET schema`, `SET search_path`, `USE`, RESET; unqualified names
    looked up along it and created in its default schema; `current_schema(s)`, `current_query()`, SHOW SCHEMAS
  - [x] COPY, casts to a database's types and `PRAGMA table_info('db.t')` run in the database they name
  - [x] ATTACH options as expressions, unknown options and TYPEs refused, `OR REPLACE`, access-mode conflicts,
    a rollback detaches what the transaction attached; one database written per transaction
  - [x] statements that read one database and write another (`CREATE TABLE db2.t AS SELECT ... FROM db1.t`): they
    run in the database they write, and bind the tables of the others by name; unqualified names that database
    lacks are looked up in the default one, as are those of the views of an attached database
  - [x] the catalog in table bindings (`BindingAlias`): `db1.t.c`, `db1.s1.t.c`, `EXCLUDE (db1.s1.t.c)`, the struct
    pack `db1.s1.t`, and `MinimumUniqueAlias` in the errors of ambiguous tables and columns
  - [x] temp tables created in the transaction in a statement routed to another database: the forks of a
    transaction share the temporary schema of its first one, which alone commits it
  - [x] sequences of other databases: `nextval('db.seq')`, unqualified ones of the default database in a statement
    routed to another, and `nextval`/`setval` writing the database of the sequence (`NextValModifiedDatabases`), so
    that a DEFAULT or a statement that advances a sequence of another database than it writes is refused
  - [x] DETACH of the default database: it is gone by name and keeps what is attached; RESET of the search path
    then goes to a database that remains; SHOW DATABASES lists them all (`PragmaShowDatabases`)
  - [x] `RECOVERY_MODE` (`NO_WAL_WRITES` keeps commits out of the log until a checkpoint)
  - [ ] `ENCRYPTION_KEY`, `COMPRESS`, `IO_MODE` and the other ATTACH options
- [x] 11. Files that C++ DuckDB v69 can read and write (`sql/storage_version`: all pass)
- [ ] 12. `VACUUM` (`sql/vacuum`: 1 failing)
- [ ] 13. Crash-recovery fuzzing: kill the process during writes, then reopen and verify

## Phase 7: Optimizer

- [ ] 14. Filter pushdown and pullup (`optimizer/pushdown`: 19 failing, 55.8% pass)
  - [x] OR range merge, monotone preimage, filter reordering (`ExpressionHeuristics`), BETWEEN of a column as two comparisons
  - [x] a filter that can throw stays above the projection of an inlined CTE: `position`/`instr`/`strpos` and a
    string `a[i]` with a constant index cannot throw, as in C++ (`test_no_pushdown_cast_into_cte`)
  - [x] LIKE/ILIKE prefixes as ranges in the scan (`TryPushdownLikeFilter`), dense IN ranges and optional OR filters
    in Parquet scans too; `now()` and its kin are `CONSISTENT_WITHIN_QUERY`, not volatile, so they move into scans;
    statistics compare strings by the 12-byte bounds C++ keeps; a comparison of both sides of a join is kept unless
    it is always true (`JoinCondition`); `nextval('s')` shown as C++ shows it
  - [x] `ScalarFnPushdown`: `strlen`/`octet_length` of a Parquet column, when it is the only use of the column, is
    read by the scan as the byte lengths of the values (one file, no filter on the column, no
    `DELTA_LENGTH_BYTE_ARRAY`); a projection that only forwards the columns a Parquet scan reads, directly or
    through a LIMIT, is omitted, as the scan outputs just those (`projection_ids`)
  - [x] filters into table scans before statistics, as C++ does, so the scan keeps the filter as it was and
    statistics rewrite only the one above; statistics check the scan's table filters, optional ones too
    (`PropagateStatistics(LogicalGet)`); `constant_or_null` folds only of columns and constants
    (`NotNullExpressionAnalyzer`)
  - [x] the same for Parquet scans: their filters go in before statistics, which check them; count/min/max
    from a single file's row groups take the filters in its scan into account
  - [x] EXPLAIN ANALYZE row counts: the plan it renders is the one that ran, under the profiler, with each
    operator's rows and scans as `TABLE_SCAN`; under an OR, what is never true but can be NULL stays
    `constant_or_null(false, ...)` (`table_or_pushdown`, `parquet_or_pushdown`)
  - [ ] the rest of C++'s analyzed plan: the `QUERY` / `EXPLAIN_ANALYZE` nodes, `Total Time`, operator timings
  - [ ] the order of a join's conditions where an inequality comes before IS DISTINCT FROM (`issue_17042`)
  - [ ] filters through the row numbers of EXCEPT ALL / INTERSECT ALL on a collation key
    (`pushdown_window_partition_expression`); dynamic filters of blockwise NL joins (`join_filter_pushdown_any_join`)
  - [ ] the operator log and profiler output a few tests read (`join_filter_pushdown_integer_chain`,
    `cte_domain_join_filter`); `read_csv`'s `schema`; `COLUMNS(json_keys(getvariable(...)))` (`issue_16671`);
    a filter on the right of a SEMI join through
    a subquery's projection
  - [x] `can_throw` as C++ has it: only the functions C++ marks `SetFallible` can fail, by overload where C++ decides
    it at bind time (extracts, substring, unary `+`/`-`, `list_value`, ICU date parts of TIMESTAMPTZ)
  - [x] `CanThrow` of aggregates and window functions: `sum`/`avg` of HUGEINT and wide DECIMAL, `string_agg`, the
    `n` variants of min/max/arg_min/arg_max, `ntile`, ... (common subplans, window pushdown)
- [ ] 15. Statistics propagation (`optimizer/statistics`: 4 failing, 91.8% pass)
  - [x] row groups of the database's `ROW_GROUP_SIZE`; zone maps through CASE, COALESCE, greatest/least,
    arithmetic with constants, month/quarter, prefix, left, sign, two-column comparisons
  - [x] statistics of least/greatest, of sum (rows times the bounds), min and max; `constant_or_null` keeps both
    sides as C++
  - [x] `__internal_barrier` and its pushdown rules; statistics of abs, `//` and `%`;
    `error_on_division_by_zero` and `ieee_floating_point_ops` for division and modulo
  - [x] `ieee_floating_point_ops` for sqrt, log, pow and the trigonometric functions; `summary()`;
    `make_timestamp_ns`
  - [x] zone maps through the functions C++ declares monotone (`ArgProperties`: atan, sqrt, ln, round,
    date_trunc, date_diff, epoch_*, normalized_interval, ...), power of what is never negative, dayofyear and day
    within a year or month, casts between timestamp units, bare BOOLEAN columns, `NOT`, `(cmp) = false` /
    `IS FALSE`, `IS DISTINCT FROM`, `IS NOT DISTINCT FROM NULL`; a filter that can fail goes into the scan when it
    is the only one (`PushdownGet`)
  - [x] zone maps of filters on several columns of a table (`a != b`, `x < 1 OR y > 5`): a copy in the scan
    that only skips row groups (`PushMultiColumnFilter`); NULLs of a row group in a file from its validity
    statistics
  - [x] VARCHAR bounds of tables in a file: string statistics read (legacy 8-byte and current 12-byte forms) and
    written as `StatsWriter<string_t>` and `StringStats::Serialize` do, with unicode, lengths and totals
  - [ ] string comparisons that C++'s prefixes can't decide but exact bounds do (`a = b` prunes more here), an
    OR of columns proven false is an empty result here where C++ scans no row groups, INTERVAL and TIMESTAMPTZ_NS
    bounds, week, division by a column
  - [x] the scalar aggregate merger (`scalar_aggregate_merge_threshold`), over tables, `read_parquet`, `read_csv`
    and `read_json`; predicate factoring of ORs in filters
  - [ ] secure views (7 files), `json_serialize_plan`, gamma's errors without IEEE
  - [x] string statistics in `stats()`: bounds, unicode, lengths and totals of tables and constants; `approx_unique`
  - [x] string statistics through functions (contains, suffix, upper/lower, length)
  - [x] list lengths through `length` / `array_length` (a `constant_or_null` where all lists are as long),
    `list_value`, `list_concat`, `flatten` and ARRAY to LIST casts
  - [x] struct/list/map extracts: MAP columns have statistics (`stats(m)`, their keys' and values'), which
    `m[k]` / `map_extract_value` and `map_values` carry; `struct_concat`, `struct_insert` and `struct_update` keep
    their fields'; `list_position` of a value no element can be is NULL (`ListPositionPropagateStats`); Parquet row
    groups skipped by the elements of a LIST and the values of a MAP (`list_contains`, `m[k]`), and by the bloom
    filter of those leaves (`TryGetNestedBloomFilterLeaf`)
  - [ ] EXPLAIN ANALYZE metrics of other operators (EXPLAIN-shape files under `test/optimizer`, `sql/optimizer`)
- [ ] 16. Join order and cardinality estimation (`optimizer/joins`: 1 failing)
  - [x] LEFT join equality groups, `debug_force_no_cross_product`, `approximate_join_order_threshold`
  - [ ] LEFT joins reordered inside the join graph (C++'s CD-C conflict detector, `left_join_cardinality_estimation`)
- [ ] 17. Column lifetime, unused columns, TopN, expression rewriter (small dirs under `test/optimizer`)
  - [x] `CastCanThrow` of nested, DECIMAL and timestamp casts; `x * 0` and `x // 0` (`ArithmeticSimplificationRule`);
    `prefix` decided by statistics only in a one-column filter on a scan; empty statistics fold no `constant_or_null`
  - [x] count/min/max from row group statistics (`TryExecuteAggregates`), of tables and single Parquet files, with
    the row groups a scan filter is never or always true for; counts not exact with uncommitted changes or deletes
    the file had at open, bounds not with any delete (partial precomputation is off at the baseline too)
  - [x] windows and aggregates whose outputs nothing reads are removed, volatile projections too
    (`ClearUnusedExpressions`); a LIMIT above fewer rows than it lets through (`PropagateStatistics(LogicalLimit)`,
    with the row bounds of `range` and Parquet row groups); `ArrayGenericFoldStats`; large INs of an inner
    join's predicate become MARK joins; `EXPLAIN (FORMAT yaml)`; scan filters of a column as one conjunction
  - [ ] struct fields pushed into a scan through a view's projection (`unused_columns/deliver_child_columns`)
  - [ ] the ORDER BY of DISTINCT ON inside the DISTINCT, so its keys can be dropped (`remove_redundant_order_keys`)
  - [x] the values aggregates keep for their rows count toward `memory_limit` with the bytes of their strings,
    BLOBs and BITs (`bitagg_memory_limit`)
  - [ ] EXPLAIN shapes: join-dependent filters, projection placement, filter pullup join order, recursive CTE join
    order, zone maps of ORs over joins, grouping sets of the common aggregate
  - [ ] `duckdb_extensions()`, named table function arguments (`materialized_cte_column_count`)
- [ ] 18. Optimizer-dependent SQL tests (`sql/optimizer`: 62 failing, 63.7% pass)
  - [x] uncorrelated scalar subqueries planned as cross products (`PlanUncorrelatedSubquery`), with first() and
    the multiple-rows check where they may not return one row
  - [x] window self-join, window join pushdown (with `GetUniqueColumnCombinations`), filter pushdown through
    windows on their partitions; `EMPTY_RESULT_PULLUP` runs where C++ runs it
  - [x] late materialization of Top-N, LIMIT and SAMPLE over tables (a SEMI join on the rowid; the second
    scan's filters are `optional:`)
  - [x] late materialization over `read_parquet`, on `file_index` and `file_row_number`
  - [x] the second parquet scan's filters as filters that only skip row groups (`CreateLHSGet`); a Parquet
    scan applies its optional filters only to skip row groups, as table scans do
  - [x] TopN window elimination: `row_number() <= k` per partition as a grouped `arg_min`/`arg_max` of `k`,
    unnested back into rows
  - [x] its late-materialization variant: a payload of several columns of one table fetched by rowid, through
    projections, filters and joins on equal columns
  - [ ] projections between the filter and the window that compute something
  - [x] self-join aggregate (TPC-DS q95's `ws_wh` as `GROUP BY keys HAVING min(x) <> max(x)`); volatile
    uncorrelated IN subqueries are MARK joins too
  - [ ] `duckdb_types()` and the other system functions as `LogicalGet`s in EXPLAIN (`self_join_aggregate`)
  - [x] compressed materialization, stages 1-3: integral and VARCHAR columns of ORDER BY, DISTINCT, aggregate
    groups and the build side and single key pair of comparison joins (casts, `__internal_compress_integral_*`,
    `__internal_compress_string_*`); calling them is the binder error C++ gives
  - [x] compressed materialization of POINT-XY GEOMETRY columns (`__internal_compress_geometry_point`)
  - [x] compressed materialization of VARIANT columns fully shredded on a primitive type: cast to it and
    compressed further, also as grouped by (`variant_comparator`); joins of them compare the integers
  - [x] `stats()` of a VARCHAR read from several parquet files with `union_by_name`: the statistics and row
    counts of several files only when the bind opens them all (`union_by_name`, `maximum_files_for_stats`), empty
    files skipped; with a `schema`, of a column it names that the file stores as the same type
  - [x] partitioned execution: a large aggregate, ORDER BY or window over a table as a UNION ALL of parts by
    ranges of a column, cut where the row groups' zones overlap little
  - [x] list comprehension rewrite: a list comprehension whose result reads no field of the filter's struct
    becomes `list_transform(list_filter(l, f), r)`
  - [x] enum IN simplification (`InEnumSimplificationRule`, `EnumCompareSimplificationRule`), of the IN lists
    the binder spells out as comparisons too; `constant_or_null` is not folded by statistics in a statement with
    side effects, side-effecting CTEs included
  - [x] IN lists kept as IN through filter pushdown (dense ranges, optional IN and OR filters on scans, index
    scans and zone maps of them); the auxiliary `e(b) op e(c)` filters of window pushdown
  - [x] uncorrelated EXISTS as a cross product with `count_star() = 1` of its `LIMIT 1`
- [ ] 19. TPC-H 22/22 and TPC-DS 99/99 correct
  - [x] with `tools/tpch` and `tools/tpcds` building dbgen and dsdgen from the C++ checkout, the examples get
    TPC-H 22/22 at SF 0.01 and 0.1, TPC-DS 99/99 at SF 0.01
  - [ ] `dbgen` / `dsdgen` as table functions in duckrs, so the corpus's tpch and tpcds tests run
- [x] 20. Verification mode: run each query with the optimizer on and off and compare the results
  (`sqllogictest --diff-against unoptimized --diff-out <file>`); over `optimizer`, `join`, `subquery` and
  `aggregate` it found `complex_top_n` (105 rows against 102) and `test_no_pushdown_cast_into_cte`, both fixed

## Phase 6: SQL surface

- [ ] 21. `MATCH_RECOGNIZE` (`sql/match_recognize`: 24 of 29 pass)
  - [x] its own operator (`execution/match_recognize.rs`): C++'s pattern program and memoised matcher, PARTITION BY /
    ORDER BY, MEASURES with RUNNING / FINAL, navigation, CLASSIFIER(), MATCH_NUMBER(), aggregates over the match
    (DISTINCT, FILTER, ORDER BY, through macros), ONE ROW / ALL ROWS (WITH UNMATCHED ROWS, OMIT EMPTY MATCHES),
    AFTER MATCH SKIP, SUBSET, exclusion, anchors, PERMUTE, DEFINE AUTO; the binder's errors; the memo counts
    against `memory_limit`
  - [ ] `max_execution_time` (not in duckrs at all; `limits` runs into the timeout), window functions in
    `duckdb_functions()` (`clauses`, `scope`), UNNEST in a measure, `->` resolved as a JSON operator in a measure
- [ ] 22. Triggers (`sql/trigger`: 26 of 28 pass)
  - [x] CREATE / DROP TRIGGER with the checks of `BindCreateTriggerInfo`, `duckdb_triggers()`, BEFORE and
    AFTER statement triggers, AFTER row triggers with NEW / OLD, REFERENCING transition tables,
    UPDATE OF, COPY FROM, RETURNING, dependencies (DROP, ALTER, CASCADE), the WAL and checkpoints
  - [ ] the write-write conflict of a concurrent rename; a trigger body in a data-modifying CTE
- [ ] 23. CTEs: recursive, materialized, `USING KEY` (`sql/cte`: 29 failing, 9 erroring)
  - [x] data-modifying CTEs (INSERT, UPDATE, DELETE, MERGE, COPY): each runs to completion before the query, in its
    transaction, whether the query reads it or not; RETURNING rows as a table; WITH before INSERT/UPDATE/DELETE;
    prepared; EXPLAIN as C++'s `PIPELINE_DEPENDENT` CTE; refused below the top level and in views
  - [ ] the EXPLAIN shapes of materialized CTEs, and their runtime filters
- [ ] 24. Subquery flattening edge cases (`sql/subquery`: 25 failing, 3 erroring)
- [ ] 25. Joins: ASOF, positional, lateral (`sql/join`: 46 failing)
- [ ] 26. Set operations, `SAMPLE`, `RETURNING`, `WITH ORDINALITY`, `UNNEST` (`setops` 8, `sample` 7, `returning` 2, `ordinality` 0, `unnest` 2)
- [ ] 27. SQL variables: `SET VARIABLE`, `getvariable` (`sql/variables`: 3 failing)
- [ ] 28. `EXPLAIN` / `EXPLAIN ANALYZE` and profiling (`explain` 7, `profiling` 3, `detailed_profiler` 1)
- [ ] 29. Query progress bar (`sql/progress`: 1 failing)
- [ ] 29a. The parser and errors (`sql/parser`: 23 failing, 66.7%; `sql/error`: 12 failing, 47.8%)
  - [x] `has_error` as the HAS_ERROR operator (a macro of its name shadows it, exported as `system.main.has_error`);
    TRY and HAS_ERROR refuse volatile arguments, aggregates and windows; ORDER BY `*` and `COLUMNS(..)`, of a set
    operation too; one `_` between digits in DOUBLE and FLOAT casts (not when sniffing a CSV), and in the test
    runner's typed comparison; `invoke`'s errors and `alias()` of its parameters; `cast_to_type` keeping the target
    collation; a call folded to NULL keeping its arguments' collation
  - [ ] error locations (carets), `errors_as_json`, AT clauses, `preserve_identifier_case`

## Phase 5: Functions

- [ ] 30. Scalar and table functions (`sql/function`: 51 failing, 87.7%; `sql/table_function`: 31 failing)
  - [x] `remap_struct`, `struct_values`, `parse_formatted_bytes`, `equi_width_bins`, `is_histogram_other_bin`;
    `date_part` of a list of parts, the time zone parts of TIMESTAMP and TIME, `date_trunc` of an INTERVAL by
    ISO year, week and day parts; `list_where` past the end of the list; `map_concat`'s type from its non-empty
    maps; the distance functions' overloads, messages, `<->` and `<=>`, and the rescaled cosine
  - [x] `get_type` and `make_type`, whose TYPE values keep the type serialized
  - [x] `variant_exists`, `variant_extract_string`, `variant_array_length` at a path; `binom`, `replace_type`,
    `invoke`; `timezone(INTERVAL, TIMETZ)`; fmod floored and refusing a zero divisor as C++ does; the decade
    truncated; `last_day` of the last month; a `date_part` of an infinity is NULL whatever the part
  - [x] window functions in `duckdb_functions()`; GROUP BY ALL refuses a window; a string slice with a step is
    refused when bound, and slice bounds are cast to BIGINT
  - [x] `duckdb_keywords()` from the grammar's keyword lists, `duckdb_dialects()` (none) and `current_dialect`'s
    check, `pragma_version()`, `pragma_platform()`; `standard_vector_size` in `duckdb_settings()`; every catalog's
    `main` schema internal; `pragma_table_info` names parsed as `QualifiedName::TryParse` does
  - [x] the Postgres setof aliasing of `range`, `generate_series` and `unnest` (the one column takes the table
    alias, the original name stays bindable and joins USING); `WITH ORDINALITY`, numbered per input row when
    lateral; a NULL bound of a timestamp range; infinite bounds refused
  - [x] C++'s errors for: struct_insert/struct_update (no arguments, not a STRUCT, nothing to add, duplicate
    named arguments), struct_concat (positions, duplicates, named and unnamed mixed; unnamed ones make a
    TUPLE), MAP and struct_extract of the wrong types (no overload matches), cot of zero, url_decode, `__between`,
    a SET value with a column or a subquery, a DEFAULT in WHERE/GROUP BY, DISTINCT ON a literal, ORDER BY an UNNEST
    of a struct, QUALIFY of an unknown column, DROP of a built-in type or system view, make_type's name parsed
    as a qualified name; INTERVAL / 0; a timestamp out of range after adding an interval; acosh below 1 is NaN;
    struct functions with prepared parameters bind when executed
  - [x] `regexp_extract`'s group as C++ binds it (a constant 0–9, its options in its place, a NULL or missing
    group is no match) and `regexp_extract_all`'s (negative is nothing, an unmatched group is NULL);
    `parse_dirname`/`parse_dirpath` as `TrimPathFunction` and `ParseDirpathFunction`, unknown and NULL separators
    as both slashes, `parse_filename`'s trim argument of any type; `reservoir_quantile` with its sample size and
    default quantile; `struct_extract_at` of named structs; gamma and lgamma of zero, and their IEEE variants;
    the difference of timestamps across the whole range; GEOMETRY in VARIANTs; extra column aliases of a table
    function are ignored
  - [x] VARIANTs nested tens of thousands deep: decoded, encoded and dropped without recursion, a child moved
    out rather than cloned, and a deep JSON document cast on a thread with the stack it needs; `overlay`,
    `md5_number`, `make_timestamp_ms`, `char_length` and `character_length`, `unlist`; `to_timestamp`'s range as
    `EpochSecOperator` checks it; the time parts of TIME_NS (`nanosecond` exactly); `SELECT * AS x` names every
    column x; `struct_keys(NULL)`
  - [x] `strptime` and `try_strptime` as `StrpTimeFormat` parses (`function/strptime.rs`): week numbers (`%U`, `%W`),
    ISO years, weeks and weekdays (`%G`, `%V`, `%u`), `infinity`/`epoch` specials, TIMESTAMP_NS and TIMESTAMPTZ
    results for `%n` and `%z`, C++'s error text; `strftime` of TIMESTAMP_NS; TIMESTAMP_NS ± INTERVAL keeping its
    nanoseconds, and TIMESTAMP_S/MS ± INTERVAL as TIMESTAMP; `time_bucket` as the `TimeBucket` operators (month
    offsets, overflow checks); `epoch_ms` rounding and the epoch functions' range errors; `round` of every integer
    type; `create_sort_key` (BIGINT or BLOB, intervals as stored); `variant_type`, `variant_contains`,
    `variant_normalize`, `variant_keys` as a path function; `normalized_interval` as `interval_t::Normalize`;
    `ilike_escape`'s escape lowercased; `map_concat` of NULLs; `list_resize` to a NULL size; a NULL `string_agg`
    separator in `list_aggr`; `atan(inf)`
  - [x] `decimal_division` and `decimal_average`; `variant_group_object` and `variant_comparator`; a macro used as
    a window function (`variant_group_array() OVER`, any scalar macro of one aggregate); UNION comparisons as
    `UnionComparator` (a NULL member is NULL; join conditions still match whole values), UNION to UNION TRY_CAST
    losing the row when a member fails, STRUCT to UNION by case-insensitive names, a prepared `union_extract` key;
    `arg_min_nulls_last` / `arg_max_nulls_last` with n; `avg` of TIMESTAMP_NS and of TIMESTAMP_S/MS/DATE as
    TIMESTAMP; the list `range` with a step of 0; EXPORT DATABASE of UNION columns past the tag
  - [x] map subscripts and `list_has_any`/`list_has_all` deducing their template types (and failing as C++ does);
    `map_keys`/`map_values`/`map_entries`/`map_extract` of an untyped NULL typed as C++ folds them; BLOB
    `array_slice`; `array_to_string` of NULL, with a NULL separator, refusing a column separator; `bit_length` of a
    string; `make_date` of a STRUCT; `make_time` taking a leap second; `make_timestamp` and `make_timestamp_ns`
    refusing an infinity; `date_trunc` before the first timestamp; `epoch_ms/us/ns` of TIME_NS and TIMETZ; the date
    parts of a date taking no TIME, and `date_part`'s unit checked against its type when bound; `list_resize` past
    the largest vector; `list_reduce` typed as `ListReduceBind` (the accumulator from the initial value, widened to
    the lambda's type); the lambda functions' arity and NULL-argument errors, UNNEST refused in a lambda;
    `enum_code` and friends of a prepared parameter; `variant_extract` of a folded key and its overloads' types;
    `regexp_extract`'s ambiguous string group; RE2 octal escapes; OR filters short-circuiting as `SelectOr`
  - [x] `switch` as `SwitchBindExpression` (a constant MAP of cases made a CASE, its key template deduced);
    comparison operators called as functions (`"="(a, b)`); `struct_extract` refusing a VARIANT while `.key`
    of one is its variant_extract; `list_sort`/`list_grade_up` refusing a VARIANT; `list_concat` of NULLs;
    a folded tuple index; UNNEST of an empty struct refused; `fill` of HUGEINT as `InterpolateOperator`
  - [ ] `stats()` of nested types and VARIANT, Parquet UNION columns, state export of `variant_group_object`;
    `list_max`/`list_min` under a collation, regex `\C`, lambdas in a subquery that see a column of their
    parameter's name
  - [ ] TYPE columns and casts, `index_key`, JSON date-format detection, `list_contains` and the other templated list functions' type deduction,
    the external-resource and profiling table functions; `duckdb_constraints()` lists a primary key's implied
    NOT NULL after the declared constraints (the catalog keeps NOT NULL per column, not in order); a view's
    columns as bound when it was created
- [ ] 31. Aggregates (`sql/aggregate`: 34 failing, 84.3%; target ≥ 90%)
  - [x] `histogram(x, bins)` and `histogram_exact`, with the "other values" bucket; stddev and variance are
    infinite rather than out of range; the moments, covariances, regressions and Kahan sums computed with C++'s
    update and finalize, so their results are C++'s to the bit
  - [x] `approx_count_distinct` with C++'s HyperLogLog (`HyperLogLogP<10>` and Ertl's estimator), its exported
    sketch and the checks on importing one
  - [x] `approx_quantile` with C++'s t-digest (compression 100, `function/tdigest.rs`): the overloads' types, the
    values encoded as doubles and decoded with clamping as `ApproxQuantileCoding` does, FLOAT fractions, its exported
    state of centroids and the checks on importing one; `string_agg` casts its input to VARCHAR
  - [x] `count(t.*)` as `TryRewriteQualifiedCountStar` in every clause and in windows and macros, with the
    relations outer and positional joins NULL-extend tracked, `AnyColumnNotNull` where there is no row id;
    `median` of non-interpolatable types discrete; quantile interpolation of HUGEINTs and of DECIMALs stored as
    integers as C++ does it; `quantile_cont`'s overloads (no INTERVAL or VARCHAR, no single argument);
    list_aggr's "No matching aggregate function"; `list_contains` deducing its template type
  - [ ] the memory limits of quantiles and of window builds (7 files)
  - [x] `EXPORT_STATE` in C++'s layouts for the aggregates duckrs computes from their rows
    (`function/holistic_states.rs`): bool_and/or, product, string_agg, the stddev/variance family, covar, corr,
    skewness, kurtosis, fsum/favg, the regr_* family, arg_min/arg_max, the TIMESTAMP/TIME/TIMETZ/INTERVAL averages
    and the quantiles; constants the arguments were bound to (separator, fraction) kept in the state type and
    checked on COMBINE; `to_aggregate_state` with constant parameters and TYPE signatures; states in database
    files keep their `extension_info` and are bound again when read; aggregates C++ cannot export refuse to
  - [x] ordered aggregate states (`list(x ORDER BY y) EXPORT_STATE`): the rows buffered with their keys, the ORDER
    BY in the state type and its `order_bys` in files, sorted and aggregated when finalized, buffers appended by
    COMBINE; `to_aggregate_state` with an ORDER BY spec and its checks
  - [ ] the sketches of approx_count_distinct, approx_quantile and approx_top_k, `lttb`, the states of files before
    v1.5 (`LEGACY_AGGREGATE_STATE`)
- [ ] 32. Window functions (`sql/window`: 14 failing, 84.6%; target ≥ 85%)
  - [x] `fill` as `WindowFillExecutor`: only finite values count and the search stops at an order value that
    does not, interpolated integers clamped to their endpoints, values a type cannot hold (past a DECIMAL's
    width, an integer's range, a coarse timestamp's) NULL; FILTER refused on window functions
- [ ] 33. `pragma_*` functions and PRAGMA statements (`sql/pragma`: 20 failing, 65.5%)
  - [x] SHOW TABLES over the search path, SHOW TABLES FROM a database, a schema or both
  - [x] PRAGMA version, platform, functions, database_size, storage_info and metadata_info as the queries C++
    rewrites them to; a pragma no function has is a Catalog Error, and one called with the wrong arguments a
    Binder Error

## Phase 4: Types and casts

- [ ] 34a. GEOMETRY (`sql/types/geo`: 30 of 42 pass)
  - [x] WKB as stored, WKT in and out (with EWKT's SRID), BLOB and VARCHAR casts, `ST_GeomFromWKB`, `ST_AsWKB`,
    `ST_AsText`, `ST_Intersects_Extent` / `&&`, `vertex_extract` and the `.x`/`.y`/`.z`/`.m` syntax
  - [x] CRS type parameters (AUTH:CODE, PROJJSON, WKT2:2019, the OGC:CRS84/CRS83 defaults), `ST_CRS`, `ST_SetCRS`
  - [ ] `CREATE COORDINATE SYSTEM`, `duckdb_coordinate_systems()`, `ignore_unknown_crs`
  - [x] GEOMETRY columns in database files (v1.5.0 and later, as WKB) and the WAL
  - [ ] shredded GEOMETRY columns
  - [x] `GeometryStats`: in `stats()`, in database files (extent, types, flags), and `vertex_extract`'s bounds
  - [ ] `ST_GeomFromWKB`'s types from BLOB min/max (duckrs keeps no string statistics), GeoParquet, the spatial extension's blob format
- [ ] 34. Remaining type and cast failures (`sql/types`: 39 failing, 92.6%; `sql/cast`: 2 failing)
  - [x] UNNEST with `recursive`, `max_depth` and `keep_parent_names`, of structs at the root of a select list,
    refused nested; a cast to NULL of anything that is NULL
  - [x] DOUBLE to VARCHAR of powers of two from 2^53 (fmt's fallback shifts its denominator by the boundary bits);
    VARCHAR to MAP with NULL keys; VARIANT casts as `CastVariant` (UNION targets refused, a nested value of
    another shape cast from the value it holds, NULL map entries, JSON leaves parsed, the depth limit, FLOAT and
    DECIMAL leaves in JSON); `FLOAT(precision)`; type parameters described as C++ does (DECIMAL and BOOLEAN
    literals, NULL refused by name, VARCHAR with two, a user type with any); built-in types qualified by `main`, a
    missing schema of a type as a missing type
- [ ] 35. The VARIANT type (`sql/variant`: 2 failing)

## Phase 12: Catalog, settings, APIs and shell

- [ ] 36. Catalog failures (`sql/catalog`: 73 failing)
- [ ] 37. Settings, generated in full from `settings.json` (`sql/settings`: 35 failing, 2 erroring)
- [ ] 38. Secrets manager: `CREATE SECRET` and related statements (`sql/secrets`: 16 failing, 0%)
- [ ] 39. SQL export and DDL rendering (`sql/sql_export`: 36 of 101 pass)
  - [x] `EXPLAIN (SQL)` of SELECT, INSERT (with ON CONFLICT), UPDATE, DELETE, MERGE INTO, RETURNING,
    CREATE TABLE AS and COPY TO from the optimized plan; the `explain_sql` and `explain_sql_strict`
    statement verification
  - [x] scans, filters, projections, aggregates and grouping sets, windows (RANGE offsets too),
    ORDER BY / LIMIT / TopN, DISTINCT (ON), USING SAMPLE, UNNEST, set operations, recursive CTEs,
    joins (inner, outer, SEMI/ANTI, ASOF, positional, lateral), MARK joins with exact NULL results,
    decorrelated joins as correlated subqueries, subqueries, lambdas, `read_parquet` scans, USING KEY
    and `recurring` in recursive CTEs
  - [ ] WHEN NOT MATCHED BY SOURCE and CSV/JSON scans stay unsupported, as in C++; data-modifying CTEs
  - [ ] the text of C++ where tests match it (`AS r(column0, ...)` table aliases, its formatter,
    result-type casts); the `*_plan` tests compare C++ plans
- [ ] 40. Logging and error messages (`sql/logging` 8, `sql/error` 13, `test/logging` 2)
- [ ] 40a. The query profiler (`sql/pragma/profiling`: 21 of 36 pass)
  - [x] `enable_profiling = 'json'`, `profiling_output`, `tracked_metrics` (globs), `profiling_coverage`,
    `legacy_metrics_format`; the grouped JSON of `ToResultTree` with the operator tree
  - [x] measured: query time, CPU time, intermediate rows and size bytes, rows scanned, planner, optimizer
    (per pass) and physical planner times, I/O bytes and operations (WAL writes left out, as in C++),
    checkpoint, attach, WAL write, commit and replay latencies, WAL replay entry count
  - [x] `wal_autocheckpoint_entries`; `disable_checkpoint_on_shutdown` also covers attached databases
  - [ ] exact `query.total_bytes_scanned`, row groups of local storage, memory metrics, secure views
  - [ ] the `query_tree` text output, `duckdb_available_metrics()`, `enable_profiling()` / `disable_profiling()`
    as table functions, the profiler log entries
- [ ] 41. **C API** (`duckrs-capi`): implement `duckdb.h`, then pass `test/api/capi/*`. There is no code for this yet.
- [ ] 42. **CLI shell** (`crates/duckrs-cli`, the `duckrs` binary). Against `tools/shell/tests` it passes
  150 of `test_shell_basics` (C++ 1.5.6: 141), all of `test_dump`, `test_errors`, `test_import`,
  `test_table_metadata_rendering` and `test_shell_rendering`
  - [x] batch and interactive input (rustyline, history in `~/.duckdb_history`, highlighting as you type),
    the command-line options, `-c`/`-s`/`-cmd`/`-f`/`-init`/`~/.duckdbrc`, bail, echo, safe mode
  - [x] every output mode: duckbox (`box_renderer.rs`, a port of `box_renderer.cpp`), box, column, table,
    markdown, latex, line, list, tabs, csv, html, tcl, ascii, quote, json, jsonlines, insert, trash
  - [x] the dot commands but those listed below; `.tables`, `.databases` and DESCRIBE draw table
    metadata (`shell_render_table_metadata.cpp`); error and SQL highlighting (`shell_highlight.cpp`)
  - [x] `_` as the last result (through the new `Connection::add_replacement_scan`); opening or attaching a
    Parquet, CSV or JSON file (`OpenFileStorageExtension`); COPY to and from stdin, stdout and stderr
  - [ ] EXPLAIN and `query_tree` profiling need the engine's text tree renderer (`text_tree_renderer.cpp`),
    which prints `RANGE` boxes where the engine prints an indented plan
  - [ ] `getenv` and `shell_history` (functions the shell extension registers; needs item 49), `-serve` /
    `-connect` (the quack protocol), agent mode, `.manual`, `.edit`, `.excel`, `.log`, the pager,
    `.progress_bar`, `.prompt`, autocomplete, highlighting of duckbox results, the `.help` layout,
    `pragma_version` / `version()` (the shell prints the crate version)
  - [ ] engine gaps the tests reach: `SET TimeZone` (ICU, item 50), `integer_division`,
    `preserve_identifier_case`, `lock_configuration`, CSV cardinality estimates for pipes
- [ ] 43. **Rust API** (`crates/duckrs`)
  - [x] `Appender` (rows into a table, flushed as an INSERT: constraints, triggers and the WAL as for any INSERT)
  - [x] Arrow `RecordBatch` export with C++'s default types (`Connection::query_arrow`, `record_batches`)
  - [x] `Relation`: table, values, filter, project, aggregate, order, limit, distinct, joins, set operations, as SQL
  - [x] results a chunk at a time (`Connection::stream` / `fetch`), from the materialized result
  - [ ] results streamed while the query runs (the executor materializes them); appending Arrow batches;
    `Relation` from Rust closures and its own query plans rather than SQL text
- [ ] 44. Connection tests (`sql/connect`: 2 failing, 5 skipped)

## Phase 13: Performance (not started)

- [x] 45. Benchmarks: `tools/benchmark` (`benchmark_runner`) ports DuckDB's runner and reads its
  `.benchmark` files (templates, includes, arguments, caches, `result`/`result_query`/`assert`); the
  Criterion benches `duckrs-common/benches/executor.rs` (vector kernels) and `duckrs/benches/kernels.rs`
  (scan, filter, aggregates, join, sort, top-N, window, casts on a million rows)
- [ ] 46. Nightly runs against C++ DuckDB: `.github/workflows/benchmarks.yml` times TPC-H SF1, TPC-DS SF1 and
  micro benchmarks on both engines (`--engine=both`, the `reference` feature); `CALL dbgen` / `CALL dsdgen`
  are carried out with the generators of the checkout (`tools/tpch`, `tools/tpcds`)
  - [ ] ClickBench (its data is downloaded) and storing the nightly numbers over time
- [ ] 47. Faster kernels: monomorphized code, SIMD, a perfect hash aggregate, the salt/pointer hash table design
  - [x] threads for tables in their file (a row group per morsel) and for both inputs of delim joins
  - [x] reading the file: one-load bit unpacking, contiguous vector copies, shared strings in scan buffers,
    one string buffer per segment scan, `Vector::set_strs`
  - [x] dictionary vectors for DICT_FSST string columns, comparisons and group lookups on their entries
  - [x] a partitioned, parallel aggregate finalize for many groups; a Top-N boundary
  - [x] vectorized decimal scale-ups, date parts, CASE results; LIKE with substring search
  - [x] join partitions as one hash table of distinct keys (one hash per key) with their rows in one array
  - [x] filters applied while a row group is read from its file, conjunct by conjunct, reading the other
    columns only for the rows that pass (`TableSource::fetch_rows`)
  - [ ] the rest of the hash join probe (C++'s salted pointers, prefetching), group keys without a byte
    key per row, aggregate states not behind an enum per row, selective reads of dictionary strings and of
    DELTA_FOR / RLE / ALP segments, morsels smaller than a row group
- [ ] 48. v1 gate: TPC-H SF10 geometric mean ≤ 1.5× C++ DuckDB, and no query slower than 3×. The runner
  prints the gate after `--engine=both`. On TPC-H SF1 (8 threads, all 22 answers correct) duckrs is at
  1.53× (none above 3×: Q21 2.7×; Q02 0.5×, Q08 0.95×), from 12.7× at the start of item 47.
  SF10, both engines on the file C++ wrote: 0.65× on 2026-11-22 after part 65, all 22 answers correct and all 22
  faster than C++ (Q13 and Q21 0.94×, Q16 and Q18 0.93×, Q10 0.91×; Q22 0.66×, Q06 0.31×, Q15 0.22×), with the load
  average 19 at the start and 27 at the end. Before that, 0.64× after part 63 (Q10 0.96×, Q09, Q13, Q18 and Q21
  0.93×) with the load average 4 at the start and 14 at the end, the quietest run yet; 0.63× after part 60 (Q22 1.44×, Q16 1.07×,
  Q10 1.06×) with the load average rising from 10 to 32, which slowed C++'s Q01 to 0.85s from its usual 0.5s;
  0.68× after part 52, the GROUP BY rewrite done (none above 3×: Q01 1.49×, Q09 1.10×; Q10 0.97×, Q18 0.73×,
  Q15 0.17×), with the machine's load average rising from 18 to 58 during the run. Earlier that day: 0.71× after part
  49 (Q18 1.14×) under a load of 20 to 53, which slowed C++ well past its usual times. On 2026-11-20: 0.67× after part 44 (Q10 1.31×) under a load rising from 45 to 157,
  which says least of all, 0.89× after part 42 (Q10 1.97×)
  with the load falling from 43 to 14, 1.09× after parts 39 and 40, 1.20×, and before that 1.27× and 1.37×
  under heavier load, with Q18 and Q14 above 3× in them. The gate passes; measure once more on a quiet machine
  before ticking this. Q10 then took 18% less: a GROUP BY writes the keys of a
  chunk's rows into one buffer and makes a key only for a new group, and threads read blocks of the file at once
  (`pread`), where they took turns. The distinct values of each column, which the join order starts from, are
  kept in the file as C++ keeps them (`DistinctStatistics`, as `HLL_V2`, and read of older files as `HLL_V1`):
  the first query of a file at SF10 took 16–27s counting them from all the rows, and takes under a second. Q06
  then took 0.35s warm against C++'s 0.09s: the blocks of a file stay in memory up to `memory_limit` (C++'s
  buffer pool) where 256 MiB of them did, a scan keeps the metadata of the columns it read, and comparisons of
  an integer column with constants are a range of its values; warm Q06 takes 0.06s against C++'s 0.05s. Q10
  then took 3.3s of CPU warm where C++ takes 1.0s: the keys of a GROUP BY are written a column at a time, a join
  filter loops over a flat column, and the strings of the few rows of a group a filtered scan wants are decoded
  each from its entry, where all of the group's were; that takes 20% of the CPU off (2.6s). A join's build side
  keeps the strings of its input where they are rather than copying them under its lock, and a Top-N compares a
  row's key with its boundary in one buffer: warm Q10 takes 0.49s from 0.57s, against C++'s 0.23s, with 2.5s of
  CPU against 1.1s. A plain GROUP BY now finds its groups in a hash table over the row format, by the hash of
  their columns and a comparison with their rows (docs/GROUP_BY_REWRITE.md), and merges threads' tables by
  partition: Q18 1.07s from 1.49s, Q10 0.37s from 0.40s. Q01 took 4.4s of CPU warm where C++ takes 1.2s: a
  sum over few groups adds a chunk into four banks of sums per group, so that an addition doesn't wait for the
  one of the row before, and a binary operation over dictionaries without NULLs loops by the kind of selection
  of each side: 3.5s. A range of a dictionary is a window on its selection rather than a copy of its indices,
  and sums take the banks too where a column has a validity mask without NULLs: 3.4s. Bit-packed values unpack
  32 at a time by code for their width, DICT_FSST indices straight into `u32`s, and a filter's rows of a
  dictionary are a gather: 3.15s. An expression that an aggregate or a projection computes twice is computed once
  below it (`CommonSubExpressionOptimizer`): 2.8s. The loops of arithmetic inline their operation, with overflow
  errors made apart, and `count_star` counts each group's rows apart: 2.7s. A range filter on a flat column keeps
  its rows without a branch, a group's selection is no longer gathered through while it is all the group's rows,
  and fixed-width columns decode into their vectors' memory: 2.5s, and a filtered count over lineitem takes 0.22s
  of CPU from 0.54s. Q22's scan of customer read all its columns, below the column select that hides the marker
  of an IN list's MARK join; the unused columns pass narrows that select and goes on below it, and a hash join
  prefetches the keys of 32 probe rows' candidates before it compares them: Q22 0.58s of CPU from 0.93s, Q03 1.02s
  from 1.18s. Q10's GROUP BY hashes strings of customer rows that the join reads in the order of the orders, so
  hashing prefetches the string entries of 32 rows and then the bytes of those not inlined before it hashes them,
  and the join filter's Bloom check prefetches its blocks: Q10 1.8s of CPU from 2.0s. duckrs
  had no way to load SF10 itself, as an append kept its rows in memory (more than 2 GiB failed); appends now write
  their full row groups to blocks as they fill (an INSERT of 80M rows takes 90MB). duckrs now loads SF10 into its
  own file (COPY of the `.tbl` files: lineitem 24s against C++'s 22s, the rest under 5s each; at most 710MB
  resident, where C++ takes 2.2GB; 2.78GB of file against C++'s 2.79GB), and all 22 queries on that file match
  `answers/sf10`. A CSV file larger than a buffer (32MB) is sniffed on its first buffer, unless a scan of the
  sniffer gets to its end, and read a buffer at a time. It is scanned on threads at once, in parts of 8MB that
  start after a `\n` that ends a row, each read of the file: any `\n` until the first quote or escape, then the
  state machine runs on to the row ends (C++ guesses where a part starts, which a quoted newline can fool, and
  stops when its guess was wrong). An 8M-row file with quoted values loads in 2.4s on 8 threads, from 10.1s on
  one, where C++ takes 3.5s. An INSERT appends the parts in
  order; threads claim the row groups they fill, encode them with no lock held, and place them in the order of
  their rows, so the file is the one a single thread writes. An error is the one a scan of the whole file finds

## Phase 14: Extensions (not started)

- [ ] 49. Extension ABI, so `LOAD` works with `.duckrs_extension` files (`sql/extensions`: 11 failing)
- [ ] 50. ICU: time zones and collations. This unlocks the 22 skipped `sql/timezone` files.
- [ ] 51. httpfs (S3 and HTTP), delta, spatial

## Phase 0: CI and gates

- [ ] 52. Miri and ThreadSanitizer jobs in CI
- [ ] 53. A difftest harness run on every change

## Suggested order

1. Items 1–3, 9–10 and 14–18 cover the most failing files: about 1,000 between them.
2. Items 41–43 are the largest pieces of work that have no code at all.
3. Items 45–51 come after the test suite reaches ≥ 85%.
