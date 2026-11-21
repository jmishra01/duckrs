# GROUP BY hash table rewrite (plan)

Status: steps 1 to 4 done (parts 48, 49 and 51). The plain GROUP BY finds its groups in a `GroupTable`, merges
threads' tables by partition, and spills by the hash of its groups, so its states keep their tables throughout.
The aggregates with keys (DISTINCT, ORDER BY, grouping sets, VARIANT and BIT groups) are as they were. Step 5 is
left: `into_keys` is reached only by a key state now, and can go with the rest of what no path uses.

Step 4: whether an aggregate spills by hash or by key is decided once for it (`spills_by_hash`), as the rows it
spills and the groups that join them must go to the same partitions. The rows of groups a full state doesn't
hold are found by `GroupTable::find`, and go to the partition of their hash mixed with the level
(`SpillTarget::partition_of_hash`). `take_partitions` and `merge_groups` move groups between tables
(`absorb`). A table's state counts what its table grew by (`GroupTable::size_in_bytes`) and its states, where
the estimate of the values and two keys per group counted 1.4 (Q10) to 4.3 (Q18) times what the table holds.

Measured after step 3 on SF10, 8 threads, against part 46: Q18 1.49s to 1.07s (CPU 9.6s to 6.3s), Q10 0.40s
to 0.37s (CPU 7% less), Q13 4% less, Q01 the same. Step 2 alone was slower (Q10 15% more CPU), as the values
were kept twice and keys made for the merge.

## Why

On TPC-H SF10, warm Q10 takes 0.49s in duckrs against 0.23s in C++, with 2.5s of CPU against
1.1s. The GROUP BY is the largest single part of duckrs's CPU, about a third of it (profile of
part 44/46):

| Where | Samples | What it does |
|---|---|---|
| `create_row_key_buffer` | 5.3k | a byte key of all 7 group columns for every input row (about 170 bytes for Q10) |
| map lookup and `KeyHasher` | 2k | hashes and compares those bytes |
| `ColumnDataCollection::append` in `find_key_groups` | 1.5k | copies the values of new groups (copy 1) |
| `split_state` | 1.5k | copies each thread's values into partitions (copy 2) |
| `merge_groups` | 1.5k | copies them again into the merged partition (copy 3) |

C++ (`GroupedAggregateHashTable`, `RadixPartitionedHashTable`) hashes each group column and
combines the hashes. It probes a linear-probing table of 64-bit entries (a 16-bit salt plus a
pointer), and compares candidates column by column against rows in its row format
(`RowMatcher`). Only new groups are written, once, into a `TupleDataCollection` that is already
partitioned by hash bits. So there is no per-row key, and no split copy.

Two cheaper attempts were measured and reverted:
- **Split by view:** splitting threads' groups as views onto their values made the merge gather
  scattered rows, and was about 4% slower.
- **Group per build row:** remembering the group of each join build row hit 460 of 1.15M rows,
  because a build row is an order and a customer's rows come through different orders.

## What already exists

`duckrs-common/src/row` (Phase 1, parts 3 and 5) is a port of the row format that nothing uses
yet:
- `TupleDataLayout`: validity bits, columns, and 8-byte-aligned aggregate slots.
- `TupleDataCollection`: in-memory blocks that never move, `append(chunk) -> Vec<RowPtr>`,
  `scan()` back into chunks, and LIST/ARRAY values in side stores.
- `RowMatcher`: compares column `i` of a chunk with column `i` of candidate rows, per
  `ComparisonType`.

`duckrs-common/src/hash.rs` has `hash_vector` and `combine_hash` (C++'s `VectorOperations::Hash`
and `CombineHash`).

## What must keep working

`PhysicalAggregate` (`execution/physical.rs`, about 1,000 lines) also does:
- **Grouping sets:** one state per set.
- **Small dictionaries:** `find_dictionary_groups` handles group columns that are dictionaries
  with at most 65,536 combinations.
- **DISTINCT aggregates:** `distinct_seen`, keyed by group and value bytes.
- **Aggregates with ORDER BY:** `ordered`, where rows wait until they can be put in order.
- **Spilling:** past the memory limit, new groups' input rows go to 64 partition files by key
  hash (`SpillTarget::partition`), with up to 3 levels, and each partition is aggregated
  afterwards.
- **Partitioned merge:** threads' states are put together by partition once there are more than
  10,000 groups (`merge_partitioned`).
- **Memory accounting:** `bytes_per_group` and `bytes_per_row`, which the spill decision
  depends on.

Grouping equality is the equality of `create_row_keys` bytes today. The new table must group
exactly the same values together:
- **Floats:** -0.0 equals 0.0, and NaN equals NaN.
- **INTERVAL:** normalized, so '1 month' equals '30 days'.
- **TIMETZ:** ordered by time at UTC, then by offset.
- **VARIANT:** grouped by its meaning, not its bytes (`variant_comparator_key`).
- **BIT:** grouped by its bits.
- **Nested types:** LIST, STRUCT and ARRAY, including NULLs inside them.
- **NULL:** equals NULL (`NotDistinctFrom`).

## Design

1. **`GroupTable`** (new, in `duckrs-core/src/execution/group_table.rs`):
   - Group columns live in a `TupleDataCollection` with no aggregate slots at first.
   - Entries are `u64` values: the hash's top 16 bits as salt, and the group id + 1 in the low
     48 bits (0 means empty). Probing is linear, and the table resizes at 50% load
     (`GroupedAggregateHashTable::Resize`).
   - `rows: Vec<RowPtr>` maps a group id to its row.
   - `find_or_create(groups: &DataChunk, hashes: &Vector) -> Vec<usize>` follows C++'s
     `FindOrCreateGroupsInternal`:
     - Compare salts, and gather the candidates for one `RowMatcher::match` call using
       `NotDistinctFrom` per column.
     - Rows that don't match move to the next slot and probe again.
     - Rows that land on an empty slot become new groups: one append to the collection.
   - `lookup(groups, hashes) -> Vec<Option<usize>>`, which never inserts, is for the check of
     known groups once the sink is full.
   - Aggregate states stay where they are (`Vec<Vec<AggregateState>>` by group id), so
     `AggregateFunction::update` and `combine_states` don't change. In-row states (C++'s
     `RowOperations::InitializeStates`) can come later.
2. **Hashing:** `hash_vector` of the first group column, then `combine_hash` of the others. Before
   relying on it, check that it hashes the equal values above equally: -0.0 and 0.0, NaNs,
   normalized intervals, TIMETZ, VARIANT, BIT, and nested NULLs. Where it doesn't, either fix the
   hash to C++'s (C++ normalizes in `HashOp` for floats and intervals), or keep the old path for
   that type.
3. **Partitioning:** done at finalize instead of while collecting. The table keeps each
   group's hash, so a pass over the hashes gives each group's partition (bits 32 and up), with
   no copy (`merge_tables`).
4. **Finalize, per partition on its own thread:** insert the rows of every thread's partition `p`
   into a fresh `GroupTable`, gathered from the source rows (one copy, as in C++), and combine
   states. `set_output` scans the merged collection into chunks. This replaces `split_state` and
   `merge_groups` on this path.
5. **Spilling:** `SpillTarget::partition` uses the group hash instead of the `RowKey` hash, and the
   check of known groups uses `lookup`. Spilled partitions go through `GroupTable` too.
6. **Left as they are for now:** DISTINCT (`distinct_seen`), ORDER BY aggregates,
   `find_dictionary_groups`, and any group type whose hash or matcher can't be shown equal to the
   key bytes. They keep `create_row_keys`, behind one `Groups` enum (`Table(GroupTable)` or
   `Keys(RowKeyMap)`) chosen per grouping set when the aggregate is built.

## Steps

Each step is a part of its own, and is measured on Q10, Q18, Q13 and Q01 at SF10 with the
corpus clean.

1. **`GroupTable` with tests, not wired in.**
   - A property test: random chunks of every group type, grouped by `GroupTable` and by
     `RowKeyMap`, must give the same partition of rows into groups.
   - Unit tests for each equality case above, and for resizing past several load limits.
2. **Wire it into `find_groups`** for the plain path (one grouping set, no DISTINCT or ORDER BY,
   supported types), still splitting and merging the old way. Measure the sink alone.
3. **Partition while collecting, and merge by partition**, replacing `split_state` and
   `merge_groups` for this path. Measure the finalize.
4. **Spilling on hashes**, then the `temp_directory`, `aggregate` and `window` OOM files of the
   corpus. Check that `bytes_per_group` still matches what a group takes, now in the row format.
5. **Remove** what no path uses any more. Update `ARCHITECTURE.md`.

## Risks

- **Equality:** any difference between the hash or matcher equality and the key-byte equality
  splits or merges groups wrongly. The property test in step 1 is the guard, and types that
  don't pass stay on the old path.
- **Unsafe code:** `RowPtr` reads raw rows. Every `unsafe` block needs a `SAFETY` comment
  (workspace lint), and a collection must not be reset while a table points into it.
- **Memory:** the row format holds strings in the collection's own heap. Memory accounting must
  count that, or spilling decides wrongly.
- **Expected gain:** the GROUP BY is about a third of Q10's CPU, so halving it takes Q10 from
  about 2.5s to about 2.0s of CPU. Most of the rest of the gap to C++ is in the join probe, the
  join filter and the scans.
