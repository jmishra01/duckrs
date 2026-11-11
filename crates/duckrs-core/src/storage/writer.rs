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
    Encoded, encode_alp, encode_alprd, encode_dictionary, encode_old_dictionary, encode_old_fsst, encode_rle,
    encode_roaring, fill_nulls, is_signed, supports_bitpacking, supports_rle,
};
use super::fsst::SymbolTable;
use super::index_data::TableIndex;
use super::metadata::{MetaBlockPointer, metadata_block_size};
use super::serialization::generated::enums::{
    catalog_type, constraint_type, foreign_key_type, index_constraint_type, table_column_type,
};
use super::serialization::generated::{self as g};
use super::serialization::to_stored::{self, write_not_ported};
use super::table_data::{BITMASK, CONSTANT_INFO, DiskTable, VECTOR_INFO, fixed_width};
use crate::catalog::{
    Catalog, IndexCatalogEntry, MacroCatalogEntry, RowIds, SchemaCatalogEntry, SequenceCatalogEntry, TEMP_SCHEMA,
    TableCatalogEntry, TableConstraint, TableCursor, ViewCatalogEntry,
};
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
    /// `debug_force_bitpacking_mode`
    pub bitpacking_mode: &'a str,
    /// `zstd_min_string_length`
    pub zstd_min_string_length: usize,
    /// In rows: `ATTACH ... (ROW_GROUP_SIZE)` checks that it is whole vectors.
    pub row_group_size: u64,
    /// How many times row groups can come together, which gives their rows new ids. There is no
    /// such thing while a transaction goes by the ids it knows (`InitializeVacuumState`).
    pub vacuum_tasks: usize,
    /// `debug_checkpoint_abort`, in lower case: where to stop as a crash would.
    pub abort: &'a str,
    /// Transactions are reading tables from the blocks they are in: those stay as they are.
    pub keep_free_blocks: bool,
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
    /// of a GEOMETRY column or segment
    geometry: Option<duckrs_common::geometry::GeometryStats>,
}

impl Stats {
    fn unknown() -> Self {
        Self {
            has_null: true,
            has_no_null: true,
            bounds: None,
            geometry: None,
        }
    }

    fn of_validity(valid: &[bool]) -> Self {
        Self {
            has_null: valid.iter().any(|v| !v),
            has_no_null: valid.iter().any(|v| *v),
            bounds: None,
            geometry: None,
        }
    }

    fn merge(&mut self, other: &Self, physical: PhysicalType) {
        self.has_null |= other.has_null;
        self.has_no_null |= other.has_no_null;
        self.geometry = match (self.geometry, other.geometry) {
            (Some(mut a), Some(b)) => {
                a.merge(&b);
                Some(a)
            }
            (a, b) => a.or(b),
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
        // `VariantStats::Serialize`: not shredded, and the statistics of what it is stored as
        if ty.id() == LogicalTypeId::Variant {
            const NOT_SHREDDED: u8 = 1;
            s.write_property(200, "shredding_state", &NOT_SHREDDED);
            let unshredded = duckrs_common::variant::unshredded_type();
            s.write_object(225, "unshredded_stats", |s| {
                write_stats(s, Some(&unshredded), &Stats::unknown())
            });
            return;
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
        let child = |s: &mut BinarySerializer, ty: Option<&LogicalType>| {
            s.write_object_value(|s| write_stats(s, ty, &Stats::unknown()));
        };
        match ty.physical_type() {
            // `LegacyConstructMinMax` of no stats: the lowest min and the highest max
            PhysicalType::Varchar if !s.should_serialize(StorageVersion::V2_0_0) => {
                for (field, byte) in [(200, 0u8), (201, 0xFF)] {
                    s.write_field(field);
                    s.write_varint_unsigned(8);
                    s.write_raw(&[byte; 8]);
                }
                s.write_property(202, "has_unicode", &true);
                s.write_property(203, "has_max_string_length", &false);
                s.write_property(204, "max_string_length", &0u32);
            }
            // what `StringStatsField` says of no bounds, no lengths and that there can be unicode
            PhysicalType::Varchar => s.write_property(206, "packed_field", &0u32),
            // INTERVAL stats are only serialized in v2.0.0 and later
            PhysicalType::Interval if !s.should_serialize(StorageVersion::V2_0_0) => {}
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

/// `PersistentColumnData::Serialize`, without the object around it.
fn write_column(s: &mut BinarySerializer, column: &ColumnOut, ty: Option<&LogicalType>, row_start: u64) {
    // `GeometryPersistentColumnData`: stored as WKB, the default storage type
    if ty.is_some_and(|ty| ty.id() == LogicalTypeId::Geometry) {
        const GEOMETRY_EXTRA_DATA: u8 = 2;
        s.write_object(99, "extra_data", |s| {
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
    // `PersistentColumnData::Serialize` of a VARIANT: what it is stored as, unshredded
    if ty.id() == LogicalTypeId::Variant {
        let unshredded = duckrs_common::variant::unshredded_type();
        s.write_object(102, "unshredded", |s| {
            write_column(s, &column.children[0], Some(&unshredded), row_start)
        });
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
    let path = entry_path(catalog, schema, &name);
    let kind = g::CreateInfoKind::CreateTypeInfo(g::CreateTypeInfo { name, logical_type });
    to_stored::create_info(catalog_type::TYPE_ENTRY, path, kind)
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
    to_stored::create_info(catalog_type::SEQUENCE_ENTRY, path, kind)
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
    Ok(to_stored::create_info(
        entry_type,
        entry_path(catalog, schema, &entry.name),
        kind,
    ))
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
        column_comments_map: Vec::new(),
        security_type: None,
    });
    Ok(to_stored::create_info(
        catalog_type::VIEW_ENTRY,
        entry_path(catalog, schema, &view.name),
        kind,
    ))
}

pub(super) fn table_info(
    catalog: &str,
    schema: &SchemaCatalogEntry,
    table: &TableCatalogEntry,
) -> Result<g::CreateInfo> {
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
            TableConstraint::Unique { columns, primary_key } => constraints.push(boxed(g::Constraint {
                type_: constraint_type::UNIQUE,
                kind: g::ConstraintKind::UniqueConstraint(g::UniqueConstraint {
                    is_primary_key: *primary_key,
                    // `DConstants::INVALID_INDEX`: the columns are by name
                    index: u64::MAX,
                    columns: columns.iter().map(|c| table.columns[*c].name.clone()).collect(),
                    check_mode: None,
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
    Ok(to_stored::create_info(
        catalog_type::TABLE_ENTRY,
        entry_path(catalog, schema, &table.name),
        kind,
    ))
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
    /// The setting, in lower case.
    force_compression: String,
    bitpacking_mode: super::bitpacking::Mode,
    zstd_min_string_length: usize,
    /// the table and column being written, for the log of `DetectBestCompressionMethod`
    log_column: (String, usize),
    chunks_per_row_group: usize,
    vacuum_tasks: usize,
    abort: String,
    /// what the file is written in
    version: StorageVersion,
}

/// A row group that a table is written as. Its rows have the ids from `row_start` on, and those
/// with no row are deleted, as there are no gaps in a row group.
#[derive(Debug, PartialEq, Eq)]
struct PlannedGroup {
    row_start: u64,
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
fn vacuum_groups(groups: Vec<PlannedGroup>, size: usize, tasks: usize) -> Vec<PlannedGroup> {
    let mut result = Vec::with_capacity(groups.len());
    let mut at = 0;
    let mut merges = 0;
    while at < groups.len() {
        let mut planned = None;
        for target in (1..=MAX_MERGE_COUNT).filter(|_| merges < tasks) {
            let mut next = at;
            let mut rows = 0;
            while next < groups.len() && rows + groups[next].live <= target * size {
                rows += groups[next].live;
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
/// `dense`: the file can't say that row ids have gaps (`CanPersistRowIdGaps`, before v2.0.0), so row groups
/// without rows stay, and none come together.
fn plan_groups(ids: &RowIds, size: usize, vacuum_tasks: usize, dense: bool) -> Vec<PlannedGroup> {
    let vacuum_tasks = if dense { 0 } else { vacuum_tasks };
    let mut groups: Vec<PlannedGroup> = Vec::new();
    let mut add = |row_start: u64, rows: usize| {
        let first_position = ids.first_from(row_start);
        let live = ids.first_from(row_start + rows as u64) - first_position;
        if live > 0 || dense {
            groups.push(PlannedGroup {
                row_start,
                rows,
                first_position,
                live,
                merged: false,
            });
        }
    };
    let mut end = 0;
    for &(row_start, rows) in ids.stored_groups() {
        add(row_start, rows);
        end = row_start + rows as u64;
    }
    // ids that nothing has, in front of new rows, are of rows that went before a checkpoint
    while end < ids.next() {
        let rows = (ids.next() - end).min(size as u64);
        add(end, rows as usize);
        end += rows;
    }
    vacuum_groups(groups, size, vacuum_tasks)
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
        out.push(BITMASK);
        let mut words = [0u64; STANDARD_VECTOR_SIZE / 64];
        for (row, kept) in rows.iter().enumerate() {
            words[row / 64] |= u64::from(!kept) << (row % 64);
        }
        words.iter().for_each(|word| out.extend_from_slice(&word.to_le_bytes()));
    }
    out
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
            segments_in_block: HashMap::new(),
            open: None,
            pages: Vec::new(),
            taken: Vec::new(),
            next_sub_block: None,
            reserved: VecDeque::new(),
            tables: MetaStream::default(),
            force_compression: options.force_compression.to_lowercase(),
            bitpacking_mode: super::bitpacking::Mode::parse(options.bitpacking_mode),
            zstd_min_string_length: options.zstd_min_string_length,
            log_column: (String::new(), 0),
            chunks_per_row_group: options.row_group_size as usize / STANDARD_VECTOR_SIZE,
            vacuum_tasks: options.vacuum_tasks,
            abort: options.abort.to_string(),
            version: options.storage_version,
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

    /// `DUCKDB_LOG_TRACE` of `DetectBestCompressionMethod`: the method chosen for a column, and its score.
    fn log_choice(&self, compression: u8, ty: &str, score: usize) {
        let (table, column) = &self.log_column;
        crate::logging::log_database_trace(&format!(
            "ColumnDataCheckpointer FinalAnalyze({}) result for {table}.{column}({ty}): {score}",
            compression_enum_name(compression)
        ));
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
                self.buffer.block_manager().write_block(block, &pages[0].1)?;
                (block as i64, 0)
            };
            for (id, page) in pages.iter().skip(1) {
                self.buffer
                    .block_manager()
                    .write_block(id.expect("an extra page"), page)?;
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
        let force = self.force_compression.as_str();
        if self.version >= StorageVersion::V1_3_0 {
            match self.choose_string_compression(&strings, valid, limit) {
                COMPRESSION_ZSTD if !strings.is_empty() => return self.write_zstd(&strings, valid),
                COMPRESSION_DICT_FSST => {
                    if let Some(segments) = self.write_dict_fsst(&strings, valid, limit)? {
                        return Ok(segments);
                    }
                }
                _ => {}
            }
            return self.write_plain_strings(&strings, valid, limit);
        }
        // `ZSTDStorage::StringInitAnalyze`: from v1.2.0 on
        if force == "zstd" && self.version >= StorageVersion::V1_2_0 && !strings.is_empty() {
            return self.write_zstd(&strings, valid);
        }
        // before v1.3.0: DICTIONARY and FSST, which DICT_FSST replaced. Their analyze samples at random, so
        // the smallest of what they encode to is taken instead of their scores.
        if force != "uncompressed" && valid.iter().any(|v| *v) {
            let plain_size: usize = strings.iter().map(|string| string.len() + 4).sum();
            let total = |segments: &Vec<Encoded>| segments.iter().map(|(bytes, _)| bytes.len()).sum::<usize>();
            let dictionary = (force != "fsst")
                .then(|| encode_old_dictionary(&strings, valid, self.block_size, limit))
                .flatten()
                .map(|segments| (segments, COMPRESSION_DICTIONARY));
            let fsst = (force != "dictionary")
                .then(|| SymbolTable::train(&strings))
                .flatten()
                .and_then(|table| encode_old_fsst(&strings, valid, self.block_size, limit, &table))
                .map(|segments| (segments, COMPRESSION_FSST));
            let forced = matches!(force, "dictionary" | "fsst");
            let best = [dictionary, fsst]
                .into_iter()
                .flatten()
                .filter(|(segments, _)| forced || total(segments) < plain_size)
                .min_by_key(|(segments, _)| total(segments));
            if let Some((encoded, compression)) = best {
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
                return Ok(segments);
            }
        }
        self.write_plain_strings(&strings, valid, limit)
    }

    /// `ColumnDataCheckpointer::DetectBestCompressionMethod` of a VARCHAR column from v1.3.0 on:
    /// UNCOMPRESSED, ZSTD and DICT_FSST by their `final_analyze` scores (DICTIONARY and FSST don't analyze
    /// any more, so forcing either leaves UNCOMPRESSED).
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
        let mut methods = vec![COMPRESSION_UNCOMPRESSED, COMPRESSION_ZSTD, COMPRESSION_DICT_FSST];
        let forced = match force {
            "zstd" => Some(COMPRESSION_ZSTD),
            "dict_fsst" => Some(COMPRESSION_DICT_FSST),
            "uncompressed" | "dictionary" | "fsst" => Some(COMPRESSION_UNCOMPRESSED),
            _ => None,
        };
        if let Some(forced) = forced {
            methods.retain(|method| *method == forced || *method == COMPRESSION_UNCOMPRESSED);
        }
        let mut best: Option<(u8, usize)> = None;
        for method in methods {
            let score = match method {
                COMPRESSION_UNCOMPRESSED => Some(uncompressed),
                COMPRESSION_ZSTD => Some(zstd()),
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

    /// DICT_FSST segments, the dictionaries FSST encoded where that is smaller; `None` where the strings
    /// don't fit its segments.
    fn write_dict_fsst(&mut self, strings: &[&[u8]], valid: &[bool], limit: usize) -> Result<Option<Vec<SegmentOut>>> {
        let total = |segments: &Vec<Encoded>| segments.iter().map(|(bytes, _)| bytes.len()).sum::<usize>();
        let plain = encode_dictionary(strings, valid, self.block_size, limit, None);
        let table = valid.iter().any(|v| *v).then(|| SymbolTable::train(strings)).flatten();
        let compressed = table.and_then(|t| encode_dictionary(strings, valid, self.block_size, limit, Some(&t)));
        let Some(encoded) = [plain, compressed].into_iter().flatten().min_by_key(total) else {
            return Ok(None);
        };
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

    /// `DetectBestCompressionMethod` of a validity column: UNCOMPRESSED, and ROARING from v1.2.0.
    fn validity_choice(&self, valid: &[bool]) -> u8 {
        let force = self.force_compression.as_str();
        let plain_score = valid.len().div_ceil(8);
        let roaring_score = (self.version >= StorageVersion::V1_2_0 && force != "uncompressed")
            .then(|| super::roaring::final_analyze(valid, self.block_size));
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
        })
    }

    /// `ColumnData::Checkpoint`.
    fn write_column_data(&mut self, ty: &LogicalType, parts: &[Part]) -> Result<ColumnOut> {
        if ty.id() == LogicalTypeId::Variant {
            return self.write_variant(parts);
        }
        // before v1.5.0, a GEOMETRY is stored as the spatial extension's blob (`GetSpatialGeometryType`)
        if ty.id() == LogicalTypeId::Geometry && self.version < StorageVersion::V1_5_0 {
            return Err(Error::not_implemented(format!(
                "writing a GEOMETRY column to a database file of storage version {} is not yet ported to duckrs",
                self.version.display_name(false)
            )));
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
        let (segments, children) = match physical {
            PhysicalType::Varchar => (string_segments.take().expect("written above"), Vec::new()),
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

    /// `VariantColumnData::Checkpoint`, unshredded: its validity, and the STRUCT DuckDB stores its values as.
    fn write_variant(&mut self, parts: &[Part]) -> Result<ColumnOut> {
        let unshredded = duckrs_common::variant::unshredded_type();
        let mut valid = Vec::new();
        let mut stored = Vec::with_capacity(parts.len());
        for part in parts {
            let mut vector = Vector::new(unshredded.clone(), part.count.max(1));
            for row in 0..part.count {
                let value = part.vector.get_value(row);
                valid.push(!value.is_null());
                let node = match value.data() {
                    ValueData::Bytes(bytes) => duckrs_common::variant::VariantNode::decode(bytes)?,
                    _ => duckrs_common::variant::VariantNode::Null,
                };
                vector.set_value(row, &node.to_unshredded()?)?;
            }
            stored.push(vector);
        }
        let stored_parts: Vec<Part> = stored
            .iter()
            .zip(parts)
            .map(|(vector, part)| Part::new(vector, part.count))
            .collect();
        let child = self.write_column_data(&unshredded, &stored_parts)?;
        let validity = Box::new(self.write_validity(&valid)?);
        Ok(ColumnOut {
            segments: Vec::new(),
            validity: Some(validity),
            children: vec![child],
            stats: Stats::of_validity(&valid),
        })
    }

    /// `RowGroup::WriteToDisk`.
    fn write_row_group(
        &mut self,
        table: &TableCatalogEntry,
        parts: Vec<Vec<Part>>,
        row_start: u64,
        rows: usize,
        deletes: Option<&[u8]>,
        statistics: &mut [Option<Stats>],
    ) -> Result<Vec<u8>> {
        let mut tables = std::mem::take(&mut self.tables);
        let mut pointers = Vec::with_capacity(parts.len());
        for (column, parts) in parts.iter().enumerate() {
            let ty = &table.columns[column].ty;
            self.log_column = (format!("{}.{}", table.schema, table.name), column);
            let out = self.write_column_data(ty, parts)?;
            match &mut statistics[column] {
                Some(stats) => stats.merge(&out.stats, ty.physical_type()),
                empty => *empty = Some(out.stats.clone()),
            }
            let mut s = serializer(self.version);
            s.write_object_value(|s| write_column(s, &out, Some(ty), row_start));
            pointers.push(self.pointer(&mut tables));
            self.write_metadata(&mut tables, &s.into_bytes());
        }
        // one pointer per block that they take
        let size = metadata_block_size(self.block_size);
        let mut delete_pointers = Vec::new();
        let mut left = deletes.unwrap_or_default();
        while !left.is_empty() {
            delete_pointers.push(self.pointer(&mut tables));
            let (now, rest) = left.split_at(left.len().min(size - tables.offset));
            self.write_metadata(&mut tables, now);
            left = rest;
        }
        self.tables = tables;
        let mut s = serializer(self.version);
        s.write_object_value(|s| {
            s.write_property(100, "row_start", &row_start);
            s.write_property(101, "tuple_count", &(rows as u64));
            s.write_property(102, "data_pointers", &pointers);
            s.write_property(103, "delete_pointers", &delete_pointers);
            if s.should_serialize(StorageVersion::V1_4_0) {
                s.write_property(104, "has_metadata_blocks", &false);
            }
        });
        Ok(s.into_bytes())
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
    ) -> Result<(Vec<u8>, usize)> {
        let types = table.types();
        let mut parts: Vec<Vec<Part>> = types.iter().map(|_| Vec::new()).collect();
        let mut kept = Vec::with_capacity(group.rows);
        let row_id = |offset: usize| group.row_start + offset as u64;
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
                live_ids.extend((0..count).map(|row| row_id(first + row) as i64));
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
                    live_ids.push(row_id(offset) as i64);
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
        }
        let deletes = (kept.iter().any(|kept| !kept)).then(|| deletes_of(&kept));
        let written = self.write_row_group(
            table,
            parts,
            group.row_start,
            group.rows,
            deletes.as_deref(),
            statistics,
        )?;
        Ok((written, group.row_start as usize + group.rows))
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
        let mut rows = PositionedRows {
            cursor,
            memory,
            at: None,
        };
        let mut statistics: Vec<Option<Stats>> = vec![None; types.len()];
        let mut row_groups = Vec::new();
        let size = self.chunks_per_row_group * STANDARD_VECTOR_SIZE;
        let (mut stored_rows, mut next_row_id) = (0u64, 0u64);
        let dense = self.version < StorageVersion::V2_0_0;
        for group in plan_groups(&ids, size, self.vacuum_tasks, dense) {
            let (written, end) = self.write_group(table, &group, &ids, &mut rows, indexes, &mut statistics)?;
            row_groups.push(written);
            stored_rows += group.rows as u64;
            next_row_id = next_row_id.max(end as u64);
        }
        let mut s = serializer(self.version);
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
            let mut indexes = TableIndex::all_of(table, self.block_size);
            // `ART::GetSerializationFormat`: older versions have layouts of their own, which aren't written here
            if !indexes.is_empty() && self.version < StorageVersion::V2_0_0 {
                return Err(Error::not_implemented(format!(
                    "writing an index to a database file of storage version {} is not yet ported to duckrs",
                    self.version.display_name(false)
                )));
            }
            let (pointer, rows, next_row_id) = self.write_table_data(table, &mut indexes)?;
            let mut storage = Vec::with_capacity(indexes.len());
            for index in &indexes {
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

pub fn write_checkpoint(
    path: &Path,
    buffer: &Arc<BufferManager>,
    catalog: &Catalog,
    name: &str,
    options: CheckpointOptions,
    mark: &mut dyn FnMut(MetaBlockPointer) -> Result<()>,
) -> Result<()> {
    let mut writer = Writer::new(buffer, catalog, name.to_string(), options);
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
