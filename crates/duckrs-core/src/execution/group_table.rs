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
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::vector::{DataChunk, SelectionVector};

const SALT_SHIFT: u32 = 48;
const ID_MASK: u64 = (1 << SALT_SHIFT) - 1;

/// The entries a table starts with (`GroupedAggregateHashTable::InitialCapacity`).
const INITIAL_CAPACITY: usize = 4096;

/// The groups of an aggregate: their values in rows, their hashes, and a table of entries that finds them.
#[derive(Debug)]
pub struct GroupTable {
    data: TupleDataCollection,
    /// by the id of a group, its row and its hash
    rows: Vec<RowPtr>,
    hashes: Vec<u64>,
    entries: Vec<u64>,
    matcher: RowMatcher,
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
        })
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
        let mut ids = vec![0usize; count];
        let mut slots: Vec<usize> = hashes.iter().map(|&hash| hash as usize & mask).collect();
        let mut remaining: Vec<u32> = (0..count as u32).collect();
        let mut new_rows: Vec<u32> = Vec::new();
        // the row each row is compared with, by the row of `groups`; those not compared are never read
        let mut candidates: Vec<RowPtr> = Vec::new();
        let mut candidate_ids = vec![0usize; count];
        while !remaining.is_empty() {
            let mut compare: Vec<u32> = Vec::new();
            let mut compare_ids: Vec<(u32, usize)> = Vec::new();
            let first_new = new_rows.len();
            for &row in &remaining {
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
                ids[row as usize] = candidate_ids[row as usize];
            }
            // those that differ go on from the next entry
            for &row in &missed {
                slots[row as usize] = (slots[row as usize] + 1) & mask;
            }
            remaining = missed;
        }
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
            for &row in &remaining {
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
