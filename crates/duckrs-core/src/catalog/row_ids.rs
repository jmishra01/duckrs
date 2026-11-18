//! Ported in spirit from duckdb/src/storage/table/row_group_collection.cpp (`next_row_id`) and
//! row_group.cpp (`GetRowStart`) @ rust-port-baseline.
//!
//! A row keeps its id from when it comes until it goes, and ids aren't given out twice, so they
//! have gaps where C++ has deleted rows or dropped row groups. Rows are kept in the order of their
//! ids, which makes this a list of runs of rows that follow each other in both ways.

use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::vector::Vector;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `MAX_ROW_ID`: rows that only a transaction has so far are numbered from here (`LocalStorage`).
pub const MAX_ROW_ID: u64 = 1 << 55;

#[derive(Debug, Clone, Copy)]
struct Run {
    position: usize,
    id: u64,
}

/// The id of each row of a table, by position, and the id that the next row gets.
#[derive(Debug, Clone, Default)]
pub struct RowIds {
    runs: Vec<Run>,
    rows: usize,
    next: u64,
    /// The row groups that a file has, by first id and rows, deleted rows included. What came
    /// after them goes in row groups of its own.
    stored_groups: Arc<Vec<(u64, usize)>>,
}

impl RowIds {
    /// No rows, of which the first will get `next`.
    pub fn starting_at(next: u64) -> Self {
        Self {
            next,
            ..Self::default()
        }
    }

    /// Rows with these ranges of ids, in order, and `next` for the one after them.
    pub fn of_ranges(ranges: impl IntoIterator<Item = (u64, usize)>, next: u64) -> Self {
        let mut ids = Self::starting_at(next);
        for (first, count) in ranges {
            ids.push(first, count);
        }
        ids
    }

    pub fn with_stored_groups(mut self, groups: Vec<(u64, usize)>) -> Self {
        self.stored_groups = Arc::new(groups);
        self
    }

    pub fn stored_groups(&self) -> &[(u64, usize)] {
        &self.stored_groups
    }

    /// The rows of a list of ids in order.
    pub fn of_list(list: &[u64], next: u64) -> Self {
        Self::of_ranges(list.iter().map(|id| (*id, 1)), next)
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Whether rows have gone since these ids were given out.
    pub fn has_gaps(&self) -> bool {
        self.rows as u64 != self.next
    }

    /// The id that the next row gets.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Rows `first` and on, as ranges of ids.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        self.runs.iter().enumerate().map(|(index, run)| {
            let end = self.runs.get(index + 1).map_or(self.rows, |after| after.position);
            (run.id, end - run.position)
        })
    }

    fn push(&mut self, id: u64, count: usize) {
        if count == 0 {
            return;
        }
        match self.runs.last() {
            Some(last) if last.id + (self.rows - last.position) as u64 == id => {}
            _ => self.runs.push(Run {
                position: self.rows,
                id,
            }),
        }
        self.rows += count;
        self.next = self.next.max(id + count as u64);
    }

    /// There are `count` rows now: those that weren't known came at the end.
    pub fn fill(&mut self, count: usize) {
        if count > self.rows {
            self.push(self.next, count - self.rows);
        }
    }

    /// The id of the row at `position`.
    pub fn id(&self, position: usize) -> u64 {
        let run = self.runs[self.runs.partition_point(|run| run.position <= position) - 1];
        run.id + (position - run.position) as u64
    }

    /// Where the row with this id is, if there is one.
    pub fn position(&self, id: u64) -> Option<usize> {
        let index = self.runs.partition_point(|run| run.id <= id).checked_sub(1)?;
        let run = self.runs[index];
        let end = self.runs.get(index + 1).map_or(self.rows, |after| after.position);
        let position = run.position + usize::try_from(id - run.id).ok()?;
        (position < end).then_some(position)
    }

    /// Where the first row is whose id is at least `id`.
    pub fn first_from(&self, id: u64) -> usize {
        let index = self.runs.partition_point(|run| run.id < id);
        let Some(before) = index.checked_sub(1) else { return 0 };
        let run = self.runs[before];
        let end = self.runs.get(index).map_or(self.rows, |after| after.position);
        (run.position + usize::try_from(id - run.id).unwrap_or(usize::MAX)).min(end)
    }

    /// The rows at the positions in `removed`, in order, go.
    pub fn remove(&mut self, removed: &[usize]) {
        let mut kept = Self::starting_at(self.next);
        kept.stored_groups = self.stored_groups.clone();
        let mut gone = removed.iter().copied().peekable();
        for index in 0..self.runs.len() {
            let run = self.runs[index];
            let end = self.runs.get(index + 1).map_or(self.rows, |after| after.position);
            let mut at = run.position;
            while at < end {
                while at < end && gone.peek() == Some(&at) {
                    gone.next();
                    at += 1;
                }
                let stop = gone.peek().copied().unwrap_or(end).min(end);
                if at < stop {
                    kept.push(run.id + (at - run.position) as u64, stop - at);
                }
                at = stop.max(at);
            }
        }
        *self = kept;
    }

    /// The rows from `rows` on never were: their ids are free again (`RevertAppend`).
    pub fn truncate(&mut self, rows: usize) {
        if rows >= self.rows {
            return;
        }
        self.next = self.id(rows);
        let kept = self.runs.partition_point(|run| run.position < rows);
        self.runs.truncate(kept);
        self.rows = rows;
    }

    /// Where the rows are that aren't in `other`, which has no rows that this hasn't.
    pub fn missing_from(&self, other: &Self) -> Vec<usize> {
        let mut missing = Vec::new();
        let mut position = 0;
        for (first, count) in self.ranges() {
            let mut id = first;
            let end = first + count as u64;
            while id < end {
                match other.position(id) {
                    // the rest of a run of `other` needn't be looked up
                    Some(found) => {
                        let index = other.runs.partition_point(|run| run.position <= found);
                        let run_end = other.runs.get(index).map_or(other.rows, |after| after.position);
                        id = end.min(id + (run_end - found) as u64);
                    }
                    None => {
                        missing.push(position + (id - first) as usize);
                        id += 1;
                    }
                }
            }
            position += count;
        }
        missing
    }

    /// What a transaction sees of these rows: those it adds get ids of their own.
    pub fn for_transaction(&self) -> Self {
        Self {
            next: self.next.max(MAX_ROW_ID),
            ..self.clone()
        }
    }

    /// The rows that only a transaction has, numbered from `next` now that it commits.
    pub fn committed(&self, next: u64) -> Self {
        let mut result = Self::starting_at(next);
        result.stored_groups = self.stored_groups.clone();
        for (id, count) in self.ranges() {
            let id = if id >= MAX_ROW_ID { result.next } else { id };
            result.push(id, count);
        }
        result
    }

    /// After the rows that aren't of a transaction.
    pub fn next_committed(&self) -> u64 {
        if self.next < MAX_ROW_ID {
            return self.next;
        }
        self.ranges()
            .filter(|(id, _)| *id < MAX_ROW_ID)
            .last()
            .map_or(0, |(id, count)| id + count as u64)
    }

    /// The ids of `count` rows from `first` on, of which there can be more than this knows: those
    /// are the rows that a statement adds, past what it reads.
    pub fn vector(&self, first: usize, count: usize) -> Result<Vector> {
        let index = self.runs.partition_point(|run| run.position <= first);
        let dense = match index.checked_sub(1) {
            Some(run) => self
                .runs
                .get(index)
                .is_none_or(|after| after.position >= first + count)
                .then(|| self.runs[run].id + (first - self.runs[run].position) as u64),
            None => None,
        };
        if let Some(start) = dense {
            return Vector::sequence(LogicalType::BIGINT, start as i64, 1);
        }
        let mut vector = Vector::new(LogicalType::BIGINT, count.max(1));
        for (row, target) in vector.data_mut::<i64>().iter_mut().take(count).enumerate() {
            *target = if first + row < self.rows {
                self.id(first + row) as i64
            } else {
                (self.next + (first + row - self.rows) as u64) as i64
            };
        }
        Ok(vector)
    }
}

/// A set of ids, for what tends to come in long runs.
#[derive(Debug, Default, Clone)]
pub struct IdSet {
    /// The last id of each run by its first.
    runs: BTreeMap<u64, u64>,
}

impl IdSet {
    pub fn insert(&mut self, id: u64) {
        let mut first = id;
        let mut last = id;
        if let Some((&start, &end)) = self.runs.range(..=id).next_back() {
            if id <= end {
                return;
            }
            if end + 1 == id {
                first = start;
            }
        }
        if let Some(end) = self.runs.remove(&(id + 1)) {
            last = end;
        }
        self.runs.insert(first, last);
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn clear(&mut self) {
        self.runs.clear();
    }

    /// All ids in order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.runs.iter().flat_map(|(first, last)| *first..=*last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn list(ids: &RowIds) -> Vec<u64> {
        ids.ranges().flat_map(|(id, count)| id..id + count as u64).collect()
    }

    #[test]
    fn ids_stay_as_rows_go() {
        let mut ids = RowIds::default();
        ids.fill(10);
        ids.remove(&[0, 3, 4, 9]);
        assert_eq!(list(&ids), [1, 2, 5, 6, 7, 8]);
        ids.fill(8);
        assert_eq!(list(&ids), [1, 2, 5, 6, 7, 8, 10, 11]);
        assert_eq!(ids.position(5), Some(2));
        assert_eq!(ids.position(3), None);
        assert_eq!(ids.position(0), None);
        assert_eq!(ids.position(12), None);
        assert_eq!(ids.first_from(3), 2);
        assert_eq!(ids.first_from(100), 8);
    }

    #[test]
    fn a_set_of_ids_keeps_runs() {
        let mut set = IdSet::default();
        for id in [5, 3, 4, 9, 4, 7] {
            set.insert(id);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), [3, 4, 5, 7, 9]);
        assert_eq!(set.runs.len(), 3);
        set.insert(8);
        set.insert(6);
        assert_eq!(set.runs.len(), 1);
    }

    #[test]
    fn what_is_missing_is_found_by_position() {
        let mut old = RowIds::default();
        old.fill(10);
        old.remove(&[2]);
        let mut new = old.clone();
        new.remove(&[0, 4, 8]);
        new.fill(8);
        assert_eq!(old.missing_from(&new), [0, 4, 8]);
    }

    #[test]
    fn a_failed_append_gives_its_ids_back() {
        let mut ids = RowIds::default();
        ids.fill(5);
        ids.remove(&[4]);
        ids.fill(7);
        ids.truncate(4);
        assert_eq!(ids.next(), 5);
        ids.fill(5);
        assert_eq!(list(&ids), [0, 1, 2, 3, 5]);
    }

    #[test]
    fn a_commit_numbers_what_is_left_of_a_transaction() {
        let mut base = RowIds::default();
        base.fill(4);
        let mut local = base.for_transaction();
        local.fill(7);
        local.remove(&[1, 5]);
        assert_eq!(local.next_committed(), 4);
        assert_eq!(list(&local.committed(9)), [0, 2, 3, 9, 10]);
        assert_eq!(local.committed(9).next(), 11);
    }

    proptest! {
        #[test]
        fn acts_like_a_list(
            steps in proptest::collection::vec((1usize..40, proptest::collection::vec(any::<prop::sample::Index>(), 0..20)), 1..12),
        ) {
            let mut ids = RowIds::default();
            let mut model: Vec<u64> = Vec::new();
            let mut next = 0;
            for (added, gone) in steps {
                model.extend(next..next + added as u64);
                next += added as u64;
                ids.fill(model.len());
                let mut removed: Vec<usize> = gone.iter().map(|index| index.index(model.len())).collect();
                removed.sort_unstable();
                removed.dedup();
                ids.remove(&removed);
                for position in removed.iter().rev() {
                    model.remove(*position);
                }
                prop_assert_eq!(&list(&ids), &model);
                prop_assert_eq!(ids.next(), next);
                for (position, id) in model.iter().enumerate() {
                    prop_assert_eq!(ids.id(position), *id);
                    prop_assert_eq!(ids.position(*id), Some(position));
                }
                for id in 0..next + 2 {
                    prop_assert_eq!(ids.position(id), model.binary_search(&id).ok());
                    prop_assert_eq!(ids.first_from(id), model.partition_point(|row| *row < id));
                }
                if !model.is_empty() {
                    let vector = ids.vector(0, model.len()).unwrap();
                    for (position, id) in model.iter().enumerate() {
                        prop_assert_eq!(vector.get_value(position).as_i128(), Some(i128::from(*id)));
                    }
                }
            }
        }
    }
}
