// Ported from duckdb/src/execution/aggregate_hashtable.cpp (`GroupedAggregateHashTable`) and
// duckdb/src/include/duckdb/execution/ht_entry.hpp @ rust-port-baseline
//
//! The groups of an aggregate in a hash table over rows of the row format: a row's groups are found by the hash
//! of their columns and a comparison of the columns with the rows of the table, so no key of all of them is made.
//! The states of the aggregates stay apart, by the id of a group (its place in the order groups came in).
//!
//! An entry is C++'s `ht_entry_t`: the upper 16 bits of the hash as a salt, and here the group's id plus one in
//! the lower 48 bits, where C++ has a pointer to the row; 0 is an empty entry.

use duckrs_common::Result;
use duckrs_common::comparison::ComparisonType;
use duckrs_common::hash::{combine_hash, hash_vector};
use duckrs_common::row::{RowMatcher, RowPtr, RowValidity, TupleDataCollection, TupleDataLayout};
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::vector::{DataChunk, SelectionVector};

use crate::planner::logical::PerfectHashGroup;

const SALT_SHIFT: u32 = 48;
const ID_MASK: u64 = (1 << SALT_SHIFT) - 1;

/// The entries a table starts with (`GroupedAggregateHashTable::InitialCapacity`).
const INITIAL_CAPACITY: usize = 4096;

/// The most bits the slots of a [`DirectIndex`] take, so that its ids stay a few megabytes at most.
const MAX_DIRECT_BITS: u32 = 20;

/// How many rows ahead a lookup asks for the entry it will read, so that the reads of a table larger than the
/// caches overlap rather than wait one by one.
const PREFETCH_DISTANCE: usize = 32;

/// The groups of an aggregate: their values in rows, their hashes, and a table of entries that finds them.
#[derive(Debug)]
pub struct GroupTable {
    data: TupleDataCollection,
    /// by the id of a group, its row and its hash
    rows: Vec<RowPtr>,
    hashes: Vec<u64>,
    entries: Vec<u64>,
    matcher: RowMatcher,
    /// what [`Self::find_or_create`] works in, kept so that a chunk allocates none of it
    scratch: Scratch,
    /// for groups of small integer ranges, the groups by the slot their values make
    direct: Option<DirectIndex>,
}

/// The groups of a table by the offsets of their values from the minima of their ranges, side by side in the bits
/// of a slot (`PhysicalPerfectHashAggregate`): a row whose values are in their ranges finds its group by its slot,
/// with no hash and no comparison. It has only ids the table gave out, so a slot without one, or a row out of the
/// ranges, finds its group in the table as any row does.
#[derive(Debug)]
struct DirectIndex {
    ranges: Vec<PerfectHashGroup>,
    /// by slot, the id of its group plus one; 0 for a slot that has none yet
    ids: Vec<u32>,
}

/// No slot: a row with a value out of its range.
const NO_SLOT: u32 = u32::MAX;

impl DirectIndex {
    /// The slot of each row of `groups`, or [`NO_SLOT`]: per column `bits` bits of the value's offset from the
    /// minimum plus one, where 0 is NULL.
    fn slots(&self, groups: &DataChunk) -> Vec<u32> {
        let count = groups.len();
        let mut slots = vec![0u32; count];
        let mut shift = 0;
        for (column, range) in groups.columns().iter().zip(&self.ranges) {
            let format = column.to_unified(count);
            // the values offsets 1 to `2^bits - 1` stand for, in the column's type: an offset is the difference
            // of two of them, which the type holds
            let last = range.min + (1i128 << range.bits) - 2;
            macro_rules! offsets {
                ($t:ty) => {{
                    let data = format.data::<$t>();
                    let Ok(min) = <$t>::try_from(range.min) else {
                        return vec![NO_SLOT; count];
                    };
                    let max = <$t>::try_from(last).unwrap_or(<$t>::MAX);
                    let all_valid = format.validity.all_valid();
                    for (row, slot) in slots.iter_mut().enumerate() {
                        let index = format.sel.get(row);
                        if !all_valid && !format.validity.is_valid(index) {
                            continue;
                        }
                        let value = data[index];
                        *slot = if (min..=max).contains(&value) {
                            // under 2^20 apart, so their difference in 64 bits is exact, wrapped or not
                            *slot | (((value as i64).wrapping_sub(min as i64) as u32 + 1) << shift)
                        } else {
                            NO_SLOT
                        };
                    }
                }};
            }
            match column.physical_type() {
                PhysicalType::Int8 => offsets!(i8),
                PhysicalType::Int16 => offsets!(i16),
                PhysicalType::Int32 => offsets!(i32),
                PhysicalType::Int64 => offsets!(i64),
                PhysicalType::UInt8 => offsets!(u8),
                PhysicalType::UInt16 => offsets!(u16),
                PhysicalType::UInt32 => offsets!(u32),
                PhysicalType::UInt64 => offsets!(u64),
                _ => return vec![NO_SLOT; count],
            }
            shift += range.bits;
        }
        slots
    }
}

/// The buffers of a lookup, by row of the chunk or of the rows still looked for.
#[derive(Debug, Default)]
struct Scratch {
    slots: Vec<usize>,
    remaining: Vec<u32>,
    compare: Vec<u32>,
    compare_ids: Vec<(u32, usize)>,
    missed: Vec<u32>,
    candidates: Vec<RowPtr>,
    candidate_ids: Vec<usize>,
}

impl GroupTable {
    /// Whether groups of these types are what equal keys of `create_row_keys` are: a VARIANT is equal by what it
    /// means and a BIT by its bits, which comparing their bytes doesn't tell, so they keep the keys.
    pub fn supports(types: &[LogicalType]) -> bool {
        fn supported(ty: &LogicalType) -> bool {
            match ty.id() {
                LogicalTypeId::Variant | LogicalTypeId::Bit => false,
                LogicalTypeId::Struct => ty.struct_children().iter().all(|(_, child)| supported(child)),
                _ => ty.child_type().is_none_or(supported),
            }
        }
        !types.is_empty() && types.iter().all(supported)
    }

    pub fn new(types: Vec<LogicalType>) -> Result<Self> {
        let layout = TupleDataLayout::new(types, Vec::new(), RowValidity::CanHaveNull);
        let matcher = RowMatcher::new(&layout, vec![ComparisonType::NotDistinctFrom; layout.column_count()])?;
        Ok(Self {
            data: TupleDataCollection::new(layout)?,
            rows: Vec::new(),
            hashes: Vec::new(),
            entries: vec![0; INITIAL_CAPACITY],
            matcher,
            scratch: Scratch::default(),
            direct: None,
        })
    }

    /// Finds groups by their slot in `ranges` from here on, in [`Self::find_or_create_direct`]: for integer groups
    /// with a range each, whose slots take few enough bits. Nothing changes for a table that does already, or whose
    /// groups are not such.
    pub fn enable_direct(&mut self, ranges: &[PerfectHashGroup]) {
        if self.direct.is_some() {
            return;
        }
        let types = self.data.layout().types();
        let bits: u32 = ranges.iter().map(|range| range.bits).sum();
        let integers = types.iter().all(|ty| {
            matches!(
                ty.physical_type(),
                PhysicalType::Int8
                    | PhysicalType::Int16
                    | PhysicalType::Int32
                    | PhysicalType::Int64
                    | PhysicalType::UInt8
                    | PhysicalType::UInt16
                    | PhysicalType::UInt32
                    | PhysicalType::UInt64
            )
        });
        if ranges.len() != types.len() || bits > MAX_DIRECT_BITS || !integers {
            return;
        }
        self.direct = Some(DirectIndex {
            ranges: ranges.to_vec(),
            ids: vec![0; 1 << bits],
        });
    }

    /// [`Self::find_or_create`] for a table with a [`DirectIndex`]: rows whose slot has a group have it, and the
    /// others, hashed, look for theirs in the table, whose slots then have them. `None` for a table without one.
    pub fn find_or_create_direct(&mut self, groups: &DataChunk) -> Result<Option<(Vec<usize>, Vec<u32>)>> {
        let Some(direct) = &self.direct else {
            return Ok(None);
        };
        let count = groups.len();
        let slots = direct.slots(groups);
        let mut ids = vec![0usize; count];
        let mut misses: Vec<u32> = Vec::new();
        for (row, &slot) in slots.iter().enumerate() {
            match direct.ids.get(slot as usize) {
                Some(&id) if id != 0 => ids[row] = id as usize - 1,
                _ => misses.push(row as u32),
            }
        }
        if misses.is_empty() {
            return Ok(Some((ids, Vec::new())));
        }
        let mut missed = groups.clone();
        missed.slice(&SelectionVector::from_vec(misses.clone()), misses.len());
        let hashes = Self::hash(&missed)?;
        let (found, new_rows) = self.find_or_create(&missed, &hashes)?;
        let direct = self.direct.as_mut().expect("checked above");
        for (&row, &id) in misses.iter().zip(&found) {
            ids[row as usize] = id;
            if let (Some(entry), Ok(id)) = (direct.ids.get_mut(slots[row as usize] as usize), u32::try_from(id + 1)) {
                *entry = id;
            }
        }
        let new_rows = new_rows.into_iter().map(|index| misses[index as usize]).collect();
        Ok(Some((ids, new_rows)))
    }

    /// Room for `groups` groups in all, so that a table that is to get them grows once rather than doubling its
    /// way there (`GroupedAggregateHashTable::Resize` to the size of a partition).
    pub fn reserve(&mut self, groups: usize) {
        self.resize(groups * 2);
        self.rows.reserve(groups.saturating_sub(self.rows.len()));
        self.hashes.reserve(groups.saturating_sub(self.hashes.len()));
    }

    /// How many groups it has.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// What it takes of memory (`GetSizeInBytes`): its rows and their strings, the entries, and per group its row
    /// pointer and hash.
    pub fn size_in_bytes(&self) -> usize {
        self.data.size_in_bytes() + (self.entries.len() + 2 * self.rows.len()) * std::mem::size_of::<u64>()
    }

    /// The hash of the groups of each row of `groups` (`VectorOperations::Hash` and `CombineHash`).
    pub fn hash(groups: &DataChunk) -> Result<Vec<u64>> {
        let count = groups.len();
        let mut columns = groups.columns().iter();
        let first = columns.next().expect("a group column");
        let mut hashes = hash_vector(first, count)?;
        for column in columns {
            combine_hash(&mut hashes, column, count)?;
        }
        hashes.flatten(count);
        Ok(hashes.data::<u64>()[..count].to_vec())
    }

    fn mask(&self) -> usize {
        self.entries.len() - 1
    }

    fn salt(hash: u64) -> u64 {
        hash >> SALT_SHIFT
    }

    /// The group of each row of `groups`, whose hashes are `hashes`, made where there is none yet
    /// (`FindOrCreateGroupsInternal`); and the rows that made one, in the order of their ids.
    pub fn find_or_create(&mut self, groups: &DataChunk, hashes: &[u64]) -> Result<(Vec<usize>, Vec<u32>)> {
        let count = groups.len();
        // room for every row to be new, at a load of at most a half (`ResizeThreshold`)
        if (self.rows.len() + count) * 2 > self.entries.len() {
            self.resize((self.rows.len() + count) * 2);
        }
        let mask = self.mask();
        // room for every row to be a new group, so that pushing one never moves the hashes
        self.hashes.reserve(count);
        let mut ids = vec![0usize; count];
        let mut new_rows: Vec<u32> = Vec::with_capacity(count);
        let mut scratch = std::mem::take(&mut self.scratch);
        let Scratch {
            slots,
            remaining,
            compare,
            compare_ids,
            missed,
            candidates,
            candidate_ids,
        } = &mut scratch;
        slots.clear();
        slots.extend(hashes.iter().map(|&hash| hash as usize & mask));
        remaining.clear();
        remaining.extend(0..count as u32);
        // the row each row is compared with, by the row of `groups`; those not compared are never read
        candidates.clear();
        candidate_ids.resize(count, 0);
        while !remaining.is_empty() {
            compare.clear();
            compare_ids.clear();
            let first_new = new_rows.len();
            for (i, &row) in remaining.iter().enumerate() {
                if let Some(&ahead) = remaining.get(i + PREFETCH_DISTANCE) {
                    super::prefetch(&self.entries[slots[ahead as usize]]);
                }
                let r = row as usize;
                let salt = Self::salt(hashes[r]);
                let mut slot = slots[r];
                loop {
                    let entry = self.entries[slot];
                    if entry == 0 {
                        // an empty entry: the row is a new group, which rows after it can find at once
                        // the groups so far, those this round made included
                        let id = self.hashes.len();
                        self.entries[slot] = (salt << SALT_SHIFT) | (id as u64 + 1);
                        self.hashes.push(hashes[r]);
                        new_rows.push(row);
                        ids[r] = id;
                        break;
                    }
                    if entry >> SALT_SHIFT == salt {
                        compare.push(row);
                        compare_ids.push((row, (entry & ID_MASK) as usize - 1));
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
                slots[r] = slot;
            }
            // the rows of the new groups, once
            if new_rows.len() > first_new {
                let added = &new_rows[first_new..];
                let mut values = groups.clone();
                values.slice(&SelectionVector::from_vec(added.to_vec()), added.len());
                self.rows.extend(self.data.append(&values));
            }
            if compare.is_empty() {
                break;
            }
            if candidates.is_empty() {
                candidates.resize(count, self.rows[0]);
            }
            for &(row, id) in compare_ids.iter() {
                candidates[row as usize] = self.rows[id];
                candidate_ids[row as usize] = id;
            }
            missed.clear();
            // SAFETY: each candidate is a row of `self.data`, which is alive and not reset while `self` is
            unsafe {
                self.matcher
                    .match_rows(&self.data, groups, candidates, compare, Some(missed))
            };
            for &row in compare.iter() {
                ids[row as usize] = candidate_ids[row as usize];
            }
            // those that differ go on from the next entry
            for &row in missed.iter() {
                slots[row as usize] = (slots[row as usize] + 1) & mask;
            }
            std::mem::swap(remaining, missed);
        }
        self.scratch = scratch;
        Ok((ids, new_rows))
    }

    /// The group of each row of `groups` that has one, without making any (`LookupGroups`).
    pub fn find(&self, groups: &DataChunk, hashes: &[u64]) -> Vec<Option<usize>> {
        let count = groups.len();
        let mask = self.mask();
        let mut ids = vec![None; count];
        let mut slots: Vec<usize> = hashes.iter().map(|&hash| hash as usize & mask).collect();
        let mut remaining: Vec<u32> = (0..count as u32).collect();
        let mut candidates: Vec<RowPtr> = Vec::new();
        let mut candidate_ids = vec![0usize; count];
        while !remaining.is_empty() && !self.rows.is_empty() {
            let mut compare = Vec::new();
            let mut compare_ids = Vec::new();
            for (i, &row) in remaining.iter().enumerate() {
                if let Some(&ahead) = remaining.get(i + PREFETCH_DISTANCE) {
                    super::prefetch(&self.entries[slots[ahead as usize]]);
                }
                let r = row as usize;
                let salt = Self::salt(hashes[r]);
                let mut slot = slots[r];
                loop {
                    let entry = self.entries[slot];
                    if entry == 0 {
                        break;
                    }
                    if entry >> SALT_SHIFT == salt {
                        compare.push(row);
                        compare_ids.push((row, (entry & ID_MASK) as usize - 1));
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
                slots[r] = slot;
            }
            if compare.is_empty() {
                break;
            }
            if candidates.is_empty() {
                candidates = vec![self.rows[0]; count];
            }
            for &(row, id) in &compare_ids {
                candidates[row as usize] = self.rows[id];
                candidate_ids[row as usize] = id;
            }
            let mut matched = compare;
            let mut missed = Vec::new();
            // SAFETY: each candidate is a row of `self.data`, which is alive and not reset while `self` is
            unsafe {
                self.matcher
                    .match_rows(&self.data, groups, &candidates, &mut matched, Some(&mut missed))
            };
            for row in matched {
                ids[row as usize] = Some(candidate_ids[row as usize]);
            }
            for &row in &missed {
                slots[row as usize] = (slots[row as usize] + 1) & mask;
            }
            remaining = missed;
        }
        ids
    }

    /// A table of at least `wanted` entries, a power of two, with every group in it again (`Resize`).
    fn resize(&mut self, wanted: usize) {
        let capacity = wanted.next_power_of_two().max(INITIAL_CAPACITY);
        if capacity <= self.entries.len() {
            return;
        }
        self.entries = vec![0; capacity];
        let mask = capacity - 1;
        for (id, &hash) in self.hashes.iter().enumerate() {
            let mut slot = hash as usize & mask;
            while self.entries[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            self.entries[slot] = (Self::salt(hash) << SALT_SHIFT) | (id as u64 + 1);
        }
    }

    /// The values of the groups, in the order of their ids.
    pub fn scan(&self) -> impl Iterator<Item = DataChunk> + '_ {
        self.data.scan()
    }

    /// The hash of each group, by its id.
    pub fn hashes(&self) -> &[u64] {
        &self.hashes
    }

    /// The values of the groups `ids`, in that order (`TupleDataCollection::Gather`).
    pub fn gather(&self, ids: &[usize]) -> DataChunk {
        let rows: Vec<RowPtr> = ids.iter().map(|&id| self.rows[id]).collect();
        let types = self.data.layout().types().to_vec();
        let mut chunk = DataChunk::with_capacity(&types, rows.len().max(1));
        for column in 0..types.len() {
            // SAFETY: the rows are rows of `self.data`, alive while `self` is
            unsafe { self.data.gather(&rows, column, chunk.column_mut(column)) };
        }
        chunk.set_len(rows.len());
        chunk
    }
}

#[cfg(test)]
mod tests;
