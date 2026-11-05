//! Ported from duckdb/src/storage/table/column_data.cpp, column_segment.cpp and data_pointer.cpp
//! @ rust-port-baseline.
//!
//! Turns the row groups of a table in a file into chunks.

use super::block_manager::corrupt;
use super::buffer_manager::BufferManager;
use super::checkpoint::{RowGroupPointer, StoredTable};
use super::compression::*;
use super::metadata::{MetaBlockPointer, MetadataChains};
use super::statistics::{BaseStatistics, read_statistics};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::{Interval, LogicalType, NativeType, PhysicalType};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, Result};

const STANDARD_VECTOR_SIZE: usize = 2048;

/// `DataPointer`.
struct DataPointer {
    tuple_count: usize,
    block_id: i64,
    offset: usize,
    compression: u8,
    statistics: BaseStatistics,
}

fn read_data_pointer(d: &mut BinaryDeserializer<'_>, ty: Option<&LogicalType>) -> Result<DataPointer> {
    d.read_object_value(|d| {
        d.read_deleted_property::<u64>(100, "row_start")?;
        let tuple_count: u64 = d.read_property_with_default(101, "tuple_count")?;
        let (block_id, offset) = d.read_object(102, "block_pointer", |d| {
            let block_id: i64 = d.read_property(100, "block_id")?;
            let offset: u32 = d.read_property_with_default(101, "offset")?;
            Ok((block_id, offset))
        })?;
        let compression: u8 = d.read_property(103, "compression_type")?;
        let statistics = d.read_object_field(104, "statistics", |d| read_statistics(d, ty))?;
        if d.has_property(105)? {
            // `UncompressedStringStorage::DeserializeState`: which blocks hold overflow strings,
            // and the markers in the segment say so too
            d.read_object_field(105, "segment_state", |d| {
                if bool::read_value(d)? {
                    d.read_object_value(|d| d.read_property::<Vec<i64>>(1, "overflow_blocks"))?;
                }
                Ok(())
            })?;
        }
        d.read_optional_property::<u32>(106, "byte_size")?;
        Ok(DataPointer {
            tuple_count: tuple_count as usize,
            block_id,
            offset: offset as usize,
            compression,
            statistics,
        })
    })
}

/// `PersistentColumnData`.
struct PersistentColumn {
    pointers: Vec<DataPointer>,
    validity: Option<Box<PersistentColumn>>,
}

/// `PersistentColumnData::Deserialize`, of a column or (with no `ty`) of its validity.
fn read_persistent_column(d: &mut BinaryDeserializer<'_>, ty: Option<&LogicalType>) -> Result<PersistentColumn> {
    d.read_object_value(|d| {
        if d.has_property(99)? {
            return Err(Error::not_implemented(
                "reading extra column data from a database file is not yet ported to duckrs",
            ));
        }
        let pointers = if d.has_property(100)? {
            d.read_list(100, "data_pointers", |d| read_data_pointer(d, ty))?
        } else {
            Vec::new()
        };
        let validity = match ty {
            Some(_) => {
                Some(Box::new(d.read_object_field(101, "validity", |d| {
                    read_persistent_column(d, None)
                })?))
            }
            None => None,
        };
        Ok(PersistentColumn { pointers, validity })
    })
}

/// The rows of a column, as `Vector` wants them.
enum Rows {
    Fixed { width: usize, data: Vec<u8> },
    Strings(Vec<Vec<u8>>),
}

/// `GetTypeIdSize`.
fn fixed_width(physical: PhysicalType) -> Option<usize> {
    Some(match physical {
        PhysicalType::Bool | PhysicalType::Int8 | PhysicalType::UInt8 => 1,
        PhysicalType::Int16 | PhysicalType::UInt16 => 2,
        PhysicalType::Int32 | PhysicalType::UInt32 | PhysicalType::Float => 4,
        PhysicalType::Int64 | PhysicalType::UInt64 | PhysicalType::Double => 8,
        PhysicalType::Int128 | PhysicalType::UInt128 | PhysicalType::Interval => 16,
        _ => return None,
    })
}

/// The bytes from a segment's start to the end of its block.
fn segment(buffer: &BufferManager, pointer: &DataPointer) -> Result<std::sync::Arc<Vec<u8>>> {
    let block = u64::try_from(pointer.block_id).map_err(|_| corrupt("a segment with data has no block"))?;
    let handle = buffer.pin(block)?;
    if pointer.offset > handle.len() {
        return Err(corrupt("a segment starts past the end of its block"));
    }
    Ok(handle)
}

fn scan_fixed(buffer: &BufferManager, pointer: &DataPointer, width: usize, out: &mut Vec<u8>) -> Result<()> {
    let count = pointer.tuple_count;
    if pointer.compression == COMPRESSION_CONSTANT {
        return scan_constant(&pointer.statistics, count, width, out);
    }
    let handle = segment(buffer, pointer)?;
    let data = &handle[pointer.offset..];
    match pointer.compression {
        COMPRESSION_UNCOMPRESSED => scan_uncompressed(data, count, width, out),
        COMPRESSION_RLE => scan_rle(data, count, width, out),
        COMPRESSION_BITPACKING => scan_bitpacking(data, count, width, out),
        other => Err(not_ported(other)),
    }
}

fn scan_strings(buffer: &BufferManager, pointer: &DataPointer, out: &mut Vec<Vec<u8>>) -> Result<()> {
    let count = pointer.tuple_count;
    if pointer.compression == COMPRESSION_CONSTANT {
        // only all-NULL segments get here: strings don't have constant segments
        out.resize(out.len() + count, Vec::new());
        return Ok(());
    }
    let handle = segment(buffer, pointer)?;
    let data = &handle[pointer.offset..];
    match pointer.compression {
        COMPRESSION_UNCOMPRESSED => scan_string_uncompressed(data, count, &|block| buffer.pin(block), out),
        COMPRESSION_DICTIONARY => scan_dictionary(data, count, out),
        COMPRESSION_FSST => scan_fsst(data, count, out),
        other => Err(not_ported(other)),
    }
}

fn scan_validity_column(buffer: &BufferManager, column: &PersistentColumn, rows: usize) -> Result<Vec<bool>> {
    let mut valid = Vec::with_capacity(rows);
    for pointer in &column.pointers {
        match pointer.compression {
            COMPRESSION_CONSTANT => valid.resize(valid.len() + pointer.tuple_count, !pointer.statistics.has_null),
            COMPRESSION_EMPTY => valid.resize(valid.len() + pointer.tuple_count, true),
            COMPRESSION_UNCOMPRESSED => {
                let handle = segment(buffer, pointer)?;
                scan_validity(&handle[pointer.offset..], pointer.tuple_count, &mut valid)?;
            }
            other => return Err(not_ported(other)),
        }
    }
    if valid.len() != rows {
        return Err(corrupt("a validity column does not have the rows of its row group"));
    }
    Ok(valid)
}

fn scan_column(
    buffer: &BufferManager,
    column: &PersistentColumn,
    ty: &LogicalType,
    rows: usize,
) -> Result<(Rows, Vec<bool>)> {
    let physical = ty.physical_type();
    let result = if physical == PhysicalType::Varchar {
        let mut strings = Vec::with_capacity(rows);
        for pointer in &column.pointers {
            scan_strings(buffer, pointer, &mut strings)?;
        }
        if strings.len() != rows {
            return Err(corrupt("a column does not have the rows of its row group"));
        }
        Rows::Strings(strings)
    } else {
        let width = fixed_width(physical).ok_or_else(|| {
            Error::not_implemented(format!(
                "reading a {} column from a database file is not yet ported to duckrs",
                ty.id().name()
            ))
        })?;
        let mut data = Vec::with_capacity(rows * width);
        for pointer in &column.pointers {
            scan_fixed(buffer, pointer, width, &mut data)?;
        }
        if data.len() != rows * width {
            return Err(corrupt("a column does not have the rows of its row group"));
        }
        Rows::Fixed { width, data }
    };
    let validity = column.validity.as_ref().expect("a column of a table has validity");
    Ok((result, scan_validity_column(buffer, validity, rows)?))
}

/// Stores `data`'s values from row `start` on in a vector of native values of type `T`.
fn fill<T: NativeType, const N: usize>(vector: &mut Vector, data: &[u8], start: usize, read: impl Fn(&[u8; N]) -> T) {
    let target = vector.data_mut::<T>();
    let source = data[start * N..].chunks_exact(N);
    for (target, source) in target.iter_mut().zip(source) {
        *target = read(source.try_into().expect("N bytes"));
    }
}

fn fill_vector(vector: &mut Vector, rows: &Rows, start: usize, count: usize) -> Result<()> {
    let data = match rows {
        Rows::Strings(strings) => {
            for (i, string) in strings[start..start + count].iter().enumerate() {
                vector.set_str(i, string);
            }
            return Ok(());
        }
        Rows::Fixed { width, data } => &data[..(start + count) * width],
    };
    match vector.physical_type() {
        // anything but 0 and 1 would be undefined behavior as a `bool`
        PhysicalType::Bool => fill::<bool, 1>(vector, data, start, |b| b[0] != 0),
        PhysicalType::Int8 => fill::<i8, 1>(vector, data, start, |b| i8::from_le_bytes(*b)),
        PhysicalType::UInt8 => fill::<u8, 1>(vector, data, start, |b| b[0]),
        PhysicalType::Int16 => fill::<i16, 2>(vector, data, start, |b| i16::from_le_bytes(*b)),
        PhysicalType::UInt16 => fill::<u16, 2>(vector, data, start, |b| u16::from_le_bytes(*b)),
        PhysicalType::Int32 => fill::<i32, 4>(vector, data, start, |b| i32::from_le_bytes(*b)),
        PhysicalType::UInt32 => fill::<u32, 4>(vector, data, start, |b| u32::from_le_bytes(*b)),
        PhysicalType::Float => fill::<f32, 4>(vector, data, start, |b| f32::from_le_bytes(*b)),
        PhysicalType::Int64 => fill::<i64, 8>(vector, data, start, |b| i64::from_le_bytes(*b)),
        PhysicalType::UInt64 => fill::<u64, 8>(vector, data, start, |b| u64::from_le_bytes(*b)),
        PhysicalType::Double => fill::<f64, 8>(vector, data, start, |b| f64::from_le_bytes(*b)),
        PhysicalType::Int128 => fill::<i128, 16>(vector, data, start, |b| i128::from_le_bytes(*b)),
        PhysicalType::UInt128 => fill::<u128, 16>(vector, data, start, |b| u128::from_le_bytes(*b)),
        PhysicalType::Interval => fill::<Interval, 16>(vector, data, start, |b| Interval {
            months: i32::from_le_bytes(b[..4].try_into().expect("4 bytes")),
            days: i32::from_le_bytes(b[4..8].try_into().expect("4 bytes")),
            micros: i64::from_le_bytes(b[8..].try_into().expect("8 bytes")),
        }),
        other => return Err(corrupt(format!("a fixed size column of type {other:?}"))),
    }
    Ok(())
}

fn read_row_group(
    buffer: &BufferManager,
    chains: &mut MetadataChains,
    table: &StoredTable,
    row_group: &RowGroupPointer,
    types: &[LogicalType],
    result: &mut ColumnDataCollection,
) -> Result<()> {
    if !row_group.deletes_pointers.is_empty() {
        // going on would bring deleted rows back
        return Err(Error::not_implemented(
            "reading a table with deleted rows from a database file is not yet ported to duckrs",
        ));
    }
    if row_group.data_pointers.len() != table.columns.len() {
        return Err(corrupt("a row group does not have the columns of its table"));
    }
    let rows = row_group.tuple_count as usize;
    let mut columns = Vec::with_capacity(types.len());
    for (ty, &pointer) in types.iter().zip(&row_group.data_pointers) {
        let column = read_column_pointer(buffer, chains, pointer, ty)?;
        columns.push(scan_column(buffer, &column, ty, rows)?);
    }
    let mut start = 0;
    while start < rows {
        let count = STANDARD_VECTOR_SIZE.min(rows - start);
        let mut chunk = DataChunk::with_capacity(types, count);
        for (i, (values, valid)) in columns.iter().enumerate() {
            let vector = chunk.column_mut(i);
            fill_vector(vector, values, start, count)?;
            for (row, &valid) in valid[start..start + count].iter().enumerate() {
                if !valid {
                    vector.set_null(row, true);
                }
            }
        }
        chunk.set_len(count);
        result.append(&chunk);
        start += count;
    }
    Ok(())
}

fn read_column_pointer(
    buffer: &BufferManager,
    chains: &mut MetadataChains,
    pointer: MetaBlockPointer,
    ty: &LogicalType,
) -> Result<PersistentColumn> {
    let mut d = BinaryDeserializer::new(chains.at(buffer, pointer)?);
    read_persistent_column(&mut d, Some(ty))
}

/// All rows of a table.
pub fn read_table(buffer: &BufferManager, table: &StoredTable) -> Result<ColumnDataCollection> {
    let types: Vec<LogicalType> = table.columns.iter().map(|c| c.ty.clone()).collect();
    let mut result = ColumnDataCollection::new(types.clone());
    let mut chains = MetadataChains::default();
    for row_group in &table.row_groups {
        read_row_group(buffer, &mut chains, table, row_group, &types, &mut result)?;
    }
    Ok(result)
}
