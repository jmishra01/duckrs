// Ported from duckdb/src/common/types.cpp and duckdb/src/include/duckdb/common/types.hpp
// @ rust-port-baseline

mod date;
mod date_part;
mod identifier;
mod interval;
mod native;
mod string_t;
mod time;
mod timestamp;
mod uuid;

pub use date::{Date, DateCastError, ParsedDate, date_error};
pub use date_part::DatePart;
pub use identifier::sql_identifier;
pub use interval::{Interval, IntervalParseError};
pub use native::{ListEntry, NativeType};
pub use string_t::{StringHeap, StringT};
pub use time::{
    MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SEC, NANOS_PER_DAY, NANOS_PER_MICRO, Time, TimeTz,
    time_error, time_ns_to_string,
};
pub use timestamp::{
    Timestamp, TimestampCastError, timestamp_error, timestamp_ms_to_string, timestamp_ns_to_string,
    timestamp_sec_to_string, timestamp_to_string,
};
pub use uuid::{uuid_from_str, uuid_to_string};

use std::fmt;
use std::sync::Arc;

/// The logical type id. Discriminants match the C++ `LogicalTypeId` because they are serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogicalTypeId {
    Invalid = 0,
    SqlNull = 1,
    Unknown = 2,
    Any = 3,
    Unbound = 4,
    Template = 5,
    Type = 6,
    Boolean = 10,
    TinyInt = 11,
    SmallInt = 12,
    Integer = 13,
    BigInt = 14,
    Date = 15,
    Time = 16,
    TimestampSec = 17,
    TimestampMs = 18,
    Timestamp = 19,
    TimestampNs = 20,
    Decimal = 21,
    Float = 22,
    Double = 23,
    Char = 24,
    Varchar = 25,
    Blob = 26,
    Interval = 27,
    UTinyInt = 28,
    USmallInt = 29,
    UInteger = 30,
    UBigInt = 31,
    TimestampTz = 32,
    TimestampTzNs = 33,
    TimeTz = 34,
    TimeNs = 35,
    Bit = 36,
    StringLiteral = 37,
    IntegerLiteral = 38,
    Bignum = 39,
    UHugeInt = 49,
    HugeInt = 50,
    Pointer = 51,
    Validity = 53,
    Uuid = 54,
    Geometry = 60,
    Struct = 100,
    List = 101,
    Map = 102,
    Table = 103,
    Enum = 104,
    LegacyAggregateState = 105,
    Lambda = 106,
    Union = 107,
    Array = 108,
    Variant = 109,
    Tuple = 110,
}

impl LogicalTypeId {
    /// The name `EnumUtil::ToString(LogicalTypeId)` produces.
    pub fn name(self) -> &'static str {
        use LogicalTypeId::*;
        match self {
            Invalid => "INVALID",
            SqlNull => "NULL",
            Unknown => "UNKNOWN",
            Any => "ANY",
            Unbound => "UNBOUND",
            Template => "TEMPLATE",
            Type => "TYPE",
            Boolean => "BOOLEAN",
            TinyInt => "TINYINT",
            SmallInt => "SMALLINT",
            Integer => "INTEGER",
            BigInt => "BIGINT",
            Date => "DATE",
            Time => "TIME",
            TimestampSec => "TIMESTAMP_S",
            TimestampMs => "TIMESTAMP_MS",
            Timestamp => "TIMESTAMP",
            TimestampNs => "TIMESTAMP_NS",
            Decimal => "DECIMAL",
            Float => "FLOAT",
            Double => "DOUBLE",
            Char => "CHAR",
            Varchar => "VARCHAR",
            Blob => "BLOB",
            Interval => "INTERVAL",
            UTinyInt => "UTINYINT",
            USmallInt => "USMALLINT",
            UInteger => "UINTEGER",
            UBigInt => "UBIGINT",
            TimestampTz => "TIMESTAMP WITH TIME ZONE",
            TimestampTzNs => "TIMESTAMPTZ_NS",
            TimeTz => "TIME WITH TIME ZONE",
            TimeNs => "TIME_NS",
            Bit => "BIT",
            StringLiteral => "STRING_LITERAL",
            IntegerLiteral => "INTEGER_LITERAL",
            Bignum => "BIGNUM",
            UHugeInt => "UHUGEINT",
            HugeInt => "HUGEINT",
            Pointer => "POINTER",
            Validity => "VALIDITY",
            Uuid => "UUID",
            Geometry => "GEOMETRY",
            Struct => "STRUCT",
            List => "LIST",
            Map => "MAP",
            Table => "TABLE",
            Enum => "ENUM",
            LegacyAggregateState => "LEGACY_AGGREGATE_STATE",
            Lambda => "LAMBDA",
            Union => "UNION",
            Array => "ARRAY",
            Variant => "VARIANT",
            Tuple => "TUPLE",
        }
    }

    /// The physical type for ids that need no parameters (`TryGetBuiltinPhysicalType`).
    /// DECIMAL and ENUM depend on their parameters and map to `Invalid` here.
    pub fn builtin_physical_type(self) -> PhysicalType {
        use LogicalTypeId as L;
        use PhysicalType as P;
        match self {
            L::Boolean => P::Bool,
            L::TinyInt => P::Int8,
            L::UTinyInt => P::UInt8,
            L::SmallInt => P::Int16,
            L::USmallInt => P::UInt16,
            L::SqlNull | L::Date | L::Integer => P::Int32,
            L::UInteger => P::UInt32,
            L::BigInt
            | L::Time
            | L::TimeNs
            | L::Timestamp
            | L::TimestampSec
            | L::TimestampNs
            | L::TimestampMs
            | L::TimeTz
            | L::TimestampTz
            | L::TimestampTzNs => P::Int64,
            L::UBigInt => P::UInt64,
            L::UHugeInt => P::UInt128,
            L::HugeInt | L::Uuid => P::Int128,
            L::Float => P::Float,
            L::Double => P::Double,
            L::Bignum | L::Varchar | L::Char | L::Blob | L::Bit | L::Type | L::Geometry => P::Varchar,
            L::Interval => P::Interval,
            L::Union | L::Variant | L::Struct | L::Tuple => P::Struct,
            L::List | L::Map => P::List,
            L::Array => P::Array,
            L::Pointer => {
                if size_of::<usize>() == 4 {
                    P::UInt32
                } else {
                    P::UInt64
                }
            }
            L::Validity => P::Bit,
            L::Lambda => P::UInt8,
            L::Unbound => P::Unknown,
            L::Decimal
            | L::Enum
            | L::Table
            | L::Any
            | L::Invalid
            | L::Unknown
            | L::StringLiteral
            | L::IntegerLiteral
            | L::Template
            | L::LegacyAggregateState => P::Invalid,
        }
    }
}

/// The physical (storage) type. Discriminants match the C++ `PhysicalType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PhysicalType {
    Bool = 1,
    UInt8 = 2,
    Int8 = 3,
    UInt16 = 4,
    Int16 = 5,
    UInt32 = 6,
    Int32 = 7,
    UInt64 = 8,
    Int64 = 9,
    Float = 11,
    Double = 12,
    Interval = 21,
    List = 23,
    Struct = 24,
    Array = 29,
    Varchar = 200,
    UInt128 = 203,
    Int128 = 204,
    Unknown = 205,
    Bit = 206,
    Invalid = 255,
}

impl PhysicalType {
    /// `TypeIdToString`.
    pub fn name(self) -> &'static str {
        use PhysicalType::*;
        match self {
            Bool => "BOOL",
            Int8 => "INT8",
            Int16 => "INT16",
            Int32 => "INT32",
            Int64 => "INT64",
            UInt8 => "UINT8",
            UInt16 => "UINT16",
            UInt32 => "UINT32",
            UInt64 => "UINT64",
            Int128 => "INT128",
            UInt128 => "UINT128",
            Float => "FLOAT",
            Double => "DOUBLE",
            Varchar => "VARCHAR",
            Interval => "INTERVAL",
            Struct => "STRUCT",
            List => "LIST",
            Array => "ARRAY",
            Invalid => "INVALID",
            Bit => "BIT",
            Unknown => "UNKNOWN",
        }
    }

    /// Bytes per value in a flat vector (`GetTypeIdSize`). Nested STRUCT/ARRAY store no data
    /// of their own.
    pub fn size(self) -> usize {
        use PhysicalType::*;
        match self {
            Bit | Bool | Int8 | UInt8 => 1,
            Int16 | UInt16 => 2,
            Int32 | UInt32 | Float => 4,
            Int64 | UInt64 | Double => 8,
            Int128 | UInt128 | Interval | Varchar | List => 16,
            Struct | Array | Unknown | Invalid => 0,
        }
    }

    /// Whether values are fixed-size and stored directly in the vector (`TypeIsConstantSize`).
    pub fn is_constant_size(self) -> bool {
        use PhysicalType::*;
        matches!(
            self,
            Bool | Int8
                | Int16
                | Int32
                | Int64
                | UInt8
                | UInt16
                | UInt32
                | UInt64
                | Int128
                | UInt128
                | Float
                | Double
                | Interval
        )
    }
}

impl fmt::Display for PhysicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Decimal width limits per physical type (`Decimal::MAX_WIDTH_*`).
pub mod decimal {
    pub const MAX_WIDTH_INT16: u8 = 4;
    pub const MAX_WIDTH_INT32: u8 = 9;
    pub const MAX_WIDTH_INT64: u8 = 18;
    pub const MAX_WIDTH_INT128: u8 = 38;
    pub const MAX_WIDTH: u8 = MAX_WIDTH_INT128;
}

/// Parameters of a type, mirroring the C++ `ExtraTypeInfo` subclasses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Params {
    None,
    Decimal {
        width: u8,
        scale: u8,
    },
    /// LIST and MAP (whose child is `STRUCT(key, value)`).
    List(LogicalType),
    /// STRUCT, TUPLE and UNION (whose first child is the `UTINYINT` tag).
    Struct(Vec<(String, LogicalType)>),
    Array {
        child: LogicalType,
        size: u32,
    },
    Enum(Arc<[String]>),
    /// INTEGER_LITERAL: the constant's own integral type and its value (`IntegerLiteralTypeInfo`).
    /// A UHUGEINT above `i128::MAX` keeps `value == i128::MAX` with `beyond_i128` set.
    IntegerLiteral {
        ty: LogicalTypeId,
        value: i128,
        beyond_i128: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TypeInfo {
    params: Params,
    alias: Option<String>,
}

/// A SQL type. Cheap to clone: parameters live behind an `Arc`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LogicalType {
    id: LogicalTypeId,
    info: Option<Arc<TypeInfo>>,
}

impl fmt::Debug for LogicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalType({self})")
    }
}

impl From<LogicalTypeId> for LogicalType {
    fn from(id: LogicalTypeId) -> Self {
        Self::new(id)
    }
}

impl LogicalType {
    pub const fn new(id: LogicalTypeId) -> Self {
        Self { id, info: None }
    }

    fn with_params(id: LogicalTypeId, params: Params) -> Self {
        Self {
            id,
            info: Some(Arc::new(TypeInfo { params, alias: None })),
        }
    }

    pub const INVALID: Self = Self::new(LogicalTypeId::Invalid);
    pub const SQLNULL: Self = Self::new(LogicalTypeId::SqlNull);
    pub const BOOLEAN: Self = Self::new(LogicalTypeId::Boolean);
    pub const TINYINT: Self = Self::new(LogicalTypeId::TinyInt);
    pub const SMALLINT: Self = Self::new(LogicalTypeId::SmallInt);
    pub const INTEGER: Self = Self::new(LogicalTypeId::Integer);
    pub const BIGINT: Self = Self::new(LogicalTypeId::BigInt);
    pub const HUGEINT: Self = Self::new(LogicalTypeId::HugeInt);
    pub const UTINYINT: Self = Self::new(LogicalTypeId::UTinyInt);
    pub const USMALLINT: Self = Self::new(LogicalTypeId::USmallInt);
    pub const UINTEGER: Self = Self::new(LogicalTypeId::UInteger);
    pub const UBIGINT: Self = Self::new(LogicalTypeId::UBigInt);
    pub const UHUGEINT: Self = Self::new(LogicalTypeId::UHugeInt);
    pub const FLOAT: Self = Self::new(LogicalTypeId::Float);
    pub const DOUBLE: Self = Self::new(LogicalTypeId::Double);
    pub const DATE: Self = Self::new(LogicalTypeId::Date);
    pub const TIME: Self = Self::new(LogicalTypeId::Time);
    pub const TIMESTAMP: Self = Self::new(LogicalTypeId::Timestamp);
    pub const INTERVAL: Self = Self::new(LogicalTypeId::Interval);
    pub const VARCHAR: Self = Self::new(LogicalTypeId::Varchar);
    pub const BLOB: Self = Self::new(LogicalTypeId::Blob);
    pub const UUID: Self = Self::new(LogicalTypeId::Uuid);

    /// `DECIMAL(width, scale)`. Panics if the parameters are out of range; the binder
    /// validates user input before constructing types.
    pub fn decimal(width: u8, scale: u8) -> Self {
        assert!(
            (1..=decimal::MAX_WIDTH).contains(&width) && scale <= width,
            "invalid DECIMAL({width},{scale})"
        );
        Self::with_params(LogicalTypeId::Decimal, Params::Decimal { width, scale })
    }

    pub fn list(child: LogicalType) -> Self {
        Self::with_params(LogicalTypeId::List, Params::List(child))
    }

    pub fn struct_type(children: Vec<(String, LogicalType)>) -> Self {
        Self::with_params(LogicalTypeId::Struct, Params::Struct(children))
    }

    /// `TUPLE(children)`: a STRUCT whose members have no names.
    pub fn tuple(children: Vec<LogicalType>) -> Self {
        let children = children.into_iter().map(|c| (String::new(), c)).collect();
        Self::struct_type(children).with_id(LogicalTypeId::Tuple)
    }

    /// `MAP(entry)` from its `STRUCT(key, value)` entry type.
    pub fn map_from_entry(entry: LogicalType) -> Self {
        Self::with_params(LogicalTypeId::Map, Params::List(entry))
    }

    /// `MAP(key, value)`: physically a `LIST(STRUCT(key, value))`.
    pub fn map(key: LogicalType, value: LogicalType) -> Self {
        let entry = Self::struct_type(vec![("key".into(), key), ("value".into(), value)]);
        Self::with_params(LogicalTypeId::Map, Params::List(entry))
    }

    pub fn array(child: LogicalType, size: u32) -> Self {
        Self::with_params(LogicalTypeId::Array, Params::Array { child, size })
    }

    /// `UNION(members)`: physically a STRUCT whose first child is the `UTINYINT` tag.
    pub fn union_type(members: Vec<(String, LogicalType)>) -> Self {
        let mut children = Vec::with_capacity(members.len() + 1);
        children.push((String::new(), LogicalType::UTINYINT));
        children.extend(members);
        Self::with_params(LogicalTypeId::Union, Params::Struct(children))
    }

    pub fn enum_type(values: Vec<String>) -> Self {
        Self::with_params(LogicalTypeId::Enum, Params::Enum(values.into()))
    }

    /// `LogicalType::INTEGER_LITERAL(constant)`: the type of an integer constant while binding,
    /// which casts cheaply to any integer type it fits in. `ty` must be integral.
    pub fn integer_literal(ty: LogicalTypeId, value: i128, beyond_i128: bool) -> Self {
        assert!(
            LogicalType::new(ty).is_integral(),
            "INTEGER_LITERAL of non-integral type {ty:?}"
        );
        Self::with_params(
            LogicalTypeId::IntegerLiteral,
            Params::IntegerLiteral { ty, value, beyond_i128 },
        )
    }

    /// `IntegerLiteral::GetType` and the literal's value, for an INTEGER_LITERAL.
    pub fn integer_literal_value(&self) -> Option<(LogicalType, i128, bool)> {
        match self.params() {
            Params::IntegerLiteral { ty, value, beyond_i128 } => Some((LogicalType::new(*ty), *value, *beyond_i128)),
            _ => None,
        }
    }

    /// Returns this type with a user-facing alias (e.g. `CREATE TYPE`). The alias replaces the
    /// type name in `to_string`.
    pub fn with_alias(&self, alias: impl Into<String>) -> Self {
        let mut info = self.info.as_deref().cloned().unwrap_or(TypeInfo {
            params: Params::None,
            alias: None,
        });
        info.alias = Some(alias.into());
        Self {
            id: self.id,
            info: Some(Arc::new(info)),
        }
    }

    /// The same parameters under another id (e.g. a STRUCT's children as a TUPLE).
    pub(crate) fn with_id(mut self, id: LogicalTypeId) -> Self {
        self.id = id;
        self
    }

    pub fn id(&self) -> LogicalTypeId {
        self.id
    }

    pub fn alias(&self) -> Option<&str> {
        self.info.as_ref().and_then(|i| i.alias.as_deref())
    }

    fn params(&self) -> &Params {
        self.info.as_ref().map_or(&Params::None, |i| &i.params)
    }

    pub fn has_params(&self) -> bool {
        !matches!(self.params(), Params::None)
    }

    /// The physical type (`LogicalType::InternalType`).
    pub fn physical_type(&self) -> PhysicalType {
        match (self.id, self.params()) {
            (LogicalTypeId::Decimal, Params::Decimal { width, .. }) => match *width {
                w if w <= decimal::MAX_WIDTH_INT16 => PhysicalType::Int16,
                w if w <= decimal::MAX_WIDTH_INT32 => PhysicalType::Int32,
                w if w <= decimal::MAX_WIDTH_INT64 => PhysicalType::Int64,
                _ => PhysicalType::Int128,
            },
            (LogicalTypeId::Enum, Params::Enum(values)) => enum_dict_type(values.len()),
            (id, _) => id.builtin_physical_type(),
        }
    }

    /// `(width, scale)` of a DECIMAL.
    pub fn decimal_properties(&self) -> Option<(u8, u8)> {
        match self.params() {
            Params::Decimal { width, scale } => Some((*width, *scale)),
            _ => None,
        }
    }

    /// The child of a LIST, MAP (its `STRUCT(key, value)`) or ARRAY.
    pub fn child_type(&self) -> Option<&LogicalType> {
        match self.params() {
            Params::List(child) | Params::Array { child, .. } => Some(child),
            _ => None,
        }
    }

    pub fn array_size(&self) -> Option<u32> {
        match self.params() {
            Params::Array { size, .. } => Some(*size),
            _ => None,
        }
    }

    /// Children of a STRUCT, TUPLE or UNION (for UNION, including the tag at index 0).
    pub fn struct_children(&self) -> &[(String, LogicalType)] {
        match self.params() {
            Params::Struct(children) => children,
            _ => &[],
        }
    }

    pub fn enum_values(&self) -> &[String] {
        match self.params() {
            Params::Enum(values) => values,
            _ => &[],
        }
    }

    pub fn is_numeric(&self) -> bool {
        use LogicalTypeId::*;
        matches!(
            self.id,
            TinyInt
                | SmallInt
                | Integer
                | BigInt
                | HugeInt
                | Float
                | Double
                | Decimal
                | UTinyInt
                | USmallInt
                | UInteger
                | UBigInt
                | UHugeInt
        )
    }

    pub fn is_integral(&self) -> bool {
        use LogicalTypeId::*;
        matches!(
            self.id,
            TinyInt | SmallInt | Integer | BigInt | HugeInt | UTinyInt | USmallInt | UInteger | UBigInt | UHugeInt
        )
    }

    pub fn is_nested(&self) -> bool {
        matches!(
            self.physical_type(),
            PhysicalType::List | PhysicalType::Struct | PhysicalType::Array
        )
    }
}

/// The physical type used to store an enum with `size` values (`EnumTypeInfo::DictType`).
fn enum_dict_type(size: usize) -> PhysicalType {
    if size <= u8::MAX as usize {
        PhysicalType::UInt8
    } else if size <= u16::MAX as usize {
        PhysicalType::UInt16
    } else {
        PhysicalType::UInt32
    }
}

/// Renders a string literal the way `SQLString` does: single-quoted, quotes doubled.
fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl fmt::Display for LogicalType {
    /// `LogicalType::ToString`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.id != LogicalTypeId::Unbound
            && let Some(alias) = self.alias()
        {
            return f.write_str(alias);
        }
        match (self.id, self.params()) {
            (LogicalTypeId::Struct | LogicalTypeId::Tuple, Params::Struct(children)) if !children.is_empty() => {
                let tuple = self.id == LogicalTypeId::Tuple;
                f.write_str(if tuple { "TUPLE(" } else { "STRUCT(" })?;
                for (i, (name, ty)) in children.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    if !tuple {
                        write!(f, "{} ", sql_identifier(name))?;
                    }
                    write!(f, "{ty}")?;
                }
                f.write_str(")")
            }
            (LogicalTypeId::List, Params::List(child)) => write!(f, "{child}[]"),
            (LogicalTypeId::Array, Params::Array { child, size }) => {
                if *size == 0 {
                    write!(f, "{child}[ANY]")
                } else {
                    write!(f, "{child}[{size}]")
                }
            }
            (LogicalTypeId::Map, Params::List(entry)) => {
                let children = entry.struct_children();
                write!(f, "MAP({}, {})", children[0].1, children[1].1)
            }
            (LogicalTypeId::Union, Params::Struct(children)) => {
                f.write_str("UNION(")?;
                for (i, (name, ty)) in children.iter().skip(1).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} {ty}", sql_identifier(name))?;
                }
                f.write_str(")")
            }
            (LogicalTypeId::Decimal, Params::Decimal { width, scale }) => write!(f, "DECIMAL({width},{scale})"),
            (LogicalTypeId::Enum, params) => {
                f.write_str("ENUM(")?;
                if let Params::Enum(values) = params {
                    for (i, v) in values.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(&sql_string(v))?;
                    }
                }
                f.write_str(")")
            }
            (LogicalTypeId::SqlNull, _) => f.write_str("\"NULL\""),
            (LogicalTypeId::Template, _) => f.write_str("T"),
            (id, _) => f.write_str(id.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_duckdb() {
        assert_eq!(LogicalType::INTEGER.to_string(), "INTEGER");
        assert_eq!(LogicalType::decimal(10, 2).to_string(), "DECIMAL(10,2)");
        assert_eq!(LogicalType::list(LogicalType::VARCHAR).to_string(), "VARCHAR[]");
        assert_eq!(
            LogicalType::array(LogicalType::list(LogicalType::INTEGER), 3).to_string(),
            "INTEGER[][3]"
        );
        assert_eq!(
            LogicalType::struct_type(vec![
                ("a".into(), LogicalType::INTEGER),
                ("my col".into(), LogicalType::VARCHAR)
            ])
            .to_string(),
            "STRUCT(a INTEGER, \"my col\" VARCHAR)"
        );
        assert_eq!(
            LogicalType::map(LogicalType::VARCHAR, LogicalType::BIGINT).to_string(),
            "MAP(VARCHAR, BIGINT)"
        );
        assert_eq!(
            LogicalType::union_type(vec![("num".into(), LogicalType::INTEGER)]).to_string(),
            "UNION(num INTEGER)"
        );
        assert_eq!(
            LogicalType::enum_type(vec!["a".into(), "it's".into()]).to_string(),
            "ENUM('a', 'it''s')"
        );
        assert_eq!(LogicalType::SQLNULL.to_string(), "\"NULL\"");
        assert_eq!(
            LogicalType::new(LogicalTypeId::TimestampTz).to_string(),
            "TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(LogicalType::INTEGER.with_alias("myint").to_string(), "myint");
    }

    #[test]
    fn physical_types() {
        assert_eq!(LogicalType::DATE.physical_type(), PhysicalType::Int32);
        assert_eq!(LogicalType::UUID.physical_type(), PhysicalType::Int128);
        assert_eq!(LogicalType::decimal(4, 1).physical_type(), PhysicalType::Int16);
        assert_eq!(LogicalType::decimal(9, 1).physical_type(), PhysicalType::Int32);
        assert_eq!(LogicalType::decimal(18, 1).physical_type(), PhysicalType::Int64);
        assert_eq!(LogicalType::decimal(38, 1).physical_type(), PhysicalType::Int128);
        assert_eq!(
            LogicalType::map(LogicalType::VARCHAR, LogicalType::BIGINT).physical_type(),
            PhysicalType::List
        );
        let big_enum = LogicalType::enum_type((0..300).map(|i| i.to_string()).collect());
        assert_eq!(big_enum.physical_type(), PhysicalType::UInt16);
        assert_eq!(PhysicalType::Varchar.size(), 16);
    }

    #[test]
    fn discriminants_match_cpp() {
        assert_eq!(LogicalTypeId::Varchar as u8, 25);
        assert_eq!(LogicalTypeId::HugeInt as u8, 50);
        assert_eq!(LogicalTypeId::Tuple as u8, 110);
        assert_eq!(PhysicalType::Varchar as u8, 200);
        assert_eq!(PhysicalType::Int128 as u8, 204);
    }
}
