# Remaining work

What is still missing from the roadmap in [`RUST_PORT_PLAN.md`](../../RUST_PORT_PLAN.md) §6.
The test numbers come from [`STATUS.md`](STATUS.md) at baseline `d4eea3f208d`.

- Overall: 3797 of 6367 test files pass (**62.7%**). The v1 target is ≥ 90%.
- Phases 0–11 have code but are below their pass targets.
- Phase 12 is partly done. Phases 13 and 14 have not started.
- In git, the branches named "Phase 13"–"Phase 21" are rounds of fixes against the test suite.
  They are not the plan's Phase 13 (Performance) or Phase 14 (Extensions).

Each item gives the number of failing files in `test/` that it should fix, where known.

## Phase 11: File formats and I/O

- [ ] 1. COPY TO/FROM failures (`sql/copy`: 208 failing, 66.8% pass; target ≥ 85%)
  - [x] `PARTITION_BY` for CSV and JSON (Parquet had it), `PARTITION_BY *`, `PARTITIONED_BY` alias
  - [x] the `ORDER BY` option of COPY TO, and its checks against `USE_TMP_FILE` / `PER_THREAD_OUTPUT`
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
  - [ ] many CSV sniffer, rejects-table and error-message differences (~50 files under `sql/copy/csv`)
- [ ] 2. JSON: `read_json`/`read_json_auto`, the JSON type, JSON functions (`sql/json`: 95 failing, 34.9%)
  - [x] `json_normalize`, `json_deep_merge`, `json_merge_patch_diff`
  - [x] `json_each` / `json_tree`, with columns as arguments (LATERAL) and the hidden `json`, `root`, `rowid`
  - [x] escaped quotes and backslashes in quoted JSON path keys; no wildcards in `json_set`/`json_remove`/...
  - [ ] documents nested deeper than 4096 levels: the parser and printer recurse, C++'s yyjson does not
  - [ ] `json_serialize_sql` / `json_deserialize_sql` / `json_execute_serialized_sql` / `json_serialize_plan` (~20 files): needs an AST serializer
  - [ ] GeoJSON (`read_json` geojson, `ST_AsGeoJSON`, `json_geometry_format`): needs the GEOMETRY type
  - [ ] `json_group_object`/`json_group_array` as window functions (they are macros today)
- [ ] 3. Parquet reader/writer: missing types, filter pushdown, delta encoding (`test/parquet`: 28 failing, `corrupt`: 5)
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
  - [ ] encryption, UNION, VARIANT and GEOMETRY columns
  - [ ] rotation inside partitions groups rows into files differently from C++ (`partitioned_file_rotation`)
  - [ ] reader side: row-group pruning by bloom filters and page indexes, `bound_schema` (needs struct field
    projection and `EXPLAIN (SQL)`), reading a directory path, `maximum_files_for_stats`
  - [ ] corrupt files C++ rejects that the crate reads (INT96 out of range, WKB types, page index row counts)
  - [ ] prefetch / read-ahead metrics and EXPLAIN shapes of Parquet scans (~20 files)
  - [ ] `write_timestamp_as_int96`, `parquet_metadata` logical type names (`Float16Type()`), duplicate column names
- [ ] 4. Parquet VARIANT, including shredding (`parquet/variant`: 47 failing, 0%)
- [ ] 5. `EXPORT DATABASE` / `IMPORT DATABASE` (`sql/export`: 27 failing, 0%)
- [ ] 6. `COPY FROM DATABASE` (`sql/copy_database`: 12 failing, 0%)
- [ ] 7. MultiFileReader: hive partitioning, `union_by_name`, filename column (`sql/multi_file`: 4 failing)
- [ ] 8. GeoParquet (`geoparquet`: 7 failing)

## Phase 9: Storage engine

- [ ] 9. Remaining storage failures (`sql/storage`: 216 failing, 64.0%; target ≥ 85%)
- [ ] 10. `ATTACH`/`DETACH` and multi-database features (`sql/attach`: 71 failing, 28.3%)
- [ ] 11. Files that C++ DuckDB v69 can read and write (`sql/storage_version`: 2 failing)
- [ ] 12. `VACUUM` (`sql/vacuum`: 1 failing)
- [ ] 13. Crash-recovery fuzzing: kill the process during writes, then reopen and verify

## Phase 7: Optimizer

- [ ] 14. Filter pushdown and pullup (`optimizer/pushdown`: 29 failing, 32.6%)
- [ ] 15. Statistics propagation (`optimizer/statistics`: 32 failing, 37.3%)
- [ ] 16. Join order and cardinality estimation (`optimizer/joins`: 14 failing)
- [ ] 17. Column lifetime, unused columns, TopN, expression rewriter (small dirs under `test/optimizer`)
- [ ] 18. Optimizer-dependent SQL tests (`sql/optimizer`: 114 failing, 32.7%)
- [ ] 19. TPC-H 22/22 and TPC-DS 99/99 correct. Today the tests are skipped and the generators in `tools/tpch` and `tools/tpcds` are stubs.
- [ ] 20. Verification mode: run each query with the optimizer on and off and compare the results

## Phase 6: SQL surface

- [ ] 21. `MATCH_RECOGNIZE` (`sql/match_recognize`: 29 failing, 0%)
- [ ] 22. Triggers (`sql/trigger`: 29 failing, 0%)
- [ ] 23. CTEs: recursive, materialized, `USING KEY` (`sql/cte`: 31 failing, 9 erroring)
- [ ] 24. Subquery flattening edge cases (`sql/subquery`: 25 failing, 3 erroring)
- [ ] 25. Joins: ASOF, positional, lateral (`sql/join`: 46 failing)
- [ ] 26. Set operations, `SAMPLE`, `RETURNING`, `WITH ORDINALITY`, `UNNEST` (`setops` 8, `sample` 7, `returning` 2, `ordinality` 2, `unnest` 2)
- [ ] 27. SQL variables: `SET VARIABLE`, `getvariable` (`sql/variables`: 3 failing)
- [ ] 28. `EXPLAIN` / `EXPLAIN ANALYZE` and profiling (`explain` 7, `profiling` 3, `detailed_profiler` 1)
- [ ] 29. Query progress bar (`sql/progress`: 1 failing)

## Phase 5: Functions

- [ ] 30. Scalar and table functions (`sql/function`: 164 failing; `sql/table_function`: 43 failing)
- [ ] 31. Aggregates (`sql/aggregate`: 84 failing, 61.3%; target ≥ 90%)
- [ ] 32. Window functions (`sql/window`: 20 failing, 78.0%; target ≥ 85%)
- [ ] 33. `pragma_*` functions and PRAGMA statements (`sql/pragma`: 52 failing, 10.3%)

## Phase 4: Types and casts

- [ ] 34. Remaining type and cast failures (`sql/types`: 146 failing, 72.4%; `sql/cast`: 6 failing)
- [ ] 35. The VARIANT type (`sql/variant`: 2 failing)

## Phase 12: Catalog, settings, APIs and shell

- [ ] 36. Catalog failures (`sql/catalog`: 73 failing)
- [ ] 37. Settings, generated in full from `settings.json` (`sql/settings`: 35 failing, 2 erroring)
- [ ] 38. Secrets manager: `CREATE SECRET` and related statements (`sql/secrets`: 16 failing, 0%)
- [ ] 39. SQL export and DDL rendering (`sql/sql_export`: 69 failing)
- [ ] 40. Logging and error messages (`sql/logging` 8, `sql/error` 13, `test/logging` 2)
- [ ] 41. **C API** (`duckrs-capi`): implement `duckdb.h`, then pass `test/api/capi/*`. There is no code for this yet.
- [ ] 42. **CLI shell**: rustyline, `.mode` box renderer, `.timer`, `.read`, `.import`, autocomplete. There is no code for this yet.
- [ ] 43. **Rust API**: `Appender`, streaming results, Arrow `RecordBatch` export, `Relation` API. There is no code for this yet.
- [ ] 44. Connection tests (`sql/connect`: 2 failing, 5 skipped)

## Phase 13: Performance (not started)

- [ ] 45. Criterion benchmarks for the kernels (only `duckrs-common/benches/executor.rs` exists)
- [ ] 46. Nightly TPC-H, TPC-DS and ClickBench runs against C++ DuckDB at the baseline
- [ ] 47. Faster kernels: monomorphized code, SIMD, a perfect hash aggregate, the salt/pointer hash table design
- [ ] 48. v1 gate: TPC-H SF10 geometric mean ≤ 1.5× C++ DuckDB, and no query slower than 3×

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
