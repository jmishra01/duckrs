//! Ported from duckdb/src/storage/checkpoint_manager.cpp, metadata/metadata_writer.cpp,
//! single_file_block_manager.cpp (`WriteHeader`), table/column_checkpoint_state.cpp and
//! compression/{numeric_constant,string_uncompressed,validity_uncompressed}.cpp @ rust-port-baseline.
//!
//! Writes a checkpoint: the whole database into blocks that the last checkpoint doesn't use, and
//! then a new header. A crash anywhere before the header leaves the last checkpoint as it was.
//!
//! Files are written as v2.0.0, with everything uncompressed: compression is the analysis of each
//! codec, and it changes nothing about the format. Segments of a size that fits share blocks, or
//! small tables would take a block per column.

use super::block_manager::{DatabaseHeader, INVALID_BLOCK};
use super::buffer_manager::BufferManager;
use super::checkpoint::{self, STORAGE_VERSION_V2_0_0, StoredEntry};
use super::compression::{COMPRESSION_CONSTANT, COMPRESSION_UNCOMPRESSED};
use super::metadata::{MetaBlockPointer, metadata_block_size};
use super::serialization::generated::enums::{catalog_type, constraint_type, table_column_type};
use super::serialization::generated::{self as g};
use super::serialization::to_stored::{self, write_not_ported};
use super::table_data::{DiskTable, fixed_width};
use crate::catalog::{Catalog, SchemaCatalogEntry, TEMP_SCHEMA, TableCatalogEntry, TableConstraint, TableCursor};
use crate::planner::binder::Binder;
use duckrs_common::serializer::{BinarySerializer, SerializationOptions, Serialize};
use duckrs_common::types::{Interval, ListEntry, LogicalType, LogicalTypeId, PhysicalType, StringT};
use duckrs_common::vector::{SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{self, Expr};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

/// `DEFAULT_ROW_GROUP_SIZE`, in chunks.
const CHUNKS_PER_ROW_GROUP: usize = 60;
/// `StringUncompressed::STRING_BLOCK_LIMIT`.
const STRING_BLOCK_LIMIT: usize = 4096;
const STRING_HEADER_SIZE: usize = 8;
const BIG_STRING_MARKER_SIZE: usize = 12;
const SUB_BLOCK_HEADER_SIZE: usize = 8;
const METADATA_BLOCK_COUNT: usize = 64;
/// `CompressionType::COMPRESSION_AUTO`.
const COMPRESSION_AUTO: u64 = 0;

fn internal(message: &str) -> Error {
    Error::new(ErrorKind::Internal, message)
}

/// A position in one of the chains of metadata that a checkpoint writes.
#[derive(Default)]
struct MetaStream {
    /// the sub-block, as a page and an index in it
    current: Option<(usize, usize)>,
    offset: usize,
}

/// What is known of the values of a segment or a column. Without bounds a reader assumes nothing.
#[derive(Debug, Clone)]
struct Stats {
    has_null: bool,
    has_no_null: bool,
    bounds: Option<([u8; 16], [u8; 16])>,
}

impl Stats {
    fn unknown() -> Self {
        Self {
            has_null: true,
            has_no_null: true,
            bounds: None,
        }
    }

    fn of_validity(valid: &[bool]) -> Self {
        Self {
            has_null: valid.iter().any(|v| !v),
            has_no_null: valid.iter().any(|v| *v),
            bounds: None,
        }
    }

    fn merge(&mut self, other: &Self, physical: PhysicalType) {
        self.has_null |= other.has_null;
        self.has_no_null |= other.has_no_null;
        self.bounds = match (self.bounds, other.bounds) {
            (Some(a), Some(b)) => {
                let min = if compare(physical, &b.0, &a.0) == Ordering::Less {
                    b.0
                } else {
                    a.0
                };
                let max = if compare(physical, &b.1, &a.1) == Ordering::Greater {
                    b.1
                } else {
                    a.1
                };
                Some((min, max))
            }
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
    }
}

/// NaN sorts last, as it does in `LessThan` of C++.
fn compare_floats(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).expect("neither is NaN"),
    }
}

fn compare(physical: PhysicalType, a: &[u8; 16], b: &[u8; 16]) -> Ordering {
    macro_rules! ints {
        ($t:ty) => {{
            let width = size_of::<$t>();
            let read = |x: &[u8; 16]| <$t>::from_le_bytes(x[..width].try_into().expect("the width"));
            read(a).cmp(&read(b))
        }};
    }
    macro_rules! floats {
        ($t:ty) => {{
            let width = size_of::<$t>();
            let read = |x: &[u8; 16]| f64::from(<$t>::from_le_bytes(x[..width].try_into().expect("the width")));
            compare_floats(read(a), read(b))
        }};
    }
    match physical {
        PhysicalType::Bool | PhysicalType::UInt8 => ints!(u8),
        PhysicalType::Int8 => ints!(i8),
        PhysicalType::Int16 => ints!(i16),
        PhysicalType::UInt16 => ints!(u16),
        PhysicalType::Int32 => ints!(i32),
        PhysicalType::UInt32 => ints!(u32),
        PhysicalType::Int64 => ints!(i64),
        PhysicalType::UInt64 => ints!(u64),
        PhysicalType::Int128 => ints!(i128),
        PhysicalType::UInt128 => ints!(u128),
        PhysicalType::Float => floats!(f32),
        PhysicalType::Double => floats!(f64),
        _ => Ordering::Equal,
    }
}

/// The values of a flat vector as a segment has them.
fn raw_bytes(vector: &Vector, count: usize) -> Result<Vec<u8>> {
    macro_rules! plain {
        ($t:ty) => {
            vector.data::<$t>()[..count]
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        };
    }
    Ok(match vector.physical_type() {
        PhysicalType::Bool => vector.data::<bool>()[..count].iter().map(|&b| u8::from(b)).collect(),
        PhysicalType::Int8 => plain!(i8),
        PhysicalType::UInt8 => plain!(u8),
        PhysicalType::Int16 => plain!(i16),
        PhysicalType::UInt16 => plain!(u16),
        PhysicalType::Int32 => plain!(i32),
        PhysicalType::UInt32 => plain!(u32),
        PhysicalType::Float => plain!(f32),
        PhysicalType::Int64 => plain!(i64),
        PhysicalType::UInt64 => plain!(u64),
        PhysicalType::Double => plain!(f64),
        PhysicalType::Int128 => plain!(i128),
        PhysicalType::UInt128 => plain!(u128),
        PhysicalType::Interval => vector.data::<Interval>()[..count]
            .iter()
            .flat_map(|v| {
                let mut bytes = [0; 16];
                bytes[..4].copy_from_slice(&v.months.to_le_bytes());
                bytes[4..8].copy_from_slice(&v.days.to_le_bytes());
                bytes[8..].copy_from_slice(&v.micros.to_le_bytes());
                bytes
            })
            .collect(),
        other => return Err(internal(&format!("no fixed width for {other:?}"))),
    })
}

/// The smallest and largest value of the rows that aren't NULL.
fn bounds(physical: PhysicalType, data: &[u8], width: usize, valid: &[bool]) -> Option<([u8; 16], [u8; 16])> {
    if physical == PhysicalType::Interval {
        return None;
    }
    let mut result: Option<([u8; 16], [u8; 16])> = None;
    for (value, _) in data.chunks_exact(width).zip(valid).filter(|(_, valid)| **valid) {
        let mut bytes = [0; 16];
        bytes[..width].copy_from_slice(value);
        result = Some(match result {
            None => (bytes, bytes),
            Some((min, max)) => (
                if compare(physical, &bytes, &min) == Ordering::Less {
                    bytes
                } else {
                    min
                },
                if compare(physical, &bytes, &max) == Ordering::Greater {
                    bytes
                } else {
                    max
                },
            ),
        });
    }
    result
}

/// `NumericStats::Serialize` of a bound, of the type it has.
fn write_bound(s: &mut BinarySerializer, field: u16, tag: &str, physical: PhysicalType, value: Option<&[u8; 16]>) {
    s.write_object(field, tag, |s| {
        s.write_property(100, "has_value", &value.is_some());
        let Some(bytes) = value else { return };
        macro_rules! value {
            ($t:ty) => {{
                let width = size_of::<$t>();
                let value = <$t>::from_le_bytes(bytes[..width].try_into().expect("the width"));
                s.write_property(101, "value", &value);
            }};
        }
        match physical {
            PhysicalType::Bool => s.write_property(101, "value", &(bytes[0] != 0)),
            PhysicalType::Int8 => value!(i8),
            PhysicalType::UInt8 => value!(u8),
            PhysicalType::Int16 => value!(i16),
            PhysicalType::UInt16 => value!(u16),
            PhysicalType::Int32 => value!(i32),
            PhysicalType::UInt32 => value!(u32),
            PhysicalType::Int64 => value!(i64),
            PhysicalType::UInt64 => value!(u64),
            PhysicalType::Int128 => value!(i128),
            PhysicalType::UInt128 => value!(u128),
            PhysicalType::Float => value!(f32),
            PhysicalType::Double => value!(f64),
            _ => {}
        }
    });
}

/// `BaseStatistics::Serialize`. `ty` is `None` for a validity column.
fn write_stats(s: &mut BinarySerializer, ty: Option<&LogicalType>, stats: &Stats) {
    s.write_property(100, "has_null", &stats.has_null);
    s.write_property(101, "has_no_null", &stats.has_no_null);
    s.write_property(102, "distinct_count", &0u64);
    s.write_object(103, "type_stats", |s| {
        let Some(ty) = ty.filter(|ty| ty.id() != LogicalTypeId::SqlNull) else {
            return;
        };
        let child = |s: &mut BinarySerializer, ty: Option<&LogicalType>| {
            s.write_object_value(|s| write_stats(s, ty, &Stats::unknown()));
        };
        match ty.physical_type() {
            // what `StringStatsField` says of no bounds, no lengths and that there can be unicode
            PhysicalType::Varchar => s.write_property(206, "packed_field", &0u32),
            PhysicalType::Struct => {
                let children = ty.struct_children();
                s.write_list(200, "child_stats", children.len(), |s, i| {
                    child(s, Some(&children[i].1))
                });
            }
            PhysicalType::List | PhysicalType::Array => {
                s.write_object(200, "child_stats", |s| {
                    write_stats(s, ty.child_type(), &Stats::unknown())
                });
            }
            PhysicalType::Bit | PhysicalType::Invalid | PhysicalType::Unknown => {}
            physical => {
                let (min, max) = stats.bounds.as_ref().map(|(a, b)| (a, b)).unzip();
                write_bound(s, 200, "min", physical, min);
                write_bound(s, 201, "max", physical, max);
            }
        }
    });
}

/// `DataPointer`.
struct SegmentOut {
    tuple_count: usize,
    block_id: i64,
    offset: u32,
    compression: u8,
    stats: Stats,
    /// Blocks of the strings that didn't fit.
    overflow_blocks: Vec<i64>,
}

/// `PersistentColumnData`.
struct ColumnOut {
    segments: Vec<SegmentOut>,
    validity: Option<Box<ColumnOut>>,
    children: Vec<ColumnOut>,
    stats: Stats,
}

fn write_segment(s: &mut BinarySerializer, segment: &SegmentOut, ty: Option<&LogicalType>) {
    s.write_object_value(|s| {
        s.write_property_with_default(101, "tuple_count", &(segment.tuple_count as u64));
        s.write_object(102, "block_pointer", |s| {
            s.write_property(100, "block_id", &segment.block_id);
            s.write_property_with_default(101, "offset", &segment.offset);
        });
        s.write_property(103, "compression_type", &segment.compression);
        s.write_object(104, "statistics", |s| write_stats(s, ty, &segment.stats));
        if !segment.overflow_blocks.is_empty() {
            s.write_field(105);
            true.write_value(s);
            s.write_object_value(|s| s.write_property(1, "overflow_blocks", &segment.overflow_blocks));
        }
    });
}

/// `PersistentColumnData::Serialize`, without the object around it.
fn write_column(s: &mut BinarySerializer, column: &ColumnOut, ty: Option<&LogicalType>) {
    if !column.segments.is_empty() {
        s.write_list(100, "data_pointers", column.segments.len(), |s, i| {
            write_segment(s, &column.segments[i], ty);
        });
    }
    let Some(ty) = ty else { return };
    if let Some(validity) = &column.validity {
        s.write_object(101, "validity", |s| write_column(s, validity, None));
    }
    match ty.physical_type() {
        PhysicalType::List | PhysicalType::Array => {
            s.write_object(102, "child_column", |s| {
                write_column(s, &column.children[0], ty.child_type())
            });
        }
        PhysicalType::Struct => {
            let fields = ty.struct_children();
            s.write_list(102, "sub_columns", column.children.len(), |s, i| {
                s.write_object_value(|s| write_column(s, &column.children[i], Some(&fields[i].1)));
            });
        }
        _ => {}
    }
}

/// A piece of a column: a flat vector and how many rows it has.
struct Part {
    vector: Vector,
    count: usize,
}

impl Part {
    fn new(vector: &Vector, count: usize) -> Self {
        let mut vector = vector.clone();
        vector.flatten(count);
        Self { vector, count }
    }
}

/// One checkpoint in the making.
struct Writer<'a> {
    buffer: &'a BufferManager,
    catalog: &'a Catalog,
    name: String,
    block_size: usize,
    free: BTreeSet<u64>,
    next_block: u64,
    used: BTreeSet<u64>,
    /// How many segments each block holds, which DuckDB has to know to free them one by one.
    segments_in_block: HashMap<u64, u32>,
    /// The block that segments go into now, with how much of it they fill.
    open: Option<(u64, Vec<u8>, usize)>,
    /// Metadata blocks, which go out at the end.
    pages: Vec<(u64, Vec<u8>)>,
    /// Per page, which of its sub-blocks are taken.
    taken: Vec<u64>,
    next_sub_block: Option<(usize, usize)>,
    /// Sub-blocks that were put aside for what comes next.
    reserved: VecDeque<(usize, usize)>,
    tables: MetaStream,
}

impl<'a> Writer<'a> {
    fn new(buffer: &'a BufferManager, catalog: &'a Catalog, name: String) -> Self {
        let blocks = buffer.block_manager();
        Self {
            buffer,
            catalog,
            name,
            block_size: blocks.block_size(),
            free: blocks.free_list().into_iter().collect(),
            next_block: blocks.header().block_count,
            used: BTreeSet::new(),
            segments_in_block: HashMap::new(),
            open: None,
            pages: Vec::new(),
            taken: Vec::new(),
            next_sub_block: None,
            reserved: VecDeque::new(),
            tables: MetaStream::default(),
        }
    }

    /// `GetFreeBlockIdForCheckpoint`: never one that the last checkpoint needs.
    fn allocate_block(&mut self) -> u64 {
        let block = self.free.pop_first().unwrap_or_else(|| {
            self.next_block += 1;
            self.next_block - 1
        });
        self.used.insert(block);
        block
    }

    fn flush_open_block(&mut self) -> Result<()> {
        if let Some((block, data, _)) = self.open.take() {
            self.buffer.block_manager().write_block(block, &data)?;
        }
        Ok(())
    }

    /// Puts a segment in the open block, or a new one if it doesn't fit.
    fn place(&mut self, data: &[u8]) -> Result<(i64, u32)> {
        let at = self.open.as_ref().map(|(_, _, used)| used.next_multiple_of(8));
        if at.is_none_or(|at| at + data.len() > self.block_size) {
            self.flush_open_block()?;
            self.open = Some((self.allocate_block(), vec![0; self.block_size], 0));
        }
        let (block, buffer, used) = self.open.as_mut().expect("just ensured");
        let at = used.next_multiple_of(8);
        buffer[at..at + data.len()].copy_from_slice(data);
        *used = at + data.len();
        *self.segments_in_block.entry(*block).or_default() += 1;
        Ok((*block as i64, at as u32))
    }

    /// The disk pointer of a new sub-block, which starts out as the last of its chain.
    fn next_sub_block(&mut self) -> (usize, usize) {
        if let Some(reserved) = self.reserved.pop_front() {
            return reserved;
        }
        let (page, index) = match self.next_sub_block {
            Some(next) => next,
            None => {
                let block = self.allocate_block();
                self.pages.push((block, vec![0; self.block_size]));
                self.taken.push(0);
                (self.pages.len() - 1, 0)
            }
        };
        self.next_sub_block = (index + 1 < METADATA_BLOCK_COUNT).then_some((page, index + 1));
        self.taken[page] |= 1 << index;
        let size = metadata_block_size(self.block_size);
        self.pages[page].1[index * size..index * size + 8].copy_from_slice(&INVALID_BLOCK.to_le_bytes());
        (page, index)
    }

    fn disk_pointer(&self, (page, index): (usize, usize)) -> u64 {
        self.pages[page].0 | (index as u64) << 56
    }

    /// `MetadataWriter::GetMetaBlockPointer`.
    fn pointer(&mut self, stream: &mut MetaStream) -> MetaBlockPointer {
        let size = metadata_block_size(self.block_size);
        if stream.current.is_none() || stream.offset >= size {
            self.advance(stream);
        }
        let current = stream.current.expect("just ensured");
        MetaBlockPointer::new(self.disk_pointer(current), stream.offset as u32)
    }

    fn advance(&mut self, stream: &mut MetaStream) {
        let next = self.next_sub_block();
        if let Some((page, index)) = stream.current {
            let size = metadata_block_size(self.block_size);
            let pointer = self.disk_pointer(next);
            self.pages[page].1[index * size..index * size + 8].copy_from_slice(&pointer.to_le_bytes());
        }
        stream.current = Some(next);
        stream.offset = SUB_BLOCK_HEADER_SIZE;
    }

    /// `MetadataWriter::WriteData`.
    fn write_metadata(&mut self, stream: &mut MetaStream, mut data: &[u8]) {
        let size = metadata_block_size(self.block_size);
        while !data.is_empty() {
            if stream.current.is_none() || stream.offset >= size {
                self.advance(stream);
            }
            let (page, index) = stream.current.expect("just ensured");
            let take = data.len().min(size - stream.offset);
            let at = index * size + stream.offset;
            self.pages[page].1[at..at + take].copy_from_slice(&data[..take]);
            stream.offset += take;
            data = &data[take..];
        }
    }

    fn write_fixed(&mut self, ty: &LogicalType, parts: &[Part], valid: &[bool]) -> Result<Vec<SegmentOut>> {
        let physical = ty.physical_type();
        let width = fixed_width(physical).ok_or_else(|| write_not_ported(&format!("a column of type {ty}")))?;
        let mut data = Vec::with_capacity(valid.len() * width);
        for part in parts {
            data.extend_from_slice(&raw_bytes(&part.vector, part.count)?);
        }
        let rows_per_segment = self.block_size / width;
        let mut segments = Vec::new();
        for (bytes, valid) in data
            .chunks(rows_per_segment * width)
            .zip(valid.chunks(rows_per_segment))
        {
            let (block_id, offset) = self.place(bytes)?;
            let mut stats = Stats::of_validity(valid);
            stats.bounds = bounds(physical, bytes, width, valid);
            segments.push(SegmentOut {
                tuple_count: valid.len(),
                block_id,
                offset,
                compression: COMPRESSION_UNCOMPRESSED,
                stats,
                overflow_blocks: Vec::new(),
            });
        }
        Ok(segments)
    }

    /// `WriteString`, of a string that doesn't fit in a dictionary: a length and the string in a
    /// block of the segment, over as many as it takes, each with the next one in its last 8 bytes.
    fn write_overflow(&mut self, string: &[u8], overflow: &mut Vec<(u64, Vec<u8>, usize)>) -> Result<(i64, i32)> {
        let space = self.block_size - 8;
        if overflow.last().is_none_or(|(_, _, used)| used + 4 >= space) {
            overflow.push((self.allocate_block(), vec![0; self.block_size], 0));
        }
        let (block, data, used) = overflow.last_mut().expect("just ensured");
        let start = (*block as i64, *used as i32);
        data[*used..*used + 4].copy_from_slice(&(string.len() as u32).to_le_bytes());
        *used += 4;
        let mut rest = string;
        loop {
            let (_, data, used) = overflow.last_mut().expect("just ensured");
            let take = rest.len().min(space - *used);
            data[*used..*used + take].copy_from_slice(&rest[..take]);
            *used += take;
            rest = &rest[take..];
            if rest.is_empty() {
                return Ok(start);
            }
            let next = self.allocate_block();
            let (_, data, _) = overflow.last_mut().expect("just ensured");
            data[space..].copy_from_slice(&next.to_le_bytes());
            overflow.push((next, vec![0; self.block_size], 0));
        }
    }

    fn write_strings(&mut self, parts: &[Part], valid: &[bool]) -> Result<Vec<SegmentOut>> {
        let limit = STRING_BLOCK_LIMIT.min((self.block_size / 4) & !7);
        let mut strings: Vec<&[u8]> = Vec::with_capacity(valid.len());
        for part in parts {
            let data = part.vector.data::<StringT>();
            for (row, string) in data.iter().enumerate().take(part.count) {
                strings.push(if part.vector.is_valid(row) {
                    // SAFETY: the row is valid, so its string lives in the heap of the vector
                    unsafe { string.as_bytes() }
                } else {
                    &[]
                });
            }
        }
        let mut segments = Vec::new();
        let mut start = 0;
        while start < strings.len() {
            let mut entries: Vec<Vec<u8>> = Vec::new();
            let mut negative = Vec::new();
            let mut overflow = Vec::new();
            let mut dictionary = 0;
            let mut row = start;
            while row < strings.len() {
                let string = strings[row];
                let big = string.len() >= limit;
                let size = if big { BIG_STRING_MARKER_SIZE } else { string.len() };
                if STRING_HEADER_SIZE + (entries.len() + 1) * 4 + dictionary + size > self.block_size {
                    break;
                }
                if big {
                    let (block, offset) = self.write_overflow(string, &mut overflow)?;
                    let mut marker = Vec::with_capacity(BIG_STRING_MARKER_SIZE);
                    marker.extend_from_slice(&block.to_le_bytes());
                    marker.extend_from_slice(&offset.to_le_bytes());
                    entries.push(marker);
                } else {
                    entries.push(string.to_vec());
                }
                negative.push(big);
                dictionary += size;
                row += 1;
            }
            let end = STRING_HEADER_SIZE + entries.len() * 4 + dictionary;
            let mut segment = vec![0; end];
            segment[..4].copy_from_slice(&(dictionary as u32).to_le_bytes());
            segment[4..8].copy_from_slice(&(end as u32).to_le_bytes());
            let mut total = 0;
            for (index, (entry, big)) in entries.iter().zip(&negative).enumerate() {
                total += entry.len();
                let offset = if *big { -(total as i32) } else { total as i32 };
                let at = STRING_HEADER_SIZE + index * 4;
                segment[at..at + 4].copy_from_slice(&offset.to_le_bytes());
                segment[end - total..end - total + entry.len()].copy_from_slice(entry);
            }
            let (block_id, offset) = self.place(&segment)?;
            let overflow_blocks = overflow.iter().map(|(block, _, _)| *block as i64).collect();
            for (block, data, _) in overflow {
                self.buffer.block_manager().write_block(block, &data)?;
            }
            segments.push(SegmentOut {
                tuple_count: row - start,
                block_id,
                offset,
                compression: COMPRESSION_UNCOMPRESSED,
                stats: Stats::of_validity(&valid[start..row]),
                overflow_blocks,
            });
            start = row;
        }
        Ok(segments)
    }

    /// A validity column: nothing at all if the segment is the same throughout.
    fn write_validity(&mut self, valid: &[bool]) -> Result<ColumnOut> {
        let stats = Stats::of_validity(valid);
        let mut segments = Vec::new();
        if !valid.is_empty() && (!stats.has_null || !stats.has_no_null) {
            segments.push(SegmentOut {
                tuple_count: valid.len(),
                block_id: -1,
                offset: 0,
                compression: COMPRESSION_CONSTANT,
                stats: stats.clone(),
                overflow_blocks: Vec::new(),
            });
        } else {
            for valid in valid.chunks(self.block_size / 8 * 64) {
                let mut words = vec![0u8; valid.len().div_ceil(64) * 8];
                for (row, _) in valid.iter().enumerate().filter(|(_, valid)| **valid) {
                    words[row / 8] |= 1 << (row % 8);
                }
                let (block_id, offset) = self.place(&words)?;
                segments.push(SegmentOut {
                    tuple_count: valid.len(),
                    block_id,
                    offset,
                    compression: COMPRESSION_UNCOMPRESSED,
                    stats: Stats::of_validity(valid),
                    overflow_blocks: Vec::new(),
                });
            }
        }
        Ok(ColumnOut {
            segments,
            validity: None,
            children: Vec::new(),
            stats,
        })
    }

    /// `ColumnData::Checkpoint`.
    fn write_column_data(&mut self, ty: &LogicalType, parts: &[Part]) -> Result<ColumnOut> {
        if matches!(ty.id(), LogicalTypeId::Geometry | LogicalTypeId::Variant) {
            return Err(write_not_ported(&format!("a {} column", ty.id().name())));
        }
        let rows = parts.iter().map(|part| part.count).sum();
        let mut valid = Vec::with_capacity(rows);
        for part in parts {
            valid.extend((0..part.count).map(|row| part.vector.is_valid(row)));
        }
        let validity = Box::new(self.write_validity(&valid)?);
        let mut stats = Stats::of_validity(&valid);
        let physical = ty.physical_type();
        let (segments, children) = match physical {
            PhysicalType::Varchar => (self.write_strings(parts, &valid)?, Vec::new()),
            PhysicalType::List => {
                let mut offsets = Vec::with_capacity(rows * 8);
                let mut child_parts = Vec::with_capacity(parts.len());
                let mut total = 0u64;
                for part in parts {
                    let entries = part.vector.data::<ListEntry>();
                    let mut selection = Vec::new();
                    for (row, entry) in entries.iter().enumerate().take(part.count) {
                        if part.vector.is_valid(row) {
                            selection.extend((entry.offset..entry.offset + entry.length).map(|i| i as u32));
                            total += entry.length;
                        }
                        offsets.extend_from_slice(&total.to_le_bytes());
                    }
                    let mut child = part.vector.list_child().clone();
                    let count = selection.len();
                    child.slice(&SelectionVector::from_vec(selection), count);
                    child_parts.push(Part::new(&child, count));
                }
                let child_type = ty.child_type().ok_or_else(|| internal("a list has no child type"))?;
                let child = self.write_column_data(child_type, &child_parts)?;
                let mut segments = Vec::new();
                let rows_per_segment = self.block_size / 8;
                for (bytes, valid) in offsets.chunks(rows_per_segment * 8).zip(valid.chunks(rows_per_segment)) {
                    let (block_id, offset) = self.place(bytes)?;
                    segments.push(SegmentOut {
                        tuple_count: valid.len(),
                        block_id,
                        offset,
                        compression: COMPRESSION_UNCOMPRESSED,
                        stats: Stats::of_validity(valid),
                        overflow_blocks: Vec::new(),
                    });
                }
                (segments, vec![child])
            }
            PhysicalType::Array => {
                let size = ty.array_size().ok_or_else(|| internal("an array has no size"))? as usize;
                let child_type = ty.child_type().ok_or_else(|| internal("an array has no child type"))?;
                let child_parts: Vec<Part> = parts
                    .iter()
                    .map(|part| Part::new(part.vector.array_child(), part.count * size))
                    .collect();
                (Vec::new(), vec![self.write_column_data(child_type, &child_parts)?])
            }
            PhysicalType::Struct => {
                let fields = ty.struct_children();
                let mut children = Vec::with_capacity(fields.len());
                for (index, (_, field)) in fields.iter().enumerate() {
                    let child_parts: Vec<Part> = parts
                        .iter()
                        .map(|part| Part::new(&part.vector.struct_children()[index], part.count))
                        .collect();
                    children.push(self.write_column_data(field, &child_parts)?);
                }
                (Vec::new(), children)
            }
            _ => (self.write_fixed(ty, parts, &valid)?, Vec::new()),
        };
        for segment in &segments {
            stats.merge(&segment.stats, physical);
        }
        Ok(ColumnOut {
            segments,
            validity: Some(validity),
            children,
            stats,
        })
    }

    /// `RowGroup::WriteToDisk`.
    fn write_row_group(
        &mut self,
        table: &TableCatalogEntry,
        parts: Vec<Vec<Part>>,
        row_start: u64,
        rows: usize,
        statistics: &mut [Option<Stats>],
    ) -> Result<Vec<u8>> {
        let mut tables = std::mem::take(&mut self.tables);
        let mut pointers = Vec::with_capacity(parts.len());
        for (column, parts) in parts.iter().enumerate() {
            let ty = &table.columns[column].ty;
            let out = self.write_column_data(ty, parts)?;
            match &mut statistics[column] {
                Some(stats) => stats.merge(&out.stats, ty.physical_type()),
                empty => *empty = Some(out.stats.clone()),
            }
            let mut s = BinarySerializer::new(SerializationOptions::default());
            s.write_object_value(|s| write_column(s, &out, Some(ty)));
            pointers.push(self.pointer(&mut tables));
            self.write_metadata(&mut tables, &s.into_bytes());
        }
        self.tables = tables;
        let mut s = BinarySerializer::new(SerializationOptions::default());
        s.write_object_value(|s| {
            s.write_property(100, "row_start", &row_start);
            s.write_property(101, "tuple_count", &(rows as u64));
            s.write_property(102, "data_pointers", &pointers);
            s.write_property(103, "delete_pointers", &Vec::<MetaBlockPointer>::new());
            s.write_property(104, "has_metadata_blocks", &false);
        });
        Ok(s.into_bytes())
    }

    /// `SingleFileTableDataWriter::FinalizeTable`: where the rows of a table are, and how many.
    fn write_table_data(&mut self, table: &TableCatalogEntry) -> Result<(MetaBlockPointer, u64)> {
        let types = table.types();
        let columns: Vec<usize> = (0..types.len()).collect();
        let total = table.row_count();
        let chunks = total.div_ceil(STANDARD_VECTOR_SIZE);
        let mut cursor = table
            .source()
            .map(|source| TableCursor::new(source, columns.clone(), types.clone()));
        let memory = if cursor.is_none() { Some(table.rows()?) } else { None };
        let mut statistics: Vec<Option<Stats>> = vec![None; types.len()];
        let mut row_groups = Vec::new();
        let mut row_start = 0u64;
        for first in (0..chunks).step_by(CHUNKS_PER_ROW_GROUP) {
            let mut parts: Vec<Vec<Part>> = columns.iter().map(|_| Vec::new()).collect();
            let mut rows = 0;
            for index in first..chunks.min(first + CHUNKS_PER_ROW_GROUP) {
                let chunk = match (&mut cursor, &memory) {
                    (Some(cursor), _) => cursor.chunk(index)?,
                    (None, Some(memory)) => memory.chunk(index).clone(),
                    (None, None) => unreachable!("one of the two is set"),
                };
                rows += chunk.len();
                for (column, parts) in parts.iter_mut().enumerate() {
                    parts.push(Part::new(chunk.column(column), chunk.len()));
                }
            }
            row_groups.push(self.write_row_group(table, parts, row_start, rows, &mut statistics)?);
            row_start += rows as u64;
        }
        let mut s = BinarySerializer::new(SerializationOptions::default());
        s.write_object_value(|s| {
            s.write_list(100, "column_stats", types.len(), |s, column| {
                true.write_value(s);
                let stats = statistics[column].clone().unwrap_or_else(Stats::unknown);
                s.write_object_value(|s| {
                    s.write_object(100, "statistics", |s| write_stats(s, Some(&types[column]), &stats));
                });
            });
        });
        let mut tables = std::mem::take(&mut self.tables);
        let pointer = self.pointer(&mut tables);
        self.write_metadata(&mut tables, &s.into_bytes());
        self.write_metadata(&mut tables, &(row_groups.len() as u64).to_le_bytes());
        for row_group in &row_groups {
            self.write_metadata(&mut tables, row_group);
        }
        self.tables = tables;
        Ok((pointer, total as u64))
    }

    fn entry_path(&self, schema: &SchemaCatalogEntry, name: &str) -> Vec<String> {
        vec![self.name.clone(), schema.name.clone(), name.to_string()]
    }

    fn create_table_info(&self, table: &TableCatalogEntry) -> Result<g::CreateTableInfo> {
        let mut columns = Vec::with_capacity(table.columns.len());
        let mut constraints = Vec::new();
        let boxed = |constraint: g::Constraint| Some(Box::new(constraint));
        for (index, column) in table.logical_columns().iter().enumerate() {
            let expression = match (
                column.generated,
                column.definition.and_then(|d| d.default_text.as_deref()),
            ) {
                (Some(expr), _) => Some(expr.clone()),
                (None, Some(text)) => Some(parse_expression(text)?),
                (None, None) => None,
            };
            columns.push(g::ColumnDefinition {
                name: column.name.to_string(),
                type_: column.ty.clone(),
                expression: expression
                    .as_ref()
                    .map(|expr| to_stored::expression(expr).map(Box::new))
                    .transpose()?,
                category: if column.generated.is_some() {
                    table_column_type::GENERATED
                } else {
                    table_column_type::STANDARD
                },
                compression_type: COMPRESSION_AUTO,
                comment: None,
                tags: Vec::new(),
            });
            if column.definition.is_some_and(|d| d.not_null) {
                constraints.push(boxed(g::Constraint {
                    type_: constraint_type::NOT_NULL,
                    kind: g::ConstraintKind::NotNullConstraint(g::NotNullConstraint { index: index as u64 }),
                }));
            }
        }
        for constraint in &table.constraints {
            match constraint {
                TableConstraint::Check { parsed, .. } => constraints.push(boxed(g::Constraint {
                    type_: constraint_type::CHECK,
                    kind: g::ConstraintKind::CheckConstraint(g::CheckConstraint {
                        expression: Some(Box::new(to_stored::expression(parsed)?)),
                    }),
                })),
                _ => return Err(write_not_ported("keys, which need the ART of Phase 10,")),
            }
        }
        Ok(g::CreateTableInfo {
            table: table.name.clone(),
            columns: g::ColumnList { columns },
            constraints,
            query: None,
            partition_keys: Vec::new(),
            sort_keys: Vec::new(),
            options: Vec::new(),
        })
    }

    /// The entries of a schema, in an order in which each can be read: tables can use macros and
    /// sequences in their defaults, and C++ doesn't bind macros or views when it reads them.
    fn write_schema(&mut self, schema: &Arc<SchemaCatalogEntry>, entries: &mut Vec<Vec<u8>>) -> Result<()> {
        let simple = |entry_type: u64, info: g::CreateInfo| {
            let mut s = BinarySerializer::new(SerializationOptions::default());
            s.write_object_value(|s| {
                s.write_property(99, "type", &entry_type);
                s.write_property(100, "info", &Some(Box::new(info)));
            });
            s.into_bytes()
        };
        let info = to_stored::create_info(
            catalog_type::SCHEMA_ENTRY,
            vec![schema.name.clone(), String::new()],
            g::CreateInfoKind::CreateSchemaInfo(g::CreateSchemaInfo { options: Vec::new() }),
        );
        entries.push(simple(catalog_type::SCHEMA_ENTRY, info));
        for (name, logical_type) in schema.type_entries() {
            let path = self.entry_path(schema, &name);
            let kind = g::CreateInfoKind::CreateTypeInfo(g::CreateTypeInfo { name, logical_type });
            entries.push(simple(
                catalog_type::TYPE_ENTRY,
                to_stored::create_info(catalog_type::TYPE_ENTRY, path, kind),
            ));
        }
        for sequence in schema.sequence_entries() {
            let data = sequence.data();
            let kind = g::CreateInfoKind::CreateSequenceInfo(g::CreateSequenceInfo {
                name: sequence.name.clone(),
                usage_count: u64::from(data.last_value.is_some()),
                increment: data.increment,
                min_value: data.min_value,
                max_value: data.max_value,
                start_value: data.counter,
                cycle: data.cycle,
                last_value: data.last_value,
            });
            let path = self.entry_path(schema, &sequence.name);
            entries.push(simple(
                catalog_type::SEQUENCE_ENTRY,
                to_stored::create_info(catalog_type::SEQUENCE_ENTRY, path, kind),
            ));
        }
        let binder = Binder::new(self.catalog);
        for table_macro in [false, true] {
            let entry_type = if table_macro {
                catalog_type::TABLE_MACRO_ENTRY
            } else {
                catalog_type::MACRO_ENTRY
            };
            for entry in schema.macro_entries(table_macro) {
                let bind = |ty: &ast::CastType| binder.bind_type(ty);
                let mut macros = entry
                    .macros
                    .iter()
                    .map(|function| to_stored::macro_function(function, &bind));
                let function = macros.next().ok_or_else(|| internal("a macro has no function"))??;
                let extra_functions = macros
                    .map(|function| function.map(|f| Some(Box::new(f))))
                    .collect::<Result<_>>()?;
                let kind = g::CreateInfoKind::CreateMacroInfo(g::CreateMacroInfo {
                    name: entry.name.clone(),
                    function: Some(Box::new(function)),
                    extra_functions,
                });
                let path = self.entry_path(schema, &entry.name);
                entries.push(simple(entry_type, to_stored::create_info(entry_type, path, kind)));
            }
        }
        let mut views = Vec::new();
        for entry in schema.tables_and_views() {
            match entry {
                crate::catalog::TableOrView::View(view) => views.push(view),
                crate::catalog::TableOrView::Table(table) => {
                    if !table.indexes.read().is_empty() {
                        return Err(write_not_ported("indexes, which need the ART of Phase 10,"));
                    }
                    let info = self.create_table_info(&table)?;
                    let (pointer, rows) = self.write_table_data(&table)?;
                    let info = to_stored::create_info(
                        catalog_type::TABLE_ENTRY,
                        self.entry_path(schema, &table.name),
                        g::CreateInfoKind::CreateTableInfo(info),
                    );
                    let mut s = BinarySerializer::new(SerializationOptions::default());
                    s.write_object_value(|s| {
                        s.write_property(99, "type", &catalog_type::TABLE_ENTRY);
                        s.write_property(100, "info", &Some(Box::new(info)));
                        s.write_property(101, "table_pointer", &pointer);
                        s.write_property(102, "total_rows", &rows);
                        s.write_property(105, "next_row_id", &rows);
                    });
                    entries.push(s.into_bytes());
                }
            }
        }
        for view in views {
            let kind = g::CreateInfoKind::CreateViewInfo(g::CreateViewInfo {
                view_name: view.name.clone(),
                aliases: view.aliases.clone(),
                types: Vec::new(),
                query: Some(Box::new(to_stored::select_statement(&view.query)?)),
                names: Vec::new(),
                column_comments: Vec::new(),
                column_comments_map: Vec::new(),
                security_type: None,
            });
            let path = self.entry_path(schema, &view.name);
            entries.push(simple(
                catalog_type::VIEW_ENTRY,
                to_stored::create_info(catalog_type::VIEW_ENTRY, path, kind),
            ));
        }
        Ok(())
    }

    /// `WriteHeader`: the free list, the metadata, and then the header that makes it all count.
    fn finish(mut self, root: MetaBlockPointer) -> Result<()> {
        self.flush_open_block()?;
        let multi_use: Vec<(u64, u32)> = self
            .segments_in_block
            .iter()
            .filter(|(_, segments)| **segments > 1)
            .map(|(&block, &segments)| (block, segments))
            .collect();
        // `GetFreeListBlocks`: the room for the free list has to be taken before it is known, and
        // the blocks that takes are no longer free. So this takes enough for any outcome, and the
        // sub-blocks it doesn't need stay free, which the list of metadata blocks says.
        let payload = metadata_block_size(self.block_size) - SUB_BLOCK_HEADER_SIZE;
        let mut pages = self.pages.len();
        loop {
            let entries = self.next_block as usize + self.free.len() + pages;
            let bytes = 24 + 8 * entries + 12 * multi_use.len() + 16 * pages;
            let needed = bytes.div_ceil(payload).div_ceil(METADATA_BLOCK_COUNT) + 1;
            if self.pages.len() + needed <= pages {
                break;
            }
            pages = self.pages.len() + needed;
        }
        let needed = pages - self.pages.len();
        let mut sub_blocks = Vec::with_capacity(needed * METADATA_BLOCK_COUNT);
        for _ in 0..needed * METADATA_BLOCK_COUNT {
            sub_blocks.push(self.next_sub_block());
        }
        let free: Vec<u64> = (0..self.next_block)
            .filter(|block| !self.used.contains(block))
            .collect();
        let size = 24 + free.len() * 8 + multi_use.len() * 12 + self.pages.len() * 16;
        sub_blocks.truncate(size.div_ceil(payload));
        for &(page, index) in &sub_blocks {
            self.taken[page] |= 1 << index;
        }
        // those beyond what the list takes are free after all
        for page in 0..self.pages.len() {
            if page >= pages - needed {
                self.taken[page] = 0;
            }
        }
        for &(page, index) in &sub_blocks {
            self.taken[page] |= 1 << index;
        }
        let mut list = Vec::with_capacity(size);
        list.extend_from_slice(&(free.len() as u64).to_le_bytes());
        for block in &free {
            list.extend_from_slice(&block.to_le_bytes());
        }
        list.extend_from_slice(&(multi_use.len() as u64).to_le_bytes());
        for (block, segments) in &multi_use {
            list.extend_from_slice(&block.to_le_bytes());
            list.extend_from_slice(&segments.to_le_bytes());
        }
        list.extend_from_slice(&(self.pages.len() as u64).to_le_bytes());
        for ((block, _), taken) in self.pages.iter().zip(&self.taken) {
            list.extend_from_slice(&block.to_le_bytes());
            list.extend_from_slice(&(!taken).to_le_bytes());
        }
        self.reserved = sub_blocks.into();
        let mut stream = MetaStream::default();
        let free_list = self.pointer(&mut stream).block_pointer;
        self.write_metadata(&mut stream, &list);
        let blocks = self.buffer.block_manager();
        for (block, data) in &self.pages {
            blocks.write_block(*block, data)?;
        }
        blocks.write_header(DatabaseHeader {
            meta_block: root.block_pointer,
            free_list,
            block_count: self.next_block,
            storage_compatibility: STORAGE_VERSION_V2_0_0,
            ..blocks.header()
        })?;
        blocks.set_free_list(free.into_iter().collect(), multi_use.into_iter().collect());
        Ok(())
    }
}

fn parse_expression(text: &str) -> Result<Expr> {
    let statements = duckrs_parser::parse_ast(&format!("SELECT {text}"))?;
    if let [ast::Statement::Select(select)] = statements.as_slice()
        && let ast::QueryNodeKind::Select(node) = &select.node.kind
        && let [expr] = node.select_list.as_slice()
    {
        return Ok(expr.clone());
    }
    Err(internal("a default that isn't an expression"))
}

/// `SingleFileCheckpointWriter::CreateCheckpoint`: makes the file at `path` hold what is in
/// `catalog`, and leaves the tables of the catalog in it.
pub fn write_checkpoint(path: &Path, buffer: &Arc<BufferManager>, catalog: &Catalog, name: &str) -> Result<()> {
    let mut writer = Writer::new(buffer, catalog, name.to_string());
    let mut entries = Vec::new();
    for schema in catalog.schemas() {
        if !schema.name.eq_ignore_ascii_case(TEMP_SCHEMA) {
            writer.write_schema(&schema, &mut entries)?;
        }
    }
    let mut s = BinarySerializer::new(SerializationOptions::default());
    s.write_object_value(|s| {
        s.write_list(100, "catalog_entries", entries.len(), |s, i| s.write_raw(&entries[i]));
    });
    let mut root = MetaStream::default();
    let pointer = writer.pointer(&mut root);
    writer.write_metadata(&mut root, &s.into_bytes());
    writer.finish(pointer)?;
    // what the log held is in the file now
    let mut log = path.as_os_str().to_owned();
    log.push(".wal");
    match std::fs::remove_file(&log) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::new(ErrorKind::Io, format!("{}: {error}", path.display()))),
    }
    // the blocks of the last checkpoint are free now, so what reads a table has to read the new ones
    buffer.clear();
    for entry in checkpoint::read_catalog(buffer)?.entries {
        if let StoredEntry::Table(table) = entry {
            let schema = table.schema.clone();
            let name = table.name.clone();
            let disk = Arc::new(DiskTable::new(buffer.clone(), *table)?);
            catalog.get_table(Some(&schema), &name)?.leave_in_file(disk);
        }
    }
    Ok(())
}
