//! Ported from duckdb/src/storage/statistics/*.cpp @ rust-port-baseline.
//!
//! Serialized properties have no length, so to get past statistics they have to be read. What the
//! scans need of them is whether a segment has NULLs and, for a constant segment, its value.

use super::block_manager::corrupt;
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::{Interval, LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::{Error, ErrorKind, Result};

/// `ReadProperty(field, tag, data, size)`: a length and bytes.
pub struct RawBytes(pub Vec<u8>);

impl Deserialize for RawBytes {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let size = d.read_varint_unsigned(64)? as usize;
        Ok(Self(d.read_raw(size)?.to_vec()))
    }
}

/// `BaseStatistics`.
#[derive(Debug, Clone, Default)]
pub struct BaseStatistics {
    pub has_null: bool,
    pub has_no_null: bool,
    /// The bounds of a numeric column as they are in a vector, in little-endian bytes.
    pub min: Option<[u8; 16]>,
    pub max: Option<[u8; 16]>,
    /// of a GEOMETRY that the file has the `GeometryStats` of
    pub geometry: Option<duckrs_common::geometry::GeometryStats>,
    /// of a VARCHAR, the bounds the file has
    pub strings: Option<StringBounds>,
}

fn numeric_value(d: &mut BinaryDeserializer<'_>, physical: PhysicalType) -> Result<Option<[u8; 16]>> {
    if !d.read_property::<bool>(100, "has_value")? {
        return Ok(None);
    }
    let mut result = [0; 16];
    macro_rules! value {
        ($t:ty) => {{
            let value: $t = d.read_property(101, "value")?;
            let bytes = value.to_le_bytes();
            result[..bytes.len()].copy_from_slice(&bytes);
        }};
    }
    match physical {
        PhysicalType::Bool => result[0] = u8::from(d.read_property::<bool>(101, "value")?),
        PhysicalType::Int8 => value!(i8),
        PhysicalType::Int16 => value!(i16),
        PhysicalType::Int32 => value!(i32),
        PhysicalType::Int64 => value!(i64),
        PhysicalType::UInt8 => value!(u8),
        PhysicalType::UInt16 => value!(u16),
        PhysicalType::UInt32 => value!(u32),
        PhysicalType::UInt64 => value!(u64),
        PhysicalType::Int128 => value!(i128),
        PhysicalType::UInt128 => value!(u128),
        PhysicalType::Float => value!(f32),
        PhysicalType::Double => value!(f64),
        PhysicalType::Interval => {
            let value: Interval = d.read_property(101, "value")?;
            result[..4].copy_from_slice(&value.months.to_le_bytes());
            result[4..8].copy_from_slice(&value.days.to_le_bytes());
            result[8..].copy_from_slice(&value.micros.to_le_bytes());
        }
        other => {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("no numeric statistics for {other:?}"),
            ));
        }
    }
    Ok(Some(result))
}

/// The bounds of a VARCHAR column's values: prefixes of them where `exact` is false (`StringStatsData`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringBounds {
    pub min: Vec<u8>,
    pub min_exact: bool,
    pub max: Vec<u8>,
    pub max_exact: bool,
}

/// `StringStatsData::LEGACY_MAX_STRING_MINMAX_SIZE` and `CURRENT_MAX_STRING_MINMAX_SIZE`.
const LEGACY_MINMAX_SIZE: usize = 8;
const CURRENT_MINMAX_SIZE: usize = 12;

/// `StringStats::Deserialize`: the bounds, `None` without any (`NO_STATS`, `EMPTY_STATS`).
fn string_stats(d: &mut BinaryDeserializer<'_>) -> Result<Option<StringBounds>> {
    let legacy_min = d.read_optional_property::<RawBytes>(200, "min")?;
    let legacy_max = d.read_optional_property::<RawBytes>(201, "max")?;
    if let (Some(min), Some(max)) = (legacy_min, legacy_max) {
        d.read_property::<bool>(202, "has_unicode")?;
        let has_max_length = d.read_property::<bool>(203, "has_max_string_length")?;
        let max_length = d.read_property::<u32>(204, "max_string_length")? as usize;
        let (min, max) = (min.0, max.0);
        if min.len() != LEGACY_MINMAX_SIZE || max.len() != LEGACY_MINMAX_SIZE {
            return Ok(None);
        }
        // `LegacyGetMinMaxType`
        if min[0] > max[0] || (min.iter().all(|b| *b == 0) && max.iter().all(|b| *b == 0xFF)) {
            return Ok(None);
        }
        let exact = has_max_length
            && max_length <= LEGACY_MINMAX_SIZE
            && (max_length == 0 || (min[max_length - 1] != 0 && max[max_length - 1] != 0));
        // `LegacyReadMinMax`: without the zeros that pad it
        let trimmed = |bytes: Vec<u8>| {
            let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |at| at + 1);
            bytes[..end].to_vec()
        };
        return Ok(Some(StringBounds {
            min: trimmed(min),
            min_exact: exact,
            max: trimmed(max),
            max_exact: exact,
        }));
    }
    d.read_optional_property::<u32>(204, "max_string_length")?;
    let min_max = d.read_optional_property::<RawBytes>(205, "min_max")?;
    let packed = d.read_property::<u32>(206, "packed_field")?;
    d.read_optional_property::<u64>(207, "total_string_length")?;
    // `StringStatsField`
    let Some(min_max) = min_max.filter(|m| m.0.len() == 2 * CURRENT_MINMAX_SIZE) else {
        return Ok(None);
    };
    let min_length = ((packed >> 3) & 0xF) as usize;
    let max_length = ((packed >> 7) & 0xF) as usize;
    if min_length > CURRENT_MINMAX_SIZE || max_length > CURRENT_MINMAX_SIZE {
        return Ok(None);
    }
    Ok(Some(StringBounds {
        min: min_max.0[..min_length].to_vec(),
        min_exact: packed & (1 << 1) != 0,
        max: min_max.0[CURRENT_MINMAX_SIZE..CURRENT_MINMAX_SIZE + max_length].to_vec(),
        max_exact: packed & (1 << 2) != 0,
    }))
}

/// `BaseStatistics::Deserialize`, as a value. `ty` is the column's type, or `None` for validity.
pub fn read_statistics(d: &mut BinaryDeserializer<'_>, ty: Option<&LogicalType>) -> Result<BaseStatistics> {
    d.read_object_value(|d| {
        let has_null = d.read_property(100, "has_null")?;
        let has_no_null = d.read_property(101, "has_no_null")?;
        d.read_property::<u64>(102, "distinct_count")?;
        let mut geometry = None;
        let mut strings = None;
        let bounds = d.read_object(103, "type_stats", |d| read_type_stats(d, ty, &mut geometry, &mut strings))?;
        Ok(BaseStatistics {
            has_null,
            has_no_null,
            min: bounds.map(|(min, _)| min),
            max: bounds.and_then(|(_, max)| max),
            geometry,
            strings,
        })
    })
}

/// The bounds of the values, of the types that have numeric statistics.
type Bounds = Option<([u8; 16], Option<[u8; 16]>)>;

fn read_type_stats(
    d: &mut BinaryDeserializer<'_>,
    ty: Option<&LogicalType>,
    geometry: &mut Option<duckrs_common::geometry::GeometryStats>,
    strings: &mut Option<StringBounds>,
) -> Result<Bounds> {
    let Some(ty) = ty.filter(|ty| ty.id() != LogicalTypeId::SqlNull) else {
        return Ok(None);
    };
    if ty.id() == LogicalTypeId::Variant {
        let state: u8 = d.read_property(200, "shredding_state")?;
        if state != 1 {
            return Err(Error::not_implemented(
                "reading a shredded VARIANT column from a database file is not yet ported to duckrs",
            ));
        }
        let unshredded = duckrs_common::variant::unshredded_type();
        d.read_object_field(225, "unshredded_stats", |d| read_statistics(d, Some(&unshredded)))?;
        return Ok(None);
    }
    // `GeometryStats::Deserialize`: the string stats of files of the spatial extension, or the extent,
    // types and flags, none of which is used yet
    if ty.id() == LogicalTypeId::Geometry {
        if d.has_property(200)? {
            return string_stats(d).map(|_| None);
        }
        let mut stats = duckrs_common::geometry::GeometryStats::empty();
        let bounds = d.read_object_field(300, "extent", |d| {
            d.read_object_value(|d| {
                let mut bounds = [0f64; 8];
                for (field, bound) in (101..=108).zip(bounds.iter_mut()) {
                    *bound = d.read_property::<f64>(field, "extent")?;
                }
                Ok(bounds)
            })
        })?;
        let extent = &mut stats.extent;
        [extent.x_min, extent.x_max, extent.y_min, extent.y_max] = [bounds[0], bounds[1], bounds[2], bounds[3]];
        [extent.z_min, extent.z_max, extent.m_min, extent.m_max] = [bounds[4], bounds[5], bounds[6], bounds[7]];
        stats.types = d.read_object_field(301, "types", |d| {
            d.read_object_value(|d| {
                let mut types = [0u8; 4];
                for (field, set) in (101..=104).zip(types.iter_mut()) {
                    *set = d.read_property::<u8>(field, "types")?;
                }
                Ok(types)
            })
        })?;
        stats.flags = d.read_optional_property::<u8>(302, "flags")?.unwrap_or(0);
        *geometry = Some(stats);
        return Ok(None);
    }
    let physical = ty.physical_type();
    match physical {
        PhysicalType::Varchar => {
            *strings = string_stats(d)?;
            Ok(None)
        }
        PhysicalType::Struct => {
            let children = ty.struct_children();
            let mut i = 0;
            d.read_list(200, "child_stats", |d| {
                let child = children.get(i).map(|(_, ty)| ty);
                i += 1;
                read_statistics(d, child)
            })?;
            Ok(None)
        }
        PhysicalType::List => {
            d.read_object_field(200, "child_stats", |d| read_statistics(d, ty.child_type()))?;
            Ok(None)
        }
        PhysicalType::Array => {
            d.read_object_field(200, "child_stats", |d| read_statistics(d, ty.child_type()))?;
            Ok(None)
        }
        PhysicalType::Bit | PhysicalType::Invalid | PhysicalType::Unknown => Ok(None),
        _ => {
            if !d.has_property(200)? {
                return Ok(None);
            }
            let min = d.read_object(200, "min", |d| numeric_value(d, physical))?;
            let max = d.read_object(201, "max", |d| numeric_value(d, physical))?;
            Ok(min.map(|min| (min, max)))
        }
    }
}

/// `HyperLogLog::Deserialize`, whose registers nothing uses yet.
struct HyperLogLog;

impl Deserialize for HyperLogLog {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            d.read_property::<u8>(100, "type")?;
            d.read_property::<RawBytes>(101, "data")?;
            Ok(Self)
        })
    }
}

/// `DistinctStatistics::Deserialize`.
struct DistinctStatistics;

impl Deserialize for DistinctStatistics {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            d.read_property_with_default::<u64>(100, "sample_count")?;
            d.read_property_with_default::<u64>(101, "total_count")?;
            d.read_optional_property::<Option<HyperLogLog>>(102, "log")?;
            Ok(Self)
        })
    }
}

/// `ColumnStatistics::Deserialize`, of an element of a list of pointers, which can be null.
pub fn read_column_statistics(d: &mut BinaryDeserializer<'_>, ty: &LogicalType) -> Result<Option<BaseStatistics>> {
    if !bool::read_value(d)? {
        return Ok(None);
    }
    d.read_object_value(|d| {
        let statistics = d.read_object_field(100, "statistics", |d| read_statistics(d, Some(ty)))?;
        d.read_optional_property::<Option<DistinctStatistics>>(101, "distinct")?;
        Ok(Some(statistics))
    })
}

/// `Vector::Deserialize`, for a vector nothing needs: all it does is get past one.
fn skip_vector(d: &mut BinaryDeserializer<'_>, ty: &LogicalType) -> Result<()> {
    const FLAT_VECTOR: u8 = 0;
    const CONSTANT_VECTOR: u8 = 2;
    let vector_type = d
        .read_optional_property::<u8>(90, "vector_type")?
        .unwrap_or(FLAT_VECTOR);
    if vector_type == CONSTANT_VECTOR {
        return skip_vector(d, ty);
    }
    let physical = ty.physical_type();
    if vector_type != FLAT_VECTOR || ty.id() == LogicalTypeId::Geometry {
        return Err(Error::not_implemented(
            "reading this kind of vector from a database file is not yet ported to duckrs",
        ));
    }
    if d.read_property::<bool>(100, "has_validity_mask")? {
        d.read_property::<RawBytes>(101, "validity")?;
    }
    match physical {
        PhysicalType::Varchar => {
            if d.read_optional_property::<u64>(107, "byte_data_length")?.is_some() {
                d.read_property::<RawBytes>(108, "length_data")?;
                d.read_property::<RawBytes>(109, "byte_data")?;
            } else {
                d.read_property::<Vec<RawBytes>>(102, "data")?;
            }
        }
        PhysicalType::Struct => {
            let children = ty.struct_children();
            let mut i = 0;
            d.read_list(103, "children", |d| {
                let (_, child) = children
                    .get(i)
                    .ok_or_else(|| corrupt("a vector has too many children"))?;
                i += 1;
                d.read_object_value(|d| skip_vector(d, child))
            })?;
        }
        PhysicalType::List => {
            let child = ty.child_type().ok_or_else(|| corrupt("a list has no child type"))?;
            d.read_property::<u64>(104, "list_size")?;
            d.read_list(105, "entries", |d| {
                d.read_object_value(|d| {
                    d.read_property_with_default::<u64>(100, "offset")?;
                    d.read_property_with_default::<u64>(101, "length")
                })
            })?;
            d.read_object(106, "child", |d| skip_vector(d, child))?;
        }
        PhysicalType::Array => {
            let child = ty.child_type().ok_or_else(|| corrupt("an array has no child type"))?;
            d.read_property::<u64>(103, "array_size")?;
            d.read_object(104, "child", |d| skip_vector(d, child))?;
        }
        _ => {
            d.read_property::<RawBytes>(102, "data")?;
        }
    }
    Ok(())
}

/// `BlockingSample::Deserialize`, without the sample, which only `SAMPLE` over a table might use.
/// It sits in front of the row groups, so it has to be read. `read_value` of a pointer that can be
/// null, like every property it is in.
pub fn skip_table_sample(d: &mut BinaryDeserializer<'_>) -> Result<()> {
    const RESERVOIR_SAMPLE: u8 = 1;
    if !bool::read_value(d)? {
        return Ok(());
    }
    d.read_object_value(|d| {
        if d.has_property(100)? {
            d.read_object_field(100, "base_reservoir_sample", |d| {
                if bool::read_value(d)? {
                    d.read_object_value(|d| {
                        d.read_property_with_default::<u64>(100, "next_index_to_sample")?;
                        d.read_property::<f64>(101, "min_weight_threshold")?;
                        d.read_property_with_default::<u64>(102, "min_weighted_entry_index")?;
                        d.read_property_with_default::<u64>(103, "num_entries_to_skip_b4_next_sample")?;
                        d.read_property_with_default::<u64>(104, "num_entries_seen_total")?;
                        d.read_property_with_default::<Vec<(f64, u64)>>(105, "reservoir_weights")?;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
        }
        let sample_type: u8 = d.read_property(101, "type")?;
        d.read_property_with_default::<bool>(102, "destroyed")?;
        if sample_type != RESERVOIR_SAMPLE {
            return Err(corrupt("the sample of a table is not a reservoir sample"));
        }
        d.read_property_with_default::<u64>(200, "sample_count")?;
        if d.has_property(201)? {
            d.read_object_field(201, "reservoir_chunk", |d| {
                if bool::read_value(d)? {
                    d.read_object_value(skip_data_chunk)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}

/// `DataChunk::Deserialize`, for one nothing needs.
fn skip_data_chunk(d: &mut BinaryDeserializer<'_>) -> Result<()> {
    d.read_property::<u32>(100, "rows")?;
    let types: Vec<LogicalType> = d.read_property(101, "types")?;
    let mut column = types.iter();
    d.read_list(102, "columns", |d| {
        let ty = column
            .next()
            .ok_or_else(|| corrupt("a chunk has more columns than types"))?;
        d.read_object_value(|d| skip_vector(d, ty))
    })?;
    Ok(())
}
