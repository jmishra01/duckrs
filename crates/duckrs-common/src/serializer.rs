// Ported from duckdb/src/common/serializer/{binary_serializer,binary_deserializer}.cpp,
// include/duckdb/common/serializer/{serializer,deserializer,encoding_util}.hpp, and the
// generated duckdb/src/storage/serialization/serialize_types.cpp @ rust-port-baseline
//
// DuckDB's binary format, used for the WAL, checkpoints and plan caches:
// - an object is a sequence of properties ended by the field id 0xFFFF;
// - a property is a little-endian u16 field id followed by its value;
// - integers are LEB128 varints (signed ones sign-extended), floats raw little-endian, bools one
//   byte, strings a varint length plus bytes, lists a varint count plus elements;
// - optional (nullable) values are a bool followed by the value when present;
// - properties equal to their default are omitted, and readers fill the default back in.

use crate::types::{Interval, LogicalType, LogicalTypeId, PhysicalType};
use crate::value::{Value, ValueData};
use crate::{Error, ErrorKind, Result};

/// Marks the end of an object.
pub const MESSAGE_TERMINATOR_FIELD_ID: u16 = 0xFFFF;

pub use crate::storage_version::StorageVersion;

#[derive(Debug, Clone, Copy)]
pub struct SerializationOptions {
    /// Write properties even when they equal their default (DuckDB's JSON output does).
    pub serialize_default_values: bool,
    pub target: StorageVersion,
}

impl Default for SerializationOptions {
    fn default() -> Self {
        Self {
            serialize_default_values: false,
            target: StorageVersion::V2_0_0,
        }
    }
}

/// A value that can be written inside a property or list.
pub trait Serialize {
    fn write_value(&self, s: &mut BinarySerializer);

    /// Whether the value equals `SerializationDefaultValue::GetDefault` (and may be omitted).
    fn is_default(&self) -> bool {
        false
    }
}

/// A value that can be read back.
pub trait Deserialize: Sized {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self>;

    /// The value assumed when an optional property is absent.
    fn default_value() -> Option<Self> {
        None
    }
}

#[derive(Debug, Default)]
pub struct BinarySerializer {
    buf: Vec<u8>,
    options: SerializationOptions,
}

impl BinarySerializer {
    pub fn new(options: SerializationOptions) -> Self {
        Self {
            buf: Vec::new(),
            options,
        }
    }

    /// Serializes `value` as a top-level object.
    pub fn serialize<T: Serialize + ?Sized>(value: &T, options: SerializationOptions) -> Vec<u8> {
        let mut s = Self::new(options);
        value.write_value(&mut s);
        s.buf
    }

    pub fn options(&self) -> &SerializationOptions {
        &self.options
    }

    /// Whether the target storage version includes a feature added in `version`.
    pub fn should_serialize(&self, version: StorageVersion) -> bool {
        self.options.target >= version
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// The id of a field, for a value that writes itself.
    pub fn write_field(&mut self, field_id: u16) {
        self.buf.extend_from_slice(&field_id.to_le_bytes());
    }

    pub fn write_varint_unsigned(&mut self, mut value: u64) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            self.buf.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    pub fn write_varint_signed(&mut self, mut value: i64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
                self.buf.push(byte);
                break;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes `value` wrapped in an object (`OnObjectBegin` ... `OnObjectEnd`).
    pub fn write_object_value(&mut self, body: impl FnOnce(&mut Self)) {
        body(self);
        self.write_field(MESSAGE_TERMINATOR_FIELD_ID);
    }

    /// `WriteProperty`.
    pub fn write_property<T: Serialize + ?Sized>(&mut self, field_id: u16, _tag: &str, value: &T) {
        self.write_field(field_id);
        value.write_value(self);
    }

    /// A property that is there when it is `Some`.
    pub fn write_optional_property<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: Option<&T>) {
        if let Some(value) = value {
            self.write_property(field_id, tag, value);
        }
    }

    /// `WritePropertyWithDefault`: omitted when the value is its type's default.
    pub fn write_property_with_default<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: &T) {
        if !self.options.serialize_default_values && value.is_default() {
            return;
        }
        self.write_property(field_id, tag, value);
    }

    /// `WritePropertyWithDefault(.., default_value)`: omitted when `value == default`.
    pub fn write_property_with_explicit_default<T: Serialize + PartialEq>(
        &mut self,
        field_id: u16,
        tag: &str,
        value: &T,
        default: &T,
    ) {
        if !self.options.serialize_default_values && value == default {
            return;
        }
        self.write_property(field_id, tag, value);
    }

    /// `WriteObject`: a property whose value is an object built by `body`.
    pub fn write_object(&mut self, field_id: u16, _tag: &str, body: impl FnOnce(&mut Self)) {
        self.write_field(field_id);
        self.write_object_value(body);
    }

    /// `WriteList`: a property holding `count` elements written by `element`.
    pub fn write_list(&mut self, field_id: u16, _tag: &str, count: usize, mut element: impl FnMut(&mut Self, usize)) {
        self.write_field(field_id);
        self.write_varint_unsigned(count as u64);
        for i in 0..count {
            element(self, i);
        }
    }
}

pub struct BinaryDeserializer<'a> {
    data: &'a [u8],
    pos: usize,
    peeked: Option<u16>,
    type_expression: Option<TypeExpressionReader>,
    from_before_v2: bool,
}

/// Reads a parsed expression that names a type, and gives its SQL. Parsed expressions are beyond
/// this crate, so what reads them passes this in.
pub type TypeExpressionReader = fn(&mut BinaryDeserializer<'_>) -> Result<String>;

fn serialization_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Serialization, message)
}

impl<'a> BinaryDeserializer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            peeked: None,
            type_expression: None,
            from_before_v2: false,
        }
    }

    /// For data of a database file written for a storage version before v2.0.0.
    pub fn from_before_v2(mut self, before: bool) -> Self {
        self.from_before_v2 = before;
        self
    }

    /// `StorageManager::GetStorageVersion() < StorageVersion::V2_0_0`.
    pub fn is_from_before_v2(&self) -> bool {
        self.from_before_v2
    }

    /// For data that can hold a type that is named by a parsed expression.
    pub fn with_type_expressions(mut self, reader: TypeExpressionReader) -> Self {
        self.type_expression = Some(reader);
        self
    }

    /// Reads a top-level object.
    pub fn deserialize<T: Deserialize>(data: &'a [u8]) -> Result<T> {
        let mut d = Self::new(data);
        let value = T::read_value(&mut d)?;
        if d.pos != data.len() {
            return Err(serialization_error(format!(
                "Failed to deserialize: {} trailing bytes",
                data.len() - d.pos
            )));
        }
        Ok(value)
    }

    pub fn read_raw(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| serialization_error("Failed to deserialize: unexpected end of data"))?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn next_field(&mut self) -> Result<u16> {
        if let Some(f) = self.peeked.take() {
            return Ok(f);
        }
        let b = self.read_raw(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn peek_field(&mut self) -> Result<u16> {
        if self.peeked.is_none() {
            self.peeked = Some(self.next_field()?);
        }
        Ok(self.peeked.expect("just set"))
    }

    pub fn read_varint_unsigned(&mut self, bits: u32) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = self.read_raw(1)?[0];
            if shift >= bits {
                return Err(Error::new(
                    ErrorKind::Io,
                    "Failed to decode LEB128 integer: data may be corrupt",
                ));
            }
            result |= u64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
    }

    pub fn read_varint_signed(&mut self, bits: u32) -> Result<i64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        let mut byte;
        loop {
            byte = self.read_raw(1)?[0];
            if shift >= bits {
                return Err(Error::new(
                    ErrorKind::Io,
                    "Failed to decode LEB128 integer: data may be corrupt",
                ));
            }
            result |= u64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        if shift < 64 && byte & 0x40 != 0 {
            result |= u64::MAX << shift;
        }
        Ok(result as i64)
    }

    /// How much of the data was read. Between objects that is where the next one starts.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// `ReadDataPtr`: bytes with their length in front.
    pub fn read_blob(&mut self) -> Result<&'a [u8]> {
        let length = self.read_varint_unsigned(64)?;
        let length = usize::try_from(length).map_err(|_| serialization_error("Failed to deserialize: a huge blob"))?;
        self.read_raw(length)
    }

    /// `ReadProperty(field_id, tag, ptr, count)`.
    pub fn read_blob_property(&mut self, field_id: u16) -> Result<&'a [u8]> {
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        self.read_blob()
    }

    /// `CanDeserializeProperty`: whether the next field is `field_id`.
    pub fn has_property(&mut self, field_id: u16) -> Result<bool> {
        Ok(self.peek_field()? == field_id)
    }

    /// `ReadProperty`: the next field must be `field_id`.
    pub fn read_property<T: Deserialize>(&mut self, field_id: u16, _tag: &str) -> Result<T> {
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        T::read_value(self)
    }

    /// Reads an optional property, returning `None` when the next field is a different one.
    pub fn read_optional_property<T: Deserialize>(&mut self, field_id: u16, _tag: &str) -> Result<Option<T>> {
        if self.peek_field()? != field_id {
            return Ok(None);
        }
        self.peeked = None;
        T::read_value(self).map(Some)
    }

    /// `ReadPropertyWithDefault`.
    pub fn read_property_with_default<T: Deserialize>(&mut self, field_id: u16, tag: &str) -> Result<T> {
        match self.read_optional_property(field_id, tag)? {
            Some(v) => Ok(v),
            None => T::default_value().ok_or_else(|| serialization_error(format!("property {tag} has no default"))),
        }
    }

    /// `ReadPropertyWithExplicitDefault`.
    pub fn read_property_with_explicit_default<T: Deserialize>(
        &mut self,
        field_id: u16,
        tag: &str,
        default: T,
    ) -> Result<T> {
        Ok(self.read_optional_property(field_id, tag)?.unwrap_or(default))
    }

    /// Skips a property that newer versions no longer write (`ReadDeletedProperty`).
    pub fn read_deleted_property<T: Deserialize>(&mut self, field_id: u16, tag: &str) -> Result<()> {
        self.read_optional_property::<T>(field_id, tag).map(drop)
    }

    /// Reads an object whose properties `body` reads, then its terminator.
    pub fn read_object_value<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let value = body(self)?;
        let field = self.next_field()?;
        if field != MESSAGE_TERMINATOR_FIELD_ID {
            return Err(serialization_error(format!(
                "Failed to deserialize: expected end of object, but found field id: {field}"
            )));
        }
        Ok(value)
    }

    /// `ReadProperty` of a value that reads its own object, which `body` does.
    pub fn read_object_field<T>(
        &mut self,
        field_id: u16,
        _tag: &str,
        body: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        body(self)
    }

    /// `ReadObject`.
    pub fn read_object<T>(
        &mut self,
        field_id: u16,
        _tag: &str,
        body: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        self.read_object_value(body)
    }

    /// `ReadList`.
    pub fn read_list<T>(
        &mut self,
        field_id: u16,
        _tag: &str,
        mut element: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        let count = self.read_varint_unsigned(64)?;
        (0..count).map(|_| element(self)).collect()
    }
}

macro_rules! varint_unsigned {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.write_varint_unsigned(u64::from(*self));
            }
            fn is_default(&self) -> bool {
                *self == 0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                Ok(d.read_varint_unsigned(<$t>::BITS)? as $t)
            }
            fn default_value() -> Option<Self> {
                Some(0)
            }
        }
    )*};
}
varint_unsigned!(u8, u16, u32, u64);

macro_rules! varint_signed {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.write_varint_signed(i64::from(*self));
            }
            fn is_default(&self) -> bool {
                *self == 0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                Ok(d.read_varint_signed(<$t>::BITS)? as $t)
            }
            fn default_value() -> Option<Self> {
                Some(0)
            }
        }
    )*};
}
varint_signed!(i8, i16, i32, i64);

impl Serialize for bool {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_raw(&[u8::from(*self)]);
    }
    fn is_default(&self) -> bool {
        !*self
    }
}

impl Deserialize for bool {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        Ok(d.read_raw(1)?[0] != 0)
    }
    fn default_value() -> Option<Self> {
        Some(false)
    }
}

/// HUGEINT: the signed upper 64 bits, then the unsigned lower 64 bits.
impl Serialize for i128 {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_varint_signed((*self >> 64) as i64);
        s.write_varint_unsigned(*self as u64);
    }
}

impl Deserialize for i128 {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let upper = d.read_varint_signed(64)?;
        let lower = d.read_varint_unsigned(64)?;
        Ok((i128::from(upper) << 64) | i128::from(lower))
    }
}

impl Serialize for u128 {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_varint_unsigned((*self >> 64) as u64);
        s.write_varint_unsigned(*self as u64);
    }
}

impl Deserialize for u128 {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let upper = d.read_varint_unsigned(64)?;
        let lower = d.read_varint_unsigned(64)?;
        Ok((u128::from(upper) << 64) | u128::from(lower))
    }
}

macro_rules! raw_float {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.write_raw(&self.to_le_bytes());
            }
            fn is_default(&self) -> bool {
                *self == 0.0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                let bytes = d.read_raw(size_of::<$t>())?;
                Ok(<$t>::from_le_bytes(bytes.try_into().expect("exact length")))
            }
            fn default_value() -> Option<Self> {
                Some(0.0)
            }
        }
    )*};
}
raw_float!(f32, f64);

impl Serialize for str {
    fn write_value(&self, s: &mut BinarySerializer) {
        self.as_bytes().write_value(s);
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl Serialize for [u8] {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_varint_unsigned(self.len() as u64);
        s.write_raw(self);
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl Serialize for String {
    fn write_value(&self, s: &mut BinarySerializer) {
        self.as_str().write_value(s);
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl Deserialize for String {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let len = d.read_varint_unsigned(32)? as usize;
        let bytes = d.read_raw(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| serialization_error("Failed to deserialize: invalid UTF-8 string"))
    }
    fn default_value() -> Option<Self> {
        Some(String::new())
    }
}

impl<T: Serialize> Serialize for Vec<T> {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_varint_unsigned(self.len() as u64);
        for item in self {
            item.write_value(s);
        }
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl<T: Deserialize> Deserialize for Vec<T> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let count = d.read_varint_unsigned(64)?;
        (0..count).map(|_| T::read_value(d)).collect()
    }
    fn default_value() -> Option<Self> {
        Some(Vec::new())
    }
}

/// A nullable value: a presence flag, then the value (`WriteValue(const T *)`).
impl<T: Serialize> Serialize for Option<T> {
    fn write_value(&self, s: &mut BinarySerializer) {
        match self {
            None => false.write_value(s),
            Some(v) => {
                true.write_value(s);
                v.write_value(s);
            }
        }
    }
    fn is_default(&self) -> bool {
        self.is_none()
    }
}

impl<T: Deserialize> Deserialize for Option<T> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        if bool::read_value(d)? {
            T::read_value(d).map(Some)
        } else {
            Ok(None)
        }
    }
    fn default_value() -> Option<Self> {
        Some(None)
    }
}

impl<T: Serialize> Serialize for Box<T> {
    fn write_value(&self, s: &mut BinarySerializer) {
        (**self).write_value(s);
    }
    fn is_default(&self) -> bool {
        (**self).is_default()
    }
}

impl<T: Deserialize> Deserialize for Box<T> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        T::read_value(d).map(Box::new)
    }
}

/// A pair is an object with properties `0: first` and `1: second`.
impl<A: Serialize, B: Serialize> Serialize for (A, B) {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| {
            s.write_property(0, "first", &self.0);
            s.write_property(1, "second", &self.1);
        });
    }
}

impl<A: Deserialize, B: Deserialize> Deserialize for (A, B) {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| Ok((d.read_property(0, "first")?, d.read_property(1, "second")?)))
    }
}

impl Serialize for Interval {
    /// `interval_t::Serialize`.
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| {
            s.write_property_with_default(1, "months", &self.months);
            s.write_property_with_default(2, "days", &self.days);
            s.write_property_with_default(3, "micros", &self.micros);
        });
    }
}

impl Deserialize for Interval {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            Ok(Interval::new(
                d.read_property_with_default(1, "months")?,
                d.read_property_with_default(2, "days")?,
                d.read_property_with_default(3, "micros")?,
            ))
        })
    }
}

/// `LogicalTypeInfoType`.
mod type_info {
    pub const GENERIC: u8 = 1;
    pub const DECIMAL: u8 = 2;
    pub const STRING: u8 = 3;
    pub const LIST: u8 = 4;
    pub const STRUCT: u8 = 5;
    pub const ENUM: u8 = 6;
    pub const UNBOUND: u8 = 7;
    pub const ARRAY: u8 = 9;
}

fn logical_type_id_from(raw: u8) -> Result<LogicalTypeId> {
    use LogicalTypeId::*;
    const ALL: [LogicalTypeId; 54] = [
        Invalid,
        SqlNull,
        Unknown,
        Any,
        Unbound,
        Template,
        Type,
        Boolean,
        TinyInt,
        SmallInt,
        Integer,
        BigInt,
        Date,
        Time,
        TimestampSec,
        TimestampMs,
        Timestamp,
        TimestampNs,
        Decimal,
        Float,
        Double,
        Char,
        Varchar,
        Blob,
        Interval,
        UTinyInt,
        USmallInt,
        UInteger,
        UBigInt,
        TimestampTz,
        TimestampTzNs,
        TimeTz,
        TimeNs,
        Bit,
        StringLiteral,
        IntegerLiteral,
        Bignum,
        UHugeInt,
        HugeInt,
        Pointer,
        Validity,
        Uuid,
        Geometry,
        Struct,
        List,
        Map,
        Table,
        Enum,
        LegacyAggregateState,
        Lambda,
        Union,
        Array,
        Variant,
        Tuple,
    ];
    ALL.iter()
        .copied()
        .find(|id| *id as u8 == raw)
        .ok_or_else(|| serialization_error(format!("Failed to deserialize: unknown LogicalTypeId {raw}")))
}

impl Serialize for LogicalTypeId {
    fn write_value(&self, s: &mut BinarySerializer) {
        (*self as u8).write_value(s);
    }
}

impl Deserialize for LogicalTypeId {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        logical_type_id_from(u8::read_value(d)?)
    }
}

impl Serialize for LogicalType {
    /// `LogicalType::Serialize`: the id, then the type info object if the type has parameters
    /// or an alias.
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| {
            let id = if self.id() == LogicalTypeId::Tuple && !s.should_serialize(StorageVersion::V2_0_0) {
                LogicalTypeId::Struct
            } else {
                self.id()
            };
            s.write_property(100, "id", &id);
            let info_type = match self.id() {
                // `StringTypeInfo`: a VARCHAR's collation
                LogicalTypeId::Varchar if self.collation().is_some() => Some(type_info::STRING),
                _ if !self.has_params() && self.alias().is_none() => None,
                LogicalTypeId::Decimal => Some(type_info::DECIMAL),
                LogicalTypeId::List | LogicalTypeId::Map => Some(type_info::LIST),
                LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union => Some(type_info::STRUCT),
                LogicalTypeId::Array => Some(type_info::ARRAY),
                LogicalTypeId::Enum => Some(type_info::ENUM),
                _ if self.has_params() => Some(type_info::STRING),
                _ => Some(type_info::GENERIC),
            };
            let Some(info_type) = info_type else { return };
            // type_info is a nullable pointer: presence flag, then the info object.
            s.write_field(101);
            true.write_value(s);
            s.write_object_value(|s| {
                s.write_property(100, "type", &info_type);
                s.write_property_with_default(101, "alias", self.alias().unwrap_or(""));
                match info_type {
                    type_info::DECIMAL => {
                        let (w, sc) = self.decimal_properties().expect("DECIMAL has properties");
                        s.write_property_with_default(200, "width", &w);
                        s.write_property_with_default(201, "scale", &sc);
                    }
                    type_info::LIST => {
                        s.write_property(200, "child_type", self.child_type().expect("LIST has a child"));
                    }
                    type_info::ARRAY => {
                        s.write_property(200, "child_type", self.child_type().expect("ARRAY has a child"));
                        s.write_property_with_default(201, "size", &self.array_size().expect("ARRAY has a size"));
                    }
                    type_info::STRUCT => {
                        let children: Vec<(String, LogicalType)> = self.struct_children().to_vec();
                        s.write_property_with_default(200, "child_types", &children);
                    }
                    type_info::STRING if self.collation().is_some() => {
                        s.write_property_with_default(200, "collation", self.collation().unwrap_or(""));
                    }
                    type_info::ENUM => {
                        let values = self.enum_values();
                        s.write_property(200, "values_count", &(values.len() as u64));
                        s.write_list(201, "values", values.len(), |s, i| values[i].write_value(s));
                    }
                    _ => {}
                }
            });
        });
    }
}

impl Deserialize for LogicalType {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let mut id: LogicalTypeId = d.read_property(100, "id")?;
            let present = match d.read_optional_property::<bool>(101, "type_info")? {
                None | Some(false) => return Ok(LogicalType::new(id)),
                Some(true) => true,
            };
            debug_assert!(present);
            d.read_object_value(|d| {
                let info_type: u8 = d.read_property(100, "type")?;
                let alias: String = d.read_property_with_default(101, "alias")?;
                d.read_deleted_property::<Vec<Value>>(102, "modifiers")?;
                let base = match info_type {
                    type_info::GENERIC | type_info::STRING => {
                        let collation: String = if info_type == type_info::STRING {
                            d.read_property_with_default(200, "collation")?
                        } else {
                            String::new()
                        };
                        let ty = LogicalType::new(id);
                        if id == LogicalTypeId::Varchar && !collation.is_empty() {
                            ty.with_collation(Some(&collation))
                        } else {
                            ty
                        }
                    }
                    type_info::DECIMAL => {
                        let w: u8 = d.read_property_with_default(200, "width")?;
                        let sc: u8 = d.read_property_with_default(201, "scale")?;
                        LogicalType::decimal(w, sc)
                    }
                    type_info::LIST => {
                        let child: LogicalType = d.read_property(200, "child_type")?;
                        if id == LogicalTypeId::Map {
                            let kv = child.struct_children();
                            LogicalType::map(kv[0].1.clone(), kv[1].1.clone())
                        } else {
                            LogicalType::list(child)
                        }
                    }
                    type_info::ARRAY => {
                        let child: LogicalType = d.read_property(200, "child_type")?;
                        let size: u32 = d.read_property_with_default(201, "size")?;
                        LogicalType::array(child, size)
                    }
                    type_info::STRUCT => {
                        let children: Vec<(String, LogicalType)> = d.read_property_with_default(200, "child_types")?;
                        match id {
                            LogicalTypeId::Union => LogicalType::union_type(children.into_iter().skip(1).collect()),
                            _ => {
                                // Unnamed structs written for older versions read back as TUPLE.
                                if id == LogicalTypeId::Struct
                                    && !children.is_empty()
                                    && children.iter().all(|(n, _)| n.is_empty())
                                {
                                    id = LogicalTypeId::Tuple;
                                }
                                LogicalType::struct_type(children).with_id(id)
                            }
                        }
                    }
                    type_info::ENUM => {
                        let _count: u64 = d.read_property(200, "values_count")?;
                        let values = d.read_list(201, "values", String::read_value)?;
                        LogicalType::enum_type(values)
                    }
                    type_info::UNBOUND => read_unbound_type(d)?,
                    other => {
                        return Err(Error::not_implemented(format!(
                            "deserializing type info {other} is not supported yet"
                        )));
                    }
                };
                Ok(if alias.is_empty() { base } else { base.with_alias(alias) })
            })
        })
    }
}

/// `UnboundTypeInfo::Deserialize`: a parsed expression, or the name of a "USER" type before v1.5.0.
fn read_unbound_type(d: &mut BinaryDeserializer<'_>) -> Result<LogicalType> {
    if d.has_property(204)? {
        let Some(reader) = d.type_expression else {
            return Err(Error::not_implemented(
                "deserializing a type expression here is not supported yet",
            ));
        };
        return d
            .read_object_field(204, "expr", |d| {
                if !bool::read_value(d)? {
                    return Err(serialization_error(
                        "Failed to deserialize: a type expression is missing",
                    ));
                }
                reader(d)
            })
            .map(LogicalType::unbound);
    }
    let name: String = d.read_property_with_default(200, "name")?;
    let catalog: String = d.read_property_with_default(201, "catalog")?;
    let schema: String = d.read_property_with_default(202, "schema")?;
    let modifiers: Vec<Value> = d.read_property_with_default(203, "user_type_modifiers")?;
    let mut sql = String::new();
    for part in [&catalog, &schema].into_iter().filter(|part| !part.is_empty()) {
        sql += &crate::types::sql_identifier(part);
        sql.push('.');
    }
    sql += &crate::types::sql_identifier(&name);
    if !modifiers.is_empty() {
        let modifiers = modifiers.iter().map(Value::try_to_string).collect::<Result<Vec<_>>>()?;
        sql += &format!("({})", modifiers.join(", "));
    }
    Ok(LogicalType::unbound(sql))
}

impl Serialize for Value {
    /// `Value::Serialize`: the root value writes its type; children of nested values do not
    /// (since storage v1.2).
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| write_value_body(self, s, true));
    }
}

fn write_value_body(v: &Value, s: &mut BinarySerializer, with_type: bool) {
    if with_type || !s.should_serialize(StorageVersion::V1_2_0) {
        s.write_property(100, "type", v.logical_type());
    }
    s.write_property(101, "is_null", &v.is_null());
    let ty = v.logical_type();
    match v.data() {
        ValueData::Null => {}
        ValueData::Bool(x) => s.write_property(102, "value", x),
        ValueData::Int8(x) => s.write_property(102, "value", x),
        ValueData::Int16(x) => s.write_property(102, "value", x),
        ValueData::Int32(x) => s.write_property(102, "value", x),
        ValueData::Int64(x) => s.write_property(102, "value", x),
        ValueData::Int128(x) => s.write_property(102, "value", x),
        ValueData::UInt8(x) => s.write_property(102, "value", x),
        ValueData::UInt16(x) => s.write_property(102, "value", x),
        ValueData::UInt32(x) => s.write_property(102, "value", x),
        ValueData::UInt64(x) => s.write_property(102, "value", x),
        ValueData::UInt128(x) => s.write_property(102, "value", x),
        ValueData::Float(x) => s.write_property(102, "value", x),
        ValueData::Double(x) => s.write_property(102, "value", x),
        ValueData::Interval(x) => s.write_property(102, "value", x),
        ValueData::Bytes(b) if ty.id() == LogicalTypeId::Blob => {
            s.write_property(102, "value", crate::string_cast::format_blob(b).as_str())
        }
        ValueData::Bytes(b) => s.write_property(102, "value", b.as_slice()),
        ValueData::List(children) | ValueData::Struct(children) => {
            s.write_object(102, "value", |s| {
                s.write_list(100, "children", children.len(), |s, i| {
                    s.write_object_value(|s| write_value_body(&children[i], s, false));
                });
            });
        }
    }
}

impl Deserialize for Value {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let ty: LogicalType = d.read_property(100, "type")?;
            read_value_body(d, ty)
        })
    }
}

fn child_types(ty: &LogicalType, count: usize) -> Vec<LogicalType> {
    match ty.physical_type() {
        PhysicalType::List | PhysicalType::Array => vec![ty.child_type().expect("has a child").clone(); count],
        _ => ty.struct_children().iter().map(|(_, t)| t.clone()).collect(),
    }
}

fn read_value_body(d: &mut BinaryDeserializer<'_>, ty: LogicalType) -> Result<Value> {
    let is_null: bool = d.read_property(101, "is_null")?;
    if is_null {
        return Ok(Value::null(ty));
    }
    let data = match ty.physical_type() {
        PhysicalType::Bool => ValueData::Bool(d.read_property(102, "value")?),
        PhysicalType::Int8 => ValueData::Int8(d.read_property(102, "value")?),
        PhysicalType::Int16 => ValueData::Int16(d.read_property(102, "value")?),
        PhysicalType::Int32 => ValueData::Int32(d.read_property(102, "value")?),
        PhysicalType::Int64 => ValueData::Int64(d.read_property(102, "value")?),
        PhysicalType::Int128 => ValueData::Int128(d.read_property(102, "value")?),
        PhysicalType::UInt8 => ValueData::UInt8(d.read_property(102, "value")?),
        PhysicalType::UInt16 => ValueData::UInt16(d.read_property(102, "value")?),
        PhysicalType::UInt32 => ValueData::UInt32(d.read_property(102, "value")?),
        PhysicalType::UInt64 => ValueData::UInt64(d.read_property(102, "value")?),
        PhysicalType::UInt128 => ValueData::UInt128(d.read_property(102, "value")?),
        PhysicalType::Float => ValueData::Float(d.read_property(102, "value")?),
        PhysicalType::Double => ValueData::Double(d.read_property(102, "value")?),
        PhysicalType::Interval => ValueData::Interval(d.read_property(102, "value")?),
        PhysicalType::Varchar if ty.id() == LogicalTypeId::Blob => {
            let text: String = d.read_property(102, "value")?;
            ValueData::Bytes(parse_blob_text(&text)?)
        }
        PhysicalType::Varchar => {
            let field = d.next_field()?;
            if field != 102 {
                return Err(serialization_error(format!(
                    "Failed to deserialize: field id mismatch, expected: 102, got: {field}"
                )));
            }
            let len = d.read_varint_unsigned(32)? as usize;
            ValueData::Bytes(d.read_raw(len)?.to_vec())
        }
        PhysicalType::List | PhysicalType::Struct | PhysicalType::Array => {
            let children = d.read_object(102, "value", |d| {
                let field = d.next_field()?;
                if field != 100 {
                    return Err(serialization_error(format!(
                        "Failed to deserialize: field id mismatch, expected: 100, got: {field}"
                    )));
                }
                let count = d.read_varint_unsigned(64)? as usize;
                let types = child_types(&ty, count);
                types
                    .into_iter()
                    .map(|t| d.read_object_value(|d| read_value_body(d, t)))
                    .collect::<Result<Vec<_>>>()
            })?;
            if ty.physical_type() == PhysicalType::Struct {
                ValueData::Struct(children)
            } else {
                ValueData::List(children)
            }
        }
        other => {
            return Err(Error::not_implemented(format!(
                "deserializing {other} values is not supported"
            )));
        }
    };
    Value::new(ty, data)
}

/// Parses `Blob::ToString` output back into bytes (`Blob::ToBlob`).
fn parse_blob_text(text: &str) -> Result<Vec<u8>> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            let hex = std::str::from_utf8(b.get(i + 2..i + 4).unwrap_or(&[]))
                .ok()
                .filter(|_| b.get(i + 1) == Some(&b'x'))
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| serialization_error("Failed to deserialize: invalid blob escape"))?;
            out.push(hex);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
