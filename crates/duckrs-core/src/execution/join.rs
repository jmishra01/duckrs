// Ported from duckdb/src/execution/operator/join/{physical_cross_product,physical_hash_join,
// physical_blockwise_nl_join,physical_nested_loop_join,physical_piecewise_merge_join,
// physical_iejoin,physical_range_join,physical_comparison_join}.cpp and
// duckdb/src/execution/physical_plan/plan_comparison_join.cpp @ rust-port-baseline
//
//! Joins. The right child is the build side: it is gathered first (sink), and then the left
//! child's chunks stream through the join (operator). RIGHT and FULL joins emit the build rows
//! nothing matched once the left child is done (source).
//!
//! Execution is in memory. Threads can sink the build side at once, each keeping its chunks and
//! their keys until it is done. The keys go into partitions by their hash, and threads look up
//! the keys of each partition in a table of its own once all of them are there
//! (`JoinHashTable::Finalize` over radix partitions). Then threads probe the finished build side
//! at once: it no longer changes, apart from the flags of the build rows that found a match,
//! which are atomic.
//!
//! A hash join past `memory_limit` spills (`JoinHashTable` with external partitions): the largest
//! partitions of the build side go to temporary files, and so do their rows that come later. A
//! probe row whose key is in a spilled partition goes to a file of its partition too. Once the
//! probe is done, the join is a source: each spilled partition is read back and joined on its own,
//! the same way, with the build rows nothing matched for RIGHT and FULL joins. The rows of spilled
//! partitions come after the others, and a spilled partition is joined in memory whatever its size.
//! The build side is looked up by the sort key of its join keys, like grouping; `JoinHashTable`
//! over the row format replaces that when performance gets its turn (Phase 13).
//!
//! The joins on comparisons that are not equalities run the same way, with what finds the build
//! rows of a probe row in place of the hash table. `PhysicalPiecewiseMergeJoin` sorts the build
//! side by its first comparison, so the rows a probe row matches are a run of them that a binary
//! search finds. `PhysicalIEJoin` does that for two comparisons: the runs of the first one are
//! cut into nodes of a merge sort tree whose nodes are sorted by the second one, where binary
//! searches find the rows that match both. `PhysicalNestedLoopJoin` checks every pair, as
//! `PhysicalBlockwiseNLJoin` does. The comparisons the lookups do not cover are checked on the
//! pairs, with the rest of the predicate. C++ merges sorted runs of both sides instead of looking
//! up each probe row, so rows come out in another order.

use super::expression_executor::{MULTIPLE_ROWS, execute, select};
use super::physical::{
    LocalSinkState, OperatorResult, OperatorState, PhysicalOperator, SourceState, check_buffered, check_buffered_rows,
    parallel_expression,
};
use super::temporary::{PartitionedSpill, SpillFile, current, estimated_bytes};
use crate::optimizer::range_joins::{range_order, reorder_conditions};
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{JoinAlgorithm, JoinCondition};
use crate::storage::log_writer::can_serialize;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{KeyHasher, RowKey, create_row_keys};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, StringCopy, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::ComparisonOp;
use duckrs_parser::ast::JoinType;
use parking_lot::Mutex;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A chunk without rows, for when an operator has nothing to output this time.
fn empty_chunk() -> DataChunk {
    DataChunk::from_columns(Vec::new(), 0)
}

/// `PhysicalCrossProduct`.
#[derive(Debug)]
pub struct PhysicalCrossProduct {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub right: Mutex<ColumnDataCollection>,
}

/// `CrossProductExecutor`: walks one chunk row by row against the whole of the other one.
#[derive(Default)]
struct CrossProductState {
    /// the current chunk of the right side, and the next one
    current: Option<usize>,
    next: usize,
    position: usize,
    /// walk the input against the right chunk, not the other way around
    scan_input: bool,
}

impl PhysicalCrossProduct {
    pub fn new(types: Vec<LogicalType>, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        let right = ColumnDataCollection::new(children[1].types().to_vec());
        Self {
            types,
            children,
            right: Mutex::new(right),
        }
    }
}

impl PhysicalOperator for PhysicalCrossProduct {
    fn name(&self) -> &'static str {
        "CROSS_PRODUCT"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn is_sink(&self) -> bool {
        true
    }
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let mut right = self.right.lock();
        right.append(chunk);
        check_buffered(&right)
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(CrossProductState::default())
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let state = state.downcast_mut::<CrossProductState>().expect("cross product state");
        let right = self.right.lock();
        // NextValue
        state.position += 1;
        let size = match state.current {
            Some(_) if state.scan_input => input.len(),
            Some(current) => right.chunk(current).len(),
            None => 0,
        };
        if state.position >= size {
            if state.next >= right.chunk_count() {
                *state = CrossProductState::default();
                return Ok((empty_chunk(), OperatorResult::NeedMoreInput));
            }
            state.current = Some(state.next);
            state.next += 1;
            state.position = 0;
            // the larger chunk is the one output whole, for larger output chunks
            state.scan_input = input.len() < right.chunk(state.next - 1).len();
        }
        let right_chunk = right.chunk(state.current.expect("just set"));
        let constants = |chunk: &DataChunk| -> Vec<Vector> {
            (0..chunk.column_count())
                .map(|column| Vector::constant(&chunk.get_value(column, state.position)))
                .collect()
        };
        let (mut columns, right_columns, count) = if state.scan_input {
            (constants(input), right_chunk.columns().to_vec(), right_chunk.len())
        } else {
            (input.columns().to_vec(), constants(right_chunk), input.len())
        };
        columns.extend(right_columns);
        Ok((DataChunk::from_columns(columns, count), OperatorResult::HaveMoreOutput))
    }
}

/// `PhysicalHashJoin` with join keys, and `PhysicalBlockwiseNLJoin` without: every pair of a
/// left row and the build rows with its keys (all of them, without keys) is checked against the
/// predicate.
#[derive(Debug)]
pub struct PhysicalJoin {
    pub types: Vec<LogicalType>,
    pub join_type: JoinType,
    pub conditions: Vec<JoinCondition>,
    pub predicate: Option<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub build: Mutex<JoinBuild>,
    /// the build side once all of it is there
    finished: OnceLock<FinishedBuild>,
    /// how many threads sank the build side
    builders: AtomicUsize,
    /// SINGLE: a left row with two matches fails, instead of taking the first one
    pub error_on_multiple_rows: bool,
    /// where the partitions of a hash join past the memory limit go
    spill: Option<JoinSpill>,
    /// the comparisons a range join looks the build rows up by: one for a piecewise merge join,
    /// two for an IE join
    range: Vec<JoinCondition>,
    /// the bounds of the build keys that the inequalities give the scans of the probe side, with the key each is of
    bounds: Vec<(BoundExpr, std::sync::Arc<super::join_filter::TopNFilter>)>,
    /// the name of the operator, if it is not a hash join or a blockwise nested loop join
    operator_name: Option<&'static str>,
}

/// How many positions of the sorted build side a leaf of [`SecondIndex`] has.
const RANGE_BLOCK: usize = 64;

/// What a range join looks the build rows up by: the build rows without a NULL key, sorted by
/// the key of the first comparison (`PhysicalRangeJoin::GlobalSortedTable`).
#[derive(Debug)]
struct RangeIndex {
    /// the build row at each position
    rows: Vec<u32>,
    /// the key of the first comparison at each position, ascending
    first: Vec<RowKey>,
    /// IE join: the second comparison
    second: Option<SecondIndex>,
}

/// The keys of the second comparison of an IE join, at each position, in a merge sort tree: in
/// level `k`, the positions of each node of `RANGE_BLOCK << k` of them, sorted by their key.
#[derive(Debug)]
struct SecondIndex {
    keys: Vec<RowKey>,
    levels: Vec<Vec<u32>>,
}

/// Whether `left op right`, for sort keys.
fn compare_keys(op: ComparisonOp, left: &[u8], right: &[u8]) -> bool {
    match op {
        ComparisonOp::LessThan => left < right,
        ComparisonOp::LessThanOrEqual => left <= right,
        ComparisonOp::GreaterThan => left > right,
        ComparisonOp::GreaterThanOrEqual => left >= right,
        _ => unreachable!("range joins look up ranges"),
    }
}

/// The positions `0..len`, whose keys `key(i)` ascend, with `left op key(i)`: a suffix for `<`
/// and `<=`, a prefix for `>` and `>=`.
fn matching_run<'a>(
    len: usize,
    key: impl Fn(usize) -> &'a [u8],
    op: ComparisonOp,
    left: &[u8],
) -> std::ops::Range<usize> {
    let point = |pred: &dyn Fn(&[u8]) -> bool| {
        let (mut low, mut high) = (0, len);
        while low < high {
            let mid = (low + high) / 2;
            if pred(key(mid)) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    };
    match op {
        ComparisonOp::LessThan => point(&|k| k <= left)..len,
        ComparisonOp::LessThanOrEqual => point(&|k| k < left)..len,
        ComparisonOp::GreaterThan => 0..point(&|k| k < left),
        ComparisonOp::GreaterThanOrEqual => 0..point(&|k| k <= left),
        _ => unreachable!("range joins look up ranges"),
    }
}

impl SecondIndex {
    fn new(keys: Vec<RowKey>) -> Self {
        let count = keys.len();
        let mut level: Vec<u32> = (0..count as u32).collect();
        for block in level.chunks_mut(RANGE_BLOCK) {
            block.sort_by(|a, b| keys[*a as usize].as_bytes().cmp(keys[*b as usize].as_bytes()));
        }
        let mut levels = vec![level];
        let mut size = RANGE_BLOCK;
        while size < count {
            let below = levels.last().expect("a level");
            let mut merged = Vec::with_capacity(count);
            for pair in below.chunks(size * 2) {
                let (a, b) = pair.split_at(pair.len().min(size));
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    if keys[b[j] as usize].as_bytes() < keys[a[i] as usize].as_bytes() {
                        merged.push(b[j]);
                        j += 1;
                    } else {
                        merged.push(a[i]);
                        i += 1;
                    }
                }
                merged.extend_from_slice(&a[i..]);
                merged.extend_from_slice(&b[j..]);
            }
            levels.push(merged);
            size *= 2;
        }
        Self { keys, levels }
    }

    /// Adds the positions in `range` whose keys `k` have `left op k` to `out`.
    fn find(&self, range: std::ops::Range<usize>, op: ComparisonOp, left: &[u8], out: &mut Vec<u32>) {
        let (mut position, end) = (range.start, range.end);
        while position < end {
            if position % RANGE_BLOCK != 0 || position + RANGE_BLOCK > end {
                let stop = end.min((position / RANGE_BLOCK + 1) * RANGE_BLOCK);
                for p in position..stop {
                    if compare_keys(op, left, self.keys[p].as_bytes()) {
                        out.push(p as u32);
                    }
                }
                position = stop;
                continue;
            }
            let mut level = 0;
            while level + 1 < self.levels.len()
                && position % (RANGE_BLOCK << (level + 1)) == 0
                && position + (RANGE_BLOCK << (level + 1)) <= end
            {
                level += 1;
            }
            let size = RANGE_BLOCK << level;
            let node = &self.levels[level][position..position + size];
            let run = matching_run(node.len(), |i| self.keys[node[i] as usize].as_bytes(), op, left);
            out.extend_from_slice(&node[run]);
            position += size;
        }
    }
}

/// The temporary files of a hash join past the memory limit.
#[derive(Debug)]
struct JoinSpill {
    build: PartitionedSpill,
    probe: PartitionedSpill,
    /// the build rows of each spilled partition, once the build side is done
    build_files: Mutex<Vec<Option<SpillFile>>>,
}

/// A build side has at least this many rows in memory before it spills.
const MIN_SPILL_ROWS: usize = 16 * STANDARD_VECTOR_SIZE;

/// Roughly what a build row takes besides its values: its key, and its place in a chain.
const BYTES_PER_KEY: usize = 48;

/// A thread of the build side adds its rows once it has this many chunks, so that what threads
/// hold apart stays small next to the memory limit.
const LOCAL_BUILD_CHUNKS: usize = 64;

/// How many partitions the keys of the build side go into.
const PARTITIONS: usize = 64;

/// How many build rows it takes to look up their keys on more than one thread.
const MIN_PARALLEL_BUILD_ROWS: usize = 100_000;

/// The partition of a key. The tables read the low and the high bits of the hash, so the
/// partition is in the middle ones.
fn partition_of(key: &RowKey) -> usize {
    partition_of_hash(key_hash(key))
}

fn partition_of_hash(hash: u64) -> usize {
    (hash >> 32) as usize % PARTITIONS
}

/// The hash of a key, which picks both its partition and its slot in the partition's table.
fn key_hash(key: &RowKey) -> u64 {
    let mut hasher = KeyHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

/// A key of the build side, with its partition (always 0 when one thread sinks), or `None` when a key that is not null-safe is NULL.
type BuildKey = Option<(usize, RowKey)>;

/// The build side, while it comes in.
#[derive(Debug)]
pub struct JoinBuild {
    /// the build rows
    columns: Vec<Vector>,
    count: usize,
    /// per partition, the keys of the build rows and their rows, in the order they came in;
    /// rows with a NULL key that is not null-safe are not in there
    keys: Vec<Vec<(RowKey, u32)>>,
    /// whether a key that is not null-safe was NULL
    has_null_key: bool,
    /// whether the keys are in the partitions of their hash: threads put them there, and so does
    /// spilling
    partitioned: bool,
    /// the partitions whose rows are in a file
    spilled: Vec<bool>,
    /// how many rows went to files
    spilled_rows: usize,
    /// what the rows in memory took of the memory limit
    reserved: usize,
    /// per column, the vectors of the chunks that came in and are not in `columns` yet, each with the row it starts
    /// at and its rows: threads hand their chunks over while they hold the build, and the copying waits for
    /// [`Self::materialize`], which does it a column per thread
    pending: Vec<Vec<(Vector, usize, usize)>>,
}

impl JoinBuild {
    fn new(types: &[LogicalType]) -> Self {
        Self {
            columns: types
                .iter()
                .map(|t| Vector::new(t.clone(), STANDARD_VECTOR_SIZE))
                .collect(),
            count: 0,
            keys: (0..PARTITIONS).map(|_| Vec::new()).collect(),
            has_null_key: false,
            partitioned: false,
            spilled: vec![false; PARTITIONS],
            spilled_rows: 0,
            reserved: 0,
            pending: types.iter().map(|_| Vec::new()).collect(),
        }
    }

    /// Copies the chunks that came in into `columns`, each column on a thread of its own when there are many rows.
    fn materialize(&mut self) {
        let rows: usize = self
            .pending
            .first()
            .map_or(0, |column| column.iter().map(|&(_, _, len)| len).sum());
        if rows == 0 {
            return;
        }
        let count = self.count;
        let copy = |target: &mut Vector, sources: Vec<(Vector, usize, usize)>| {
            if target.capacity() < count {
                target.resize(count.next_power_of_two());
            }
            for (source, offset, len) in sources {
                let sel = SelectionVector::from_vec((0..len as u32).collect());
                target.copy_from(&source, &sel, len, offset, StringCopy::Reference);
            }
        };
        let pending: Vec<_> = self.pending.iter_mut().map(std::mem::take).collect();
        if rows < MIN_PARALLEL_BUILD_ROWS || self.columns.len() < 2 {
            for (target, sources) in self.columns.iter_mut().zip(pending) {
                copy(target, sources);
            }
            return;
        }
        std::thread::scope(|scope| {
            for (target, sources) in self.columns.iter_mut().zip(pending) {
                scope.spawn(move || copy(target, sources));
            }
        });
    }

    /// The build rows listed in `rows`, as a chunk of their own.
    fn rows_of(&self, rows: &[u32]) -> DataChunk {
        let sel = SelectionVector::from_vec(rows.to_vec());
        let columns = self
            .columns
            .iter()
            .map(|source| {
                let mut column = Vector::new(source.logical_type().clone(), rows.len().max(1));
                column.copy_from(source, &sel, rows.len(), 0, StringCopy::Deep);
                column
            })
            .collect();
        DataChunk::from_columns(columns, rows.len())
    }
}

/// The keys of a partition of the build side: a table of the distinct keys by their hash, and the
/// build rows of each in one array (those of key `g` at `offsets[g]..offsets[g + 1]`, in order).
#[derive(Debug, Default)]
struct Partition {
    /// the group of each distinct key, found by its hash
    table: hashbrown::HashTable<u32>,
    keys: Vec<RowKey>,
    hashes: Vec<u64>,
    offsets: Vec<u32>,
    rows: Vec<u32>,
}

impl Partition {
    fn new(keys: Vec<(RowKey, u32)>) -> Self {
        let mut partition = Self::default();
        let mut group_of = Vec::with_capacity(keys.len());
        let mut sizes: Vec<u32> = Vec::new();
        let mut rows_in_order = Vec::with_capacity(keys.len());
        for (key, row) in keys {
            let hash = key_hash(&key);
            let group = match partition.table.find(hash, |&g| partition.keys[g as usize] == key) {
                Some(&group) => group,
                None => {
                    let group = partition.keys.len() as u32;
                    let hashes = &partition.hashes;
                    partition.table.insert_unique(hash, group, |&g| hashes[g as usize]);
                    partition.keys.push(key);
                    partition.hashes.push(hash);
                    sizes.push(0);
                    group
                }
            };
            sizes[group as usize] += 1;
            group_of.push(group);
            rows_in_order.push(row);
        }
        // the rows of each key together, in the order they came in
        let mut offsets = Vec::with_capacity(sizes.len() + 1);
        let mut total = 0u32;
        offsets.push(0);
        for size in &sizes {
            total += size;
            offsets.push(total);
        }
        let mut next: Vec<u32> = offsets[..sizes.len()].to_vec();
        let mut rows = vec![0u32; rows_in_order.len()];
        for (group, row) in group_of.into_iter().zip(rows_in_order) {
            rows[next[group as usize] as usize] = row;
            next[group as usize] += 1;
        }
        partition.offsets = offsets;
        partition.rows = rows;
        partition
    }

    /// The group of `key`, whose hash is `hash`.
    fn group(&self, hash: u64, key: &RowKey) -> Option<usize> {
        self.table
            .find(hash, |&g| self.keys[g as usize] == *key)
            .map(|&g| g as usize)
    }

    /// The build rows of a group.
    fn rows(&self, group: usize) -> &[u32] {
        &self.rows[self.offsets[group] as usize..self.offsets[group + 1] as usize]
    }

    /// Asks for where the rows of `group` are.
    #[inline]
    fn prefetch_offsets(&self, group: usize) {
        if let Some(offset) = self.offsets.get(group) {
            super::prefetch(offset);
        }
    }

    /// Asks for the rows of `group`, reading where they are.
    #[inline]
    fn prefetch_rows(&self, group: usize) {
        if let Some(row) = self
            .offsets
            .get(group)
            .and_then(|&offset| self.rows.get(offset as usize))
        {
            super::prefetch(row);
        }
    }
}

/// The build side once all of it is there, which threads probe at once.
#[derive(Debug)]
struct FinishedBuild {
    /// the build rows, in one chunk
    data: DataChunk,
    count: usize,
    partitions: Vec<Partition>,
    /// whether the keys are in the partitions of their hash, which threads sinking does
    partitioned: bool,
    has_null_key: bool,
    /// whether each build row found a match
    matched: Vec<AtomicBool>,
    /// the partitions whose rows are in a file, which probe rows of theirs go to as well
    spilled: Vec<bool>,
    /// the build rows, those in files included
    total_count: usize,
    /// a range join: what finds the build rows of a probe row
    range: Option<RangeIndex>,
}

impl FinishedBuild {
    fn find_hashed(&self, hash: u64, key: &RowKey) -> Option<(usize, usize)> {
        // the hash picks the partition and the slot in its table both
        let partition = if self.partitioned { partition_of_hash(hash) } else { 0 };
        self.partitions[partition]
            .group(hash, key)
            .map(|group| (partition, group))
    }

    /// [`Self::find`] of each key, a window of them at a time: the keys of a window's candidate groups are
    /// fetched for all of its rows first, so that their reads from memory overlap rather than wait one by one.
    fn find_all(&self, keys: &[Option<RowKey>]) -> Vec<Option<(usize, usize)>> {
        const WINDOW: usize = 32;
        let hashes: Vec<u64> = keys.iter().map(|key| key.as_ref().map_or(0, key_hash)).collect();
        let mut groups = Vec::with_capacity(keys.len());
        for (keys, hashes) in keys.chunks(WINDOW).zip(hashes.chunks(WINDOW)) {
            for (key, &hash) in keys.iter().zip(hashes) {
                if key.is_some() {
                    let partition = &self.partitions[if self.partitioned { partition_of_hash(hash) } else { 0 }];
                    // every candidate is fetched and none taken, so this only reads the table
                    partition.table.find(hash, |&group| {
                        super::prefetch(&partition.keys[group as usize]);
                        false
                    });
                }
            }
            groups.extend(
                keys.iter()
                    .zip(hashes)
                    .map(|(key, &hash)| key.as_ref().and_then(|key| self.find_hashed(hash, key))),
            );
        }
        groups
    }

    /// Asks for what [`Partition::rows`] reads for the probe rows ahead of `row`, whose groups are `groups`: where
    /// the rows of the group of a row further on are, and the rows of the group of one nearer, whose place was asked
    /// for before. Probe rows find groups all over the table, and the reads overlap so.
    #[inline]
    fn prefetch_chains(&self, groups: &[Option<(usize, usize)>], row: usize) {
        const AHEAD: usize = 16;
        if let Some(&Some((partition, group))) = groups.get(row + 2 * AHEAD) {
            self.partitions[partition].prefetch_offsets(group);
        }
        if let Some(&Some((partition, group))) = groups.get(row + AHEAD) {
            self.partitions[partition].prefetch_rows(group);
        }
    }

    fn set_matched(&self, row: u32) {
        let flag = &self.matched[row as usize];
        // a load first, so that threads do not keep writing the same cache line
        if !flag.load(Ordering::Relaxed) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// What a thread sinks into the build side: its chunks and their keys.
type LocalBuild = Vec<(DataChunk, Vec<BuildKey>)>;

/// What a thread probing a join keeps between calls.
#[derive(Default)]
struct JoinProbeState {
    probe: Option<Probe>,
    /// the rows of the input chunk that are probed now, when others went to a file
    input: Option<DataChunk>,
}

/// Where the join is as a source.
#[derive(Default)]
struct JoinSource {
    /// the next build row in memory to look at for being unmatched
    next: usize,
    /// whether the build rows in memory are done
    memory_done: bool,
    /// the spilled partitions still to join: their build rows and probe rows
    pending: Option<Vec<(SpillFile, Option<SpillFile>)>>,
    active: Option<ActivePartition>,
}

/// A spilled partition being joined.
struct ActivePartition {
    build: FinishedBuild,
    probe: Option<super::temporary::SpillReader>,
    input: Option<DataChunk>,
    state: JoinProbeState,
    /// the next build row to look at for being unmatched
    next: usize,
}

/// Where probing the current input chunk is.
struct Probe {
    /// per input row, the partition and the group of its key (empty without keys)
    groups: Vec<Option<(usize, usize)>>,
    row: usize,
    /// the next candidate of `row`
    candidate: usize,
    /// whether each input row found a match
    found: Vec<bool>,
    /// MARK: whether the predicate was NULL for a pair of each input row
    nulls: Vec<bool>,
    /// SINGLE: the build row of each input row
    single: Vec<Option<u32>>,
    /// a range join: the keys of each input row
    range_keys: Vec<Option<Vec<RowKey>>>,
    /// a range join: the build rows `row` matches, once looked up: positions of the sorted
    /// build side for a piecewise merge join, build rows for an IE join
    current: Vec<u32>,
    current_run: std::ops::Range<usize>,
    current_row: Option<usize>,
}

impl PhysicalJoin {
    pub fn new(
        types: Vec<LogicalType>,
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let build = JoinBuild::new(children[1].types());
        // the keys are computed again for rows that come back from a file
        let spill = (!conditions.is_empty()
            && conditions
                .iter()
                .all(|c| parallel_expression(&c.left) && parallel_expression(&c.right))
            && can_serialize(children[0].types())
            && can_serialize(children[1].types()))
        .then(|| JoinSpill {
            build: PartitionedSpill::new(PARTITIONS),
            probe: PartitionedSpill::new(PARTITIONS),
            build_files: Mutex::default(),
        });
        Self {
            types,
            join_type,
            conditions,
            predicate,
            children,
            build: Mutex::new(build),
            finished: OnceLock::new(),
            builders: AtomicUsize::new(0),
            error_on_multiple_rows: false,
            spill,
            range: Vec::new(),
            bounds: Vec::new(),
            operator_name: None,
        }
    }

    /// `PlanComparisonJoin` for the operator the optimizer picked: the comparisons each operator
    /// does not run itself are checked on the pairs, with the predicate.
    pub fn for_algorithm(
        types: Vec<LogicalType>,
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        algorithm: JoinAlgorithm,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let bounds: Vec<_> = conditions
            .iter()
            .filter_map(|c| c.bound.clone().map(|bound| (c.right.clone(), bound)))
            .collect();
        let mut join = Self::for_algorithm_unbounded(types, join_type, conditions, predicate, algorithm, children);
        join.bounds = bounds;
        join
    }

    fn for_algorithm_unbounded(
        types: Vec<LogicalType>,
        join_type: JoinType,
        mut conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        algorithm: JoinAlgorithm,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let left_width = children[0].types().len();
        // `RewriteJoinCondition`: the right side of a pair comes after its left side
        let pair_predicate = |conditions: Vec<JoinCondition>, predicate: Option<BoundExpr>| {
            let mut expressions: Vec<BoundExpr> = conditions
                .into_iter()
                .map(|c| {
                    let mut right = c.right;
                    right.walk_mut(&mut |e| {
                        if let BoundExprKind::Reference(index) = &mut e.kind {
                            *index += left_width;
                        }
                    });
                    BoundExpr::new(
                        BoundExprKind::Comparison {
                            op: c.comparison,
                            left: Box::new(c.left),
                            right: Box::new(right),
                        },
                        LogicalType::BOOLEAN,
                    )
                })
                .collect();
            expressions.extend(predicate);
            (!expressions.is_empty()).then(|| crate::optimizer::conjunction_of(expressions))
        };
        match algorithm {
            JoinAlgorithm::Hash => {
                reorder_conditions(&mut conditions);
                let (keys, others): (Vec<_>, Vec<_>) = conditions.into_iter().partition(JoinCondition::is_equality);
                let predicate = pair_predicate(others, predicate);
                Self::new(types, join_type, keys, predicate, children)
            }
            JoinAlgorithm::NestedLoop => {
                reorder_conditions(&mut conditions);
                let predicate = pair_predicate(conditions, predicate);
                let mut join = Self::new(types, join_type, Vec::new(), predicate, children);
                join.operator_name = Some("NESTED_LOOP_JOIN");
                join
            }
            JoinAlgorithm::PiecewiseMerge | JoinAlgorithm::IeJoin => {
                range_order(&mut conditions);
                let looked_up = if algorithm == JoinAlgorithm::IeJoin { 2 } else { 1 };
                let rest = conditions.split_off(looked_up);
                let predicate = pair_predicate(rest, predicate);
                let mut join = Self::new(types, join_type, Vec::new(), predicate, children);
                join.range = conditions;
                join.operator_name = Some(if algorithm == JoinAlgorithm::IeJoin {
                    "IE_JOIN"
                } else {
                    "PIECEWISE_MERGE_JOIN"
                });
                join
            }
        }
    }

    /// The keys of the comparisons of a range join for `side` of the rows of `chunk`, or `None`
    /// for a row with a NULL key, which matches nothing.
    fn range_keys(&self, chunk: &DataChunk, right: bool) -> Result<Vec<Option<Vec<RowKey>>>> {
        let mut keys: Vec<Option<Vec<RowKey>>> = vec![Some(Vec::with_capacity(self.range.len())); chunk.len()];
        for condition in &self.range {
            let vector = execute(if right { &condition.right } else { &condition.left }, chunk)?;
            let unified = vector.to_unified(chunk.len());
            let row_keys = create_row_keys(&[&vector], chunk.len())?;
            for (row, key) in row_keys.into_iter().enumerate() {
                if !unified.is_valid(row) {
                    keys[row] = None;
                } else if let Some(keys) = &mut keys[row] {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    }

    /// The range index of the build rows of `data`.
    fn range_index(&self, data: &DataChunk, count: usize) -> Result<RangeIndex> {
        let mut entries: Vec<(u32, Vec<RowKey>)> = Vec::new();
        let mut start = 0;
        while start < count {
            let end = (start + STANDARD_VECTOR_SIZE).min(count);
            let chunk = Self::rows_of(data, (start as u32..end as u32).collect());
            for (row, keys) in self.range_keys(&chunk, true)?.into_iter().enumerate() {
                if let Some(keys) = keys {
                    entries.push(((start + row) as u32, keys));
                }
            }
            start = end;
        }
        entries.sort_by(|a, b| a.1[0].as_bytes().cmp(b.1[0].as_bytes()));
        let mut rows = Vec::with_capacity(entries.len());
        let mut first = Vec::with_capacity(entries.len());
        let mut second = Vec::new();
        for (row, mut keys) in entries {
            rows.push(row);
            if keys.len() > 1 {
                second.push(keys.pop().expect("two keys"));
            }
            first.push(keys.pop().expect("a key"));
        }
        let second = (self.range.len() > 1).then(|| SecondIndex::new(second));
        Ok(RangeIndex { rows, first, second })
    }

    /// The build rows that `keys`, the keys of a probe row, match: the positions of the sorted
    /// build side it gives for one comparison, and in `out` for two.
    fn range_candidates(&self, index: &RangeIndex, keys: &[RowKey], out: &mut Vec<u32>) -> std::ops::Range<usize> {
        out.clear();
        let run = matching_run(
            index.first.len(),
            |i| index.first[i].as_bytes(),
            self.range[0].comparison,
            keys[0].as_bytes(),
        );
        match &index.second {
            None => run,
            Some(second) => {
                second.find(run, self.range[1].comparison, keys[1].as_bytes(), out);
                for position in out.iter_mut() {
                    *position = index.rows[*position as usize];
                }
                0..0
            }
        }
    }

    /// Whether the join emits build rows after the probe: those nothing matched, or for RIGHT
    /// SEMI those something did.
    fn emits_build_rows(&self) -> bool {
        matches!(
            self.join_type,
            JoinType::Right | JoinType::Outer | JoinType::RightSemi | JoinType::RightAnti
        )
    }

    fn finished(&self) -> &FinishedBuild {
        self.finished.get().expect("the build side is finished")
    }

    /// The keys of the build rows of `chunk`.
    fn build_keys(&self, chunk: &DataChunk, partitioned: bool) -> Result<Vec<BuildKey>> {
        if self.conditions.is_empty() {
            return Ok(Vec::new());
        }
        let keys = self.keys(chunk, |c| &c.right)?;
        let partition = |key: &RowKey| if partitioned { partition_of(key) } else { 0 };
        Ok(keys
            .into_iter()
            .map(|key| key.map(|key| (partition(&key), key)))
            .collect())
    }

    /// Adds `chunk`, whose keys are `keys`, to the build side: in partitions of their hash if
    /// `partitioned`. Past the memory limit, partitions go to files.
    fn append(
        &self,
        build: &mut JoinBuild,
        chunk: &DataChunk,
        mut keys: Vec<BuildKey>,
        partitioned: bool,
    ) -> Result<()> {
        if build.partitioned && !partitioned {
            for (partition, key) in keys.iter_mut().flatten() {
                *partition = partition_of(key);
            }
        }
        let Some(spill) = &self.spill else {
            Self::append_rows(build, chunk, keys);
            return check_buffered_rows(self.children[1].types(), build.count);
        };
        let mut kept = None;
        if build.spilled.iter().any(|&spilled| spilled) {
            // the rows of spilled partitions go to their files
            let mut spilled: Vec<Vec<u32>> = vec![Vec::new(); PARTITIONS];
            let mut stay = Vec::new();
            let mut stay_keys = Vec::new();
            for (row, key) in keys.into_iter().enumerate() {
                match key {
                    Some((partition, _)) if build.spilled[partition] => spilled[partition].push(row as u32),
                    key => {
                        stay.push(row as u32);
                        stay_keys.push(key);
                    }
                }
            }
            for (partition, rows) in spilled.into_iter().enumerate().filter(|(_, rows)| !rows.is_empty()) {
                build.spilled_rows += rows.len();
                spill.build.write(partition, &Self::rows_of(chunk, rows))?;
            }
            keys = stay_keys;
            kept = Some(Self::rows_of(chunk, stay));
        }
        let chunk = kept.as_ref().unwrap_or(chunk);
        let bytes = estimated_bytes(chunk) + chunk.len() * BYTES_PER_KEY;
        Self::append_rows(build, chunk, keys);
        if current().try_reserve(bytes) {
            build.reserved += bytes;
        } else if build.count >= MIN_SPILL_ROWS {
            self.spill_partitions(build, spill)?;
        }
        Ok(())
    }

    /// Adds `chunk` to the build rows in memory, to be copied into the build columns later
    /// ([`JoinBuild::materialize`]): copying is what threads would take turns at here. Its strings stay where they
    /// are, in the buffers of the chunk, which the build columns keep.
    fn append_rows(build: &mut JoinBuild, chunk: &DataChunk, keys: Vec<BuildKey>) {
        let offset = build.count;
        if !chunk.is_empty() {
            for (pending, source) in build.pending.iter_mut().zip(chunk.columns()) {
                pending.push((source.clone(), offset, chunk.len()));
            }
        }
        build.count += chunk.len();
        for (row, key) in keys.into_iter().enumerate() {
            match key {
                Some((partition, key)) => build.keys[partition].push((key, (offset + row) as u32)),
                None => build.has_null_key = true,
            }
        }
    }

    /// Writes the largest partitions in memory to their files, about half of the rows with keys,
    /// and keeps the rest of the build rows in their order.
    fn spill_partitions(&self, build: &mut JoinBuild, spill: &JoinSpill) -> Result<()> {
        build.materialize();
        if !build.partitioned {
            for (key, row) in std::mem::take(&mut build.keys[0]) {
                build.keys[partition_of(&key)].push((key, row));
            }
            build.partitioned = true;
        }
        let keyed: usize = build.keys.iter().map(Vec::len).sum();
        let mut order: Vec<usize> = (0..PARTITIONS)
            .filter(|&p| !build.spilled[p] && !build.keys[p].is_empty())
            .collect();
        order.sort_by_key(|&p| std::cmp::Reverse(build.keys[p].len()));
        let mut dead = vec![false; build.count];
        let mut moved = 0;
        for partition in order {
            if moved * 2 >= keyed {
                break;
            }
            let keys = std::mem::take(&mut build.keys[partition]);
            let rows: Vec<u32> = keys.iter().map(|(_, row)| *row).collect();
            for part in rows.chunks(STANDARD_VECTOR_SIZE) {
                spill.build.write(partition, &build.rows_of(part))?;
            }
            for &row in &rows {
                dead[row as usize] = true;
            }
            moved += rows.len();
            build.spilled[partition] = true;
            build.spilled_rows += rows.len();
        }
        if moved == 0 {
            return Ok(());
        }
        // what is left, in its order, with the rows of the keys moved to match
        let live: Vec<u32> = (0..build.count as u32).filter(|&row| !dead[row as usize]).collect();
        let mut new_row = vec![0u32; build.count];
        for (new, &old) in live.iter().enumerate() {
            new_row[old as usize] = new as u32;
        }
        let old_count = build.count;
        let mut columns = build.rows_of(&live).columns().to_vec();
        for column in &mut columns {
            // more rows come in after these
            let mut grown = Vector::new(column.logical_type().clone(), live.len().max(STANDARD_VECTOR_SIZE));
            let sel = SelectionVector::from_vec((0..live.len() as u32).collect());
            grown.copy_from(column, &sel, live.len(), 0, StringCopy::Deep);
            *column = grown;
        }
        build.columns = columns;
        build.count = live.len();
        for keys in &mut build.keys {
            for (_, row) in keys.iter_mut() {
                *row = new_row[*row as usize];
            }
        }
        let keep = (build.reserved as u128 * live.len() as u128 / old_count.max(1) as u128) as usize;
        current().release(build.reserved - keep);
        build.reserved = keep;
        Ok(())
    }

    /// The build side, done.
    fn finish_build(build: &mut JoinBuild, threads: usize, total_count: usize, has_null_key: bool) -> FinishedBuild {
        build.materialize();
        let count = build.count;
        FinishedBuild {
            data: DataChunk::from_columns(std::mem::take(&mut build.columns), count),
            count,
            partitions: Self::partitions(std::mem::take(&mut build.keys), threads),
            partitioned: build.partitioned,
            has_null_key,
            matched: (0..count).map(|_| AtomicBool::new(false)).collect(),
            spilled: std::mem::take(&mut build.spilled),
            total_count,
            range: None,
        }
    }

    /// The build side of a spilled partition, from its file.
    fn load_partition(&self, file: SpillFile) -> Result<FinishedBuild> {
        let mut build = JoinBuild::new(self.children[1].types());
        let mut reader = file.into_reader()?;
        while let Some(chunk) = reader.next_chunk()? {
            let keys = self.build_keys(&chunk, false)?;
            Self::append_rows(&mut build, &chunk, keys);
        }
        let main = self.finished();
        let mut finished = Self::finish_build(&mut build, 1, main.total_count, main.has_null_key);
        finished.spilled = vec![false; PARTITIONS];
        Ok(finished)
    }

    /// The next build rows of `build` to emit after the probe from row `next` on: those nothing
    /// matched, or for RIGHT SEMI those something did.
    fn build_rows_chunk(&self, build: &FinishedBuild, next: &mut usize) -> Option<DataChunk> {
        let wanted = self.join_type == JoinType::RightSemi;
        let mut rows = Vec::new();
        while *next < build.count && rows.len() < STANDARD_VECTOR_SIZE {
            if build.matched[*next].load(Ordering::Relaxed) == wanted {
                rows.push(*next as u32);
            }
            *next += 1;
        }
        if rows.is_empty() {
            return None;
        }
        let count = rows.len();
        let right = Self::rows_of(&build.data, rows);
        if matches!(self.join_type, JoinType::RightSemi | JoinType::RightAnti) {
            return Some(right);
        }
        let mut columns: Vec<Vector> = self.children[0]
            .types()
            .iter()
            .map(|t| Vector::constant_null(t.clone()))
            .collect();
        columns.extend_from_slice(right.columns());
        Some(DataChunk::from_columns(columns, count))
    }

    /// The next output of the spilled partitions.
    fn next_spilled(&self, source: &mut JoinSource) -> Result<Option<DataChunk>> {
        let Some(spill) = &self.spill else {
            return Ok(None);
        };
        loop {
            if source.active.is_none() {
                let pending = match &mut source.pending {
                    Some(pending) => pending,
                    None => {
                        let builds = std::mem::take(&mut *spill.build_files.lock());
                        let mut probes = spill.probe.take()?;
                        let mut pending = Vec::new();
                        for (partition, build) in builds.into_iter().enumerate() {
                            if let Some(build) = build {
                                pending.push((build, probes[partition].take()));
                            }
                        }
                        // popped from the back, so the first partition goes first
                        pending.reverse();
                        source.pending.insert(pending)
                    }
                };
                let Some((build, probe)) = pending.pop() else {
                    return Ok(None);
                };
                if probe.is_none() && !self.emits_build_rows() {
                    continue;
                }
                source.active = Some(ActivePartition {
                    build: self.load_partition(build)?,
                    probe: probe.map(SpillFile::into_reader).transpose()?,
                    input: None,
                    state: JoinProbeState::default(),
                    next: 0,
                });
            }
            let active = source.active.as_mut().expect("just set");
            if let Some(input) = &active.input {
                let (chunk, result) = self.probe_with(&active.build, input, &mut active.state, false)?;
                if result == OperatorResult::NeedMoreInput {
                    active.input = None;
                }
                if !chunk.is_empty() {
                    return Ok(Some(chunk));
                }
                continue;
            }
            if let Some(reader) = &mut active.probe {
                match reader.next_chunk()? {
                    Some(chunk) => active.input = Some(chunk),
                    None => active.probe = None,
                }
                continue;
            }
            if self.emits_build_rows()
                && let Some(chunk) = self.build_rows_chunk(&active.build, &mut active.next)
            {
                return Ok(Some(chunk));
            }
            source.active = None;
        }
    }

    /// The key of each row of `chunk`, or `None` for a row that can match nothing because a key
    /// that is not null-safe is NULL.
    fn keys(&self, chunk: &DataChunk, side: impl Fn(&JoinCondition) -> &BoundExpr) -> Result<Vec<Option<RowKey>>> {
        let vectors = self
            .conditions
            .iter()
            .map(|c| execute(side(c), chunk))
            .collect::<Result<Vec<_>>>()?;
        let references: Vec<&Vector> = vectors.iter().collect();
        let keys = create_row_keys(&references, chunk.len())?;
        let nulls: Vec<_> = self
            .conditions
            .iter()
            .zip(&vectors)
            .filter(|(c, _)| !c.null_safe())
            .map(|(_, v)| v.to_unified(chunk.len()))
            .collect();
        Ok(keys
            .into_iter()
            .enumerate()
            .map(|(row, key)| nulls.iter().all(|u| u.is_valid(row)).then_some(key))
            .collect())
    }

    /// Probes `build` with `input`: `PhysicalJoin::ExecuteInternal`. With `defer`, the rows whose
    /// keys are in spilled partitions go to the files of their partitions instead.
    fn probe_with(
        &self,
        build: &FinishedBuild,
        input: &DataChunk,
        state: &mut JoinProbeState,
        defer: bool,
    ) -> Result<(DataChunk, OperatorResult)> {
        let JoinProbeState {
            probe: slot,
            input: kept,
        } = state;
        let probe = match slot {
            Some(probe) => probe,
            None => {
                let mut nulls = vec![false; input.len()];
                let groups = if self.conditions.is_empty() {
                    Vec::new()
                } else {
                    let mut keys = self.keys(input, |c| &c.left)?;
                    if defer && build.spilled.iter().any(|&spilled| spilled) {
                        keys = self.defer_rows(build, input, keys, kept, &mut nulls)?;
                    }
                    // a comparison with NULL is NULL, but there is no comparison without right rows
                    if self.join_type == JoinType::Mark && build.total_count > 0 {
                        for (null, key) in nulls.iter_mut().zip(&keys) {
                            *null = key.is_none() || build.has_null_key;
                        }
                    }
                    build.find_all(&keys)
                };
                let count = kept.as_ref().map_or(input.len(), DataChunk::len);
                let range_keys = if self.range.is_empty() {
                    Vec::new()
                } else {
                    self.range_keys(input, false)?
                };
                slot.insert(Probe {
                    groups,
                    row: 0,
                    candidate: 0,
                    found: vec![false; count],
                    nulls,
                    single: vec![None; count],
                    range_keys,
                    current: Vec::new(),
                    current_run: 0..0,
                    current_row: None,
                })
            }
        };
        let input = kept.as_ref().unwrap_or(input);
        let data = &build.data;
        let emit_pairs = matches!(
            self.join_type,
            JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Outer
        );
        loop {
            let (left, right) = if let Some(index) = &build.range {
                self.next_range_pairs(index, probe)
            } else if self.conditions.is_empty() {
                self.next_pairs(build, probe)
            } else {
                self.next_key_pairs(build, probe)
            };
            if left.is_empty() {
                let result = self.finish_probe(input, probe, data)?;
                *slot = None;
                *kept = None;
                return Ok((result, OperatorResult::NeedMoreInput));
            }
            let count = left.len();
            // the pairs' rows, which a join that only marks its rows needs for a predicate only
            let pairs_of = || {
                let mut columns = Self::rows_of(input, left.clone()).columns().to_vec();
                columns.extend_from_slice(Self::rows_of(data, right.clone()).columns());
                DataChunk::from_columns(columns, count)
            };
            let mut pairs = (emit_pairs || self.predicate.is_some()).then(pairs_of);
            let matches = match &self.predicate {
                Some(predicate) if self.join_type == JoinType::Mark => {
                    let pairs = pairs.as_ref().expect("made for the predicate");
                    let result = super::expression_executor::as_join_condition(|| execute(predicate, pairs))?;
                    let mut selected = Vec::new();
                    for (row, left) in left.iter().enumerate() {
                        match result.get_value(row).data() {
                            ValueData::Bool(true) => selected.push(row as u32),
                            ValueData::Bool(false) => {}
                            _ => probe.nulls[*left as usize] = true,
                        }
                    }
                    let matched = selected.len();
                    Some((SelectionVector::from_vec(selected), matched))
                }
                Some(predicate) => {
                    let pairs = pairs.as_ref().expect("made for the predicate");
                    let (sel, matched) = super::expression_executor::as_join_condition(|| select(predicate, pairs))?;
                    (matched != count).then_some((sel, matched))
                }
                None => None,
            };
            match &matches {
                Some((sel, matched)) => {
                    for i in 0..*matched {
                        probe.found[left[sel.get(i)] as usize] = true;
                        build.set_matched(right[sel.get(i)]);
                    }
                }
                None => {
                    for (left, right) in left.iter().zip(&right) {
                        probe.found[*left as usize] = true;
                        build.set_matched(*right);
                    }
                }
            }
            if self.join_type == JoinType::Single {
                let pairs_found: Vec<(u32, u32)> = match &matches {
                    Some((sel, matched)) => (0..*matched).map(|i| (left[sel.get(i)], right[sel.get(i)])).collect(),
                    None => left.iter().copied().zip(right.iter().copied()).collect(),
                };
                for (left, right) in pairs_found {
                    let slot = &mut probe.single[left as usize];
                    if slot.is_none() {
                        *slot = Some(right);
                    } else if self.error_on_multiple_rows {
                        return Err(Error::new(ErrorKind::InvalidInput, MULTIPLE_ROWS));
                    }
                }
            }
            if !emit_pairs {
                continue;
            }
            let mut pairs = pairs.take().expect("made for the pairs to emit");
            if let Some((sel, matched)) = matches {
                pairs.slice(&sel, matched);
            }
            return Ok((pairs, OperatorResult::HaveMoreOutput));
        }
    }

    /// Writes the rows of `input` whose keys are in spilled partitions to the files of their
    /// partitions, and keeps the others in `kept`. Gives the keys of those that stay.
    fn defer_rows(
        &self,
        build: &FinishedBuild,
        input: &DataChunk,
        keys: Vec<Option<RowKey>>,
        kept: &mut Option<DataChunk>,
        nulls: &mut Vec<bool>,
    ) -> Result<Vec<Option<RowKey>>> {
        let spill = self.spill.as_ref().expect("spilled partitions come from spilling");
        let mut deferred: Vec<Vec<u32>> = vec![Vec::new(); PARTITIONS];
        let mut stay = Vec::new();
        let mut stay_keys = Vec::new();
        for (row, key) in keys.into_iter().enumerate() {
            match key {
                Some(key) if build.spilled[partition_of(&key)] => deferred[partition_of(&key)].push(row as u32),
                key => {
                    stay.push(row as u32);
                    stay_keys.push(key);
                }
            }
        }
        if stay.len() == input.len() {
            return Ok(stay_keys);
        }
        for (partition, rows) in deferred.into_iter().enumerate().filter(|(_, rows)| !rows.is_empty()) {
            spill.probe.write(partition, &Self::rows_of(input, rows))?;
        }
        nulls.truncate(stay.len());
        *kept = Some(Self::rows_of(input, stay));
        Ok(stay_keys)
    }

    /// Whether a left row is done at its first match.
    fn first_match_only(&self) -> bool {
        match self.join_type {
            JoinType::Semi | JoinType::Anti | JoinType::Mark => true,
            JoinType::Single => !self.error_on_multiple_rows,
            _ => false,
        }
    }

    /// Looks up the keys of each partition, on up to `threads` threads.
    fn partitions(keys: Vec<Vec<(RowKey, u32)>>, threads: usize) -> Vec<Partition> {
        if threads < 2 {
            return keys.into_iter().map(Partition::new).collect();
        }
        let keys: Vec<Mutex<Vec<(RowKey, u32)>>> = keys.into_iter().map(Mutex::new).collect();
        let partitions: Vec<Mutex<Partition>> = (0..keys.len()).map(|_| Mutex::default()).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.min(keys.len()) {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(keys) = keys.get(index) else {
                            break;
                        };
                        *partitions[index].lock() = Partition::new(std::mem::take(&mut *keys.lock()));
                    }
                });
            }
        });
        partitions.into_iter().map(Mutex::into_inner).collect()
    }

    /// The next pairs of a left row and a build row its range keys match, at most a vector of them.
    fn next_range_pairs(&self, index: &RangeIndex, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        while left.len() < STANDARD_VECTOR_SIZE && probe.row < probe.found.len() {
            let row = probe.row;
            if probe.current_row != Some(row) {
                probe.current.clear();
                probe.current_run = 0..0;
                if !(self.first_match_only() && probe.found[row])
                    && let Some(keys) = &probe.range_keys[row]
                {
                    probe.current_run = self.range_candidates(index, keys, &mut probe.current);
                }
                probe.current_row = Some(row);
            }
            let candidates = match index.second {
                None => &index.rows[probe.current_run.clone()],
                Some(_) => &probe.current[..],
            };
            // a SEMI or ANTI row is done at its first match, which the pairs so far may have; without
            // a predicate every build row the lookup finds is a match (`ResolveSimpleJoin`)
            let done = self.first_match_only() && probe.found[row];
            let chain = if done {
                &[][..]
            } else if self.first_match_only() && self.predicate.is_none() {
                &candidates[..candidates.len().min(1)]
            } else {
                candidates
            };
            let take = chain
                .len()
                .saturating_sub(probe.candidate)
                .min(STANDARD_VECTOR_SIZE - left.len());
            left.extend(std::iter::repeat_n(row as u32, take));
            if take > 0 {
                right.extend_from_slice(&chain[probe.candidate..probe.candidate + take]);
            }
            probe.candidate += take;
            if probe.candidate >= chain.len() {
                probe.row += 1;
                probe.candidate = 0;
            }
        }
        (left, right)
    }

    /// The next pairs of a left and a build row with equal keys, at most a vector of them.
    fn next_key_pairs(&self, build: &FinishedBuild, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        if matches!(self.join_type, JoinType::RightSemi | JoinType::RightAnti) {
            return Self::next_unmatched_pairs(build, probe);
        }
        let mut left = Vec::new();
        let mut right = Vec::new();
        while left.len() < STANDARD_VECTOR_SIZE && probe.row < probe.found.len() {
            let row = probe.row;
            build.prefetch_chains(&probe.groups, row);
            let chain: &[u32] = match probe.groups[row] {
                Some((partition, group)) if !(self.first_match_only() && probe.found[row]) => {
                    build.partitions[partition].rows(group)
                }
                _ => &[],
            };
            let take = chain
                .len()
                .saturating_sub(probe.candidate)
                .min(STANDARD_VECTOR_SIZE - left.len());
            left.extend(std::iter::repeat_n(row as u32, take));
            if take > 0 {
                right.extend_from_slice(&chain[probe.candidate..probe.candidate + take]);
            }
            probe.candidate += take;
            if probe.candidate >= chain.len() {
                probe.row += 1;
                probe.candidate = 0;
            }
        }
        (left, right)
    }

    /// [`Self::next_key_pairs`] of a join that gives the build rows that match or those that do not: a build row
    /// that matched already is done, so it pairs with no more probe rows. A flag only ever turns true, so one that
    /// another thread sets meanwhile only costs a pair.
    fn next_unmatched_pairs(build: &FinishedBuild, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        while left.len() < STANDARD_VECTOR_SIZE && probe.row < probe.found.len() {
            let row = probe.row;
            build.prefetch_chains(&probe.groups, row);
            let chain: &[u32] = match probe.groups[row] {
                Some((partition, group)) => build.partitions[partition].rows(group),
                None => &[],
            };
            while probe.candidate < chain.len() && left.len() < STANDARD_VECTOR_SIZE {
                let candidate = chain[probe.candidate];
                if !build.matched[candidate as usize].load(Ordering::Relaxed) {
                    left.push(row as u32);
                    right.push(candidate);
                }
                probe.candidate += 1;
            }
            if probe.candidate >= chain.len() {
                probe.row += 1;
                probe.candidate = 0;
            }
        }
        (left, right)
    }

    /// The next pairs without join keys. They come in the order of `CrossProductExecutor`, which
    /// `PhysicalBlockwiseNLJoin` runs on: per vector of the build side, a row of the smaller
    /// chunk against all of the larger one. `probe.row` counts those vectors.
    fn next_pairs(&self, build: &FinishedBuild, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        let input_count = probe.found.len();
        loop {
            let start = probe.row * STANDARD_VECTOR_SIZE;
            if start >= build.count {
                return (Vec::new(), Vec::new());
            }
            let size = (build.count - start).min(STANDARD_VECTOR_SIZE);
            let scan_input = input_count < size;
            if probe.candidate >= if scan_input { input_count } else { size } {
                probe.row += 1;
                probe.candidate = 0;
                continue;
            }
            let position = probe.candidate;
            probe.candidate += 1;
            let skip = |row: usize| self.first_match_only() && probe.found[row];
            let (left, right): (Vec<u32>, Vec<u32>) = if scan_input {
                if skip(position) {
                    continue;
                }
                (
                    vec![position as u32; size],
                    (start..start + size).map(|row| row as u32).collect(),
                )
            } else {
                let left: Vec<u32> = (0..input_count)
                    .filter(|&row| !skip(row))
                    .map(|row| row as u32)
                    .collect();
                let right = vec![(start + position) as u32; left.len()];
                (left, right)
            };
            if !left.is_empty() {
                return (left, right);
            }
        }
    }

    /// The rows of `chunk` listed in `rows`.
    fn rows_of(chunk: &DataChunk, rows: Vec<u32>) -> DataChunk {
        let mut result = chunk.clone();
        let count = rows.len();
        result.slice(&SelectionVector::from_vec(rows), count);
        result
    }

    /// The columns of `chunk`, then a NULL column of each of `types`.
    fn with_nulls(chunk: DataChunk, types: &[LogicalType]) -> DataChunk {
        let mut columns = chunk.columns().to_vec();
        columns.extend(types.iter().map(|t| Vector::constant_null(t.clone())));
        DataChunk::from_columns(columns, chunk.len())
    }

    /// What the input chunk adds now that all its pairs are done (`NextLeftJoin` and friends).
    fn finish_probe(&self, input: &DataChunk, probe: &Probe, data: &DataChunk) -> Result<DataChunk> {
        let found = &probe.found;
        let rows = |wanted: bool| -> Vec<u32> {
            (0..found.len() as u32)
                .filter(|&r| found[r as usize] == wanted)
                .collect()
        };
        Ok(match self.join_type {
            JoinType::Mark => {
                let count = input.len();
                let mut mark = Vector::new(LogicalType::BOOLEAN, count.max(1));
                mark.prepare_flat(count.max(1));
                // a row that found a match is true, one that compared with a NULL NULL, the others false
                for (slot, &found) in mark.data_mut::<bool>().iter_mut().zip(found.iter()) {
                    *slot = found;
                }
                for (row, (&found, &null)) in found.iter().zip(&probe.nulls).enumerate() {
                    if !found && null {
                        mark.set_null(row, true);
                    }
                }
                let mut columns = input.columns().to_vec();
                columns.push(mark);
                DataChunk::from_columns(columns, count)
            }
            JoinType::Single if data.is_empty() => Self::with_nulls(input.clone(), self.children[1].types()),
            JoinType::Single => {
                let count = input.len();
                let sel = SelectionVector::from_vec(probe.single.iter().map(|row| row.unwrap_or(0)).collect());
                let mut columns = input.columns().to_vec();
                for source in data.columns() {
                    let mut column = Vector::new(source.logical_type().clone(), count.max(1));
                    column.prepare_flat(count.max(1));
                    column.copy_from(source, &sel, count, 0, StringCopy::Deep);
                    for (row, _) in probe.single.iter().enumerate().filter(|(_, row)| row.is_none()) {
                        column.validity_mut().set_invalid(row);
                    }
                    columns.push(column);
                }
                DataChunk::from_columns(columns, count)
            }
            JoinType::Left | JoinType::Outer => {
                Self::with_nulls(Self::rows_of(input, rows(false)), self.children[1].types())
            }
            JoinType::Semi => Self::rows_of(input, rows(true)),
            JoinType::Anti => Self::rows_of(input, rows(false)),
            _ => empty_chunk(),
        })
    }
}

impl PhysicalOperator for PhysicalJoin {
    fn explain_details(&self) -> Vec<String> {
        vec![format!("Join Type: {}", self.join_type.name())]
    }
    fn name(&self) -> &'static str {
        if let Some(name) = self.operator_name {
            name
        } else if self.conditions.is_empty() {
            "BLOCKWISE_NL_JOIN"
        } else {
            "HASH_JOIN"
        }
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn is_sink(&self) -> bool {
        true
    }
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let keys = self.build_keys(chunk, false)?;
        self.append(&mut self.build.lock(), chunk, keys, false)
    }
    fn parallel_sink(&self) -> bool {
        self.conditions
            .iter()
            .chain(&self.range)
            .all(|c| parallel_expression(&c.right))
    }
    fn local_sink_state(&self) -> LocalSinkState {
        self.builders.fetch_add(1, Ordering::Relaxed);
        self.build.lock().partitioned = true;
        Box::new(LocalBuild::new())
    }
    fn sink_local(&self, local: &mut LocalSinkState, _batch: usize, chunk: &DataChunk) -> Result<()> {
        let keys = self.build_keys(chunk, true)?;
        let local: &mut LocalBuild = local.downcast_mut().expect("join local state");
        local.push((chunk.clone(), keys));
        // a join that can spill counts its rows only once they are in, so they come in often
        if self.spill.is_some() && local.len() >= LOCAL_BUILD_CHUNKS {
            let mut build = self.build.lock();
            for (chunk, keys) in local.drain(..) {
                self.append(&mut build, &chunk, keys, true)?;
            }
        }
        Ok(())
    }
    fn combine_local(&self, local: LocalSinkState) -> Result<()> {
        let local = *local.downcast::<LocalBuild>().expect("join local state");
        let mut build = self.build.lock();
        for (chunk, keys) in local {
            self.append(&mut build, &chunk, keys, true)?;
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut build = self.build.lock();
        let builders = self.builders.load(Ordering::Relaxed);
        let threads = if build.count < MIN_PARALLEL_BUILD_ROWS {
            1
        } else {
            builders
        };
        let total_count = build.count + build.spilled_rows;
        let has_null_key = build.has_null_key;
        if let Some(spill) = &self.spill {
            *spill.build_files.lock() = spill.build.take()?;
        }
        let complete = build.spilled_rows == 0;
        let mut finished = Self::finish_build(&mut build, threads, total_count, has_null_key);
        if !self.range.is_empty() {
            finished.range = Some(self.range_index(&finished.data, finished.count)?);
        }
        // `CreateDynamicMinMaxFilters`: the smallest or largest key of the build side, where it is all there
        for (key, bound) in &self.bounds {
            if !complete || finished.count == 0 || key.is_volatile() {
                continue;
            }
            let Ok(keys) = crate::execution::expression_executor::execute(key, &finished.data) else {
                continue;
            };
            let lower = bound.comparison == duckrs_parser::ast::ComparisonOp::GreaterThanOrEqual;
            let mut extreme: Option<Value> = None;
            for row in 0..finished.count {
                let value = keys.get_value(row);
                if value.is_null() {
                    continue;
                }
                let op = if lower {
                    duckrs_parser::ast::ComparisonOp::LessThan
                } else {
                    duckrs_parser::ast::ComparisonOp::GreaterThan
                };
                if extreme
                    .as_ref()
                    .is_none_or(|e| crate::optimizer::statistics::compare_values(op, &value, e) == Some(true))
                {
                    extreme = Some(value);
                }
            }
            if let Some(extreme) = extreme {
                bound.set(extreme);
            }
        }
        for condition in &self.conditions {
            if let Some(filter) = &condition.filter {
                match condition.right.kind {
                    BoundExprKind::Reference(column) => {
                        filter.set(finished.data.column(column), finished.count, complete)
                    }
                    // a key that is computed, of the build rows (`CAST(b AS INTEGER)`), which computing again
                    // must not change
                    _ if condition.right.is_volatile() => filter.set(&Vector::constant(&Value::boolean(true)), 0, false),
                    _ => match crate::execution::expression_executor::execute(&condition.right, &finished.data) {
                        Ok(keys) => filter.set(&keys, finished.count, complete),
                        Err(_) => filter.set(&Vector::constant(&Value::boolean(true)), 0, false),
                    },
                }
            }
        }
        self.finished
            .set(finished)
            .map_err(|_| Error::new(ErrorKind::Internal, "a join build side was finished twice"))
    }
    fn parallel_operator(&self) -> bool {
        self.conditions
            .iter()
            .chain(&self.range)
            .all(|c| parallel_expression(&c.left))
            && self.predicate.as_ref().is_none_or(parallel_expression)
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(JoinProbeState::default())
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let state = state.downcast_mut::<JoinProbeState>().expect("join state");
        self.probe_with(self.finished(), input, state, true)
    }
    fn has_unmatched_source(&self) -> bool {
        // a join that can spill may have partitions to join after the probe
        self.emits_build_rows() || self.spill.is_some()
    }
    fn source_state(&self) -> SourceState {
        Box::new(JoinSource::default())
    }
    /// The build rows nothing matched, or for RIGHT SEMI the ones something did, and then what
    /// the spilled partitions give.
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let source = state.downcast_mut::<JoinSource>().expect("join source state");
        if !source.memory_done {
            if self.emits_build_rows()
                && let Some(chunk) = self.build_rows_chunk(self.finished(), &mut source.next)
            {
                return Ok(Some(chunk));
            }
            source.memory_done = true;
        }
        self.next_spilled(source)
    }
}
