// Ported from duckdb/src/function/table/arrow/arrow_duck_schema.cpp (ArrowType::GetTypeFromFormat),
// src/function/table/arrow.cpp (PopulateArrowTableSchema) and src/function/table/arrow_conversion.cpp
// (ArrowToDuckDBConversion) @ rust-port-baseline

//! Arrow schemas and arrays C hands in, as DuckDB types and vectors.
//!
//! A schema becomes an [`ArrowType`] per column (`ArrowType::GetArrowLogicalType`): the DuckDB type and how its
//! Arrow array is laid out. An array is read a whole column at a time into a vector of that type, its buffers read
//! as bytes, so they need not be aligned (the C data interface allows that, and C++ reads them as they are). The
//! values are copied: a vector does not point into the Arrow array, which [`import_chunk`] keeps alive only as long
//! as the vectors that were read from it are, as C++'s zero-copy vectors keep it.

use super::raw::{ArrayView, ArrowArray, SchemaView};
use duckrs::{DataChunk, Error, ErrorKind, LogicalType, LogicalTypeId, Result};
use duckrs_common::types::{Interval, ListEntry, NativeType, PhysicalType};
use duckrs_common::vector::{SelectionVector, Vector};
use std::sync::Arc;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn not_implemented(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::NotImplemented, message)
}

/// A unit of time of an Arrow type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Second,
    Milli,
    Micro,
    Nano,
}

impl Unit {
    /// The microseconds of `value` of this unit.
    fn to_micros(self, value: i64) -> i64 {
        match self {
            Unit::Second => value.saturating_mul(1_000_000),
            Unit::Milli => value.saturating_mul(1000),
            Unit::Micro => value,
            Unit::Nano => value / 1000,
        }
    }
}

/// How the Arrow array of a column is laid out (`ArrowTypeInfo` and the physical type of the format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Null,
    /// bit-packed booleans
    Bool,
    /// `arrow.bool8`: a byte per boolean
    Bool8,
    /// values of `width` bytes, the same as the vector's (integers, floats, HUGEINT and TIME_TZ of DuckDB's
    /// extension types)
    Fixed(usize),
    /// a decimal of `bits` bits, into the vector's decimal of its width
    Decimal(u32),
    Date32,
    Date64,
    /// TIME of `unit`, in 32 bits for seconds and milliseconds
    Time(Unit),
    /// TIMESTAMP of `unit`; `to_micros` for a time zone, which DuckDB keeps in microseconds
    Timestamp { unit: Unit, to_micros: bool },
    Duration(Unit),
    IntervalMonths,
    IntervalDayTime,
    IntervalMonthDayNano,
    /// strings or blobs with offsets of 32 bits, or 64 when `large`
    Bytes { large: bool },
    /// string or binary views
    View,
    FixedBinary(usize),
    /// `arrow.uuid`: 16 bytes, big-endian
    Uuid,
    List { large: bool, view: bool },
    FixedList(usize),
    Struct,
    Map,
    /// dictionary-encoded: indices of `index` (an integer layout) into the dictionary (the one child)
    Dictionary { index: Box<ArrowType> },
}

/// A column's DuckDB type and the layout of its Arrow array (`ArrowType`); `children` are those of a nested type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowType {
    pub ty: LogicalType,
    pub layout: Layout,
    pub children: Vec<ArrowType>,
}

impl ArrowType {
    fn leaf(ty: LogicalType, layout: Layout) -> Self {
        Self {
            ty,
            layout,
            children: Vec::new(),
        }
    }
}

/// `ArrowTableSchema`: the columns of a schema, by name (`duckdb_arrow_converted_schema`).
#[derive(Debug, Clone)]
pub struct ConvertedSchema {
    pub names: Vec<String>,
    pub columns: Vec<ArrowType>,
}

impl ConvertedSchema {
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.ty.clone()).collect()
    }
}

/// `ArrowType::GetTypeFromFormat` of a format that names a type without children.
fn type_from_format(format: &str) -> Result<Option<ArrowType>> {
    use LogicalType as T;
    let leaf = |ty: LogicalType, layout: Layout| Ok(Some(ArrowType::leaf(ty, layout)));
    let id = |id: LogicalTypeId| LogicalType::new(id);
    match format {
        "n" => return leaf(T::SQLNULL, Layout::Null),
        "b" => return leaf(T::BOOLEAN, Layout::Bool),
        "c" => return leaf(T::TINYINT, Layout::Fixed(1)),
        "s" => return leaf(T::SMALLINT, Layout::Fixed(2)),
        "i" => return leaf(T::INTEGER, Layout::Fixed(4)),
        "l" => return leaf(T::BIGINT, Layout::Fixed(8)),
        "C" => return leaf(T::UTINYINT, Layout::Fixed(1)),
        "S" => return leaf(T::USMALLINT, Layout::Fixed(2)),
        "I" => return leaf(T::UINTEGER, Layout::Fixed(4)),
        "L" => return leaf(T::UBIGINT, Layout::Fixed(8)),
        "f" => return leaf(T::FLOAT, Layout::Fixed(4)),
        "g" => return leaf(T::DOUBLE, Layout::Fixed(8)),
        "u" => return leaf(T::VARCHAR, Layout::Bytes { large: false }),
        "U" => return leaf(T::VARCHAR, Layout::Bytes { large: true }),
        "vu" => return leaf(T::VARCHAR, Layout::View),
        "z" => return leaf(T::BLOB, Layout::Bytes { large: false }),
        "Z" => return leaf(T::BLOB, Layout::Bytes { large: true }),
        "vz" => return leaf(T::BLOB, Layout::View),
        "tsn:" => return leaf(id(LogicalTypeId::TimestampNs), Layout::Timestamp { unit: Unit::Nano, to_micros: false }),
        "tsu:" => return leaf(T::TIMESTAMP, Layout::Timestamp { unit: Unit::Micro, to_micros: false }),
        "tsm:" => return leaf(id(LogicalTypeId::TimestampMs), Layout::Timestamp { unit: Unit::Milli, to_micros: false }),
        "tss:" => return leaf(id(LogicalTypeId::TimestampSec), Layout::Timestamp { unit: Unit::Second, to_micros: false }),
        "tdD" => return leaf(T::DATE, Layout::Date32),
        "tdm" => return leaf(T::DATE, Layout::Date64),
        "tts" => return leaf(T::TIME, Layout::Time(Unit::Second)),
        "ttm" => return leaf(T::TIME, Layout::Time(Unit::Milli)),
        "ttu" => return leaf(T::TIME, Layout::Time(Unit::Micro)),
        "ttn" => return leaf(id(LogicalTypeId::TimeNs), Layout::Time(Unit::Nano)),
        "tDs" => return leaf(T::INTERVAL, Layout::Duration(Unit::Second)),
        "tDm" => return leaf(T::INTERVAL, Layout::Duration(Unit::Milli)),
        "tDu" => return leaf(T::INTERVAL, Layout::Duration(Unit::Micro)),
        "tDn" => return leaf(T::INTERVAL, Layout::Duration(Unit::Nano)),
        "tiD" => return leaf(T::INTERVAL, Layout::IntervalDayTime),
        "tiM" => return leaf(T::INTERVAL, Layout::IntervalMonths),
        "tin" => return leaf(T::INTERVAL, Layout::IntervalMonthDayNano),
        _ => {}
    }
    if let Some(parameters) = format.strip_prefix('d') {
        // decimal128 or decimal of a bit width (e.g. d:38,0 or d:9,3,32)
        let Some(parameters) = parameters.strip_prefix(':') else {
            return Err(invalid(format!(
                "Decimal format of Arrow object is incomplete, it is missing the scale and width. Current format: {format}"
            )));
        };
        let parts: Vec<&str> = parameters.split(',').collect();
        if parts.len() != 2 && parts.len() != 3 {
            return Err(invalid(format!(
                "Decimal format of Arrow object is incomplete, it is missing the scale or width. Current format: {format}"
            )));
        }
        let number = |text: &str| {
            text.trim()
                .parse::<u64>()
                .map_err(|_| invalid(format!("Invalid Arrow decimal format: {format}")))
        };
        let width = number(parts[0])?;
        let scale = number(parts[1])?;
        let bits = if parts.len() == 3 { number(parts[2])? } else { 128 };
        if width > 38 || bits > 128 {
            return Err(not_implemented(format!("Unsupported Internal Arrow Type for Decimal {format}")));
        }
        if !matches!(bits, 32 | 64 | 128) {
            return Err(not_implemented(format!(
                "Unsupported bit-width value of {bits}  for Arrow Decimal type"
            )));
        }
        if width == 0 || scale > width {
            return Err(invalid(format!("Invalid Arrow decimal format: {format}")));
        }
        return leaf(T::decimal(width as u8, scale as u8), Layout::Decimal(bits as u32));
    }
    if let Some(size) = format.strip_prefix("w:") {
        let size = size
            .parse::<usize>()
            .map_err(|_| invalid(format!("Invalid Arrow fixed-size binary format string: \"{format}\"")))?;
        return leaf(T::BLOB, Layout::FixedBinary(size));
    }
    if format.starts_with('w') {
        return Err(invalid(format!("Invalid Arrow fixed-size binary format string: \"{format}\"")));
    }
    if let Some(rest) = format.strip_prefix("ts") {
        // a timestamp with a time zone: DuckDB keeps the instant
        let unit = match rest.as_bytes().first() {
            Some(b'n') => {
                return leaf(
                    id(LogicalTypeId::TimestampTzNs),
                    Layout::Timestamp { unit: Unit::Nano, to_micros: false },
                );
            }
            Some(b'u') => Unit::Micro,
            Some(b'm') => Unit::Milli,
            Some(b's') => Unit::Second,
            _ => return Err(not_implemented(" Timestamptz precision of not accepted")),
        };
        return leaf(id(LogicalTypeId::TimestampTz), Layout::Timestamp { unit, to_micros: true });
    }
    Ok(None)
}

/// `QueryResult::DeduplicateColumns`: a name that came before, ignoring case, gets `_1`, `_2`, ... appended.
pub fn deduplicate(names: &mut [String]) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for name in names.iter() {
        seen.entry(name.to_lowercase()).or_insert(0);
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in names.iter_mut() {
        let key = name.to_lowercase();
        let count = counts.entry(key.clone()).or_insert(0);
        if *count > 0 {
            loop {
                let candidate = format!("{name}_{count}");
                *count += 1;
                if let std::collections::hash_map::Entry::Vacant(entry) = seen.entry(candidate.to_lowercase()) {
                    entry.insert(0);
                    *name = candidate;
                    break;
                }
            }
        } else {
            *count = 1;
        }
    }
}

/// The extension a field's metadata names: its `ARROW:extension:name`, and for `arrow.opaque` the `type_name` of
/// DuckDB's vendor.
fn extension(schema: &SchemaView<'_>) -> Option<(String, Option<String>)> {
    let metadata = schema.metadata();
    let name = metadata.iter().find(|(k, _)| k == "ARROW:extension:name")?.1.clone();
    let type_name = metadata
        .iter()
        .find(|(k, _)| k == "ARROW:extension:metadata")
        .and_then(|(_, v)| {
            let vendor_is_duckdb = v.contains("\"vendor_name\"") && v.contains("\"DuckDB\"");
            let start = v.find("\"type_name\"")? + "\"type_name\"".len();
            let rest = v[start..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            let end = rest.find('"')?;
            vendor_is_duckdb.then(|| rest[..end].to_string())
        });
    Some((name, type_name))
}

/// `ArrowType::GetArrowLogicalType`.
pub fn arrow_type(schema: SchemaView<'_>) -> Result<ArrowType> {
    let format = schema.format();
    if let Some(dictionary) = schema.dictionary() {
        let index = arrow_type_without_dictionary(schema, format)?;
        if !matches!(index.layout, Layout::Fixed(_)) || !index.ty.is_integral() {
            return Err(invalid(format!("Unsupported dictionary index type {format}")));
        }
        let values = arrow_type(dictionary)?;
        return Ok(ArrowType {
            ty: values.ty.clone(),
            layout: Layout::Dictionary { index: Box::new(index) },
            children: vec![values],
        });
    }
    arrow_type_without_dictionary(schema, format)
}

fn arrow_type_without_dictionary(schema: SchemaView<'_>, format: &str) -> Result<ArrowType> {
    if let Some((name, type_name)) = extension(&schema) {
        let known = match (name.as_str(), type_name.as_deref(), format) {
            ("arrow.uuid", _, "w:16") => Some(ArrowType::leaf(LogicalType::UUID, Layout::Uuid)),
            ("arrow.bool8", _, "c") => Some(ArrowType::leaf(LogicalType::BOOLEAN, Layout::Bool8)),
            ("arrow.opaque", Some("hugeint"), "w:16") => Some(ArrowType::leaf(LogicalType::HUGEINT, Layout::Fixed(16))),
            ("arrow.opaque", Some("uhugeint"), "w:16") => {
                Some(ArrowType::leaf(LogicalType::UHUGEINT, Layout::Fixed(16)))
            }
            ("arrow.opaque", Some("time_tz"), "w:8") => {
                Some(ArrowType::leaf(LogicalType::new(LogicalTypeId::TimeTz), Layout::Fixed(8)))
            }
            ("arrow.json", _, "u" | "U" | "vu") => {
                let mut storage = type_from_format(format)?.expect("a string format");
                storage.ty = LogicalType::VARCHAR.with_alias("JSON");
                Some(storage)
            }
            _ => None,
        };
        if let Some(known) = known {
            return Ok(known);
        }
    }
    if let Some(ty) = type_from_format(format)? {
        return Ok(ty);
    }
    let child = |i: usize| arrow_type(schema.child(i)?);
    let list = |large: bool, view: bool| -> Result<ArrowType> {
        let child = child(0)?;
        Ok(ArrowType {
            ty: LogicalType::list(child.ty.clone()),
            layout: Layout::List { large, view },
            children: vec![child],
        })
    };
    match format {
        "+l" => return list(false, false),
        "+L" => return list(true, false),
        "+vl" => return list(false, true),
        "+vL" => return list(true, true),
        "+s" => {
            let mut children = Vec::with_capacity(schema.n_children());
            let mut members = Vec::with_capacity(schema.n_children());
            for i in 0..schema.n_children() {
                let child_schema = schema.child(i)?;
                let child = arrow_type(child_schema)?;
                members.push((child_schema.name().to_string(), child.ty.clone()));
                children.push(child);
            }
            return Ok(ArrowType {
                ty: LogicalType::struct_type(members),
                layout: Layout::Struct,
                children,
            });
        }
        "+m" => {
            let entries = schema.child(0)?;
            if entries.n_children() != 2 {
                return Err(invalid("Arrow map entries must have a key and a value"));
            }
            let key = arrow_type(entries.child(0)?)?;
            let value = arrow_type(entries.child(1)?)?;
            let ty = LogicalType::map(key.ty.clone(), value.ty.clone());
            let entry = ArrowType {
                ty: LogicalType::struct_type(vec![("key".to_string(), key.ty.clone()), ("value".to_string(), value.ty.clone())]),
                layout: Layout::Struct,
                children: vec![key, value],
            };
            return Ok(ArrowType {
                ty,
                layout: Layout::Map,
                children: vec![entry],
            });
        }
        _ => {}
    }
    if let Some(size) = format.strip_prefix("+w") {
        let Some(size) = size.strip_prefix(':').and_then(|s| s.parse::<u32>().ok()) else {
            return Err(invalid(format!("Invalid Arrow fixed-size list format string: \"{format}\"")));
        };
        let child = child(0)?;
        return Ok(ArrowType {
            ty: LogicalType::array(child.ty.clone(), size),
            layout: Layout::FixedList(size as usize),
            children: vec![child],
        });
    }
    if let Some(kind) = format.strip_prefix("+u") {
        return Err(not_implemented(format!(
            "Unsupported Internal Arrow Type: \"{}\" Union",
            kind.chars().next().unwrap_or(' ')
        )));
    }
    Err(not_implemented(format!("Unsupported Internal Arrow Type {format}")))
}

/// `ArrowTableFunction::PopulateArrowTableSchema`: the columns of a struct schema.
pub fn convert_schema(schema: SchemaView<'_>) -> Result<ConvertedSchema> {
    let mut names = Vec::with_capacity(schema.n_children());
    for i in 0..schema.n_children() {
        let child = schema.child(i)?;
        if child.is_released() {
            return Err(invalid("arrow_scan: released schema passed"));
        }
        let name = child.name();
        names.push(if name.is_empty() { format!("v{i}") } else { name.to_string() });
    }
    deduplicate(&mut names);
    let mut columns = Vec::with_capacity(names.len());
    for i in 0..schema.n_children() {
        columns.push(arrow_type(schema.child(i)?)?);
    }
    Ok(ConvertedSchema { names, columns })
}

/// A fixed-width value read from bytes in the machine's order.
trait FromBytes: Sized + Copy {
    const WIDTH: usize;
    fn read(bytes: &[u8]) -> Self;
}

macro_rules! from_bytes {
    ($($t:ty),*) => {$(
        impl FromBytes for $t {
            const WIDTH: usize = size_of::<$t>();
            fn read(bytes: &[u8]) -> Self {
                <$t>::from_ne_bytes(bytes[..size_of::<$t>()].try_into().expect("enough bytes"))
            }
        }
    )*};
}
from_bytes!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

/// The `count` values of `T` from row `base` of a values buffer.
fn values<'a, T: FromBytes + 'a>(bytes: &'a [u8], base: usize, count: usize) -> impl Iterator<Item = T> + 'a {
    bytes[base * T::WIDTH..(base + count) * T::WIDTH]
        .chunks_exact(T::WIDTH)
        .map(T::read)
}

/// Buffer `index` of `array` with `count` values of `width` bytes from row `base`.
fn value_buffer<'a>(array: &ArrayView<'a>, index: usize, base: usize, count: usize, width: usize) -> Result<&'a [u8]> {
    let len = (base + count)
        .checked_mul(width)
        .ok_or_else(|| invalid("Arrow array is too large"))?;
    array.data(index, len)
}

/// Writes `values` to the first rows of `out`.
fn write<T: NativeType>(out: &mut Vector, values: impl Iterator<Item = T>) {
    for (slot, value) in out.data_mut::<T>().iter_mut().zip(values) {
        *slot = value;
    }
}

/// Whether each of rows `base..base + count` of `array` is valid, or `None` when all are.
fn validity_bits(array: &ArrayView<'_>, base: usize, count: usize) -> Result<Option<Vec<bool>>> {
    if array.null_count() == 0 || count == 0 {
        return Ok(None);
    }
    let Some(bits) = array.buffer(0, (base + count).div_ceil(8))? else {
        return Ok(None);
    };
    Ok(Some((base..base + count).map(|bit| bits[bit / 8] >> (bit % 8) & 1 == 1).collect()))
}

/// `ArrowToDuckDBConversion::SetValidityMask`: the NULLs of rows `base..base + count` of `array`.
fn read_validity(array: &ArrayView<'_>, base: usize, count: usize, out: &mut Vector) -> Result<()> {
    if let Some(valid) = validity_bits(array, base, count)? {
        let validity = out.validity_mut();
        for (i, valid) in valid.into_iter().enumerate() {
            if !valid {
                validity.set_invalid(i);
            }
        }
    }
    Ok(())
}

/// The offsets of rows `base..=base + count` of a string, blob or list array (buffer 1), as `i64`s, checked to only
/// grow.
fn offsets(array: &ArrayView<'_>, base: usize, count: usize, large: bool) -> Result<Vec<i64>> {
    let result: Vec<i64> = if large {
        values::<i64>(value_buffer(array, 1, base, count + 1, 8)?, base, count + 1).collect()
    } else {
        values::<i32>(value_buffer(array, 1, base, count + 1, 4)?, base, count + 1)
            .map(i64::from)
            .collect()
    };
    if result.first().is_some_and(|&first| first < 0) || result.windows(2).any(|w| w[1] < w[0]) {
        return Err(invalid("Arrow array has offsets that are negative or decrease"));
    }
    Ok(result)
}

/// The `count` strings or blobs of a view array (`vu`/`vz`) from row `base`.
fn views<'a>(array: &ArrayView<'a>, base: usize, count: usize) -> Result<Vec<&'a [u8]>> {
    let data = value_buffer(array, 1, base, count, 16)?;
    // buffers: validity, views, the variadic data buffers, and their sizes
    let variadic = array.n_buffers().saturating_sub(3);
    let sizes: Vec<i64> = if variadic > 0 {
        values::<i64>(array.data(array.n_buffers() - 1, variadic * 8)?, 0, variadic).collect()
    } else {
        Vec::new()
    };
    let mut buffers = Vec::with_capacity(variadic);
    for (i, &size) in sizes.iter().enumerate() {
        let size = usize::try_from(size).map_err(|_| invalid("Arrow string view buffer has a negative size"))?;
        buffers.push(array.data(2 + i, size)?);
    }
    let valid = validity_bits(array, base, count)?;
    let mut result = Vec::with_capacity(count);
    for (i, view) in data[base * 16..(base + count) * 16].chunks_exact(16).enumerate() {
        if valid.as_ref().is_some_and(|valid| !valid[i]) {
            // the view of a NULL need not be one
            result.push(&[][..]);
            continue;
        }
        let length = i32::read(&view[0..4]);
        let length = usize::try_from(length).map_err(|_| invalid("Arrow string view has a negative length"))?;
        if length <= 12 {
            result.push(&view[4..4 + length]);
            continue;
        }
        let buffer = usize::try_from(i32::read(&view[8..12])).unwrap_or(usize::MAX);
        let offset = usize::try_from(i32::read(&view[12..16])).unwrap_or(usize::MAX);
        let bytes = buffers
            .get(buffer)
            .and_then(|b| b.get(offset..offset.checked_add(length)?))
            .ok_or_else(|| invalid("Arrow string view points past its data buffers"))?;
        result.push(bytes);
    }
    Ok(result)
}

/// Strings (or blobs) into `out`, with the NULLs of `array`.
fn write_strings(out: &mut Vector, strings: Vec<&[u8]>) {
    out.set_strs(strings);
}

/// Reads rows `start..start + count` of `array` (rows after its own offset) into a new vector of `ty.ty`
/// (`ArrowToDuckDBConversion::ColumnArrowToDuckDB`).
pub fn import_column(ty: &ArrowType, array: &ArrayView<'_>, start: usize, count: usize) -> Result<Vector> {
    array.check()?;
    let base = array.offset() + start;
    if let Layout::Dictionary { index } = &ty.layout {
        return import_dictionary(ty, index, array, base, count);
    }
    let mut out = Vector::new(ty.ty.clone(), count.max(1));
    if count == 0 {
        return Ok(out);
    }
    match &ty.layout {
        Layout::Null => out.validity_mut().set_all_invalid(count),
        Layout::Bool => {
            let bits = array.data(1, (base + count).div_ceil(8))?;
            write::<bool>(&mut out, (base..base + count).map(|bit| bits[bit / 8] >> (bit % 8) & 1 == 1));
        }
        Layout::Bool8 => {
            let bytes = value_buffer(array, 1, base, count, 1)?;
            write::<bool>(&mut out, values::<i8>(bytes, base, count).map(|v| v != 0));
        }
        Layout::Fixed(width) => {
            let bytes = value_buffer(array, 1, base, count, *width)?;
            let source = &bytes[base * width..(base + count) * width];
            match out.physical_type() {
                PhysicalType::Int8
                | PhysicalType::Int16
                | PhysicalType::Int32
                | PhysicalType::Int64
                | PhysicalType::Int128
                | PhysicalType::UInt8
                | PhysicalType::UInt16
                | PhysicalType::UInt32
                | PhysicalType::UInt64
                | PhysicalType::UInt128
                | PhysicalType::Float
                | PhysicalType::Double => out.number_bytes_mut()[..source.len()].copy_from_slice(source),
                _ => return Err(not_implemented(format!("Unsupported Arrow type for {}", ty.ty))),
            }
        }
        Layout::Decimal(bits) => {
            let wide: Vec<i128> = match bits {
                32 => values::<i32>(value_buffer(array, 1, base, count, 4)?, base, count).map(i128::from).collect(),
                64 => values::<i64>(value_buffer(array, 1, base, count, 8)?, base, count).map(i128::from).collect(),
                _ => values::<i128>(value_buffer(array, 1, base, count, 16)?, base, count).collect(),
            };
            match out.physical_type() {
                PhysicalType::Int16 => write::<i16>(&mut out, wide.into_iter().map(|v| v as i16)),
                PhysicalType::Int32 => write::<i32>(&mut out, wide.into_iter().map(|v| v as i32)),
                PhysicalType::Int64 => write::<i64>(&mut out, wide.into_iter().map(|v| v as i64)),
                _ => write::<i128>(&mut out, wide.into_iter()),
            }
        }
        Layout::Date32 => write::<i32>(&mut out, values::<i32>(value_buffer(array, 1, base, count, 4)?, base, count)),
        Layout::Date64 => write::<i32>(
            &mut out,
            values::<i64>(value_buffer(array, 1, base, count, 8)?, base, count)
                .map(|ms| ms.div_euclid(86_400_000) as i32),
        ),
        Layout::Time(unit @ (Unit::Second | Unit::Milli)) => write::<i64>(
            &mut out,
            values::<i32>(value_buffer(array, 1, base, count, 4)?, base, count).map(|v| unit.to_micros(i64::from(v))),
        ),
        Layout::Time(_) => write::<i64>(&mut out, values::<i64>(value_buffer(array, 1, base, count, 8)?, base, count)),
        Layout::Timestamp { unit, to_micros } => {
            let source = values::<i64>(value_buffer(array, 1, base, count, 8)?, base, count);
            if *to_micros {
                write::<i64>(&mut out, source.map(|v| unit.to_micros(v)));
            } else {
                write::<i64>(&mut out, source);
            }
        }
        Layout::Duration(unit) => write::<Interval>(
            &mut out,
            values::<i64>(value_buffer(array, 1, base, count, 8)?, base, count).map(|v| Interval {
                months: 0,
                days: 0,
                micros: unit.to_micros(v),
            }),
        ),
        Layout::IntervalMonths => write::<Interval>(
            &mut out,
            values::<i32>(value_buffer(array, 1, base, count, 4)?, base, count).map(|months| Interval {
                months,
                days: 0,
                micros: 0,
            }),
        ),
        Layout::IntervalDayTime => {
            let bytes = value_buffer(array, 1, base, count, 8)?;
            write::<Interval>(
                &mut out,
                bytes[base * 8..(base + count) * 8].chunks_exact(8).map(|v| Interval {
                    months: 0,
                    days: i32::read(&v[0..4]),
                    micros: i64::from(i32::read(&v[4..8])) * 1000,
                }),
            );
        }
        Layout::IntervalMonthDayNano => {
            let bytes = value_buffer(array, 1, base, count, 16)?;
            write::<Interval>(
                &mut out,
                bytes[base * 16..(base + count) * 16].chunks_exact(16).map(|v| Interval {
                    months: i32::read(&v[0..4]),
                    days: i32::read(&v[4..8]),
                    micros: i64::read(&v[8..16]) / 1000,
                }),
            );
        }
        Layout::Bytes { large } => {
            let offsets = offsets(array, base, count, *large)?;
            let end = *offsets.last().expect("count + 1 offsets") as usize;
            let data = array.data(2, end)?;
            let strings = offsets
                .windows(2)
                .map(|w| &data[w[0] as usize..w[1] as usize])
                .collect::<Vec<_>>();
            if out.physical_type() != PhysicalType::Varchar {
                return Err(not_implemented(format!("Unsupported Arrow type for {}", ty.ty)));
            }
            write_strings(&mut out, strings);
        }
        Layout::View => {
            let strings = views(array, base, count)?;
            write_strings(&mut out, strings);
        }
        Layout::FixedBinary(width) => {
            let bytes = value_buffer(array, 1, base, count, *width)?;
            let strings = if *width == 0 {
                vec![&[][..]; count]
            } else {
                bytes[base * width..(base + count) * width].chunks_exact(*width).collect()
            };
            write_strings(&mut out, strings);
        }
        Layout::Uuid => {
            let bytes = value_buffer(array, 1, base, count, 16)?;
            write::<i128>(
                &mut out,
                bytes[base * 16..(base + count) * 16]
                    .chunks_exact(16)
                    .map(|v| (u128::from_be_bytes(v.try_into().expect("16 bytes")) ^ (1 << 127)) as i128),
            );
        }
        Layout::List { large, view } => {
            let (entries, child_start, child_count) = if *view {
                let read = |index: usize| -> Result<Vec<i64>> {
                    Ok(if *large {
                        values::<i64>(value_buffer(array, index, base, count, 8)?, base, count).collect()
                    } else {
                        values::<i32>(value_buffer(array, index, base, count, 4)?, base, count)
                            .map(i64::from)
                            .collect()
                    })
                };
                let (starts, sizes) = (read(1)?, read(2)?);
                if starts.iter().chain(&sizes).any(|&v| v < 0) {
                    return Err(invalid("Arrow list view has a negative offset or size"));
                }
                let end = starts.iter().zip(&sizes).map(|(s, n)| s + n).max().unwrap_or(0);
                let entries: Vec<ListEntry> = starts
                    .iter()
                    .zip(&sizes)
                    .map(|(&s, &n)| ListEntry { offset: s as u64, length: n as u64 })
                    .collect();
                (entries, 0, end as usize)
            } else {
                let offsets = offsets(array, base, count, *large)?;
                let first = offsets[0];
                let entries = offsets
                    .windows(2)
                    .map(|w| ListEntry {
                        offset: (w[0] - first) as u64,
                        length: (w[1] - w[0]) as u64,
                    })
                    .collect();
                (entries, first as usize, (offsets[count] - first) as usize)
            };
            let child = import_column(&ty.children[0], &array.child(0)?, child_start, child_count)?;
            write::<ListEntry>(&mut out, entries.into_iter());
            out.set_list_size(child_count);
            *out.list_child_mut() = child;
        }
        Layout::FixedList(size) => {
            let child = import_column(&ty.children[0], &array.child(0)?, base * size, count * size)?;
            *out.array_child_mut() = child;
        }
        Layout::Struct => {
            if array.n_children() != ty.children.len() {
                return Err(invalid("Arrow struct array has a different number of children than its schema"));
            }
            for (i, child_type) in ty.children.iter().enumerate() {
                let child = import_column(child_type, &array.child(i)?, base, count)?;
                out.struct_children_mut()[i] = child;
            }
        }
        Layout::Map => {
            let offsets = offsets(array, base, count, false)?;
            let first = offsets[0];
            let child_count = (offsets[count] - first) as usize;
            let child = import_column(&ty.children[0], &array.child(0)?, first as usize, child_count)?;
            write::<ListEntry>(
                &mut out,
                offsets.windows(2).map(|w| ListEntry {
                    offset: (w[0] - first) as u64,
                    length: (w[1] - w[0]) as u64,
                }),
            );
            out.set_list_size(child_count);
            *out.list_child_mut() = child;
        }
        Layout::Dictionary { .. } => unreachable!("handled above"),
    }
    read_validity(array, base, count, &mut out)?;
    Ok(out)
}

/// `ColumnArrowToDuckDBDictionary`: the dictionary's values, selected by the indices of rows `base..base + count`.
fn import_dictionary(ty: &ArrowType, index: &ArrowType, array: &ArrayView<'_>, base: usize, count: usize) -> Result<Vector> {
    let dictionary = array.dictionary()?;
    let dictionary_len = dictionary.len();
    let Layout::Fixed(width) = index.layout else {
        return Err(invalid("Unsupported dictionary index type"));
    };
    let bytes = value_buffer(array, 1, base, count, width)?;
    let signed = !matches!(
        index.ty.id(),
        LogicalTypeId::UTinyInt | LogicalTypeId::USmallInt | LogicalTypeId::UInteger | LogicalTypeId::UBigInt
    );
    let raw: Vec<i128> = match (width, signed) {
        (1, true) => values::<i8>(bytes, base, count).map(i128::from).collect(),
        (2, true) => values::<i16>(bytes, base, count).map(i128::from).collect(),
        (4, true) => values::<i32>(bytes, base, count).map(i128::from).collect(),
        (8, true) => values::<i64>(bytes, base, count).map(i128::from).collect(),
        (1, false) => values::<u8>(bytes, base, count).map(i128::from).collect(),
        (2, false) => values::<u16>(bytes, base, count).map(i128::from).collect(),
        (4, false) => values::<u32>(bytes, base, count).map(i128::from).collect(),
        _ => values::<u64>(bytes, base, count).map(i128::from).collect(),
    };
    // the NULLs are those of the indices
    let valid = validity_bits(array, base, count)?;
    let is_valid = |i: usize| valid.as_ref().is_none_or(|valid| valid[i]);
    let mut selection = Vec::with_capacity(count);
    for (i, &index) in raw.iter().enumerate() {
        if !is_valid(i) {
            selection.push(0);
            continue;
        }
        if index < 0 || index as usize >= dictionary_len {
            return Err(invalid("Arrow dictionary index is out of range"));
        }
        selection.push(index as u32);
    }
    if dictionary_len == 0 {
        let mut out = Vector::new(ty.ty.clone(), count.max(1));
        out.validity_mut().set_all_invalid(count);
        return Ok(out);
    }
    let mut out = import_column(&ty.children[0], &dictionary, 0, dictionary_len)?;
    out.slice(&SelectionVector::from_vec(selection), count);
    out.flatten(count);
    for i in 0..count {
        if !is_valid(i) {
            out.validity_mut().set_invalid(i);
        }
    }
    Ok(out)
}

/// `ValidateArrowArrayShape`: rejects an array whose shape disagrees with the schema before anything reads it.
pub fn validate_shape(schema: &ConvertedSchema, array: &ArrayView<'_>) -> Result<()> {
    array.check()?;
    let column_count = schema.columns.len();
    if array.n_children() != column_count {
        return Err(invalid("Arrow array child count does not match the schema's column count"));
    }
    if column_count > 0 && !array.has_children_pointer() {
        return Err(invalid("Arrow array has null children"));
    }
    for i in 0..column_count {
        let child = array
            .child(i)
            .map_err(|_| invalid("Arrow array child is null or already released"))?;
        if child.len() != array.len() {
            return Err(invalid("Arrow array child length does not match the array length"));
        }
    }
    Ok(())
}

/// An Arrow array duckrs took over from C: released when the last vector read from it goes away.
pub struct OwnedArray(pub ArrowArray);

// SAFETY: the array is only read while it is converted and released once, on drop; the C data interface lets the
// consumer release an array from any thread, and C++ does so from whichever thread drops its last vector.
unsafe impl Send for OwnedArray {}

// SAFETY: as for `Send`: nothing reads or writes the array through a shared reference after the conversion.
unsafe impl Sync for OwnedArray {}

impl Drop for OwnedArray {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            // SAFETY: the array was moved out of C's struct (whose release C++ then sets to null), so this is its
            // only owner and it is released once.
            unsafe { release(&mut self.0) };
        }
    }
}

/// Rows `start..start + count` of the struct array `array` as a chunk of the columns of `schema`, each vector
/// keeping `owner` alive (`duckdb_data_chunk_from_arrow`, `ArrowTableFunction::ArrowToDuckDB`).
pub fn import_chunk(
    schema: &ConvertedSchema,
    array: &ArrayView<'_>,
    start: usize,
    count: usize,
    owner: Option<Arc<OwnedArray>>,
) -> Result<DataChunk> {
    let mut columns = Vec::with_capacity(schema.columns.len());
    for (i, column) in schema.columns.iter().enumerate() {
        // the struct parent's offset applies to its children
        let mut vector = import_column(column, &array.child(i)?, array.offset() + start, count)?;
        if let Some(owner) = &owner {
            vector.set_auxiliary(owner.clone());
        }
        columns.push(vector);
    }
    Ok(DataChunk::from_columns(columns, count))
}
