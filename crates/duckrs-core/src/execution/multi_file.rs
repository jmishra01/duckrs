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
}

impl VirtualColumn {
    pub const ALL: [Self; 3] = [Self::Filename, Self::FileIndex, Self::FileRowNumber];

    pub fn name(self) -> &'static str {
        match self {
            Self::Filename => "filename",
            Self::FileIndex => "file_index",
            Self::FileRowNumber => "file_row_number",
        }
    }

    pub fn logical_type(self) -> LogicalType {
        match self {
            Self::Filename => LogicalType::VARCHAR,
            Self::FileIndex => LogicalType::UBIGINT,
            Self::FileRowNumber => LogicalType::BIGINT,
        }
    }

    /// The virtual columns a scan with columns `names` offers: those that no column of it shadows.
    pub fn for_columns(names: &[String]) -> Vec<Self> {
        Self::ALL
            .into_iter()
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
        });
    }
    Ok(DataChunk::from_columns(result, count))
}

/// The virtual columns of a scan whose other columns are `names`, and where they go in its output, by name
/// and type (`Binding::virtual_columns`).
#[allow(clippy::type_complexity)]
pub fn bind_virtual_columns(names: &[String]) -> (Vec<VirtualColumn>, Vec<(String, LogicalType, usize)>) {
    let columns = VirtualColumn::for_columns(names);
    let bound = columns
        .iter()
        .enumerate()
        .map(|(i, column)| (column.name().to_string(), column.logical_type(), names.len() + i))
        .collect();
    (columns, bound)
}
