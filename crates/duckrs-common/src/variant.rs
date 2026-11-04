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

const NULL_TAG: u8 = 0;
const OBJECT_TAG: u8 = 1;
const ARRAY_TAG: u8 = 2;
const SCALAR_TAG: u8 = 3;

/// The scalar types a VARIANT holds, by the code they are stored with. DECIMAL has its own code.
const SCALAR_TYPES: [LogicalTypeId; 29] = {
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
    ]
};
const DECIMAL_CODE: u8 = 255;

impl VariantNode {
    /// `ConvertToVariant`.
    pub fn from_value(value: &Value) -> Result<Self> {
        if value.is_null() {
            return Ok(Self::Null);
        }
        let ty = value.logical_type();
        Ok(match (ty.id(), value.data()) {
            (LogicalTypeId::Variant, ValueData::Bytes(bytes)) => Self::decode(bytes)?,
            (LogicalTypeId::Union, ValueData::Struct(fields)) => match fields[0].as_i128() {
                Some(tag) => Self::from_value(&fields[tag as usize + 1])?,
                None => Self::Null,
            },
            (LogicalTypeId::Tuple, ValueData::Struct(fields)) => {
                Self::Array(fields.iter().map(Self::from_value).collect::<Result<_>>()?)
            }
            (_, ValueData::Struct(fields)) => {
                let mut children = Vec::with_capacity(fields.len());
                for ((name, _), field) in ty.struct_children().iter().zip(fields) {
                    children.push((name.clone(), Self::from_value(field)?));
                }
                children.sort_by(|a, b| a.0.cmp(&b.0));
                Self::Object(children)
            }
            (_, ValueData::List(items)) => Self::Array(items.iter().map(Self::from_value).collect::<Result<_>>()?),
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
        match self {
            Self::Null => Ok("NULL".to_string()),
            Self::Scalar(value) => value.try_to_string(),
            Self::Object(children) => {
                let mut entries = Vec::with_capacity(children.len());
                for (key, child) in children {
                    entries.push(format!(
                        "{}: {}",
                        escape_nested_string(key, true),
                        child.render_child()?
                    ));
                }
                Ok(format!("{{{}}}", entries.join(", ")))
            }
            Self::Array(items) => {
                let items = items.iter().map(Self::render_child).collect::<Result<Vec<_>>>()?;
                Ok(format!("[{}]", items.join(", ")))
            }
        }
    }

    /// `render` inside of a nested value: strings are escaped where they need it.
    pub fn render_child(&self) -> Result<String> {
        match self {
            Self::Scalar(value) => Ok(escape_nested_string(&value.try_to_string()?, false)),
            other => other.render(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        let write_len = |out: &mut Vec<u8>, length: usize| out.extend_from_slice(&(length as u32).to_le_bytes());
        match self {
            Self::Null => out.push(NULL_TAG),
            Self::Object(children) => {
                out.push(OBJECT_TAG);
                write_len(out, children.len());
                for (key, child) in children {
                    write_len(out, key.len());
                    out.extend_from_slice(key.as_bytes());
                    child.write(out);
                }
            }
            Self::Array(items) => {
                out.push(ARRAY_TAG);
                write_len(out, items.len());
                items.iter().for_each(|item| item.write(out));
            }
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

    fn node(&mut self) -> Result<VariantNode> {
        let [tag] = self.take::<1>()?;
        match tag {
            NULL_TAG => Ok(VariantNode::Null),
            OBJECT_TAG => {
                let size = self.length()?;
                let mut children = Vec::with_capacity(size);
                for _ in 0..size {
                    let length = self.length()?;
                    let key = String::from_utf8_lossy(self.slice(length)?).into_owned();
                    children.push((key, self.node()?));
                }
                Ok(VariantNode::Object(children))
            }
            ARRAY_TAG => {
                let size = self.length()?;
                (0..size)
                    .map(|_| self.node())
                    .collect::<Result<Vec<_>>>()
                    .map(VariantNode::Array)
            }
            _ => self.scalar().map(VariantNode::Scalar),
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
    }
}
