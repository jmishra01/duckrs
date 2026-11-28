//! Ported from duckdb/src/storage/statistics/*.cpp @ rust-port-baseline.
//!
//! Serialized properties have no length, so to get past statistics they have to be read. What the
//! scans need of them is whether a segment has NULLs and, for a constant segment, its value.

use super::block_manager::corrupt;
use pintail_common::serializer::{BinaryDeserializer, Deserialize};
use pintail_common::types::{Interval, LogicalType, LogicalTypeId, PhysicalType};
use pintail_common::{Error, ErrorKind, Result};

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
    pub geometry: Option<pintail_common::geometry::GeometryStats>,
    /// of a VARCHAR, the bounds the file has
    pub strings: Option<StringBounds>,
    /// of a VARIANT, its `VariantStatsShreddingState`
    pub variant_state: Option<u8>,
    /// of a shredded VARIANT, the type of its shredded part and the statistics of that
    pub variant_shredded: Option<Box<(LogicalType, BaseStatistics)>>,
    /// of a LIST or ARRAY its element's, of a STRUCT each field's
    pub children: Vec<BaseStatistics>,
    /// of a LIST, the lengths of its lists, as files from v2.1.0 have them
    pub lists: Option<crate::catalog::ListLengths>,
    /// of a table's column, its `DistinctStatistics`, if the file has them in a form that is read
    pub distinct: Option<crate::optimizer::distinct_statistics::DistinctState>,
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
    /// whether `min` and `max` are bounds: without them only the rest is known
    pub has_bounds: bool,
    pub min: Vec<u8>,
    pub min_exact: bool,
    pub max: Vec<u8>,
    pub max_exact: bool,
    pub has_unicode: bool,
    pub min_length: Option<u64>,
    pub max_length: Option<u64>,
    pub total_length: Option<u64>,
}

/// `StringStatsData::LEGACY_MAX_STRING_MINMAX_SIZE` and `CURRENT_MAX_STRING_MINMAX_SIZE`.
const LEGACY_MINMAX_SIZE: usize = 8;
const CURRENT_MINMAX_SIZE: usize = 12;
/// `StringStatsData::MAXIMUM_MIN_STRING_LENGTH`
const MAXIMUM_MIN_STRING_LENGTH: u32 = (1 << 21) - 1;

/// `StringStats::Deserialize`: the bounds, `None` without any (`NO_STATS`, `EMPTY_STATS`).
fn string_stats(d: &mut BinaryDeserializer<'_>) -> Result<Option<StringBounds>> {
    let legacy_min = d.read_optional_property::<RawBytes>(200, "min")?;
    let legacy_max = d.read_optional_property::<RawBytes>(201, "max")?;
    if let (Some(min), Some(max)) = (legacy_min, legacy_max) {
        let has_unicode = d.read_property::<bool>(202, "has_unicode")?;
        let has_max_length = d.read_property::<bool>(203, "has_max_string_length")?;
        let max_length = d.read_property::<u32>(204, "max_string_length")? as usize;
        let (min, max) = (min.0, max.0);
        // `LegacyGetMinMaxType`: EMPTY_STATS or NO_STATS have no bounds, but the rest
        if min.len() != LEGACY_MINMAX_SIZE
            || max.len() != LEGACY_MINMAX_SIZE
            || min[0] > max[0]
            || (min.iter().all(|b| *b == 0) && max.iter().all(|b| *b == 0xFF))
        {
            return Ok(Some(StringBounds {
                has_bounds: false,
                min: Vec::new(),
                min_exact: false,
                max: Vec::new(),
                max_exact: false,
                has_unicode,
                min_length: None,
                max_length: has_max_length.then_some(max_length as u64),
                total_length: None,
            }));
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
            has_bounds: true,
            min: trimmed(min),
            min_exact: exact,
            max: trimmed(max),
            max_exact: exact,
            has_unicode,
            min_length: None,
            max_length: has_max_length.then_some(max_length as u64),
            total_length: None,
        }));
    }
    let max_length = d.read_optional_property::<u32>(204, "max_string_length")?;
    let min_max = d.read_optional_property::<RawBytes>(205, "min_max")?;
    let packed = d.read_property::<u32>(206, "packed_field")?;
    let total_length = d.read_optional_property::<u64>(207, "total_string_length")?;
    // `StringStatsField::GetMinStringLength`: 0 for not set, the length plus one, or the most it can hold
    let min_length = match (packed >> 11) & ((1 << 21) - 1) {
        0 => None,
        MAXIMUM_MIN_STRING_LENGTH => Some(u64::from(u32::MAX)),
        length => Some(u64::from(length - 1)),
    };
    let has_unicode = packed & 1 == 0;
    let max_length = max_length.filter(|length| *length != u32::MAX).map(u64::from);
    // `StringStatsField`
    let min_size = ((packed >> 3) & 0xF) as usize;
    let max_size = ((packed >> 7) & 0xF) as usize;
    let Some(min_max) = min_max.filter(|m| {
        m.0.len() == 2 * CURRENT_MINMAX_SIZE && min_size <= CURRENT_MINMAX_SIZE && max_size <= CURRENT_MINMAX_SIZE
    }) else {
        return Ok(Some(StringBounds {
            has_bounds: false,
            min: Vec::new(),
            min_exact: false,
            max: Vec::new(),
            max_exact: false,
            has_unicode,
            min_length,
            max_length,
            total_length,
        }));
    };
    Ok(Some(StringBounds {
        has_bounds: true,
        min: min_max.0[..min_size].to_vec(),
        min_exact: packed & (1 << 1) != 0,
        max: min_max.0[CURRENT_MINMAX_SIZE..CURRENT_MINMAX_SIZE + max_size].to_vec(),
        max_exact: packed & (1 << 2) != 0,
        has_unicode,
        min_length,
        max_length,
        total_length,
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
        let mut nested = Nested::default();
        let bounds = d.read_object(103, "type_stats", |d| {
            read_type_stats(d, ty, &mut geometry, &mut strings, &mut nested)
        })?;
        Ok(BaseStatistics {
            has_null,
            has_no_null,
            min: bounds.map(|(min, _)| min),
            max: bounds.and_then(|(_, max)| max),
            geometry,
            strings,
            variant_state: nested.variant_state,
            variant_shredded: nested.variant_shredded,
            children: nested.children,
            lists: nested.lists,
            distinct: None,
        })
    })
}

/// What statistics of a VARIANT or a nested type have besides their own.
#[derive(Default)]
struct Nested {
    variant_state: Option<u8>,
    variant_shredded: Option<Box<(LogicalType, BaseStatistics)>>,
    children: Vec<BaseStatistics>,
    lists: Option<crate::catalog::ListLengths>,
}

/// The bounds of the values, of the types that have numeric statistics.
type Bounds = Option<([u8; 16], Option<[u8; 16]>)>;

fn read_type_stats(
    d: &mut BinaryDeserializer<'_>,
    ty: Option<&LogicalType>,
    geometry: &mut Option<pintail_common::geometry::GeometryStats>,
    strings: &mut Option<StringBounds>,
    nested: &mut Nested,
) -> Result<Bounds> {
    let Some(ty) = ty.filter(|ty| ty.id() != LogicalTypeId::SqlNull) else {
        return Ok(None);
    };
    if ty.id() == LogicalTypeId::Variant {
        // `VariantStats::Deserialize`: the statistics of the unshredded part, and of a shredded one
        const SHREDDED: u8 = 2;
        let state: u8 = d.read_property(200, "shredding_state")?;
        nested.variant_state = Some(state);
        let unshredded = pintail_common::variant::unshredded_type();
        d.read_object_field(225, "unshredded_stats", |d| read_statistics(d, Some(&unshredded)))?;
        if state == SHREDDED {
            let shredded: LogicalType = d.read_property(230, "shredded_type")?;
            let stats = d.read_object_field(235, "shredded_stats", |d| read_statistics(d, Some(&shredded)))?;
            nested.variant_shredded = Some(Box::new((shredded, stats)));
        }
        return Ok(None);
    }
    // `GeometryStats::Deserialize`: the string stats of files of the spatial extension, or the extent,
    // types and flags, none of which is used yet
    if ty.id() == LogicalTypeId::Geometry {
        if d.has_property(200)? {
            // (the string statistics are not of the geometries: they are unknown, `GeometryStats::CreateUnknown`)
            *geometry = Some(pintail_common::geometry::GeometryStats::unknown());
            return string_stats(d).map(|_| None);
        }
        let mut stats = pintail_common::geometry::GeometryStats::empty();
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
            nested.children = d.read_list(200, "child_stats", |d| {
                let child = children.get(i).map(|(_, ty)| ty);
                i += 1;
                read_statistics(d, child)
            })?;
            Ok(None)
        }
        PhysicalType::List | PhysicalType::Array => {
            let child = d.read_object_field(200, "child_stats", |d| read_statistics(d, ty.child_type()))?;
            nested.children = vec![child];
            // `ListStats::Deserialize`: what is not there is not known
            if ty.physical_type() == PhysicalType::List {
                nested.lists = Some(crate::catalog::ListLengths {
                    min: d.read_optional_property::<u64>(201, "min_list_length")?,
                    max: d.read_optional_property::<u64>(202, "max_list_length")?,
                    total: d.read_optional_property::<u64>(203, "total_list_length")?,
                })
                .filter(|lists| lists.min.is_some() || lists.max.is_some() || lists.total.is_some());
            }
            Ok(None)
        }
        PhysicalType::Bit | PhysicalType::Invalid | PhysicalType::Unknown => Ok(None),
        _ => {
            if !d.has_property(200)? {
                return Ok(None);
            }
            let min = d.read_object(200, "min", |d| numeric_value(d, physical))?;
            let max = d.read_object(201, "max", |d| numeric_value(d, physical))?;
            // `NumericStats::HasMinMax`: a minimum above the maximum, of no values, is no bounds
            if let (Some(min), Some(max)) = (&min, &max)
                && super::writer::compare(physical, min, max) == std::cmp::Ordering::Greater
            {
                return Ok(None);
            }
            Ok(min.map(|min| (min, max)))
        }
    }
}

/// `HyperLogLog::Deserialize`, whose registers nothing uses yet.
/// `HyperLogLog::Deserialize`: the registers, as they are of `HLL_V2`, and of `HLL_V1` as `HLLV1::ToNew` makes
/// them; `None` of what is neither.
struct HyperLogLog(Option<Vec<u8>>);

use crate::optimizer::distinct_statistics::{HLL_V1, HLL_V2, registers_of_v1};

impl Deserialize for HyperLogLog {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let storage = d.read_property::<u8>(100, "type")?;
            let data = d.read_property::<RawBytes>(101, "data")?;
            Ok(Self(match storage {
                HLL_V2 => Some(data.0),
                HLL_V1 => registers_of_v1(&data.0),
                _ => None,
            }))
        })
    }
}

/// `DistinctStatistics::Deserialize`.
struct DistinctStatistics(Option<crate::optimizer::distinct_statistics::DistinctState>);

impl Deserialize for DistinctStatistics {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let sample_count = d.read_property_with_default::<u64>(100, "sample_count")?;
            let total_count = d.read_property_with_default::<u64>(101, "total_count")?;
            let log = d.read_optional_property::<Option<HyperLogLog>>(102, "log")?.flatten();
            let state = log.and_then(|log| log.0).and_then(|registers| {
                crate::optimizer::distinct_statistics::DistinctState::stored(&registers, sample_count, total_count)
            });
            Ok(Self(state))
        })
    }
}

/// `ColumnStatistics::Deserialize`, of an element of a list of pointers, which can be null.
pub fn read_column_statistics(d: &mut BinaryDeserializer<'_>, ty: &LogicalType) -> Result<Option<BaseStatistics>> {
    if !bool::read_value(d)? {
        return Ok(None);
    }
    d.read_object_value(|d| {
        let mut statistics = d.read_object_field(100, "statistics", |d| read_statistics(d, Some(ty)))?;
        statistics.distinct = d
            .read_optional_property::<Option<DistinctStatistics>>(101, "distinct")?
            .flatten()
            .and_then(|distinct| distinct.0);
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
            "reading this kind of vector from a database file is not yet ported to pintail",
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

#[cfg(test)]
mod tests {
    use super::*;
    use pintail_common::serializer::{BinarySerializer, SerializationOptions};

    #[test]
    fn distinct_statistics_read_back_as_written() {
        use crate::optimizer::distinct_statistics::DistinctState;
        let mut state = DistinctState::default();
        let mut vector = pintail_common::vector::Vector::new(LogicalType::BIGINT, 2048);
        vector.prepare_flat(2048);
        for (row, value) in vector.data_mut::<i64>().iter_mut().enumerate() {
            *value = (row % 300) as i64;
        }
        assert!(state.update(&LogicalType::BIGINT, &vector, 2048));
        let mut s = BinarySerializer::new(SerializationOptions::default());
        s.write_object_value(|s| state.write(s));
        let bytes = s.into_bytes();
        let mut d = BinaryDeserializer::new(&bytes);
        let read = DistinctStatistics::read_value(&mut d).unwrap().0.unwrap();
        assert_eq!(read.count(), state.count());
        assert!(read.count() > 0);
    }
}
