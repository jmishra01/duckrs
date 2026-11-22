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

use super::block_manager::{DEFAULT_BLOCK_ALLOC_SIZE, DatabaseHeader, INVALID_BLOCK};
use super::buffer_manager::BufferManager;
use super::checkpoint::{self, StoredEntry};
use super::compression::{
    COMPRESSION_ALP, COMPRESSION_ALPRD, COMPRESSION_BITPACKING, COMPRESSION_CONSTANT, COMPRESSION_DICT_FSST,
    COMPRESSION_DICTIONARY, COMPRESSION_EMPTY, COMPRESSION_FSST, COMPRESSION_RLE, COMPRESSION_ROARING,
    COMPRESSION_UNCOMPRESSED, COMPRESSION_ZSTD,
};
use super::encode::{
    Encoded, encode_alp, encode_alprd, encode_old_dictionary, encode_old_fsst, encode_rle, encode_roaring, fill_nulls,
    is_signed, supports_bitpacking, supports_rle,
};
use super::fsst::SymbolTable;
use super::index_data::TableIndex;
use super::metadata::{MetaBlockPointer, metadata_block_size};
use super::serialization::generated::enums::{
    catalog_type, constraint_type, foreign_key_type, index_constraint_type, table_column_type,
};
use super::serialization::generated::{self as g};
use super::serialization::to_stored::{self, write_not_ported};
use super::table_data::{BITMASK, CONSTANT_INFO, DiskTable, INVALID_VALUES, VALID_VALUES, VECTOR_INFO, fixed_width};
use crate::catalog::{
    Catalog, IndexCatalogEntry, MacroCatalogEntry, RowIds, SchemaCatalogEntry, SequenceCatalogEntry, TEMP_SCHEMA,
    TableCatalogEntry, TableConstraint, TableCursor, ViewCatalogEntry,
};
use crate::optimizer::distinct_statistics::{DistinctState, type_is_supported};
use crate::planner::binder::Binder;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::serializer::StorageVersion;
use duckrs_common::serializer::{BinarySerializer, SerializationOptions, Serialize};
use duckrs_common::types::{Interval, ListEntry, LogicalType, LogicalTypeId, PhysicalType, StringT};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{self, Expr};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

/// What a database or a session asks of its checkpoints.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointOptions<'a> {
    /// what the file is written in
    pub storage_version: StorageVersion,
    pub force_compression: &'a str,
    /// `disabled_compression_methods`: the names of the methods no column is compressed with, in lower case
    pub disabled_compression: &'a [String],
    /// `debug_force_bitpacking_mode`
    pub bitpacking_mode: &'a str,
    /// `zstd_min_string_length`
    pub zstd_min_string_length: usize,
    /// In rows: `ATTACH ... (ROW_GROUP_SIZE)` checks that it is whole vectors.
    pub row_group_size: u64,
    /// How many times row groups can come together, which gives their rows new ids. There is no
    /// such thing while a transaction goes by the ids it knows (`InitializeVacuumState`).
    pub vacuum_tasks: usize,
    /// `vacuum_rebuild_indexes`: of tables with up to this many rows, indexes are built again after a vacuum
    /// that gives rows new ids, where they couldn't be remapped (`VacuumIndexStrategy::REBUILD`); 0 for never
    pub vacuum_rebuild_indexes: u64,
    /// `experimental_metadata_reuse`: row groups and columns that stay the same keep their metadata
    pub metadata_reuse: bool,
    /// `force_column_metadata_reuse`: columns keep theirs on their own from v1.4.0 on, not only from v2.0.0
    pub force_column_metadata_reuse: bool,
    /// `debug_checkpoint_abort`, in lower case: where to stop as a crash would.
    pub abort: &'a str,
    /// Transactions are reading tables from the blocks they are in: those stay as they are.
    pub keep_free_blocks: bool,
    /// `variant_minimum_shredding_size`: the rows a row group needs for its VARIANTs to be shredded,
    /// -1 for never
    pub variant_minimum_shredding_size: i64,
    /// `force_variant_shredding`: the type VARIANTs are shredded on, rather than the one their values
    /// suggest
    pub force_variant_shredding: Option<&'a LogicalType>,
}

/// `DEBUG_ABORT_*`: what was written so far stays, and the next open has to cope with it.
fn check_abort(abort: &str, point: &str, message: &str) -> Result<()> {
    if abort != point {
        return Ok(());
    }
    let kind = if point.ends_with("non_fatal") {
        ErrorKind::Io
    } else {
        ErrorKind::Fatal
    };
    Err(Error::new(kind, message))
}
/// `StringUncompressed::STRING_BLOCK_LIMIT`.
const STRING_BLOCK_LIMIT: usize = 4096;
const STRING_HEADER_SIZE: usize = 8;
const BIG_STRING_MARKER_SIZE: usize = 12;
const SUB_BLOCK_HEADER_SIZE: usize = 8;
use super::buffer_manager::PROVISIONAL_BLOCK;
/// The next provisional id of a block an append fills.
static NEXT_PROVISIONAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A block segments go into: its id, its bytes, and how much of it they fill.
type PartialBlock = (u64, Arc<parking_lot::Mutex<Vec<u8>>>, usize);

/// `PartialBlockManager` of an append (`APPEND_TO_TABLE`): the blocks its row groups fill, which stay open between the
/// row groups it writes until its commit flushes them (`FinalFlush`).
#[derive(Debug, Default)]
pub struct AppendBlocks {
    partial: std::collections::BTreeMap<(usize, u64), PartialBlock>,
    count: u64,
}

impl AppendBlocks {
    /// `Rollback`: the blocks still open go; nothing was written of them.
    pub fn clear(&mut self, buffer: &BufferManager) {
        let ids: Vec<u64> = self.partial.values().map(|(id, _, _)| *id).collect();
        buffer.drop_provisional(&ids);
        self.partial.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.partial.is_empty()
    }
}
const METADATA_BLOCK_COUNT: usize = 64;
/// `CompressionType::COMPRESSION_AUTO`.
const COMPRESSION_AUTO: u64 = 0;

fn internal(message: &str) -> Error {
    Error::new(ErrorKind::Internal, message)
}

/// A position in one of the chains of metadata that a checkpoint writes.
#[derive(Default)]
struct MetaStream {
    /// the sub-block, as its metadata block and its index there
    current: Option<(u64, usize)>,
    offset: usize,
}

/// What is known of the values of a segment or a column. Without bounds a reader assumes nothing.
#[derive(Debug, Clone)]
struct Stats {
    has_null: bool,
    has_no_null: bool,
    bounds: Option<([u8; 16], [u8; 16])>,
    /// of a GEOMETRY column or segment
    geometry: Option<duckrs_common::geometry::GeometryStats>,
    /// of a VARCHAR or BLOB column or segment; without them, `NO_STATS`
    strings: Option<StringStatsOut>,
    /// of a shredded VARIANT column: the type of its shredded part, and the statistics of that
    variant_shredded: Option<Box<(LogicalType, Stats)>>,
    /// of a VARIANT column whose row groups are shredded differently (`INCONSISTENT`)
    variant_inconsistent: bool,
    /// of a LIST or ARRAY its element's, of a STRUCT each field's; empty when not known
    children: Vec<Stats>,
    /// of a LIST, the lengths of its lists (`ListStatsData`), if known
    lists: Option<crate::catalog::ListLengths>,
}

/// `StringStatsData` as `StatsWriter<string_t>` works it out: the bounds in full (a serialization keeps a
/// prefix), `None` for a segment of only NULLs (`EMPTY_STATS`).
#[derive(Debug, Clone)]
struct StringStatsOut {
    bounds: Option<(Vec<u8>, Vec<u8>)>,
    has_unicode: bool,
    max_length: u32,
    min_length: u32,
    total_length: Option<u64>,
}

impl StringStatsOut {
    fn of(strings: impl Iterator<Item = Vec<u8>>, varchar: bool) -> Self {
        let mut result = Self {
            bounds: None,
            has_unicode: false,
            max_length: 0,
            min_length: u32::MAX,
            total_length: Some(0),
        };
        for string in strings {
            let length = u32::try_from(string.len()).unwrap_or(u32::MAX);
            result.max_length = result.max_length.max(length);
            result.min_length = result.min_length.min(length);
            result.total_length = result.total_length.and_then(|t| t.checked_add(string.len() as u64));
            result.has_unicode |= varchar && string.iter().any(|b| *b >= 0x80);
            result.bounds = Some(match result.bounds.take() {
                None => (string.clone(), string),
                Some((min, max)) => (
                    if string < min { string.clone() } else { min },
                    if string > max { string } else { max },
                ),
            });
        }
        result
    }

    /// `StringStats::Merge`
    fn merge(&mut self, other: &Self) {
        self.bounds = match (self.bounds.take(), &other.bounds) {
            (Some((a_min, a_max)), Some((b_min, b_max))) => Some((a_min.min(b_min.clone()), a_max.max(b_max.clone()))),
            (a, b) => a.or_else(|| b.clone()),
        };
        self.has_unicode |= other.has_unicode;
        self.max_length = self.max_length.max(other.max_length);
        self.min_length = self.min_length.min(other.min_length);
        self.total_length = match (self.total_length, other.total_length) {
            (Some(a), Some(b)) => a.checked_add(b),
            _ => None,
        };
    }
}

impl Stats {
    fn unknown() -> Self {
        Self {
            has_null: true,
            has_no_null: true,
            bounds: None,
            geometry: None,
            strings: None,
            variant_shredded: None,
            variant_inconsistent: false,
            children: Vec::new(),
            lists: None,
        }
    }

    fn of_validity(valid: &[bool]) -> Self {
        Self {
            has_null: valid.iter().any(|v| !v),
            has_no_null: valid.iter().any(|v| *v),
            bounds: None,
            geometry: None,
            strings: None,
            variant_shredded: None,
            variant_inconsistent: false,
            children: Vec::new(),
            lists: None,
        }
    }

    /// `BaseStatistics::Merge`, of the statistics of a column of type `ty`.
    fn merge(&mut self, other: &Self, ty: &LogicalType) {
        let physical = ty.physical_type();
        // `ListStats::Merge`: of row groups that are not the same, the totals add up
        self.lists = match (self.lists, other.lists) {
            (Some(a), Some(b)) => Some(a.merge(&b, true)),
            _ => None,
        };
        if self.children.len() == other.children.len() {
            let types = child_types(ty);
            for (at, (child, other)) in self.children.iter_mut().zip(&other.children).enumerate() {
                if let Some(child_type) = types.get(at) {
                    child.merge(other, child_type);
                }
            }
        } else {
            self.children.clear();
        }
        // `VariantStats::Merge`: shredded alike or INCONSISTENT
        if ty.id() == LogicalTypeId::Variant {
            let merged = match (&self.variant_shredded, &other.variant_shredded) {
                _ if self.variant_inconsistent || other.variant_inconsistent => None,
                (Some(a), Some(b)) => merge_shredding((&a.0, &a.1), (&b.0, &b.1)),
                (None, None) => {
                    self.has_null |= other.has_null;
                    self.has_no_null |= other.has_no_null;
                    return;
                }
                _ => None,
            };
            self.variant_inconsistent = merged.is_none();
            self.variant_shredded = merged.map(Box::new);
        }
        self.has_null |= other.has_null;
        self.has_no_null |= other.has_no_null;
        self.geometry = match (self.geometry, other.geometry) {
            (Some(mut a), Some(b)) => {
                a.merge(&b);
                Some(a)
            }
            (a, b) => a.or(b),
        };
        self.strings = match (self.strings.take(), &other.strings) {
            (Some(mut a), Some(b)) => {
                a.merge(b);
                Some(a)
            }
            (a, b) => a.or_else(|| b.clone()),
        };
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

/// The types of the children of `ty` that statistics have: a LIST's or ARRAY's element, a STRUCT's fields.
fn child_types(ty: &LogicalType) -> Vec<LogicalType> {
    match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Array => ty.child_type().cloned().into_iter().collect(),
        LogicalTypeId::Struct => ty.struct_children().iter().map(|(_, ty)| ty.clone()).collect(),
        _ => Vec::new(),
    }
}

/// `VariantStats::GetTypedStats`: of `STRUCT(typed_value, ...)` its `typed_value`, else itself.
fn typed_stats<'a>(ty: &'a LogicalType, stats: &'a Stats) -> Option<(&'a LogicalType, &'a Stats)> {
    if ty.id() != LogicalTypeId::Struct {
        return Some((ty, stats));
    }
    Some((&ty.struct_children().first()?.1, stats.children.first()?))
}

/// `VariantStats::GetUntypedStats`: the statistics of `untyped_value_index`, if there is one.
fn untyped_stats(ty: &LogicalType, stats: &Stats) -> Option<Stats> {
    (ty.id() == LogicalTypeId::Struct && ty.struct_children().len() == 2)
        .then(|| stats.children.get(1).cloned().unwrap_or_else(Stats::unknown))
}

/// `WrapTypedValue`: the shredding of these typed value statistics, with an `untyped_value_index`.
fn wrap_typed_value(typed: (LogicalType, Stats), untyped: Option<Stats>) -> (LogicalType, Stats) {
    let nested = matches!(
        typed.0.id(),
        LogicalTypeId::Struct | LogicalTypeId::List | LogicalTypeId::Array
    );
    if untyped.is_none() && !nested {
        return typed;
    }
    let mut fields = vec![("typed_value".to_string(), typed.0)];
    let mut children = vec![typed.1];
    if let Some(untyped) = untyped {
        fields.push(("untyped_value_index".to_string(), LogicalType::UINTEGER));
        children.push(untyped);
    }
    // `BaseStatistics::CreateEmpty`
    let mut stats = Stats::of_validity(&[]);
    stats.children = children;
    (LogicalType::struct_type(fields), stats)
}

/// `VariantStats::MergeShredding`: of two shreddings the fields they share as alike, `None` if nothing is.
fn merge_shredding(a: (&LogicalType, &Stats), b: (&LogicalType, &Stats)) -> Option<(LogicalType, Stats)> {
    let (a_type, a_typed) = typed_stats(a.0, a.1)?;
    let (b_type, b_typed) = typed_stats(b.0, b.1)?;
    let untyped = match (untyped_stats(a.0, a.1), untyped_stats(b.0, b.1)) {
        (Some(mut x), Some(y)) => {
            x.merge(&y, &LogicalType::UINTEGER);
            Some(x)
        }
        (x, y) => x.or(y),
    };
    let combined_validity = || {
        let mut stats = Stats::of_validity(&[]);
        stats.has_null = a_typed.has_null || b_typed.has_null;
        stats.has_no_null = a_typed.has_no_null || b_typed.has_no_null;
        stats
    };
    match a_type.id() {
        LogicalTypeId::Struct => {
            if b_type.id() != LogicalTypeId::Struct {
                return None;
            }
            let b_fields = b_type.struct_children();
            let mut fields = Vec::new();
            let mut children = Vec::new();
            for (at, (name, field)) in a_type.struct_children().iter().enumerate() {
                let Some(other_at) = b_fields.iter().position(|(other, _)| other == name) else {
                    continue;
                };
                let other = &b_fields[other_at].1;
                if other.id() != field.id() {
                    continue;
                }
                let (Some(x), Some(y)) = (a_typed.children.get(at), b_typed.children.get(other_at)) else {
                    continue;
                };
                if let Some((ty, stats)) = merge_shredding((field, x), (other, y)) {
                    fields.push((name.clone(), ty));
                    children.push(stats);
                }
            }
            if fields.is_empty() {
                return None;
            }
            let mut typed = combined_validity();
            typed.children = children;
            Some(wrap_typed_value((LogicalType::struct_type(fields), typed), untyped))
        }
        LogicalTypeId::List => {
            if b_type.id() != LogicalTypeId::List {
                return None;
            }
            let (x, y) = (a_typed.children.first()?, b_typed.children.first()?);
            let (child_type, child) = merge_shredding((a_type.child_type()?, x), (b_type.child_type()?, y))?;
            let mut typed = combined_validity();
            typed.children = vec![child];
            Some(wrap_typed_value((LogicalType::list(child_type), typed), untyped))
        }
        _ => {
            if a_type != b_type {
                return None;
            }
            let mut typed = a_typed.clone();
            typed.merge(b_typed, a_type);
            Some(wrap_typed_value((a_type.clone(), typed), untyped))
        }
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

pub(super) fn compare(physical: PhysicalType, a: &[u8; 16], b: &[u8; 16]) -> Ordering {
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
pub(super) fn raw_bytes(vector: &Vector, count: usize) -> Result<Vec<u8>> {
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
/// `NumericStats::CreateEmpty`: the greatest value as the minimum, the least as the maximum (for floating
/// point NaN and the lowest).
fn empty_bounds(physical: PhysicalType) -> Option<([u8; 16], [u8; 16])> {
    let mut min = [0u8; 16];
    let mut max = [0u8; 16];
    macro_rules! limits {
        ($low:expr, $high:expr) => {{
            let (low, high) = ($low.to_le_bytes(), $high.to_le_bytes());
            min[..high.len()].copy_from_slice(&high);
            max[..low.len()].copy_from_slice(&low);
        }};
    }
    match physical {
        PhysicalType::Bool => limits!(0u8, 1u8),
        PhysicalType::Int8 => limits!(i8::MIN, i8::MAX),
        PhysicalType::UInt8 => limits!(u8::MIN, u8::MAX),
        PhysicalType::Int16 => limits!(i16::MIN, i16::MAX),
        PhysicalType::UInt16 => limits!(u16::MIN, u16::MAX),
        PhysicalType::Int32 => limits!(i32::MIN, i32::MAX),
        PhysicalType::UInt32 => limits!(u32::MIN, u32::MAX),
        PhysicalType::Int64 => limits!(i64::MIN, i64::MAX),
        PhysicalType::UInt64 => limits!(u64::MIN, u64::MAX),
        PhysicalType::Int128 => limits!(i128::MIN, i128::MAX),
        PhysicalType::UInt128 => limits!(u128::MIN, u128::MAX),
        PhysicalType::Float => limits!(f32::MIN, f32::NAN),
        PhysicalType::Double => limits!(f64::MIN, f64::NAN),
        _ => return None,
    }
    Some((min, max))
}

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

/// `StringStats::Serialize`, of no stats (`NO_STATS`) without `strings`.
fn write_string_stats(s: &mut BinarySerializer, strings: Option<&StringStatsOut>) {
    const LEGACY_SIZE: usize = 8;
    const CURRENT_SIZE: usize = 12;
    const MAXIMUM_MIN_STRING_LENGTH: u32 = (1 << 21) - 1;
    if !s.should_serialize(StorageVersion::V2_0_0) {
        // `LegacyConstructMinMax`: no stats are the lowest min and the highest max, empty ones the other way
        // around, and bounds their first bytes padded with zeros
        let legacy = |bound: Option<&Vec<u8>>, fill: u8| -> [u8; LEGACY_SIZE] {
            let mut result = [fill; LEGACY_SIZE];
            if let Some(bound) = bound {
                result = [0; LEGACY_SIZE];
                let size = bound.len().min(LEGACY_SIZE);
                result[..size].copy_from_slice(&bound[..size]);
            }
            result
        };
        let (min, max) = match strings {
            None => ([0u8; LEGACY_SIZE], [0xFF; LEGACY_SIZE]),
            Some(strings) => (
                legacy(strings.bounds.as_ref().map(|(min, _)| min), 0xFF),
                legacy(strings.bounds.as_ref().map(|(_, max)| max), 0x00),
            ),
        };
        for (field, bytes) in [(200, min), (201, max)] {
            s.write_field(field);
            s.write_varint_unsigned(LEGACY_SIZE as u64);
            s.write_raw(&bytes);
        }
        s.write_property(202, "has_unicode", &strings.is_none_or(|strings| strings.has_unicode));
        s.write_property(203, "has_max_string_length", &strings.is_some());
        s.write_property(
            204,
            "max_string_length",
            &strings.map_or(0, |strings| strings.max_length),
        );
        return;
    }
    // what `StringStatsField` says of no bounds, no lengths and that there can be unicode
    let Some(strings) = strings else {
        s.write_property(206, "packed_field", &0u32);
        return;
    };
    // `StringStatsField`
    let mut packed = 0u32;
    if !strings.has_unicode {
        packed |= 1;
    }
    s.write_property(204, "max_string_length", &strings.max_length);
    match &strings.bounds {
        Some((min, max)) => {
            let mut blob = [0u8; 2 * CURRENT_SIZE];
            let (min_size, max_size) = (min.len().min(CURRENT_SIZE), max.len().min(CURRENT_SIZE));
            blob[..min_size].copy_from_slice(&min[..min_size]);
            blob[CURRENT_SIZE..CURRENT_SIZE + max_size].copy_from_slice(&max[..max_size]);
            s.write_field(205);
            s.write_varint_unsigned(blob.len() as u64);
            s.write_raw(&blob);
            // exact where nothing was cut off
            if min.len() <= CURRENT_SIZE {
                packed |= 1 << 1;
            }
            if max.len() <= CURRENT_SIZE {
                packed |= 1 << 2;
            }
            packed |= (min_size as u32) << 3;
            packed |= (max_size as u32) << 7;
        }
        // empty: the exact-or-empty flags without bounds
        None => packed |= (1 << 1) | (1 << 2),
    }
    let min_length = if strings.min_length >= MAXIMUM_MIN_STRING_LENGTH {
        MAXIMUM_MIN_STRING_LENGTH
    } else {
        strings.min_length + 1
    };
    packed |= min_length << 11;
    s.write_property(206, "packed_field", &packed);
    if let Some(total) = strings.total_length {
        s.write_property(207, "total_string_length", &total);
    }
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
        // `VariantStats::Serialize`: not shredded, and the statistics of what it is stored as
        if ty.id() == LogicalTypeId::Variant {
            const NOT_SHREDDED: u8 = 1;
            const SHREDDED: u8 = 2;
            const INCONSISTENT: u8 = 3;
            let state = if stats.variant_inconsistent {
                INCONSISTENT
            } else if stats.variant_shredded.is_some() {
                SHREDDED
            } else {
                NOT_SHREDDED
            };
            s.write_property(200, "shredding_state", &state);
            let unshredded = duckrs_common::variant::unshredded_type();
            s.write_object(225, "unshredded_stats", |s| {
                write_stats(s, Some(&unshredded), &Stats::unknown())
            });
            if let Some(shredded) = &stats.variant_shredded {
                let (shredded_type, shredded_stats) = &**shredded;
                s.write_property(230, "shredded_type", shredded_type);
                s.write_object(235, "shredded_stats", |s| {
                    write_stats(s, Some(shredded_type), shredded_stats)
                });
            }
            return;
        }
        // `GeometryStats::Serialize` before v1.5.0: unknown string statistics
        if ty.id() == LogicalTypeId::Geometry && !s.should_serialize(StorageVersion::V1_5_0) {
            return write_string_stats(s, None);
        }
        // `GeometryStats::Serialize` of `CreateUnknown`: any extent, type and flag, which prunes nothing
        if ty.id() == LogicalTypeId::Geometry {
            let geometry = stats
                .geometry
                .unwrap_or_else(duckrs_common::geometry::GeometryStats::unknown);
            let e = geometry.extent;
            s.write_object(300, "extent", |s| {
                let bounds = [e.x_min, e.x_max, e.y_min, e.y_max, e.z_min, e.z_max, e.m_min, e.m_max];
                for (field, value) in (101..=108).zip(bounds) {
                    s.write_property(field, "extent", &value);
                }
            });
            s.write_object(301, "types", |s| {
                for (field, set) in (101..=104).zip(geometry.types) {
                    s.write_property(field, "types", &set);
                }
            });
            s.write_property_with_default(302, "flags", &geometry.flags);
            return;
        }
        let unknown = Stats::unknown();
        let child_stats = |at: usize| stats.children.get(at).unwrap_or(&unknown);
        let child = |s: &mut BinarySerializer, at: usize, ty: Option<&LogicalType>| {
            s.write_object_value(|s| write_stats(s, ty, child_stats(at)));
        };
        match ty.physical_type() {
            PhysicalType::Varchar => write_string_stats(s, stats.strings.as_ref()),
            // INTERVAL stats are only serialized in v2.0.0 and later
            PhysicalType::Interval if !s.should_serialize(StorageVersion::V2_0_0) => {}
            PhysicalType::Struct => {
                let children = ty.struct_children();
                s.write_list(200, "child_stats", children.len(), |s, i| {
                    child(s, i, Some(&children[i].1))
                });
            }
            PhysicalType::List | PhysicalType::Array => {
                s.write_object(200, "child_stats", |s| write_stats(s, ty.child_type(), child_stats(0)));
                // `ListStats::Serialize`: from v2.1.0, the lengths that are known
                if let (PhysicalType::List, Some(lists)) = (ty.physical_type(), &stats.lists)
                    && s.should_serialize(StorageVersion::V2_1_0)
                {
                    write_list_lengths(s, lists, 201);
                }
            }
            PhysicalType::Bit | PhysicalType::Invalid | PhysicalType::Unknown => {}
            physical => {
                // `NumericStats::CreateEmpty`: without values the minimum is above the maximum, which a constant
                // segment of only NULLs is scanned with
                let empty = (!stats.has_no_null).then(|| empty_bounds(physical)).flatten();
                let (min, max) = stats.bounds.as_ref().or(empty.as_ref()).map(|(a, b)| (a, b)).unzip();
                write_bound(s, 200, "min", physical, min);
                write_bound(s, 201, "max", physical, max);
            }
        }
    });
}

/// `DataPointer`.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
struct ColumnOut {
    segments: Vec<SegmentOut>,
    validity: Option<Box<ColumnOut>>,
    children: Vec<ColumnOut>,
    stats: Stats,
    /// of a shredded VARIANT: the type of its shredded part, its second child
    shredded_type: Option<LogicalType>,
}

fn write_segment(s: &mut BinarySerializer, segment: &SegmentOut, ty: Option<&LogicalType>, row_start: u64) {
    s.write_object_value(|s| {
        // `DataPointer::row_start`, which v1.5.0 deleted
        if !s.should_serialize(StorageVersion::V1_5_0) {
            s.write_property(100, "row_start", &row_start);
        }
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

/// `min_list_length`, `max_list_length` and `total_list_length` from field `first` on, those that are known
/// (`WritePropertyWithDefault` of an `optional_idx`).
fn write_list_lengths(s: &mut BinarySerializer, lists: &crate::catalog::ListLengths, first: u16) {
    let fields = [
        ("min_list_length", lists.min_length()),
        ("max_list_length", lists.max),
        ("total_list_length", lists.total),
    ];
    for (at, (tag, value)) in fields.into_iter().enumerate() {
        if let Some(value) = value {
            s.write_property(first + at as u16, tag, &value);
        }
    }
}

/// `PersistentColumnData::Serialize`, without the object around it.
fn write_column(s: &mut BinarySerializer, column: &ColumnOut, ty: Option<&LogicalType>, row_start: u64) {
    // before v1.5.0 a GEOMETRY is the BLOB of the spatial extension's format, with nothing to say so
    // (`GeoColumnData::Serialize` of `GeometryStorageType::SPATIAL`)
    let blob = LogicalType::BLOB;
    let ty = match ty {
        Some(ty) if ty.id() == LogicalTypeId::Geometry && !s.should_serialize(StorageVersion::V1_5_0) => Some(&blob),
        ty => ty,
    };
    // `GeometryPersistentColumnData`: stored as WKB, the default storage type
    // `VariantPersistentColumnData`: the type of a shredded VARIANT's shredded part
    if let Some(shredded_type) = &column.shredded_type {
        const VARIANT_EXTRA_DATA: u8 = 1;
        s.write_nullable_object(99, "extra_data", |s| {
            s.write_property(100, "type", &VARIANT_EXTRA_DATA);
            s.write_property(101, "storage_type", shredded_type);
        });
    }
    // `ListPersistentColumnData`, from v2.1.0: the lengths of the lists of the row group
    if let (Some(ty), Some(lists)) = (ty, &column.stats.lists)
        && ty.physical_type() == PhysicalType::List
        && s.should_serialize(StorageVersion::V2_1_0)
    {
        const LIST_EXTRA_DATA: u8 = 3;
        s.write_nullable_object(99, "extra_data", |s| {
            s.write_property(100, "type", &LIST_EXTRA_DATA);
            write_list_lengths(s, lists, 101);
        });
    }
    if ty.is_some_and(|ty| ty.id() == LogicalTypeId::Geometry) {
        const GEOMETRY_EXTRA_DATA: u8 = 2;
        s.write_nullable_object(99, "extra_data", |s| {
            s.write_property(100, "type", &GEOMETRY_EXTRA_DATA)
        });
    }
    if !column.segments.is_empty() {
        let mut starts = Vec::with_capacity(column.segments.len());
        let mut start = row_start;
        for segment in &column.segments {
            starts.push(start);
            start += segment.tuple_count as u64;
        }
        s.write_list(100, "data_pointers", column.segments.len(), |s, i| {
            write_segment(s, &column.segments[i], ty, starts[i]);
        });
    }
    let Some(ty) = ty else { return };
    if let Some(validity) = &column.validity {
        s.write_object(101, "validity", |s| write_column(s, validity, None, row_start));
    }
    // `PersistentColumnData::Serialize` of a VARIANT: what it is stored as, unshredded, and shredded
    if ty.id() == LogicalTypeId::Variant {
        let unshredded = duckrs_common::variant::unshredded_type();
        s.write_object(102, "unshredded", |s| {
            write_column(s, &column.children[0], Some(&unshredded), row_start)
        });
        if let (Some(shredded), Some(shredded_type)) = (column.children.get(1), &column.shredded_type) {
            s.write_object(103, "shredded", |s| {
                write_column(s, shredded, Some(shredded_type), row_start)
            });
        }
        return;
    }
    match ty.physical_type() {
        PhysicalType::List | PhysicalType::Array => {
            s.write_object(102, "child_column", |s| {
                write_column(s, &column.children[0], ty.child_type(), 0)
            });
        }
        PhysicalType::Struct => {
            let fields = ty.struct_children();
            s.write_list(102, "sub_columns", column.children.len(), |s, i| {
                s.write_object_value(|s| write_column(s, &column.children[i], Some(&fields[i].1), row_start));
            });
        }
        _ => {}
    }
}

/// `SchemaCatalogEntry::GetQualifiedName`: [catalog, schema path..., name].
fn entry_path(catalog: &str, schema: &SchemaCatalogEntry, name: &str) -> Vec<String> {
    let mut path = vec![catalog.to_string()];
    path.extend(schema.path.iter().cloned());
    path.push(name.to_string());
    path
}

/// `SchemaCatalogEntry::GetInfo`: a top-level schema is its name, a nested one its full path, each
/// with an empty name for the entry.
pub(super) fn schema_info(catalog: &str, path: &[String]) -> g::CreateInfo {
    let mut full = if path.len() > 1 {
        vec![catalog.to_string()]
    } else {
        Vec::new()
    };
    full.extend(path.iter().cloned());
    full.push(String::new());
    to_stored::create_info(
        catalog_type::SCHEMA_ENTRY,
        full,
        g::CreateInfoKind::CreateSchemaInfo(g::CreateSchemaInfo { options: Vec::new() }),
    )
}

pub(super) fn type_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    name: String,
    logical_type: LogicalType,
) -> g::CreateInfo {
    let comment = schema.type_comment(&name);
    let path = entry_path(catalog, schema, &name);
    let kind = g::CreateInfoKind::CreateTypeInfo(g::CreateTypeInfo { name, logical_type });
    let mut info = to_stored::create_info(catalog_type::TYPE_ENTRY, path, kind);
    info.comment = comment.map(Value::varchar);
    info
}

pub(super) fn sequence_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    sequence: &SequenceCatalogEntry,
) -> g::CreateInfo {
    let data = sequence.data();
    let kind = g::CreateInfoKind::CreateSequenceInfo(g::CreateSequenceInfo {
        name: sequence.name.clone(),
        usage_count: data.usage_count,
        increment: data.increment,
        min_value: data.min_value,
        max_value: data.max_value,
        start_value: data.counter,
        cycle: data.cycle,
        last_value: data.last_value,
    });
    let path = entry_path(catalog, schema, &sequence.name);
    let mut info = to_stored::create_info(catalog_type::SEQUENCE_ENTRY, path, kind);
    info.comment = sequence.comment.clone().map(Value::varchar);
    info
}

pub(super) fn macro_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    entry: &MacroCatalogEntry,
    types: &Catalog,
) -> Result<g::CreateInfo> {
    let binder = Binder::new(types);
    let bind = |ty: &ast::CastType| binder.bind_type(ty);
    let mut macros = entry
        .macros
        .iter()
        .map(|function| to_stored::macro_function(function, &bind));
    let function = macros.next().ok_or_else(|| internal("a macro has no function"))??;
    let extra_functions = macros
        .map(|function| function.map(|f| Some(Box::new(f))))
        .collect::<Result<_>>()?;
    let entry_type = if entry.table_macro {
        catalog_type::TABLE_MACRO_ENTRY
    } else {
        catalog_type::MACRO_ENTRY
    };
    let kind = g::CreateInfoKind::CreateMacroInfo(g::CreateMacroInfo {
        name: entry.name.clone(),
        function: Some(Box::new(function)),
        extra_functions,
    });
    let mut info = to_stored::create_info(entry_type, entry_path(catalog, schema, &entry.name), kind);
    info.comment = entry.comment.clone().map(Value::varchar);
    Ok(info)
}

/// `TriggerCatalogEntry::GetInfo`: the table as the CREATE TRIGGER named it.
pub(super) fn trigger_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    trigger: &crate::catalog::TriggerCatalogEntry,
) -> Result<g::CreateInfo> {
    let base_table: ast::TableRef = ast::TableRefKind::BaseTable {
        name: ast::QualifiedName::from_parts(trigger.table_written.clone()),
        at_clause: None,
    }
    .into();
    let kind = g::CreateInfoKind::CreateTriggerInfo(g::CreateTriggerInfo {
        trigger_name: trigger.name.clone(),
        base_table: Some(Box::new(to_stored::table_ref(&base_table)?)),
        timing: trigger.timing as u64,
        event_type: trigger.event as u64,
        columns: trigger.columns.clone(),
        for_each: trigger.for_each as u64,
        trigger_action: Some(Box::new(to_stored::query_node(&trigger.action)?)),
        referencing_new_table: trigger.referencing_new.clone(),
        referencing_old_table: trigger.referencing_old.clone(),
    });
    let mut info = to_stored::create_info(
        catalog_type::TRIGGER_ENTRY,
        entry_path(catalog, schema, &trigger.name),
        kind,
    );
    info.temporary = trigger.temporary;
    info.comment = trigger.comment.clone().map(Value::varchar);
    Ok(info)
}

pub(super) fn view_info(catalog: &str, schema: &SchemaCatalogEntry, view: &ViewCatalogEntry) -> Result<g::CreateInfo> {
    let kind = g::CreateInfoKind::CreateViewInfo(g::CreateViewInfo {
        view_name: view.name.clone(),
        aliases: view.aliases.clone(),
        types: Vec::new(),
        query: Some(Box::new(to_stored::select_statement(&view.query)?)),
        names: Vec::new(),
        column_comments: Vec::new(),
        column_comments_map: view
            .column_comments
            .iter()
            .map(|(name, comment)| (name.clone(), Value::varchar(comment.clone())))
            .collect(),
        security_type: view.secure.then_some(g::enums::view_security_type::SECURE_VIEW),
    });
    let mut info = to_stored::create_info(catalog_type::VIEW_ENTRY, entry_path(catalog, schema, &view.name), kind);
    info.comment = view.comment.clone().map(Value::varchar);
    Ok(info)
}

/// `ColumnDefinition` of a column of a table, as CREATE TABLE and ALTER TABLE ... ADD COLUMN say it.
pub(super) fn stored_column(column: &crate::catalog::LogicalColumn<'_>) -> Result<g::ColumnDefinition> {
    let expression = match (
        column.generated,
        column.definition.and_then(|d| d.default_text.as_deref()),
    ) {
        (Some(expr), _) => Some(expr.clone()),
        (None, Some(text)) => Some(parse_expression(text)?),
        (None, None) => None,
    };
    Ok(g::ColumnDefinition {
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
        compression_type: column
            .definition
            .and_then(|d| d.compression)
            .map_or(COMPRESSION_AUTO, u64::from),
        comment: column.definition.and_then(|d| d.comment.clone()).map(Value::varchar),
        tags: Vec::new(),
    })
}

pub(super) fn table_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    table: &TableCatalogEntry,
) -> Result<g::CreateInfo> {
    let mut columns = Vec::with_capacity(table.columns.len());
    let mut constraints = Vec::new();
    let boxed = |constraint: g::Constraint| Some(Box::new(constraint));
    for column in table.logical_columns().iter() {
        columns.push(stored_column(column)?);
    }
    // a column by its place among all the columns, generated ones too (`LogicalIndex`)
    let logical = table.logical_columns();
    let logical_index = |column: usize| -> u64 {
        let name = &table.columns[column].name;
        logical
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .unwrap_or(column) as u64
    };
    // in the order the table has them, as C++ keeps its `constraints`
    for constraint in &table.constraints {
        match constraint {
            TableConstraint::NotNull(column) => constraints.push(boxed(g::Constraint {
                type_: constraint_type::NOT_NULL,
                kind: g::ConstraintKind::NotNullConstraint(g::NotNullConstraint {
                    index: logical_index(*column),
                }),
            })),
            TableConstraint::Check { parsed, .. } => constraints.push(boxed(g::Constraint {
                type_: constraint_type::CHECK,
                kind: g::ConstraintKind::CheckConstraint(g::CheckConstraint {
                    expression: Some(Box::new(to_stored::expression(parsed)?)),
                }),
            })),
            TableConstraint::Unique {
                columns,
                primary_key,
                column_level,
                check_mode,
            } => constraints.push(boxed(g::Constraint {
                type_: constraint_type::UNIQUE,
                kind: g::ConstraintKind::UniqueConstraint(g::UniqueConstraint {
                    is_primary_key: *primary_key,
                    // a column constraint is by index, a table constraint by name
                    // (`DConstants::INVALID_INDEX`)
                    index: match (column_level, columns.as_slice()) {
                        (true, [column]) => logical_index(*column),
                        _ => u64::MAX,
                    },
                    columns: columns.iter().map(|c| table.columns[*c].name.clone()).collect(),
                    check_mode: match check_mode {
                        duckrs_parser::ast::ConstraintCheckMode::Default => None,
                        duckrs_parser::ast::ConstraintCheckMode::Immediate => Some(1),
                        duckrs_parser::ast::ConstraintCheckMode::Deferred => Some(2),
                    },
                }),
            })),
            TableConstraint::ForeignKey(key) => {
                let parent = key.referenced.as_ref().map(|referenced| referenced.table());
                let names = |table: &TableCatalogEntry, columns: &[usize]| -> Vec<String> {
                    columns.iter().map(|c| table.columns[*c].name.clone()).collect()
                };
                constraints.push(boxed(g::Constraint {
                    type_: constraint_type::FOREIGN_KEY,
                    kind: g::ConstraintKind::ForeignKeyConstraint(g::ForeignKeyConstraint {
                        pk_columns: names(parent.as_deref().unwrap_or(table), &key.referenced_columns),
                        fk_columns: names(table, &key.columns),
                        // the side that is referenced gets its constraint when C++ reads this one
                        fk_type: if parent.is_some() {
                            foreign_key_type::FK_TYPE_FOREIGN_KEY_TABLE
                        } else {
                            foreign_key_type::FK_TYPE_SELF_REFERENCE_TABLE
                        },
                        schema: schema.name.clone(),
                        table: parent.as_deref().unwrap_or(table).name.clone(),
                        pk_keys: key.referenced_columns.iter().map(|c| *c as u64).collect(),
                        fk_keys: key.columns.iter().map(|c| *c as u64).collect(),
                    }),
                }));
            }
        }
    }
    let kind = g::CreateInfoKind::CreateTableInfo(g::CreateTableInfo {
        table: table.name.clone(),
        columns: g::ColumnList { columns },
        constraints,
        query: None,
        partition_keys: Vec::new(),
        sort_keys: Vec::new(),
        options: Vec::new(),
    });
    let mut info = to_stored::create_info(
        catalog_type::TABLE_ENTRY,
        entry_path(catalog, schema, &table.name),
        kind,
    );
    info.comment = table.comment.lock().clone().map(Value::varchar);
    Ok(info)
}

/// `IndexCatalogEntry::GetInfo`. An index that backs a constraint isn't an entry in C++ either.
pub(super) fn index_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    table: &TableCatalogEntry,
    index: &IndexCatalogEntry,
) -> Result<g::CreateInfo> {
    let parsed = index
        .parsed
        .iter()
        .map(|expression| to_stored::expression(expression).map(|e| Some(Box::new(e))))
        .collect::<Result<Vec<_>>>()?;
    let mut scan_types: Vec<LogicalType> = index.columns.iter().map(|c| table.columns[*c].ty.clone()).collect();
    scan_types.push(LogicalType::BIGINT); // the row id
    let kind = g::CreateInfoKind::CreateIndexInfo(g::CreateIndexInfo {
        name: index.name.clone(),
        table: table.name.clone(),
        constraint_type: if index.unique {
            index_constraint_type::UNIQUE
        } else {
            index_constraint_type::NONE
        },
        parsed_expressions: parsed,
        scan_types,
        names: table.columns.iter().map(|c| c.name.clone()).collect(),
        column_ids: index.columns.iter().map(|c| *c as u64).collect(),
        options: Vec::new(),
        index_type_name: index.index_type.clone(),
    });
    let mut info = to_stored::create_info(
        catalog_type::INDEX_ENTRY,
        entry_path(catalog, schema, &index.name),
        kind,
    );
    info.sql.clone_from(&index.sql);
    info.comment = index.comment.clone().map(Value::varchar);
    Ok(info)
}

/// `ReorderTableEntries`: of the positions in `tables`, each table comes after those it references.
pub(super) fn parents_first<'a>(tables: impl IntoIterator<Item = &'a Arc<TableCatalogEntry>>) -> Vec<usize> {
    fn visit(table: usize, tables: &[&Arc<TableCatalogEntry>], seen: &mut [bool], order: &mut Vec<usize>) {
        if std::mem::replace(&mut seen[table], true) {
            return;
        }
        for constraint in &tables[table].constraints {
            if let TableConstraint::ForeignKey(key) = constraint
                && let Some(referenced) = &key.referenced
                && let Some(parent) = tables.iter().position(|other| {
                    other.schema.eq_ignore_ascii_case(&tables[table].schema)
                        && other.name.eq_ignore_ascii_case(&referenced.name)
                })
            {
                visit(parent, tables, seen, order);
            }
        }
        order.push(table);
    }
    let tables: Vec<_> = tables.into_iter().collect();
    let mut seen = vec![false; tables.len()];
    let mut order = Vec::with_capacity(tables.len());
    for table in 0..tables.len() {
        visit(table, &tables, &mut seen, &mut order);
    }
    order
}

/// A piece of a column: a flat vector and how many rows it has.
#[derive(Debug)]
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

/// `MetadataBlock`: which sub-blocks are free, and the bytes once the checkpoint writes some of it.
struct MetaBlock {
    free: u64,
    data: Option<Vec<u8>>,
    dirty: bool,
    /// whether this checkpoint can take its free sub-blocks
    available: bool,
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
    /// `MetadataManager::blocks`: the metadata blocks, of the file and new ones
    meta: std::collections::BTreeMap<u64, MetaBlock>,
    /// `MetadataManager::modified_blocks`: of each metadata block of the file, the sub-blocks it had taken, which
    /// are free once this checkpoint is written unless it keeps them (`ClearModifiedBlocks`)
    modified: std::collections::BTreeMap<u64, u64>,
    /// Of the table being written: whether its rows are those of its last checkpoint as they were, so that its
    /// row groups are kept without reading them (`RowGroup::HasChanges`), and which columns of each of those
    /// were loaded (`RowGroup::is_loaded`).
    in_file: Option<super::layered::LayeredTable>,
    stored_loaded: Vec<Vec<bool>>,
    /// Which columns of each row group written of the table are loaded: all of one that was written, and of
    /// one that was kept, those that were.
    group_loaded: Vec<Vec<bool>>,
    /// Of the table being written, the `DistinctStatistics` counted from the values written of the columns it has
    /// none of (one an ALTER added or changed the type of), while every row group of them is written from values
    counted_distinct: Vec<Option<DistinctState>>,
    /// That of each table, by schema and name, for the table read back from the file.
    pub(super) loaded: HashMap<(String, String), Vec<Vec<bool>>>,
    /// Of each table whose statistics the file has less of than they are (before v2.0.0, strings to 8 bytes), the
    /// statistics of each column as v2.0.0 writes them: until the database opens again, they are what a table has.
    pub(super) memory_statistics: HashMap<(String, String), Vec<Vec<u8>>>,
    /// How many segments each block holds, which DuckDB has to know to free them one by one.
    segments_in_block: HashMap<u64, u32>,
    /// The block that segments go into now, with how much of it they fill.
    /// `PartialBlockManager::partially_filled_blocks`: blocks segments still go into, by the room they have left
    /// (and then in the order they came, as a multimap has them), with their bytes and how much they fill.
    partial: std::collections::BTreeMap<(usize, u64), PartialBlock>,
    partial_count: u64,
    /// `PartialBlockType::APPEND_TO_TABLE`: rows written ahead of a commit, whose blocks get their ids as they are
    /// flushed (`PartialBlock::Flush`); until then a partial block has an id of its own, which this maps.
    append: bool,
    provisional: HashMap<u64, u64>,
    /// Of the table being written, the row groups an append wrote to blocks (`OptimisticDataWriter`).
    written: Vec<std::sync::Arc<WrittenGroup>>,
    /// Sub-blocks that were put aside for the free list (`GetFreeListBlocks`).
    reserved: VecDeque<(u64, usize)>,
    tables: MetaStream,
    /// The setting, in lower case.
    force_compression: String,
    /// `disabled_compression_methods`, in lower case
    disabled_compression: Vec<String>,
    variant_minimum_shredding_size: i64,
    force_variant_shredding: Option<LogicalType>,
    bitpacking_mode: super::bitpacking::Mode,
    zstd_min_string_length: usize,
    /// the table and column being written, for the log of `DetectBestCompressionMethod`
    log_column: (String, usize),
    chunks_per_row_group: usize,
    vacuum_tasks: usize,
    vacuum_rebuild_indexes: u64,
    metadata_reuse: bool,
    /// `RowGroupCollection::SupportsPerColumnWrites`: a row group says which metadata blocks each of its columns
    /// takes, and a checkpoint writes again only the columns of it that changed
    per_column_writes: bool,
    /// While a column's metadata is written: the metadata sub-blocks that it goes on to (`StartWritingColumns`).
    written_pages: Option<Vec<u64>>,
    abort: String,
    /// what the file is written in
    version: StorageVersion,
    /// While a column is encoded on a thread of its own: what it would place and log, in order, for the writer of
    /// the file to place and log after the columns before it (see [`Deferred`]).
    deferred: Option<Deferred>,
}

/// What a column's encoding would do to the file, held back: the segments it places, in order, and the lines it
/// logs. One that needs a block of its own (overflow strings, segments over several pages) is `failed`, and is
/// encoded again on the writer of the file.
#[derive(Debug, Default)]
struct Deferred {
    placed: Vec<Vec<u8>>,
    logs: parking_lot::Mutex<Vec<String>>,
    failed: bool,
}

/// Where a deferred segment says it is until it is placed: `DEFERRED_BLOCK` plus its index in `Deferred::placed`.
const DEFERRED_BLOCK: i64 = 1 << 61;

/// A row group that a table is written as. Its rows have the ids from `row_start` on, and those
/// with no row are deleted, as there are no gaps in a row group.
#[derive(Debug, PartialEq, Eq)]
struct PlannedGroup {
    row_start: u64,
    /// the id of its first row in the file: `row_start` unless the ids are rewritten densely
    file_start: u64,
    rows: usize,
    first_position: usize,
    live: usize,
    /// It takes rows of groups before it, so that its rows have new ids, without gaps.
    merged: bool,
}

/// `MAX_MERGE_COUNT`.
const MAX_MERGE_COUNT: usize = 3;

/// `ScheduleVacuumTasks`: `groups` as they are written, some of them together where that makes for
/// fewer row groups.
/// `keep_ids`: the ids can't change (`can_change_row_ids` is false), so a merge takes only row groups without
/// deletes whose ids follow each other.
fn vacuum_groups(groups: Vec<PlannedGroup>, size: usize, tasks: usize, keep_ids: bool) -> Vec<PlannedGroup> {
    let mut result = Vec::with_capacity(groups.len());
    let mut at = 0;
    let mut merges = 0;
    while at < groups.len() {
        let mut planned = None;
        for target in (1..=MAX_MERGE_COUNT).filter(|_| merges < tasks) {
            let mut next = at;
            let mut rows = 0;
            while next < groups.len() && rows + groups[next].live <= target * size {
                let group = &groups[next];
                let follows =
                    next == at || group.row_start == groups[next - 1].row_start + groups[next - 1].rows as u64;
                if keep_ids && (group.live != group.rows || !follows) {
                    break;
                }
                rows += group.live;
                next += 1;
            }
            // what is appended would otherwise rewrite the last row group each time
            if next == groups.len() {
                let least = (groups[at].live * 2).min(size) * target;
                if rows >= STANDARD_VECTOR_SIZE && rows < least {
                    continue;
                }
            }
            if target < next - at {
                planned = Some((target, next, rows));
                break;
            }
        }
        let Some((target, next, rows)) = planned else {
            result.push(PlannedGroup {
                row_start: groups[at].row_start,
                file_start: groups[at].file_start,
                rows: groups[at].rows,
                first_position: groups[at].first_position,
                live: groups[at].live,
                merged: groups[at].merged,
            });
            at += 1;
            continue;
        };
        merges += 1;
        let (mut row_start, mut first_position) = (groups[at].row_start, groups[at].first_position);
        let mut left = rows;
        for _ in 0..target {
            let count = left.min(size);
            if count > 0 {
                result.push(PlannedGroup {
                    row_start,
                    file_start: row_start,
                    rows: count,
                    first_position,
                    live: count,
                    merged: true,
                });
            }
            row_start += count as u64;
            first_position += count;
            left -= count;
        }
        at = next;
    }
    result
}

/// The row groups of rows with these ids: those that a file has, without the ones whose rows all
/// went, and new ones of `size` for what came after them.
/// `dense`: the file can't say that row ids have gaps (`CanPersistRowIdGaps`, before v2.0.0), so the ids are
/// rewritten to follow each other. `keep_ids`: indexes don't let the ids change (`KEEP_ROW_IDS`), so none come
/// together, and in a `dense` file row groups without rows stay where later rows follow them.
fn plan_groups(ids: &RowIds, size: usize, vacuum_tasks: usize, dense: bool, keep_ids: bool) -> Vec<PlannedGroup> {
    let mut groups: Vec<PlannedGroup> = Vec::new();
    let mut add = |row_start: u64, rows: usize| {
        let first_position = ids.first_from(row_start);
        let live = ids.first_from(row_start + rows as u64) - first_position;
        if live > 0 || (keep_ids && dense) {
            groups.push(PlannedGroup {
                row_start,
                file_start: row_start,
                rows,
                first_position,
                live,
                merged: false,
            });
        }
    };
    let mut end = 0;
    for &(row_start, rows) in ids.stored_groups() {
        // rows in front of row groups a commit wrote ahead are new ones of their own
        while end < row_start {
            let rows = (row_start - end).min(size as u64);
            add(end, rows as usize);
            end += rows;
        }
        add(row_start, rows);
        end = row_start + rows as u64;
    }
    // ids that nothing has, in front of new rows, are of rows that went before a checkpoint
    while end < ids.next() {
        let rows = (ids.next() - end).min(size as u64);
        add(end, rows as usize);
        end += rows;
    }
    if keep_ids {
        // a suffix of row groups without rows shifts no row that stays
        while groups.last().is_some_and(|group| group.live == 0) {
            groups.pop();
        }
        return vacuum_groups(groups, size, vacuum_tasks, true);
    }
    let mut groups = vacuum_groups(groups, size, vacuum_tasks, false);
    if dense {
        // `RowGroupCollection::Checkpoint`: without `next_row_id`, the row groups are numbered without gaps
        let mut next = 0u64;
        for group in &mut groups {
            if group.merged {
                group.row_start = next;
            }
            group.file_start = next;
            next += group.rows as u64;
        }
    }
    groups
}

/// Of a row group that has now the rows `live` (those of its offsets that `kept` says), against `group` of `disk`:
/// whether the same rows are deleted, and of each column, the column of `disk` it has the values of, if it is
/// `stored` of it and has them all (`ColumnData::HasAnyChanges`). The values of deleted rows don't count.
fn matching_columns(
    disk: &super::table_data::DiskTable,
    group: usize,
    kept: &[bool],
    live: &[DataChunk],
    stored: &[Option<usize>],
    types: &[LogicalType],
) -> Result<(bool, Vec<Option<usize>>)> {
    let same_deletes = same_deletes(disk, group, kept);
    let mut same = Vec::with_capacity(types.len());
    for (column, ty) in types.iter().enumerate() {
        let candidate = stored
            .get(column)
            .copied()
            .flatten()
            .filter(|&at| disk.column_type(at) == Some(ty));
        let Some(at) = candidate else {
            same.push(None);
            continue;
        };
        // as many as `kept`: the group is that of `disk` with as many rows
        let values = disk.read_column_unfiltered(group, at)?;
        let mut rows = live
            .iter()
            .flat_map(|chunk| (0..chunk.len()).map(move |row| (chunk, row)));
        let equal = kept.iter().enumerate().filter(|(_, kept)| **kept).all(|(offset, _)| {
            rows.next()
                .is_some_and(|(chunk, row)| identical(&chunk.column(column).get_value(row), &values.get_value(offset)))
        });
        same.push(equal.then_some(at));
    }
    Ok((same_deletes, same))
}

/// Whether the rows of `left` are the first of `ids`, with the same ids.
fn ids_are_prefix(left: &RowIds, ids: &RowIds) -> bool {
    if left.len() > ids.len() {
        return false;
    }
    let mut position = 0;
    for (first, count) in left.ranges() {
        if ids.id(position) != first || ids.id(position + count - 1) != first + count as u64 - 1 {
            return false;
        }
        position += count;
    }
    true
}

/// Whether the rows of `group` of `disk` that are left are those `kept` says.
fn same_deletes(disk: &super::table_data::DiskTable, group: usize, kept: &[bool]) -> bool {
    match disk.alive_rows(group) {
        None => kept.iter().all(|kept| *kept),
        Some(alive) => {
            let mut alive = alive.iter().peekable();
            kept.iter().enumerate().all(|(row, kept)| {
                let stored = alive.peek().is_some_and(|at| **at as usize == row);
                if stored {
                    alive.next();
                }
                stored == *kept
            })
        }
    }
}

/// What a row group's deletes are: as the file has them, or to be written (if there are any).
enum Deletes {
    Keep(Vec<MetaBlockPointer>),
    Write(Option<Vec<u8>>),
}

/// The values of the rows of a row group that are left, `live`, with NULLs where `kept` says a row was deleted.
fn with_deleted(live: Vector, kept: &[bool], ty: &LogicalType) -> Result<Vector> {
    if kept.iter().all(|kept| *kept) {
        return Ok(live);
    }
    let mut result = Vector::new(ty.clone(), kept.len());
    let mut at = 0;
    for (row, kept) in kept.iter().enumerate() {
        if *kept {
            result.set_value(row, &live.get_value(at))?;
            at += 1;
        } else {
            result.set_value(row, &Value::null(ty.clone()))?;
        }
    }
    Ok(result)
}

/// Whether two values are the same, floating point ones bit for bit (-0.0 is not 0.0).
fn identical(a: &Value, b: &Value) -> bool {
    use duckrs_common::value::ValueData as V;
    if a.logical_type() != b.logical_type() {
        return false;
    }
    match (a.data(), b.data()) {
        (V::Float(x), V::Float(y)) => x.to_bits() == y.to_bits(),
        (V::Double(x), V::Double(y)) => x.to_bits() == y.to_bits(),
        (V::List(xs), V::List(ys)) | (V::Struct(xs), V::Struct(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| identical(x, y))
        }
        _ => a == b,
    }
}

/// `StringStatsData` as the file has it: a bound that was cut off is made one that sorts past it (above for
/// the maximum, as a byte below the rest does for the minimum), so that it is not taken to be exact.
fn stored_strings(strings: &super::statistics::StringBounds) -> StringStatsOut {
    let bound = |bytes: &[u8], exact: bool, pad: u8| {
        let mut bound = bytes.to_vec();
        if !exact {
            bound.push(pad);
        }
        bound
    };
    StringStatsOut {
        bounds: strings.has_bounds.then(|| {
            (
                bound(&strings.min, strings.min_exact, 0x00),
                bound(&strings.max, strings.max_exact, 0xFF),
            )
        }),
        has_unicode: strings.has_unicode,
        max_length: strings
            .max_length
            .map_or(u32::MAX, |length| length.min(u64::from(u32::MAX)) as u32),
        min_length: strings
            .min_length
            .map_or(0, |length| length.min(u64::from(u32::MAX)) as u32),
        total_length: strings.total_length,
    }
}

/// [`Stats`] of what the file says of a segment.
fn segment_stats(stored: &super::statistics::BaseStatistics) -> Stats {
    let mut stats = Stats::unknown();
    stats.has_null = stored.has_null;
    stats.has_no_null = stored.has_no_null;
    stats.bounds = stored.min.zip(stored.max);
    stats.geometry = stored.geometry;
    stats.strings = stored.strings.as_ref().map(stored_strings);
    stats
}

/// `ColumnData::MergeIntoStatistics` of a column a row group keeps: the statistics of its segments, its
/// validity's, its children's and of a VARIANT's shredded part.
fn stored_stats(column: &super::table_data::PersistentColumn, ty: &LogicalType) -> Stats {
    let mut stats = Stats::of_validity(&[]);
    if let Some(validity) = &column.validity {
        for pointer in &validity.pointers {
            stats.has_null |= pointer.statistics.has_null;
            stats.has_no_null |= pointer.statistics.has_no_null;
        }
    }
    let data_type = if column.spatial { LogicalType::BLOB } else { ty.clone() };
    let mut segments = column.pointers.iter().map(|pointer| segment_stats(&pointer.statistics));
    if let Some(mut merged) = segments.next() {
        for segment in segments {
            merged.merge(&segment, &data_type);
        }
        let (has_null, has_no_null) = (stats.has_null, stats.has_no_null);
        stats = merged;
        if column.validity.is_some() {
            stats.has_null = has_null;
            stats.has_no_null = has_no_null;
        }
    }
    if column.spatial {
        stats.strings = None;
    }
    if ty.id() == LogicalTypeId::Variant {
        if let (Some(shredded_type), Some(shredded)) = (&column.shredded_type, column.children.get(1)) {
            stats.variant_shredded = Some(Box::new((shredded_type.clone(), stored_stats(shredded, shredded_type))));
        }
        return stats;
    }
    let types = child_types(ty);
    if column.children.len() == types.len() {
        stats.children = column
            .children
            .iter()
            .zip(&types)
            .map(|(child, ty)| stored_stats(child, ty))
            .collect();
    }
    stats.lists = column.list_lengths;
    stats
}

/// `RowVersionManager::Checkpoint`: which rows of a row group are deleted. `kept` has its rows.
fn deletes_of(kept: &[bool]) -> Vec<u8> {
    let vectors: Vec<(usize, &[bool])> = kept
        .chunks(STANDARD_VECTOR_SIZE)
        .enumerate()
        .filter(|(_, rows)| rows.iter().any(|kept| !kept))
        .collect();
    let mut out = (vectors.len() as u64).to_le_bytes().to_vec();
    for (index, rows) in vectors {
        out.extend_from_slice(&(index as u64).to_le_bytes());
        let start = (index * STANDARD_VECTOR_SIZE) as u64;
        if rows.len() == STANDARD_VECTOR_SIZE && rows.iter().all(|kept| !kept) {
            out.push(CONSTANT_INFO);
            out.extend_from_slice(&start.to_le_bytes());
            continue;
        }
        out.push(VECTOR_INFO);
        out.extend_from_slice(&start.to_le_bytes());
        // `ChunkVectorInfo::Write`: a mask whose valid rows are the deleted ones, rows past the vector's not
        let deleted = |row: usize| rows.get(row).is_some_and(|kept| !kept);
        write_validity(&mut out, deleted);
    }
    out
}

/// `ValidityMask::Write` of a vector's mask: the indexes of its valid or its invalid rows, whichever are fewer, where
/// that is less than the bitmask.
fn write_validity(out: &mut Vec<u8>, valid: impl Fn(usize) -> bool) {
    const COUNT: usize = STANDARD_VECTOR_SIZE;
    let valid_values = (0..COUNT).filter(|row| valid(*row)).count();
    let invalid_values = COUNT - valid_values;
    let bitmask_bytes = COUNT / 8;
    let valid_size = 2 * valid_values + 4;
    let invalid_size = 2 * invalid_values + 4;
    if valid_size < bitmask_bytes || invalid_size < bitmask_bytes {
        let serialize_valid = valid_size < invalid_size;
        out.push(if serialize_valid { VALID_VALUES } else { INVALID_VALUES });
        out.extend_from_slice(&(valid_values.min(invalid_values) as u32).to_le_bytes());
        for row in (0..COUNT).filter(|row| valid(*row) == serialize_valid) {
            out.extend_from_slice(&(row as u16).to_le_bytes());
        }
        return;
    }
    out.push(BITMASK);
    let mut words = [0u64; COUNT / 64];
    for row in (0..COUNT).filter(|row| valid(*row)) {
        words[row / 64] |= 1 << (row % 64);
    }
    words.iter().for_each(|word| out.extend_from_slice(&word.to_le_bytes()));
}

/// The rows of a table in order of position, as they come from its file or from memory.
struct PositionedRows<'a> {
    cursor: Option<TableCursor>,
    memory: Option<parking_lot::RwLockReadGuard<'a, ColumnDataCollection>>,
    at: Option<(usize, DataChunk)>,
}

impl PositionedRows<'_> {
    fn chunk(&mut self, index: usize) -> Result<&DataChunk> {
        if self.at.as_ref().is_none_or(|(at, _)| *at != index) {
            let chunk = match (&mut self.cursor, &self.memory) {
                (Some(cursor), _) => cursor.chunk(index)?,
                (None, Some(memory)) => memory.chunk(index).clone(),
                (None, None) => unreachable!("one of the two is set"),
            };
            self.at = Some((index, chunk));
        }
        Ok(&self.at.as_ref().expect("just set").1)
    }
}

impl<'a> Writer<'a> {
    fn new(buffer: &'a BufferManager, catalog: &'a Catalog, name: String, options: CheckpointOptions) -> Self {
        let blocks = buffer.block_manager();
        Self {
            buffer,
            catalog,
            name,
            block_size: blocks.block_size(),
            free: if options.keep_free_blocks {
                BTreeSet::new()
            } else {
                blocks.free_list().into_iter().collect()
            },
            next_block: blocks.header().block_count,
            used: BTreeSet::new(),
            meta: blocks
                .metadata_blocks()
                .into_iter()
                .map(|(block, free)| {
                    let state = MetaBlock {
                        free,
                        data: None,
                        dirty: false,
                        // while other transactions may still read what it frees, what the file has stays
                        available: !options.keep_free_blocks,
                    };
                    (block, state)
                })
                .collect(),
            modified: blocks
                .metadata_blocks()
                .into_iter()
                .map(|(block, free)| (block, !free))
                .collect(),
            in_file: None,
            stored_loaded: Vec::new(),
            group_loaded: Vec::new(),
            counted_distinct: Vec::new(),
            loaded: HashMap::new(),
            memory_statistics: HashMap::new(),
            segments_in_block: HashMap::new(),
            partial: std::collections::BTreeMap::new(),
            partial_count: 0,
            append: false,
            provisional: HashMap::new(),
            written: Vec::new(),
            reserved: VecDeque::new(),
            tables: MetaStream::default(),
            force_compression: options.force_compression.to_lowercase(),
            disabled_compression: options.disabled_compression.to_vec(),
            variant_minimum_shredding_size: options.variant_minimum_shredding_size,
            force_variant_shredding: options.force_variant_shredding.cloned(),
            bitpacking_mode: super::bitpacking::Mode::parse(options.bitpacking_mode),
            zstd_min_string_length: options.zstd_min_string_length,
            log_column: (String::new(), 0),
            chunks_per_row_group: options.row_group_size as usize / STANDARD_VECTOR_SIZE,
            vacuum_tasks: options.vacuum_tasks,
            vacuum_rebuild_indexes: options.vacuum_rebuild_indexes,
            metadata_reuse: options.metadata_reuse,
            per_column_writes: options.storage_version >= StorageVersion::V2_0_0
                || (options.storage_version >= StorageVersion::V1_4_0 && options.force_column_metadata_reuse),
            written_pages: None,
            abort: options.abort.to_string(),
            version: options.storage_version,
            deferred: None,
        }
    }

    /// `GetFreeBlockIdForCheckpoint`: never one that the last checkpoint needs. Ahead of a commit, `GetFreeBlockId`.
    fn allocate_block(&mut self) -> u64 {
        if let Some(deferred) = &mut self.deferred {
            deferred.failed = true;
            return 0;
        }
        if self.append {
            return self.buffer.block_manager().allocate_free_block();
        }
        let block = self.free.pop_first().unwrap_or_else(|| {
            self.next_block += 1;
            self.next_block - 1
        });
        self.used.insert(block);
        block
    }

    /// A block of a segment's own, which a deferred encoding doesn't write: it has `failed`, and is encoded again.
    fn write_data_block(&self, block: u64, data: &[u8]) -> Result<()> {
        if self.deferred.is_some() {
            return Ok(());
        }
        self.buffer.block_manager().write_block(block, data)
    }

    /// `FlushPartialBlocks`.
    fn flush_open_block(&mut self) -> Result<()> {
        for (_, (block, data, _)) in std::mem::take(&mut self.partial) {
            self.flush_partial(block, &data)?;
        }
        Ok(())
    }

    /// `PartialBlock::Flush`: ahead of a commit, this is when the block gets its id.
    fn flush_partial(&mut self, block: u64, data: &parking_lot::Mutex<Vec<u8>>) -> Result<()> {
        let id = if self.append {
            let id = self.allocate_block();
            self.provisional.insert(block, id);
            id
        } else {
            block
        };
        self.buffer.block_manager().write_block(id, &data.lock())?;
        if self.append {
            self.buffer.flushed_provisional(block, id);
        }
        Ok(())
    }

    /// `PartialBlockManager::Write` of a segment: into the fullest block that has room for it, unless it is too
    /// large to share one, or else a new block; a block keeps taking segments while a fifth of it is free.
    fn place(&mut self, data: &[u8]) -> Result<(i64, u32)> {
        if let Some(deferred) = &mut self.deferred {
            deferred.placed.push(data.to_vec());
            return Ok((DEFERRED_BLOCK + deferred.placed.len() as i64 - 1, 0));
        }
        let max_partial_size = self.block_size / 5 * 4;
        let min_reusable_space = self.block_size - max_partial_size;
        let taken = if data.len() <= max_partial_size {
            let key = self.partial.range((data.len(), 0)..).next().map(|(key, _)| *key);
            key.and_then(|key| self.partial.remove(&key))
        } else {
            None
        };
        let (block, buffer, used) = match taken {
            Some(partial) => partial,
            None if self.append => {
                let id = PROVISIONAL_BLOCK + NEXT_PROVISIONAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let buffer = Arc::new(parking_lot::Mutex::new(vec![0; self.block_size]));
                // what is in it is read from memory until it is written
                self.buffer.open_provisional(id, buffer.clone());
                (id, buffer, 0)
            }
            None => (
                self.allocate_block(),
                Arc::new(parking_lot::Mutex::new(vec![0; self.block_size])),
                0,
            ),
        };
        let at = used;
        buffer.lock()[at..at + data.len()].copy_from_slice(data);
        let used = (at + data.len()).next_multiple_of(8);
        *self.segments_in_block.entry(block).or_default() += 1;
        // `RetainOrFlush`
        let available = self.block_size - used;
        if available < min_reusable_space {
            self.flush_partial(block, &buffer)?;
        } else {
            self.partial_count += 1;
            self.partial
                .insert((available, self.partial_count), (block, buffer, used));
        }
        Ok((block as i64, at as u32))
    }

    /// `MetadataManager::AllocateHandle`: the lowest free sub-block of the first metadata block that has one,
    /// unless a new block would have a lower id; it starts out as the last of its chain.
    fn next_sub_block(&mut self) -> (u64, usize) {
        if let Some(reserved) = self.reserved.pop_front() {
            return reserved;
        }
        let candidate = self
            .meta
            .iter()
            .find(|(_, block)| block.available && block.free != 0)
            .map(|(id, _)| *id);
        let peek = self.free.first().copied().unwrap_or(self.next_block);
        let block = match candidate {
            Some(block) if block <= peek => block,
            _ => {
                let block = self.allocate_block();
                let state = MetaBlock {
                    free: u64::MAX,
                    data: Some(vec![0; self.block_size]),
                    dirty: true,
                    available: true,
                };
                self.meta.insert(block, state);
                block
            }
        };
        let size = metadata_block_size(self.block_size);
        let state = self.meta.get_mut(&block).expect("just found");
        let index = state.free.trailing_zeros() as usize;
        state.free &= !(1 << index);
        state.dirty = true;
        let data = state.data.as_mut().expect("loaded in `load_metadata_blocks`");
        // `NextBlock` and `Flush`: the chain ends here, and nothing of what was there before stays
        data[index * size..(index + 1) * size].fill(0);
        data[index * size..index * size + 8].copy_from_slice(&INVALID_BLOCK.to_le_bytes());
        (block, index)
    }

    /// `ConvertToTransient` ahead of time: the metadata blocks of the file that sub-blocks can be taken from.
    fn load_metadata_blocks(&mut self) -> Result<()> {
        let blocks = self.buffer.block_manager();
        for (id, block) in self.meta.iter_mut() {
            if block.available && block.free != 0 && block.data.is_none() {
                block.data = Some(blocks.read_block(*id)?);
            }
        }
        Ok(())
    }

    /// `ClearModifiedBlocks`: what the last checkpoint wrote at these sub-blocks stays; before v1.5.0, so does the
    /// sub-block each of them goes on to.
    fn keep_sub_blocks(&mut self, pointers: &[u64]) -> Result<()> {
        let mut next = Vec::new();
        if self.version < StorageVersion::V1_5_0 {
            let size = metadata_block_size(self.block_size);
            let mut seen = std::collections::HashSet::new();
            for &pointer in pointers {
                if !seen.insert(pointer) {
                    continue;
                }
                let at = MetaBlockPointer::new(pointer, 0);
                let block = self.buffer.pin(at.block_id())?;
                let start = at.block_index() as usize * size;
                let following = u64::from_le_bytes(block[start..start + 8].try_into().expect("8 bytes"));
                if following != INVALID_BLOCK {
                    next.push(following);
                }
            }
        }
        for pointer in pointers.iter().chain(&next) {
            let at = MetaBlockPointer::new(*pointer, 0);
            if let Some(taken) = self.modified.get_mut(&at.block_id()) {
                *taken &= !(1 << at.block_index());
            }
        }
        Ok(())
    }

    fn disk_pointer(&self, (block, index): (u64, usize)) -> u64 {
        block | (index as u64) << 56
    }

    /// The bytes of a sub-block that is being written.
    fn sub_block(&mut self, (block, index): (u64, usize)) -> &mut [u8] {
        let size = metadata_block_size(self.block_size);
        let data = self
            .meta
            .get_mut(&block)
            .and_then(|block| block.data.as_mut())
            .expect("a sub-block that was taken");
        &mut data[index * size..(index + 1) * size]
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
        if let Some(current) = stream.current {
            let pointer = self.disk_pointer(next);
            self.sub_block(current)[..8].copy_from_slice(&pointer.to_le_bytes());
        }
        stream.current = Some(next);
        stream.offset = SUB_BLOCK_HEADER_SIZE;
        if self.written_pages.is_some() {
            let pointer = self.disk_pointer(next);
            self.written_pages.as_mut().expect("just checked").push(pointer);
        }
    }

    /// `MetadataWriter::WriteData`.
    fn write_metadata(&mut self, stream: &mut MetaStream, mut data: &[u8]) {
        let size = metadata_block_size(self.block_size);
        while !data.is_empty() {
            if stream.current.is_none() || stream.offset >= size {
                self.advance(stream);
            }
            let current = stream.current.expect("just ensured");
            let take = data.len().min(size - stream.offset);
            let at = stream.offset;
            self.sub_block(current)[at..at + take].copy_from_slice(&data[..take]);
            stream.offset += take;
            data = &data[take..];
        }
    }

    /// `DUCKDB_LOG_TRACE` of `DetectBestCompressionMethod`: the method chosen for a column, and its score.
    /// `CompressionFunctionSet::GetCompressionFunctions`: whether `disabled_compression_methods` leaves the method
    /// out.
    fn is_disabled(&self, compression: u8) -> bool {
        let name = compression_setting_name(compression);
        self.disabled_compression.iter().any(|disabled| disabled == name)
    }

    fn log_choice(&self, compression: u8, ty: &str, score: usize) {
        let (table, column) = &self.log_column;
        let line = format!(
            "ColumnDataCheckpointer FinalAnalyze({}) result for {table}.{column}({ty}): {score}",
            compression_enum_name(compression)
        );
        match &self.deferred {
            Some(deferred) => deferred.logs.lock().push(line),
            None => crate::logging::log_database_trace(&line),
        }
    }

    /// `ColumnDataCheckpointer::DetectBestCompressionMethod`: the methods that can store the type, in
    /// the order of `internal_compression_methods`, each scored by its `final_analyze`; the lowest score
    /// wins, the first of equal ones. With `force_compression` only that method and UNCOMPRESSED (to
    /// fall back on) are left, and the forced one is chosen whenever it can store the column.
    /// ALP and ALPRD are scored by what they encode to, as their analyze isn't ported yet.
    fn encode_fixed(&self, ty: &LogicalType, data: &[u8], valid: &[bool], width: usize) -> (Vec<Encoded>, u8) {
        use super::bitpacking;
        let physical = ty.physical_type();
        let count = valid.len();
        let force = self.force_compression.as_str();
        // `AlpInitAnalyze`: before v1.5.0 vectors can't be stored uncompressed, which small blocks need
        let fallback = self.version >= StorageVersion::V1_5_0;
        let small_blocks = self.buffer.block_manager().block_alloc_size() < DEFAULT_BLOCK_ALLOC_SIZE as usize;
        let float = matches!(physical, PhysicalType::Float | PhysicalType::Double) && (fallback || !small_blocks);
        let kind = bitpacking::Kind {
            width,
            signed: is_signed(physical),
        };
        // the methods for the type, in C++'s order
        let mut methods: Vec<u8> = vec![COMPRESSION_UNCOMPRESSED];
        if supports_rle(physical) {
            methods.push(COMPRESSION_RLE);
        }
        if supports_bitpacking(physical) {
            methods.push(COMPRESSION_BITPACKING);
        }
        if float {
            methods.extend([COMPRESSION_ALP, COMPRESSION_ALPRD]);
        }
        // `RoaringInitAnalyze`: booleans from v1.5.0
        let bool_roaring = physical == PhysicalType::Bool && self.version >= StorageVersion::V1_5_0;
        if bool_roaring {
            methods.push(COMPRESSION_ROARING);
        }
        let roaring = || {
            let bits: Vec<bool> = data.iter().map(|byte| *byte != 0).collect();
            encode_roaring(&bits, self.block_size)
        };
        methods.retain(|method| !self.is_disabled(*method));
        // `ForceCompression`: a forced method the type has leaves only it and UNCOMPRESSED
        let forced = methods
            .iter()
            .copied()
            .find(|method| compression_setting_name(*method) == force);
        if let Some(forced) = forced {
            methods.retain(|method| *method == forced || *method == COMPRESSION_UNCOMPRESSED);
        }
        let mut packed_groups = None;
        let mut best: Option<(u8, usize)> = None;
        for method in methods {
            let score = match method {
                COMPRESSION_UNCOMPRESSED => Some(width * count),
                COMPRESSION_RLE => Some(rle_score(data, valid, width)),
                COMPRESSION_BITPACKING => {
                    let groups = bitpacking::groups(data, valid, kind, self.bitpacking_mode, self.block_size);
                    let score = groups.as_deref().map(bitpacking::analyzed_size);
                    packed_groups = groups;
                    score
                }
                COMPRESSION_ALP => Some(encoded_size(&encode_alp(data, width, self.block_size, fallback))),
                COMPRESSION_ALPRD => Some(encoded_size(&encode_alprd(data, width, self.block_size, fallback))),
                _ => {
                    let bits: Vec<bool> = data
                        .iter()
                        .zip(valid)
                        .map(|(byte, valid)| *valid && *byte != 0)
                        .collect();
                    Some(super::roaring::final_analyze(&bits, self.block_size))
                }
            };
            let Some(score) = score else { continue };
            let is_forced = Some(method) == forced;
            if best.is_none_or(|(_, best)| score < best) || is_forced {
                best = Some((method, score));
            }
            if is_forced {
                break;
            }
        }
        let (method, score) = best.expect("UNCOMPRESSED can store any fixed-size type");
        self.log_choice(method, &ty.to_string(), score);
        let segments = match method {
            COMPRESSION_RLE => encode_rle(data, width, self.block_size),
            COMPRESSION_BITPACKING => bitpacking::segments(packed_groups.expect("analyzed"), self.block_size),
            COMPRESSION_ALP => encode_alp(data, width, self.block_size, fallback),
            COMPRESSION_ALPRD => encode_alprd(data, width, self.block_size, fallback),
            COMPRESSION_ROARING => roaring(),
            _ => {
                let rows = self.block_size / width;
                data.chunks(rows * width)
                    .map(|d| (d.to_vec(), d.len() / width))
                    .collect()
            }
        };
        (segments, method)
    }

    fn write_fixed(&mut self, ty: &LogicalType, parts: &[Part], valid: &[bool]) -> Result<Vec<SegmentOut>> {
        let physical = ty.physical_type();
        let width = fixed_width(physical).ok_or_else(|| write_not_ported(&format!("a column of type {ty}")))?;
        let mut data = Vec::with_capacity(valid.len() * width);
        for part in parts {
            data.extend_from_slice(&raw_bytes(&part.vector, part.count)?);
        }
        let stored = if valid.iter().all(|v| *v) {
            data
        } else {
            let mut filled = data;
            fill_nulls(&mut filled, width, valid);
            filled
        };
        let (encoded, compression) = self.encode_fixed(ty, &stored, valid, width);
        let mut segments = Vec::with_capacity(encoded.len());
        let mut row = 0;
        for (bytes, rows) in encoded {
            let (values, valid_rows) = (&stored[row * width..(row + rows) * width], &valid[row..row + rows]);
            row += rows;
            let mut stats = Stats::of_validity(valid_rows);
            stats.bounds = bounds(physical, values, width, valid_rows);
            // `IsConstant`: a value that is the same throughout is in the statistics, with no block
            // and throughout means its bits: -0.0 and 0.0 compare as equal
            let same_bits = || {
                let mut valid = values.chunks_exact(width).zip(valid_rows).filter(|(_, valid)| **valid);
                let first = valid.next().map(|(value, _)| value);
                valid.all(|(value, _)| Some(value) == first)
            };
            // an all-NULL segment is constant too: its empty statistics have the minimum above the maximum
            let all_null = !valid_rows.iter().any(|valid| *valid);
            let constant = all_null || (stats.bounds.is_some_and(|(min, max)| min == max) && same_bits());
            if constant && physical != PhysicalType::Interval {
                segments.push(SegmentOut {
                    tuple_count: rows,
                    block_id: -1,
                    offset: 0,
                    compression: COMPRESSION_CONSTANT,
                    stats,
                    overflow_blocks: Vec::new(),
                });
                continue;
            }
            let (block_id, offset) = self.place(&bytes)?;
            segments.push(SegmentOut {
                tuple_count: rows,
                block_id,
                offset,
                compression,
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

    /// `ZSTDCompressionState`: per segment the metadata of its vectors, and per vector of 2048 strings their
    /// lengths and a zstd frame of them, after one another and on through pages of the segment's own, each of
    /// which ends in the id of the next where a vector goes on.
    fn write_zstd(&mut self, strings: &[&[u8]], valid: &[bool]) -> Result<Vec<SegmentOut>> {
        const VECTOR_ROWS: usize = 2048;
        let block_size = self.block_size;
        let writable = block_size - 8;
        let metadata_size =
            |vectors: usize| ((8 * vectors).next_multiple_of(4) + 4 * vectors).next_multiple_of(8) + 16 * vectors;
        let mut per_segment = 1;
        while metadata_size(per_segment + 1) <= writable {
            per_segment += 1;
        }
        let mut segments = Vec::new();
        let mut start = 0;
        while start < strings.len() {
            let rows = (strings.len() - start).min(per_segment * VECTOR_ROWS);
            let vectors = rows.div_ceil(VECTOR_ROWS);
            // the pages: the segment's first, then those of its own
            let mut pages: Vec<(Option<u64>, Vec<u8>)> = vec![(None, vec![0; block_size])];
            let mut position = metadata_size(vectors);
            let mut metadata: Vec<(i64, u32, u64, u64)> = Vec::with_capacity(vectors);
            for vector in 0..vectors {
                let first = start + vector * VECTOR_ROWS;
                let last = (first + VECTOR_ROWS).min(start + rows);
                let lengths: Vec<u32> = (first..last)
                    .map(|row| if valid[row] { strings[row].len() as u32 } else { 0 })
                    .collect();
                let data: Vec<u8> = (first..last)
                    .filter(|&row| valid[row])
                    .flat_map(|row| strings[row].iter().copied())
                    .collect();
                let frame = zstd::bulk::compress(&data, 3)
                    .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("ZSTD Compression failed: {e}")))?;
                position = position.next_multiple_of(4);
                let next_page = |pages: &mut Vec<(Option<u64>, Vec<u8>)>, position: usize, this: &mut Self| {
                    let id = this.allocate_block();
                    let page = &mut pages.last_mut().expect("a page").1;
                    page[position..position + 8].copy_from_slice(&id.to_le_bytes());
                    pages.push((Some(id), vec![0; block_size]));
                };
                if position + lengths.len() * 4 >= writable {
                    next_page(&mut pages, position, self);
                    position = 0;
                }
                let page_id = pages.last().expect("a page").0.map_or(-1, |id| id as i64);
                metadata.push((page_id, position as u32, data.len() as u64, frame.len() as u64));
                {
                    let page = &mut pages.last_mut().expect("a page").1;
                    for length in &lengths {
                        page[position..position + 4].copy_from_slice(&length.to_le_bytes());
                        position += 4;
                    }
                }
                let mut rest = frame.as_slice();
                while !rest.is_empty() {
                    let take = rest.len().min(writable - position);
                    let page = &mut pages.last_mut().expect("a page").1;
                    page[position..position + take].copy_from_slice(&rest[..take]);
                    position += take;
                    rest = &rest[take..];
                    if !rest.is_empty() {
                        next_page(&mut pages, position, self);
                        position = 0;
                    }
                }
            }
            // the metadata, at the start of the segment
            let ids_at = 0;
            let offsets_at = (8 * vectors).next_multiple_of(4);
            let uncompressed_at = (offsets_at + 4 * vectors).next_multiple_of(8);
            let compressed_at = uncompressed_at + 8 * vectors;
            let segment = &mut pages[0].1;
            for (vector, (page, offset, uncompressed, compressed)) in metadata.iter().enumerate() {
                segment[ids_at + 8 * vector..ids_at + 8 * vector + 8].copy_from_slice(&page.to_le_bytes());
                segment[offsets_at + 4 * vector..offsets_at + 4 * vector + 4].copy_from_slice(&offset.to_le_bytes());
                let at = uncompressed_at + 8 * vector;
                segment[at..at + 8].copy_from_slice(&uncompressed.to_le_bytes());
                let at = compressed_at + 8 * vector;
                segment[at..at + 8].copy_from_slice(&compressed.to_le_bytes());
            }
            let overflow_blocks: Vec<i64> = pages.iter().filter_map(|(id, _)| id.map(|id| id as i64)).collect();
            let (block_id, offset) = if overflow_blocks.is_empty() {
                // a segment on its page alone can share its block
                let size = position;
                let page = std::mem::take(&mut pages[0].1);
                self.place(&page[..size])?
            } else {
                // what goes on to other pages starts the block of its segment
                let block = self.allocate_block();
                *self.segments_in_block.entry(block).or_default() += 1;
                self.write_data_block(block, &pages[0].1)?;
                (block as i64, 0)
            };
            for (id, page) in pages.iter().skip(1) {
                self.write_data_block(id.expect("an extra page"), page)?;
            }
            segments.push(SegmentOut {
                tuple_count: rows,
                block_id,
                offset,
                compression: COMPRESSION_ZSTD,
                stats: Stats::of_validity(&valid[start..start + rows]),
                overflow_blocks,
            });
            start += rows;
        }
        Ok(segments)
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
        match self.choose_string_compression(&strings, valid, limit) {
            COMPRESSION_ZSTD if !strings.is_empty() => return self.write_zstd(&strings, valid),
            COMPRESSION_DICT_FSST => {
                if let Some(segments) = self.write_dict_fsst(&strings, valid, limit)? {
                    return Ok(segments);
                }
            }
            COMPRESSION_DICTIONARY => {
                if let Some(encoded) = encode_old_dictionary(&strings, valid, self.block_size, limit) {
                    return self.place_encoded(encoded, valid, COMPRESSION_DICTIONARY);
                }
            }
            COMPRESSION_FSST => {
                let encoded = SymbolTable::train(&strings)
                    .and_then(|table| encode_old_fsst(&strings, valid, self.block_size, limit, &table));
                if let Some(encoded) = encoded {
                    return self.place_encoded(encoded, valid, COMPRESSION_FSST);
                }
            }
            _ => {}
        }
        self.write_plain_strings(&strings, valid, limit)
    }

    /// `ColumnDataCheckpointer::DetectBestCompressionMethod` of a VARCHAR column: from v1.3.0 on UNCOMPRESSED,
    /// ZSTD and DICT_FSST by their `final_analyze` scores (DICTIONARY and FSST don't analyze any more, so forcing
    /// either leaves UNCOMPRESSED); before it UNCOMPRESSED, DICTIONARY, FSST and, from v1.2.0, ZSTD. A forced
    /// string method the version doesn't have leaves UNCOMPRESSED.
    fn choose_string_compression(&self, strings: &[&[u8]], valid: &[bool], limit: usize) -> u8 {
        let force = self.force_compression.as_str();
        let rows = strings.len();
        let valid_lengths = || {
            strings
                .iter()
                .zip(valid)
                .filter(|(_, valid)| **valid)
                .map(|(s, _)| s.len())
        };
        let total_size: usize = valid_lengths().sum();
        // `UncompressedStringStorage::StringFinalAnalyze`
        let overflow = valid_lengths().filter(|length| *length >= limit).count();
        let uncompressed = rows * 4 + total_size + overflow * BIG_STRING_MARKER_SIZE;
        // `ZSTDStorage::StringFinalAnalyze`
        let zstd = || {
            if rows == 0 {
                return usize::MAX;
            }
            let penalty = if total_size / rows >= self.zstd_min_string_length {
                1.0
            } else {
                1000.0
            };
            let vectors = rows.div_ceil(2048);
            let metadata = ((8 * vectors).next_multiple_of(4) + 4 * vectors).next_multiple_of(8) + 16 * vectors;
            let estimate = (total_size as f64 / 2.0) as usize + rows * 4 + metadata;
            (estimate as f64 * penalty) as usize
        };
        let mut methods = if self.version >= StorageVersion::V1_3_0 {
            vec![COMPRESSION_UNCOMPRESSED, COMPRESSION_ZSTD, COMPRESSION_DICT_FSST]
        } else {
            let mut methods = vec![COMPRESSION_UNCOMPRESSED, COMPRESSION_DICTIONARY, COMPRESSION_FSST];
            // `ZSTDStorage::StringInitAnalyze`: from v1.2.0 on
            if self.version >= StorageVersion::V1_2_0 {
                methods.push(COMPRESSION_ZSTD);
            }
            methods
        };
        methods.retain(|method| !self.is_disabled(*method));
        let string_method = matches!(force, "zstd" | "dict_fsst" | "dictionary" | "fsst" | "uncompressed");
        let forced = methods
            .iter()
            .copied()
            .find(|method| compression_setting_name(*method) == force)
            .or_else(|| string_method.then_some(COMPRESSION_UNCOMPRESSED));
        if let Some(forced) = forced {
            methods.retain(|method| *method == forced || *method == COMPRESSION_UNCOMPRESSED);
        }
        let mut best: Option<(u8, usize)> = None;
        for method in methods {
            let score = match method {
                COMPRESSION_UNCOMPRESSED => Some(uncompressed),
                COMPRESSION_ZSTD => Some(zstd()),
                COMPRESSION_DICTIONARY => {
                    super::old_string_analyze::dictionary_final_analyze(strings, valid, self.block_size, limit)
                }
                COMPRESSION_FSST => {
                    super::old_string_analyze::fsst_final_analyze(strings, valid, self.block_size, limit)
                }
                _ => super::dict_fsst_analyze::final_analyze(strings, valid, self.block_size),
            };
            let Some(score) = score else { continue };
            let is_forced = Some(method) == forced;
            if best.is_none_or(|(_, best)| score < best) || is_forced {
                best = Some((method, score));
            }
            if is_forced {
                break;
            }
        }
        let (method, score) = best.expect("UNCOMPRESSED can store any string");
        self.log_choice(method, "VARCHAR", score);
        method
    }

    /// Segments of `compression` encoded already, placed in blocks.
    fn place_encoded(&mut self, encoded: Vec<Encoded>, valid: &[bool], compression: u8) -> Result<Vec<SegmentOut>> {
        let mut segments = Vec::with_capacity(encoded.len());
        let mut row = 0;
        for (bytes, rows) in encoded {
            let (block_id, offset) = self.place(&bytes)?;
            segments.push(SegmentOut {
                tuple_count: rows,
                block_id,
                offset,
                compression,
                stats: Stats::of_validity(&valid[row..row + rows]),
                overflow_blocks: Vec::new(),
            });
            row += rows;
        }
        Ok(segments)
    }

    /// DICT_FSST segments, as `DictFSSTCompressionState` fills them; `None` where a string is too long for them.
    fn write_dict_fsst(&mut self, strings: &[&[u8]], valid: &[bool], limit: usize) -> Result<Option<Vec<SegmentOut>>> {
        if strings
            .iter()
            .zip(valid)
            .any(|(string, valid)| *valid && string.len() >= limit)
        {
            return Ok(None);
        }
        let fsst_limit = super::dict_fsst_analyze::fsst_string_limit(self.block_size);
        let encoded = super::dict_fsst_compress::compress(strings, valid, self.block_size, fsst_limit);
        let mut segments = Vec::with_capacity(encoded.len());
        let mut row = 0;
        for (bytes, rows) in encoded {
            let (block_id, offset) = self.place(&bytes)?;
            segments.push(SegmentOut {
                tuple_count: rows,
                block_id,
                offset,
                compression: COMPRESSION_DICT_FSST,
                stats: Stats::of_validity(&valid[row..row + rows]),
                overflow_blocks: Vec::new(),
            });
            row += rows;
        }
        Ok(Some(segments))
    }

    /// `UncompressedStringStorage`: the strings after one another, those too large for a segment in
    /// overflow blocks.
    fn write_plain_strings(&mut self, strings: &[&[u8]], valid: &[bool], limit: usize) -> Result<Vec<SegmentOut>> {
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
                self.write_data_block(block, &data)?;
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

    /// `DetectBestCompressionMethod` of a validity column: UNCOMPRESSED, and ROARING from v1.2.0.
    fn validity_choice(&self, valid: &[bool]) -> u8 {
        let force = self.force_compression.as_str();
        let plain_score = valid.len().div_ceil(8);
        let can_roaring =
            self.version >= StorageVersion::V1_2_0 && force != "uncompressed" && !self.is_disabled(COMPRESSION_ROARING);
        let roaring_score = can_roaring.then(|| super::roaring::final_analyze(valid, self.block_size));
        let use_roaring = roaring_score.is_some_and(|score| force == "roaring" || score < plain_score);
        let (chosen, score) = if use_roaring {
            (COMPRESSION_ROARING, roaring_score.expect("scored"))
        } else {
            (COMPRESSION_UNCOMPRESSED, plain_score)
        };
        self.log_choice(chosen, "VALIDITY", score);
        chosen
    }

    /// `ValidityCoveredByBasedata`: DICT_FSST keeps the NULLs of its column, so the validity column is
    /// EMPTY, one segment of no data, or a constant where it is the same throughout.
    fn write_empty_validity(&mut self, valid: &[bool]) -> ColumnOut {
        let stats = Stats::of_validity(valid);
        let uniform = !stats.has_null || !stats.has_no_null;
        if !uniform {
            self.validity_choice(valid);
        }
        let segments = if valid.is_empty() {
            Vec::new()
        } else {
            vec![SegmentOut {
                tuple_count: valid.len(),
                block_id: -1,
                offset: 0,
                compression: if uniform {
                    COMPRESSION_CONSTANT
                } else {
                    COMPRESSION_EMPTY
                },
                stats: stats.clone(),
                overflow_blocks: Vec::new(),
            }]
        };
        ColumnOut {
            segments,
            validity: None,
            children: Vec::new(),
            stats,
            shredded_type: None,
        }
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
            let use_roaring = self.validity_choice(valid) == COMPRESSION_ROARING;
            let encoded: Vec<(Vec<u8>, usize, u8)> = if use_roaring {
                encode_roaring(valid, self.block_size)
                    .into_iter()
                    .map(|(bytes, rows)| (bytes, rows, COMPRESSION_ROARING))
                    .collect()
            } else {
                valid
                    .chunks(self.block_size / 8 * 64)
                    .map(|valid| {
                        let mut words = vec![0u8; valid.len().div_ceil(64) * 8];
                        for (row, _) in valid.iter().enumerate().filter(|(_, valid)| **valid) {
                            words[row / 8] |= 1 << (row % 8);
                        }
                        (words, valid.len(), COMPRESSION_UNCOMPRESSED)
                    })
                    .collect()
            };
            let mut row = 0;
            for (bytes, rows, compression) in encoded {
                let (block_id, offset) = self.place(&bytes)?;
                segments.push(SegmentOut {
                    tuple_count: rows,
                    block_id,
                    offset,
                    compression,
                    stats: Stats::of_validity(&valid[row..row + rows]),
                    overflow_blocks: Vec::new(),
                });
                row += rows;
            }
        }
        Ok(ColumnOut {
            segments,
            validity: None,
            children: Vec::new(),
            stats,
            shredded_type: None,
        })
    }

    /// `ColumnData::Checkpoint`.
    fn write_column_data(&mut self, ty: &LogicalType, parts: &[Part]) -> Result<ColumnOut> {
        if ty.id() == LogicalTypeId::Variant {
            return self.write_variant(parts);
        }
        // before v1.5.0, a GEOMETRY is stored as the spatial extension's blob (`GetSpatialGeometryType`)
        if ty.id() == LogicalTypeId::Geometry && self.version < StorageVersion::V1_5_0 {
            let mut spatial_parts = Vec::with_capacity(parts.len());
            for part in parts {
                let mut blobs = Vector::new(LogicalType::BLOB, part.count.max(1));
                for row in 0..part.count {
                    let value = part.vector.get_value(row);
                    match value.data() {
                        ValueData::Bytes(wkb) if !value.is_null() => {
                            blobs.set_value(row, &Value::blob(duckrs_common::geometry::to_spatial(wkb)?))?
                        }
                        _ => blobs.set_null(row, true),
                    }
                }
                spatial_parts.push(Part::new(&blobs, part.count));
            }
            let mut out = self.write_column_data(&LogicalType::BLOB, &spatial_parts)?;
            // `GeometryStats::CreateUnknown`, of which only the validity is kept
            out.stats.strings = None;
            return Ok(out);
        }
        let rows = parts.iter().map(|part| part.count).sum();
        let mut valid = Vec::with_capacity(rows);
        for part in parts {
            valid.extend((0..part.count).map(|row| part.vector.is_valid(row)));
        }
        let physical = ty.physical_type();
        // the data of a column is analyzed before its validity, which DICT_FSST covers
        let mut string_segments = match physical {
            PhysicalType::Varchar => Some(self.write_strings(parts, &valid)?),
            _ => None,
        };
        let covered = string_segments
            .as_ref()
            .and_then(|segments| segments.first())
            .is_some_and(|segment| segment.compression == COMPRESSION_DICT_FSST);
        let validity = Box::new(if covered {
            self.write_empty_validity(&valid)
        } else {
            self.write_validity(&valid)?
        });
        let mut stats = Stats::of_validity(&valid);
        let mut list_lengths = None;
        let (segments, children) = match physical {
            PhysicalType::Varchar => (string_segments.take().expect("written above"), Vec::new()),
            PhysicalType::List => {
                let mut offsets = Vec::with_capacity(rows * 8);
                let mut child_parts = Vec::with_capacity(parts.len());
                let mut total = 0u64;
                // `ListColumnData::Append`: the lengths of the lists that aren't NULL
                let mut lengths = crate::catalog::ListLengths::empty();
                for part in parts {
                    let entries = part.vector.data::<ListEntry>();
                    let mut selection = Vec::new();
                    for (row, entry) in entries.iter().enumerate().take(part.count) {
                        if part.vector.is_valid(row) {
                            selection.extend((entry.offset..entry.offset + entry.length).map(|i| i as u32));
                            total += entry.length;
                            lengths = lengths.merge(&crate::catalog::ListLengths::of(entry.length), true);
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
                list_lengths = Some(lengths);
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
        let mut segments = segments;
        // `GeometryStats::Update` of each value, by the segment it is in
        if ty.id() == LogicalTypeId::Geometry {
            let mut values = parts
                .iter()
                .flat_map(|part| (0..part.count).map(|row| part.vector.get_value(row)));
            for segment in &mut segments {
                let mut geometry = duckrs_common::geometry::GeometryStats::empty();
                for value in values.by_ref().take(segment.tuple_count) {
                    if let duckrs_common::value::ValueData::Bytes(wkb) = value.data()
                        && !value.is_null()
                    {
                        geometry.update(wkb)?;
                    }
                }
                segment.stats.geometry = Some(geometry);
            }
            if segments.is_empty() {
                stats.geometry = Some(duckrs_common::geometry::GeometryStats::empty());
            }
        }
        // `StatsWriter<string_t>::Update` of each value, by the segment it is in
        if matches!(ty.id(), LogicalTypeId::Varchar | LogicalTypeId::Blob) {
            let varchar = ty.id() == LogicalTypeId::Varchar;
            let mut values = parts.iter().flat_map(|part| {
                let data = part.vector.data::<StringT>();
                (0..part.count).map(move |row| {
                    // SAFETY: the row is valid, so its string lives in the heap of the vector
                    part.vector
                        .is_valid(row)
                        .then(|| unsafe { data[row].as_bytes() }.to_vec())
                })
            });
            for segment in &mut segments {
                let strings: Vec<Vec<u8>> = values.by_ref().take(segment.tuple_count).flatten().collect();
                segment.stats.strings = Some(StringStatsOut::of(strings.into_iter(), varchar));
            }
            if segments.is_empty() {
                stats.strings = Some(StringStatsOut::of(std::iter::empty(), varchar));
            }
        }
        for segment in &segments {
            stats.merge(&segment.stats, ty);
        }
        stats.children = children.iter().map(|child| child.stats.clone()).collect();
        // they are of the column, not of its segments
        stats.lists = list_lengths;
        Ok(ColumnOut {
            segments,
            validity: Some(validity),
            children,
            stats,
            shredded_type: None,
        })
    }

    /// `VariantColumnData::Checkpoint`: its validity, and the STRUCT DuckDB stores its values as, with the
    /// values that have a common shape in a shredded part next to it when the row group is large enough.
    fn write_variant(&mut self, parts: &[Part]) -> Result<ColumnOut> {
        use duckrs_common::variant::{VariantNode, shredding};
        let unshredded = duckrs_common::variant::unshredded_type();
        let rows: usize = parts.iter().map(|part| part.count).sum();
        let mut valid = Vec::with_capacity(rows);
        let mut nodes = Vec::with_capacity(rows);
        for part in parts {
            for row in 0..part.count {
                let value = part.vector.get_value(row);
                valid.push(!value.is_null());
                nodes.push(match value.data() {
                    ValueData::Bytes(bytes) => VariantNode::decode(bytes)?,
                    _ => VariantNode::Null,
                });
            }
        }
        // `EnableShredding`, then the type: the one forced, or the one the values suggest
        let minimum = self.variant_minimum_shredding_size;
        let shredded_type = if minimum >= 0 && rows as u64 >= minimum as u64 {
            match &self.force_variant_shredding {
                Some(forced) => Some(forced.clone()),
                None => {
                    // a NULL row counts as a VARIANT NULL (`VariantVisitor::Visit`)
                    let mut stats = shredding::ShreddingStats::new();
                    for node in &nodes {
                        stats.update(node)?;
                    }
                    stats.shredded_type()
                }
            }
        } else {
            None
        };
        let mut stored = Vector::new(unshredded.clone(), rows.max(1));
        let mut shredded = shredded_type.as_ref().map(|ty| Vector::new(ty.clone(), rows.max(1)));
        for (row, node) in nodes.iter().enumerate() {
            if !valid[row] {
                stored.set_null(row, true);
                if let Some(shredded) = &mut shredded {
                    shredded.set_null(row, true);
                }
                continue;
            }
            match (&shredded_type, &mut shredded) {
                (Some(ty), Some(shredded)) => {
                    let (typed, rest) = shredding::shred(node, ty)?;
                    shredded.set_value(row, &typed)?;
                    stored.set_value(row, &rest)?;
                }
                _ => stored.set_value(row, &node.to_unshredded()?)?,
            }
        }
        let mut children = vec![self.write_column_data(&unshredded, &[Part::new(&stored, rows)])?];
        let mut stats = Stats::of_validity(&valid);
        if let (Some(ty), Some(shredded)) = (&shredded_type, &shredded) {
            let out = self.write_column_data(ty, &[Part::new(shredded, rows)])?;
            stats.variant_shredded = Some(Box::new((ty.clone(), out.stats.clone())));
            children.push(out);
        }
        let validity = Box::new(self.write_validity(&valid)?);
        Ok(ColumnOut {
            segments: Vec::new(),
            validity: Some(validity),
            children,
            stats,
            shredded_type,
        })
    }

    /// `ColumnData::Checkpoint` of the rows of a column of a row group.
    fn encode_column(&mut self, table: &TableCatalogEntry, column: usize, parts: &[Part]) -> Result<ColumnOut> {
        let ty = &table.columns[column].ty;
        self.log_column = (format!("{}.{}", table.schema, table.name), column);
        // `ColumnCheckpointInfo::GetCompressionType`: the method of the column, its validity and children
        // too, in place of `force_compression`; one that none of them has leaves them all to choose
        let forced = table.columns[column]
            .compression
            .map(|compression| compression_setting_name(compression).to_string());
        let setting = forced.map(|forced| std::mem::replace(&mut self.force_compression, forced));
        let out = self.write_column_data(ty, parts);
        if let Some(setting) = setting {
            self.force_compression = setting;
        }
        out
    }

    /// A column [`encode_deferred`] encoded, with its segments placed now and its lines logged, as encoding it here
    /// would have.
    fn place_deferred(&mut self, mut out: ColumnOut, held: Deferred) -> Result<ColumnOut> {
        for line in held.logs.into_inner() {
            crate::logging::log_database_trace(&line);
        }
        let placed = held
            .placed
            .iter()
            .map(|data| self.place(data))
            .collect::<Result<Vec<_>>>()?;
        fn patch(column: &mut ColumnOut, placed: &[(i64, u32)]) {
            for segment in &mut column.segments {
                if (DEFERRED_BLOCK..DEFERRED_BLOCK + placed.len() as i64).contains(&segment.block_id) {
                    (segment.block_id, segment.offset) = placed[(segment.block_id - DEFERRED_BLOCK) as usize];
                }
            }
            if let Some(validity) = &mut column.validity {
                patch(validity, placed);
            }
            for child in &mut column.children {
                patch(child, placed);
            }
        }
        patch(&mut out, &placed);
        Ok(out)
    }

    /// The blocks of a column written ahead of a commit, which the checkpoint keeps.
    fn keep_written_blocks(&mut self, column: &ColumnOut) {
        for segment in &column.segments {
            if segment.block_id >= 0 {
                self.used.insert(segment.block_id as u64);
                *self.segments_in_block.entry(segment.block_id as u64).or_default() += 1;
            }
            self.used.extend(
                segment
                    .overflow_blocks
                    .iter()
                    .filter(|block| **block >= 0)
                    .map(|block| *block as u64),
            );
        }
        if let Some(validity) = &column.validity {
            self.keep_written_blocks(validity);
        }
        for child in &column.children {
            self.keep_written_blocks(child);
        }
    }

    /// `RowGroup::WriteToDisk`.
    fn write_row_group(
        &mut self,
        table: &TableCatalogEntry,
        parts: Vec<Vec<Part>>,
        row_start: u64,
        rows: usize,
        deletes: Deletes,
        statistics: &mut [Option<Stats>],
        reused: Vec<Option<(MetaBlockPointer, Vec<u64>)>>,
        written: Option<&[ColumnOut]>,
    ) -> Result<Vec<u8>> {
        let mut tables = std::mem::take(&mut self.tables);
        let mut pointers = Vec::with_capacity(parts.len());
        // of each column, the metadata blocks it takes other than the one it starts in
        let mut column_blocks: Vec<Vec<u64>> = Vec::with_capacity(parts.len());
        for (column, (parts, reused)) in parts.iter().zip(reused).enumerate() {
            if let Some((pointer, extras)) = reused {
                // what it has isn't read again, so it can't be counted
                if let Some(counted) = self.counted_distinct.get_mut(column) {
                    *counted = None;
                }
                pointers.push(pointer);
                column_blocks.push(extras);
                continue;
            }
            let ty = &table.columns[column].ty;
            if let Some(counted) = self.counted_distinct.get_mut(column)
                && let Some(state) = counted
            {
                let complete = written.is_none() && parts.iter().all(|part| state.update(ty, &part.vector, part.count));
                if !complete {
                    *counted = None;
                }
            }
            // rows written ahead of the commit are in their blocks already (`ColumnData::HasChanges`)
            let out = match written {
                Some(columns) => {
                    let out = resolved(&columns[column], self.buffer);
                    self.keep_written_blocks(&out);
                    out
                }
                None => self.encode_column(table, column, parts)?,
            };
            match &mut statistics[column] {
                Some(stats) => stats.merge(&out.stats, ty),
                empty => *empty = Some(out.stats.clone()),
            }
            let mut s = serializer(self.version);
            s.write_object_value(|s| write_column(s, &out, Some(ty), row_start));
            // `StartWritingColumns`: the blocks the column's metadata goes on to
            self.written_pages = Some(Vec::new());
            let pointer = self.pointer(&mut tables);
            self.write_metadata(&mut tables, &s.into_bytes());
            let pages = self.written_pages.take().unwrap_or_default();
            pointers.push(pointer);
            column_blocks.push(
                pages
                    .into_iter()
                    .filter(|page| *page != pointer.block_pointer)
                    .collect(),
            );
        }
        // `RowGroup::CheckpointDeletes`: as they were, or in a writer of their own, at the sub-blocks it takes
        let delete_pointers = match deletes {
            Deletes::Keep(pointers) => {
                let sub_blocks: Vec<u64> = pointers.iter().map(|pointer| pointer.block_pointer).collect();
                self.keep_sub_blocks(&sub_blocks)?;
                pointers
            }
            Deletes::Write(None) => Vec::new(),
            Deletes::Write(Some(bytes)) => {
                let mut stream = MetaStream::default();
                let columns = self.written_pages.replace(Vec::new());
                self.pointer(&mut stream);
                self.write_metadata(&mut stream, &bytes);
                let written = std::mem::replace(&mut self.written_pages, columns).unwrap_or_default();
                written.into_iter().map(|page| MetaBlockPointer::new(page, 0)).collect()
            }
        };
        self.tables = tables;
        let mut s = serializer(self.version);
        s.write_object_value(|s| {
            s.write_property(100, "row_start", &row_start);
            s.write_property(101, "tuple_count", &(rows as u64));
            s.write_property(102, "data_pointers", &pointers);
            s.write_property(103, "delete_pointers", &delete_pointers);
            // `RowGroup::Serialize`: the metadata blocks of the columns, all together and, where a checkpoint can
            // keep columns on their own, per column (`PerColumnMetadataBlocks`, a column's index marked by the top bit)
            if s.should_serialize(StorageVersion::V1_4_0) {
                let all: Vec<u64> = column_blocks.iter().flatten().copied().collect();
                s.write_property(104, "has_metadata_blocks", &true);
                s.write_property_with_default(105, "extra_metadata_blocks", &all);
                if self.per_column_writes {
                    let mut packed = Vec::new();
                    for (column, blocks) in column_blocks.iter().enumerate().filter(|(_, b)| !b.is_empty()) {
                        packed.push(column as u64 | 1 << 63);
                        packed.extend(blocks);
                    }
                    s.write_property(106, "has_per_column_metadata_blocks", &true);
                    s.write_property_with_default(107, "per_column_metadata_blocks", &packed);
                }
            }
        });
        Ok(s.into_bytes())
    }

    /// The blocks the segments of a column that a row group keeps are in, which stay taken.
    fn keep_blocks(&mut self, column: &super::table_data::PersistentColumn) {
        for pointer in &column.pointers {
            if pointer.block_id >= 0 {
                self.used.insert(pointer.block_id as u64);
                *self.segments_in_block.entry(pointer.block_id as u64).or_default() += 1;
            }
            self.used.extend(
                pointer
                    .overflow_blocks
                    .iter()
                    .filter(|block| **block >= 0)
                    .map(|block| *block as u64),
            );
        }
        if let Some(validity) = &column.validity {
            self.keep_blocks(validity);
        }
        for child in &column.children {
            self.keep_blocks(child);
        }
    }

    /// What the columns of a row group of the last checkpoint that stay as they were keep: their metadata,
    /// blocks and statistics, and whether they are loaded. `columns` says, of each column now, the stored one it
    /// is if it stays; the others are written again. Of those that stay, where their metadata starts and the
    /// metadata blocks it goes on to.
    fn keep_columns(
        &mut self,
        stored_group: usize,
        kept_group: &super::table_data::StoredGroup,
        columns: &[Option<usize>],
        types: &[LogicalType],
        statistics: &mut [Option<Stats>],
    ) -> Result<Vec<Option<(MetaBlockPointer, Vec<u64>)>>> {
        let mut reused = Vec::with_capacity(columns.len());
        let mut loaded = Vec::with_capacity(columns.len());
        // `RowGroup::Checkpoint`: the metadata of the columns that stay is kept (`ClearModifiedBlocks`)
        let mut kept = Vec::new();
        for (column, stored) in columns.iter().enumerate() {
            let Some(stored) = *stored else {
                reused.push(None);
                loaded.push(true);
                continue;
            };
            let ty = &types[column];
            let stored_column = &kept_group.columns[stored];
            let stats = stored_stats(stored_column, ty);
            match &mut statistics[column] {
                Some(merged) => merged.merge(&stats, ty),
                empty => *empty = Some(stats),
            }
            self.keep_blocks(stored_column);
            let pointer = kept_group.data_pointers[stored];
            let extras = kept_group.column_blocks[stored].clone();
            kept.push(pointer.block_pointer);
            kept.extend(extras.iter().copied());
            reused.push(Some((pointer, extras)));
            let was = self.stored_loaded.get(stored_group).and_then(|group| group.get(stored));
            loaded.push(was.copied().unwrap_or(true));
        }
        self.group_loaded.push(loaded);
        self.keep_sub_blocks(&kept)?;
        Ok(reused)
    }

    /// The vectors of a row group, with what indexes have to know. Rows that are deleted are NULL.
    fn write_group(
        &mut self,
        table: &TableCatalogEntry,
        group: &PlannedGroup,
        ids: &RowIds,
        rows: &mut PositionedRows<'_>,
        indexes: &mut [TableIndex],
        statistics: &mut [Option<Stats>],
    ) -> Result<(Vec<u8>, usize, bool)> {
        let types = table.types();
        let mut parts: Vec<Vec<Part>> = types.iter().map(|_| Vec::new()).collect();
        let mut kept = Vec::with_capacity(group.rows);
        // `RowGroup::CanReuseMetadata`: a row group of the last checkpoint, whose rows may still be the same
        let checkpointed = table.checkpointed();
        let stored = match (&checkpointed, group.merged || group.file_start != group.row_start) {
            (Some(source), false) => source
                .as_any()
                .downcast_ref::<super::table_data::DiskTable>()
                .filter(|disk| disk.is_in(self.buffer))
                .and_then(|disk| Some((disk, disk.group_at(group.row_start, group.rows)?))),
            _ => None,
        };
        // the rows are those the file has: of the columns, those still there stay, unread, and those an UPDATE or
        // an ALTER changed are written from memory
        if let (Some(source), true, Some((disk, stored_group))) = (self.in_file.clone(), indexes.is_empty(), stored) {
            use super::layered::Layer;
            let same: Vec<Option<usize>> = source
                .layers()
                .iter()
                .map(|layer| match layer {
                    Layer::Base(column) => Some(*column),
                    Layer::Memory { groups, from } => groups[stored_group].is_none().then_some(*from).flatten(),
                })
                .collect();
            let kept_group = disk.stored_group(stored_group)?;
            let identity = disk.column_count() == types.len() && same.iter().enumerate().all(|(at, c)| *c == Some(at));
            let unchanged = identity && (self.per_column_writes || !kept_group.per_column);
            let partly = !unchanged && self.per_column_writes && kept_group.per_column;
            if unchanged || partly {
                // the rows of the group that are left: of those the file has, those a DELETE didn't take
                let stored_rows: Vec<u32> = match disk.alive_rows(stored_group) {
                    Some(alive) => alive.to_vec(),
                    None => (0..group.rows as u32).collect(),
                };
                let mut kept = vec![false; group.rows];
                match source.alive_rows(stored_group) {
                    Some(left) => left
                        .iter()
                        .for_each(|row| kept[stored_rows[*row as usize] as usize] = true),
                    None => stored_rows.iter().for_each(|row| kept[*row as usize] = true),
                }
                // no DELETE since: the deletes are those of the file
                let deletes = match source.alive_rows(stored_group) {
                    None => Deletes::Keep(kept_group.deletes_pointers.clone()),
                    Some(_) => Deletes::Write((kept.iter().any(|kept| !kept)).then(|| deletes_of(&kept))),
                };
                let mut parts: Vec<Vec<Part>> = types.iter().map(|_| Vec::new()).collect();
                for (column, keep) in same.iter().enumerate() {
                    if keep.is_some() {
                        continue;
                    }
                    let live = crate::catalog::TableSource::read_group(&source, stored_group, &[column])?
                        .column(0)
                        .clone();
                    let values = with_deleted(live, &kept, &types[column])?;
                    for first in (0..group.rows).step_by(STANDARD_VECTOR_SIZE) {
                        let count = STANDARD_VECTOR_SIZE.min(group.rows - first);
                        let mut part = values.clone();
                        part.slice(
                            &SelectionVector::from_vec((first as u32..(first + count) as u32).collect()),
                            count,
                        );
                        parts[column].push(Part::new(&part, count));
                    }
                }
                let reused = self.keep_columns(stored_group, &kept_group, &same, &types, statistics)?;
                // `RowGroupWriteAction::REUSE_EXISTING_ROW_GROUP_METADATA`, deletes and all
                let whole = unchanged && matches!(deletes, Deletes::Keep(_));
                let written = self.write_row_group(
                    table,
                    parts,
                    group.file_start,
                    group.rows,
                    deletes,
                    statistics,
                    reused,
                    None,
                )?;
                return Ok((written, group.file_start as usize + group.rows, whole));
            }
        }
        // a row group a commit wrote ahead, whose rows are all still there: its columns stay in their blocks, and
        // only their metadata is written (`OptimisticDataWriter`)
        if indexes.is_empty() && !group.merged {
            let written = self
                .written
                .iter()
                .find(|written| written.row_start == group.row_start && written.rows == group.rows)
                .cloned();
            if let Some(written) = written {
                // the rows a DELETE took are deletes of the row group (`RowVersionManager::Checkpoint`)
                let kept: Vec<bool> = (0..group.rows)
                    .map(|offset| ids.position(group.row_start + offset as u64).is_some())
                    .collect();
                let deletes = Deletes::Write(kept.iter().any(|kept| !kept).then(|| deletes_of(&kept)));
                self.group_loaded.push(vec![true; types.len()]);
                let parts = types.iter().map(|_| Vec::new()).collect();
                let reused = vec![None; types.len()];
                let bytes = self.write_row_group(
                    table,
                    parts,
                    group.file_start,
                    group.rows,
                    deletes,
                    statistics,
                    reused,
                    Some(&written.columns),
                )?;
                return Ok((bytes, group.file_start as usize + group.rows, false));
            }
        }
        let mut live_chunks = Vec::new();
        let row_id = |offset: usize| group.row_start + offset as u64;
        let file_id = |offset: usize| group.file_start + offset as u64;
        let position_of = |offset: usize| {
            if group.merged {
                Some(group.first_position + offset)
            } else {
                ids.position(row_id(offset))
            }
        };
        for first in (0..group.rows).step_by(STANDARD_VECTOR_SIZE) {
            let count = STANDARD_VECTOR_SIZE.min(group.rows - first);
            let start = position_of(first);
            let in_place = start.is_some_and(|start| {
                start % STANDARD_VECTOR_SIZE == 0 && (0..count).all(|row| position_of(first + row) == Some(start + row))
            });
            let mut live_ids = Vec::with_capacity(count);
            let (chunk, live) = if let (true, Some(start)) = (in_place, start) {
                let mut chunk = rows.chunk(start / STANDARD_VECTOR_SIZE)?.clone();
                if count < chunk.len() {
                    chunk.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
                }
                live_ids.extend((0..count).map(|row| file_id(first + row) as i64));
                kept.resize(kept.len() + count, true);
                (chunk.clone(), chunk)
            } else {
                let nulls: Vec<Value> = types.iter().map(|ty| Value::null(ty.clone())).collect();
                let mut chunk = DataChunk::new(&types);
                let mut live = DataChunk::new(&types);
                for offset in first..first + count {
                    let Some(position) = position_of(offset) else {
                        chunk.append_row(&nulls)?;
                        kept.push(false);
                        continue;
                    };
                    let source = rows.chunk(position / STANDARD_VECTOR_SIZE)?;
                    let row = position % STANDARD_VECTOR_SIZE;
                    let values: Vec<Value> = (0..types.len()).map(|c| source.column(c).get_value(row)).collect();
                    chunk.append_row(&values)?;
                    live.append_row(&values)?;
                    live_ids.push(file_id(offset) as i64);
                    kept.push(true);
                }
                (chunk, live)
            };
            for index in indexes.iter_mut() {
                index.add(&live, |row| live_ids[row])?;
            }
            for (column, parts) in parts.iter_mut().enumerate() {
                parts.push(Part::new(chunk.column(column), count));
            }
            if stored.is_some() {
                live_chunks.push(live);
            }
        }
        // `RowVersionManager::Checkpoint`: deletes the row group of the file had, the same, stay where they are
        let deletes = match stored {
            Some((disk, stored_group)) if same_deletes(disk, stored_group, &kept) => {
                Deletes::Keep(disk.stored_group(stored_group)?.deletes_pointers)
            }
            _ => Deletes::Write((kept.iter().any(|kept| !kept)).then(|| deletes_of(&kept))),
        };
        // `RowGroup::WriteToDisk`: what stays of the row group as the last checkpoint wrote it
        let mut reused = vec![None; types.len()];
        let mut kept_any = false;
        let mut whole = false;
        if let Some((disk, stored_group)) = stored.filter(|_| self.metadata_reuse) {
            let stored_columns = table.checkpointed_columns();
            let (same_deletes, same) =
                matching_columns(disk, stored_group, &kept, &live_chunks, &stored_columns, &types)?;
            let kept_group = disk.stored_group(stored_group)?;
            let identity = disk.column_count() == types.len()
                && stored_columns
                    .iter()
                    .enumerate()
                    .all(|(at, column)| *column == Some(at));
            // `HasChanges`: the same rows deleted, and the others with the same values (REUSE_EXISTING_ROW_GROUP_METADATA),
            // unless the file says which blocks each column takes and this can't
            let unchanged = identity
                && same_deletes
                && same.iter().all(Option::is_some)
                && (self.per_column_writes || !kept_group.per_column);
            // PARTIALLY_REUSE_COLUMN_METADATA: the columns with the same values keep their metadata
            let partly =
                !unchanged && self.per_column_writes && kept_group.per_column && same.iter().any(Option::is_some);
            if unchanged || partly {
                reused = self.keep_columns(stored_group, &kept_group, &same, &types, statistics)?;
                kept_any = true;
            }
            whole = unchanged && matches!(deletes, Deletes::Keep(_));
        }
        if !kept_any {
            self.group_loaded.push(vec![true; types.len()]);
        }
        let written = self.write_row_group(
            table,
            parts,
            group.file_start,
            group.rows,
            deletes,
            statistics,
            reused,
            None,
        )?;
        Ok((written, group.file_start as usize + group.rows, whole))
    }

    /// `SingleFileTableDataWriter::FinalizeTable`: where the rows of a table are, how many rows its
    /// row groups have, and the id of the row after them.
    fn write_table_data(
        &mut self,
        table: &TableCatalogEntry,
        indexes: &mut [TableIndex],
    ) -> Result<(MetaBlockPointer, u64, u64)> {
        let types = table.types();
        let columns: Vec<usize> = (0..types.len()).collect();
        let total = table.row_count();
        let ids = table.row_ids(total);
        let cursor = table
            .source()
            .map(|source| TableCursor::new(source, columns.clone(), types.clone(), table.row_group_size()));
        let memory = if cursor.is_none() { Some(table.rows()?) } else { None };
        // what was loaded of the file before the rows are read for this
        let checkpointed = table.checkpointed();
        let disk = checkpointed
            .as_ref()
            .and_then(|source| source.as_any().downcast_ref::<super::table_data::DiskTable>())
            .filter(|disk| disk.is_in(self.buffer));
        self.stored_loaded = disk
            .map(super::table_data::DiskTable::loaded_columns)
            .unwrap_or_default();
        self.group_loaded.clear();
        // the rows are still those of the file, as a layer over its table or that table itself
        // (that a DELETE took rows of too)
        // of rows appended after those of the file, those the file has (`RowGroupCollection::MergeStorage`)
        // also under a DELETE, which leaves the columns as they are
        let appended = table.source().and_then(|source| {
            if let Some(appended) = source.as_any().downcast_ref::<super::appended::AppendedTable>() {
                return Some(appended.clone());
            }
            let layered = source.as_any().downcast_ref::<super::layered::LayeredTable>()?;
            let identity = layered
                .layers()
                .iter()
                .enumerate()
                .all(|(at, layer)| matches!(layer, super::layered::Layer::Base(column) if *column == at));
            identity
                .then(|| {
                    layered
                        .base()
                        .as_any()
                        .downcast_ref::<super::appended::AppendedTable>()
                        .cloned()
                })
                .flatten()
        });
        self.written = appended
            .as_ref()
            .map(|appended| appended.written_groups().into_iter().map(|(_, group)| group).collect())
            .unwrap_or_default();
        let in_file_source = match &appended {
            Some(appended) => appended.base().cloned(),
            None => table.source(),
        };
        self.in_file = match (in_file_source, disk) {
            (Some(source), Some(disk)) if self.metadata_reuse => {
                let layered = super::layered::LayeredTable::of(&source, types.clone());
                let left = crate::catalog::TableSource::row_ids(&layered);
                // its rows are the first of the table, with the same ids
                let prefix = appended.is_some() && ids_are_prefix(&left, &ids);
                let same = std::ptr::addr_eq(Arc::as_ptr(layered.base()), disk as *const _)
                    && (prefix || (left.next() == ids.next() && left.ranges().eq(ids.ranges())));
                same.then_some(layered)
            }
            _ => None,
        };
        let mut rows = PositionedRows {
            cursor,
            memory,
            at: None,
        };
        let mut statistics: Vec<Option<Stats>> = vec![None; types.len()];
        // `ColumnStatistics::distinct_stats`, which only files of v1.2.0 on keep (`HLL_V2`)
        let mut distinct = (self.version >= StorageVersion::V1_2_0)
            .then(|| table.distinct_states())
            .flatten()
            .unwrap_or_default();
        // `InitializeAddColumn`, `InitializeAlterType`: a column without them starts empty and counts the values the
        // row groups are written with
        self.counted_distinct = (0..types.len())
            .map(|column| {
                (self.version >= StorageVersion::V1_2_0
                    && !distinct.is_empty()
                    && distinct.get(column).is_some_and(Option::is_none)
                    && type_is_supported(&types[column]))
                .then(DistinctState::default)
            })
            .collect();
        let mut row_groups = Vec::new();
        let size = self.chunks_per_row_group * STANDARD_VECTOR_SIZE;
        let (mut stored_rows, mut next_row_id) = (0u64, 0u64);
        let dense = self.version < StorageVersion::V2_0_0;
        // `GetVacuumIndexStrategy`: KEEP_ROW_IDS where indexes can't be remapped (`CanVacuumRemap`), which needs
        // gaps a file can say and no keys of GEOMETRYs as before v1.5.0
        // `CanRebuildExistingIndexesAfterVacuum`: REBUILD before KEEP_ROW_IDS, for tables small enough
        let rebuild = self.vacuum_rebuild_indexes > 0 && ids.next() <= self.vacuum_rebuild_indexes;
        let keep_ids =
            !rebuild && ((dense && !indexes.is_empty()) || indexes.iter().any(|index| index.legacy_geometry));
        // `storage.total_vacuum_time`: what writing the row groups that came together took
        let mut vacuum_time = std::time::Duration::ZERO;
        // `WriteUnchangedTable`: of a table whose row groups all stay as they were, the statistics and the list of
        // row groups stay too
        let existing = disk
            .filter(|_| self.metadata_reuse)
            .map(|disk| {
                let (pointer, blocks) = disk.table_data();
                (pointer, blocks.to_vec(), crate::catalog::TableSource::group_count(disk))
            })
            .filter(|(pointer, _, _)| pointer.is_valid());
        let mut all_whole = existing.is_some();
        for group in plan_groups(&ids, size, self.vacuum_tasks, dense, keep_ids) {
            let start = std::time::Instant::now();
            let (written, end, whole) = self.write_group(table, &group, &ids, &mut rows, indexes, &mut statistics)?;
            all_whole &= whole;
            if group.merged {
                vacuum_time += start.elapsed();
            }
            row_groups.push(written);
            stored_rows += group.rows as u64;
            next_row_id = next_row_id.max(end as u64);
        }
        crate::execution::profiler::add_timing("storage.total_vacuum_time", vacuum_time);
        self.in_file = None;
        let loaded = std::mem::take(&mut self.group_loaded);
        self.loaded.insert((table.schema.clone(), table.name.clone()), loaded);
        if self.version < StorageVersion::V2_0_0 {
            let columns = (0..types.len())
                .map(|column| {
                    let stats = statistics[column].clone().unwrap_or_else(Stats::unknown);
                    let mut s = serializer(StorageVersion::V2_0_0);
                    true.write_value(&mut s);
                    s.write_object_value(|s| {
                        s.write_object(100, "statistics", |s| write_stats(s, Some(&types[column]), &stats));
                    });
                    s.into_bytes()
                })
                .collect();
            self.memory_statistics
                .insert((table.schema.clone(), table.name.clone()), columns);
        }
        for (state, counted) in distinct.iter_mut().zip(std::mem::take(&mut self.counted_distinct)) {
            if state.is_none() {
                *state = counted;
            }
        }
        let mut s = serializer(self.version);
        s.write_object_value(|s| {
            s.write_list(100, "column_stats", types.len(), |s, column| {
                true.write_value(s);
                let stats = statistics[column].clone().unwrap_or_else(Stats::unknown);
                s.write_object_value(|s| {
                    s.write_object(100, "statistics", |s| write_stats(s, Some(&types[column]), &stats));
                    if let Some(Some(state)) = distinct.get(column) {
                        s.write_nullable_object(101, "distinct", |s| state.write(s));
                    }
                });
            });
        });
        if let Some((pointer, blocks, groups)) = existing
            && all_whole
            && groups == row_groups.len()
        {
            self.keep_sub_blocks(&blocks)?;
            return Ok((pointer, stored_rows, next_row_id));
        }
        let mut tables = std::mem::take(&mut self.tables);
        let pointer = self.pointer(&mut tables);
        self.write_metadata(&mut tables, &s.into_bytes());
        self.write_metadata(&mut tables, &(row_groups.len() as u64).to_le_bytes());
        for row_group in &row_groups {
            self.write_metadata(&mut tables, row_group);
        }
        self.tables = tables;
        Ok((pointer, stored_rows, next_row_id))
    }

    /// The entries of a schema, in an order in which each can be read: tables can use macros and
    /// sequences in their defaults, and C++ doesn't bind macros or views when it reads them.
    fn write_schema(&mut self, schema: &Arc<SchemaCatalogEntry>, entries: &mut Vec<Vec<u8>>) -> Result<()> {
        let version = self.version;
        let simple = |entry_type: u64, info: g::CreateInfo| {
            let mut s = serializer(version);
            s.write_object_value(|s| {
                s.write_property(99, "type", &entry_type);
                s.write_property(100, "info", &Some(Box::new(info)));
            });
            s.into_bytes()
        };
        let name = self.name.clone();
        entries.push(simple(catalog_type::SCHEMA_ENTRY, schema_info(&name, &schema.path)));
        for (type_name, logical_type) in schema.type_entries() {
            let info = type_info(&name, schema, type_name, logical_type);
            entries.push(simple(catalog_type::TYPE_ENTRY, info));
        }
        for sequence in schema.sequence_entries() {
            entries.push(simple(
                catalog_type::SEQUENCE_ENTRY,
                sequence_info(&name, schema, &sequence),
            ));
        }
        for table_macro in [false, true] {
            for entry in schema.macro_entries(table_macro) {
                let info = macro_info(&name, schema, &entry, self.catalog)?;
                entries.push(simple(info.type_, info));
            }
        }
        let mut views = Vec::new();
        let mut tables = Vec::new();
        for entry in schema.tables_and_views() {
            match entry {
                crate::catalog::TableOrView::View(view) => views.push(view),
                crate::catalog::TableOrView::Table(table) => tables.push(table),
            }
        }
        for position in parents_first(&tables) {
            let table = &tables[position];
            let info = table_info(&name, schema, table)?;
            let mut indexes = TableIndex::all_of(table, self.block_size, self.version);
            let (pointer, rows, next_row_id) = self.write_table_data(table, &mut indexes)?;
            let mut storage = Vec::with_capacity(indexes.len());
            for index in &mut indexes {
                storage.push(index.storage_info(|data| {
                    let (block_id, offset) = self.place(data)?;
                    Ok(g::BlockPointer { block_id, offset })
                })?);
            }
            let mut s = serializer(self.version);
            s.write_object_value(|s| {
                s.write_property(99, "type", &catalog_type::TABLE_ENTRY);
                s.write_property(100, "info", &Some(Box::new(info)));
                s.write_property(101, "table_pointer", &pointer);
                s.write_property(102, "total_rows", &rows);
                s.write_property(103, "index_pointers", &Vec::<g::BlockPointer>::new());
                s.write_property(104, "index_storage_infos", &storage);
                if s.should_serialize(StorageVersion::V2_0_0) {
                    s.write_property(105, "next_row_id", &next_row_id);
                }
            });
            entries.push(s.into_bytes());
        }
        for view in views {
            entries.push(simple(catalog_type::VIEW_ENTRY, view_info(&name, schema, &view)?));
        }
        for table in &tables {
            for index in table.indexes.read().iter() {
                let info = index_info(&name, schema, table, index)?;
                entries.push(simple(catalog_type::INDEX_ENTRY, info));
            }
        }
        // after the tables they are on and read
        for trigger in schema.trigger_entries() {
            entries.push(simple(
                catalog_type::TRIGGER_ENTRY,
                trigger_info(&name, schema, &trigger)?,
            ));
        }
        Ok(())
    }

    /// `WriteHeader`: the free list, the metadata, and then the header that makes it all count.
    fn finish(mut self, root: MetaBlockPointer) -> Result<()> {
        self.flush_open_block()?;
        // in the order of `multi_use_blocks`, a map
        let mut multi_use: Vec<(u64, u32)> = self
            .segments_in_block
            .iter()
            .filter(|(_, segments)| **segments > 1)
            .map(|(&block, &segments)| (block, segments))
            .collect();
        multi_use.sort_unstable();
        // `GetFreeListBlocks`: the room for the free list is taken before it is known, as sub-blocks taken then
        // are no longer free, until what it would take is less
        let payload = metadata_block_size(self.block_size) - SUB_BLOCK_HEADER_SIZE;
        let free_count = |writer: &Self| {
            (0..writer.next_block)
                .filter(|block| !writer.used.contains(block) && !writer.meta.contains_key(block))
                .count()
        };
        let mut sub_blocks = Vec::new();
        let mut allocated = 0;
        loop {
            let total = 8 + 8 * free_count(&self) + 8 + 12 * multi_use.len() + 8 + 16 * self.meta.len();
            if total < allocated {
                break;
            }
            sub_blocks.push(self.next_sub_block());
            allocated += payload;
        }
        // `MarkBlocksAsModified`: what the last checkpoint took and this one doesn't keep is free; a metadata block
        // all free is a free block
        for (block, taken) in std::mem::take(&mut self.modified) {
            if let Some(state) = self.meta.get_mut(&block) {
                let free = state.free | taken;
                if free == u64::MAX {
                    self.meta.remove(&block);
                } else {
                    state.free = free;
                }
            }
        }
        let free: Vec<u64> = (0..self.next_block)
            .filter(|block| !self.used.contains(block) && !self.meta.contains_key(block))
            .collect();
        let mut list = Vec::new();
        list.extend_from_slice(&(free.len() as u64).to_le_bytes());
        for block in &free {
            list.extend_from_slice(&block.to_le_bytes());
        }
        list.extend_from_slice(&(multi_use.len() as u64).to_le_bytes());
        for (block, segments) in &multi_use {
            list.extend_from_slice(&block.to_le_bytes());
            list.extend_from_slice(&segments.to_le_bytes());
        }
        list.extend_from_slice(&(self.meta.len() as u64).to_le_bytes());
        for (block, state) in &self.meta {
            list.extend_from_slice(&block.to_le_bytes());
            list.extend_from_slice(&state.free.to_le_bytes());
        }
        self.reserved = sub_blocks.into();
        let mut stream = MetaStream::default();
        let free_list = self.pointer(&mut stream).block_pointer;
        self.write_metadata(&mut stream, &list);
        // `MetadataManager::Flush`: the blocks written to, past their sub-blocks zeros
        let blocks = self.buffer.block_manager();
        let end = metadata_block_size(self.block_size) * METADATA_BLOCK_COUNT;
        for (block, state) in self.meta.iter_mut().filter(|(_, state)| state.dirty) {
            let data = state.data.as_mut().expect("written");
            data[end..].fill(0);
            blocks.write_block(*block, data)?;
        }
        blocks.set_metadata_blocks(self.meta.iter().map(|(block, state)| (*block, state.free)).collect());
        const FLAG: &str = "because of PRAGMA checkpoint_abort flag";
        check_abort(
            &self.abort,
            "after_free_list_write",
            &format!("Checkpoint aborted after free list write {FLAG}"),
        )?;
        check_abort(
            &self.abort,
            "before_header",
            &format!("Checkpoint aborted before header write {FLAG}"),
        )?;
        check_abort(
            &self.abort,
            "before_header_non_fatal",
            &format!("Checkpoint aborted before header write (non-fatal) {FLAG}"),
        )?;
        blocks.write_header(DatabaseHeader {
            meta_block: root.block_pointer,
            free_list,
            block_count: self.next_block,
            storage_compatibility: self.version.database_header_number(),
            ..blocks.header()
        })?;
        blocks.set_free_list(free.into_iter().collect(), multi_use.into_iter().collect());
        for point in ["before_truncate", "before_wal_finish"] {
            check_abort(
                &self.abort,
                point,
                &format!("Checkpoint aborted before truncate {FLAG}"),
            )?;
        }
        Ok(())
    }
}

pub(crate) fn parse_expression(text: &str) -> Result<Expr> {
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
/// `catalog`, and leaves the tables of the catalog in it. `mark` gets the root of the checkpoint
/// before any of it is written, for the log to say what a crash can be told by.
/// A serializer of what goes into a file in `version`.
fn serializer(version: StorageVersion) -> BinarySerializer {
    BinarySerializer::new(SerializationOptions {
        target: version,
        ..Default::default()
    })
}

/// A row group a large insert wrote to blocks ahead of its commit (`OptimisticDataWriter`): where its columns are,
/// which a checkpoint keeps, writing only their metadata.
pub struct WrittenGroup {
    /// the id of its first row
    pub row_start: u64,
    pub rows: usize,
    columns: Vec<ColumnOut>,
    /// the columns as a reader of the file sees them, to read them back from their blocks
    persistent: Vec<super::table_data::PersistentColumn>,
    types: Vec<LogicalType>,
    buffer: std::sync::Arc<BufferManager>,
    /// the `DistinctStatistics` of each column, if it was counted as its rows were appended; empty if not
    distinct: Vec<Option<DistinctState>>,
}

impl std::fmt::Debug for WrittenGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrittenGroup")
            .field("row_start", &self.row_start)
            .field("rows", &self.rows)
            .finish()
    }
}

impl WrittenGroup {
    /// The `DistinctStatistics` of its columns, if they were counted as its rows were appended.
    pub fn distinct(&self) -> Option<Vec<Option<DistinctState>>> {
        (self.distinct.len() == self.types.len()).then(|| self.distinct.clone())
    }

    fn new(
        row_start: u64,
        rows: usize,
        columns: Vec<ColumnOut>,
        types: Vec<LogicalType>,
        version: StorageVersion,
        buffer: std::sync::Arc<BufferManager>,
    ) -> Result<Self> {
        let mut persistent = Vec::with_capacity(columns.len());
        for (column, ty) in columns.iter().zip(&types) {
            let mut s = serializer(version);
            s.write_object_value(|s| write_column(s, column, Some(ty), 0));
            let bytes = s.into_bytes();
            let mut d = duckrs_common::serializer::BinaryDeserializer::new(&bytes);
            persistent.push(super::table_data::read_persistent_column(&mut d, Some(ty))?);
        }
        Ok(Self {
            row_start,
            rows,
            columns,
            persistent,
            types,
            buffer,
            distinct: Vec::new(),
        })
    }

    /// A row group a log of C++ says is in blocks (`ROW_GROUP_DATA`); `None` where its columns are of a kind the
    /// writer doesn't write as they are (a GEOMETRY of the spatial extension's format).
    pub(super) fn from_persistent(
        row_start: u64,
        rows: usize,
        columns: &[super::table_data::PersistentColumn],
        types: Vec<LogicalType>,
        version: StorageVersion,
        buffer: std::sync::Arc<BufferManager>,
    ) -> Result<Option<Self>> {
        fn out_of(column: &super::table_data::PersistentColumn, ty: Option<&LogicalType>) -> Option<ColumnOut> {
            if column.spatial {
                return None;
            }
            let segments = column
                .pointers
                .iter()
                .map(|pointer| SegmentOut {
                    tuple_count: pointer.tuple_count,
                    block_id: pointer.block_id,
                    offset: pointer.offset as u32,
                    compression: pointer.compression,
                    stats: segment_stats(&pointer.statistics),
                    overflow_blocks: pointer.overflow_blocks.clone(),
                })
                .collect();
            let validity = match &column.validity {
                Some(validity) => Some(Box::new(out_of(validity, None)?)),
                None => None,
            };
            let child_types = match ty {
                Some(ty) if ty.id() == LogicalTypeId::Variant => {
                    let mut types = vec![duckrs_common::variant::unshredded_type()];
                    types.extend(column.shredded_type.clone());
                    types
                }
                Some(ty) => child_types(ty),
                None => Vec::new(),
            };
            if column.children.len() != child_types.len() {
                return None;
            }
            let children = column
                .children
                .iter()
                .zip(&child_types)
                .map(|(child, ty)| out_of(child, Some(ty)))
                .collect::<Option<Vec<_>>>()?;
            let stats = match ty {
                Some(ty) => stored_stats(column, ty),
                None => {
                    let mut stats = Stats::of_validity(&[]);
                    for pointer in &column.pointers {
                        stats.has_null |= pointer.statistics.has_null;
                        stats.has_no_null |= pointer.statistics.has_no_null;
                    }
                    stats
                }
            };
            Some(ColumnOut {
                segments,
                validity,
                children,
                stats,
                shredded_type: column.shredded_type.clone(),
            })
        }
        let mut outs = Vec::with_capacity(columns.len());
        for (column, ty) in columns.iter().zip(&types) {
            match out_of(column, Some(ty)) {
                Some(out) => outs.push(out),
                None => return Ok(None),
            }
        }
        Self::new(row_start, rows, outs, types, version, buffer).map(Some)
    }

    /// A column of the group, read from its blocks.
    pub fn read_column(&self, column: usize) -> Result<Vector> {
        super::table_data::scan_table_column(&self.buffer, &self.persistent[column], &self.types[column], self.rows)
    }

    pub fn buffer(&self) -> std::sync::Arc<BufferManager> {
        self.buffer.clone()
    }

    /// `pragma_storage_info` of its columns, as row group `group`.
    pub(super) fn segments(
        &self,
        buffer: Option<&BufferManager>,
        group: u64,
    ) -> Result<Vec<crate::catalog::SegmentInfo>> {
        let mut result = Vec::new();
        for (column, ty) in self.types.iter().enumerate() {
            super::table_data::column_segments(
                buffer,
                &mut result,
                &self.persistent[column],
                group,
                column,
                &[column],
                Some(ty),
            )?;
        }
        for segment in &mut result {
            if segment.block_id >= 0 {
                segment.block_id = self.buffer.resolve(segment.block_id as u64) as i64;
            }
        }
        Ok(result)
    }

    /// What its columns say of their values, per column (`ColumnZone`).
    pub(super) fn zone(&self, column: usize) -> Result<crate::catalog::ColumnZone> {
        super::table_data::column_zone(&self.persistent[column], &self.types[column])
    }
}

/// `WRITE_BUFFER_ROW_GROUP_COUNT`: the row groups written together, a column of all of them at a time.
pub const WRITE_BUFFER_ROW_GROUP_COUNT: usize = 5;

/// `OptimisticDataWriter::WriteNewRowGroup` and `WriteUnflushedRowGroups` of the rows of `table` from position
/// `first` on, the first of which has id `first_id`: in row groups of the table's size, `WRITE_BUFFER_ROW_GROUP_COUNT`
/// at a time, into blocks that get their ids as they fill (`PartialBlockType::APPEND_TO_TABLE`).
pub fn write_ahead(
    buffer: &std::sync::Arc<BufferManager>,
    catalog: &Catalog,
    table: &TableCatalogEntry,
    rows: &ColumnDataCollection,
    first: usize,
    first_id: u64,
    options: CheckpointOptions,
    blocks: &mut AppendBlocks,
    final_flush: bool,
) -> Result<Vec<WrittenGroup>> {
    let encoded = encode_ahead(buffer, catalog, table, rows, first, options);
    place_ahead(buffer, catalog, table, encoded, first_id, options, blocks, final_flush)
}

/// The row groups of [`write_ahead`] encoded, with what they place held back, for [`place_ahead`] to place: which
/// needs no lock, so that threads encode row groups at once and place them in their order after.
#[derive(Debug)]
pub struct EncodedAhead {
    first: usize,
    batches: Vec<EncodedBatch>,
}

/// `WRITE_BUFFER_ROW_GROUP_COUNT` row groups of an [`EncodedAhead`]: their rows, the parts of each of their columns,
/// and of each column of each (`tasks`), what encoding it on a thread of its own gave.
#[derive(Debug)]
struct EncodedBatch {
    bounds: Vec<(usize, usize)>,
    parts: Vec<Vec<Vec<Part>>>,
    tasks: Vec<(usize, usize)>,
    deferred: Vec<Option<(ColumnOut, Deferred)>>,
    /// per row group, the `DistinctStatistics` of each column, as appending its rows updates them in C++
    distinct: Vec<Vec<Option<DistinctState>>>,
}

/// The encoding of [`write_ahead`], each column of each row group on a thread of its own where there are threads.
pub fn encode_ahead(
    buffer: &BufferManager,
    catalog: &Catalog,
    table: &TableCatalogEntry,
    rows: &ColumnDataCollection,
    first: usize,
    options: CheckpointOptions,
) -> EncodedAhead {
    let size = table.row_group_size();
    let total = rows.len();
    let types = table.types();
    // the parts of each column of the row group of rows [start, end)
    let parts_of = |start: usize, end: usize| -> Vec<Vec<Part>> {
        let mut parts: Vec<Vec<Part>> = types.iter().map(|_| Vec::new()).collect();
        let mut chunk_index = start / STANDARD_VECTOR_SIZE;
        while chunk_index * STANDARD_VECTOR_SIZE < end {
            let chunk = rows.chunk(chunk_index);
            let chunk_start = chunk_index * STANDARD_VECTOR_SIZE;
            let from = start.saturating_sub(chunk_start);
            let to = (end - chunk_start).min(chunk.len());
            let mut piece = chunk.clone();
            if from > 0 || to < chunk.len() {
                piece.slice(
                    &SelectionVector::from_vec((from as u32..to as u32).collect()),
                    to - from,
                );
            }
            for (column, parts) in parts.iter_mut().enumerate() {
                parts.push(Part::new(piece.column(column), to - from));
            }
            chunk_index += 1;
        }
        parts
    };
    let bounds: Vec<(usize, usize)> = (first..total)
        .step_by(size)
        .map(|start| (start, (start + size).min(total)))
        .collect();
    let batches = bounds
        .chunks(WRITE_BUFFER_ROW_GROUP_COUNT)
        .map(|batch| {
            let parts: Vec<Vec<Vec<Part>>> = batch.iter().map(|&(start, end)| parts_of(start, end)).collect();
            // `RowGroup::WriteToDisk`: a column of all the row groups, then the next
            let tasks: Vec<(usize, usize)> = (0..types.len())
                .flat_map(|column| (0..batch.len()).map(move |group| (column, group)))
                .collect();
            let deferred = encode_deferred(buffer, catalog, table, options, &parts, &tasks);
            let distinct = parts.iter().map(|group| parts_distinct(group, &types)).collect();
            EncodedBatch {
                bounds: batch.to_vec(),
                parts,
                tasks,
                deferred,
                distinct,
            }
        })
        .collect();
    EncodedAhead { first, batches }
}

/// The `DistinctStatistics` of each column of a row group, from its parts.
fn parts_distinct(parts: &[Vec<Part>], types: &[LogicalType]) -> Vec<Option<DistinctState>> {
    parts
        .iter()
        .zip(types)
        .map(|(parts, ty)| {
            if !type_is_supported(ty) {
                return None;
            }
            let mut state = DistinctState::default();
            parts
                .iter()
                .all(|part| state.update(ty, &part.vector, part.count))
                .then_some(state)
        })
        .collect()
}

/// The placing of [`write_ahead`]: the columns of `encoded` placed in their order, those that couldn't be encoded
/// apart encoded here. The rows have ids from `first_id` on.
#[allow(clippy::too_many_arguments)] // those of `write_ahead`
pub fn place_ahead(
    buffer: &std::sync::Arc<BufferManager>,
    catalog: &Catalog,
    table: &TableCatalogEntry,
    encoded: EncodedAhead,
    first_id: u64,
    options: CheckpointOptions,
    blocks: &mut AppendBlocks,
    final_flush: bool,
) -> Result<Vec<WrittenGroup>> {
    let mut writer = Writer::new(buffer, catalog, String::new(), options);
    writer.append = true;
    writer.partial = std::mem::take(&mut blocks.partial);
    writer.partial_count = blocks.count;
    let types = table.types();
    let first = encoded.first;
    let mut groups = Vec::new();
    for mut batch in encoded.batches {
        let mut columns: Vec<Vec<ColumnOut>> = batch.bounds.iter().map(|_| Vec::with_capacity(types.len())).collect();
        for (index, &(column, group)) in batch.tasks.iter().enumerate() {
            let out = match batch.deferred.get_mut(index).and_then(Option::take) {
                Some((out, held)) if !held.failed => writer.place_deferred(out, held)?,
                _ => writer.encode_column(table, column, &batch.parts[group][column])?,
            };
            columns[group].push(out);
        }
        for ((&(start, end), columns), distinct) in batch.bounds.iter().zip(columns).zip(batch.distinct) {
            groups.push((first_id + (start - first) as u64, end - start, columns, distinct));
        }
    }
    // `FinalFlush` at the commit; until then the blocks that aren't full stay open for the next row groups
    if final_flush {
        writer.flush_open_block()?;
    }
    blocks.partial = std::mem::take(&mut writer.partial);
    blocks.count = writer.partial_count;
    let version = writer.version;
    groups
        .into_iter()
        .map(|(row_start, rows, columns, distinct)| {
            let mut group = WrittenGroup::new(row_start, rows, columns, types.clone(), version, buffer.clone())?;
            group.distinct = distinct;
            Ok(group)
        })
        .collect()
}

/// The columns `tasks` (a column and a row group of `parts` each) encoded on threads at once, each with what it
/// would place held back: `None` of one whose encoding failed. Empty if there is one thread, which encodes them on
/// the writer of the file.
fn encode_deferred(
    buffer: &BufferManager,
    catalog: &Catalog,
    table: &TableCatalogEntry,
    options: CheckpointOptions,
    parts: &[Vec<Vec<Part>>],
    tasks: &[(usize, usize)],
) -> Vec<Option<(ColumnOut, Deferred)>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = crate::execution::temporary::current().threads().min(tasks.len());
    if threads < 2 {
        return Vec::new();
    }
    let results: Vec<parking_lot::Mutex<Option<(ColumnOut, Deferred)>>> =
        tasks.iter().map(|_| parking_lot::Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut writer = Writer::new(buffer, catalog, String::new(), options);
                writer.append = true;
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(column, group)) = tasks.get(index) else {
                        break;
                    };
                    writer.deferred = Some(Deferred::default());
                    let out = writer.encode_column(table, column, &parts[group][column]);
                    let held = writer.deferred.take().expect("just set");
                    *results[index].lock() = out.ok().map(|out| (out, held));
                }
            });
        }
    });
    results.into_iter().map(parking_lot::Mutex::into_inner).collect()
}

/// `TableStatistics` of row groups written ahead of a checkpoint: the statistics of their columns merged, as a
/// checkpoint writes those of a table and a reader of the file sees them.
pub fn written_statistics(groups: &[std::sync::Arc<WrittenGroup>]) -> Option<Vec<crate::catalog::ColumnStatistics>> {
    let first = groups.first()?;
    let mut result = Vec::with_capacity(first.types.len());
    for (column, ty) in first.types.iter().enumerate() {
        let mut merged = first.columns[column].stats.clone();
        for group in &groups[1..] {
            merged.merge(&group.columns[column].stats, ty);
        }
        let mut s = serializer(StorageVersion::V2_1_0);
        true.write_value(&mut s);
        s.write_object_value(|s| s.write_object(100, "statistics", |s| write_stats(s, Some(ty), &merged)));
        let bytes = s.into_bytes();
        let mut d = duckrs_common::serializer::BinaryDeserializer::new(&bytes);
        let stored = super::statistics::read_column_statistics(&mut d, ty).ok()??;
        result.push(super::table_data::column_statistics(&stored, ty).ok()?);
    }
    Some(result)
}

/// A column with the blocks of the file its segments are in, those an append filled having ids by now.
fn resolved(column: &ColumnOut, buffer: &BufferManager) -> ColumnOut {
    let mut column = column.clone();
    fn patch(column: &mut ColumnOut, buffer: &BufferManager) {
        for segment in &mut column.segments {
            if segment.block_id >= 0 {
                segment.block_id = buffer.resolve(segment.block_id as u64) as i64;
            }
        }
        if let Some(validity) = &mut column.validity {
            patch(validity, buffer);
        }
        for child in &mut column.children {
            patch(child, buffer);
        }
    }
    patch(&mut column, buffer);
    column
}

/// `PersistentCollectionData::Serialize` of row groups written ahead of a commit, for a log (`WriteRowGroupData`).
pub fn write_row_group_data(s: &mut BinarySerializer, types: &[LogicalType], groups: &[std::sync::Arc<WrittenGroup>]) {
    s.write_object(101, "row_group_data", |s| {
        s.write_list(100, "row_groups", groups.len(), |s, at| {
            let group = &groups[at];
            s.write_object_value(|s| {
                s.write_property(100, "types", &types.to_vec());
                s.write_list(101, "columns", group.columns.len(), |s, column| {
                    let out = resolved(&group.columns[column], &group.buffer);
                    s.write_object_value(|s| write_column(s, &out, Some(&types[column]), 0));
                });
                s.write_property(102, "start", &group.row_start);
                s.write_property(103, "count", &(group.rows as u64));
            });
        });
    });
}

pub fn write_checkpoint(
    path: &Path,
    buffer: &Arc<BufferManager>,
    catalog: &Catalog,
    name: &str,
    options: CheckpointOptions,
    mark: &mut dyn FnMut(MetaBlockPointer) -> Result<()>,
) -> Result<()> {
    let mut writer = Writer::new(buffer, catalog, name.to_string(), options);
    writer.load_metadata_blocks()?;
    // `RewriteMainHeader`: a file of v0.10.2 that goes to v1.2.0 or newer says so in its main header
    if writer.version >= StorageVersion::V1_2_0 && buffer.block_manager().main_header().version_number == 64 {
        buffer.block_manager().rewrite_main_header(StorageVersion::V1_2_0)?;
    }
    let mut root = MetaStream::default();
    let pointer = writer.pointer(&mut root);
    mark(pointer)?;
    let mut entries = Vec::new();
    for schema in catalog.schemas() {
        if !schema.name.eq_ignore_ascii_case(TEMP_SCHEMA) {
            writer.write_schema(&schema, &mut entries)?;
        }
    }
    let mut s = serializer(writer.version);
    s.write_object_value(|s| {
        s.write_list(100, "catalog_entries", entries.len(), |s, i| s.write_raw(&entries[i]));
    });
    writer.write_metadata(&mut root, &s.into_bytes());
    let loaded = std::mem::take(&mut writer.loaded);
    let memory_statistics = std::mem::take(&mut writer.memory_statistics);
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
        if let StoredEntry::Table(mut table) = entry {
            let schema = table.schema.clone();
            let name = table.name.clone();
            // `RowGroupCollection::Checkpoint` keeps the statistics it had: those of the file are read on opening it
            if let Some(columns) = memory_statistics.get(&(schema.clone(), name.clone())) {
                let statistics = columns
                    .iter()
                    .zip(&table.columns)
                    .map(|(bytes, column)| {
                        let mut d = checkpoint::catalog_deserializer(bytes, false);
                        super::statistics::read_column_statistics(&mut d, &column.ty)
                    })
                    .collect::<Result<Vec<_>>>()?;
                table.column_statistics = statistics;
            }
            let disk = Arc::new(DiskTable::new(buffer.clone(), *table)?);
            if let Some(loaded) = loaded.get(&(schema.clone(), name.clone())) {
                disk.set_loaded(loaded);
            }
            let table = catalog.get_table(Some(&schema), &name)?;
            table.leave_in_file(disk);
            table.reset_log();
        }
    }
    Ok(())
}

fn encoded_size(segments: &[Encoded]) -> usize {
    segments.iter().map(|(bytes, _)| bytes.len()).sum()
}

/// `RLEFinalAnalyze`: a count and a value for each run; a NULL goes on with the run before it, and a
/// run ends at the most a count holds.
fn rle_score(data: &[u8], valid: &[bool], width: usize) -> usize {
    let mut seen_count = 0usize;
    let mut last: Option<&[u8]> = None;
    let mut last_seen = 0u32;
    for (value, valid) in data.chunks_exact(width).zip(valid) {
        if *valid {
            match last {
                None => {
                    last = Some(value);
                    seen_count += 1;
                    last_seen += 1;
                }
                Some(previous) if previous == value => last_seen += 1,
                Some(_) => {
                    if last_seen > 0 {
                        seen_count += 1;
                    }
                    last = Some(value);
                    last_seen = 1;
                }
            }
        } else {
            last_seen += 1;
        }
        if last_seen == u32::from(u16::MAX) {
            last_seen = 0;
            seen_count += 1;
        }
    }
    (2 + width) * seen_count
}

/// The name `force_compression` gives a method.
fn compression_setting_name(compression: u8) -> &'static str {
    match compression {
        COMPRESSION_UNCOMPRESSED => "uncompressed",
        COMPRESSION_RLE => "rle",
        COMPRESSION_BITPACKING => "bitpacking",
        COMPRESSION_ALP => "alp",
        COMPRESSION_ALPRD => "alprd",
        COMPRESSION_ROARING => "roaring",
        COMPRESSION_DICTIONARY => "dictionary",
        COMPRESSION_FSST => "fsst",
        COMPRESSION_ZSTD => "zstd",
        COMPRESSION_DICT_FSST => "dict_fsst",
        _ => "",
    }
}

/// `EnumUtil::ToString(CompressionType)`.
fn compression_enum_name(compression: u8) -> &'static str {
    match compression {
        COMPRESSION_UNCOMPRESSED => "UNCOMPRESSED",
        COMPRESSION_CONSTANT => "CONSTANT",
        COMPRESSION_RLE => "RLE",
        COMPRESSION_DICTIONARY => "DICTIONARY",
        COMPRESSION_BITPACKING => "BITPACKING",
        COMPRESSION_FSST => "FSST",
        COMPRESSION_ALP => "ALP",
        COMPRESSION_ALPRD => "ALPRD",
        COMPRESSION_ZSTD => "ZSTD",
        COMPRESSION_ROARING => "ROARING",
        COMPRESSION_EMPTY => "EMPTY",
        COMPRESSION_DICT_FSST => "DICT_FSST",
        _ => "AUTO",
    }
}
