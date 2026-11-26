// Ported from duckdb/extension/tpcds/dsdgen/append_info-c.cpp @ rust-port-baseline

//! Collects the rows the table builders append into `DataChunk`s of at most
//! [`STANDARD_VECTOR_SIZE`] rows: column by column, in typed buffers, which become flat vectors
//! when a chunk is full.

use crate::dsdgen_c::decimal::Decimal;
use crate::{ColumnType, Table};
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::types::{Date, LogicalType, PhysicalType};
use duckrs_common::vector::{DataChunk, Vector};
use std::collections::VecDeque;

/// The Julian day of 1970-01-01, which DuckDB dates count from.
const UNIX_EPOCH_JULIAN: i64 = 2440588;

/// The values of one column of the chunk being built.
enum ColumnData {
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Date(Vec<Date>),
    Varchar { bytes: Vec<u8>, ends: Vec<usize> },
}

struct Column {
    ty: LogicalType,
    data: ColumnData,
    valid: Vec<bool>,
    #[cfg(debug_assertions)]
    decimal_scale: Option<i32>,
}

impl Column {
    fn new(ty: ColumnType) -> Self {
        let logical = ty.logical_type();
        let data = match ty {
            ColumnType::Varchar => ColumnData::Varchar {
                bytes: Vec::new(),
                ends: Vec::with_capacity(STANDARD_VECTOR_SIZE),
            },
            ColumnType::Date => ColumnData::Date(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
            ColumnType::Integer | ColumnType::BigInt | ColumnType::Decimal(..) => match logical.physical_type() {
                PhysicalType::Int16 => ColumnData::Int16(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
                PhysicalType::Int32 => ColumnData::Int32(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
                PhysicalType::Int64 => ColumnData::Int64(Vec::with_capacity(STANDARD_VECTOR_SIZE)),
                other => panic!("Unexpected TPC-DS column type {other}"),
            },
        };
        Self {
            ty: logical,
            data,
            valid: Vec::with_capacity(STANDARD_VECTOR_SIZE),
            #[cfg(debug_assertions)]
            decimal_scale: match ty {
                ColumnType::Decimal(_, scale) => Some(i32::from(scale)),
                _ => None,
            },
        }
    }

    fn push_null(&mut self) {
        match &mut self.data {
            ColumnData::Int16(v) => v.push(0),
            ColumnData::Int32(v) => v.push(0),
            ColumnData::Int64(v) => v.push(0),
            ColumnData::Date(v) => v.push(Date(0)),
            ColumnData::Varchar { bytes, ends } => ends.push(bytes.len()),
        }
        self.valid.push(false);
    }

    fn push_int(&mut self, value: i64) {
        match &mut self.data {
            ColumnData::Int16(v) => v.push(value as i16),
            ColumnData::Int32(v) => v.push(value as i32),
            ColumnData::Int64(v) => v.push(value),
            ColumnData::Varchar { bytes, ends } => {
                bytes.extend_from_slice(value.to_string().as_bytes());
                ends.push(bytes.len());
            }
            ColumnData::Date(_) => panic!("Unexpected TPC-DS integer column type"),
        }
        self.valid.push(true);
    }

    /// Moves the buffered values into a flat vector.
    fn take_vector(&mut self, count: usize) -> Vector {
        let mut vector = Vector::new(self.ty.clone(), count);
        match &mut self.data {
            ColumnData::Int16(v) => {
                vector.data_mut::<i16>()[..count].copy_from_slice(v);
                v.clear();
            }
            ColumnData::Int32(v) => {
                vector.data_mut::<i32>()[..count].copy_from_slice(v);
                v.clear();
            }
            ColumnData::Int64(v) => {
                vector.data_mut::<i64>()[..count].copy_from_slice(v);
                v.clear();
            }
            ColumnData::Date(v) => {
                vector.data_mut::<Date>()[..count].copy_from_slice(v);
                v.clear();
            }
            ColumnData::Varchar { bytes, ends } => {
                let capacity = bytes.capacity();
                vector.set_strs_owned(std::mem::replace(bytes, Vec::with_capacity(capacity)), ends);
                ends.clear();
            }
        }
        for (row, &valid) in self.valid.iter().enumerate() {
            if !valid {
                vector.set_null(row, true);
            }
        }
        self.valid.clear();
        vector
    }
}

/// `tpcds_append_information`: the chunk being built for one table.
pub(crate) struct TableAppender {
    table: Table,
    columns: Vec<Column>,
    row: usize,
    active_col: usize,
    first_column: i32,
    null_bitmap: i64,
}

impl TableAppender {
    fn new(table: Table, first_column: i32) -> Self {
        Self {
            table,
            columns: table.info().columns.iter().map(|&(_, ty)| Column::new(ty)).collect(),
            row: 0,
            active_col: 0,
            first_column,
            null_bitmap: 0,
        }
    }

    /// `IsNull`: whether the row's null bitmap has the bit of column number `n_column`.
    fn is_null(&self, n_column: i32) -> bool {
        let column_offset = n_column - self.first_column;
        debug_assert!(column_offset >= 0);
        // shifts past 63 bits wrap, as they do on the machines dsdgen runs on
        let bit_mask = 1i64.wrapping_shl(column_offset as u32);
        (self.null_bitmap & bit_mask) != 0
    }

    fn next_column(&mut self) -> &mut Column {
        let column = &mut self.columns[self.active_col];
        self.active_col += 1;
        column
    }

    /// `append_row_end`
    pub(crate) fn row_end(&mut self) {
        debug_assert_eq!(
            self.active_col,
            self.columns.len(),
            "{}: wrong column count",
            self.table.name()
        );
        self.row += 1;
    }

    /// `append_varchar`: NULL for NULL columns and empty strings.
    pub(crate) fn append_varchar(&mut self, value: &[u8], n_column: i32) {
        if self.is_null(n_column) || value.is_empty() {
            self.next_column().push_null();
            return;
        }
        let column = self.next_column();
        match &mut column.data {
            ColumnData::Varchar { bytes, ends } => {
                bytes.extend_from_slice(value);
                ends.push(bytes.len());
            }
            _ => panic!("Unexpected TPC-DS varchar column type"),
        }
        column.valid.push(true);
    }

    /// `append_varchar` of a value that may be missing (a NULL `char *`).
    pub(crate) fn append_varchar_opt(&mut self, value: Option<&[u8]>, n_column: i32) {
        self.append_varchar(value.unwrap_or(b""), n_column);
    }

    /// `append_key`: NULL for NULL columns and negative keys.
    pub(crate) fn append_key(&mut self, value: i64, n_column: i32) {
        if self.is_null(n_column) || value < 0 {
            self.next_column().push_null();
        } else {
            self.next_column().push_int(value);
        }
    }

    /// `append_integer`
    pub(crate) fn append_integer(&mut self, value: i32, n_column: i32) {
        if self.is_null(n_column) {
            self.next_column().push_null();
        } else {
            self.next_column().push_int(i64::from(value));
        }
    }

    /// `append_integer_decimal`: an integer as a decimal with two digits after the point.
    pub(crate) fn append_integer_decimal(&mut self, value: i32, n_column: i32) {
        if self.is_null(n_column) {
            self.next_column().push_null();
        } else {
            self.next_column().push_int(i64::from(value) * 100);
        }
    }

    /// `append_date` of a Julian day: NULL for NULL columns and negative days.
    pub(crate) fn append_date(&mut self, value: i64, n_column: i32) {
        if self.is_null(n_column) || value < 0 {
            self.next_column().push_null();
            return;
        }
        let column = self.next_column();
        match &mut column.data {
            ColumnData::Date(v) => v.push(Date((value - UNIX_EPOCH_JULIAN) as i32)),
            _ => panic!("Unexpected TPC-DS date column type"),
        }
        column.valid.push(true);
    }

    /// `append_decimal`: the decimal's digits, which have the column's scale.
    pub(crate) fn append_decimal(&mut self, value: &Decimal, n_column: i32) {
        if self.is_null(n_column) {
            self.next_column().push_null();
            return;
        }
        let column = self.next_column();
        #[cfg(debug_assertions)]
        debug_assert_eq!(column.decimal_scale, Some(value.precision), "decimal scale mismatch");
        column.push_int(value.number);
    }

    fn take_chunk(&mut self) -> DataChunk {
        let count = self.row;
        let columns = self.columns.iter_mut().map(|c| c.take_vector(count)).collect();
        self.row = 0;
        DataChunk::from_columns(columns, count)
    }
}

/// The appenders of all tables (`append_info_get`), and the chunks they have filled.
pub(crate) struct AppendInfo {
    tables: Vec<TableAppender>,
    pub(crate) ready: VecDeque<(Table, DataChunk)>,
}

impl AppendInfo {
    pub(crate) fn new(first_columns: impl Fn(Table) -> i32) -> Self {
        Self {
            tables: Table::ALL
                .iter()
                .map(|&t| TableAppender::new(t, first_columns(t)))
                .collect(),
            ready: VecDeque::new(),
        }
    }

    /// `append_info_get` and `append_row_start`: starts a row of `table`, whose null bitmap for the
    /// row is `null_bitmap`. A full chunk is moved to [`Self::ready`] first.
    pub(crate) fn row_start(&mut self, table: i32, null_bitmap: i64) -> &mut TableAppender {
        let t = &mut self.tables[table as usize];
        if t.row >= STANDARD_VECTOR_SIZE {
            let chunk = t.take_chunk();
            self.ready.push_back((t.table, chunk));
        }
        let t = &mut self.tables[table as usize];
        t.active_col = 0;
        t.null_bitmap = null_bitmap;
        t
    }

    /// `FlushChunk`: moves the rows of `table` not yet in a chunk to [`Self::ready`].
    pub(crate) fn flush(&mut self, table: i32) {
        let t = &mut self.tables[table as usize];
        if t.row > 0 {
            let chunk = t.take_chunk();
            self.ready.push_back((t.table, chunk));
        }
    }
}
