// Ported from duckdb/src/common/sort/{sort,sorted_run_merger}.cpp @ rust-port-baseline
//
//! ORDER BY past `memory_limit`. Rows gather in memory as long as [`QueryMemory`] gives them
//! room; when it does not, they are sorted into a run that goes to a temporary file. At the end
//! the runs and what is still in memory are merged: the smallest key of the runs' next rows
//! comes first, and of equal keys the one of the earlier run, so ties keep the order they came in,
//! as the in-memory sort does. More runs than [`MAX_MERGE_WIDTH`] are merged into fewer first.

use super::temporary::{QueryMemory, SpillFile, SpillReader, current, estimated_bytes};
use crate::storage::log_writer::can_serialize;
use duckrs_common::Result;
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{OrderModifiers, create_sort_keys, sort_collection};
use duckrs_common::types::LogicalType;
use duckrs_common::vector::{DataChunk, SelectionVector, StringCopy, Vector};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

/// A run has at least this many rows, however little memory there is, so that there are not
/// too many of them.
const MIN_RUN_ROWS: usize = 16 * STANDARD_VECTOR_SIZE;

/// How many runs are merged at once.
const MAX_MERGE_WIDTH: usize = 32;

/// The rows of a sort that does not fit in memory.
#[derive(Debug, Default)]
pub struct SortSpill {
    runs: Vec<SpillFile>,
    /// what the rows in memory took of the limit
    reserved: usize,
    /// the merge of the runs, once the input is done
    merge: Option<Merge>,
}

impl SortSpill {
    /// Whether rows of these types can spill.
    pub fn can_spill(types: &[LogicalType]) -> bool {
        can_serialize(types)
    }

    /// `data` just took `chunk`: spills it as a run if the memory limit says so.
    pub fn sink(
        &mut self,
        data: &mut ColumnDataCollection,
        chunk: &DataChunk,
        orders: &[(usize, OrderModifiers)],
    ) -> Result<()> {
        let memory = current();
        let bytes = estimated_bytes(chunk);
        if memory.try_reserve(bytes) {
            self.reserved += bytes;
            return Ok(());
        }
        if data.len() < MIN_RUN_ROWS {
            return Ok(());
        }
        let sorted = sort_collection(data, orders)?;
        *data = ColumnDataCollection::new(data.types().to_vec());
        let mut run = memory.spill_file()?;
        for chunk in sorted.chunks() {
            run.write(chunk)?;
        }
        self.runs.push(run);
        memory.release(std::mem::take(&mut self.reserved));
        Ok(())
    }

    /// Whether any rows went to a run, so that the output comes from [`Self::next_chunk`].
    pub fn spilled(&self) -> bool {
        !self.runs.is_empty() || self.merge.is_some()
    }

    /// Sorts what is still in memory, and starts merging it with the runs.
    pub fn finish(&mut self, data: ColumnDataCollection, orders: &[(usize, OrderModifiers)]) -> Result<()> {
        let memory = current();
        let types = data.types().to_vec();
        let mut runs: Vec<RunInput> = Vec::new();
        for run in std::mem::take(&mut self.runs) {
            runs.push(RunInput::File(run.into_reader()?));
        }
        // the earlier runs into one, until few enough are left; the rows in memory came last
        while runs.len() > MAX_MERGE_WIDTH {
            let rest = runs.split_off(MAX_MERGE_WIDTH);
            let mut merge = Merge::new(std::mem::replace(&mut runs, rest), &types, orders)?;
            let mut merged = memory.spill_file()?;
            while let Some(chunk) = merge.next_chunk()? {
                merged.write(&chunk)?;
            }
            runs.insert(0, RunInput::File(merged.into_reader()?));
        }
        runs.push(RunInput::Memory {
            data: sort_collection(&data, orders)?,
            next: 0,
        });
        self.merge = Some(Merge::new(runs, &types, orders)?);
        self.release(&memory);
        Ok(())
    }

    /// The next chunk of the sorted rows.
    pub fn next_chunk(&mut self) -> Result<Option<DataChunk>> {
        match &mut self.merge {
            Some(merge) => merge.next_chunk(),
            None => Ok(None),
        }
    }

    fn release(&mut self, memory: &Arc<QueryMemory>) {
        memory.release(std::mem::take(&mut self.reserved));
    }
}

/// Where the rows of a run come from.
#[derive(Debug)]
enum RunInput {
    File(SpillReader),
    Memory { data: ColumnDataCollection, next: usize },
}

impl RunInput {
    fn next_chunk(&mut self) -> Result<Option<DataChunk>> {
        match self {
            Self::File(reader) => reader.next_chunk(),
            Self::Memory { data, next } => {
                let chunk = (*next < data.chunk_count()).then(|| data.chunk(*next).clone());
                *next += 1;
                Ok(chunk)
            }
        }
    }
}

/// A run being merged: its current chunk, the sort keys of its rows, and the next row.
#[derive(Debug)]
struct Source {
    input: RunInput,
    chunk: DataChunk,
    keys: Vec<Vec<u8>>,
    row: usize,
}

/// `SortedRunMerger`: merges runs of rows sorted by the same keys.
#[derive(Debug)]
struct Merge {
    sources: Vec<Source>,
    /// the key of each source's next row, smallest first, then the earliest source
    heap: BinaryHeap<Reverse<(Vec<u8>, usize)>>,
    types: Vec<LogicalType>,
    orders: Vec<(usize, OrderModifiers)>,
}

impl Merge {
    fn new(inputs: Vec<RunInput>, types: &[LogicalType], orders: &[(usize, OrderModifiers)]) -> Result<Self> {
        let mut merge = Self {
            sources: Vec::with_capacity(inputs.len()),
            heap: BinaryHeap::new(),
            types: types.to_vec(),
            orders: orders.to_vec(),
        };
        for input in inputs {
            let index = merge.sources.len();
            merge.sources.push(Source {
                input,
                chunk: DataChunk::new(types),
                keys: Vec::new(),
                row: 0,
            });
            merge.advance(index)?;
        }
        Ok(merge)
    }

    /// Puts the next row of source `index` on the heap, reading its next chunk when it needs to.
    fn advance(&mut self, index: usize) -> Result<()> {
        let source = &mut self.sources[index];
        while source.row >= source.keys.len() {
            let Some(chunk) = source.input.next_chunk()? else {
                return Ok(());
            };
            let columns: Vec<(&Vector, OrderModifiers)> =
                self.orders.iter().map(|(i, m)| (chunk.column(*i), *m)).collect();
            source.keys = create_sort_keys(&columns, chunk.len())?;
            source.chunk = chunk;
            source.row = 0;
        }
        let key = std::mem::take(&mut source.keys[source.row]);
        self.heap.push(Reverse((key, index)));
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<DataChunk>> {
        let mut out = DataChunk::with_capacity(&self.types, STANDARD_VECTOR_SIZE);
        let mut count = 0;
        while count < STANDARD_VECTOR_SIZE {
            let Some(Reverse((_, index))) = self.heap.pop() else {
                break;
            };
            let source = &mut self.sources[index];
            let sel = SelectionVector::from_vec(vec![source.row as u32]);
            for (target, column) in out.columns_mut().iter_mut().zip(source.chunk.columns()) {
                target.copy_from(column, &sel, 1, count, StringCopy::Deep);
            }
            source.row += 1;
            count += 1;
            self.advance(index)?;
        }
        if count == 0 {
            return Ok(None);
        }
        out.set_len(count);
        Ok(Some(out))
    }
}
