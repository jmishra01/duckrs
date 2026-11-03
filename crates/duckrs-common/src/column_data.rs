// Ported from duckdb/src/common/types/column/column_data_collection.cpp @ rust-port-baseline
//
// This is the in-memory allocator variant (`ColumnDataAllocatorType::IN_MEMORY_ALLOCATOR`).
// Spilling through the buffer manager comes with the storage engine (Phase 9).

use crate::STANDARD_VECTOR_SIZE;
use crate::types::LogicalType;
use crate::value::Value;
use crate::vector::{DataChunk, SelectionVector, StringCopy};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An append-only, chunked, columnar buffer of rows (`ColumnDataCollection`). Results, CTEs and
/// in-memory tables are materialized into one. Appended data is copied, so the collection owns
/// everything it holds; scans borrow chunks without copying.
#[derive(Debug, Clone)]
pub struct ColumnDataCollection {
    types: Vec<LogicalType>,
    chunks: Vec<DataChunk>,
    count: usize,
}

/// Hands out chunk indexes to threads scanning the same collection
/// (`ColumnDataParallelScanState`).
#[derive(Debug, Default)]
pub struct ParallelScanState {
    next: AtomicUsize,
}

impl ColumnDataCollection {
    pub fn new(types: Vec<LogicalType>) -> Self {
        Self {
            types,
            chunks: Vec::new(),
            count: 0,
        }
    }

    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    pub fn column_count(&self) -> usize {
        self.types.len()
    }

    /// Rows in the collection (`Count`).
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Appends the rows of `chunk`, topping up the last chunk before starting a new one.
    pub fn append(&mut self, chunk: &DataChunk) {
        assert_eq!(chunk.types(), self.types, "appended chunk has the wrong types");
        let mut done = 0;
        while done < chunk.len() {
            if self.chunks.last().is_none_or(|c| c.len() == STANDARD_VECTOR_SIZE) {
                self.chunks.push(DataChunk::new(&self.types));
            }
            let target = self.chunks.last_mut().expect("just ensured");
            let offset = target.len();
            let n = (chunk.len() - done).min(STANDARD_VECTOR_SIZE - offset);
            let sel = SelectionVector::from_vec((done..done + n).map(|i| i as u32).collect());
            for (column, source) in target.columns_mut().iter_mut().zip(chunk.columns()) {
                column.copy_from(source, &sel, n, offset, StringCopy::Deep);
            }
            target.set_len(offset + n);
            done += n;
        }
        self.count += chunk.len();
    }

    /// Appends every row of `other` (`Append(const ColumnDataCollection &)`).
    pub fn append_collection(&mut self, other: &ColumnDataCollection) {
        for chunk in &other.chunks {
            self.append(chunk);
        }
    }

    /// Moves the chunks of `other` to the end of this collection without copying (`Combine`).
    pub fn combine(&mut self, other: ColumnDataCollection) {
        assert_eq!(other.types, self.types, "combined collections have different types");
        self.count += other.count;
        self.chunks.extend(other.chunks);
    }

    /// The chunk at `index` (`FetchChunk`).
    pub fn chunk(&self, index: usize) -> &DataChunk {
        &self.chunks[index]
    }

    /// The chunks in order (`Chunks()`).
    pub fn chunks(&self) -> impl Iterator<Item = &DataChunk> {
        self.chunks.iter()
    }

    /// The chunks restricted to `column_ids`, in that order (`Chunks(column_ids)`).
    pub fn scan_columns<'a>(&'a self, column_ids: &'a [usize]) -> impl Iterator<Item = DataChunk> + 'a {
        self.chunks.iter().map(move |chunk| {
            let columns = column_ids.iter().map(|&c| chunk.column(c).clone()).collect();
            DataChunk::from_columns(columns, chunk.len())
        })
    }

    /// The next unscanned chunk for this thread, or `None` when every chunk has been claimed.
    pub fn scan_parallel(&self, state: &ParallelScanState) -> Option<&DataChunk> {
        self.chunks.get(state.next.fetch_add(1, Ordering::Relaxed))
    }

    /// The value at `column` of row `row` (`GetValue`).
    pub fn get_value(&self, column: usize, row: usize) -> Value {
        assert!(row < self.count, "row {row} out of range ({} rows)", self.count);
        self.chunks[row / STANDARD_VECTOR_SIZE].get_value(column, row % STANDARD_VECTOR_SIZE)
    }

    /// One row as values.
    pub fn row(&self, row: usize) -> Vec<Value> {
        (0..self.types.len()).map(|c| self.get_value(c, row)).collect()
    }

    /// Removes every row (`Reset`).
    pub fn reset(&mut self) {
        self.chunks.clear();
        self.count = 0;
    }

    /// Compares two results row by row (`ResultEquals`), sorting rows first unless `ordered`.
    /// Returns a description of the first difference.
    pub fn result_equals(left: &Self, right: &Self, ordered: bool) -> Result<(), String> {
        if left.types.len() != right.types.len() {
            return Err(format!(
                "Column count mismatch ({} vs {})",
                left.types.len(),
                right.types.len()
            ));
        }
        if left.count != right.count {
            return Err(format!("Row count mismatch ({} vs {})", left.count, right.count));
        }
        let render = |c: &Self| -> Vec<Vec<String>> {
            let mut rows: Vec<Vec<String>> = (0..c.count)
                .map(|r| {
                    c.row(r)
                        .iter()
                        .map(|v| v.try_to_string().unwrap_or_else(|_| format!("{:?}", v.data())))
                        .collect()
                })
                .collect();
            if !ordered {
                rows.sort();
            }
            rows
        };
        let (l, r) = (render(left), render(right));
        match l.iter().zip(&r).position(|(a, b)| a != b) {
            None => Ok(()),
            Some(i) => Err(format!(
                "Row {i} differs: [{}] vs [{}]",
                l[i].join(", "),
                r[i].join(", ")
            )),
        }
    }
}

impl fmt::Display for ColumnDataCollection {
    /// A debugging dump (`ToString`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Column Data Collection [{} Columns, {} Rows, {} Chunks]",
            self.types.len(),
            self.count,
            self.chunks.len()
        )?;
        for chunk in &self.chunks {
            write!(f, "{chunk}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::Vector;

    fn chunk_of(start: i32, n: usize) -> DataChunk {
        let mut chunk = DataChunk::with_capacity(&[LogicalType::INTEGER, LogicalType::VARCHAR], n.max(1));
        for i in 0..n {
            let v = start + i as i32;
            let text = if v % 3 == 0 {
                Value::null(LogicalType::VARCHAR)
            } else {
                Value::varchar(format!("a fairly long string number {v}"))
            };
            chunk.append_row(&[Value::integer(v), text]).unwrap();
        }
        chunk
    }

    #[test]
    fn append_spans_chunks_and_owns_strings() {
        let mut cdc = ColumnDataCollection::new(vec![LogicalType::INTEGER, LogicalType::VARCHAR]);
        let mut start = 0;
        for n in [1000, 1500, 3000] {
            let chunk = chunk_of(start, n);
            cdc.append(&chunk);
            drop(chunk);
            start += n as i32;
        }
        assert_eq!(cdc.len(), 5500);
        assert_eq!(cdc.chunk_count(), 3);
        assert_eq!(cdc.chunk(0).len(), STANDARD_VECTOR_SIZE);
        assert_eq!(cdc.chunk(2).len(), 5500 - 2 * STANDARD_VECTOR_SIZE);
        for row in [0, 1, 2047, 2048, 4000, 5499] {
            let v = row as i32;
            assert_eq!(cdc.get_value(0, row), Value::integer(v));
            let expected = if v % 3 == 0 {
                Value::null(LogicalType::VARCHAR)
            } else {
                Value::varchar(format!("a fairly long string number {v}"))
            };
            assert_eq!(cdc.get_value(1, row), expected);
        }
    }

    #[test]
    fn appends_non_flat_vectors() {
        let mut cdc = ColumnDataCollection::new(vec![LogicalType::BIGINT, LogicalType::VARCHAR]);
        let columns = vec![
            Vector::sequence(LogicalType::BIGINT, 10, 1).unwrap(),
            Vector::constant(&Value::varchar("constant string that is long")),
        ];
        let mut chunk = DataChunk::from_columns(columns, 5);
        chunk.slice(&SelectionVector::from_vec(vec![4, 0]), 2);
        cdc.append(&chunk);
        assert_eq!(
            cdc.row(0),
            [Value::bigint(14), Value::varchar("constant string that is long")]
        );
        assert_eq!(cdc.row(1)[0], Value::bigint(10));
    }

    #[test]
    fn nested_types() {
        let list_ty = LogicalType::list(LogicalType::VARCHAR);
        let mut source = DataChunk::with_capacity(std::slice::from_ref(&list_ty), 3);
        let a = Value::list(
            LogicalType::VARCHAR,
            vec![Value::varchar("x"), Value::varchar("a long list element string")],
        )
        .unwrap();
        source.append_row(std::slice::from_ref(&a)).unwrap();
        source.append_row(&[Value::null(list_ty.clone())]).unwrap();
        let mut cdc = ColumnDataCollection::new(vec![list_ty]);
        cdc.append(&source);
        cdc.append(&source);
        drop(source);
        assert_eq!(cdc.get_value(0, 0), a);
        assert!(cdc.get_value(0, 1).is_null());
        assert_eq!(cdc.get_value(0, 2), a);

        let s = Value::struct_value(vec![("k".into(), Value::integer(1)), ("v".into(), a.clone())]);
        let mut source = DataChunk::with_capacity(&[s.logical_type().clone()], 1);
        source.append_row(std::slice::from_ref(&s)).unwrap();
        let mut cdc = ColumnDataCollection::new(vec![s.logical_type().clone()]);
        cdc.append(&source);
        assert_eq!(cdc.get_value(0, 0), s);
    }

    #[test]
    fn scans_and_comparison() {
        let mut a = ColumnDataCollection::new(vec![LogicalType::INTEGER, LogicalType::VARCHAR]);
        a.append(&chunk_of(0, 3000));
        let state = ParallelScanState::default();
        let mut scanned = 0;
        while let Some(chunk) = a.scan_parallel(&state) {
            scanned += chunk.len();
        }
        assert_eq!(scanned, 3000);
        let only_ints: Vec<DataChunk> = a.scan_columns(&[0]).collect();
        assert_eq!(only_ints[1].column_count(), 1);
        assert_eq!(only_ints[1].get_value(0, 0), Value::integer(2048));

        let mut b = ColumnDataCollection::new(a.types().to_vec());
        b.append(&chunk_of(1500, 1500));
        b.append(&chunk_of(0, 1500));
        assert!(ColumnDataCollection::result_equals(&a, &b, false).is_ok());
        assert!(ColumnDataCollection::result_equals(&a, &b, true).is_err());

        let mut c = a.clone();
        c.combine(b);
        assert_eq!(c.len(), 6000);
        c.reset();
        assert!(c.is_empty());
    }
}
