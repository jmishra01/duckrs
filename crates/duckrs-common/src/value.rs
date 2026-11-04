// Ported from duckdb/src/common/types/value.cpp @ rust-port-baseline

use crate::string_cast::{escape_nested_string, format_blob, format_decimal, format_double, format_float};
use crate::types::{
    Date, Interval, LogicalType, LogicalTypeId, PhysicalType, Time, TimeTz, Timestamp, time_ns_to_string,
    timestamp_ms_to_string, timestamp_ns_to_string, timestamp_sec_to_string, timestamp_to_string, uuid_to_string,
};
use crate::{Error, Result};

/// A value's payload, stored by physical type. DATE is `Int32`, TIMESTAMP `Int64`, DECIMAL the
/// unscaled integer, and VARCHAR/BLOB/BIT the raw bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueData {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    UInt8(u8),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    UInt128(u128),
    Float(f32),
    Double(f64),
    Interval(Interval),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Struct(Vec<Value>),
}

/// A single typed SQL value (`Value`). Used for constants in plans and for row-at-a-time access;
/// execution works on [`crate::vector::Vector`]s instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    ty: LogicalType,
    data: ValueData,
}

impl Value {
    /// Builds a value, checking that `data` fits the physical type of `ty`.
    pub fn new(ty: LogicalType, data: ValueData) -> Result<Self> {
        let ok = match (&data, ty.physical_type()) {
            (ValueData::Null, _) => true,
            (ValueData::Bool(_), PhysicalType::Bool)
            | (ValueData::Int8(_), PhysicalType::Int8)
            | (ValueData::Int16(_), PhysicalType::Int16)
            | (ValueData::Int32(_), PhysicalType::Int32)
            | (ValueData::Int64(_), PhysicalType::Int64)
            | (ValueData::Int128(_), PhysicalType::Int128)
            | (ValueData::UInt8(_), PhysicalType::UInt8)
            | (ValueData::UInt16(_), PhysicalType::UInt16)
            | (ValueData::UInt32(_), PhysicalType::UInt32)
            | (ValueData::UInt64(_), PhysicalType::UInt64)
            | (ValueData::UInt128(_), PhysicalType::UInt128)
            | (ValueData::Float(_), PhysicalType::Float)
            | (ValueData::Double(_), PhysicalType::Double)
            | (ValueData::Interval(_), PhysicalType::Interval)
            | (ValueData::Bytes(_), PhysicalType::Varchar) => true,
            (ValueData::List(items), PhysicalType::List | PhysicalType::Array) => {
                let child = ty.child_type().expect("LIST/ARRAY types carry a child type");
                let size_ok = ty.array_size().is_none_or(|s| s == 0 || s as usize == items.len());
                size_ok && items.iter().all(|v| v.ty == *child)
            }
            (ValueData::Struct(fields), PhysicalType::Struct) => {
                let children = ty.struct_children();
                children.len() == fields.len() && children.iter().zip(fields).all(|((_, t), v)| *t == v.ty)
            }
            _ => false,
        };
        if !ok {
            return Err(Error::new(
                crate::ErrorKind::Internal,
                format!("value data {data:?} does not match type {ty}"),
            ));
        }
        Ok(Self { ty, data })
    }

    /// The value at `position` of an ENUM type.
    pub fn enum_value(ty: &LogicalType, position: usize) -> Self {
        let data = match ty.physical_type() {
            PhysicalType::UInt8 => ValueData::UInt8(position as u8),
            PhysicalType::UInt16 => ValueData::UInt16(position as u16),
            _ => ValueData::UInt32(position as u32),
        };
        Self { ty: ty.clone(), data }
    }

    /// The same data under another type of the same physical type (`Vector::Reinterpret`).
    pub fn reinterpret(&self, ty: &LogicalType) -> Result<Self> {
        Self::new(ty.clone(), self.data.clone())
    }

    pub fn null(ty: LogicalType) -> Self {
        Self {
            ty,
            data: ValueData::Null,
        }
    }

    /// The untyped NULL literal (`Value()`).
    pub fn sql_null() -> Self {
        Self::null(LogicalType::SQLNULL)
    }

    pub fn boolean(v: bool) -> Self {
        Self {
            ty: LogicalType::BOOLEAN,
            data: ValueData::Bool(v),
        }
    }

    pub fn tinyint(v: i8) -> Self {
        Self {
            ty: LogicalType::TINYINT,
            data: ValueData::Int8(v),
        }
    }

    pub fn smallint(v: i16) -> Self {
        Self {
            ty: LogicalType::SMALLINT,
            data: ValueData::Int16(v),
        }
    }

    pub fn integer(v: i32) -> Self {
        Self {
            ty: LogicalType::INTEGER,
            data: ValueData::Int32(v),
        }
    }

    pub fn bigint(v: i64) -> Self {
        Self {
            ty: LogicalType::BIGINT,
            data: ValueData::Int64(v),
        }
    }

    pub fn hugeint(v: i128) -> Self {
        Self {
            ty: LogicalType::HUGEINT,
            data: ValueData::Int128(v),
        }
    }

    pub fn utinyint(v: u8) -> Self {
        Self {
            ty: LogicalType::UTINYINT,
            data: ValueData::UInt8(v),
        }
    }

    pub fn usmallint(v: u16) -> Self {
        Self {
            ty: LogicalType::USMALLINT,
            data: ValueData::UInt16(v),
        }
    }

    pub fn uinteger(v: u32) -> Self {
        Self {
            ty: LogicalType::UINTEGER,
            data: ValueData::UInt32(v),
        }
    }

    pub fn ubigint(v: u64) -> Self {
        Self {
            ty: LogicalType::UBIGINT,
            data: ValueData::UInt64(v),
        }
    }

    pub fn uhugeint(v: u128) -> Self {
        Self {
            ty: LogicalType::UHUGEINT,
            data: ValueData::UInt128(v),
        }
    }

    pub fn float(v: f32) -> Self {
        Self {
            ty: LogicalType::FLOAT,
            data: ValueData::Float(v),
        }
    }

    pub fn double(v: f64) -> Self {
        Self {
            ty: LogicalType::DOUBLE,
            data: ValueData::Double(v),
        }
    }

    pub fn interval(v: Interval) -> Self {
        Self {
            ty: LogicalType::INTERVAL,
            data: ValueData::Interval(v),
        }
    }

    pub fn varchar(v: impl Into<String>) -> Self {
        Self {
            ty: LogicalType::VARCHAR,
            data: ValueData::Bytes(v.into().into_bytes()),
        }
    }

    pub fn date(v: Date) -> Self {
        Self {
            ty: LogicalType::DATE,
            data: ValueData::Int32(v.0),
        }
    }

    pub fn time(v: Time) -> Self {
        Self {
            ty: LogicalType::TIME,
            data: ValueData::Int64(v.0),
        }
    }

    pub fn time_tz(v: TimeTz) -> Self {
        Self {
            ty: LogicalType::new(LogicalTypeId::TimeTz),
            data: ValueData::Int64(v.0 as i64),
        }
    }

    pub fn timestamp(v: Timestamp) -> Self {
        Self {
            ty: LogicalType::TIMESTAMP,
            data: ValueData::Int64(v.0),
        }
    }

    pub fn timestamp_tz(v: Timestamp) -> Self {
        Self {
            ty: LogicalType::new(LogicalTypeId::TimestampTz),
            data: ValueData::Int64(v.0),
        }
    }

    /// A UUID from its stored (top-bit-flipped) HUGEINT form; see [`crate::types::uuid_from_str`].
    pub fn uuid(stored: i128) -> Self {
        Self {
            ty: LogicalType::UUID,
            data: ValueData::Int128(stored),
        }
    }

    pub fn blob(v: impl Into<Vec<u8>>) -> Self {
        Self {
            ty: LogicalType::BLOB,
            data: ValueData::Bytes(v.into()),
        }
    }

    /// A DECIMAL from its unscaled value, e.g. `decimal(12345, 10, 2)` is `123.45`.
    pub fn decimal(unscaled: i128, width: u8, scale: u8) -> Result<Self> {
        let ty = LogicalType::decimal(width, scale);
        let limit = 10i128.pow(u32::from(width));
        if unscaled.abs() >= limit {
            return Err(Error::new(
                crate::ErrorKind::OutOfRange,
                format!("Could not cast value {unscaled} to DECIMAL({width},{scale})"),
            ));
        }
        let data = match ty.physical_type() {
            PhysicalType::Int16 => ValueData::Int16(unscaled as i16),
            PhysicalType::Int32 => ValueData::Int32(unscaled as i32),
            PhysicalType::Int64 => ValueData::Int64(unscaled as i64),
            _ => ValueData::Int128(unscaled),
        };
        Ok(Self { ty, data })
    }

    pub fn list(child: LogicalType, items: Vec<Value>) -> Result<Self> {
        Self::new(LogicalType::list(child), ValueData::List(items))
    }

    pub fn struct_value(fields: Vec<(String, Value)>) -> Self {
        let ty = LogicalType::struct_type(fields.iter().map(|(n, v)| (n.clone(), v.ty.clone())).collect());
        Self {
            ty,
            data: ValueData::Struct(fields.into_iter().map(|(_, v)| v).collect()),
        }
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.ty
    }

    pub fn data(&self) -> &ValueData {
        &self.data
    }

    pub fn is_null(&self) -> bool {
        matches!(self.data, ValueData::Null)
    }

    /// The unscaled integer of an integral or DECIMAL value.
    pub fn as_i128(&self) -> Option<i128> {
        Some(match self.data {
            ValueData::Int8(v) => v.into(),
            ValueData::Int16(v) => v.into(),
            ValueData::Int32(v) => v.into(),
            ValueData::Int64(v) => v.into(),
            ValueData::Int128(v) => v,
            ValueData::UInt8(v) => v.into(),
            ValueData::UInt16(v) => v.into(),
            ValueData::UInt32(v) => v.into(),
            ValueData::UInt64(v) => v.into(),
            ValueData::UInt128(v) => i128::try_from(v).ok()?,
            _ => return None,
        })
    }

    /// The member a UNION's tag selects, if there is one.
    fn union_member<'a>(&self, fields: &'a [Value]) -> Option<&'a Value> {
        let tag = fields[0].as_i128()?;
        fields.get(tag as usize + 1)
    }

    /// The value inside of a nested value's rendering: strings are escaped where they need it.
    fn child_string(&self) -> Result<String> {
        if self.is_null() {
            return Ok("NULL".into());
        }
        if let (LogicalTypeId::Union, ValueData::Struct(fields)) = (self.ty.id(), &self.data) {
            // `NestedToVarcharCast::WrapChildCast`: a UNION shows its member as a child would
            return match self.union_member(fields) {
                Some(member) => member.child_string(),
                None => Ok("NULL".into()),
            };
        }
        if let (LogicalTypeId::Variant, ValueData::Bytes(bytes)) = (self.ty.id(), &self.data) {
            return crate::variant::VariantNode::decode(bytes)?.render_child();
        }
        let text = self.try_to_string()?;
        Ok(if self.ty.is_nested() {
            text
        } else {
            escape_nested_string(&text, false)
        })
    }

    /// The value rendered as `CAST(value AS VARCHAR)` would (`Value::ToString`).
    ///
    /// GEOMETRY values go through cast functions that are not ported yet, and return
    /// `Not implemented`.
    pub fn try_to_string(&self) -> Result<String> {
        if self.is_null() {
            return Ok("NULL".into());
        }
        let ty = self.ty.id();
        Ok(match (&self.data, ty) {
            (_, LogicalTypeId::Enum) => {
                let position = match self.data {
                    ValueData::UInt8(v) => v as usize,
                    ValueData::UInt16(v) => v as usize,
                    ValueData::UInt32(v) => v as usize,
                    _ => unreachable!("an ENUM is stored as an unsigned integer"),
                };
                self.ty.enum_values()[position].clone()
            }
            (ValueData::Bool(v), _) => if *v { "true" } else { "false" }.into(),
            (_, LogicalTypeId::Decimal) => {
                let (width, scale) = self.ty.decimal_properties().expect("DECIMAL has properties");
                format_decimal(self.as_i128().expect("DECIMAL is integral"), width, scale)
            }
            (ValueData::Float(v), _) => format_float(*v),
            (ValueData::Double(v), _) => format_double(*v),
            (ValueData::Bytes(b), LogicalTypeId::Varchar | LogicalTypeId::Char) => String::from_utf8_lossy(b).into(),
            (ValueData::Bytes(b), LogicalTypeId::Blob) => format_blob(b),
            (ValueData::Bytes(b), LogicalTypeId::Bit) => crate::types::bit::bit_to_string(b),
            (ValueData::Bytes(b), LogicalTypeId::Variant) => crate::variant::VariantNode::decode(b)?.render()?,
            (ValueData::Bytes(b), LogicalTypeId::Bignum) => crate::types::bignum::to_string(b),
            (
                _,
                LogicalTypeId::TinyInt
                | LogicalTypeId::SmallInt
                | LogicalTypeId::Integer
                | LogicalTypeId::BigInt
                | LogicalTypeId::HugeInt
                | LogicalTypeId::UTinyInt
                | LogicalTypeId::USmallInt
                | LogicalTypeId::UInteger
                | LogicalTypeId::UBigInt,
            ) => self.as_i128().expect("integral").to_string(),
            (ValueData::UInt128(v), LogicalTypeId::UHugeInt) => v.to_string(),
            (ValueData::Int32(v), LogicalTypeId::Date) => Date(*v).to_string(),
            (ValueData::Int64(v), LogicalTypeId::Time) => Time(*v).to_string(),
            (ValueData::Int64(v), LogicalTypeId::TimeNs) => time_ns_to_string(*v),
            (ValueData::Int64(v), LogicalTypeId::TimeTz) => TimeTz(*v as u64).to_string(),
            (ValueData::Int64(v), LogicalTypeId::Timestamp) => timestamp_to_string(*v, false),
            (ValueData::Int64(v), LogicalTypeId::TimestampTz) => timestamp_to_string(*v, true),
            (ValueData::Int64(v), LogicalTypeId::TimestampNs) => timestamp_ns_to_string(*v, false),
            (ValueData::Int64(v), LogicalTypeId::TimestampTzNs) => timestamp_ns_to_string(*v, true),
            (ValueData::Int64(v), LogicalTypeId::TimestampMs) => timestamp_ms_to_string(*v),
            (ValueData::Int64(v), LogicalTypeId::TimestampSec) => timestamp_sec_to_string(*v),
            (ValueData::Interval(v), _) => v.to_string(),
            (ValueData::Int128(v), LogicalTypeId::Uuid) => uuid_to_string(*v),
            (ValueData::List(items), LogicalTypeId::Map) => {
                let mut entries = Vec::with_capacity(items.len());
                for item in items {
                    let ValueData::Struct(fields) = &item.data else {
                        unreachable!("a map entry is a STRUCT");
                    };
                    entries.push(format!("{}={}", fields[0].child_string()?, fields[1].child_string()?));
                }
                format!("{{{}}}", entries.join(", "))
            }
            (ValueData::List(items), _) => {
                let items = items.iter().map(Value::child_string).collect::<Result<Vec<_>>>()?;
                format!("[{}]", items.join(", "))
            }
            (ValueData::Struct(fields), LogicalTypeId::Union) => match self.union_member(fields) {
                Some(member) => member.try_to_string()?,
                None => "NULL".into(),
            },
            (ValueData::Struct(fields), LogicalTypeId::Tuple) => {
                let fields = fields.iter().map(Value::child_string).collect::<Result<Vec<_>>>()?;
                // a single-element tuple needs a trailing comma to round-trip
                if let [field] = fields.as_slice() {
                    format!("({field},)")
                } else {
                    format!("({})", fields.join(", "))
                }
            }
            (ValueData::Struct(fields), _) => {
                let mut entries = Vec::with_capacity(fields.len());
                for ((name, _), field) in self.ty.struct_children().iter().zip(fields) {
                    entries.push(format!(
                        "{}: {}",
                        escape_nested_string(name, true),
                        field.child_string()?
                    ));
                }
                format!("{{{}}}", entries.join(", "))
            }
            _ => {
                return Err(Error::not_implemented(format!(
                    "converting {} values to VARCHAR is not supported yet",
                    self.ty
                )));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_string_matches_duckdb() {
        assert_eq!(Value::integer(-42).try_to_string().unwrap(), "-42");
        assert_eq!(Value::boolean(true).try_to_string().unwrap(), "true");
        assert_eq!(Value::double(100.0).try_to_string().unwrap(), "100.0");
        assert_eq!(Value::float(0.1).try_to_string().unwrap(), "0.1");
        assert_eq!(Value::decimal(-5, 4, 2).unwrap().try_to_string().unwrap(), "-0.05");
        assert_eq!(
            Value::hugeint(i128::MAX).try_to_string().unwrap(),
            "170141183460469231731687303715884105727"
        );
        assert_eq!(
            Value::uhugeint(u128::MAX).try_to_string().unwrap(),
            u128::MAX.to_string()
        );
        assert_eq!(
            Value::blob(b"\x00ab\xFF".to_vec()).try_to_string().unwrap(),
            "\\x00ab\\xFF"
        );
        assert_eq!(Value::varchar("héllo").try_to_string().unwrap(), "héllo");
        assert_eq!(Value::null(LogicalType::INTEGER).try_to_string().unwrap(), "NULL");
        assert_eq!(
            Value::interval(Interval::new(1, 0, 0)).try_to_string().unwrap(),
            "1 month"
        );
        assert_eq!(Value::date(Date(8035)).try_to_string().unwrap(), "1992-01-01");
        assert_eq!(
            Value::time(Time::from_hms(12, 30, 0, 500_000)).try_to_string().unwrap(),
            "12:30:00.5"
        );
        assert_eq!(
            Value::timestamp(Timestamp(-1)).try_to_string().unwrap(),
            "1969-12-31 23:59:59.999999"
        );
        assert_eq!(
            Value::timestamp_tz(Timestamp(1_577_873_472_500_000))
                .try_to_string()
                .unwrap(),
            "2020-01-01 10:11:12.5+00"
        );
        let uuid = crate::types::uuid_from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8", true).unwrap();
        assert_eq!(
            Value::uuid(uuid).try_to_string().unwrap(),
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
        );
        assert_eq!(
            Value::list(
                LogicalType::INTEGER,
                vec![Value::integer(1), Value::null(LogicalType::INTEGER)]
            )
            .unwrap()
            .try_to_string()
            .unwrap(),
            "[1, NULL]"
        );
        assert_eq!(
            Value::list(
                LogicalType::VARCHAR,
                vec![Value::varchar("a b"), Value::varchar("c, d")]
            )
            .unwrap()
            .try_to_string()
            .unwrap(),
            "[a b, 'c, d']"
        );
        assert_eq!(
            Value::struct_value(vec![
                ("a".into(), Value::integer(1)),
                ("b c".into(), Value::varchar(""))
            ])
            .try_to_string()
            .unwrap(),
            "{'a': 1, 'b c': ''}"
        );
    }

    #[test]
    fn type_checking() {
        assert!(Value::new(LogicalType::INTEGER, ValueData::Int64(1)).is_err());
        assert!(Value::new(LogicalType::DATE, ValueData::Int32(1)).is_ok());
        assert!(Value::list(LogicalType::INTEGER, vec![Value::bigint(1)]).is_err());
        assert!(
            Value::list(
                LogicalType::INTEGER,
                vec![Value::integer(1), Value::null(LogicalType::INTEGER)]
            )
            .is_ok()
        );
        assert!(Value::decimal(10_000, 4, 2).is_err());
        assert_eq!(
            Value::decimal(9_999, 4, 2).unwrap().logical_type().physical_type(),
            PhysicalType::Int16
        );
    }
}
