// Ported from duckdb/src/common/multi_file/multi_file_reader.cpp (`GetVirtualColumns`) @ rust-port-baseline
//
//! The virtual columns of the scans that read several files (`read_csv`, `read_json`, `read_parquet`):
//! columns that `*` leaves out, but that a query can name. The scans put them after their other columns.

use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};

/// A column every file of a multi-file scan has (`MultiFileReader::GetVirtualColumns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualColumn {
    /// the path of the file of the row (`COLUMN_IDENTIFIER_FILENAME`)
    Filename,
    /// the position of the file in the list of files (`COLUMN_IDENTIFIER_FILE_INDEX`)
    FileIndex,
    /// the position of the row in its file, from 0 (`COLUMN_IDENTIFIER_FILE_ROW_NUMBER`)
    FileRowNumber,
    /// Parquet only: the row group of the row in its file, from 0 (`COLUMN_IDENTIFIER_FILE_ROW_GROUP_NUMBER`)
    FileRowGroupNumber,
}

impl VirtualColumn {
    pub const ALL: [Self; 3] = [Self::Filename, Self::FileIndex, Self::FileRowNumber];
    /// `ParquetMultiFileInfo::GetVirtualColumns`: those of every multi-file scan, and the row group
    pub const PARQUET: [Self; 4] = [Self::Filename, Self::FileIndex, Self::FileRowNumber, Self::FileRowGroupNumber];

    pub fn name(self) -> &'static str {
        match self {
            Self::Filename => "filename",
            Self::FileIndex => "file_index",
            Self::FileRowNumber => "file_row_number",
            Self::FileRowGroupNumber => "file_row_group_number",
        }
    }

    pub fn logical_type(self) -> LogicalType {
        match self {
            Self::Filename => LogicalType::VARCHAR,
            Self::FileIndex => LogicalType::UBIGINT,
            Self::FileRowNumber => LogicalType::BIGINT,
            Self::FileRowGroupNumber => LogicalType::UBIGINT,
        }
    }

    /// The virtual columns a scan with columns `names` offers: those that no column of it shadows.
    pub fn for_columns(names: &[String]) -> Vec<Self> {
        Self::offered(&Self::ALL, names)
    }

    /// Those of `all` that no column of `names` shadows.
    fn offered(all: &[Self], names: &[String]) -> Vec<Self> {
        all.iter()
            .copied()
            .filter(|column| !names.iter().any(|n| n.eq_ignore_ascii_case(column.name())))
            .collect()
    }
}

/// The types of `columns`, to go after the other output types of a scan.
pub fn virtual_types(columns: &[VirtualColumn]) -> impl Iterator<Item = LogicalType> + '_ {
    columns.iter().map(|column| column.logical_type())
}

/// What a scan counts to fill `file_row_number`.
#[derive(Debug, Default)]
pub struct VirtualRows {
    file: Option<usize>,
    rows: i64,
    /// of a Parquet file: the row groups read, in order, each with how many rows it has
    row_groups: Vec<(u64, i64)>,
}

impl VirtualRows {
    /// The row groups the scan reads of the file it opens next, in order, with their row counts.
    pub fn set_row_groups(&mut self, row_groups: Vec<(u64, i64)>) {
        self.row_groups = row_groups;
    }

    /// `file_row_group_number` of the `count` rows from position `first` of the rows read of the file.
    fn row_group_numbers(&self, first: i64, count: usize) -> Vector {
        let mut result = Vector::new(LogicalType::UBIGINT, count);
        let numbers = result.data_mut::<u64>();
        let mut groups = self.row_groups.iter();
        let (mut group, mut end) = (0u64, 0i64);
        for (row, number) in (first..).zip(numbers.iter_mut().take(count)) {
            while row >= end {
                match groups.next() {
                    Some(&(next, rows)) => {
                        group = next;
                        end += rows;
                    }
                    None => {
                        end = i64::MAX;
                    }
                }
            }
            *number = group;
        }
        result
    }
}

/// `chunk`, rows of file number `file` at `path`, with `columns` after its own.
pub fn append_virtual_columns(
    chunk: DataChunk,
    columns: &[VirtualColumn],
    path: &str,
    file: usize,
    state: &mut VirtualRows,
) -> Result<DataChunk> {
    if state.file != Some(file) {
        state.file = Some(file);
        state.rows = 0;
    }
    let count = chunk.len();
    let first = state.rows;
    state.rows += count as i64;
    if columns.is_empty() {
        return Ok(chunk);
    }
    let mut result = chunk.columns().to_vec();
    for column in columns {
        result.push(match column {
            VirtualColumn::Filename => Vector::constant(&Value::varchar(path)),
            VirtualColumn::FileIndex => Vector::constant(&Value::ubigint(file as u64)),
            VirtualColumn::FileRowNumber => Vector::sequence(LogicalType::BIGINT, first, 1)?,
            VirtualColumn::FileRowGroupNumber => state.row_group_numbers(first, count),
        });
    }
    Ok(DataChunk::from_columns(result, count))
}

/// The virtual columns of a scan whose other columns are `names`, and where they go in its output, by name
/// and type (`Binding::virtual_columns`).
#[allow(clippy::type_complexity)]
pub fn bind_virtual_columns(names: &[String]) -> (Vec<VirtualColumn>, Vec<(String, LogicalType, usize)>) {
    bind_offered_columns(&VirtualColumn::ALL, names)
}

/// [`bind_virtual_columns`] of a Parquet scan, which also offers `file_row_group_number`.
#[allow(clippy::type_complexity)]
pub fn bind_parquet_virtual_columns(names: &[String]) -> (Vec<VirtualColumn>, Vec<(String, LogicalType, usize)>) {
    bind_offered_columns(&VirtualColumn::PARQUET, names)
}

#[allow(clippy::type_complexity)]
fn bind_offered_columns(
    all: &[VirtualColumn],
    names: &[String],
) -> (Vec<VirtualColumn>, Vec<(String, LogicalType, usize)>) {
    let columns = VirtualColumn::offered(all, names);
    let bound = columns
        .iter()
        .enumerate()
        .map(|(i, column)| (column.name().to_string(), column.logical_type(), names.len() + i))
        .collect();
    (columns, bound)
}

/// The order in which the binder first read the columns of a scan (`LogicalGet::column_ids`): the
/// columns a scan with projection pushdown outputs come in this order.
#[derive(Debug, Clone, Default)]
pub struct ColumnOrder(std::sync::Arc<std::sync::Mutex<Vec<usize>>>);

impl ColumnOrder {
    /// The binder read column `column` of the scan.
    pub fn note(&self, column: usize) {
        let mut order = self.0.lock().expect("not poisoned");
        if !order.contains(&column) {
            order.push(column);
        }
    }

    /// `columns` in the order the binder read them, those it did not read last.
    pub fn arrange(&self, columns: &[usize]) -> Vec<usize> {
        let order = self.0.lock().expect("not poisoned");
        let mut arranged: Vec<usize> = order.iter().copied().filter(|c| columns.contains(c)).collect();
        arranged.extend(columns.iter().copied().filter(|c| !order.contains(c)));
        arranged
    }
}
