// Ported from duckdb/src/common/arrow/arrow_converter.cpp (SetArrowFormat), arrow_appender.cpp and
// arrow_type_extension.cpp @ rust-port-baseline

//! DuckDB types and data chunks as Arrow (`ArrowConverter::ToArrowSchema` / `ToArrowArray`).
//!
//! The arrays are built with arrow-rs a whole vector at a time, from the typed values of the (flattened) vector, and
//! handed to C through `arrow::ffi`, whose release callbacks free them. The type of each column follows C++'s
//! `SetArrowFormat`, with the options of [`ArrowOptions`].

use super::options::ArrowOptions;
use crate::vector::prepare_vector;
use arrow::array::{
    Array, ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, DictionaryArray, FixedSizeBinaryArray,
    FixedSizeListArray, GenericListArray, GenericListViewArray, LargeBinaryArray, LargeStringArray, MapArray, NullArray,
    OffsetSizeTrait, PrimitiveArray, StringArray, StructArray, UInt64Array,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{
    ArrowPrimitiveType, DataType, Date32Type, Decimal32Type, Decimal64Type, Decimal128Type, Field, FieldRef, Fields,
    Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, IntervalMonthDayNano, IntervalMonthDayNanoType,
    IntervalUnit, Time64MicrosecondType, Time64NanosecondType, TimeUnit, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type, UInt32Type,
    UInt64Type,
};
use duckrs::{DataChunk, Error, ErrorKind, LogicalType, LogicalTypeId, Result};
use duckrs_common::types::{Interval, ListEntry, NativeType, PhysicalType, TimeTz, uuid_to_string};
use duckrs_common::vector::Vector;
use std::collections::HashMap;
use std::sync::Arc;

const EXTENSION_NAME: &str = "ARROW:extension:name";
const EXTENSION_METADATA: &str = "ARROW:extension:metadata";

/// `ArrowSchemaMetadata::ArrowCanonicalType`.
fn canonical(name: &str) -> HashMap<String, String> {
    HashMap::from([
        (EXTENSION_NAME.to_string(), name.to_string()),
        (EXTENSION_METADATA.to_string(), String::new()),
    ])
}

/// `ArrowSchemaMetadata::NonCanonicalType`: `arrow.opaque` of DuckDB's `type_name`.
fn opaque(type_name: &str) -> HashMap<String, String> {
    HashMap::from([
        (EXTENSION_NAME.to_string(), "arrow.opaque".to_string()),
        (
            EXTENSION_METADATA.to_string(),
            format!("{{\"type_name\":\"{type_name}\",\"vendor_name\":\"DuckDB\"}}"),
        ),
    ])
}

fn arrow_error(error: arrow::error::ArrowError) -> Error {
    Error::new(ErrorKind::InvalidInput, error.to_string())
}

fn unsupported(ty: &LogicalType) -> Error {
    Error::new(ErrorKind::NotImplemented, format!("Unsupported Arrow type {ty}"))
}

fn is_json(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Varchar && ty.alias().is_some_and(|alias| alias.eq_ignore_ascii_case("json"))
}

/// The field of a column named `name` of type `ty` (`InitializeChild` and `SetArrowFormat`).
pub fn field(name: &str, ty: &LogicalType, options: &ArrowOptions) -> Result<Field> {
    let (data_type, metadata) = data_type(ty, options)?;
    Ok(Field::new(name, data_type, true).with_metadata(metadata))
}

/// The fields of the children of a STRUCT.
fn struct_fields(ty: &LogicalType, options: &ArrowOptions) -> Result<Fields> {
    ty.struct_children()
        .iter()
        .map(|(name, child)| field(name, child, options))
        .collect::<Result<Vec<_>>>()
        .map(Fields::from)
}

/// The field `entries` of a MAP: a struct of a non-nullable key and a value.
fn map_entries(ty: &LogicalType, options: &ArrowOptions) -> Result<FieldRef> {
    let entry = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
    let fields: Vec<Field> = struct_fields(&entry, options)?
        .iter()
        .enumerate()
        .map(|(i, f)| f.as_ref().clone().with_nullable(i != 0))
        .collect();
    Ok(Arc::new(Field::new("entries", DataType::Struct(fields.into()), false)))
}

/// `SetArrowFormat`: the Arrow type of `ty`, and the metadata of its field (extension types).
fn data_type(ty: &LogicalType, options: &ArrowOptions) -> Result<(DataType, HashMap<String, String>)> {
    use LogicalTypeId as L;
    let plain = |dt: DataType| Ok((dt, HashMap::new()));
    let binary = || {
        if options.at_least_1_4() {
            DataType::BinaryView
        } else if options.large_offsets {
            DataType::LargeBinary
        } else {
            DataType::Binary
        }
    };
    match ty.id() {
        L::Boolean if options.lossless => Ok((DataType::Int8, canonical("arrow.bool8"))),
        L::Boolean => plain(DataType::Boolean),
        L::TinyInt => plain(DataType::Int8),
        L::SmallInt => plain(DataType::Int16),
        L::Integer => plain(DataType::Int32),
        L::BigInt => plain(DataType::Int64),
        L::UTinyInt => plain(DataType::UInt8),
        L::USmallInt => plain(DataType::UInt16),
        L::UInteger => plain(DataType::UInt32),
        L::UBigInt => plain(DataType::UInt64),
        L::Float => plain(DataType::Float32),
        L::Double => plain(DataType::Float64),
        L::HugeInt if options.lossless => Ok((DataType::FixedSizeBinary(16), opaque("hugeint"))),
        L::UHugeInt if options.lossless => Ok((DataType::FixedSizeBinary(16), opaque("uhugeint"))),
        L::HugeInt | L::UHugeInt => plain(DataType::Decimal128(38, 0)),
        L::Uuid if options.lossless => Ok((DataType::FixedSizeBinary(16), canonical("arrow.uuid"))),
        L::Uuid if options.large_offsets => plain(DataType::LargeUtf8),
        L::Uuid => plain(DataType::Utf8),
        L::Varchar | L::Char => {
            let dt = if options.string_view && options.at_least_1_4() {
                DataType::Utf8View
            } else if options.large_offsets {
                DataType::LargeUtf8
            } else {
                DataType::Utf8
            };
            if is_json(ty) && options.lossless {
                Ok((dt, canonical("arrow.json")))
            } else {
                plain(dt)
            }
        }
        L::Date => plain(DataType::Date32),
        L::TimeTz if options.lossless => Ok((DataType::FixedSizeBinary(8), opaque("time_tz"))),
        L::Time | L::TimeTz => plain(DataType::Time64(TimeUnit::Microsecond)),
        L::TimeNs => plain(DataType::Time64(TimeUnit::Nanosecond)),
        L::Timestamp => plain(DataType::Timestamp(TimeUnit::Microsecond, None)),
        L::TimestampTz => plain(DataType::Timestamp(
            TimeUnit::Microsecond,
            Some(options.time_zone.as_str().into()),
        )),
        L::TimestampTzNs => plain(DataType::Timestamp(
            TimeUnit::Nanosecond,
            Some(options.time_zone.as_str().into()),
        )),
        L::TimestampSec => plain(DataType::Timestamp(TimeUnit::Second, None)),
        L::TimestampMs => plain(DataType::Timestamp(TimeUnit::Millisecond, None)),
        L::TimestampNs => plain(DataType::Timestamp(TimeUnit::Nanosecond, None)),
        L::Interval => plain(DataType::Interval(IntervalUnit::MonthDayNano)),
        L::Decimal => {
            let (width, scale) = ty.decimal_properties().unwrap_or((18, 3));
            let scale = scale as i8;
            if options.version <= 4 {
                return plain(DataType::Decimal128(width, scale));
            }
            match ty.physical_type() {
                PhysicalType::Int16 | PhysicalType::Int32 => plain(DataType::Decimal32(width, scale)),
                PhysicalType::Int64 => plain(DataType::Decimal64(width, scale)),
                _ => plain(DataType::Decimal128(width, scale)),
            }
        }
        L::SqlNull => plain(DataType::Null),
        L::Blob => plain(binary()),
        L::Bit if options.lossless => Ok((DataType::Binary, opaque("bit"))),
        L::Bit => plain(binary()),
        L::Bignum => Ok((DataType::Binary, opaque("bignum"))),
        L::List => {
            let child = Arc::new(field(
                "l",
                ty.child_type().unwrap_or(&LogicalType::SQLNULL),
                options,
            )?);
            plain(match (options.list_view && options.at_least_1_4(), options.large_offsets) {
                (true, true) => DataType::LargeListView(child),
                (true, false) => DataType::ListView(child),
                (false, true) => DataType::LargeList(child),
                (false, false) => DataType::List(child),
            })
        }
        L::Struct | L::Tuple => plain(DataType::Struct(struct_fields(ty, options)?)),
        L::Array => {
            let size = ty.array_size().unwrap_or(0) as i32;
            let child = field("", ty.child_type().unwrap_or(&LogicalType::SQLNULL), options)?;
            plain(DataType::FixedSizeList(Arc::new(child), size))
        }
        L::Map => plain(DataType::Map(map_entries(ty, options)?, false)),
        L::Enum => {
            let index = match ty.physical_type() {
                PhysicalType::UInt8 => DataType::UInt8,
                PhysicalType::UInt16 => DataType::UInt16,
                PhysicalType::UInt32 => DataType::UInt32,
                _ => return Err(Error::new(ErrorKind::Internal, "Unsupported Enum Internal Type")),
            };
            plain(DataType::Dictionary(Box::new(index), Box::new(DataType::Utf8)))
        }
        _ => Err(unsupported(ty)),
    }
}

/// The NULLs of the first `count` rows of a flat vector, or none when all are valid.
fn nulls(vector: &Vector, count: usize) -> Option<NullBuffer> {
    if vector.validity().all_valid() {
        return None;
    }
    let valid: BooleanBuffer = (0..count).map(|i| vector.is_valid(i)).collect();
    Some(NullBuffer::new(valid))
}

/// A primitive array of the values of a flat vector, each converted with `convert`, of type `data_type`.
fn primitive<T: ArrowPrimitiveType, N: NativeType>(
    vector: &Vector,
    count: usize,
    data_type: &DataType,
    convert: impl Fn(N) -> T::Native,
) -> ArrayRef {
    let values: ScalarBuffer<T::Native> = vector.data::<N>()[..count].iter().map(|&v| convert(v)).collect();
    Arc::new(PrimitiveArray::<T>::new(values, nulls(vector, count)).with_data_type(data_type.clone()))
}

/// The strings (or blobs) of the first `count` rows of a flat vector, `None` for NULL.
fn strings(vector: &Vector, count: usize) -> Vec<Option<&[u8]>> {
    (0..count).map(|i| vector.str_at(i)).collect()
}

/// An array of `strings` of the variable-size type `data_type`.
fn byte_array(strings: Vec<Option<&[u8]>>, data_type: &DataType) -> Result<ArrayRef> {
    Ok(match data_type {
        DataType::Utf8 => Arc::new(StringArray::try_from_binary(BinaryArray::from_iter(strings)).map_err(arrow_error)?),
        DataType::LargeUtf8 => Arc::new(
            LargeStringArray::try_from_binary(LargeBinaryArray::from_iter(strings)).map_err(arrow_error)?,
        ),
        DataType::Utf8View => Arc::new(BinaryViewArray::from_iter(strings).to_string_view().map_err(arrow_error)?),
        DataType::Binary => Arc::new(BinaryArray::from_iter(strings)),
        DataType::LargeBinary => Arc::new(LargeBinaryArray::from_iter(strings)),
        DataType::BinaryView => Arc::new(BinaryViewArray::from_iter(strings)),
        other => return Err(Error::new(ErrorKind::Internal, format!("not a string type: {other}"))),
    })
}

/// A fixed-size binary array of `width` bytes per row, `bytes` giving those of a valid row.
fn fixed_binary<N: NativeType>(vector: &Vector, count: usize, width: usize, bytes: impl Fn(N) -> Vec<u8>) -> Result<ArrayRef> {
    let mut data = Vec::with_capacity(count * width);
    for (i, &value) in vector.data::<N>()[..count].iter().enumerate() {
        if vector.is_valid(i) {
            data.extend_from_slice(&bytes(value));
        } else {
            data.resize(data.len() + width, 0);
        }
    }
    let array = FixedSizeBinaryArray::try_new(width as i32, Buffer::from_vec(data), nulls(vector, count))
        .map_err(arrow_error)?;
    Ok(Arc::new(array))
}

/// The entries of a LIST/MAP vector as offsets of type `O`, and the child rows they cover in order: `None` when those
/// are the child's rows `0..child_count` as they are, else the rows to take.
fn list_offsets<O: OffsetSizeTrait>(
    vector: &Vector,
    count: usize,
    child_count: usize,
) -> Result<(OffsetBuffer<O>, Option<UInt64Array>)> {
    let entries = &vector.data::<ListEntry>()[..count];
    let mut offsets = Vec::with_capacity(count + 1);
    offsets.push(O::usize_as(0));
    let mut take = Vec::new();
    let mut in_order = true;
    let mut total = 0u64;
    for (i, entry) in entries.iter().enumerate() {
        if vector.is_valid(i) {
            if entry.offset != total {
                in_order = false;
            }
            take.extend(entry.offset..entry.offset + entry.length);
            total += entry.length;
        }
        let offset = O::from_usize(total as usize).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "Arrow Appender: The maximum combined list offset for regular list buffers is 2147483647 but the \
                 offset of the list is too large. Consider setting arrow_large_buffer_size=true",
            )
        })?;
        offsets.push(offset);
    }
    let take = (!in_order || total as usize != child_count).then(|| UInt64Array::from(take));
    Ok((OffsetBuffer::new(ScalarBuffer::from(offsets)), take))
}

/// `child` with the rows `take` gives, in that order (all of them without a `take`).
fn take_rows(child: ArrayRef, take: Option<UInt64Array>) -> Result<ArrayRef> {
    match take {
        None => Ok(child),
        Some(indices) => arrow::compute::take(child.as_ref(), &indices, None).map_err(arrow_error),
    }
}

fn list_array<O: OffsetSizeTrait>(
    vector: &Vector,
    count: usize,
    child_field: &FieldRef,
    options: &ArrowOptions,
) -> Result<ArrayRef> {
    let child_vector = vector.list_child();
    let child_count = vector.list_size();
    let child = array(child_vector, child_count, child_field.data_type(), options)?;
    let (offsets, take) = list_offsets::<O>(vector, count, child_count)?;
    let child = take_rows(child, take)?;
    let list = GenericListArray::<O>::try_new(child_field.clone(), offsets, child, nulls(vector, count))
        .map_err(arrow_error)?;
    Ok(Arc::new(list))
}

/// A list view keeps DuckDB's entries as they are: an offset and a size per row.
fn list_view_array<O: OffsetSizeTrait>(
    vector: &Vector,
    count: usize,
    child_field: &FieldRef,
    options: &ArrowOptions,
) -> Result<ArrayRef> {
    let child = array(vector.list_child(), vector.list_size(), child_field.data_type(), options)?;
    let entries = &vector.data::<ListEntry>()[..count];
    let mut offsets = Vec::with_capacity(count);
    let mut sizes = Vec::with_capacity(count);
    let too_large = || Error::new(ErrorKind::InvalidInput, "Arrow Appender: list offset too large for a ListView");
    for (i, entry) in entries.iter().enumerate() {
        let (offset, size) = if vector.is_valid(i) {
            (entry.offset, entry.length)
        } else {
            (0, 0)
        };
        offsets.push(O::from_usize(offset as usize).ok_or_else(too_large)?);
        sizes.push(O::from_usize(size as usize).ok_or_else(too_large)?);
    }
    let list = GenericListViewArray::<O>::try_new(
        child_field.clone(),
        ScalarBuffer::from(offsets),
        ScalarBuffer::from(sizes),
        child,
        nulls(vector, count),
    )
    .map_err(arrow_error)?;
    Ok(Arc::new(list))
}

fn struct_array(children: &[Vector], fields: &Fields, count: usize, nulls: Option<NullBuffer>, options: &ArrowOptions) -> Result<StructArray> {
    let arrays = children
        .iter()
        .zip(fields.iter())
        .map(|(child, field)| array(child, count, field.data_type(), options))
        .collect::<Result<Vec<_>>>()?;
    StructArray::try_new_with_length(fields.clone(), arrays, nulls, count).map_err(arrow_error)
}

/// The first `count` rows of a flat vector (nested vectors flat too, as `prepare_vector` leaves them) as an array of
/// `data_type`, the type [`data_type`] gives its logical type with `options` (`ArrowAppender`).
fn array(vector: &Vector, count: usize, data_type: &DataType, options: &ArrowOptions) -> Result<ArrayRef> {
    use LogicalTypeId as L;
    let ty = vector.logical_type();
    let dt = data_type;
    Ok(match (ty.id(), dt) {
        (L::SqlNull, _) => Arc::new(NullArray::new(count)),
        (L::Boolean, DataType::Int8) => primitive::<Int8Type, bool>(vector, count, dt, i8::from),
        (L::Boolean, _) => {
            let values: BooleanBuffer = vector.data::<bool>()[..count].iter().copied().collect();
            Arc::new(BooleanArray::new(values, nulls(vector, count)))
        }
        (L::TinyInt, _) => primitive::<Int8Type, i8>(vector, count, dt, |v| v),
        (L::SmallInt, _) => primitive::<Int16Type, i16>(vector, count, dt, |v| v),
        (L::Integer, _) => primitive::<Int32Type, i32>(vector, count, dt, |v| v),
        (L::BigInt, _) => primitive::<Int64Type, i64>(vector, count, dt, |v| v),
        (L::UTinyInt, _) => primitive::<UInt8Type, u8>(vector, count, dt, |v| v),
        (L::USmallInt, _) => primitive::<UInt16Type, u16>(vector, count, dt, |v| v),
        (L::UInteger, _) => primitive::<UInt32Type, u32>(vector, count, dt, |v| v),
        (L::UBigInt, _) => primitive::<UInt64Type, u64>(vector, count, dt, |v| v),
        (L::Float, _) => primitive::<Float32Type, f32>(vector, count, dt, |v| v),
        (L::Double, _) => primitive::<Float64Type, f64>(vector, count, dt, |v| v),
        (L::HugeInt, DataType::FixedSizeBinary(_)) => {
            fixed_binary::<i128>(vector, count, 16, |v| v.to_le_bytes().to_vec())?
        }
        (L::UHugeInt, DataType::FixedSizeBinary(_)) => {
            fixed_binary::<u128>(vector, count, 16, |v| v.to_le_bytes().to_vec())?
        }
        (L::HugeInt, _) => primitive::<Decimal128Type, i128>(vector, count, dt, |v| v),
        (L::UHugeInt, _) => primitive::<Decimal128Type, u128>(vector, count, dt, |v| v as i128),
        (L::Uuid, DataType::FixedSizeBinary(_)) => fixed_binary::<i128>(vector, count, 16, |v| {
            ((v as u128) ^ (1 << 127)).to_be_bytes().to_vec()
        })?,
        (L::Uuid, _) => {
            let texts: Vec<Option<String>> = vector.data::<i128>()[..count]
                .iter()
                .enumerate()
                .map(|(i, &v)| vector.is_valid(i).then(|| uuid_to_string(v)))
                .collect();
            byte_array(texts.iter().map(|t| t.as_deref().map(str::as_bytes)).collect(), dt)?
        }
        (L::Varchar | L::Char | L::Blob | L::Bit | L::Bignum, _) => byte_array(strings(vector, count), dt)?,
        (L::Date, _) => primitive::<Date32Type, i32>(vector, count, dt, |v| v),
        (L::TimeTz, DataType::FixedSizeBinary(_)) => {
            fixed_binary::<u64>(vector, count, 8, |v| v.to_le_bytes().to_vec())?
        }
        (L::TimeTz, _) => primitive::<Time64MicrosecondType, u64>(vector, count, dt, |v| TimeTz(v).time().0),
        (L::Time, _) => primitive::<Time64MicrosecondType, i64>(vector, count, dt, |v| v),
        (L::TimeNs, _) => primitive::<Time64NanosecondType, i64>(vector, count, dt, |v| v),
        (L::Timestamp | L::TimestampTz, _) => primitive::<TimestampMicrosecondType, i64>(vector, count, dt, |v| v),
        (L::TimestampSec, _) => primitive::<TimestampSecondType, i64>(vector, count, dt, |v| v),
        (L::TimestampMs, _) => primitive::<TimestampMillisecondType, i64>(vector, count, dt, |v| v),
        (L::TimestampNs | L::TimestampTzNs, _) => primitive::<TimestampNanosecondType, i64>(vector, count, dt, |v| v),
        (L::Interval, _) => primitive::<IntervalMonthDayNanoType, Interval>(vector, count, dt, |v| {
            IntervalMonthDayNano::new(v.months, v.days, v.micros.saturating_mul(1000))
        }),
        (L::Decimal, _) => {
            macro_rules! decimal {
                ($n:ty) => {
                    match dt {
                        DataType::Decimal32(..) => primitive::<Decimal32Type, $n>(vector, count, dt, |v| v as i32),
                        DataType::Decimal64(..) => primitive::<Decimal64Type, $n>(vector, count, dt, |v| v as i64),
                        _ => primitive::<Decimal128Type, $n>(vector, count, dt, |v| v as i128),
                    }
                };
            }
            match ty.physical_type() {
                PhysicalType::Int16 => decimal!(i16),
                PhysicalType::Int32 => decimal!(i32),
                PhysicalType::Int64 => decimal!(i64),
                _ => decimal!(i128),
            }
        }
        (L::List, DataType::List(child)) => list_array::<i32>(vector, count, child, options)?,
        (L::List, DataType::LargeList(child)) => list_array::<i64>(vector, count, child, options)?,
        (L::List, DataType::ListView(child)) => list_view_array::<i32>(vector, count, child, options)?,
        (L::List, DataType::LargeListView(child)) => list_view_array::<i64>(vector, count, child, options)?,
        (L::Struct | L::Tuple, DataType::Struct(fields)) => {
            if fields.is_empty() {
                Arc::new(StructArray::new_empty_fields(count, nulls(vector, count)))
            } else {
                Arc::new(struct_array(vector.struct_children(), fields, count, nulls(vector, count), options)?)
            }
        }
        (L::Array, DataType::FixedSizeList(child_field, size)) => {
            let child_count = count * (*size as usize);
            let child = array(vector.array_child(), child_count, child_field.data_type(), options)?;
            let array = FixedSizeListArray::try_new(child_field.clone(), *size, child, nulls(vector, count))
                .map_err(arrow_error)?;
            Arc::new(array)
        }
        (L::Map, DataType::Map(entries_field, _)) => {
            let DataType::Struct(entry_fields) = entries_field.data_type() else {
                return Err(unsupported(ty));
            };
            let child_count = vector.list_size();
            let entries = struct_array(vector.list_child().struct_children(), entry_fields, child_count, None, options)?;
            let (offsets, take) = list_offsets::<i32>(vector, count, child_count)?;
            let entries = take_rows(Arc::new(entries), take)?;
            let entries = entries
                .as_any()
                .downcast_ref::<StructArray>()
                .cloned()
                .ok_or_else(|| unsupported(ty))?;
            let map = MapArray::try_new(entries_field.clone(), offsets, entries, nulls(vector, count), false)
                .map_err(arrow_error)?;
            Arc::new(map)
        }
        (L::Enum, DataType::Dictionary(index, _)) => {
            let values = Arc::new(StringArray::from_iter_values(ty.enum_values()));
            macro_rules! dictionary {
                ($t:ty, $n:ty) => {{
                    let keys = PrimitiveArray::<$t>::new(
                        vector.data::<$n>()[..count].iter().copied().collect(),
                        nulls(vector, count),
                    );
                    Arc::new(DictionaryArray::<$t>::try_new(keys, values).map_err(arrow_error)?)
                }};
            }
            match index.as_ref() {
                DataType::UInt8 => dictionary!(UInt8Type, u8),
                DataType::UInt16 => dictionary!(UInt16Type, u16),
                _ => dictionary!(UInt32Type, u32),
            }
        }
        _ => return Err(unsupported(ty)),
    })
}

/// The schema of columns `names` of `types`, named `duckdb_query_result` (`ArrowConverter::ToArrowSchema`).
pub fn schema(
    types: &[LogicalType],
    names: &[String],
    options: &ArrowOptions,
) -> Result<arrow::ffi::FFI_ArrowSchema> {
    let fields = types
        .iter()
        .zip(names)
        .map(|(ty, name)| field(name, ty, options))
        .collect::<Result<Vec<_>>>()?;
    let schema = arrow::datatypes::Schema::new(fields);
    arrow::ffi::FFI_ArrowSchema::try_from(&schema)
        .and_then(|schema| schema.with_name("duckdb_query_result"))
        .map_err(arrow_error)
}

/// A chunk as a struct array of its columns (`ArrowConverter::ToArrowArray`).
pub fn chunk_array(chunk: &DataChunk, options: &ArrowOptions) -> Result<StructArray> {
    let count = chunk.len();
    let mut fields = Vec::with_capacity(chunk.column_count());
    let mut arrays = Vec::with_capacity(chunk.column_count());
    for column in chunk.columns() {
        let mut vector = column.clone();
        prepare_vector(&mut vector, count);
        let field = field("", vector.logical_type(), options)?;
        arrays.push(array(&vector, count, field.data_type(), options)?);
        fields.push(field);
    }
    StructArray::try_new_with_length(fields.into(), arrays, None, count).map_err(arrow_error)
}

/// [`chunk_array`] in the C data interface, for C to release.
pub fn chunk_to_ffi(chunk: &DataChunk, options: &ArrowOptions) -> Result<arrow::ffi::FFI_ArrowArray> {
    let array = chunk_array(chunk, options)?;
    Ok(arrow::ffi::FFI_ArrowArray::new(&array.to_data()))
}
