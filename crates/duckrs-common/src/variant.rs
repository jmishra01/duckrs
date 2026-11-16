// Ported from duckdb/src/function/cast/variant/to_variant.cpp and
// duckdb/src/function/scalar/variant/variant_typeof.cpp @ rust-port-baseline
//
//! VARIANT values. DuckDB stores them in a nested columnar layout, with shredding. Here a value
//! is a self-contained byte string: this is never persisted yet, and DuckDB's layout belongs with
//! the storage work of Phase 9.

use crate::string_cast::escape_nested_string;
use crate::types::{LogicalType, LogicalTypeId};
use crate::value::{Value, ValueData};
use crate::{Error, ErrorKind, Result};

/// A VARIANT value, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum VariantNode {
    Null,
    Scalar(Value),
    /// sorted by key
    Object(Vec<(String, VariantNode)>),
    Array(Vec<VariantNode>),
}

/// Dropping a deeply nested value one level at a time, as the drop glue's recursion would overflow
/// the stack.
impl Drop for VariantNode {
    fn drop(&mut self) {
        let mut pending: Vec<VariantNode> = match self {
            Self::Object(children) if children.iter().any(|(_, c)| c.is_nested()) => {
                std::mem::take(children).into_iter().map(|(_, c)| c).collect()
            }
            Self::Array(items) if items.iter().any(VariantNode::is_nested) => std::mem::take(items),
            _ => return,
        };
        while let Some(mut node) = pending.pop() {
            match &mut node {
                Self::Object(children) => pending.extend(std::mem::take(children).into_iter().map(|(_, c)| c)),
                Self::Array(items) => pending.append(items),
                _ => {}
            }
        }
    }
}

impl VariantNode {
    fn is_nested(&self) -> bool {
        matches!(self, Self::Object(_) | Self::Array(_))
    }
}

const NULL_TAG: u8 = 0;
const OBJECT_TAG: u8 = 1;
const ARRAY_TAG: u8 = 2;
const SCALAR_TAG: u8 = 3;

/// The scalar types a VARIANT holds, by the code they are stored with. DECIMAL has its own code.
const SCALAR_TYPES: [LogicalTypeId; 30] = {
    use LogicalTypeId::*;
    [
        Boolean,
        TinyInt,
        SmallInt,
        Integer,
        BigInt,
        HugeInt,
        UTinyInt,
        USmallInt,
        UInteger,
        UBigInt,
        UHugeInt,
        Float,
        Double,
        Varchar,
        Blob,
        Uuid,
        Date,
        Time,
        TimeNs,
        TimeTz,
        TimestampSec,
        TimestampMs,
        Timestamp,
        TimestampNs,
        TimestampTz,
        TimestampTzNs,
        Interval,
        Bignum,
        Bit,
        // `VariantLogicalType::GEOMETRY`: the geometry's blob
        Geometry,
    ]
};
const DECIMAL_CODE: u8 = 255;

impl VariantNode {
    /// `ConvertToVariant`.
    pub fn from_value(value: &Value) -> Result<Self> {
        Self::from_value_with(value, &|_| None)
    }

    /// `from_value`, where `leaf` converts the values it knows (a JSON, which is parsed) itself.
    pub fn from_value_with(value: &Value, leaf: &dyn Fn(&Value) -> Option<Result<Self>>) -> Result<Self> {
        if value.is_null() {
            return Ok(Self::Null);
        }
        if let Some(node) = leaf(value) {
            return node;
        }
        let from_value = |v: &Value| Self::from_value_with(v, leaf);
        let ty = value.logical_type();
        Ok(match (ty.id(), value.data()) {
            (LogicalTypeId::Variant, ValueData::Bytes(bytes)) => Self::decode(bytes)?,
            (LogicalTypeId::Union, ValueData::Struct(fields)) => match fields[0].as_i128() {
                Some(tag) => from_value(&fields[tag as usize + 1])?,
                None => Self::Null,
            },
            (LogicalTypeId::Tuple, ValueData::Struct(fields)) => {
                Self::Array(fields.iter().map(from_value).collect::<Result<_>>()?)
            }
            (_, ValueData::Struct(fields)) => {
                let mut children = Vec::with_capacity(fields.len());
                for ((name, _), field) in ty.struct_children().iter().zip(fields) {
                    children.push((name.clone(), from_value(field)?));
                }
                children.sort_by(|a, b| a.0.cmp(&b.0));
                Self::Object(children)
            }
            (_, ValueData::List(items)) => Self::Array(items.iter().map(from_value).collect::<Result<_>>()?),
            (LogicalTypeId::Enum, _) => Self::Scalar(Value::varchar(value.try_to_string()?)),
            _ => Self::Scalar(value.clone()),
        })
    }

    /// `VariantLogicalType`'s name for what is at the top of this value.
    fn scalar_name(value: &Value) -> String {
        use LogicalTypeId::*;
        let ty = value.logical_type();
        match ty.id() {
            Boolean => if *value == Value::boolean(true) {
                "BOOL_TRUE"
            } else {
                "BOOL_FALSE"
            }
            .to_string(),
            Decimal => {
                let (width, scale) = ty.decimal_properties().expect("DECIMAL has properties");
                format!("DECIMAL({width}, {scale})")
            }
            Time => "TIME_MICROS".to_string(),
            TimeNs => "TIME_NANOS".to_string(),
            TimeTz => "TIME_MICROS_TZ".to_string(),
            TimestampSec => "TIMESTAMP_SEC".to_string(),
            TimestampMs => "TIMESTAMP_MILIS".to_string(),
            Timestamp => "TIMESTAMP_MICROS".to_string(),
            TimestampNs => "TIMESTAMP_NANOS".to_string(),
            TimestampTz => "TIMESTAMP_MICROS_TZ".to_string(),
            TimestampTzNs => "TIMESTAMP_NANOS_TZ".to_string(),
            Bit => "BITSTRING".to_string(),
            TinyInt | SmallInt | Integer | BigInt | HugeInt | UTinyInt | USmallInt | UInteger | UBigInt | UHugeInt => {
                ty.physical_type().to_string()
            }
            _ => ty.to_string(),
        }
    }

    /// The name of the type at the top, as errors show it (`EnumUtil::ToString`).
    pub fn type_name(&self) -> String {
        match self {
            Self::Null => "VARIANT_NULL".to_string(),
            Self::Scalar(value) => Self::scalar_name(value),
            Self::Object(_) => "OBJECT".to_string(),
            Self::Array(_) => "ARRAY".to_string(),
        }
    }

    /// `variant_typeof`.
    pub fn describe(&self) -> String {
        match self {
            Self::Object(children) => {
                let keys: Vec<&str> = children.iter().map(|(k, _)| k.as_str()).collect();
                format!("OBJECT({})", keys.join(", "))
            }
            Self::Array(items) => format!("ARRAY({})", items.len()),
            other => other.type_name(),
        }
    }

    /// `CAST(variant AS VARCHAR)`.
    pub fn render(&self) -> Result<String> {
        self.render_at(0)
    }

    /// `VariantVisitor::CheckDepth`: a value this deep is refused.
    fn render_at(&self, depth: usize) -> Result<String> {
        const MAX_RECURSION_DEPTH: usize = 128;
        if depth >= MAX_RECURSION_DEPTH {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("VARIANT exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"),
            ));
        }
        match self {
            Self::Null => Ok("NULL".to_string()),
            Self::Scalar(value) => value.try_to_string(),
            Self::Object(children) => {
                let mut entries = Vec::with_capacity(children.len());
                for (key, child) in children {
                    entries.push(format!(
                        "{}: {}",
                        escape_nested_string(key, true),
                        child.render_child_at(depth + 1)?
                    ));
                }
                Ok(format!("{{{}}}", entries.join(", ")))
            }
            Self::Array(items) => {
                let items = items
                    .iter()
                    .map(|item| item.render_child_at(depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("[{}]", items.join(", ")))
            }
        }
    }

    /// `render` inside of a nested value: strings are escaped where they need it.
    pub fn render_child(&self) -> Result<String> {
        self.render_child_at(0)
    }

    fn render_child_at(&self, depth: usize) -> Result<String> {
        match self {
            Self::Scalar(value) if depth < 128 => Ok(escape_nested_string(&value.try_to_string()?, false)),
            other => other.render_at(depth),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    /// The values in order, nested ones from a stack of their own rather than by recursion.
    fn write(&self, out: &mut Vec<u8>) {
        enum Next<'a> {
            Node(&'a VariantNode),
            Key(&'a str),
        }
        let write_len = |out: &mut Vec<u8>, length: usize| out.extend_from_slice(&(length as u32).to_le_bytes());
        let mut pending = vec![Next::Node(self)];
        while let Some(next) = pending.pop() {
            match next {
                Next::Key(key) => {
                    write_len(out, key.len());
                    out.extend_from_slice(key.as_bytes());
                }
                Next::Node(Self::Object(children)) => {
                    out.push(OBJECT_TAG);
                    write_len(out, children.len());
                    for (key, child) in children.iter().rev() {
                        pending.push(Next::Node(child));
                        pending.push(Next::Key(key));
                    }
                }
                Next::Node(Self::Array(items)) => {
                    out.push(ARRAY_TAG);
                    write_len(out, items.len());
                    pending.extend(items.iter().rev().map(Next::Node));
                }
                Next::Node(other) => other.write_scalar(out),
            }
        }
    }

    fn write_scalar(&self, out: &mut Vec<u8>) {
        let write_len = |out: &mut Vec<u8>, length: usize| out.extend_from_slice(&(length as u32).to_le_bytes());
        match self {
            Self::Null => out.push(NULL_TAG),
            Self::Object(_) | Self::Array(_) => unreachable!("written by write"),
            Self::Scalar(value) => {
                out.push(SCALAR_TAG);
                let ty = value.logical_type();
                if let Some((width, scale)) = ty.decimal_properties().filter(|_| ty.id() == LogicalTypeId::Decimal) {
                    out.extend_from_slice(&[DECIMAL_CODE, width, scale]);
                    // the unscaled integer, whatever its width
                    out.extend_from_slice(&value.as_i128().expect("a DECIMAL value").to_le_bytes());
                    return;
                }
                let code = SCALAR_TYPES
                    .iter()
                    .position(|id| *id == ty.id())
                    .expect("a scalar type a VARIANT holds");
                out.push(code as u8);
                match value.data() {
                    ValueData::Bool(v) => out.push(u8::from(*v)),
                    ValueData::Int8(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Int16(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Int32(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Int64(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Int128(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::UInt8(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::UInt16(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::UInt32(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::UInt64(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::UInt128(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
                    ValueData::Interval(v) => {
                        out.extend_from_slice(&v.months.to_le_bytes());
                        out.extend_from_slice(&v.days.to_le_bytes());
                        out.extend_from_slice(&v.micros.to_le_bytes());
                    }
                    ValueData::Bytes(bytes) => {
                        write_len(out, bytes.len());
                        out.extend_from_slice(bytes);
                    }
                    other => unreachable!("{other:?} is not a scalar"),
                }
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader { bytes, pos: 0 };
        reader.node()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.slice(N)?;
        Ok(slice.try_into().expect("N bytes"))
    }

    fn slice(&mut self, size: usize) -> Result<&[u8]> {
        let end = self.pos.checked_add(size).filter(|e| *e <= self.bytes.len());
        let Some(end) = end else {
            return Err(Error::new(ErrorKind::Internal, "truncated VARIANT value"));
        };
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn length(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.take::<4>()?) as usize)
    }

    /// One value, its nested ones read with a stack of their own rather than by recursion.
    fn node(&mut self) -> Result<VariantNode> {
        // an object or array being read: what it has so far and how many more children it has
        enum Open {
            Object(Vec<(String, VariantNode)>, usize, String),
            Array(Vec<VariantNode>, usize),
        }
        let mut open: Vec<Open> = Vec::new();
        loop {
            // the key of the next child of an object comes before it
            if let Some(Open::Object(children, remaining, key)) = open.last_mut()
                && *remaining > children.len()
            {
                let length = self.length()?;
                *key = String::from_utf8_lossy(self.slice(length)?).into_owned();
            }
            let [tag] = self.take::<1>()?;
            let mut done = match tag {
                NULL_TAG => VariantNode::Null,
                OBJECT_TAG => match self.length()? {
                    0 => VariantNode::Object(Vec::new()),
                    size => {
                        open.push(Open::Object(Vec::with_capacity(size.min(1024)), size, String::new()));
                        continue;
                    }
                },
                ARRAY_TAG => match self.length()? {
                    0 => VariantNode::Array(Vec::new()),
                    size => {
                        open.push(Open::Array(Vec::with_capacity(size.min(1024)), size));
                        continue;
                    }
                },
                _ => VariantNode::Scalar(self.scalar()?),
            };
            // hand the finished value to its parent, closing every parent it completes
            loop {
                match open.last_mut() {
                    None => return Ok(done),
                    Some(Open::Object(children, size, key)) => {
                        children.push((std::mem::take(key), done));
                        if children.len() < *size {
                            break;
                        }
                    }
                    Some(Open::Array(items, size)) => {
                        items.push(done);
                        if items.len() < *size {
                            break;
                        }
                    }
                }
                done = match open.pop().expect("checked above") {
                    Open::Object(children, ..) => VariantNode::Object(children),
                    Open::Array(items, _) => VariantNode::Array(items),
                };
            }
        }
    }

    fn scalar(&mut self) -> Result<Value> {
        use LogicalTypeId::*;
        let [code] = self.take::<1>()?;
        if code == DECIMAL_CODE {
            let [width, scale] = self.take::<2>()?;
            return Value::decimal(i128::from_le_bytes(self.take::<16>()?), width, scale);
        }
        let id = SCALAR_TYPES[code as usize];
        let ty = LogicalType::new(id);
        let data = match ty.physical_type() {
            crate::types::PhysicalType::Bool => ValueData::Bool(self.take::<1>()?[0] != 0),
            crate::types::PhysicalType::Int8 => ValueData::Int8(i8::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Int16 => ValueData::Int16(i16::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Int32 => ValueData::Int32(i32::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Int64 => ValueData::Int64(i64::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Int128 => ValueData::Int128(i128::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::UInt8 => ValueData::UInt8(u8::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::UInt16 => ValueData::UInt16(u16::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::UInt32 => ValueData::UInt32(u32::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::UInt64 => ValueData::UInt64(u64::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::UInt128 => ValueData::UInt128(u128::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Float => ValueData::Float(f32::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Double => ValueData::Double(f64::from_le_bytes(self.take()?)),
            crate::types::PhysicalType::Interval => {
                let months = i32::from_le_bytes(self.take()?);
                let days = i32::from_le_bytes(self.take()?);
                let micros = i64::from_le_bytes(self.take()?);
                ValueData::Interval(crate::types::Interval::new(months, days, micros))
            }
            _ => {
                let length = self.length()?;
                ValueData::Bytes(self.slice(length)?.to_vec())
            }
        };
        debug_assert!(!matches!(id, Decimal));
        Value::new(ty, data)
    }
}

/// `VariantLogicalType`: how DuckDB's physical layout tags a value.
mod physical_type {
    pub const NULL: u8 = 0;
    pub const BOOL_TRUE: u8 = 1;
    pub const BOOL_FALSE: u8 = 2;
    pub const DECIMAL: u8 = 15;
    pub const OBJECT: u8 = 29;
    pub const ARRAY: u8 = 30;
}

/// The scalar types of `VariantLogicalType`, by their tag; `None` where the tag is no plain scalar.
fn scalar_type_of_tag(tag: u8) -> Option<LogicalTypeId> {
    use LogicalTypeId::*;
    Some(match tag {
        3 => TinyInt,
        4 => SmallInt,
        5 => Integer,
        6 => BigInt,
        7 => HugeInt,
        8 => UTinyInt,
        9 => USmallInt,
        10 => UInteger,
        11 => UBigInt,
        12 => UHugeInt,
        13 => Float,
        14 => Double,
        16 => Varchar,
        17 => Blob,
        18 => Uuid,
        19 => Date,
        20 => Time,
        21 => TimeNs,
        22 => TimestampSec,
        23 => TimestampMs,
        24 => Timestamp,
        25 => TimestampNs,
        26 => TimeTz,
        27 => TimestampTz,
        28 => Interval,
        31 => Bignum,
        32 => Bit,
        34 => TimestampTzNs,
        _ => return None,
    })
}

fn tag_of_scalar_type(id: LogicalTypeId) -> Option<u8> {
    (0..=34u8).find(|tag| scalar_type_of_tag(*tag) == Some(id))
}

/// `VarintEncode`: 7 bits at a time, the low ones first.
fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at).ok_or_else(corrupt_variant)?;
        *at += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(corrupt_variant());
        }
    }
}

fn corrupt_variant() -> Error {
    Error::new(
        ErrorKind::Serialization,
        "Corrupted VARIANT value: the data is out of range",
    )
}

/// The type DuckDB stores a VARIANT as (`VariantShredding::GetUnshreddedType`): per row its keys, the children
/// of its objects and arrays, its values, and their data.
pub fn unshredded_type() -> LogicalType {
    LogicalType::struct_type(vec![
        ("keys".to_string(), LogicalType::list(LogicalType::VARCHAR)),
        (
            "children".to_string(),
            LogicalType::list(LogicalType::struct_type(vec![
                ("keys_index".to_string(), LogicalType::UINTEGER),
                ("values_index".to_string(), LogicalType::UINTEGER),
            ])),
        ),
        (
            "values".to_string(),
            LogicalType::list(LogicalType::struct_type(vec![
                ("type_id".to_string(), LogicalType::UTINYINT),
                ("byte_offset".to_string(), LogicalType::UINTEGER),
            ])),
        ),
        ("data".to_string(), LogicalType::BLOB),
    ])
}

/// What one row of a VARIANT holds in DuckDB's layout.
#[derive(Debug, Default)]
struct Physical {
    keys: Vec<String>,
    children: Vec<(Option<u32>, u32)>,
    values: Vec<(u8, u32)>,
    data: Vec<u8>,
}

impl Physical {
    fn emit(&mut self, node: &VariantNode) -> Result<u32> {
        let index = self.values.len() as u32;
        self.values.push((physical_type::NULL, self.data.len() as u32));
        let offset = self.data.len() as u32;
        let tag = match node {
            VariantNode::Null => physical_type::NULL,
            VariantNode::Array(items) => {
                self.container(items.iter().map(|item| (None, item)).collect())?;
                physical_type::ARRAY
            }
            VariantNode::Object(entries) => {
                let mut children = Vec::with_capacity(entries.len());
                for (key, child) in entries {
                    let key_index = self.keys.binary_search(key).map_err(|_| corrupt_variant())? as u32;
                    children.push((Some(key_index), child));
                }
                self.container(children)?;
                physical_type::OBJECT
            }
            VariantNode::Scalar(value) => self.scalar(value)?,
        };
        self.values[index as usize] = (tag, offset);
        Ok(index)
    }

    /// `WriteContainerData`: how many children, and where they start.
    fn container(&mut self, children: Vec<(Option<u32>, &VariantNode)>) -> Result<()> {
        let start = self.children.len();
        varint(&mut self.data, children.len() as u64);
        if !children.is_empty() {
            varint(&mut self.data, start as u64);
        }
        self.children.extend(children.iter().map(|(key, _)| (*key, 0)));
        for (i, (_, child)) in children.into_iter().enumerate() {
            let value = self.emit(child)?;
            self.children[start + i].1 = value;
        }
        Ok(())
    }

    fn scalar(&mut self, value: &Value) -> Result<u8> {
        let ty = value.logical_type();
        let out = &mut self.data;
        if ty.id() == LogicalTypeId::Boolean {
            return Ok(if *value == Value::boolean(true) {
                physical_type::BOOL_TRUE
            } else {
                physical_type::BOOL_FALSE
            });
        }
        if ty.id() == LogicalTypeId::Decimal {
            let (width, scale) = ty.decimal_properties().expect("DECIMAL has properties");
            varint(out, u64::from(width));
            varint(out, u64::from(scale));
            let unscaled = value.as_i128().unwrap_or(0);
            match width {
                0..=4 => out.extend_from_slice(&(unscaled as i16).to_le_bytes()),
                5..=9 => out.extend_from_slice(&(unscaled as i32).to_le_bytes()),
                10..=18 => out.extend_from_slice(&(unscaled as i64).to_le_bytes()),
                _ => out.extend_from_slice(&unscaled.to_le_bytes()),
            }
            return Ok(physical_type::DECIMAL);
        }
        let tag = tag_of_scalar_type(ty.id()).ok_or_else(|| {
            Error::new(
                ErrorKind::NotImplemented,
                format!("storing {ty} in a VARIANT is not yet ported to duckrs"),
            )
        })?;
        match value.data() {
            ValueData::Int8(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Int16(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Int32(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Int64(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Int128(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::UInt8(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::UInt16(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::UInt32(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::UInt64(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::UInt128(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
            ValueData::Interval(v) => {
                out.extend_from_slice(&v.months.to_le_bytes());
                out.extend_from_slice(&v.days.to_le_bytes());
                out.extend_from_slice(&v.micros.to_le_bytes());
            }
            ValueData::Bytes(bytes) => {
                varint(out, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
            _ => return Err(corrupt_variant()),
        }
        Ok(tag)
    }

    fn node(&self, value: u32, depth: usize) -> Result<VariantNode> {
        if depth > 1000 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "VARIANT value is nested too deeply",
            ));
        }
        let &(tag, offset) = self.values.get(value as usize).ok_or_else(corrupt_variant)?;
        let mut at = offset as usize;
        let data = &self.data;
        let take = |at: &mut usize, size: usize| -> Result<&[u8]> {
            let slice = data.get(*at..*at + size).ok_or_else(corrupt_variant)?;
            *at += size;
            Ok(slice)
        };
        Ok(match tag {
            physical_type::NULL => VariantNode::Null,
            physical_type::BOOL_TRUE => VariantNode::Scalar(Value::boolean(true)),
            physical_type::BOOL_FALSE => VariantNode::Scalar(Value::boolean(false)),
            physical_type::ARRAY | physical_type::OBJECT => {
                let count = read_varint(data, &mut at)? as usize;
                let start = if count > 0 {
                    read_varint(data, &mut at)? as usize
                } else {
                    0
                };
                let mut array = Vec::new();
                let mut object = Vec::new();
                for i in 0..count {
                    let &(key, child) = self.children.get(start + i).ok_or_else(corrupt_variant)?;
                    let child = self.node(child, depth + 1)?;
                    if tag == physical_type::ARRAY {
                        array.push(child);
                    } else {
                        let key = key
                            .and_then(|key| self.keys.get(key as usize))
                            .ok_or_else(corrupt_variant)?;
                        object.push((key.clone(), child));
                    }
                }
                if tag == physical_type::ARRAY {
                    VariantNode::Array(array)
                } else {
                    object.sort_by(|a, b| a.0.cmp(&b.0));
                    VariantNode::Object(object)
                }
            }
            physical_type::DECIMAL => {
                let width = read_varint(data, &mut at)? as u8;
                let scale = read_varint(data, &mut at)? as u8;
                let unscaled = match width {
                    0..=4 => i128::from(i16::from_le_bytes(take(&mut at, 2)?.try_into().expect("2 bytes"))),
                    5..=9 => i128::from(i32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes"))),
                    10..=18 => i128::from(i64::from_le_bytes(take(&mut at, 8)?.try_into().expect("8 bytes"))),
                    _ => i128::from_le_bytes(take(&mut at, 16)?.try_into().expect("16 bytes")),
                };
                VariantNode::Scalar(Value::decimal(unscaled, width, scale)?)
            }
            tag => {
                let id = scalar_type_of_tag(tag).ok_or_else(|| {
                    Error::new(
                        ErrorKind::NotImplemented,
                        format!("VARIANT values of type {tag} are not yet ported to duckrs"),
                    )
                })?;
                let ty = LogicalType::new(id);
                use crate::types::PhysicalType as P;
                let data = match ty.physical_type() {
                    P::Int8 => ValueData::Int8(i8::from_le_bytes(take(&mut at, 1)?.try_into().expect("1"))),
                    P::Int16 => ValueData::Int16(i16::from_le_bytes(take(&mut at, 2)?.try_into().expect("2"))),
                    P::Int32 => ValueData::Int32(i32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4"))),
                    P::Int64 => ValueData::Int64(i64::from_le_bytes(take(&mut at, 8)?.try_into().expect("8"))),
                    P::Int128 => ValueData::Int128(i128::from_le_bytes(take(&mut at, 16)?.try_into().expect("16"))),
                    P::UInt8 => ValueData::UInt8(take(&mut at, 1)?[0]),
                    P::UInt16 => ValueData::UInt16(u16::from_le_bytes(take(&mut at, 2)?.try_into().expect("2"))),
                    P::UInt32 => ValueData::UInt32(u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4"))),
                    P::UInt64 => ValueData::UInt64(u64::from_le_bytes(take(&mut at, 8)?.try_into().expect("8"))),
                    P::UInt128 => ValueData::UInt128(u128::from_le_bytes(take(&mut at, 16)?.try_into().expect("16"))),
                    P::Float => ValueData::Float(f32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4"))),
                    P::Double => ValueData::Double(f64::from_le_bytes(take(&mut at, 8)?.try_into().expect("8"))),
                    P::Interval => {
                        let raw = take(&mut at, 16)?;
                        let months = i32::from_le_bytes(raw[0..4].try_into().expect("4"));
                        let days = i32::from_le_bytes(raw[4..8].try_into().expect("4"));
                        let micros = i64::from_le_bytes(raw[8..16].try_into().expect("8"));
                        ValueData::Interval(crate::types::Interval::new(months, days, micros))
                    }
                    _ => {
                        let length = read_varint(data, &mut at)? as usize;
                        ValueData::Bytes(take(&mut at, length)?.to_vec())
                    }
                };
                VariantNode::Scalar(Value::new(ty, data)?)
            }
        })
    }
}

impl VariantNode {
    fn collect_keys(&self, keys: &mut std::collections::BTreeSet<String>) {
        match self {
            Self::Object(entries) => {
                for (key, child) in entries {
                    keys.insert(key.clone());
                    child.collect_keys(keys);
                }
            }
            Self::Array(items) => items.iter().for_each(|item| item.collect_keys(keys)),
            _ => {}
        }
    }

    /// The row of [`unshredded_type`] that DuckDB stores this value as. A NULL at the top is a NULL row.
    pub fn to_unshredded(&self) -> Result<Value> {
        let ty = unshredded_type();
        if *self == Self::Null {
            return Ok(Value::null(ty));
        }
        let mut keys = std::collections::BTreeSet::new();
        self.collect_keys(&mut keys);
        let mut physical = Physical {
            keys: keys.into_iter().collect(),
            ..Physical::default()
        };
        physical.emit(self)?;
        let fields = ty.struct_children();
        let child_of = |at: usize| fields[at].1.child_type().cloned().expect("a list");
        let keys = Value::list(
            LogicalType::VARCHAR,
            physical.keys.iter().map(|k| Value::varchar(k.as_str())).collect(),
        )?;
        let children_type = child_of(1);
        let children = physical
            .children
            .iter()
            .map(|(key, value)| {
                Value::new(
                    children_type.clone(),
                    ValueData::Struct(vec![
                        key.map_or_else(|| Value::null(LogicalType::UINTEGER), Value::uinteger),
                        Value::uinteger(*value),
                    ]),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let values_type = child_of(2);
        let values = physical
            .values
            .iter()
            .map(|(tag, offset)| {
                Value::new(
                    values_type.clone(),
                    ValueData::Struct(vec![Value::utinyint(*tag), Value::uinteger(*offset)]),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Value::new(
            ty,
            ValueData::Struct(vec![
                keys,
                Value::list(children_type, children)?,
                Value::list(values_type, values)?,
                Value::blob(physical.data.as_slice()),
            ]),
        )
    }

    /// A row of [`unshredded_type`] back as a value; a NULL row is a NULL.
    pub fn from_unshredded(row: &Value) -> Result<Self> {
        let ValueData::Struct(fields) = row.data() else {
            return Ok(Self::Null);
        };
        let list = |at: usize| -> Result<&[Value]> {
            match fields.get(at).map(Value::data) {
                Some(ValueData::List(items)) => Ok(items),
                _ => Err(corrupt_variant()),
            }
        };
        let u32_of = |value: &Value| -> Option<u32> { value.as_i128().map(|v| v as u32) };
        let pair = |value: &Value| -> Result<(Option<u32>, u32)> {
            let ValueData::Struct(parts) = value.data() else {
                return Err(corrupt_variant());
            };
            Ok((u32_of(&parts[0]), u32_of(&parts[1]).ok_or_else(corrupt_variant)?))
        };
        let physical = Physical {
            keys: list(0)?.iter().map(Value::try_to_string).collect::<Result<_>>()?,
            children: list(1)?.iter().map(pair).collect::<Result<_>>()?,
            values: list(2)?
                .iter()
                .map(|value| pair(value).map(|(tag, offset)| (tag.unwrap_or(0) as u8, offset)))
                .collect::<Result<_>>()?,
            data: match fields.get(3).map(Value::data) {
                Some(ValueData::Bytes(bytes)) => bytes.clone(),
                _ => Vec::new(),
            },
        };
        if physical.values.is_empty() {
            return Ok(Self::Null);
        }
        physical.node(0, 0)
    }
}

/// The rows of a VARIANT vector as the unshredded struct C++ stores, a NULL VARIANT as a NULL row.
pub fn to_unshredded_vector(vector: &crate::vector::Vector, count: usize) -> Result<crate::vector::Vector> {
    let mut result = crate::vector::Vector::new(unshredded_type(), count.max(1));
    for row in 0..count {
        let value = vector.get_value(row);
        let node = match value.data() {
            crate::value::ValueData::Bytes(bytes) if !value.is_null() => VariantNode::decode(bytes)?,
            _ => {
                result.set_null(row, true);
                continue;
            }
        };
        result.set_value(row, &node.to_unshredded()?)?;
    }
    Ok(result)
}

/// The VARIANT vector of `ty` that the unshredded struct rows of [`to_unshredded_vector`] stand for.
pub fn from_unshredded_vector(
    stored: &crate::vector::Vector,
    ty: &LogicalType,
    count: usize,
) -> Result<crate::vector::Vector> {
    let mut result = crate::vector::Vector::new(ty.clone(), count.max(1));
    for row in 0..count {
        if !stored.is_valid(row) {
            result.set_null(row, true);
            continue;
        }
        let node = VariantNode::from_unshredded(&stored.get_value(row))?;
        result.set_value(
            row,
            &Value::new(ty.clone(), crate::value::ValueData::Bytes(node.encode()))?,
        )?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        let value = Value::struct_value(vec![
            (
                "b".into(),
                Value::list(
                    LogicalType::INTEGER,
                    vec![Value::integer(1), Value::null(LogicalType::INTEGER)],
                )
                .unwrap(),
            ),
            ("a".into(), Value::varchar("x, y")),
            ("c".into(), Value::decimal(1234, 9, 2).unwrap()),
        ]);
        let node = VariantNode::from_value(&value).unwrap();
        assert_eq!(VariantNode::decode(&node.encode()).unwrap(), node);
        assert_eq!(node.render().unwrap(), "{'a': 'x, y', 'b': [1, NULL], 'c': 12.34}");
        assert_eq!(node.describe(), "OBJECT(a, b, c)");
        // as DuckDB stores it, and back
        let stored = node.to_unshredded().unwrap();
        assert_eq!(VariantNode::from_unshredded(&stored).unwrap(), node);
        let nested = VariantNode::Array(vec![node.clone(), VariantNode::Null, VariantNode::Object(Vec::new())]);
        assert_eq!(
            VariantNode::from_unshredded(&nested.to_unshredded().unwrap()).unwrap(),
            nested
        );
    }
}
