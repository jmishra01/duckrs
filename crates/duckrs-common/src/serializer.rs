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

use crate::json::Json;
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

/// The names of the members of a C++ enum, as `EnumUtil` writes and reads them: the first name of
/// a value is the one written, any of them is read (ignoring case).
pub trait EnumNames {
    /// The name of the enum, for errors.
    const NAME: &'static str;
    const NAMES: &'static [(&'static str, u64)];
}

/// A value of an enum: a number in a binary file, its name in JSON (`serialize_enum_as_string`).
pub struct Enum<N: EnumNames>(pub u64, std::marker::PhantomData<N>);

impl<N: EnumNames> Enum<N> {
    pub fn new(value: u64) -> Self {
        Self(value, std::marker::PhantomData)
    }
}

impl<N: EnumNames> Serialize for Enum<N> {
    fn write_value(&self, s: &mut BinarySerializer) {
        if s.is_json() {
            match N::NAMES.iter().find(|(_, value)| *value == self.0) {
                Some((name, _)) => s.write_str(name),
                None => s.write_u64(self.0),
            }
        } else {
            s.write_varint_unsigned(self.0);
        }
    }
    fn is_default(&self) -> bool {
        self.0 == 0
    }
}

impl<N: EnumNames> Deserialize for Enum<N> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        if d.json.is_none() {
            return Ok(Self::new(d.read_varint_unsigned(64)?));
        }
        let name = d.read_string()?;
        N::NAMES
            .iter()
            .find(|(member, _)| member.eq_ignore_ascii_case(&name))
            .map(|(_, value)| Self::new(*value))
            .ok_or_else(|| {
                Error::not_implemented(format!(
                    "Enum value: '{name}' not implemented in FromChars<{}>",
                    N::NAME
                ))
            })
    }
    fn default_value() -> Option<Self> {
        Some(Self::new(0))
    }
}

/// A JSON value being built: an array or an object, and the key it goes under in its parent.
#[derive(Debug)]
struct OpenJson {
    key: Option<String>,
    value: Json,
    /// Whether it may be left out of its parent when empty (`stack_can_be_omitted`).
    can_be_omitted: bool,
    /// A list is in its parent unless it was left out at its start.
    attached: bool,
}

/// `JsonSerializer`: what writes the properties of an object as members of a JSON object.
#[derive(Debug)]
struct JsonWriter {
    stack: Vec<OpenJson>,
    tag: String,
    property_can_be_omitted: Vec<bool>,
    skip_if_null: bool,
    skip_if_empty: bool,
}

impl JsonWriter {
    fn current_is_object(&self) -> bool {
        matches!(self.stack.last().map(|frame| &frame.value), Some(Json::Object(_)))
    }

    fn current_property_can_be_omitted(&self) -> bool {
        self.property_can_be_omitted.last().copied().unwrap_or(false)
    }

    fn omittable(&self) -> bool {
        self.current_is_object() && self.current_property_can_be_omitted()
    }

    fn push(&mut self, value: Json) {
        let tag = self.tag.clone();
        match &mut self.stack.last_mut().expect("the root").value {
            Json::Array(items) => items.push(value),
            Json::Object(items) => items.push((tag, value)),
            _ => unreachable!("only arrays and objects are open"),
        }
    }

    fn begin(&mut self, value: Json, attached: bool) {
        let can_be_omitted = self.omittable();
        let key = self.current_is_object().then(|| self.tag.clone());
        self.stack.push(OpenJson {
            key,
            value,
            can_be_omitted,
            attached,
        });
    }

    fn end(&mut self) {
        let frame = self.stack.pop().expect("an open value");
        let keep = match &frame.value {
            Json::Object(items) => !(items.is_empty() && self.skip_if_empty && frame.can_be_omitted),
            _ => frame.attached,
        };
        if !keep {
            return;
        }
        match &mut self.stack.last_mut().expect("the root").value {
            Json::Array(items) => items.push(frame.value),
            Json::Object(items) => items.push((frame.key.unwrap_or_default(), frame.value)),
            _ => unreachable!("only arrays and objects are open"),
        }
    }
}

#[derive(Debug, Default)]
pub struct BinarySerializer {
    buf: Vec<u8>,
    options: SerializationOptions,
    json: Option<JsonWriter>,
}

impl BinarySerializer {
    pub fn new(options: SerializationOptions) -> Self {
        Self {
            buf: Vec::new(),
            options,
            json: None,
        }
    }

    /// Serializes `value` as a top-level object.
    pub fn serialize<T: Serialize + ?Sized>(value: &T, options: SerializationOptions) -> Vec<u8> {
        let mut s = Self::new(options);
        value.write_value(&mut s);
        s.buf
    }

    /// `JsonSerializer::Serialize`: `value` as JSON, which leaves out nulls and empty values that
    /// could be left out as asked, and writes defaults unless `options` says not to.
    pub fn serialize_json<T: Serialize + ?Sized>(
        value: &T,
        options: SerializationOptions,
        skip_if_null: bool,
        skip_if_empty: bool,
    ) -> Json {
        let mut s = Self::new(options);
        s.json = Some(JsonWriter {
            stack: vec![OpenJson {
                key: None,
                value: Json::Array(Vec::new()),
                can_be_omitted: false,
                attached: true,
            }],
            tag: String::new(),
            property_can_be_omitted: Vec::new(),
            skip_if_null,
            skip_if_empty,
        });
        value.write_value(&mut s);
        let mut writer = s.json.take().expect("set above");
        match writer.stack.pop().map(|frame| frame.value) {
            Some(Json::Array(mut items)) if !items.is_empty() => items.swap_remove(0),
            _ => Json::Object(Vec::new()),
        }
    }

    pub fn is_json(&self) -> bool {
        self.json.is_some()
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

    // `WriteValue` of the primitive types.

    pub fn write_u64(&mut self, value: u64) {
        match &mut self.json {
            Some(json) => json.push(Json::UInt(value)),
            None => self.write_varint_unsigned(value),
        }
    }

    pub fn write_i64(&mut self, value: i64) {
        match &mut self.json {
            Some(json) => json.push(Json::Int(value)),
            None => self.write_varint_signed(value),
        }
    }

    pub fn write_bool(&mut self, value: bool) {
        match &mut self.json {
            Some(json) => json.push(Json::Bool(value)),
            None => self.buf.push(u8::from(value)),
        }
    }

    /// `DoubleToJSONValue`: infinities and NaN are raw text in JSON.
    fn write_json_real(&mut self, value: f64) {
        let json = self.json.as_mut().expect("JSON");
        if value.is_finite() {
            json.push(Json::Real(value));
        } else if value.is_nan() {
            json.push(Json::Raw("NAN".to_string()));
        } else {
            json.push(Json::Raw(
                if value < 0.0 { "-Infinity" } else { "Infinity" }.to_string(),
            ));
        }
    }

    pub fn write_f64(&mut self, value: f64) {
        if self.json.is_some() {
            self.write_json_real(value);
        } else {
            self.write_raw(&value.to_le_bytes());
        }
    }

    pub fn write_f32(&mut self, value: f32) {
        if self.json.is_some() {
            self.write_json_real(f64::from(value));
        } else {
            self.write_raw(&value.to_le_bytes());
        }
    }

    pub fn write_str(&mut self, value: &str) {
        match &mut self.json {
            Some(json) => {
                if !(json.skip_if_empty && value.is_empty() && json.omittable()) {
                    json.push(Json::Str(value.to_string()));
                }
            }
            None => {
                self.write_varint_unsigned(value.len() as u64);
                self.write_raw(value.as_bytes());
            }
        }
    }

    /// `WriteDataPtr`: bytes, which JSON has as the text of a BLOB.
    pub fn write_bytes(&mut self, value: &[u8]) {
        match &mut self.json {
            Some(json) => json.push(Json::Str(crate::string_cast::format_blob(value))),
            None => {
                self.write_varint_unsigned(value.len() as u64);
                self.write_raw(value);
            }
        }
    }

    /// `WriteNull`.
    fn write_null(&mut self) {
        let json = self.json.as_mut().expect("JSON");
        if json.skip_if_null && json.omittable() {
            return;
        }
        json.push(Json::Null);
    }

    /// `OnNullableBegin`: whether the value of a pointer follows.
    pub fn begin_nullable(&mut self, present: bool) {
        if self.json.is_some() {
            if !present {
                self.write_null();
            }
        } else {
            self.buf.push(u8::from(present));
        }
    }

    /// `OnListBegin`.
    pub fn begin_list(&mut self, count: usize) {
        match &mut self.json {
            Some(json) => {
                let attached = !(count == 0 && json.skip_if_empty && json.omittable());
                json.begin(Json::Array(Vec::new()), attached);
            }
            None => self.write_varint_unsigned(count as u64),
        }
    }

    /// `OnListEnd`.
    pub fn end_list(&mut self) {
        if let Some(json) = &mut self.json {
            json.end();
        }
    }

    fn begin_property(&mut self, field_id: u16, tag: &str, can_be_omitted: bool) {
        match &mut self.json {
            Some(json) => {
                json.tag = tag.to_string();
                json.property_can_be_omitted.push(can_be_omitted);
            }
            None => self.write_field(field_id),
        }
    }

    fn end_property(&mut self) {
        if let Some(json) = &mut self.json {
            json.property_can_be_omitted.pop();
        }
    }

    /// Writes `value` wrapped in an object (`OnObjectBegin` ... `OnObjectEnd`).
    pub fn write_object_value(&mut self, body: impl FnOnce(&mut Self)) {
        if let Some(json) = &mut self.json {
            json.begin(Json::Object(Vec::new()), true);
            body(self);
            self.json.as_mut().expect("JSON").end();
            return;
        }
        body(self);
        self.write_field(MESSAGE_TERMINATOR_FIELD_ID);
    }

    /// `WriteProperty`.
    pub fn write_property<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: &T) {
        self.begin_property(field_id, tag, false);
        value.write_value(self);
        self.end_property();
    }

    /// `OnOptionalPropertyBegin(.., true)`: a property that may be left out.
    fn write_present_property<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: &T) {
        self.begin_property(field_id, tag, true);
        value.write_value(self);
        self.end_property();
    }

    /// A property that is there when it is `Some`.
    pub fn write_optional_property<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: Option<&T>) {
        if let Some(value) = value {
            self.write_present_property(field_id, tag, value);
        }
    }

    /// `WritePropertyWithDefault`: omitted when the value is its type's default.
    pub fn write_property_with_default<T: Serialize + ?Sized>(&mut self, field_id: u16, tag: &str, value: &T) {
        if !self.options.serialize_default_values && value.is_default() {
            return;
        }
        self.write_present_property(field_id, tag, value);
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
        self.write_present_property(field_id, tag, value);
    }

    /// `WriteObject`: a property whose value is an object built by `body`.
    pub fn write_object(&mut self, field_id: u16, tag: &str, body: impl FnOnce(&mut Self)) {
        self.begin_property(field_id, tag, false);
        self.write_object_value(body);
        self.end_property();
    }

    /// `WriteList`: a property holding `count` elements written by `element`.
    pub fn write_list(&mut self, field_id: u16, tag: &str, count: usize, mut element: impl FnMut(&mut Self, usize)) {
        self.begin_property(field_id, tag, false);
        self.begin_list(count);
        for i in 0..count {
            element(self, i);
        }
        self.end_list();
        self.end_property();
    }
}

pub struct BinaryDeserializer<'a> {
    data: &'a [u8],
    pos: usize,
    peeked: Option<u16>,
    type_expression: Option<TypeExpressionReader>,
    from_before_v2: bool,
    json: Option<JsonReader<'a>>,
    disallow_pointers: bool,
}

/// An open JSON value: the members of an object, or an array and how much of it was read.
enum JsonFrame<'a> {
    Object(&'a Json),
    Array(&'a [Json], usize),
}

/// `JsonDeserializer`: what reads the properties of an object by their names.
struct JsonReader<'a> {
    stack: Vec<JsonFrame<'a>>,
    tag: String,
}

/// `yyjson_get_type_desc`.
fn json_type_desc(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(true) => "true",
        Json::Bool(false) => "false",
        Json::UInt(_) => "uint",
        Json::Int(_) => "sint",
        Json::Real(_) => "real",
        Json::Raw(_) => "raw",
        Json::Str(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

fn parser_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Parser, message)
}

impl<'a> JsonReader<'a> {
    fn member(object: &'a Json, tag: &str) -> Option<&'a Json> {
        match object {
            Json::Object(members) => members.iter().find(|(key, _)| key == tag).map(|(_, value)| value),
            _ => None,
        }
    }

    fn current_has(&self, tag: &str) -> bool {
        match self.stack.last() {
            Some(JsonFrame::Object(object)) => Self::member(object, tag).is_some(),
            _ => false,
        }
    }

    /// `GetNextValue`: the member of the current tag, or the next element of an array.
    fn next_value(&mut self, consume: bool) -> Result<&'a Json> {
        match self.stack.last_mut().expect("the root") {
            JsonFrame::Object(object) => Self::member(object, &self.tag).ok_or_else(|| {
                parser_error(format!(
                    "Expected but did not find property '{}' in json object: '{}'",
                    self.tag,
                    object.text()
                ))
            }),
            JsonFrame::Array(items, at) => {
                let Some(value) = items.get(*at) else {
                    return Err(parser_error(format!(
                        "Expected but did not find another value after exhausting json array: '{}'",
                        Json::Array(items.to_vec()).text()
                    )));
                };
                if consume {
                    *at += 1;
                }
                Ok(value)
            }
        }
    }

    /// `ThrowTypeError`.
    fn type_error(&self, value: &Json, expected: &str) -> Error {
        let actual = json_type_desc(value);
        match self.stack.last() {
            Some(JsonFrame::Array(..)) => parser_error(format!(
                "Sequence expect child of type '{expected}', but got type: {actual}"
            )),
            _ => parser_error(format!(
                "property '{}' expected type '{expected}', but got type: '{actual}'",
                self.tag
            )),
        }
    }
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
            json: None,
            disallow_pointers: false,
        }
    }

    /// `JsonDeserializer`: reads the objects of a JSON document, the properties by their names.
    pub fn from_json(root: &'a Json) -> Self {
        let mut d = Self::new(&[]);
        d.json = Some(JsonReader {
            stack: vec![JsonFrame::Array(std::slice::from_ref(root), 0)],
            tag: String::new(),
        });
        d
    }

    pub fn is_json(&self) -> bool {
        self.json.is_some()
    }

    fn json_value(&mut self) -> Result<(&'a Json, &JsonReader<'a>)> {
        let json = self.json.as_mut().expect("JSON");
        let value = json.next_value(true)?;
        Ok((value, json))
    }

    // `Read*` of the primitive types.

    /// An unsigned integer of `bits` bits, named `name` in C++ (`ReadUnsignedInt*`).
    pub fn read_uint(&mut self, bits: u32, name: &str) -> Result<u64> {
        if self.json.is_none() {
            return self.read_varint_unsigned(bits);
        }
        let (value, json) = self.json_value()?;
        let Json::UInt(number) = value else {
            return Err(json.type_error(value, name));
        };
        if bits < 64 && *number > (1u64 << bits) - 1 {
            return Err(serialization_error(format!(
                "Value {number} is out of range for type '{name}'"
            )));
        }
        Ok(*number)
    }

    /// A signed integer of `bits` bits (`ReadSignedInt*`).
    pub fn read_sint(&mut self, bits: u32, name: &str) -> Result<i64> {
        if self.json.is_none() {
            return self.read_varint_signed(bits);
        }
        let (value, json) = self.json_value()?;
        let number = match value {
            Json::Int(number) => *number,
            Json::UInt(number) => *number as i64,
            _ => return Err(json.type_error(value, name)),
        };
        if bits < 64 && !(-(1i64 << (bits - 1))..(1i64 << (bits - 1))).contains(&number) {
            return Err(serialization_error(format!(
                "Value {number} is out of range for type '{name}'"
            )));
        }
        Ok(number)
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        if self.json.is_none() {
            return Ok(self.read_raw(1)?[0] != 0);
        }
        let (value, json) = self.json_value()?;
        match value {
            Json::Bool(flag) => Ok(*flag),
            _ => Err(json.type_error(value, "bool")),
        }
    }

    /// `ReadDouble`: a real number, or the text of one.
    pub fn read_f64(&mut self) -> Result<f64> {
        if self.json.is_none() {
            let bytes = self.read_raw(8)?;
            return Ok(f64::from_le_bytes(bytes.try_into().expect("exact length")));
        }
        let (value, json) = self.json_value()?;
        match value {
            Json::Real(number) => Ok(*number),
            Json::Raw(text) => text.trim().parse::<f64>().ok().ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Expected a floating point number, but got the string {text}"),
                )
            }),
            _ => Err(json.type_error(value, "double")),
        }
    }

    pub fn read_f32(&mut self) -> Result<f32> {
        if self.json.is_none() {
            let bytes = self.read_raw(4)?;
            return Ok(f32::from_le_bytes(bytes.try_into().expect("exact length")));
        }
        Ok(self.read_f64()? as f32)
    }

    pub fn read_string(&mut self) -> Result<String> {
        if self.json.is_none() {
            let len = self.read_varint_unsigned(32)? as usize;
            let bytes = self.read_raw(len)?;
            return String::from_utf8(bytes.to_vec())
                .map_err(|_| serialization_error("Failed to deserialize: invalid UTF-8 string"));
        }
        let (value, json) = self.json_value()?;
        match value {
            Json::Str(text) => Ok(text.clone()),
            _ => Err(json.type_error(value, "string")),
        }
    }

    /// `ReadDataPtr` of a string: its bytes.
    pub fn read_string_bytes(&mut self) -> Result<Vec<u8>> {
        if self.json.is_none() {
            let len = self.read_varint_unsigned(32)? as usize;
            return Ok(self.read_raw(len)?.to_vec());
        }
        self.read_string().map(String::into_bytes)
    }

    /// `OnNullableBegin`: whether a pointer has a value; a null is read with it.
    pub fn begin_nullable(&mut self) -> Result<bool> {
        if self.json.is_none() {
            return self.read_bool();
        }
        let json = self.json.as_mut().expect("JSON");
        let value = json.next_value(false)?;
        if matches!(value, Json::Null) {
            json.next_value(true)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// `OnListBegin`: how many elements follow.
    pub fn begin_list(&mut self) -> Result<u64> {
        if self.json.is_none() {
            return self.read_varint_unsigned(64);
        }
        let (value, json) = self.json_value()?;
        let Json::Array(items) = value else {
            return Err(json.type_error(value, "array"));
        };
        self.push_json(JsonFrame::Array(items, 0))?;
        Ok(items.len() as u64)
    }

    /// `OnListEnd`.
    pub fn end_list(&mut self) {
        if let Some(json) = &mut self.json {
            json.stack.pop();
        }
    }

    fn begin_property(&mut self, field_id: u16) -> Result<()> {
        if self.json.is_some() {
            return Ok(());
        }
        let field = self.next_field()?;
        if field != field_id {
            return Err(serialization_error(format!(
                "Failed to deserialize: field id mismatch, expected: {field_id}, got: {field}"
            )));
        }
        Ok(())
    }

    /// `JsonDeserializer::Push`: no deeper than `MAX_NESTING_DEPTH` (with the array the root is in).
    fn push_json(&mut self, frame: JsonFrame<'a>) -> Result<()> {
        const MAX_NESTING_DEPTH: usize = 256;
        let json = self.json.as_mut().expect("JSON");
        if json.stack.len() > MAX_NESTING_DEPTH {
            return Err(serialization_error(format!(
                "Serialized JSON exceeds the maximum nesting depth of {MAX_NESTING_DEPTH}"
            )));
        }
        json.stack.push(frame);
        Ok(())
    }

    /// `DeserializationPolicy::Disallow(POINTER)`: no pointers of this process in what is read.
    pub fn disallow_pointers(mut self) -> Self {
        self.disallow_pointers = true;
        self
    }

    /// `CheckAllowed` of a POINTER.
    pub fn check_pointer_allowed(&self) -> Result<()> {
        if self.disallow_pointers {
            return Err(serialization_error("Deserialization of POINTER is not allowed"));
        }
        Ok(())
    }

    pub fn set_tag(&mut self, tag: &str) {
        if let Some(json) = &mut self.json {
            json.tag = tag.to_string();
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

    /// `CanDeserializeProperty` by name, as JSON has it, or by id.
    pub fn has_named_property(&mut self, field_id: u16, tag: &str) -> Result<bool> {
        match &self.json {
            Some(json) => Ok(json.current_has(tag)),
            None => self.has_property(field_id),
        }
    }

    /// `ReadProperty`: the next field must be `field_id`.
    pub fn read_property<T: Deserialize>(&mut self, field_id: u16, tag: &str) -> Result<T> {
        self.set_tag(tag);
        self.begin_property(field_id)?;
        T::read_value(self)
    }

    /// Reads an optional property, returning `None` when the next field is a different one.
    pub fn read_optional_property<T: Deserialize>(&mut self, field_id: u16, tag: &str) -> Result<Option<T>> {
        if let Some(json) = &mut self.json {
            if !json.current_has(tag) {
                return Ok(None);
            }
            json.tag = tag.to_string();
            return T::read_value(self).map(Some);
        }
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
        if self.json.is_some() {
            let (value, json) = self.json_value()?;
            if !matches!(value, Json::Object(_)) {
                return Err(json.type_error(value, "object"));
            }
            self.push_json(JsonFrame::Object(value))?;
            let result = body(self);
            self.json.as_mut().expect("JSON").stack.pop();
            return result;
        }
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
        tag: &str,
        body: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.set_tag(tag);
        self.begin_property(field_id)?;
        body(self)
    }

    /// `ReadObject`.
    pub fn read_object<T>(&mut self, field_id: u16, tag: &str, body: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.set_tag(tag);
        self.begin_property(field_id)?;
        self.read_object_value(body)
    }

    /// `ReadList`.
    pub fn read_list<T>(
        &mut self,
        field_id: u16,
        tag: &str,
        mut element: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        self.set_tag(tag);
        self.begin_property(field_id)?;
        let count = self.begin_list()?;
        let items = (0..count).map(|_| element(self)).collect();
        self.end_list();
        items
    }
}

macro_rules! varint_unsigned {
    ($($t:ty: $name:literal),*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.write_u64(u64::from(*self));
            }
            fn is_default(&self) -> bool {
                *self == 0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                Ok(d.read_uint(<$t>::BITS, $name)? as $t)
            }
            fn default_value() -> Option<Self> {
                Some(0)
            }
        }
    )*};
}
varint_unsigned!(u8: "uint8_t", u16: "uint16_t", u32: "uint32_t", u64: "uint64_t");

macro_rules! varint_signed {
    ($($t:ty: $name:literal),*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.write_i64(i64::from(*self));
            }
            fn is_default(&self) -> bool {
                *self == 0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                Ok(d.read_sint(<$t>::BITS, $name)? as $t)
            }
            fn default_value() -> Option<Self> {
                Some(0)
            }
        }
    )*};
}
varint_signed!(i8: "int8_t", i16: "int16_t", i32: "int32_t", i64: "int64_t");

impl Serialize for bool {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_bool(*self);
    }
    fn is_default(&self) -> bool {
        !*self
    }
}

impl Deserialize for bool {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_bool()
    }
    fn default_value() -> Option<Self> {
        Some(false)
    }
}

/// HUGEINT: the signed upper 64 bits, then the unsigned lower 64 bits.
/// JSON has it as an object of `upper` and `lower`.
impl Serialize for i128 {
    fn write_value(&self, s: &mut BinarySerializer) {
        if s.is_json() {
            let (upper, lower) = ((*self >> 64) as i64, *self as u64);
            return s.write_object_value(|s| {
                s.write_property(100, "upper", &upper);
                s.write_property(101, "lower", &lower);
            });
        }
        s.write_varint_signed((*self >> 64) as i64);
        s.write_varint_unsigned(*self as u64);
    }
}

impl Deserialize for i128 {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let (upper, lower): (i64, u64) = if d.is_json() {
            d.read_object_value(|d| Ok((d.read_property(100, "upper")?, d.read_property(101, "lower")?)))?
        } else {
            (d.read_varint_signed(64)?, d.read_varint_unsigned(64)?)
        };
        Ok((i128::from(upper) << 64) | i128::from(lower))
    }
}

impl Serialize for u128 {
    fn write_value(&self, s: &mut BinarySerializer) {
        if s.is_json() {
            let (upper, lower) = ((*self >> 64) as u64, *self as u64);
            return s.write_object_value(|s| {
                s.write_property(100, "upper", &upper);
                s.write_property(101, "lower", &lower);
            });
        }
        s.write_varint_unsigned((*self >> 64) as u64);
        s.write_varint_unsigned(*self as u64);
    }
}

impl Deserialize for u128 {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let (upper, lower): (u64, u64) = if d.is_json() {
            d.read_object_value(|d| Ok((d.read_property(100, "upper")?, d.read_property(101, "lower")?)))?
        } else {
            (d.read_varint_unsigned(64)?, d.read_varint_unsigned(64)?)
        };
        Ok((u128::from(upper) << 64) | u128::from(lower))
    }
}

macro_rules! raw_float {
    ($($t:ty, $write:ident, $read:ident);*) => {$(
        impl Serialize for $t {
            fn write_value(&self, s: &mut BinarySerializer) {
                s.$write(*self);
            }
            fn is_default(&self) -> bool {
                *self == 0.0
            }
        }
        impl Deserialize for $t {
            fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
                d.$read()
            }
            fn default_value() -> Option<Self> {
                Some(0.0)
            }
        }
    )*};
}
raw_float!(f32, write_f32, read_f32; f64, write_f64, read_f64);

impl Serialize for str {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_str(self);
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl Serialize for [u8] {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_bytes(self);
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
        d.read_string()
    }
    fn default_value() -> Option<Self> {
        Some(String::new())
    }
}

impl<T: Serialize> Serialize for Vec<T> {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.begin_list(self.len());
        for item in self {
            item.write_value(s);
        }
        s.end_list();
    }
    fn is_default(&self) -> bool {
        self.is_empty()
    }
}

impl<T: Deserialize> Deserialize for Vec<T> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let count = d.begin_list()?;
        let items = (0..count).map(|_| T::read_value(d)).collect();
        d.end_list();
        items
    }
    fn default_value() -> Option<Self> {
        Some(Vec::new())
    }
}

/// A nullable value: a presence flag, then the value (`WriteValue(const T *)`).
impl<T: Serialize> Serialize for Option<T> {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.begin_nullable(self.is_some());
        if let Some(v) = self {
            v.write_value(s);
        }
    }
    fn is_default(&self) -> bool {
        self.is_none()
    }
}

impl<T: Deserialize> Deserialize for Option<T> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        if d.begin_nullable()? {
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

/// The entries of a map, to write: a list of objects with properties `0: key` and `1: value`.
pub struct MapRef<'a, K, V>(pub &'a [(K, V)]);

impl<K: Serialize, V: Serialize> Serialize for MapRef<'_, K, V> {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.begin_list(self.0.len());
        for (key, value) in self.0 {
            s.write_object_value(|s| {
                s.write_property(0, "key", key);
                s.write_property(1, "value", value);
            });
        }
        s.end_list();
    }
    fn is_default(&self) -> bool {
        self.0.is_empty()
    }
}

/// The entries of a map, read.
pub struct Map<K, V>(pub Vec<(K, V)>);

impl<K: Deserialize, V: Deserialize> Deserialize for Map<K, V> {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let count = d.begin_list()?;
        let entries: Result<Vec<(K, V)>> = (0..count)
            .map(|_| d.read_object_value(|d| Ok((d.read_property(0, "key")?, d.read_property(1, "value")?))))
            .collect();
        d.end_list();
        entries.map(Self)
    }
    fn default_value() -> Option<Self> {
        Some(Self(Vec::new()))
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
    pub const GEO: u8 = 13;
}

/// The ids in the order of `LOGICAL_TYPE_NAMES`.
const ALL_TYPE_IDS: [LogicalTypeId; 54] = [
    LogicalTypeId::Invalid,
    LogicalTypeId::SqlNull,
    LogicalTypeId::Unknown,
    LogicalTypeId::Any,
    LogicalTypeId::Unbound,
    LogicalTypeId::Template,
    LogicalTypeId::Type,
    LogicalTypeId::Boolean,
    LogicalTypeId::TinyInt,
    LogicalTypeId::SmallInt,
    LogicalTypeId::Integer,
    LogicalTypeId::BigInt,
    LogicalTypeId::Date,
    LogicalTypeId::Time,
    LogicalTypeId::TimestampSec,
    LogicalTypeId::TimestampMs,
    LogicalTypeId::Timestamp,
    LogicalTypeId::TimestampNs,
    LogicalTypeId::Decimal,
    LogicalTypeId::Float,
    LogicalTypeId::Double,
    LogicalTypeId::Char,
    LogicalTypeId::Varchar,
    LogicalTypeId::Blob,
    LogicalTypeId::Interval,
    LogicalTypeId::UTinyInt,
    LogicalTypeId::USmallInt,
    LogicalTypeId::UInteger,
    LogicalTypeId::UBigInt,
    LogicalTypeId::TimestampTz,
    LogicalTypeId::TimestampTzNs,
    LogicalTypeId::TimeTz,
    LogicalTypeId::TimeNs,
    LogicalTypeId::Bit,
    LogicalTypeId::StringLiteral,
    LogicalTypeId::IntegerLiteral,
    LogicalTypeId::Bignum,
    LogicalTypeId::UHugeInt,
    LogicalTypeId::HugeInt,
    LogicalTypeId::Pointer,
    LogicalTypeId::Validity,
    LogicalTypeId::Uuid,
    LogicalTypeId::Geometry,
    LogicalTypeId::Struct,
    LogicalTypeId::List,
    LogicalTypeId::Map,
    LogicalTypeId::Table,
    LogicalTypeId::Enum,
    LogicalTypeId::LegacyAggregateState,
    LogicalTypeId::Lambda,
    LogicalTypeId::Union,
    LogicalTypeId::Array,
    LogicalTypeId::Variant,
    LogicalTypeId::Tuple,
];

/// `EnumUtil::ToChars<LogicalTypeId>`.
const LOGICAL_TYPE_NAMES: [&str; 54] = [
    "INVALID",
    "NULL",
    "UNKNOWN",
    "ANY",
    "UNBOUND",
    "TEMPLATE",
    "TYPE",
    "BOOLEAN",
    "TINYINT",
    "SMALLINT",
    "INTEGER",
    "BIGINT",
    "DATE",
    "TIME",
    "TIMESTAMP_S",
    "TIMESTAMP_MS",
    "TIMESTAMP",
    "TIMESTAMP_NS",
    "DECIMAL",
    "FLOAT",
    "DOUBLE",
    "CHAR",
    "VARCHAR",
    "BLOB",
    "INTERVAL",
    "UTINYINT",
    "USMALLINT",
    "UINTEGER",
    "UBIGINT",
    "TIMESTAMP WITH TIME ZONE",
    "TIMESTAMPTZ_NS",
    "TIME WITH TIME ZONE",
    "TIME_NS",
    "BIT",
    "STRING_LITERAL",
    "INTEGER_LITERAL",
    "BIGNUM",
    "UHUGEINT",
    "HUGEINT",
    "POINTER",
    "VALIDITY",
    "UUID",
    "GEOMETRY",
    "STRUCT",
    "LIST",
    "MAP",
    "TABLE",
    "ENUM",
    "LEGACY_AGGREGATE_STATE",
    "LAMBDA",
    "UNION",
    "ARRAY",
    "VARIANT",
    "TUPLE",
];

/// `LogicalTypeInfoType` as `EnumUtil` names it.
const TYPE_INFO_NAMES: [&str; 14] = [
    "INVALID_TYPE_INFO",
    "GENERIC_TYPE_INFO",
    "DECIMAL_TYPE_INFO",
    "STRING_TYPE_INFO",
    "LIST_TYPE_INFO",
    "STRUCT_TYPE_INFO",
    "ENUM_TYPE_INFO",
    "UNBOUND_TYPE_INFO",
    "LEGACY_AGGREGATE_STATE_TYPE_INFO",
    "ARRAY_TYPE_INFO",
    "ANY_TYPE_INFO",
    "INTEGER_LITERAL_TYPE_INFO",
    "TEMPLATE_TYPE_INFO",
    "GEO_TYPE_INFO",
];

fn logical_type_id_from(raw: u8) -> Result<LogicalTypeId> {
    ALL_TYPE_IDS
        .iter()
        .copied()
        .find(|id| *id as u8 == raw)
        .ok_or_else(|| serialization_error(format!("Failed to deserialize: unknown LogicalTypeId {raw}")))
}

/// A name, as JSON has it, from `names`; an error names the enum `enum_name`.
fn read_enum_name(d: &mut BinaryDeserializer<'_>, names: &[&str], enum_name: &str) -> Result<u8> {
    let name = d.read_string()?;
    names
        .iter()
        .position(|member| member.eq_ignore_ascii_case(&name))
        .map(|at| at as u8)
        .ok_or_else(|| {
            Error::not_implemented(format!(
                "Enum value: '{name}' not implemented in FromChars<{enum_name}>"
            ))
        })
}

impl Serialize for LogicalTypeId {
    fn write_value(&self, s: &mut BinarySerializer) {
        if s.is_json() {
            let at = ALL_TYPE_IDS.iter().position(|id| id == self).expect("every id");
            return s.write_str(LOGICAL_TYPE_NAMES[at]);
        }
        (*self as u8).write_value(s);
    }
}

impl Deserialize for LogicalTypeId {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        if d.is_json() {
            let at = read_enum_name(d, &LOGICAL_TYPE_NAMES, "LogicalTypeId")?;
            return Ok(ALL_TYPE_IDS[at as usize]);
        }
        logical_type_id_from(u8::read_value(d)?)
    }
}

/// `LogicalTypeInfoType`: a number, or a name in JSON.
struct TypeInfoType(u8);

impl Serialize for TypeInfoType {
    fn write_value(&self, s: &mut BinarySerializer) {
        if s.is_json() {
            return s.write_str(TYPE_INFO_NAMES[self.0 as usize]);
        }
        self.0.write_value(s);
    }
}

impl Deserialize for TypeInfoType {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        if d.is_json() {
            return read_enum_name(d, &TYPE_INFO_NAMES, "LogicalTypeInfoType").map(Self);
        }
        u8::read_value(d).map(Self)
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
                // `CreateVariantTypeInfo`: the STRUCT it is stored as
                LogicalTypeId::Variant => Some(type_info::STRUCT),
                _ if !self.has_params() && self.alias().is_none() => None,
                LogicalTypeId::Decimal => Some(type_info::DECIMAL),
                LogicalTypeId::List | LogicalTypeId::Map => Some(type_info::LIST),
                LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union => Some(type_info::STRUCT),
                LogicalTypeId::Array => Some(type_info::ARRAY),
                LogicalTypeId::Enum => Some(type_info::ENUM),
                LogicalTypeId::Geometry if self.crs().is_some() => Some(type_info::GEO),
                _ if self.has_params() => Some(type_info::STRING),
                _ => Some(type_info::GENERIC),
            };
            let Some(info_type) = info_type else {
                if s.options.serialize_default_values {
                    s.begin_property(101, "type_info", true);
                    s.begin_nullable(false);
                    s.end_property();
                }
                return;
            };
            // type_info is a nullable pointer: presence flag, then the info object.
            s.begin_property(101, "type_info", true);
            s.begin_nullable(true);
            s.write_object_value(|s| {
                s.write_property(100, "type", &TypeInfoType(info_type));
                s.write_property_with_default(101, "alias", self.alias().unwrap_or(""));
                s.write_property_with_default(103, "extension_info", &None::<u8>);
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
                    type_info::STRUCT if self.id() == LogicalTypeId::Variant => {
                        let children = crate::variant::unshredded_type().struct_children().to_vec();
                        s.write_property_with_default(200, "child_types", &children);
                    }
                    type_info::STRUCT => {
                        let children: Vec<(String, LogicalType)> = self.struct_children().to_vec();
                        s.write_property_with_default(200, "child_types", &children);
                    }
                    type_info::STRING if self.collation().is_some() => {
                        s.write_property_with_default(200, "collation", self.collation().unwrap_or(""));
                    }
                    // `GeoTypeInfo::Serialize`: the definition of the CRS
                    type_info::GEO => {
                        let definition = self.crs().map_or("", |crs| crs.definition.as_str());
                        s.write_object(200, "crs", |s| {
                            s.write_property_with_default(100, "definition", definition)
                        });
                    }
                    type_info::ENUM => {
                        let values = self.enum_values();
                        s.write_property(200, "values_count", &(values.len() as u64));
                        s.write_list(201, "values", values.len(), |s, i| values[i].write_value(s));
                    }
                    _ => {}
                }
            });
            s.end_property();
        });
    }
}

impl Deserialize for LogicalType {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let mut id: LogicalTypeId = d.read_property(100, "id")?;
            if id == LogicalTypeId::Pointer {
                d.check_pointer_allowed()?;
            }
            if !d.has_named_property(101, "type_info")? {
                return Ok(LogicalType::new(id));
            }
            d.set_tag("type_info");
            if !d.is_json() {
                d.next_field()?;
            }
            if !d.begin_nullable()? {
                return Ok(LogicalType::new(id));
            }
            d.read_object_value(|d| {
                let TypeInfoType(info_type) = d.read_property(100, "type")?;
                let alias: String = d.read_property_with_default(101, "alias")?;
                d.read_deleted_property::<Vec<Value>>(102, "modifiers")?;
                if d.is_json() && d.has_named_property(103, "extension_info")? {
                    d.set_tag("extension_info");
                    if d.begin_nullable()? {
                        return Err(Error::not_implemented(
                            "deserializing extension type info is not supported yet",
                        ));
                    }
                }
                // `LogicalType::Deserialize`: info of parameters the type doesn't have
                let matches = match info_type {
                    type_info::GENERIC | type_info::STRING | type_info::UNBOUND => true,
                    type_info::DECIMAL => id == LogicalTypeId::Decimal,
                    type_info::LIST => matches!(id, LogicalTypeId::List | LogicalTypeId::Map),
                    type_info::STRUCT => matches!(
                        id,
                        LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union | LogicalTypeId::Variant
                    ),
                    type_info::ENUM => id == LogicalTypeId::Enum,
                    type_info::ARRAY => id == LogicalTypeId::Array,
                    type_info::GEO => id == LogicalTypeId::Geometry,
                    _ => true,
                };
                if !matches {
                    // the info is read first, as C++ reads it before it checks it
                    let name = TYPE_INFO_NAMES
                        .get(info_type as usize)
                        .copied()
                        .unwrap_or("INVALID_TYPE_INFO");
                    let id_name = LOGICAL_TYPE_NAMES[ALL_TYPE_IDS.iter().position(|i| *i == id).expect("every id")];
                    if info_type == type_info::DECIMAL {
                        let w: u8 = d.read_property_with_default(200, "width")?;
                        let sc: u8 = d.read_property_with_default(201, "scale")?;
                        if !(1..=38).contains(&w) || sc > w {
                            return Err(serialization_error(format!(
                                "Failed to deserialize type: invalid DECIMAL({w}, {sc})"
                            )));
                        }
                    }
                    return Err(serialization_error(format!(
                        "Failed to deserialize type {id_name}: type info {name} does not match the type"
                    )));
                }
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
                        // `Decimal::IsValidWidthScale`
                        if !(1..=38).contains(&w) || sc > w {
                            return Err(serialization_error(format!(
                                "Failed to deserialize type: invalid DECIMAL({w}, {sc})"
                            )));
                        }
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
                            LogicalTypeId::Variant => LogicalType::new(LogicalTypeId::Variant),
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
                    // `CoordinateReferenceSystem::Deserialize`: parsed again from its definition
                    type_info::GEO => {
                        let definition: String = d.read_object_field(200, "crs", |d| {
                            d.read_object_value(|d| d.read_property_with_default(100, "definition"))
                        })?;
                        LogicalType::geometry(Some(crate::geometry_crs::Crs::parse(&definition)))
                    }
                    type_info::ENUM => {
                        let count: u64 = d.read_property(200, "values_count")?;
                        if count > u64::from(u32::MAX) {
                            return Err(serialization_error(format!(
                                "Corrupted enum: enum size {count} exceeds maximum {}",
                                u32::MAX
                            )));
                        }
                        let values = d.read_list(201, "values", String::read_value)?;
                        if values.len() as u64 != count {
                            return Err(serialization_error(format!(
                                "Corrupted enum: expected {count} values, but found {}",
                                values.len()
                            )));
                        }
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
    if d.has_named_property(204, "expr")? {
        let Some(reader) = d.type_expression else {
            return Err(Error::not_implemented(
                "deserializing a type expression here is not supported yet",
            ));
        };
        return d
            .read_object_field(204, "expr", |d| {
                if !d.begin_nullable()? {
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
        ValueData::Bytes(b) if s.is_json() => s.write_property(102, "value", String::from_utf8_lossy(b).as_ref()),
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
    // `Value::Deserialize`: these types are nothing without their parameters
    if d.is_json()
        && !ty.has_params()
        && matches!(
            ty.id(),
            LogicalTypeId::Decimal
                | LogicalTypeId::List
                | LogicalTypeId::Map
                | LogicalTypeId::Struct
                | LogicalTypeId::Union
                | LogicalTypeId::Array
                | LogicalTypeId::Enum
        )
    {
        let name = LOGICAL_TYPE_NAMES[ALL_TYPE_IDS.iter().position(|i| *i == ty.id()).expect("every id")];
        return Err(serialization_error(format!(
            "Failed to deserialize value: type {name} is missing its type info"
        )));
    }
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
            d.set_tag("value");
            d.begin_property(102)?;
            ValueData::Bytes(d.read_string_bytes()?)
        }
        PhysicalType::List | PhysicalType::Struct | PhysicalType::Array => {
            let children = d.read_object(102, "value", |d| {
                d.set_tag("children");
                d.begin_property(100)?;
                let count = d.begin_list()? as usize;
                let types = child_types(&ty, count);
                if count > types.len() {
                    return Err(serialization_error(format!(
                        "Failed to deserialize value: too many children for type {ty}"
                    )));
                }
                // `ReadPropertyWithExplicitDefault(100, "type", ..)`: children written before v1.2
                // have their own types
                let children = types
                    .into_iter()
                    .map(|t| {
                        d.read_object_value(|d| {
                            let t = if d.is_json() && d.has_named_property(100, "type")? {
                                let own: LogicalType = d.read_property(100, "type")?;
                                if own.id() == LogicalTypeId::Invalid { t } else { own }
                            } else {
                                t
                            };
                            read_value_body(d, t)
                        })
                    })
                    .collect::<Result<Vec<_>>>();
                d.end_list();
                children
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
    if !value_is_valid_shallow(&ty, &data) {
        return Err(serialization_error(format!(
            "Failed to deserialize value: value is not valid for type {ty}"
        )));
    }
    Value::new(ty, data)
}

/// `ValueIsValidShallow`: whether a value read is one of its type; its children were checked when
/// they were read.
fn value_is_valid_shallow(ty: &LogicalType, data: &ValueData) -> bool {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    const MAX_OFFSET: i64 = 16 * 60 * 60 - 1;
    let integer = || -> Option<i128> {
        Some(match data {
            ValueData::Int8(x) => i128::from(*x),
            ValueData::Int16(x) => i128::from(*x),
            ValueData::Int32(x) => i128::from(*x),
            ValueData::Int64(x) => i128::from(*x),
            ValueData::Int128(x) => *x,
            ValueData::UInt8(x) => i128::from(*x),
            ValueData::UInt16(x) => i128::from(*x),
            ValueData::UInt32(x) => i128::from(*x),
            ValueData::UInt64(x) => i128::from(*x),
            _ => return None,
        })
    };
    match ty.id() {
        LogicalTypeId::Varchar => match data {
            ValueData::Bytes(bytes) => std::str::from_utf8(bytes).is_ok(),
            _ => true,
        },
        LogicalTypeId::Enum => integer().is_some_and(|index| index >= 0 && (index as usize) < ty.enum_values().len()),
        LogicalTypeId::Time => integer().is_some_and(|micros| (0..=i128::from(MICROS_PER_DAY)).contains(&micros)),
        LogicalTypeId::TimeNs => {
            integer().is_some_and(|nanos| (0..=i128::from(MICROS_PER_DAY) * 1000).contains(&nanos))
        }
        // `dtime_tz_t`: the micros in the upper 40 bits, the encoded offset in the lower 24
        LogicalTypeId::TimeTz => integer().is_some_and(|bits| {
            let bits = bits as u64;
            let micros = (bits >> 24) as i64;
            let offset = MAX_OFFSET - (bits & 0xFF_FFFF) as i64;
            (0..=MICROS_PER_DAY).contains(&micros) && (-MAX_OFFSET..=MAX_OFFSET).contains(&offset)
        }),
        LogicalTypeId::Decimal => {
            let width = ty.decimal_properties().map_or(18, |(width, _)| width);
            let limit = 10i128.pow(u32::from(width));
            integer().is_some_and(|value| value > -limit && value < limit)
        }
        LogicalTypeId::Bit => match data {
            ValueData::Bytes(bits) => {
                if bits.len() < 2 || bits[0] >= 8 {
                    return false;
                }
                let padding = bits[0];
                let mask = (0xFFu16 << (8 - padding)) as u8;
                padding == 0 || bits[1] & mask == mask
            }
            _ => true,
        },
        LogicalTypeId::Union => match data {
            ValueData::Struct(children) => {
                let members = ty.struct_children().len().saturating_sub(1);
                children.len() == members + 1
                    && matches!(children[0].data(), ValueData::UInt8(tag) if (*tag as usize) < members)
            }
            _ => true,
        },
        LogicalTypeId::Struct | LogicalTypeId::Tuple => match data {
            ValueData::Struct(children) => children.len() == ty.struct_children().len(),
            _ => true,
        },
        LogicalTypeId::Array => match data {
            ValueData::List(children) => ty.array_size().is_none_or(|size| children.len() == size as usize),
            _ => true,
        },
        _ => true,
    }
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
