// Ported from duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! The built-in types of the system catalog (`BUILTIN_TYPES`), with the signatures of the
//! constructors that take their modifiers, as `duckdb_types()` lists them.

use pintail_common::types::LogicalTypeId;

/// A `TypeConstructor` signature: its named parameters with their types, and the type of its
/// `*args`/`**kwargs` if it has them.
pub struct Constructor {
    pub parameters: &'static [(&'static str, &'static str)],
    pub varargs: Option<&'static str>,
}

const fn fixed(parameters: &'static [(&'static str, &'static str)]) -> Constructor {
    Constructor {
        parameters,
        varargs: None,
    }
}

const fn variadic(varargs: &'static str) -> Constructor {
    Constructor {
        parameters: &[],
        varargs: Some(varargs),
    }
}

/// `TypeConstructor::Identity`: no modifiers.
const IDENTITY: &[Constructor] = &[fixed(&[])];
const FLOAT: &[Constructor] = &[fixed(&[]), fixed(&[("precision", "BIGINT")])];
const DECIMAL: &[Constructor] = &[fixed(&[]), fixed(&[("width", "UTINYINT"), ("scale", "UTINYINT")])];
const TIMESTAMP: &[Constructor] = &[fixed(&[]), fixed(&[("precision", "UTINYINT")])];
const VARCHAR: &[Constructor] = &[
    fixed(&[]),
    fixed(&[("length", "BIGINT")]),
    fixed(&[("collation", "VARCHAR")]),
];
const BIT: &[Constructor] = &[fixed(&[]), fixed(&[("length", "BIGINT")])];
const INTERVAL: &[Constructor] = &[fixed(&[]), fixed(&[("precision", "UTINYINT")])];
const ENUM: &[Constructor] = &[variadic("VARCHAR")];
const LIST: &[Constructor] = &[fixed(&[("child", "TYPE")])];
const ARRAY: &[Constructor] = &[fixed(&[("child", "TYPE"), ("size", "BIGINT")])];
const STRUCT: &[Constructor] = &[variadic("TYPE")];
const TUPLE: &[Constructor] = &[variadic("TYPE")];
const MAP: &[Constructor] = &[fixed(&[("key", "TYPE"), ("value", "TYPE")])];
const UNION: &[Constructor] = &[variadic("TYPE")];
const VARIANT: &[Constructor] = &[fixed(&[])];
const GEOMETRY: &[Constructor] = &[fixed(&[]), fixed(&[("crs", "VARCHAR")])];

/// `BUILTIN_TYPES`: (name, type, constructors).
pub static BUILTIN_TYPES: [(&str, LogicalTypeId, &[Constructor]); 83] = [
    ("decimal", LogicalTypeId::Decimal, DECIMAL),
    ("dec", LogicalTypeId::Decimal, DECIMAL),
    ("numeric", LogicalTypeId::Decimal, DECIMAL),
    ("time", LogicalTypeId::Time, IDENTITY),
    ("time_ns", LogicalTypeId::TimeNs, IDENTITY),
    ("date", LogicalTypeId::Date, IDENTITY),
    ("timestamp", LogicalTypeId::Timestamp, TIMESTAMP),
    ("datetime", LogicalTypeId::Timestamp, TIMESTAMP),
    ("timestamp_us", LogicalTypeId::Timestamp, IDENTITY),
    ("timestamp_ms", LogicalTypeId::TimestampMs, IDENTITY),
    ("timestamp_ns", LogicalTypeId::TimestampNs, IDENTITY),
    ("timestamp_s", LogicalTypeId::TimestampSec, IDENTITY),
    ("timestamptz", LogicalTypeId::TimestampTz, IDENTITY),
    ("timestamp with time zone", LogicalTypeId::TimestampTz, IDENTITY),
    ("timestamptz_ns", LogicalTypeId::TimestampTzNs, IDENTITY),
    ("timetz", LogicalTypeId::TimeTz, IDENTITY),
    ("time with time zone", LogicalTypeId::TimeTz, IDENTITY),
    ("interval", LogicalTypeId::Interval, INTERVAL),
    ("varchar", LogicalTypeId::Varchar, VARCHAR),
    ("bpchar", LogicalTypeId::Varchar, VARCHAR),
    ("string", LogicalTypeId::Varchar, VARCHAR),
    ("char", LogicalTypeId::Varchar, VARCHAR),
    ("nvarchar", LogicalTypeId::Varchar, VARCHAR),
    ("text", LogicalTypeId::Varchar, VARCHAR),
    ("blob", LogicalTypeId::Blob, IDENTITY),
    ("bytea", LogicalTypeId::Blob, IDENTITY),
    ("varbinary", LogicalTypeId::Blob, IDENTITY),
    ("binary", LogicalTypeId::Blob, IDENTITY),
    ("hugeint", LogicalTypeId::HugeInt, IDENTITY),
    ("int128", LogicalTypeId::HugeInt, IDENTITY),
    ("uhugeint", LogicalTypeId::UHugeInt, IDENTITY),
    ("uint128", LogicalTypeId::UHugeInt, IDENTITY),
    ("bigint", LogicalTypeId::BigInt, IDENTITY),
    ("oid", LogicalTypeId::BigInt, IDENTITY),
    ("long", LogicalTypeId::BigInt, IDENTITY),
    ("int8", LogicalTypeId::BigInt, IDENTITY),
    ("int64", LogicalTypeId::BigInt, IDENTITY),
    ("ubigint", LogicalTypeId::UBigInt, IDENTITY),
    ("uint64", LogicalTypeId::UBigInt, IDENTITY),
    ("integer", LogicalTypeId::Integer, IDENTITY),
    ("int", LogicalTypeId::Integer, IDENTITY),
    ("int4", LogicalTypeId::Integer, IDENTITY),
    ("signed", LogicalTypeId::Integer, IDENTITY),
    ("integral", LogicalTypeId::Integer, IDENTITY),
    ("int32", LogicalTypeId::Integer, IDENTITY),
    ("uinteger", LogicalTypeId::UInteger, IDENTITY),
    ("uint32", LogicalTypeId::UInteger, IDENTITY),
    ("smallint", LogicalTypeId::SmallInt, IDENTITY),
    ("int2", LogicalTypeId::SmallInt, IDENTITY),
    ("short", LogicalTypeId::SmallInt, IDENTITY),
    ("int16", LogicalTypeId::SmallInt, IDENTITY),
    ("usmallint", LogicalTypeId::USmallInt, IDENTITY),
    ("uint16", LogicalTypeId::USmallInt, IDENTITY),
    ("tinyint", LogicalTypeId::TinyInt, IDENTITY),
    ("int1", LogicalTypeId::TinyInt, IDENTITY),
    ("utinyint", LogicalTypeId::UTinyInt, IDENTITY),
    ("uint8", LogicalTypeId::UTinyInt, IDENTITY),
    ("struct", LogicalTypeId::Struct, STRUCT),
    ("row", LogicalTypeId::Struct, STRUCT),
    ("tuple", LogicalTypeId::Tuple, TUPLE),
    ("list", LogicalTypeId::List, LIST),
    ("array", LogicalTypeId::Array, ARRAY),
    ("map", LogicalTypeId::Map, MAP),
    ("union", LogicalTypeId::Union, UNION),
    ("bit", LogicalTypeId::Bit, BIT),
    ("bitstring", LogicalTypeId::Bit, BIT),
    ("variant", LogicalTypeId::Variant, VARIANT),
    ("bignum", LogicalTypeId::Bignum, IDENTITY),
    ("varint", LogicalTypeId::Bignum, IDENTITY),
    ("boolean", LogicalTypeId::Boolean, IDENTITY),
    ("bool", LogicalTypeId::Boolean, IDENTITY),
    ("logical", LogicalTypeId::Boolean, IDENTITY),
    ("uuid", LogicalTypeId::Uuid, IDENTITY),
    ("guid", LogicalTypeId::Uuid, IDENTITY),
    ("enum", LogicalTypeId::Enum, ENUM),
    ("null", LogicalTypeId::SqlNull, IDENTITY),
    ("float", LogicalTypeId::Float, FLOAT),
    ("real", LogicalTypeId::Float, IDENTITY),
    ("float4", LogicalTypeId::Float, IDENTITY),
    ("double", LogicalTypeId::Double, IDENTITY),
    ("float8", LogicalTypeId::Double, IDENTITY),
    ("geometry", LogicalTypeId::Geometry, GEOMETRY),
    ("type", LogicalTypeId::Type, IDENTITY),
];
