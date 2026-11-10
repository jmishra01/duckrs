//! Ported from duckdb/src/storage/table/column_data.cpp, column_segment.cpp and data_pointer.cpp
//! @ rust-port-baseline.
//!
//! Turns the row groups of a table in a file into chunks.

use super::block_manager::{corrupt, corrupted};
use super::buffer_manager::BufferManager;
use super::checkpoint::StoredTable;
use super::compression::*;
use super::metadata::{MetaBlockPointer, MetadataChains, take};
use super::statistics::{BaseStatistics, read_statistics};
use crate::catalog::{ColumnStatistics, ColumnZone, RowIds, SegmentInfo, TableSource};
use crate::optimizer::statistics::{compare_values, has_ordered_statistics};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::{Interval, ListEntry, LogicalType, LogicalTypeId, NativeType, PhysicalType};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, Result};
use duckrs_parser::ast::ComparisonOp;
use parking_lot::Mutex;
use std::sync::Arc;

const STANDARD_VECTOR_SIZE: usize = 2048;
/// `Storage::MAX_ROW_GROUP_SIZE`.
const MAX_ROW_GROUP_SIZE: u64 = 1 << 30;

/// `DataPointer`.
struct DataPointer {
    tuple_count: usize,
    block_id: i64,
    offset: usize,
    compression: u8,
    statistics: BaseStatistics,
    /// `UncompressedStringSegmentState`: the blocks of its own that a segment reads from
    overflow_blocks: Vec<i64>,
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
        let mut overflow_blocks = Vec::new();
        if d.has_property(105)? {
            // `UncompressedStringStorage::DeserializeState`: which blocks hold overflow strings,
            // and the markers in the segment say so too
            d.read_object_field(105, "segment_state", |d| {
                if bool::read_value(d)? {
                    overflow_blocks = d.read_object_value(|d| d.read_property::<Vec<i64>>(1, "overflow_blocks"))?;
                }
                Ok(())
            })?;
        }
        d.read_property_with_default::<Option<u32>>(106, "byte_size")?;
        Ok(DataPointer {
            tuple_count: tuple_count as usize,
            block_id,
            offset: offset as usize,
            compression,
            statistics,
            overflow_blocks,
        })
    })
}

/// `PersistentColumnData`.
pub(super) struct PersistentColumn {
    pointers: Vec<DataPointer>,
    validity: Option<Box<PersistentColumn>>,
    /// Of a LIST or ARRAY, one; of a STRUCT, its fields.
    children: Vec<PersistentColumn>,
}

fn not_ported_column(ty: &LogicalType) -> Error {
    Error::not_implemented(format!(
        "reading a {} column from a database file is not yet ported to duckrs",
        ty.id().name()
    ))
}

/// `PersistentColumnData::Deserialize`, of a column or (with no `ty`) of its validity.
fn read_persistent_column(d: &mut BinaryDeserializer<'_>, ty: Option<&LogicalType>) -> Result<PersistentColumn> {
    d.read_object_value(|d| {
        // `GeometryPersistentColumnData`: a GEOMETRY stored as WKB reads as the strings it is
        if ty.map(LogicalType::id) == Some(LogicalTypeId::Geometry) && d.has_property(99)? {
            let storage_type = d.read_object_field(99, "extra_data", |d| {
                d.read_object_value(|d| {
                    d.read_optional_property::<u8>(100, "type")?;
                    d.read_optional_property::<u8>(101, "storage_type")
                })
            })?;
            const WKB: u8 = 1;
            if storage_type.unwrap_or(WKB) != WKB {
                return Err(Error::not_implemented(
                    "reading a shredded GEOMETRY column from a database file is not yet ported to duckrs",
                ));
            }
        } else if ty.map(LogicalType::id) == Some(LogicalTypeId::Geometry) {
            return Err(Error::not_implemented(
                "reading a GEOMETRY column of the spatial extension from a database file is not yet ported to duckrs",
            ));
        }
        if d.has_property(99)? {
            return Err(Error::not_implemented(match ty.map(LogicalType::id) {
                Some(LogicalTypeId::Variant) => {
                    "reading a shredded VARIANT column from a database file is not yet ported to duckrs"
                }
                _ => "reading extra column data from a database file is not yet ported to duckrs",
            }));
        }
        let pointers = if d.has_property(100)? {
            d.read_list(100, "data_pointers", |d| read_data_pointer(d, ty))?
        } else {
            Vec::new()
        };
        let Some(ty) = ty else {
            return Ok(PersistentColumn {
                pointers,
                validity: None,
                children: Vec::new(),
            });
        };
        let validity = d.read_object_field(101, "validity", |d| read_persistent_column(d, None))?;
        if ty.id() == LogicalTypeId::Variant {
            let unshredded = duckrs_common::variant::unshredded_type();
            let child = d.read_object_field(102, "unshredded", |d| read_persistent_column(d, Some(&unshredded)))?;
            return Ok(PersistentColumn {
                pointers,
                validity: Some(Box::new(validity)),
                children: vec![child],
            });
        }
        let children = match ty.physical_type() {
            PhysicalType::List | PhysicalType::Array => {
                let child = ty.child_type().ok_or_else(|| corrupt("a list has no child type"))?;
                vec![d.read_object_field(102, "child_column", |d| read_persistent_column(d, Some(child)))?]
            }
            PhysicalType::Struct => {
                let mut fields = ty.struct_children().iter();
                d.read_list(102, "sub_columns", |d| {
                    let (_, field) = fields.next().ok_or_else(|| corrupt("a struct has too many columns"))?;
                    read_persistent_column(d, Some(field))
                })?
            }
            _ => Vec::new(),
        };
        Ok(PersistentColumn {
            pointers,
            validity: Some(Box::new(validity)),
            children,
        })
    })
}

/// `GetTypeIdSize`.
pub(super) fn fixed_width(physical: PhysicalType) -> Option<usize> {
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
    let block = u64::try_from(pointer.block_id).map_err(|_| {
        let name = storage_name(pointer.compression);
        corrupted(&format!("Persistent {name} segment is missing its data block"))
    })?;
    let handle = buffer.pin(block)?;
    if pointer.offset > handle.len() {
        return Err(corrupted(
            "Corrupted fixed-size segment: block offset exceeds the block size",
        ));
    }
    Ok(handle)
}

/// The rows come from the file, so before a decoder gets to allocate them: its segments must add
/// up to the rows of the column, which their parent vouches for or which are bounded themselves.
fn check_rows(column: &PersistentColumn, rows: usize) -> Result<()> {
    let total = column
        .pointers
        .iter()
        .try_fold(0usize, |total, pointer| total.checked_add(pointer.tuple_count));
    if total != Some(rows) {
        return Err(corrupted(
            "Corrupted LIST column: offset exceeds the child column count",
        ));
    }
    Ok(())
}

fn scan_fixed(
    buffer: &BufferManager,
    pointer: &DataPointer,
    ty: PhysicalType,
    width: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    let count = pointer.tuple_count;
    if pointer.compression == COMPRESSION_CONSTANT {
        return scan_constant(&pointer.statistics, count, width, out);
    }
    let handle = segment(buffer, pointer)?;
    let data = &handle[pointer.offset..];
    let float = matches!(ty, PhysicalType::Float | PhysicalType::Double);
    match pointer.compression {
        COMPRESSION_UNCOMPRESSED => scan_uncompressed(data, count, width, out),
        COMPRESSION_RLE => scan_rle(data, count, width, out),
        COMPRESSION_BITPACKING => scan_bitpacking(data, count, width, out),
        COMPRESSION_ALP if float => scan_alp(data, count, width, out),
        COMPRESSION_ALPRD if float => scan_alprd(data, count, width, out),
        COMPRESSION_ROARING if ty == PhysicalType::Bool => {
            // booleans go through the validity format, where a set bit is true
            let mut bits = Vec::with_capacity(count);
            scan_roaring(data, count, &mut bits)?;
            out.extend(bits.into_iter().map(u8::from));
            Ok(())
        }
        other => Err(not_ported(other)),
    }
}

fn scan_strings(
    buffer: &BufferManager,
    pointer: &DataPointer,
    out: &mut Vec<Vec<u8>>,
    nulls: &mut Vec<usize>,
) -> Result<()> {
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
        COMPRESSION_DICT_FSST => scan_dict_fsst(data, count, out, nulls),
        COMPRESSION_ZSTD => {
            // `UncompressedStringSegmentState::GetHandle`: only the pages of the segment
            let pin = |block: u64| {
                if !pointer.overflow_blocks.contains(&(block as i64)) {
                    return Err(Error::new(
                        duckrs_common::ErrorKind::DataCorruption,
                        "Corrupted uncompressed string segment: overflow string block is not owned by the segment",
                    ));
                }
                buffer.pin(block)
            };
            scan_zstd(data, count, pointer.offset != 0, &pin, out)
        }
        other => Err(not_ported(other)),
    }
}

fn scan_validity_column(buffer: &BufferManager, column: &PersistentColumn, rows: usize) -> Result<Vec<bool>> {
    check_rows(column, rows)?;
    let mut valid = Vec::with_capacity(rows);
    for pointer in &column.pointers {
        let count = pointer.tuple_count;
        match pointer.compression {
            COMPRESSION_CONSTANT => valid.resize(valid.len() + count, !pointer.statistics.has_null),
            COMPRESSION_EMPTY => valid.resize(valid.len() + count, true),
            COMPRESSION_UNCOMPRESSED => {
                let handle = segment(buffer, pointer)?;
                scan_validity(&handle[pointer.offset..], count, &mut valid)?;
            }
            COMPRESSION_ROARING => {
                let handle = segment(buffer, pointer)?;
                scan_roaring(&handle[pointer.offset..], count, &mut valid)?;
            }
            other => return Err(not_ported(other)),
        }
    }
    Ok(valid)
}

/// Stores `data`'s values in a vector of native values of type `T`.
fn fill<T: NativeType, const N: usize>(vector: &mut Vector, data: &[u8], read: impl Fn(&[u8; N]) -> T) {
    let target = vector.data_mut::<T>();
    for (target, source) in target.iter_mut().zip(data.chunks_exact(N)) {
        *target = read(source.try_into().expect("N bytes"));
    }
}

pub(super) fn fill_vector(vector: &mut Vector, data: &[u8]) -> Result<()> {
    match vector.physical_type() {
        // anything but 0 and 1 would be undefined behavior as a `bool`
        PhysicalType::Bool => fill::<bool, 1>(vector, data, |b| b[0] != 0),
        PhysicalType::Int8 => fill::<i8, 1>(vector, data, |b| i8::from_le_bytes(*b)),
        PhysicalType::UInt8 => fill::<u8, 1>(vector, data, |b| b[0]),
        PhysicalType::Int16 => fill::<i16, 2>(vector, data, |b| i16::from_le_bytes(*b)),
        PhysicalType::UInt16 => fill::<u16, 2>(vector, data, |b| u16::from_le_bytes(*b)),
        PhysicalType::Int32 => fill::<i32, 4>(vector, data, |b| i32::from_le_bytes(*b)),
        PhysicalType::UInt32 => fill::<u32, 4>(vector, data, |b| u32::from_le_bytes(*b)),
        PhysicalType::Float => fill::<f32, 4>(vector, data, |b| f32::from_le_bytes(*b)),
        PhysicalType::Int64 => fill::<i64, 8>(vector, data, |b| i64::from_le_bytes(*b)),
        PhysicalType::UInt64 => fill::<u64, 8>(vector, data, |b| u64::from_le_bytes(*b)),
        PhysicalType::Double => fill::<f64, 8>(vector, data, |b| f64::from_le_bytes(*b)),
        PhysicalType::Int128 => fill::<i128, 16>(vector, data, |b| i128::from_le_bytes(*b)),
        PhysicalType::UInt128 => fill::<u128, 16>(vector, data, |b| u128::from_le_bytes(*b)),
        PhysicalType::Interval => fill::<Interval, 16>(vector, data, |b| Interval {
            months: i32::from_le_bytes(b[..4].try_into().expect("4 bytes")),
            days: i32::from_le_bytes(b[4..8].try_into().expect("4 bytes")),
            micros: i64::from_le_bytes(b[8..].try_into().expect("8 bytes")),
        }),
        other => return Err(corrupt(format!("a fixed size column of type {other:?}"))),
    }
    Ok(())
}

fn scan_list(buffer: &BufferManager, column: &PersistentColumn, ty: &LogicalType, rows: usize) -> Result<Vector> {
    let mut offsets = Vec::with_capacity(rows * 8);
    for pointer in &column.pointers {
        scan_fixed(buffer, pointer, PhysicalType::UInt64, 8, &mut offsets)?;
    }
    let mut vector = Vector::new(ty.clone(), rows);
    let mut previous = 0u64;
    let entries = vector.data_mut::<ListEntry>();
    for (entry, offset) in entries.iter_mut().zip(offsets.chunks_exact(8)) {
        let offset = u64::from_le_bytes(offset.try_into().expect("8 bytes"));
        let length = offset
            .checked_sub(previous)
            .ok_or_else(|| corrupted("Corrupted LIST column: later offset is smaller than the preceding offset"))?;
        *entry = ListEntry {
            offset: previous,
            length,
        };
        previous = offset;
    }
    let child_type = ty.child_type().ok_or_else(|| corrupt("a list has no child type"))?;
    let child_column = column
        .children
        .first()
        .ok_or_else(|| corrupt("a list has no child column"))?;
    let child_rows = previous as usize;
    let child = scan_column(buffer, child_column, child_type, child_rows)?;
    vector.set_list_size(child_rows);
    *vector.list_child_mut() = child;
    Ok(vector)
}

fn scan_array(buffer: &BufferManager, column: &PersistentColumn, ty: &LogicalType, rows: usize) -> Result<Vector> {
    let child_type = ty.child_type().ok_or_else(|| corrupt("an array has no child type"))?;
    let size = ty.array_size().ok_or_else(|| corrupt("an array has no size"))? as usize;
    let child_rows = rows.checked_mul(size).ok_or_else(|| corrupt("an array is too large"))?;
    let child_column = column
        .children
        .first()
        .ok_or_else(|| corrupt("an array has no child column"))?;
    let child = scan_column(buffer, child_column, child_type, child_rows)?;
    let mut vector = Vector::new(ty.clone(), rows);
    *vector.array_child_mut() = child;
    Ok(vector)
}

fn scan_struct(buffer: &BufferManager, column: &PersistentColumn, ty: &LogicalType, rows: usize) -> Result<Vector> {
    let fields = ty.struct_children();
    if column.children.len() != fields.len() {
        return Err(corrupt("a struct does not have the columns of its type"));
    }
    let children = column
        .children
        .iter()
        .zip(fields)
        .map(|(child, (_, field))| scan_column(buffer, child, field, rows))
        .collect::<Result<Vec<_>>>()?;
    let mut vector = Vector::new(ty.clone(), rows);
    for (target, child) in vector.struct_children_mut().iter_mut().zip(children) {
        *target = child;
    }
    Ok(vector)
}

/// `ColumnData::Scan` of all of a column, which is `rows` long.
fn scan_column(buffer: &BufferManager, column: &PersistentColumn, ty: &LogicalType, rows: usize) -> Result<Vector> {
    let validity = column
        .validity
        .as_ref()
        .ok_or_else(|| corrupt("a column has no validity"))?;
    let valid = scan_validity_column(buffer, validity, rows)?;
    if ty.id() == LogicalTypeId::Variant {
        let unshredded = duckrs_common::variant::unshredded_type();
        let child = column
            .children
            .first()
            .ok_or_else(|| corrupt("a VARIANT column has no data"))?;
        let stored = scan_column(buffer, child, &unshredded, rows)?;
        let mut vector = Vector::new(ty.clone(), rows);
        for (row, &valid) in valid.iter().enumerate() {
            if !valid {
                vector.set_null(row, true);
                continue;
            }
            let node = duckrs_common::variant::VariantNode::from_unshredded(&stored.get_value(row))?;
            vector.set_value(row, &Value::new(ty.clone(), ValueData::Bytes(node.encode()))?)?;
        }
        return Ok(vector);
    }
    let physical = ty.physical_type();
    if !matches!(physical, PhysicalType::Struct | PhysicalType::Array) {
        check_rows(column, rows)?;
    }
    let mut vector = match physical {
        PhysicalType::List => scan_list(buffer, column, ty, rows)?,
        PhysicalType::Array => scan_array(buffer, column, ty, rows)?,
        PhysicalType::Struct => scan_struct(buffer, column, ty, rows)?,
        PhysicalType::Varchar => {
            let mut strings = Vec::with_capacity(rows);
            let mut nulls = Vec::new();
            for pointer in &column.pointers {
                scan_strings(buffer, pointer, &mut strings, &mut nulls)?;
            }
            let mut vector = Vector::new(ty.clone(), rows);
            for (row, string) in strings.iter().enumerate() {
                vector.set_str(row, string);
            }
            for row in nulls {
                vector.set_null(row, true);
            }
            vector
        }
        _ => {
            let width = fixed_width(physical).ok_or_else(|| not_ported_column(ty))?;
            let mut data = Vec::with_capacity(rows * width);
            for pointer in &column.pointers {
                scan_fixed(buffer, pointer, physical, width, &mut data)?;
            }
            let mut vector = Vector::new(ty.clone(), rows);
            fill_vector(&mut vector, &data)?;
            vector
        }
    };
    for (row, &valid) in valid.iter().enumerate() {
        if !valid {
            vector.set_null(row, true);
        }
    }
    Ok(vector)
}

/// `ChunkInfoType`.
pub(super) const CONSTANT_INFO: u8 = 0;
pub(super) const VECTOR_INFO: u8 = 1;
const EMPTY_INFO: u8 = 2;
/// `ValiditySerialization`.
pub(super) const BITMASK: u8 = 0;
const VALID_VALUES: u8 = 1;
const INVALID_VALUES: u8 = 2;

/// `ValidityMask::Read` of a vector's mask, where a valid row is one that was deleted.
fn read_deleted_mask(data: &mut &[u8]) -> Result<[bool; STANDARD_VECTOR_SIZE]> {
    let flag = take::<1>(data)?[0];
    let mut deleted = [true; STANDARD_VECTOR_SIZE];
    if flag == BITMASK {
        for word in 0..STANDARD_VECTOR_SIZE / 64 {
            let bits = u64::from_le_bytes(take(data)?);
            for bit in 0..64 {
                deleted[word * 64 + bit] = bits >> bit & 1 != 0;
            }
        }
        return Ok(deleted);
    }
    if flag != VALID_VALUES && flag != INVALID_VALUES {
        return Err(corrupt(format!("unrecognized validity mask type {flag}")));
    }
    let count = u32::from_le_bytes(take(data)?) as usize;
    if count > STANDARD_VECTOR_SIZE {
        return Err(corrupt("a validity mask has too many entries"));
    }
    if flag == VALID_VALUES {
        deleted = [false; STANDARD_VECTOR_SIZE];
    }
    for _ in 0..count {
        let index = usize::from(u16::from_le_bytes(take(data)?));
        *deleted
            .get_mut(index)
            .ok_or_else(|| corrupt("a validity mask index is out of range"))? = flag == VALID_VALUES;
    }
    Ok(deleted)
}

/// `RowVersionManager::Deserialize`: which rows of a row group were deleted.
fn read_deletes(
    buffer: &BufferManager,
    chains: &mut MetadataChains,
    pointer: MetaBlockPointer,
    rows: usize,
) -> Result<Vec<bool>> {
    let mut deleted = vec![false; rows];
    let mut data = chains.at(buffer, pointer)?;
    let chunks = u64::from_le_bytes(take(&mut data)?);
    for _ in 0..chunks {
        let vector_index = u64::from_le_bytes(take(&mut data)?) as usize;
        let start = vector_index
            .checked_mul(STANDARD_VECTOR_SIZE)
            .filter(|&start| start < rows)
            .ok_or_else(|| corrupt("deletes of a vector past the end of the row group"))?;
        let end = rows.min(start + STANDARD_VECTOR_SIZE);
        match take::<1>(&mut data)?[0] {
            EMPTY_INFO => {}
            CONSTANT_INFO => {
                take::<8>(&mut data)?;
                deleted[start..end].fill(true);
            }
            VECTOR_INFO => {
                take::<8>(&mut data)?;
                let mask = read_deleted_mask(&mut data)?;
                deleted[start..end].copy_from_slice(&mask[..end - start]);
            }
            other => return Err(corrupt(format!("unrecognized chunk info type {other}"))),
        }
    }
    Ok(deleted)
}

/// The rows of a table in a file. They stay there, and are read a group and a column at a time.
#[derive(Debug)]
pub struct DiskTable {
    buffer: Arc<BufferManager>,
    table: StoredTable,
    chains: Mutex<MetadataChains>,
    /// Per group with deletes, its rows that are left. Those aren't many bytes, and the number of
    /// rows of a table is known without reading any.
    alive: Vec<Option<Vec<u32>>>,
    rows: usize,
}

impl DiskTable {
    pub fn new(buffer: Arc<BufferManager>, table: StoredTable) -> Result<Self> {
        let mut chains = MetadataChains::default();
        let mut alive = Vec::with_capacity(table.row_groups.len());
        let mut count = 0;
        for row_group in &table.row_groups {
            if row_group.data_pointers.len() != table.columns.len() {
                return Err(corrupt("a row group does not have the columns of its table"));
            }
            if row_group.tuple_count > MAX_ROW_GROUP_SIZE {
                return Err(corrupt("a row group is too large"));
            }
            let rows = row_group.tuple_count as usize;
            // only the first pointer leads anywhere: the others say which blocks that takes
            let left = match row_group.deletes_pointers.first() {
                None => None,
                Some(&deletes) => {
                    let deleted = read_deletes(&buffer, &mut chains, deletes, rows)?;
                    Some(
                        (0..rows as u32)
                            .filter(|&row| !deleted[row as usize])
                            .collect::<Vec<_>>(),
                    )
                }
            };
            count += left.as_ref().map_or(rows, Vec::len);
            alive.push(left);
        }
        Ok(Self {
            buffer,
            table,
            chains: Mutex::new(chains),
            alive,
            rows: count,
        })
    }

    pub fn next_row_id(&self) -> u64 {
        self.table.next_row_id
    }

    fn persistent_column(&self, group: usize, column: usize) -> Result<PersistentColumn> {
        let pointer = self.table.row_groups[group].data_pointers[column];
        let mut chains = self.chains.lock();
        let mut d = BinaryDeserializer::new(chains.at(&self.buffer, pointer)?);
        read_persistent_column(&mut d, Some(&self.table.columns[column].ty))
    }

    /// All rows, with the id of each if the caller wants them.
    pub fn read_rows(&self, mut row_ids: Option<&mut Vec<u64>>) -> Result<ColumnDataCollection> {
        let columns: Vec<usize> = (0..self.table.columns.len()).collect();
        let types = self.table.columns.iter().map(|c| c.ty.clone()).collect();
        let mut result = ColumnDataCollection::new(types);
        for (group, row_group) in self.table.row_groups.iter().enumerate() {
            result.append(&self.read_group(group, &columns)?);
            if let Some(row_ids) = row_ids.as_deref_mut() {
                let first = row_group.row_start;
                match &self.alive[group] {
                    Some(alive) => row_ids.extend(alive.iter().map(|&row| first + u64::from(row))),
                    None => row_ids.extend(first..first + row_group.tuple_count),
                }
            }
        }
        Ok(result)
    }

    /// A bound of a column as a value of its type.
    fn bound(&self, column: usize, bytes: Option<&[u8; 16]>) -> Result<Option<Value>> {
        let ty = &self.table.columns[column].ty;
        let width = fixed_width(ty.physical_type());
        let (Some(bytes), Some(width)) = (bytes, width.filter(|_| has_ordered_statistics(ty))) else {
            return Ok(None);
        };
        let mut vector = Vector::new(ty.clone(), 1);
        fill_vector(&mut vector, &bytes[..width])?;
        Ok(Some(vector.get_value(0)))
    }
}

impl TableSource for DiskTable {
    fn row_count(&self) -> usize {
        self.rows
    }
    fn row_ids(&self) -> RowIds {
        let mut ranges = Vec::new();
        for (row_group, alive) in self.table.row_groups.iter().zip(&self.alive) {
            let first = row_group.row_start;
            match alive {
                Some(alive) => ranges.extend(alive.iter().map(|&row| (first + u64::from(row), 1))),
                None => ranges.push((first, row_group.tuple_count as usize)),
            }
        }
        let groups = self.table.row_groups.iter();
        let groups = groups
            .map(|group| (group.row_start, group.tuple_count as usize))
            .collect();
        RowIds::of_ranges(ranges, self.table.next_row_id).with_stored_groups(groups)
    }

    fn group_count(&self) -> usize {
        self.table.row_groups.len()
    }

    fn group_rows(&self, group: usize) -> usize {
        match &self.alive[group] {
            Some(alive) => alive.len(),
            None => self.table.row_groups[group].tuple_count as usize,
        }
    }

    fn read_group(&self, group: usize, columns: &[usize]) -> Result<DataChunk> {
        let rows = self.table.row_groups[group].tuple_count as usize;
        let mut vectors = Vec::with_capacity(columns.len());
        for &column in columns {
            let stored = self.persistent_column(group, column)?;
            vectors.push(scan_column(
                &self.buffer,
                &stored,
                &self.table.columns[column].ty,
                rows,
            )?);
        }
        let Some(alive) = &self.alive[group] else {
            return Ok(DataChunk::from_columns(vectors, rows));
        };
        let selection = SelectionVector::from_vec(alive.clone());
        for vector in &mut vectors {
            vector.slice(&selection, alive.len());
        }
        Ok(DataChunk::from_columns(vectors, alive.len()))
    }

    fn read_all(&self) -> Result<ColumnDataCollection> {
        self.read_rows(None)
    }

    fn column_statistics(&self) -> Option<Vec<ColumnStatistics>> {
        if self.table.column_statistics.len() != self.table.columns.len() {
            return None;
        }
        let statistics = self.table.column_statistics.iter().enumerate();
        statistics
            .map(|(column, stored)| {
                let unknown = ColumnStatistics {
                    min: None,
                    max: None,
                    can_be_null: true,
                    can_be_valid: true,
                };
                let Some(stored) = stored else {
                    return Some(unknown);
                };
                Some(ColumnStatistics {
                    min: self.bound(column, stored.min.as_ref()).ok()?,
                    max: self.bound(column, stored.max.as_ref()).ok()?,
                    can_be_null: stored.has_null,
                    can_be_valid: stored.has_no_null,
                })
            })
            .collect()
    }

    fn segments(&self, include_info: bool) -> Result<Vec<SegmentInfo>> {
        fn add(
            buffer: Option<&BufferManager>,
            result: &mut Vec<SegmentInfo>,
            stored: &PersistentColumn,
            group: u64,
            column: usize,
            path: &[usize],
            ty: Option<&LogicalType>,
        ) -> Result<()> {
            let type_name = ty.map_or_else(|| "VALIDITY".to_string(), ToString::to_string);
            let mut start = 0;
            let text = path.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
            for (index, pointer) in stored.pointers.iter().enumerate() {
                result.push(SegmentInfo {
                    row_group: group,
                    column,
                    path: format!("[{text}]"),
                    segment: index as u64,
                    segment_type: type_name.clone(),
                    start,
                    count: pointer.tuple_count as u64,
                    compression: storage_name(pointer.compression),
                    block_id: pointer.block_id,
                    block_offset: pointer.offset as u64,
                    info: match buffer {
                        Some(buffer) if pointer.block_id >= 0 => {
                            let handle = segment(buffer, pointer)?;
                            super::compression::segment_info(
                                pointer.compression,
                                &handle[pointer.offset..],
                                pointer.tuple_count,
                            )?
                        }
                        _ => None,
                    },
                });
                start += pointer.tuple_count as u64;
            }
            if let Some(validity) = &stored.validity {
                let mut child = path.to_vec();
                child.push(0);
                add(buffer, result, validity, group, column, &child, None)?;
            }
            for (index, inner) in stored.children.iter().enumerate() {
                let mut child = path.to_vec();
                child.push(index + 1);
                let name = match ty.map(LogicalType::physical_type) {
                    Some(PhysicalType::Struct) => ty.map(|ty| &ty.struct_children()[index].1),
                    _ => ty.and_then(LogicalType::child_type),
                };
                add(buffer, result, inner, group, column, &child, name)?;
            }
            Ok(())
        }
        let mut result = Vec::new();
        let buffer = include_info.then_some(&*self.buffer);
        for group in 0..self.table.row_groups.len() {
            for column in 0..self.table.columns.len() {
                let stored = self.persistent_column(group, column)?;
                let ty = &self.table.columns[column].ty;
                add(buffer, &mut result, &stored, group as u64, column, &[column], Some(ty))?;
            }
        }
        Ok(result)
    }

    fn zones(&self, column: usize) -> Result<Vec<ColumnZone>> {
        let mut zones = Vec::with_capacity(self.table.row_groups.len());
        for group in 0..self.table.row_groups.len() {
            let stored = self.persistent_column(group, column)?;
            // nulls are in the validity column, so any group might have some
            let mut zone = ColumnZone {
                has_null: true,
                ..ColumnZone::default()
            };
            for pointer in &stored.pointers {
                let (min, max) = (
                    self.bound(column, pointer.statistics.min.as_ref())?,
                    self.bound(column, pointer.statistics.max.as_ref())?,
                );
                let (Some(min), Some(max)) = (min, max) else {
                    zone.unknown = true;
                    break;
                };
                if zone
                    .min
                    .as_ref()
                    .is_none_or(|old| compare_values(ComparisonOp::LessThan, &min, old) == Some(true))
                {
                    zone.min = Some(min);
                }
                if zone
                    .max
                    .as_ref()
                    .is_none_or(|old| compare_values(ComparisonOp::GreaterThan, &max, old) == Some(true))
                {
                    zone.max = Some(max);
                }
            }
            zone.unknown |= zone.min.is_none();
            zones.push(zone);
        }
        Ok(zones)
    }
}

/// `PersistentRowGroupData`: rows that a large insert wrote to the file ahead of its commit.
pub(super) struct PersistentRowGroup {
    types: Vec<LogicalType>,
    columns: Vec<PersistentColumn>,
    count: u64,
}

/// `PersistentCollectionData::Deserialize`.
pub(super) fn read_collection_data(d: &mut BinaryDeserializer<'_>, field_id: u16) -> Result<Vec<PersistentRowGroup>> {
    d.read_object(field_id, "row_group_data", |d| {
        d.read_list(100, "row_groups", |d| {
            d.read_object_value(|d| {
                let types: Vec<LogicalType> = d.read_property(100, "types")?;
                let mut types_left = types.iter();
                let columns = d.read_list(101, "columns", |d| {
                    let ty = types_left
                        .next()
                        .ok_or_else(|| corrupt("a row group has too many columns"))?;
                    read_persistent_column(d, Some(ty))
                })?;
                d.read_property::<u64>(102, "start")?;
                let count = d.read_property(103, "count")?;
                Ok(PersistentRowGroup { types, columns, count })
            })
        })
    })
}

/// The rows of a row group of a log.
pub(super) fn scan_row_group(buffer: &BufferManager, row_group: &PersistentRowGroup) -> Result<DataChunk> {
    if row_group.count > MAX_ROW_GROUP_SIZE || row_group.columns.len() != row_group.types.len() {
        return Err(corrupt("a row group of a log is not what it says"));
    }
    let rows = row_group.count as usize;
    let columns = row_group
        .types
        .iter()
        .zip(&row_group.columns)
        .map(|(ty, column)| scan_column(buffer, column, ty, rows))
        .collect::<Result<Vec<_>>>()?;
    Ok(DataChunk::from_columns(columns, rows))
}
