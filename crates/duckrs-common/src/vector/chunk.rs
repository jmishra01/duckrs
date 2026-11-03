// Ported from duckdb/src/common/types/data_chunk.cpp @ rust-port-baseline

use super::{SelectionVector, Vector};
use crate::STANDARD_VECTOR_SIZE;
use crate::types::LogicalType;
use crate::value::Value;
use std::fmt;

/// A horizontal slice of a result or table: equally long column vectors (`DataChunk`).
#[derive(Debug, Clone)]
pub struct DataChunk {
    columns: Vec<Vector>,
    count: usize,
    capacity: usize,
}

impl DataChunk {
    /// A chunk with one flat column per type and room for [`STANDARD_VECTOR_SIZE`] rows.
    pub fn new(types: &[LogicalType]) -> Self {
        Self::with_capacity(types, STANDARD_VECTOR_SIZE)
    }

    pub fn with_capacity(types: &[LogicalType], capacity: usize) -> Self {
        Self {
            columns: types.iter().map(|t| Vector::new(t.clone(), capacity)).collect(),
            count: 0,
            capacity,
        }
    }

    /// A chunk over existing vectors holding `count` rows.
    pub fn from_columns(columns: Vec<Vector>, count: usize) -> Self {
        Self {
            columns,
            count,
            capacity: count.max(STANDARD_VECTOR_SIZE),
        }
    }

    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.logical_type().clone()).collect()
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Rows in the chunk (`DataChunk::size`).
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Sets the row count (`SetCardinality`).
    pub fn set_len(&mut self, count: usize) {
        assert!(count <= self.capacity, "{count} rows exceed capacity {}", self.capacity);
        self.count = count;
    }

    pub fn column(&self, i: usize) -> &Vector {
        &self.columns[i]
    }

    pub fn column_mut(&mut self, i: usize) -> &mut Vector {
        &mut self.columns[i]
    }

    pub fn columns(&self) -> &[Vector] {
        &self.columns
    }

    pub fn columns_mut(&mut self) -> &mut [Vector] {
        &mut self.columns
    }

    /// Empties the chunk and makes every column a fresh flat vector (`DataChunk::Reset`).
    pub fn reset(&mut self) {
        let capacity = self.capacity;
        for c in &mut self.columns {
            c.prepare_flat(capacity);
        }
        self.count = 0;
    }

    /// Keeps only rows `sel[0..count]` (`DataChunk::Slice`).
    pub fn slice(&mut self, sel: &SelectionVector, count: usize) {
        for c in &mut self.columns {
            c.slice(sel, count);
        }
        self.count = count;
    }

    /// Flattens every column (`DataChunk::Flatten`).
    pub fn flatten(&mut self) {
        let count = self.count;
        for c in &mut self.columns {
            c.flatten(count);
        }
    }

    pub fn get_value(&self, column: usize, row: usize) -> Value {
        self.columns[column].get_value(row)
    }

    /// Appends a row of values to a chunk whose columns are flat.
    pub fn append_row(&mut self, row: &[Value]) -> crate::Result<()> {
        assert_eq!(row.len(), self.columns.len(), "row has the wrong number of values");
        assert!(self.count < self.capacity, "chunk is full");
        for (c, v) in self.columns.iter_mut().zip(row) {
            c.set_value(self.count, v)?;
        }
        self.count += 1;
        Ok(())
    }
}

impl fmt::Display for DataChunk {
    /// A debugging dump (`DataChunk::ToString`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Chunk - [{} Columns, {} Rows]", self.columns.len(), self.count)?;
        for row in 0..self.count {
            let cells: Vec<String> = (0..self.columns.len())
                .map(|c| {
                    let v = self.get_value(c, row);
                    v.try_to_string().unwrap_or_else(|_| format!("{:?}", v.data()))
                })
                .collect();
            writeln!(f, "{}", cells.join("\t"))?;
        }
        Ok(())
    }
}
