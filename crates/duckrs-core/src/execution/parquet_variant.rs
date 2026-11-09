// Ported from duckdb/src/common/types/variant/{variant_binary_decoder,parquet_variant_iterator}.cpp and
// duckdb/extension/parquet/reader/variant_column_reader.cpp @ rust-port-baseline
//
//! Reading the Parquet VARIANT encoding: a `metadata` dictionary of keys and a `value` in the binary
//! encoding of the spec, which shredding splits into `typed_value` columns.

use arrow::array::{Array, AsArray, StructArray};
use arrow::datatypes::{DataType, Field, TimeUnit};
use duckrs_common::types::{Date, LogicalType, LogicalTypeId, Time, Timestamp};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::variant::VariantNode;
use duckrs_common::{Error, ErrorKind, Result};

fn io_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Io, message)
}

fn invalid_input(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn overrun(size: usize) -> Error {
    io_error(format!(
        "Data corruption detected, read of length_in_bytes ({size}) would exceed buffer capacity"
    ))
}

/// `size` bytes of `data` from `at`.
fn slice(data: &[u8], at: usize, size: usize) -> Result<&[u8]> {
    at.checked_add(size)
        .and_then(|end| data.get(at..end))
        .ok_or_else(|| overrun(size))
}

/// `ReadVariableLengthLittleEndian`.
fn read_le(data: &[u8], at: usize, size: usize) -> Result<usize> {
    let bytes = slice(data, at, size)?;
    let mut buffer = [0u8; 8];
    buffer[..size].copy_from_slice(bytes);
    Ok(u64::from_le_bytes(buffer) as usize)
}

fn fixed<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N]> {
    Ok(slice(data, at, N)?.try_into().expect("N bytes"))
}

/// `VariantMetadata`: the keys of a row.
pub struct Metadata {
    keys: Vec<String>,
    /// the header byte, the offsets and the keys
    total_size: usize,
}

impl Metadata {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let Some(&header) = data.first() else {
            return Err(io_error("Corrupted VARIANT 'metadata' buffer, empty or nullptr"));
        };
        let version = header & 0xF;
        if version != 1 {
            return Err(Error::not_implemented(format!(
                "Only version 1 of the Variant encoding scheme is supported, found version: {version}"
            )));
        }
        let offset_size = usize::from((header >> 6) & 0x3) + 1;
        let mut at = 1;
        let count = read_le(data, at, offset_size)?;
        at += offset_size;
        let data_start = count
            .checked_add(1)
            .and_then(|n| n.checked_mul(offset_size))
            .and_then(|n| n.checked_add(at))
            .ok_or_else(|| io_error("Corrupted VARIANT 'metadata' buffer"))?;
        let mut last = read_le(data, at, offset_size)?;
        at += offset_size;
        let mut keys = Vec::with_capacity(count.min(data.len()));
        for _ in 0..count {
            let next = read_le(data, at, offset_size)?;
            at += offset_size;
            if next < last {
                return Err(io_error(
                    "Corrupted VARIANT 'metadata' buffer, the next dictionary offset is smaller than the last offset",
                ));
            }
            let key = data_start
                .checked_add(last)
                .and_then(|start| data.get(start..start + (next - last)))
                .ok_or_else(|| io_error("Corrupted VARIANT 'metadata' buffer"))?;
            let key = std::str::from_utf8(key)
                .map_err(|_| io_error("Corrupted VARIANT 'metadata' buffer, a dictionary key isn't valid UTF8"))?;
            keys.push(key.to_string());
            last = next;
        }
        Ok(Self {
            keys,
            total_size: at + last,
        })
    }
}

/// The digits of `value`, at least one.
fn decimal_digits(value: i128) -> u8 {
    let mut abs = value.unsigned_abs();
    let mut digits = 1;
    while abs >= 10 {
        abs /= 10;
        digits += 1;
    }
    digits
}

fn decimal(data: &[u8], at: usize, size: usize) -> Result<VariantNode> {
    let scale = fixed::<1>(data, at)?[0];
    if scale > 38 {
        return Err(io_error(format!(
            "Corrupted VARIANT 'value' buffer, decimal scale {scale} exceeds the maximum of 38"
        )));
    }
    let at = at + 1;
    let (unscaled, width) = match size {
        4 => {
            let v = i128::from(i32::from_le_bytes(fixed(data, at)?));
            (v, decimal_digits(v))
        }
        8 => {
            let v = i128::from(i64::from_le_bytes(fixed(data, at)?));
            (v, decimal_digits(v))
        }
        _ => {
            let v = i128::from_le_bytes(fixed(data, at)?);
            if v.unsigned_abs() >= 10u128.pow(38) {
                return Err(io_error(
                    "Corrupted VARIANT 'value' buffer, DECIMAL16 value exceeds the maximum width of 38",
                ));
            }
            (v, 38)
        }
    };
    Ok(VariantNode::Scalar(Value::decimal(unscaled, width.max(scale), scale)?))
}

fn text(bytes: &[u8], short: bool) -> Result<VariantNode> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(VariantNode::Scalar(Value::varchar(text))),
        Err(_) if short => Err(io_error("Can't decode Variant short-string, string isn't valid UTF8")),
        Err(_) => Err(io_error("Can't decode Variant string, it isn't valid UTF8")),
    }
}

/// The first of the children of an object or array: the count, the ids of an object, then the offsets.
struct Container {
    count: usize,
    ids: usize,
    id_size: usize,
    offsets: usize,
    offset_size: usize,
    values: usize,
}

impl Container {
    fn read(data: &[u8], at: usize, object: bool) -> Result<Self> {
        let header = data[at] >> 2;
        let offset_size = usize::from(header & 0x3) + 1;
        let (id_size, large) = if object {
            (usize::from((header >> 2) & 0x3) + 1, (header >> 4) & 1 == 1)
        } else {
            (0, (header >> 2) & 1 == 1)
        };
        let mut start = at + 1;
        let count = if large {
            start += 4;
            u32::from_le_bytes(fixed(data, at + 1)?) as usize
        } else {
            start += 1;
            usize::from(fixed::<1>(data, at + 1)?[0])
        };
        let offsets = count
            .checked_mul(id_size)
            .and_then(|n| n.checked_add(start))
            .ok_or_else(|| overrun(count))?;
        let values = count
            .checked_add(1)
            .and_then(|n| n.checked_mul(offset_size))
            .and_then(|n| n.checked_add(offsets))
            .ok_or_else(|| overrun(count))?;
        Ok(Self {
            count,
            ids: start,
            id_size,
            offsets,
            offset_size,
            values,
        })
    }

    fn key<'m>(&self, data: &[u8], metadata: &'m Metadata, i: usize) -> Result<&'m str> {
        let id = read_le(data, self.ids + i * self.id_size, self.id_size)?;
        metadata
            .keys
            .get(id)
            .map(String::as_str)
            .ok_or_else(|| io_error("Corrupted VARIANT 'value' buffer"))
    }

    fn child(&self, data: &[u8], i: usize) -> Result<usize> {
        let offset = read_le(data, self.offsets + i * self.offset_size, self.offset_size)?;
        let child = self.values.checked_add(offset).ok_or_else(|| overrun(1))?;
        slice(data, child, 1)?;
        Ok(child)
    }
}

/// Sorts the keys of an object, which may only be there once.
fn finish_object(mut children: Vec<(String, VariantNode)>) -> Result<VariantNode> {
    children.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(pair) = children.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid_input(format!(
            "Invalid VARIANT: object contains the key \"{}\" more than once",
            pair[0].0
        )));
    }
    Ok(VariantNode::Object(children))
}

const MAX_DEPTH: usize = 1000;

/// The value in the binary encoding at `at` of `data`.
pub fn decode_value(metadata: &Metadata, data: &[u8], at: usize) -> Result<VariantNode> {
    decode_at(metadata, data, at, 0)
}

fn decode_at(metadata: &Metadata, data: &[u8], at: usize, depth: usize) -> Result<VariantNode> {
    if depth > MAX_DEPTH {
        return Err(invalid_input("VARIANT value is nested too deeply"));
    }
    let header = fixed::<1>(data, at)?[0];
    let value_header = header >> 2;
    let payload = at + 1;
    Ok(match header & 0x3 {
        1 => {
            let size = usize::from(value_header);
            return text(slice(data, payload, size)?, true);
        }
        2 => {
            let object = Container::read(data, at, true)?;
            let mut children = Vec::with_capacity(object.count.min(data.len()));
            for i in 0..object.count {
                let key = object.key(data, metadata, i)?.to_string();
                let child = object.child(data, i)?;
                children.push((key, decode_at(metadata, data, child, depth + 1)?));
            }
            return finish_object(children);
        }
        3 => {
            let array = Container::read(data, at, false)?;
            let mut items = Vec::with_capacity(array.count.min(data.len()));
            for i in 0..array.count {
                let child = array.child(data, i)?;
                items.push(decode_at(metadata, data, child, depth + 1)?);
            }
            VariantNode::Array(items)
        }
        _ => {
            let int64 = || -> Result<i64> { Ok(i64::from_le_bytes(fixed(data, payload)?)) };
            let scalar = |ty: LogicalTypeId, value: i64| -> Result<VariantNode> {
                Ok(VariantNode::Scalar(Value::new(
                    LogicalType::new(ty),
                    ValueData::Int64(value),
                )?))
            };
            match value_header {
                0 => VariantNode::Null,
                1 => VariantNode::Scalar(Value::boolean(true)),
                2 => VariantNode::Scalar(Value::boolean(false)),
                3 => VariantNode::Scalar(Value::tinyint(fixed::<1>(data, payload)?[0] as i8)),
                4 => VariantNode::Scalar(Value::smallint(i16::from_le_bytes(fixed(data, payload)?))),
                5 => VariantNode::Scalar(Value::integer(i32::from_le_bytes(fixed(data, payload)?))),
                6 => VariantNode::Scalar(Value::bigint(int64()?)),
                7 => VariantNode::Scalar(Value::double(f64::from_le_bytes(fixed(data, payload)?))),
                8 => decimal(data, payload, 4)?,
                9 => decimal(data, payload, 8)?,
                10 => decimal(data, payload, 16)?,
                11 => VariantNode::Scalar(Value::date(Date(i32::from_le_bytes(fixed(data, payload)?)))),
                12 => VariantNode::Scalar(Value::timestamp_tz(Timestamp(int64()?))),
                13 => VariantNode::Scalar(Value::timestamp(Timestamp(int64()?))),
                14 => VariantNode::Scalar(Value::float(f32::from_le_bytes(fixed(data, payload)?))),
                15 | 16 => {
                    let size = u32::from_le_bytes(fixed(data, payload)?) as usize;
                    let bytes = slice(data, payload + 4, size)?;
                    if value_header == 15 {
                        VariantNode::Scalar(Value::blob(bytes))
                    } else {
                        return text(bytes, false);
                    }
                }
                17 => {
                    let micros = int64()?;
                    if !(0..=86_400_000_000).contains(&micros) {
                        return Err(io_error(format!(
                            "Corrupted VARIANT 'value' buffer, TIME value ({micros}) is out of range"
                        )));
                    }
                    VariantNode::Scalar(Value::time(Time(micros)))
                }
                18 => scalar(LogicalTypeId::TimestampTzNs, int64()?)?,
                19 => scalar(LogicalTypeId::TimestampNs, int64()?)?,
                20 => {
                    let raw = u128::from_be_bytes(fixed(data, payload)?);
                    VariantNode::Scalar(Value::uuid((raw ^ (1 << 127)) as i128))
                }
                other => {
                    return Err(Error::not_implemented(format!(
                        "Variant PrimitiveType ({other}) is not supported"
                    )));
                }
            }
        }
    })
}

/// The child of `group` named `name`.
fn child<'a>(group: &'a StructArray, name: &str) -> Option<&'a dyn Array> {
    group.column_by_name(name).map(|c| c.as_ref())
}

fn bytes_at(array: &dyn Array, row: usize) -> Option<&[u8]> {
    if array.is_null(row) {
        return None;
    }
    Some(match array.data_type() {
        DataType::Binary => array.as_binary::<i32>().value(row),
        DataType::LargeBinary => array.as_binary::<i64>().value(row),
        DataType::BinaryView => array.as_binary_view().value(row),
        DataType::Utf8 => array.as_string::<i32>().value(row).as_bytes(),
        DataType::LargeUtf8 => array.as_string::<i64>().value(row).as_bytes(),
        DataType::Utf8View => array.as_string_view().value(row).as_bytes(),
        _ => return None,
    })
}

/// The DuckDB type of a shredded leaf: as the reader has it, but a TIMESTAMP_NS in UTC keeps its nanoseconds.
fn leaf_type(field: &Field) -> Result<LogicalType> {
    if let DataType::Timestamp(TimeUnit::Nanosecond, Some(_)) = field.data_type() {
        return Ok(LogicalType::new(LogicalTypeId::TimestampTzNs));
    }
    super::parquet::field_type(field)
}

fn is_basic_object(data: &[u8]) -> bool {
    data.first().is_some_and(|header| header & 0x3 == 2)
}

/// `ResolveGroup`: the value of a group of `value` and maybe `typed_value`, or `None` if it is missing.
fn resolve_group(metadata: &Metadata, group: &StructArray, row: usize, depth: usize) -> Result<Option<VariantNode>> {
    if depth > MAX_DEPTH {
        return Err(invalid_input("VARIANT value is nested too deeply"));
    }
    for field in group.fields() {
        if field.name() != "value" && field.name() != "typed_value" && !(depth == 0 && field.name() == "metadata") {
            return Err(invalid_input(format!(
                "Variant group can only contain 'value'/'typed_value', not: {}",
                field.name()
            )));
        }
    }
    let value = child(group, "value").and_then(|value| bytes_at(value, row));
    let typed_index = group.fields().iter().position(|f| f.name() == "typed_value");
    if let Some(index) = typed_index {
        let typed = group.column(index).as_ref();
        let field = &group.fields()[index];
        let is_object = matches!(typed.data_type(), DataType::Struct(_));
        if !typed.is_null(row) {
            return typed_node(metadata, typed, field, row, value, depth).map(Some);
        }
        if let Some(value) = value
            && is_object
            && is_basic_object(value)
        {
            return Err(invalid_input(
                "When 'typed_value' for a shredded Object is NULL, 'value' can not contain an Object value",
            ));
        }
    }
    match value {
        Some(value) => {
            slice(value, 0, 1)?;
            decode_value(metadata, value, 0).map(Some)
        }
        None => Ok(None),
    }
}

/// The value of a valid `typed_value`, with the leftover fields of a partly shredded object in `value`.
fn typed_node(
    metadata: &Metadata,
    typed: &dyn Array,
    field: &Field,
    row: usize,
    value: Option<&[u8]>,
    depth: usize,
) -> Result<VariantNode> {
    match typed.data_type() {
        DataType::Struct(fields) => {
            let typed = typed.as_struct();
            let mut children = Vec::with_capacity(fields.len());
            for (field, column) in fields.iter().zip(typed.columns()) {
                let group = column
                    .as_struct_opt()
                    .ok_or_else(|| invalid_input("A shredded VARIANT field has to be a group"))?;
                if let Some(node) = resolve_group(metadata, group, row, depth + 1)? {
                    children.push((field.name().clone(), node));
                }
            }
            if let Some(overlay) = value {
                slice(overlay, 0, 1)?;
                if !is_basic_object(overlay) {
                    return Err(invalid_input(
                        "Partially shredded objects have to encode Object Variants in the 'value'",
                    ));
                }
                let object = Container::read(overlay, 0, true)?;
                for i in 0..object.count {
                    let key = object.key(overlay, metadata, i)?;
                    if children.iter().any(|(k, _)| k == key) {
                        continue;
                    }
                    let at = object.child(overlay, i)?;
                    children.push((key.to_string(), decode_at(metadata, overlay, at, depth + 1)?));
                }
            }
            finish_object(children)
        }
        DataType::List(_) | DataType::LargeList(_) => {
            let elements = match typed.data_type() {
                DataType::List(_) => typed.as_list::<i32>().value(row),
                _ => typed.as_list::<i64>().value(row),
            };
            let group = elements
                .as_struct_opt()
                .ok_or_else(|| invalid_input("A shredded VARIANT element has to be a group"))?;
            let mut items = Vec::with_capacity(group.len());
            for i in 0..group.len() {
                items.push(resolve_group(metadata, group, i, depth + 1)?.unwrap_or(VariantNode::Null));
            }
            Ok(VariantNode::Array(items))
        }
        _ => {
            let ty = leaf_type(field)?;
            match ty.id() {
                LogicalTypeId::Boolean
                | LogicalTypeId::TinyInt
                | LogicalTypeId::SmallInt
                | LogicalTypeId::Integer
                | LogicalTypeId::BigInt
                | LogicalTypeId::Float
                | LogicalTypeId::Double
                | LogicalTypeId::Decimal
                | LogicalTypeId::Date
                | LogicalTypeId::Time
                | LogicalTypeId::TimestampTz
                | LogicalTypeId::TimestampTzNs
                | LogicalTypeId::Timestamp
                | LogicalTypeId::TimestampNs
                | LogicalTypeId::Blob
                | LogicalTypeId::Varchar
                | LogicalTypeId::Uuid => {}
                _ => {
                    return Err(Error::not_implemented(format!(
                        "Variant shredding on type: '{ty}' is not implemented"
                    )));
                }
            }
            let value = super::parquet::field_value(typed, row, &ty)?;
            Ok(VariantNode::Scalar(value))
        }
    }
}

/// The VARIANT of `row` of a VARIANT group of `metadata`, `value` and maybe `typed_value`.
pub fn read_row(group: &StructArray, row: usize) -> Result<Value> {
    let ty = LogicalType::new(LogicalTypeId::Variant);
    let (metadata, value) = (child(group, "metadata"), child(group, "value"));
    if metadata.is_none() || value.is_none() {
        return Err(invalid_input(
            "the Variant column must have 'metadata' and 'value' as the first two columns",
        ));
    }
    let Some(metadata) = metadata.and_then(|m| bytes_at(m, row)) else {
        return Ok(Value::null(ty));
    };
    let metadata = Metadata::parse(metadata)?;
    Ok(match resolve_group(&metadata, group, row, 0)? {
        None | Some(VariantNode::Null) => Value::null(ty),
        Some(node) => Value::new(ty, ValueData::Bytes(node.encode()))?,
    })
}

/// `bytes_to_variant`: a blob of the metadata followed by the value.
pub fn from_bytes(bytes: &[u8]) -> Result<VariantNode> {
    let metadata = Metadata::parse(bytes)?;
    slice(bytes, metadata.total_size, 1)?;
    decode_value(&metadata, bytes, metadata.total_size)
}

// ---------------------------------------------------------------------------------------------------------------
// Writing: `ParquetVariantConversion::ToParquetVariant` and the analysis of `VariantColumnWriter`
// ---------------------------------------------------------------------------------------------------------------

/// `CalculateByteLength`: the bytes `value` needs, at least one.
fn byte_length(value: usize) -> usize {
    let mut length = 1;
    while length < 8 && value >> (8 * length) != 0 {
        length += 1;
    }
    length
}

fn push_le(out: &mut Vec<u8>, value: usize, size: usize) {
    out.extend_from_slice(&(value as u64).to_le_bytes()[..size]);
}

/// The keys of a row, in the order they come up, and where each is.
#[derive(Default)]
struct Dictionary {
    keys: Vec<String>,
    index: std::collections::HashMap<String, usize>,
}

impl Dictionary {
    fn collect(node: &VariantNode) -> Self {
        let mut result = Self::default();
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            match node {
                VariantNode::Object(children) => {
                    for (key, _) in children {
                        if !result.index.contains_key(key) {
                            result.index.insert(key.clone(), result.keys.len());
                            result.keys.push(key.clone());
                        }
                    }
                    stack.extend(children.iter().rev().map(|(_, child)| child));
                }
                VariantNode::Array(items) => stack.extend(items.iter().rev()),
                _ => {}
            }
        }
        result
    }

    /// `CreateMetadata`.
    fn metadata(&self) -> Result<Vec<u8>> {
        let size: usize = self.keys.iter().map(String::len).sum();
        if size >= u32::MAX as usize {
            return Err(invalid_input(
                "The total length of the dictionary exceeds a 4 byte value (uint32_t), failed to export VARIANT to Parquet",
            ));
        }
        let width = byte_length(size);
        let mut out = Vec::with_capacity(1 + width * (self.keys.len() + 2) + size);
        out.push(1 | ((width as u8 - 1) << 6));
        push_le(&mut out, self.keys.len(), width);
        let mut offset = 0;
        for key in &self.keys {
            push_le(&mut out, offset, width);
            offset += key.len();
        }
        push_le(&mut out, offset, width);
        for key in &self.keys {
            out.extend_from_slice(key.as_bytes());
        }
        Ok(out)
    }
}

fn primitive_header(id: u8) -> u8 {
    id << 2
}

/// `VariantLogicalType`'s name of a scalar Parquet can't hold.
fn unconvertible(value: &Value) -> Error {
    use LogicalTypeId as T;
    let name = match value.logical_type().id() {
        T::Interval => "INTERVAL",
        T::Bignum => "BIGNUM",
        T::Bit => "BITSTRING",
        T::TimestampMs => "TIMESTAMP_MILIS",
        T::TimestampSec => "TIMESTAMP_SEC",
        T::TimeTz => "TIME_MICROS_TZ",
        T::TimeNs => "TIME_NANOS",
        T::TimestampTzNs => "TIMESTAMP_NANOS_TZ",
        other => other.name(),
    };
    invalid_input(format!("Can't convert VARIANT of type '{name}' to Parquet VARIANT"))
}

fn raw_i64(value: &Value) -> i64 {
    match value.data() {
        ValueData::Int64(v) => *v,
        ValueData::Int32(v) => i64::from(*v),
        _ => value.as_i128().unwrap_or(0) as i64,
    }
}

/// `WritePrimitiveValueData`.
fn encode_scalar(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    use LogicalTypeId as T;
    let ty = value.logical_type();
    match (ty.id(), value.data()) {
        (T::Boolean, ValueData::Bool(flag)) => out.push(primitive_header(if *flag { 1 } else { 2 })),
        (T::Varchar | T::Blob, ValueData::Bytes(bytes)) => {
            if ty.id() == T::Blob || bytes.len() >= 64 {
                out.push(primitive_header(if ty.id() == T::Blob { 15 } else { 16 }));
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            } else {
                out.push(1 | ((bytes.len() as u8) << 2));
            }
            out.extend_from_slice(bytes);
        }
        (T::TinyInt, ValueData::Int8(v)) => {
            out.push(primitive_header(3));
            out.push(*v as u8);
        }
        (T::SmallInt, ValueData::Int16(v)) => {
            out.push(primitive_header(4));
            out.extend_from_slice(&v.to_le_bytes());
        }
        (T::Integer, ValueData::Int32(v)) => {
            out.push(primitive_header(5));
            out.extend_from_slice(&v.to_le_bytes());
        }
        (T::BigInt, ValueData::Int64(v)) => {
            out.push(primitive_header(6));
            out.extend_from_slice(&v.to_le_bytes());
        }
        (T::Float, ValueData::Float(v)) => {
            out.push(primitive_header(14));
            out.extend_from_slice(&v.to_le_bytes());
        }
        (T::Double, ValueData::Double(v)) => {
            out.push(primitive_header(7));
            out.extend_from_slice(&v.to_le_bytes());
        }
        (T::Uuid, ValueData::Int128(stored)) => {
            out.push(primitive_header(20));
            out.extend_from_slice(&((*stored as u128) ^ (1 << 127)).to_be_bytes());
        }
        (T::Date, ValueData::Int32(days)) => {
            out.push(primitive_header(11));
            out.extend_from_slice(&days.to_le_bytes());
        }
        (T::Time, _) => {
            out.push(primitive_header(17));
            out.extend_from_slice(&raw_i64(value).to_le_bytes());
        }
        (T::Timestamp, _) => {
            out.push(primitive_header(13));
            out.extend_from_slice(&raw_i64(value).to_le_bytes());
        }
        (T::TimestampNs, _) => {
            out.push(primitive_header(19));
            out.extend_from_slice(&raw_i64(value).to_le_bytes());
        }
        (T::TimestampTz, _) => {
            out.push(primitive_header(12));
            out.extend_from_slice(&raw_i64(value).to_le_bytes());
        }
        (T::Decimal, _) => {
            let (width, scale) = ty.decimal_properties().unwrap_or((38, 0));
            let unscaled = value.as_i128().unwrap_or(0);
            if width <= 9 {
                out.push(primitive_header(8));
                out.push(scale);
                out.extend_from_slice(&(unscaled as i32).to_le_bytes());
            } else if width <= 18 {
                out.push(primitive_header(9));
                out.push(scale);
                out.extend_from_slice(&(unscaled as i64).to_le_bytes());
            } else {
                out.push(primitive_header(10));
                out.push(scale);
                out.extend_from_slice(&unscaled.to_le_bytes());
            }
        }
        (T::UTinyInt, ValueData::UInt8(v)) => {
            out.push(primitive_header(4));
            out.extend_from_slice(&i16::from(*v).to_le_bytes());
        }
        (T::USmallInt, ValueData::UInt16(v)) => {
            out.push(primitive_header(5));
            out.extend_from_slice(&i32::from(*v).to_le_bytes());
        }
        (T::UInteger | T::UBigInt | T::UHugeInt | T::HugeInt, _) => {
            let wide = match value.data() {
                ValueData::UInt128(v) => i128::try_from(*v).ok(),
                _ => value.as_i128(),
            };
            let Some(narrow) = wide.and_then(|v| i64::try_from(v).ok()) else {
                return Err(Error::new(
                    ErrorKind::Conversion,
                    format!(
                        "Type {} with value {} can't be cast because the value is out of range for the destination type INT64",
                        physical_name(ty.id()),
                        value.try_to_string()?
                    ),
                ));
            };
            out.push(primitive_header(6));
            out.extend_from_slice(&narrow.to_le_bytes());
        }
        _ => return Err(unconvertible(value)),
    }
    Ok(())
}

fn physical_name(id: LogicalTypeId) -> &'static str {
    match id {
        LogicalTypeId::UInteger => "UINT32",
        LogicalTypeId::UBigInt => "UINT64",
        LogicalTypeId::UHugeInt => "UINT128",
        _ => "INT128",
    }
}

/// The value of `node`, without the keys of `omit` at its top.
fn encode_node(node: &VariantNode, dictionary: &Dictionary, omit: &[&str], out: &mut Vec<u8>) -> Result<()> {
    match node {
        VariantNode::Null => out.push(primitive_header(0)),
        VariantNode::Scalar(value) => encode_scalar(value, out)?,
        VariantNode::Object(children) => {
            let children: Vec<_> = children.iter().filter(|(k, _)| !omit.contains(&k.as_str())).collect();
            let mut values = Vec::new();
            let mut offsets = Vec::with_capacity(children.len() + 1);
            let mut highest = 0;
            for (key, child) in &children {
                offsets.push(values.len());
                highest = highest.max(dictionary.index[key.as_str()]);
                encode_node(child, dictionary, &[], &mut values)?;
            }
            offsets.push(values.len());
            let (id_size, offset_size) = (byte_length(highest), byte_length(values.len()));
            let large = children.len() > 255;
            out.push(2 | (u8::from(large) << 6) | ((id_size as u8 - 1) << 4) | ((offset_size as u8 - 1) << 2));
            push_le(out, children.len(), if large { 4 } else { 1 });
            for (key, _) in &children {
                push_le(out, dictionary.index[key.as_str()], id_size);
            }
            for offset in offsets {
                push_le(out, offset, offset_size);
            }
            out.extend_from_slice(&values);
        }
        VariantNode::Array(items) => {
            let mut values = Vec::new();
            let mut offsets = Vec::with_capacity(items.len() + 1);
            for item in items {
                offsets.push(values.len());
                encode_node(item, dictionary, &[], &mut values)?;
            }
            offsets.push(values.len());
            let offset_size = byte_length(values.len());
            let large = items.len() > 255;
            out.push(3 | (u8::from(large) << 4) | ((offset_size as u8 - 1) << 2));
            push_le(out, items.len(), if large { 4 } else { 1 });
            for offset in offsets {
                push_le(out, offset, offset_size);
            }
            out.extend_from_slice(&values);
        }
    }
    Ok(())
}

/// `TransformTypedValueRecursive`: of a shredding type, the type of its `typed_value`.
pub fn typed_value_type(shredding: &LogicalType) -> Result<LogicalType> {
    use LogicalTypeId as T;
    let group = |child: &LogicalType| -> Result<LogicalType> {
        let mut fields = vec![("value".to_string(), LogicalType::BLOB)];
        if !matches!(child.id(), T::Variant | T::SqlNull) {
            fields.push(("typed_value".to_string(), typed_value_type(child)?));
        }
        Ok(LogicalType::struct_type(fields))
    };
    Ok(match shredding.id() {
        T::Struct => LogicalType::struct_type(
            shredding
                .struct_children()
                .iter()
                .map(|(name, child)| Ok((name.clone(), group(child)?)))
                .collect::<Result<_>>()?,
        ),
        T::List => LogicalType::list(group(shredding.child_type().expect("a LIST has a child"))?),
        T::Union | T::Map | T::Variant | T::Array => {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("'{shredding}' can't appear inside a 'typed_value' shredded type!"),
            ));
        }
        _ => shredding.clone(),
    })
}

/// `GetParquetVariantType`: the struct a VARIANT is written as, shredded on `shredding`.
pub fn parquet_variant_type(shredding: Option<&LogicalType>) -> Result<LogicalType> {
    let mut fields = vec![
        ("metadata".to_string(), LogicalType::BLOB),
        ("value".to_string(), LogicalType::BLOB),
    ];
    if let Some(shredding) = shredding.filter(|ty| ty.id() != LogicalTypeId::Variant) {
        fields.push(("typed_value".to_string(), typed_value_type(shredding)?));
    }
    Ok(LogicalType::struct_type(fields))
}

/// The physical width class of a DECIMAL of `width` digits.
fn decimal_class(width: u8) -> u8 {
    match width {
        0..=4 => 0,
        5..=9 => 1,
        10..=18 => 2,
        _ => 3,
    }
}

/// `ValueIsShredded`: whether `value` goes to a `typed_value` of `ty`.
fn scalar_is_shredded(value: &Value, ty: &LogicalType) -> bool {
    use LogicalTypeId as T;
    let id = value.logical_type().id();
    match ty.id() {
        T::Decimal => {
            let (Some((width, scale)), Some((target_width, target_scale))) =
                (value.logical_type().decimal_properties(), ty.decimal_properties())
            else {
                return false;
            };
            id == T::Decimal
                && decimal_class(width) == decimal_class(target_width)
                && scale == target_scale
                && width <= target_width
        }
        T::Boolean
        | T::TinyInt
        | T::SmallInt
        | T::Integer
        | T::BigInt
        | T::Float
        | T::Double
        | T::Date
        | T::Time
        | T::TimestampTz
        | T::TimestampTzNs
        | T::Timestamp
        | T::TimestampNs
        | T::Blob
        | T::Varchar
        | T::Uuid => id == ty.id(),
        _ => false,
    }
}

/// The `value` and `typed_value` of `node`, shredded on `shredding` (`None`: not at all), in a group of `ty`.
fn shred(
    node: &VariantNode,
    dictionary: &Dictionary,
    shredding: Option<&LogicalType>,
    ty: &LogicalType,
) -> Result<Value> {
    let typed_type = ty
        .struct_children()
        .iter()
        .find(|(name, _)| name == "typed_value")
        .map(|(_, ty)| ty.clone());
    let encode = |omit: &[&str]| -> Result<Value> {
        let mut out = Vec::new();
        encode_node(node, dictionary, omit, &mut out)?;
        Ok(Value::blob(out))
    };
    let (value, typed) = match (shredding.filter(|s| s.id() != LogicalTypeId::Variant), &typed_type) {
        (Some(shredding), Some(typed_type)) => match (node, shredding.id()) {
            (VariantNode::Object(children), LogicalTypeId::Struct) => {
                let fields = shredding.struct_children();
                let mut typed_fields = Vec::with_capacity(fields.len());
                for ((name, field_shredding), (_, group_type)) in fields.iter().zip(typed_type.struct_children()) {
                    typed_fields.push(match children.iter().find(|(k, _)| k == name) {
                        Some((_, child)) => shred(child, dictionary, Some(field_shredding), group_type)?,
                        None => missing_group(group_type)?,
                    });
                }
                let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
                let leftover = children.iter().any(|(k, _)| !names.contains(&k.as_str()));
                let value = if leftover || children.is_empty() {
                    encode(&names)?
                } else {
                    Value::null(LogicalType::BLOB)
                };
                (value, Value::new(typed_type.clone(), ValueData::Struct(typed_fields))?)
            }
            (VariantNode::Array(items), LogicalTypeId::List) => {
                let element = shredding.child_type().expect("a LIST has a child");
                let group_type = typed_type.child_type().expect("a LIST has a child");
                let items = items
                    .iter()
                    .map(|item| shred(item, dictionary, Some(element), group_type))
                    .collect::<Result<Vec<_>>>()?;
                (Value::null(LogicalType::BLOB), Value::list(group_type.clone(), items)?)
            }
            (VariantNode::Scalar(value), _) if scalar_is_shredded(value, shredding) => {
                let typed = match shredding.decimal_properties() {
                    Some((width, scale)) => Value::decimal(value.as_i128().unwrap_or(0), width, scale)?,
                    None => value.clone(),
                };
                (Value::null(LogicalType::BLOB), typed)
            }
            _ => (encode(&[])?, Value::null(typed_type.clone())),
        },
        _ => (encode(&[])?, Value::null(LogicalType::BLOB)),
    };
    let fields = ty
        .struct_children()
        .iter()
        .map(|(name, _)| if name == "value" { value.clone() } else { typed.clone() })
        .collect();
    Value::new(ty.clone(), ValueData::Struct(fields))
}

/// A field a row's object doesn't have: no `value` and no `typed_value`.
fn missing_group(ty: &LogicalType) -> Result<Value> {
    let fields = ty
        .struct_children()
        .iter()
        .map(|(_, child)| Value::null(child.clone()))
        .collect();
    Value::new(ty.clone(), ValueData::Struct(fields))
}

/// `ToParquetVariant` of one VARIANT, as a `ty` of [`parquet_variant_type`] shredded on `shredding`.
pub fn to_parquet_variant(value: &Value, shredding: Option<&LogicalType>, ty: &LogicalType) -> Result<Value> {
    let node = match value.data() {
        ValueData::Bytes(bytes) if !value.is_null() => VariantNode::decode(bytes)?,
        _ => VariantNode::Null,
    };
    let dictionary = Dictionary::collect(&node);
    let metadata = Value::blob(dictionary.metadata()?);
    let mut group_fields: Vec<(String, LogicalType)> = ty.struct_children().to_vec();
    group_fields.remove(0);
    let group_type = LogicalType::struct_type(group_fields);
    let group = shred(&node, &dictionary, shredding, &group_type)?;
    let ValueData::Struct(fields) = group.data() else {
        unreachable!("a group is a STRUCT");
    };
    let mut fields = fields.clone();
    fields.insert(0, metadata);
    Value::new(ty.clone(), ValueData::Struct(fields))
}

/// The kinds of values `AnalyzeSchema` counts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Null,
    Bool,
    Scalar(LogicalTypeId),
    Decimal,
    Object,
    Array,
}

#[derive(Default)]
struct Analysis {
    counts: std::collections::HashMap<Kind, usize>,
    total: usize,
    decimal: Option<(u8, u8)>,
    decimal_consistent: bool,
    /// in the order they come up, matched without case
    fields: Vec<(String, Analysis)>,
    element: Option<Box<Analysis>>,
}

const MAX_SHREDDING_DEPTH: usize = 32;

impl Analysis {
    fn count(&self, kind: Kind) -> usize {
        self.counts.get(&kind).copied().unwrap_or(0)
    }

    /// `AnalyzeSchemaInternal`.
    fn add(&mut self, node: &VariantNode, depth: usize) {
        self.total += 1;
        let kind = match node {
            VariantNode::Null => Kind::Null,
            VariantNode::Object(_) => Kind::Object,
            VariantNode::Array(_) => Kind::Array,
            VariantNode::Scalar(value) => match value.logical_type().id() {
                LogicalTypeId::Boolean => Kind::Bool,
                LogicalTypeId::Decimal => Kind::Decimal,
                id => Kind::Scalar(id),
            },
        };
        *self.counts.entry(kind).or_default() += 1;
        match node {
            VariantNode::Object(_) | VariantNode::Array(_) if depth >= MAX_SHREDDING_DEPTH => {}
            VariantNode::Object(children) => {
                for (key, child) in children {
                    let index = match self.fields.iter().position(|(k, _)| k.eq_ignore_ascii_case(key)) {
                        Some(index) => index,
                        None => {
                            self.fields.push((key.clone(), Analysis::default()));
                            self.fields.len() - 1
                        }
                    };
                    self.fields[index].1.add(child, depth + 1);
                }
            }
            VariantNode::Array(items) => {
                let element = self.element.get_or_insert_with(Default::default);
                for item in items {
                    element.add(item, depth + 1);
                }
            }
            VariantNode::Scalar(value) if kind == Kind::Decimal => {
                let properties = value.logical_type().decimal_properties();
                if self.count(Kind::Decimal) == 1 {
                    self.decimal = properties;
                    self.decimal_consistent = true;
                } else if self.decimal_consistent && self.decimal != properties {
                    self.decimal_consistent = false;
                }
            }
            _ => {}
        }
    }

    /// `ConstructShreddedType`.
    fn shredded_type(&self, depth: usize) -> Option<LogicalType> {
        use LogicalTypeId as T;
        if self.count(Kind::Null) == self.total {
            return Some(LogicalType::INTEGER);
        }
        let candidates: [(Kind, T); 23] = [
            (Kind::Bool, T::Boolean),
            (Kind::Scalar(T::TinyInt), T::TinyInt),
            (Kind::Scalar(T::SmallInt), T::SmallInt),
            (Kind::Scalar(T::Integer), T::Integer),
            (Kind::Scalar(T::BigInt), T::BigInt),
            (Kind::Scalar(T::Float), T::Float),
            (Kind::Scalar(T::Double), T::Double),
            (Kind::Decimal, T::Decimal),
            (Kind::Scalar(T::Date), T::Date),
            (Kind::Scalar(T::Time), T::Time),
            (Kind::Scalar(T::Timestamp), T::Timestamp),
            (Kind::Scalar(T::TimestampNs), T::TimestampNs),
            (Kind::Scalar(T::TimestampTz), T::TimestampTz),
            (Kind::Scalar(T::TimestampTzNs), T::TimestampTzNs),
            (Kind::Scalar(T::Blob), T::Blob),
            (Kind::Scalar(T::Varchar), T::Varchar),
            (Kind::Scalar(T::Uuid), T::Uuid),
            (Kind::Scalar(T::UTinyInt), T::SmallInt),
            (Kind::Scalar(T::USmallInt), T::Integer),
            (Kind::Scalar(T::UInteger), T::BigInt),
            (Kind::Scalar(T::UBigInt), T::BigInt),
            (Kind::Scalar(T::UHugeInt), T::BigInt),
            (Kind::Scalar(T::HugeInt), T::BigInt),
        ];
        let mut highest = 0;
        let mut best = None;
        for (kind, target) in candidates {
            let count = self.count(kind);
            if count <= highest {
                continue;
            }
            if kind == Kind::Decimal {
                if !self.decimal_consistent {
                    continue;
                }
                let (width, scale) = self.decimal.unwrap_or((18, 3));
                highest = count;
                best = Some(LogicalType::decimal(width, scale));
            } else {
                highest = count;
                best = Some(LogicalType::new(target));
            }
        }
        let (arrays, objects) = (self.count(Kind::Array), self.count(Kind::Object));
        if arrays > objects {
            if arrays > highest {
                if depth >= MAX_SHREDDING_DEPTH {
                    return Some(LogicalType::new(T::Variant));
                }
                let element = self.element.as_ref()?.shredded_type(depth + 1)?;
                return Some(LogicalType::list(element));
            }
        } else if objects > highest {
            if depth >= MAX_SHREDDING_DEPTH {
                return Some(LogicalType::new(T::Variant));
            }
            let fields: Vec<(String, LogicalType)> = self
                .fields
                .iter()
                .filter(|(name, _)| !name.is_empty())
                .filter_map(|(name, field)| Some((name.clone(), field.shredded_type(depth + 1)?)))
                .collect();
            if fields.is_empty() {
                return None;
            }
            return Some(LogicalType::struct_type(fields));
        }
        best
    }
}

/// `AnalyzeSchema` and `AnalyzeSchemaFinalize`: the type to shred VARIANTs like `values` on, if any.
pub fn analyze_shredding(values: &[Value]) -> Result<Option<LogicalType>> {
    let mut analysis = Analysis::default();
    for value in values {
        let node = match value.data() {
            ValueData::Bytes(bytes) if !value.is_null() => VariantNode::decode(bytes)?,
            _ => VariantNode::Null,
        };
        analysis.add(&node, 0);
    }
    Ok(analysis.shredded_type(0))
}
