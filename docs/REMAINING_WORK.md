# Remaining work

What is still missing from the roadmap in [`RUST_PORT_PLAN.md`](../../RUST_PORT_PLAN.md) §6.
The test numbers come from [`STATUS.md`](STATUS.md) at baseline `d4eea3f208d`.

- Overall: 3680 of 6367 test files pass (**60.8%**). The v1 target is ≥ 90%.
- Phases 0–11 have code but are below their pass targets.
- Phase 12 is partly done. Phases 13 and 14 have not started.
- In git, the branches named "Phase 13"–"Phase 21" are rounds of fixes against the test suite.
  They are not the plan's Phase 13 (Performance) or Phase 14 (Extensions).

Each item gives the number of failing files in `test/` that it should fix, where known.

## Phase 11: File formats and I/O

- [ ] 1. COPY TO/FROM failures (`sql/copy`: 299 failing, 52.2% pass; target ≥ 85%)
  - [x] `PARTITION_BY` for CSV and JSON (Parquet had it), `PARTITION_BY *`, `PARTITIONED_BY` alias
  - [x] the `ORDER BY` option of COPY TO, and its checks against `USE_TMP_FILE` / `PER_THREAD_OUTPUT`
  - [x] `read_text` / `read_blob`, with hive partition columns
  - [ ] the `filename`, `file_index`, `file_row_number` virtual columns of the multi-file readers
  - [ ] `read_text` reads content even when only `filename`/`size` is selected, so binary files fail
  - [ ] `PER_THREAD_OUTPUT`, `FILE_SIZE_BYTES`, `APPEND`, `PARTITION_PATH`, `RETURN_STATS`
  - [ ] COPY formats `blob` and `tbl`, writing zstd
- [ ] 2. JSON: `read_json`/`read_json_auto`, the JSON type, JSON functions (`sql/json`: 95 failing, 34.9%)
- [ ] 3. Parquet reader/writer: missing types, filter/statistics pushdown, corrupt-file errors, delta encoding (`parquet*`: ~53 failing)
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
