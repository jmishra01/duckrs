// Ported from duckdb/extension/json @ rust-port-baseline
//
//! JSON: the `JSON` type, an alias of VARCHAR, and its functions. A value is parsed into [`Json`] per
//! row, as yyjson does in C++, with its flags: NaN, Infinity and trailing commas are read.

use super::cast::{CastMode, cast_vector};
use super::nested::{bound_function, row_wise};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::json_scan::{Formats, formatted_value};
use crate::planner::expression::BoundExpr;
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

/// `LogicalType::JSON()`
pub fn json_type() -> LogicalType {
    LogicalType::VARCHAR.with_alias("JSON")
}

pub fn is_json_type(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Varchar && ty.alias() == Some("JSON")
}

pub use pintail_common::json::*;

fn parsed(text: &str) -> Result<Json> {
    parse(text).map_err(|e| invalid(parse_error_text(text, &e)))
}

/// `JSONPathElement`
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
    ReverseIndex(usize),
    Append,
    /// `$.*`
    AllKeys,
    /// `$[*]`
    AllItems,
    RecursiveWildcard,
    /// `$..key`
    RecursiveKey(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    steps: Vec<Step>,
}

impl Path {
    fn is_wildcard(&self) -> bool {
        self.steps.iter().any(|s| {
            matches!(
                s,
                Step::AllKeys | Step::AllItems | Step::RecursiveWildcard | Step::RecursiveKey(_)
            )
        })
    }
}

fn path_error(path: &str, at: usize) -> Error {
    invalid(format!(
        "JSON path error near '{}'",
        path.get(at.saturating_sub(1)..).unwrap_or_default()
    ))
}

/// `JSONCommon::ValidatePath`, of a path that starts with `$`.
fn dollar_path(text: &str) -> Result<Path> {
    let bytes = text.as_bytes();
    let mut at = 1;
    let mut steps = Vec::new();
    while at < bytes.len() {
        let c = bytes[at];
        at += 1;
        if at == bytes.len() {
            return Err(path_error(text, at));
        }
        match c {
            b'.' => {
                let mut recursive = false;
                match bytes[at] {
                    b'*' => {
                        at += 1;
                        if bytes.get(at) == Some(&b'*') {
                            at += 1;
                            steps.push(Step::RecursiveWildcard);
                        } else {
                            steps.push(Step::AllKeys);
                        }
                        continue;
                    }
                    b'.' => {
                        match bytes.get(at + 1) {
                            Some(b'*') => {
                                at += 2;
                                steps.push(Step::RecursiveWildcard);
                                continue;
                            }
                            Some(b'[') => {
                                at += 1;
                                steps.push(Step::RecursiveWildcard);
                                continue;
                            }
                            None => return Err(path_error(text, at)),
                            _ => {}
                        }
                        at += 1;
                        recursive = true;
                    }
                    _ => {}
                }
                let key = if bytes[at] == b'"' {
                    at += 1;
                    let start = at;
                    let mut unescaped = String::new();
                    loop {
                        match bytes.get(at) {
                            None => return Err(path_error(text, at)),
                            Some(b'"') => break,
                            // `ReadString`: a backslash escapes a quote or a backslash, and is kept before
                            // anything else
                            Some(b'\\') => match bytes.get(at + 1) {
                                None => return Err(path_error(text, at)),
                                Some(&escaped @ (b'"' | b'\\')) => {
                                    unescaped.push(char::from(escaped));
                                    at += 2;
                                }
                                Some(_) => {
                                    unescaped.push('\\');
                                    at += 1;
                                }
                            },
                            Some(_) => {
                                let ch = text[at..].chars().next().unwrap_or(' ');
                                unescaped.push(ch);
                                at += ch.len_utf8();
                            }
                        }
                    }
                    if at == start {
                        return Err(path_error(text, at));
                    }
                    at += 1;
                    unescaped
                } else {
                    let start = at;
                    while at < bytes.len() && !matches!(bytes[at], b'.' | b'[') {
                        at += 1;
                    }
                    if at == start {
                        return Err(path_error(text, at));
                    }
                    text[start..at].to_string()
                };
                steps.push(if recursive {
                    Step::RecursiveKey(key)
                } else {
                    Step::Key(key)
                });
            }
            b'[' => {
                if bytes[at] == b'*' {
                    at += 1;
                    if bytes.get(at) != Some(&b']') {
                        return Err(path_error(text, at));
                    }
                    at += 1;
                    steps.push(Step::AllItems);
                    continue;
                }
                let mut reverse = false;
                if bytes[at] == b'#' {
                    at += 1;
                    match bytes.get(at) {
                        Some(b']') => {
                            at += 1;
                            steps.push(Step::Append);
                            continue;
                        }
                        Some(b'-') => {}
                        _ => return Err(path_error(text, at)),
                    }
                }
                if bytes.get(at) == Some(&b'-') {
                    at += 1;
                    reverse = true;
                }
                let start = at;
                while matches!(bytes.get(at), Some(b'0'..=b'9')) {
                    at += 1;
                }
                // `ReadInteger`: at most 19 digits, then the closing bracket
                let index = (at - start <= 19)
                    .then(|| text[start..at].parse::<usize>().ok())
                    .flatten();
                let (Some(index), Some(b']')) = (index, bytes.get(at)) else {
                    return Err(path_error(text, start));
                };
                at += 1;
                steps.push(if reverse {
                    Step::ReverseIndex(index)
                } else {
                    Step::Index(index)
                });
            }
            _ => return Err(path_error(text, at)),
        }
    }
    Ok(Path { steps })
}

/// `JSONCommon::ValidatePathDollar`, a JSON pointer, or a key.
pub fn parse_path(text: &str) -> Result<Path> {
    if text.is_empty() {
        return Ok(Path { steps: Vec::new() });
    }
    if text.starts_with('$') {
        return dollar_path(text);
    }
    if let Some(pointer) = text.strip_prefix('/') {
        let steps = pointer
            .split('/')
            .map(|part| {
                let key = part.replace("~1", "/").replace("~0", "~");
                // `/-`: after the last element of an array, where json_set and json_insert append
                if key == "-" {
                    return Step::Append;
                }
                match key.parse::<usize>() {
                    Ok(index) if key == index.to_string() => Step::Index(index),
                    _ => Step::Key(key),
                }
            })
            .collect();
        return Ok(Path { steps });
    }
    let key = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(text);
    Ok(Path {
        steps: vec![Step::Key(key.to_string())],
    })
}

/// The values in `value` and below it, breadth first, as `GetWildcardPathInternal` visits them.
fn breadth_first(value: &Json) -> Vec<&Json> {
    let mut nodes = vec![value];
    let mut at = 0;
    while at < nodes.len() {
        match nodes[at] {
            Json::Array(items) => nodes.extend(items.iter()),
            Json::Object(items) => nodes.extend(items.iter().map(|(_, item)| item)),
            _ => {}
        }
        at += 1;
    }
    nodes
}

/// `..`: the rest of the path from each value at or below this one; the value itself only when a path follows.
fn collect_recursive<'a>(value: &'a Json, rest: &[Step], out: &mut Vec<&'a Json>) {
    for (index, node) in breadth_first(value).into_iter().enumerate() {
        if index > 0 || !rest.is_empty() {
            walk(node, rest, out);
        }
    }
}

/// `..key`: the field `key` of each object at or below this one.
fn collect_key<'a>(value: &'a Json, key: &str, rest: &[Step], out: &mut Vec<&'a Json>) {
    for node in breadth_first(value) {
        if let Json::Object(items) = node
            && let Some((_, found)) = items.iter().find(|(k, _)| k == key)
        {
            walk(found, rest, out);
        }
    }
}

fn walk<'a>(value: &'a Json, steps: &[Step], out: &mut Vec<&'a Json>) {
    let Some((step, rest)) = steps.split_first() else {
        out.push(value);
        return;
    };
    match step {
        Step::Key(key) => {
            if let Json::Object(items) = value
                && let Some((_, found)) = items.iter().find(|(k, _)| k == key)
            {
                walk(found, rest, out);
            }
        }
        Step::Index(index) => match value {
            Json::Array(items) => {
                if let Some(item) = items.get(*index) {
                    walk(item, rest, out);
                }
            }
            Json::Object(items) => {
                let key = index.to_string();
                if let Some((_, found)) = items.iter().find(|(k, _)| *k == key) {
                    walk(found, rest, out);
                }
            }
            _ => {}
        },
        Step::ReverseIndex(index) => {
            if let Json::Array(items) = value {
                let position = if *index == 0 {
                    Some(0)
                } else {
                    items.len().checked_sub(*index)
                };
                if let Some(item) = position.and_then(|p| items.get(p)) {
                    walk(item, rest, out);
                }
            }
        }
        Step::Append => {}
        Step::AllKeys => {
            if let Json::Object(items) = value {
                for (_, item) in items {
                    walk(item, rest, out);
                }
            }
        }
        Step::AllItems => {
            if let Json::Array(items) = value {
                for item in items {
                    walk(item, rest, out);
                }
            }
        }
        Step::RecursiveWildcard => collect_recursive(value, rest, out),
        Step::RecursiveKey(key) => collect_key(value, key, rest, out),
    }
}

fn single<'a>(value: &'a Json, path: &Path) -> Option<&'a Json> {
    let mut found = Vec::new();
    walk(value, &path.steps, &mut found);
    found.first().copied()
}

/// The JSON of a `Value`, for `to_json` and casts to JSON (`JSONCreateFunctionData`).
/// `VariantCasts::CastVariantToJSON`: a VARIANT NULL is a JSON null.
fn variant_node_to_json(node: &pintail_common::variant::VariantNode) -> Result<Json> {
    variant_node_to_json_at(node, 0)
}

/// `VariantVisitor::CheckDepth`: a value nested this deep is refused.
fn variant_node_to_json_at(node: &pintail_common::variant::VariantNode, depth: usize) -> Result<Json> {
    use pintail_common::variant::VariantNode;
    const MAX_RECURSION_DEPTH: usize = 128;
    if depth >= MAX_RECURSION_DEPTH {
        return Err(invalid(format!(
            "VARIANT exceeds maximum recursion depth of {MAX_RECURSION_DEPTH}"
        )));
    }
    Ok(match node {
        VariantNode::Null => Json::Null,
        // `VariantToJSON::VisitFloat` writes a FLOAT as a double, `VisitDecimal` writes
        // `Decimal::ToString` raw
        VariantNode::Scalar(value) => match (value.logical_type().id(), value.data()) {
            (LogicalTypeId::Float, ValueData::Float(x)) => Json::Real(f64::from(*x)),
            (LogicalTypeId::Decimal, _) if !value.is_null() => {
                // JSON numbers need an integer part, which `Decimal::ToString` leaves out when all
                // digits are fractional
                let mut text = value.try_to_string()?;
                if let Some(at) = text.find('.').filter(|&at| at == usize::from(text.starts_with('-'))) {
                    text.insert(at, '0');
                }
                Json::Raw(text)
            }
            _ => value_to_json(value)?,
        },
        VariantNode::Array(items) => Json::Array(
            items
                .iter()
                .map(|item| variant_node_to_json_at(item, depth + 1))
                .collect::<Result<_>>()?,
        ),
        VariantNode::Object(children) => Json::Object(
            children
                .iter()
                .map(|(key, child)| Ok((key.clone(), variant_node_to_json_at(child, depth + 1)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

pub fn value_to_json(value: &Value) -> Result<Json> {
    if value.is_null() {
        return Ok(Json::Null);
    }
    let ty = value.logical_type();
    if is_json_type(ty) {
        return Ok(
            parse(&value.try_to_string()?).unwrap_or_else(|_| Json::Raw(value.try_to_string().unwrap_or_default()))
        );
    }
    if let (LogicalTypeId::Variant, ValueData::Bytes(bytes)) = (ty.id(), value.data()) {
        return variant_node_to_json(&pintail_common::variant::VariantNode::decode(bytes)?);
    }
    Ok(match (ty.id(), value.data()) {
        (LogicalTypeId::Boolean, ValueData::Bool(flag)) => Json::Bool(*flag),
        (LogicalTypeId::TinyInt | LogicalTypeId::SmallInt | LogicalTypeId::Integer | LogicalTypeId::BigInt, _) => {
            Json::Int(value.as_i128().unwrap_or(0) as i64)
        }
        (LogicalTypeId::UTinyInt | LogicalTypeId::USmallInt | LogicalTypeId::UInteger | LogicalTypeId::UBigInt, _) => {
            Json::UInt(value.as_i128().unwrap_or(0) as u64)
        }
        (LogicalTypeId::HugeInt | LogicalTypeId::UHugeInt | LogicalTypeId::Bignum, _) => {
            Json::Raw(value.try_to_string()?)
        }
        (LogicalTypeId::Float, ValueData::Float(x)) => Json::Raw({
            let mut out = String::new();
            if x.is_finite() {
                out = format!("{x:?}");
            } else {
                write_real(&mut out, f64::from(*x));
            }
            out
        }),
        (LogicalTypeId::Double, ValueData::Double(x)) => Json::Real(*x),
        (LogicalTypeId::Decimal, _) if ty.decimal_properties().is_some_and(|(width, _)| width > 15) => {
            Json::Raw(value.try_to_string()?)
        }
        (LogicalTypeId::Decimal, _) => {
            let (_, scale) = ty.decimal_properties().unwrap_or((18, 0));
            Json::Real(value.as_i128().unwrap_or(0) as f64 / 10f64.powi(i32::from(scale)))
        }
        (LogicalTypeId::List | LogicalTypeId::Array, ValueData::List(items)) => {
            Json::Array(items.iter().map(value_to_json).collect::<Result<_>>()?)
        }
        (LogicalTypeId::Struct, ValueData::Struct(fields)) => Json::Object(
            ty.struct_children()
                .iter()
                .zip(fields)
                .map(|((name, _), field)| Ok((name.clone(), value_to_json(field)?)))
                .collect::<Result<_>>()?,
        ),
        (LogicalTypeId::Map, ValueData::List(entries)) => {
            let mut items = Vec::with_capacity(entries.len());
            for entry in entries {
                let ValueData::Struct(parts) = entry.data() else {
                    return Err(invalid("A MAP entry is not a key and a value"));
                };
                items.push((parts[0].try_to_string()?, value_to_json(&parts[1])?));
            }
            Json::Object(items)
        }
        // `CreateValuesTuple`: an array of the fields
        (LogicalTypeId::Tuple, ValueData::Struct(fields)) => {
            Json::Array(fields.iter().map(value_to_json).collect::<Result<_>>()?)
        }
        (LogicalTypeId::Union, ValueData::Struct(fields)) => {
            // `CreateValuesUnion`: an object of the member's name and value
            let tag = fields.first().and_then(Value::as_i128).unwrap_or(0) as usize;
            match (fields.get(tag + 1), ty.struct_children().get(tag + 1)) {
                (Some(member), Some((name, _))) => Json::Object(vec![(name.clone(), value_to_json(member)?)]),
                _ => Json::Null,
            }
        }
        _ => Json::Str(value.try_to_string()?),
    })
}

/// The VARIANT of the text of a JSON value.
pub(super) fn json_text_to_variant(text: &str) -> Result<pintail_common::variant::VariantNode> {
    json_to_variant(&parsed(text)?)
}

/// `ConvertJSON` of `json_to_variant.hpp`: of a repeated key, the last value counts.
fn json_to_variant(json: &Json) -> Result<pintail_common::variant::VariantNode> {
    use pintail_common::variant::VariantNode;
    Ok(match json {
        Json::Null => VariantNode::Null,
        Json::Bool(flag) => VariantNode::Scalar(Value::boolean(*flag)),
        Json::UInt(x) => VariantNode::Scalar(Value::ubigint(*x)),
        Json::Int(x) => VariantNode::Scalar(Value::bigint(*x)),
        Json::Real(x) => VariantNode::Scalar(Value::double(*x)),
        Json::Raw(text) => match text.parse::<f64>() {
            Ok(x) => VariantNode::Scalar(Value::double(x)),
            Err(_) => return Err(invalid(format!("Unable to cast '{text}' to VARIANT"))),
        },
        Json::Str(text) => VariantNode::Scalar(Value::varchar(text.as_str())),
        Json::Array(items) => VariantNode::Array(items.iter().map(json_to_variant).collect::<Result<_>>()?),
        Json::Object(items) => {
            let mut children: Vec<(String, VariantNode)> = Vec::with_capacity(items.len());
            for (key, item) in items {
                let node = json_to_variant(item)?;
                match children.iter_mut().find(|(k, _)| k == key) {
                    Some(entry) => entry.1 = node,
                    None => children.push((key.clone(), node)),
                }
            }
            children.sort_by(|a, b| a.0.cmp(&b.0));
            VariantNode::Object(children)
        }
    })
}

/// The types `JSONTransform` reads with the cast from VARCHAR (`TransformFromString`).
fn string_cast_type(ty: &LogicalType) -> bool {
    use LogicalTypeId as T;
    matches!(
        ty.id(),
        T::Enum
            | T::Date
            | T::Interval
            | T::Time
            | T::TimeNs
            | T::TimeTz
            | T::Timestamp
            | T::TimestampTz
            | T::TimestampTzNs
            | T::TimestampNs
            | T::TimestampMs
            | T::TimestampSec
            | T::Uuid
    )
}

/// A value cast through the engine's casts, or `None` if it doesn't fit.
fn cast_value(value: &Value, ty: &LogicalType) -> Option<Value> {
    let mut source = Vector::new(value.logical_type().clone(), 1);
    source.set_value(0, value).ok()?;
    let mut target = Vector::new(ty.clone(), 1);
    cast_vector(&source, &mut target, 1, CastMode::Try).ok()?;
    let result = target.get_value(0);
    (!result.is_null()).then_some(result)
}

/// `JSONTransform`: `json` as a `ty`. A value that doesn't fit is NULL, or an error if `strict`.
pub fn transform(json: &Json, ty: &LogicalType, strict: bool) -> Result<Value> {
    transform_with(json, ty, strict, &Formats::default())
}

/// [`transform`], with the formats of dates and timestamps.
pub fn transform_with(json: &Json, ty: &LogicalType, strict: bool, formats: &Formats) -> Result<Value> {
    if let Json::Str(text) = json {
        if let Some(value) = formatted_value(text, ty, formats) {
            return Ok(value);
        }
        // `TransformFromStringWithFormat`: a string that the given format does not read is an error, not a cast
        let format = match ty.id() {
            LogicalTypeId::Date => formats.date.as_deref(),
            LogicalTypeId::Timestamp => formats.timestamp.as_deref(),
            _ => None,
        };
        // the specials (`infinity`, `epoch`) are read whatever the format
        let special = matches!(text.to_ascii_lowercase().as_str(), "infinity" | "-infinity" | "epoch");
        if let Some(format) = format.filter(|_| !special) {
            if !strict {
                return Ok(Value::null(ty.clone()));
            }
            return Err(invalid(format!(
                "Could not parse string \"{text}\" according to format specifier \"{format}\""
            )));
        }
    }
    let transform = |json: &Json, ty: &LogicalType, strict: bool| transform_with(json, ty, strict, formats);
    let fail = |json: &Json| -> Result<Value> {
        // `TransformDecimal`: of the value, what `ValToString` writes of it
        if strict && ty.id() == LogicalTypeId::Decimal {
            let mut text = json.text();
            if text.len() > 50 {
                let mut end = 50;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text = format!("{}...", &text[..end]);
            }
            return Err(invalid(format!("Failed to cast value to decimal: {text}")));
        }
        // `GetValueNumerical`
        if strict && (ty.is_numeric() || ty.id() == LogicalTypeId::Boolean) {
            let mut text = json.text();
            if text.len() > 50 {
                let mut end = 50;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text = format!("{}...", &text[..end]);
            }
            return Err(invalid(format!("Failed to cast value to numerical: {text}")));
        }
        if strict {
            Err(invalid(format!("Unable to cast '{}' to {ty}", json.text())))
        } else {
            Ok(Value::null(ty.clone()))
        }
    };
    // `JSONTransform::Transform`: the types it has a reader for
    {
        use LogicalTypeId as T;
        if !matches!(
            ty.id(),
            T::SqlNull
                | T::Boolean
                | T::TinyInt
                | T::SmallInt
                | T::Integer
                | T::BigInt
                | T::UTinyInt
                | T::USmallInt
                | T::UInteger
                | T::UBigInt
                | T::HugeInt
                | T::UHugeInt
                | T::Float
                | T::Double
                | T::Decimal
                | T::Varchar
                | T::Blob
                | T::Struct
                | T::Tuple
                | T::List
                | T::Map
                | T::Union
                | T::Array
                | T::Geometry
                | T::Variant
        ) && !string_cast_type(ty)
        {
            return Err(Error::new(
                ErrorKind::NotImplemented,
                format!("Cannot read a value of type {ty} from a json file"),
            ));
        }
    }
    if matches!(json, Json::Null) {
        return Ok(Value::null(ty.clone()));
    }
    // `ReadRawNumber`: numbers kept as text for a DECIMAL read as the default flags would read them
    // into anything that has no DECIMAL in it
    if !contains_decimal(ty) && has_raw_number(json) {
        return transform_with(&reread_raw_numbers(json.clone()), ty, strict, formats);
    }
    if is_json_type(ty) {
        return Value::varchar(json.text()).reinterpret(ty);
    }
    if ty.id() == LogicalTypeId::Variant {
        let node = json_to_variant(json)?;
        return Value::new(ty.clone(), ValueData::Bytes(node.encode()));
    }
    // `TransformToGeometry`: an object is GeoJSON (no WKT is one), a string WKT
    if ty.id() == LogicalTypeId::Geometry && matches!(json, Json::Object(_)) {
        let wkb = pintail_common::geojson::from_geojson(json).and_then(|wkb| pintail_common::geometry::from_wkb(&wkb));
        return match wkb {
            Ok(wkb) => Value::new(ty.clone(), ValueData::Bytes(wkb)),
            Err(error) if strict => Err(error),
            Err(_) => Ok(Value::null(ty.clone())),
        };
    }
    match (ty.id(), json) {
        (LogicalTypeId::SqlNull, _) => Ok(Value::null(ty.clone())),
        (LogicalTypeId::Varchar, Json::Str(text)) => Ok(Value::varchar(text.as_str())),
        // `TransformToBlob`: a string is cast, anything else is the bytes of its text
        (LogicalTypeId::Blob, Json::Str(_)) => fail_or_cast(json, ty, strict),
        (LogicalTypeId::Blob, other) => Value::new(ty.clone(), ValueData::Bytes(other.text().into_bytes())),
        (LogicalTypeId::Varchar, other) => Ok(Value::varchar(other.text())),
        (LogicalTypeId::Struct, Json::Object(items)) => {
            let mut fields = Vec::with_capacity(ty.struct_children().len());
            for (name, child) in ty.struct_children() {
                match object_member(json, items, name, formats)? {
                    Some(item) => fields.push(transform(item, child, strict)?),
                    None if strict && !formats.missing_keys_are_null => {
                        return Err(invalid(format!("Object {} does not have key \"{name}\"", json.text())));
                    }
                    None => fields.push(Value::null(child.clone())),
                }
            }
            Value::new(ty.clone(), ValueData::Struct(fields))
        }
        (LogicalTypeId::List, Json::Array(items)) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::VARCHAR);
            let values = items
                .iter()
                .map(|item| transform(item, &child, strict))
                .collect::<Result<_>>()?;
            Value::new(ty.clone(), ValueData::List(values))
        }
        (LogicalTypeId::Array, Json::Array(items)) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::VARCHAR);
            let size = ty.array_size().unwrap_or(0) as usize;
            if items.len() != size {
                // `TransformArrayToArray`
                if strict {
                    let mut text = json.text();
                    if text.len() > 50 {
                        let mut end = 50;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text = format!("{}...", &text[..end]);
                    }
                    return Err(invalid(format!(
                        "Expected array of size {size}, but got '{text}' with size {}",
                        items.len()
                    )));
                }
                return Ok(Value::null(ty.clone()));
            }
            let values = items
                .iter()
                .map(|item| transform(item, &child, strict))
                .collect::<Result<_>>()?;
            Value::new(ty.clone(), ValueData::List(values))
        }
        (LogicalTypeId::Map, Json::Object(items)) => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::VARCHAR);
            let children = entry.struct_children();
            let mut values = Vec::with_capacity(items.len());
            let mut keys: Vec<Value> = Vec::with_capacity(items.len());
            // `TransformObjectToMap`: a key that does not convert is NULL without strict casting,
            // and a map with a NULL key or the same key twice is NULL, or an error when strict
            let mut invalid_reason = None;
            for (key, item) in items {
                let key = match cast_value(&Value::varchar(key.as_str()), &children[0].1) {
                    Some(key) => key,
                    None if strict => {
                        return Err(invalid(format!("Failed to cast key {key} to {}", children[0].1)));
                    }
                    None => Value::null(children[0].1.clone()),
                };
                let item = transform(item, &children[1].1, strict)?;
                if invalid_reason.is_none() {
                    if key.is_null() {
                        invalid_reason = Some("Map keys can not be NULL");
                    } else if keys.contains(&key) {
                        invalid_reason = Some("Map keys must be unique");
                    }
                }
                keys.push(key.clone());
                values.push(Value::new(entry.clone(), ValueData::Struct(vec![key, item]))?);
            }
            if let Some(reason) = invalid_reason {
                return if strict {
                    Err(invalid(reason.to_string()))
                } else {
                    Ok(Value::null(ty.clone()))
                };
            }
            Value::new(ty.clone(), ValueData::List(values))
        }
        // `TransformValueIntoUnion`: an object of one key, the member it names
        (LogicalTypeId::Union, _) => {
            let error = |message: String| -> Result<Value> {
                if strict {
                    Err(invalid(message))
                } else {
                    Ok(Value::null(ty.clone()))
                }
            };
            let Json::Object(items) = json else {
                return error(format!(
                    "Expected an object representing a union, got {}",
                    yyjson_type_desc(json)
                ));
            };
            let (key, item) = match items.as_slice() {
                [] => return error("Found empty object, instead of union".to_string()),
                [entry] => (&entry.0, &entry.1),
                _ => return error("Found object containing more than one key, instead of union".to_string()),
            };
            // the first child of a UNION is its tag
            let members = &ty.struct_children()[1..];
            let Some(tag) = members.iter().position(|(name, _)| name == key) else {
                return error(format!("Found object containing unknown key, instead of union: {key}"));
            };
            let member = transform(item, &members[tag].1, strict)?;
            let mut fields = vec![Value::utinyint(tag as u8)];
            for (at, (_, member_type)) in members.iter().enumerate() {
                fields.push(if at == tag {
                    member.clone()
                } else {
                    Value::null(member_type.clone())
                });
            }
            Value::new(ty.clone(), ValueData::Struct(fields))
        }
        // `TransformObjectInternal`, `TransformArrayToList`, `TransformArrayToArray`, `TransformObjectToMap`
        (LogicalTypeId::Struct | LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map, _) if strict => {
            let expected = match ty.id() {
                LogicalTypeId::Struct | LogicalTypeId::Map => "OBJECT",
                _ => "ARRAY",
            };
            let mut text = json.text();
            if text.len() > 50 {
                let mut end = 50;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text = format!("{}...", &text[..end]);
            }
            Err(invalid(format!(
                "Expected {expected}, but got {}: {text}",
                json.type_name()
            )))
        }
        // `TransformArrayToTuple`: the items by position, missing ones NULL, extra ones ignored
        (LogicalTypeId::Tuple, Json::Array(items)) => {
            let mut fields = Vec::new();
            for (at, (_, child)) in ty.struct_children().iter().enumerate() {
                fields.push(match items.get(at) {
                    Some(item) => transform(item, child, strict)?,
                    None => Value::null(child.clone()),
                });
            }
            Value::new(ty.clone(), ValueData::Struct(fields))
        }
        (LogicalTypeId::Tuple, _) if strict => {
            let mut text = json.text();
            if text.len() > 50 {
                let mut end = 50;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text = format!("{}...", &text[..end]);
            }
            Err(invalid(format!("Expected ARRAY, but got {}: {text}", json.type_name())))
        }
        (LogicalTypeId::Tuple, _) => Ok(Value::null(ty.clone())),
        // `TransformObjectInternal`: what is no object is a struct of NULL fields
        (LogicalTypeId::Struct, _) => Value::new(
            ty.clone(),
            ValueData::Struct(
                ty.struct_children()
                    .iter()
                    .map(|(_, child)| Value::null(child.clone()))
                    .collect(),
            ),
        ),
        (LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map, _) => fail(json),
        (_, Json::Array(_) | Json::Object(_)) => fail(json),
        (_, Json::Bool(flag)) => match cast_value(&Value::boolean(*flag), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
        (_, Json::Str(text)) => match cast_value(&Value::varchar(text.as_str()), ty) {
            Some(value) => Ok(value),
            // `TransformFromString`: the error of the cast
            None if strict && string_cast_type(ty) => {
                let mut source = Vector::new(LogicalType::VARCHAR, 1);
                source.set_value(0, &Value::varchar(text.as_str()))?;
                let mut target = Vector::new(ty.clone(), 1);
                match cast_vector(&source, &mut target, 1, CastMode::Strict) {
                    Err(error) => Err(invalid(format!(
                        "{}\n If this error occurred during read_json, line/object number information is approximate",
                        error.message
                    ))),
                    Ok(()) => fail(json),
                }
            }
            None => fail(json),
        },
        (_, Json::UInt(x)) => match cast_value(&Value::ubigint(*x), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
        (_, Json::Int(x)) => match cast_value(&Value::bigint(*x), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
        (_, Json::Real(x)) => match cast_value(&Value::double(*x), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
        (_, other) => match cast_value(&Value::varchar(other.text()), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
    }
}

/// `TransformObjectInternal`'s key lookup: the value of the member `name` (by its spelling, or any
/// with `case_insensitive_keys`); the first of several, which is an error with `error_duplicate_key`.
pub fn object_member<'a>(
    object: &Json,
    items: &'a [(String, Json)],
    name: &str,
    formats: &Formats,
) -> Result<Option<&'a Json>> {
    let matches = |key: &str| {
        if formats.case_insensitive {
            key.eq_ignore_ascii_case(name)
        } else {
            key == name
        }
    };
    let mut found = items.iter().filter(|(key, _)| matches(key));
    let first = found.next();
    if formats.error_duplicate_key
        && let Some((duplicate, _)) = found.next()
    {
        let mut text = object.text();
        if text.len() > 50 {
            let mut end = 50;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text = format!("{}...", &text[..end]);
        }
        return Err(invalid(format!("Object {text} has duplicate key \"{duplicate}\"")));
    }
    Ok(first.map(|(_, value)| value))
}

/// A string cast to `ty`, or NULL or an error if it isn't one.
fn fail_or_cast(json: &Json, ty: &LogicalType, strict: bool) -> Result<Value> {
    let Json::Str(text) = json else {
        return Ok(Value::null(ty.clone()));
    };
    match cast_value(&Value::varchar(text.as_str()), ty) {
        Some(value) => Ok(value),
        None if strict => Err(invalid(format!("Unable to cast '{}' to {ty}", json.text()))),
        None => Ok(Value::null(ty.clone())),
    }
}

/// `JSONCommon::NumbersAsRaw`: whether a type has a DECIMAL in it.
pub fn contains_decimal(ty: &LogicalType) -> bool {
    match ty.id() {
        LogicalTypeId::Decimal => true,
        LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map => {
            ty.child_type().is_some_and(contains_decimal)
        }
        LogicalTypeId::Struct | LogicalTypeId::Union => {
            ty.struct_children().iter().any(|(_, child)| contains_decimal(child))
        }
        _ => false,
    }
}

/// A number kept as text that the default flags read as a number (not a big one, kept as text too).
fn rereads(json: &Json) -> Option<Json> {
    match json {
        Json::Raw(text) => match parse(text) {
            Ok(number @ (Json::UInt(_) | Json::Int(_) | Json::Real(_))) => Some(number),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a value has a number in it that `rereads`.
fn has_raw_number(json: &Json) -> bool {
    let mut stack = vec![json];
    while let Some(value) = stack.pop() {
        match value {
            Json::Array(items) => stack.extend(items),
            Json::Object(items) => stack.extend(items.iter().map(|(_, v)| v)),
            other if rereads(other).is_some() => return true,
            _ => {}
        }
    }
    false
}

/// The value with each number that `rereads` read again.
fn reread_raw_numbers(json: Json) -> Json {
    if let Some(number) = rereads(&json) {
        return number;
    }
    json.map_containers(|container| match container.into_array() {
        Ok(items) => Json::Array(items.into_iter().map(|item| rereads(&item).unwrap_or(item)).collect()),
        Err(container) => match container.into_object() {
            Ok(items) => Json::Object(
                items
                    .into_iter()
                    .map(|(key, item)| (key, rereads(&item).unwrap_or(item)))
                    .collect(),
            ),
            Err(other) => other,
        },
    })
}

/// `yyjson_get_type_desc`.
fn yyjson_type_desc(json: &Json) -> &'static str {
    match json {
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

/// The casts of the JSON type: to it, and from it to anything but VARCHAR.
pub fn json_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let from = source.logical_type().clone();
    let to = result.logical_type().clone();
    result.prepare_flat(count.max(1));
    for row in 0..count {
        let value = source.get_value(row);
        let converted = if value.is_null() {
            Value::null(to.clone())
        } else if is_json_type(&to) {
            if from.id() == LogicalTypeId::Varchar {
                let text = value.try_to_string()?;
                match parse(&text) {
                    Ok(_) => Value::varchar(text).reinterpret(&to)?,
                    Err(_) if matches!(mode, CastMode::Try) => Value::null(to.clone()),
                    Err(error) => return Err(Error::new(ErrorKind::Conversion, parse_error_text(&text, &error))),
                }
            } else {
                Value::varchar(value_to_json(&value)?.text()).reinterpret(&to)?
            }
        } else {
            let text = value.try_to_string()?;
            let strict = !matches!(mode, CastMode::Try);
            // `JSONCommon::NumbersAsRaw`: a DECIMAL reads numbers from their text
            let raw = contains_decimal(&to);
            with_stack_for(&text, || {
                let json = match parse_with(&text, raw) {
                    Ok(json) => json,
                    // TRY_CAST: what is no JSON is NULL
                    Err(_) if !strict => return Ok(Value::null(to.clone())),
                    Err(e) => return Err(invalid(parse_error_text(&text, &e))),
                };
                // `TransformToJSON`: an element of a JSON[] is the JSON it is, a `null` too
                if let (Json::Array(items), Some(child)) = (&json, to.child_type())
                    && to.id() == LogicalTypeId::List
                    && is_json_type(child)
                {
                    let items = items
                        .iter()
                        .map(|item| Value::varchar(item.text()).reinterpret(child))
                        .collect::<Result<Vec<_>>>()?;
                    return Value::list(child.clone(), items);
                }
                // `HandleCastError`: what the transform refuses is a conversion error of the cast
                transform(&json, &to, strict).map_err(|error| match error.kind {
                    ErrorKind::InvalidInput => Error::new(ErrorKind::Conversion, error.message),
                    _ => error,
                })
            })?
        };
        result.set_value(row, &converted)?;
    }
    Ok(())
}

/// Parsing, transforming and dropping a document recurse once per level it nests, so a deep one runs
/// on a thread whose stack is as deep as the document (yyjson and `ConvertJSON` take no stack per level).
pub(crate) fn with_stack_for<T: Send>(text: &str, work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    const SHALLOW: usize = 512;
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' | b'{' if !in_string => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b']' | b'}' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if deepest <= SHALLOW {
        return work();
    }
    // a few kilobytes per level for the parser, the transform and the drop
    let stack = 8 * 1024 * 1024 + deepest * 16 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn_scoped(scope, work)
            .map_err(|error| Error::new(ErrorKind::Internal, format!("could not start a thread: {error}")))?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// `json_structure`, and what `read_json` infers.
#[derive(Debug, Clone, PartialEq)]
pub enum Structure {
    Null,
    Scalar(&'static str),
    List(Box<Structure>),
    Object(Vec<(String, Structure)>),
    /// values that have nothing in common
    Mixed,
}

impl Structure {
    pub fn of(json: &Json) -> Structure {
        Self::of_with(json, false)
    }

    /// `JSONStructure::ExtractStructure`, with `case_insensitive` keys that differ in case one key,
    /// named by its first spelling (`case_insensitive_keys`).
    pub fn of_with(json: &Json, case_insensitive: bool) -> Structure {
        Self::of_options(json, case_insensitive, false)
    }

    /// [`Self::of_with`], with `geojson` a GeoJSON geometry at any depth a GEOMETRY (`geojson`).
    pub fn of_options(json: &Json, case_insensitive: bool, geojson: bool) -> Structure {
        if geojson && pintail_common::geojson::is_geojson_geometry(json) {
            return Structure::Scalar("GEOMETRY");
        }
        let same = |a: &str, b: &str| {
            if case_insensitive {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        match json {
            Json::Null => Structure::Null,
            Json::Bool(_) => Structure::Scalar("BOOLEAN"),
            Json::UInt(_) => Structure::Scalar("UBIGINT"),
            Json::Int(_) => Structure::Scalar("BIGINT"),
            Json::Real(_) | Json::Raw(_) => Structure::Scalar("DOUBLE"),
            Json::Str(_) => Structure::Scalar("VARCHAR"),
            Json::Array(items) => {
                let mut element = Structure::Null;
                for item in items {
                    element =
                        element.merge_with(Structure::of_options(item, case_insensitive, geojson), case_insensitive);
                }
                Structure::List(Box::new(element))
            }
            Json::Object(items) => {
                let mut fields: Vec<(String, Structure)> = Vec::with_capacity(items.len());
                for (key, value) in items {
                    let structure = Structure::of_options(value, case_insensitive, geojson);
                    match fields.iter_mut().find(|(k, _)| same(k, key)) {
                        Some((_, existing)) => *existing = existing.clone().merge_with(structure, case_insensitive),
                        None => fields.push((key.clone(), structure)),
                    }
                }
                Structure::Object(fields)
            }
        }
    }

    pub fn merge(self, other: Structure) -> Structure {
        self.merge_with(other, false)
    }

    pub fn merge_with(self, other: Structure, case_insensitive: bool) -> Structure {
        let same = |a: &str, b: &str| {
            if case_insensitive {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        match (self, other) {
            (Structure::Null, other) | (other, Structure::Null) => other,
            (Structure::Scalar(a), Structure::Scalar(b)) => {
                if a == b {
                    return Structure::Scalar(a);
                }
                match (a, b) {
                    ("UBIGINT", "BIGINT") | ("BIGINT", "UBIGINT") => Structure::Scalar("BIGINT"),
                    ("UBIGINT" | "BIGINT", "DOUBLE") | ("DOUBLE", "UBIGINT" | "BIGINT") => Structure::Scalar("DOUBLE"),
                    _ => Structure::Mixed,
                }
            }
            (Structure::List(a), Structure::List(b)) => Structure::List(Box::new(a.merge_with(*b, case_insensitive))),
            (Structure::Object(mut a), Structure::Object(b)) => {
                for (key, value) in b {
                    match a.iter_mut().find(|(k, _)| same(k, &key)) {
                        Some((_, existing)) => *existing = existing.clone().merge_with(value, case_insensitive),
                        None => a.push((key, value)),
                    }
                }
                Structure::Object(a)
            }
            _ => Structure::Mixed,
        }
    }

    fn to_json(&self) -> Json {
        match self {
            Structure::Null => Json::Str("NULL".to_string()),
            Structure::Scalar(name) => Json::Str((*name).to_string()),
            Structure::List(element) => Json::Array(vec![element.to_json()]),
            // an empty object has no structure of its own
            Structure::Object(fields) if fields.is_empty() => Json::Str("JSON".to_string()),
            Structure::Object(fields) => Json::Object(fields.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
            Structure::Mixed => Json::Str("JSON".to_string()),
        }
    }

    /// `JSONStructure::StructureToType`
    pub fn logical_type(&self, depth: i64) -> LogicalType {
        self.logical_type_with(depth, true)
    }

    /// `StructureToType`, with `maps` whether `map_inference_threshold` lets objects be MAPs.
    pub fn logical_type_with(&self, depth: i64, maps: bool) -> LogicalType {
        if depth == 0 {
            return json_type();
        }
        match self {
            // only NULLs were seen (`case LogicalTypeId::SQLNULL`)
            Structure::Null => LogicalType::SQLNULL,
            Structure::Mixed => json_type(),
            Structure::Scalar("BOOLEAN") => LogicalType::BOOLEAN,
            // `refine_type` makes those that don't fit a HUGEINT
            Structure::Scalar("UBIGINT" | "BIGINT") => LogicalType::BIGINT,
            Structure::Scalar("DOUBLE") => LogicalType::DOUBLE,
            Structure::Scalar("GEOMETRY") => LogicalType::new(LogicalTypeId::Geometry),
            Structure::Scalar(_) => LogicalType::VARCHAR,
            Structure::List(element) => LogicalType::list(element.logical_type_with(depth - 1, maps)),
            // `StructureToTypeObject`: an empty object is a MAP of what it could hold
            Structure::Object(fields) if fields.is_empty() && maps => {
                LogicalType::map(LogicalType::VARCHAR, json_type())
            }
            Structure::Object(fields) if fields.is_empty() => json_type(),
            Structure::Object(fields) => LogicalType::struct_type(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.logical_type_with(depth - 1, maps)))
                    .collect(),
            ),
        }
    }
}

/// Types in a `json_transform` structure.
fn structure_type(json: &Json, parse_type: &dyn Fn(&str) -> Result<LogicalType>) -> Result<LogicalType> {
    match json {
        Json::Str(name) => parse_type(name),
        Json::Array(items) => match items.as_slice() {
            [item] => Ok(LogicalType::list(structure_type(item, parse_type)?)),
            _ => Err(binder_error("Too many values in array of JSON structure")),
        },
        Json::Object(items) if !items.is_empty() => {
            // `StructureToTypeObject`: STRUCT field names are case-insensitive
            let mut fields: Vec<(String, LogicalType)> = Vec::with_capacity(items.len());
            for (key, value) in items {
                if key.is_empty() {
                    return Err(binder_error("Empty key in object in JSON structure"));
                }
                if fields.iter().any(|(name, _)| name.eq_ignore_ascii_case(key)) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("Duplicate keys in object in JSON structure: {}", value.text()),
                    ));
                }
                fields.push((key.clone(), structure_type(value, parse_type)?));
            }
            Ok(LogicalType::struct_type(fields))
        }
        Json::Object(_) => Err(binder_error("Empty object in JSON structure")),
        _ => Err(binder_error(
            "invalid JSON structure: expected an object, an array or the name of a type",
        )),
    }
}

/// `JSON_MACROS`: the aggregates over JSON, as SQL over their arguments.
pub fn macro_text(name: &str, args: &[String]) -> Option<String> {
    match (name.to_ascii_lowercase().as_str(), args) {
        ("json_group_array", [x]) => Some(format!(
            "CAST('[' || string_agg(CASE WHEN {x} IS NULL THEN 'null'::JSON ELSE to_json({x}) END, ',') || ']' AS JSON)"
        )),
        ("json_group_object", [n, v]) => Some(format!(
            "CAST('{{' || string_agg(CASE WHEN {n} IS NULL THEN error('json_group_object key cannot be NULL') ELSE \
             to_json({n}::VARCHAR) END || ':' || CASE WHEN {v} IS NULL THEN 'null'::JSON ELSE to_json({v}) END, ',') \
             || '}}' AS JSON)"
        )),
        ("json_group_structure", [x]) => Some(format!("json_structure(json_group_array({x}))->0")),
        _ => None,
    }
}

/// Whether [`bind_function`] binds the function `name`.
pub fn is_json_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "json"
            | "json_valid"
            | "json_type"
            | "json_array_length"
            | "json_keys"
            | "json_exists"
            | "json_extract"
            | "json_extract_path"
            | "json_extract_string"
            | "json_extract_path_text"
            | "json_value"
            | "json_contains"
            | "json_structure"
            | "json_object"
            | "json_array"
            | "to_json"
            | "json_quote"
            | "array_to_json"
            | "row_to_json"
            | "json_merge_patch"
            | "json_merge_patch_diff"
            | "json_deep_merge"
            | "json_normalize"
            | "__json_each"
            | "__json_tree"
            | "json_pretty"
            | "json_transform"
            | "from_json"
            | "json_transform_strict"
            | "from_json_strict"
            | "json_strip_nulls"
            | "json_set"
            | "json_insert"
            | "json_replace"
            | "json_remove"
            | "->"
            | "->>"
    )
}

fn constant(child: &BoundExpr) -> Result<Option<Value>> {
    if child.is_foldable() {
        return evaluate_scalar(child).map(Some);
    }
    Ok(None)
}

fn document(value: &Value) -> Result<Option<Json>> {
    if value.is_null() {
        return Ok(None);
    }
    if value.logical_type().id() != LogicalTypeId::Varchar {
        return value_to_json(value).map(Some);
    }
    parsed(&value.try_to_string()?).map(Some)
}

/// A value to put in a document: text that is JSON is that JSON.
fn inserted(value: &Value) -> Result<Json> {
    if value.logical_type().id() == LogicalTypeId::Varchar && !value.is_null() {
        // the value is a JSON argument: text that isn't JSON fails its cast
        return parsed(&value.try_to_string()?);
    }
    value_to_json(value)
}

fn path_text(value: &Value) -> Result<String> {
    // an integer is an index: from the back if negative
    match value.as_i128() {
        Some(index) if value.logical_type().id() != LogicalTypeId::Varchar => Ok(if index < 0 {
            format!("$[#{index}]")
        } else {
            format!("$[{index}]")
        }),
        _ => value.try_to_string(),
    }
}

fn map_paths(value: &Value, f: impl Fn(&Path) -> Result<Value>, child: &LogicalType) -> Result<Value> {
    match value.data() {
        ValueData::List(items) => {
            let mut results = Vec::with_capacity(items.len());
            for item in items {
                results.push(f(&parse_path(&path_text(item)?)?)?);
            }
            Value::list(child.clone(), results)
        }
        _ => f(&parse_path(&path_text(value)?)?),
    }
}

fn nothing(ty: &LogicalType) -> Value {
    Value::null(ty.clone())
}

fn with_path(
    args: &DataChunk,
    result: &mut Vector,
    ty: LogicalType,
    f: fn(Option<&Json>) -> Result<Value>,
) -> Result<()> {
    row_wise(args, result, |row| {
        // a NULL path gives NULL, without reading the document
        if row.len() > 1 && row[1].is_null() {
            return Ok(nothing(&ty));
        }
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&ty));
        };
        if row.len() == 1 {
            return f(Some(&json));
        }
        map_paths(&row[1], |path| each_found(&json, path, &ty, f), &ty)
    })
}

/// What one path gives: a list of all it finds, if it can find several.
fn each_found(json: &Json, path: &Path, ty: &LogicalType, f: impl Fn(Option<&Json>) -> Result<Value>) -> Result<Value> {
    if !path.is_wildcard() {
        return f(single(json, path));
    }
    // `JSONCommon::Get`: a path that is not constant reads one value, which a wildcard can't name
    if ty.id() != LogicalTypeId::List {
        return Err(Error::new(ErrorKind::InvalidInput, "JSON path error near '*'"));
    }
    let mut found = Vec::new();
    walk(json, &path.steps, &mut found);
    let values = found.into_iter().map(|item| f(Some(item))).collect::<Result<_>>()?;
    Value::list(ty.clone(), values)
}

fn extract_json(found: Option<&Json>) -> Result<Value> {
    match found {
        Some(value) => Value::varchar(value.text()).reinterpret(&json_type()),
        None => Ok(nothing(&json_type())),
    }
}

fn extract_string(found: Option<&Json>) -> Result<Value> {
    Ok(match found {
        Some(Json::Null) | None => nothing(&LogicalType::VARCHAR),
        Some(Json::Str(text)) => Value::varchar(text.as_str()),
        Some(other) => Value::varchar(other.text()),
    })
}

fn extract_value(found: Option<&Json>) -> Result<Value> {
    match found {
        Some(Json::Array(_) | Json::Object(_) | Json::Null) => Ok(nothing(&json_type())),
        other => extract_json(other),
    }
}

fn json_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, json_type(), extract_json)
}

fn json_extract_string(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, LogicalType::VARCHAR, extract_string)
}

fn json_value(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, json_type(), extract_value)
}

/// All that a wildcard path finds, as a list.
fn wildcard_values(
    args: &DataChunk,
    result: &mut Vector,
    child: LogicalType,
    f: fn(&Json) -> Result<Value>,
) -> Result<()> {
    row_wise(args, result, |row| {
        let list = LogicalType::list(child.clone());
        if row[1].is_null() {
            return Ok(nothing(&list));
        }
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&list));
        };
        let path = parse_path(&path_text(&row[1])?)?;
        let mut found = Vec::new();
        walk(&json, &path.steps, &mut found);
        let values = found.into_iter().map(f).collect::<Result<_>>()?;
        Value::list(child.clone(), values)
    })
}

fn json_extract_wildcard(args: &DataChunk, result: &mut Vector) -> Result<()> {
    wildcard_values(args, result, json_type(), |json| extract_json(Some(json)))
}

fn json_extract_string_wildcard(args: &DataChunk, result: &mut Vector) -> Result<()> {
    wildcard_values(args, result, LogicalType::VARCHAR, |json| extract_string(Some(json)))
}

fn json_type_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, LogicalType::VARCHAR, |found| {
        Ok(found.map_or(nothing(&LogicalType::VARCHAR), |json| Value::varchar(json.type_name())))
    })
}

fn json_exists(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, LogicalType::BOOLEAN, |found| {
        Ok(Value::boolean(found.is_some()))
    })
}

fn json_array_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    with_path(args, result, LogicalType::UBIGINT, |found| {
        Ok(match found {
            Some(Json::Array(items)) => Value::ubigint(items.len() as u64),
            Some(_) => Value::ubigint(0),
            None => nothing(&LogicalType::UBIGINT),
        })
    })
}

fn json_keys(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let list = LogicalType::list(LogicalType::VARCHAR);
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&list));
        };
        let keys = |found: Option<&Json>| -> Result<Value> {
            match found {
                Some(Json::Object(items)) => Value::list(
                    LogicalType::VARCHAR,
                    items.iter().map(|(k, _)| Value::varchar(k.as_str())).collect(),
                ),
                Some(_) => Value::list(LogicalType::VARCHAR, Vec::new()),
                None => Ok(nothing(&list)),
            }
        };
        if row.len() == 1 {
            return keys(Some(&json));
        }
        map_paths(&row[1], |path| each_found(&json, path, &list, keys), &list)
    })
}

fn json_valid(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(nothing(&LogicalType::BOOLEAN));
        }
        Ok(Value::boolean(parse(&row[0].try_to_string()?).is_ok()))
    })
}

fn contains(haystack: &Json, needle: &Json) -> bool {
    fn subset(haystack: &Json, needle: &Json) -> bool {
        match (haystack, needle) {
            (Json::Object(items), Json::Object(wanted)) => wanted
                .iter()
                .all(|(key, value)| items.iter().any(|(k, v)| k == key && subset(v, value))),
            (Json::Array(items), Json::Array(wanted)) => {
                wanted.iter().all(|value| items.iter().any(|item| subset(item, value)))
            }
            (a, b) => equal_scalars(a, b),
        }
    }
    if subset(haystack, needle) {
        return true;
    }
    match haystack {
        Json::Array(items) => items.iter().any(|item| contains(item, needle)),
        Json::Object(items) => items.iter().any(|(_, item)| contains(item, needle)),
        _ => false,
    }
}

fn equal_scalars(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::UInt(x), Json::Int(y)) | (Json::Int(y), Json::UInt(x)) => i128::from(*x) == i128::from(*y),
        _ => a == b,
    }
}

fn json_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let (Some(haystack), Some(needle)) = (document(&row[0])?, document(&row[1])?) else {
            return Ok(nothing(&LogicalType::BOOLEAN));
        };
        Ok(Value::boolean(contains(&haystack, &needle)))
    })
}

fn json_structure(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&json_type()));
        };
        Value::varchar(Structure::of(&json).to_json().text()).reinterpret(&json_type())
    })
}

fn json_object(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let mut items = Vec::with_capacity(row.len() / 2);
        for pair in row.chunks(2) {
            if pair[0].is_null() {
                return Err(invalid("JSON key cannot be NULL"));
            }
            items.push((pair[0].try_to_string()?, value_to_json(&pair[1])?));
        }
        Value::varchar(Json::Object(items).text()).reinterpret(&json_type())
    })
}

fn json_array(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let items = row.iter().map(value_to_json).collect::<Result<_>>()?;
        Value::varchar(Json::Array(items).text()).reinterpret(&json_type())
    })
}

fn to_json(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(nothing(&json_type()));
        }
        Value::varchar(value_to_json(&row[0])?.text()).reinterpret(&json_type())
    })
}

/// `yyjson_merge_patch`: what the patch doesn't name, then what it does. Without recursion: each
/// object of the patch that merges into one of the target is a frame of its own.
fn merge_patch(target: Json, patch: &Json) -> Json {
    struct Frame<'a> {
        key: String,
        merged: Vec<(String, Json)>,
        /// the members of the target that the patch names, for what it names them as
        patched: Vec<(String, Json)>,
        patch: std::slice::Iter<'a, (String, Json)>,
    }
    fn open<'a>(key: String, target: Json, wanted: &'a [(String, Json)]) -> Frame<'a> {
        let named = |key: &str| wanted.iter().any(|(k, _)| k == key);
        let (patched, merged) = target
            .into_object()
            .unwrap_or_default()
            .into_iter()
            .partition(|(key, _)| named(key));
        Frame {
            key,
            merged,
            patched,
            patch: wanted.iter(),
        }
    }
    let Json::Object(wanted) = patch else {
        return patch.clone();
    };
    let mut stack = vec![open(String::new(), target, wanted)];
    loop {
        let frame = stack.last_mut().expect("open");
        match frame.patch.next() {
            Some((_, Json::Null)) => {}
            Some((key, value @ Json::Object(wanted))) => {
                let old = frame
                    .patched
                    .iter()
                    .position(|(k, _)| k == key)
                    .map_or(Json::Null, |i| frame.patched.remove(i).1);
                let _ = value;
                stack.push(open(key.clone(), old, wanted));
            }
            Some((key, value)) => {
                if let Some(i) = frame.patched.iter().position(|(k, _)| k == key) {
                    frame.patched.remove(i);
                }
                frame.merged.push((key.clone(), value.clone()));
            }
            None => {
                let done = stack.pop().expect("open");
                let merged = Json::Object(done.merged);
                match stack.last_mut() {
                    Some(parent) => parent.merged.push((done.key, merged)),
                    None => return merged,
                }
            }
        }
    }
}

fn json_merge_patch(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let mut merged: Option<Json> = None;
        for value in row {
            let patch = document(value)?.unwrap_or(Json::Null);
            merged = Some(match merged {
                Some(target) => merge_patch(target, &patch),
                None => patch,
            });
        }
        match merged {
            Some(Json::Null) | None => Ok(nothing(&json_type())),
            Some(merged) => Value::varchar(merged.text()).reinterpret(&json_type()),
        }
    })
}

/// `DeepMerge`: a merge where a NULL in the patch keeps what was there, and objects merge all the way
/// down. Without recursion: each pair of objects that merge is a frame of its own.
fn deep_merge(original: Json, patch: Json) -> Json {
    struct Frame {
        key: String,
        merged: Vec<(String, Json)>,
        /// the members of the original that a member of the patch replaces
        replaced: Vec<(String, Json)>,
        patch: std::vec::IntoIter<(String, Json)>,
    }
    fn open(key: String, original: Vec<(String, Json)>, patch: Vec<(String, Json)>) -> Frame {
        let replaces = |key: &str| {
            patch
                .iter()
                .find(|(k, _)| k == key)
                .is_some_and(|(_, value)| !matches!(value, Json::Null))
        };
        let (replaced, merged) = original.into_iter().partition(|(key, _)| replaces(key));
        Frame {
            key,
            merged,
            replaced,
            patch: patch.into_iter(),
        }
    }
    let (original, patch) = match (original.into_object(), patch.into_object()) {
        (Ok(original), Ok(patch)) => (original, patch),
        (original, Err(Json::Null)) => return original.map_or_else(|o| o, Json::Object),
        (_, patch) => return patch.map_or_else(|p| p, Json::Object),
    };
    let mut stack = vec![open(String::new(), original, patch)];
    loop {
        let frame = stack.last_mut().expect("open");
        match frame.patch.next() {
            Some((_, Json::Null)) => {}
            Some((key, value)) => {
                let old = frame
                    .replaced
                    .iter_mut()
                    .find(|(k, _)| *k == key)
                    .map(|(_, old)| std::mem::replace(old, Json::Null));
                match (old.map(Json::into_object), value.into_object()) {
                    (Some(Ok(old)), Ok(value)) => stack.push(open(key, old, value)),
                    (_, value) => frame.merged.push((key, value.map_or_else(|v| v, Json::Object))),
                }
            }
            None => {
                let done = stack.pop().expect("open");
                let merged = Json::Object(done.merged);
                match stack.last_mut() {
                    Some(parent) => parent.merged.push((done.key, merged)),
                    None => return merged,
                }
            }
        }
    }
}

fn json_deep_merge(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let mut merged: Option<Json> = None;
        for (i, value) in row.iter().enumerate() {
            let patch = document(value)?;
            merged = match (i, merged, patch) {
                (0, _, patch) => patch,
                (_, _, None) => None,
                (_, None, patch) => patch,
                (_, Some(original), Some(patch)) => Some(deep_merge(original, patch)),
            };
        }
        match merged {
            None => Ok(nothing(&json_type())),
            Some(merged) => Value::varchar(merged.text()).reinterpret(&json_type()),
        }
    })
}

/// `ComputeDiff`: the merge patch that makes `old` into `new`, both objects; `None` if they are the
/// same. Without recursion: each pair of objects under the same key is a frame of its own.
fn object_diff(old: &[(String, Json)], new: &[(String, Json)]) -> Option<Json> {
    struct Frame<'a> {
        key: &'a str,
        diff: Vec<(String, Json)>,
        old: &'a [(String, Json)],
        new: std::slice::Iter<'a, (String, Json)>,
    }
    fn open<'a>(key: &'a str, old: &'a [(String, Json)], new: &'a [(String, Json)]) -> Frame<'a> {
        let diff = old
            .iter()
            .filter(|(key, _)| !new.iter().any(|(k, _)| k == key))
            .map(|(key, _)| (key.clone(), Json::Null))
            .collect();
        Frame {
            key,
            diff,
            old,
            new: new.iter(),
        }
    }
    let mut stack = vec![open("", old, new)];
    loop {
        let frame = stack.last_mut().expect("open");
        match frame.new.next() {
            Some((key, value)) => {
                let before = frame.old.iter().find(|(k, _)| k == key).map(|(_, v)| v);
                match (before, value) {
                    (Some(Json::Object(before)), Json::Object(after)) => stack.push(open(key, before, after)),
                    (Some(before), after) if before == after => {}
                    _ => frame.diff.push((key.clone(), value.clone())),
                }
            }
            None => {
                let done = stack.pop().expect("open");
                let diff = (!done.diff.is_empty()).then_some(Json::Object(done.diff));
                match (stack.last_mut(), diff) {
                    (Some(parent), Some(diff)) => parent.diff.push((done.key.to_string(), diff)),
                    (Some(_), None) => {}
                    (None, diff) => return diff,
                }
            }
        }
    }
}

fn json_merge_patch_diff(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(new) = document(&row[1])? else {
            return Ok(nothing(&json_type()));
        };
        let old = document(&row[0])?;
        let diff = match (&old, &new) {
            (Some(Json::Object(old)), Json::Object(after)) => {
                object_diff(old, after).unwrap_or(Json::Object(Vec::new()))
            }
            _ => new,
        };
        Value::varchar(diff.text()).reinterpret(&json_type())
    })
}

/// `SortKeys`: the keys of every object in byte order.
fn sort_keys(json: Json) -> Json {
    json.map_containers(|container| match container.into_object() {
        Ok(mut items) => {
            items.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
            Json::Object(items)
        }
        Err(other) => other,
    })
}

fn json_normalize(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&LogicalType::VARCHAR));
        };
        Ok(Value::varchar(sort_keys(json).text()))
    })
}

fn strip_nulls(json: Json) -> Json {
    json.map_containers(|container| match container.into_object() {
        Ok(items) => Json::Object(
            items
                .into_iter()
                .filter(|(_, value)| !matches!(value, Json::Null))
                .collect(),
        ),
        Err(other) => other,
    })
}

fn json_strip_nulls(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&json_type()));
        };
        Value::varchar(strip_nulls(json).text()).reinterpret(&json_type())
    })
}

fn remove_path(json: &mut Json, steps: &[Step]) {
    let Some((step, rest)) = steps.split_first() else {
        return;
    };
    let last = rest.is_empty();
    match (json, step) {
        (Json::Object(items), Step::Key(key)) => {
            if let Some(position) = items.iter().position(|(k, _)| k == key) {
                if last {
                    items.remove(position);
                } else {
                    remove_path(&mut items[position].1, rest);
                }
            }
        }
        (Json::Array(items), Step::Index(_) | Step::ReverseIndex(_)) => {
            let position = match step {
                Step::Index(index) => Some(*index),
                Step::ReverseIndex(index) => items.len().checked_sub(*index),
                _ => None,
            };
            if let Some(position) = position.filter(|p| *p < items.len()) {
                if last {
                    items.remove(position);
                } else {
                    remove_path(&mut items[position], rest);
                }
            }
        }
        _ => {}
    }
}

/// What `json_set` and its kin do with a path that is or isn't there.
#[derive(Clone, Copy)]
struct Modify {
    insert: bool,
    replace: bool,
}

/// `ResolveArrayPosition`: where an array step is in an array of `len` items, at most its end.
fn array_position(step: &Step, len: usize) -> Option<usize> {
    let position = match step {
        Step::Index(index) => *index,
        Step::ReverseIndex(index) => len.checked_sub(*index)?,
        Step::Append => len,
        _ => return None,
    };
    (position <= len).then_some(position)
}

/// `ApplyModify` at the last step of a path, in its parent: whether it changed it.
fn apply_modify(parent: &mut Json, step: &Step, value: &Json, modify: &Modify) -> bool {
    match (parent, step) {
        (Json::Object(items), Step::Key(key)) => match items.iter().position(|(k, _)| k == key) {
            Some(at) if modify.replace => {
                items[at].1 = value.clone();
                true
            }
            None if modify.insert => {
                items.push((key.clone(), value.clone()));
                true
            }
            _ => false,
        },
        (Json::Array(items), step) if !matches!(step, Step::Key(_)) => {
            let Some(position) = array_position(step, items.len()) else {
                return false;
            };
            if position < items.len() {
                if modify.replace {
                    items[position] = value.clone();
                }
                modify.replace
            } else {
                if modify.insert {
                    items.push(value.clone());
                }
                modify.insert
            }
        }
        _ => false,
    }
}

/// `ModifyDocumentElements`: the value at the path set, inserted or replaced. Setting and inserting
/// make the containers that are missing on the way, as SQLite does, but only if the value goes in.
fn set_path(json: &mut Json, steps: &[Step], value: &Json, modify: Modify) {
    let Some((last, parents)) = steps.split_last() else {
        if modify.replace {
            *json = value.clone();
        }
        return;
    };
    // `ResolveParent`: the positions of the parents that are there
    let mut positions = Vec::with_capacity(parents.len());
    let mut current: &Json = json;
    let mut missing = None;
    for (i, step) in parents.iter().enumerate() {
        let child = match (current, step) {
            (Json::Object(items), Step::Key(key)) => items.iter().position(|(k, _)| k == key),
            (Json::Array(items), step) if !matches!(step, Step::Key(_)) => {
                let Some(position) = array_position(step, items.len()) else {
                    return;
                };
                (position < items.len()).then_some(position)
            }
            _ => return,
        };
        let Some(at) = child else {
            missing = Some(i);
            break;
        };
        positions.push(at);
        current = match current {
            Json::Object(items) => &items[at].1,
            Json::Array(items) => &items[at],
            _ => unreachable!("a container"),
        };
    }
    let mut target: &mut Json = json;
    for at in positions {
        target = match target {
            Json::Object(items) => &mut items[at].1,
            Json::Array(items) => &mut items[at],
            _ => unreachable!("a container"),
        };
    }
    let Some(missing) = missing else {
        apply_modify(target, last, value, &modify);
        return;
    };
    if !modify.insert {
        return;
    }
    // the chain of containers that is missing, built inside out, each made by the step after it
    let container = |step: &Step| match step {
        Step::Key(_) => Json::Object(Vec::new()),
        _ => Json::Array(Vec::new()),
    };
    let mut chain = container(last);
    if !apply_modify(&mut chain, last, value, &modify) {
        return;
    }
    for at in (missing + 1..parents.len()).rev() {
        let mut parent = container(&steps[at]);
        if !apply_modify(
            &mut parent,
            &steps[at],
            &chain,
            &Modify {
                insert: true,
                replace: false,
            },
        ) {
            return;
        }
        chain = parent;
    }
    apply_modify(
        target,
        &steps[missing],
        &chain,
        &Modify {
            insert: true,
            replace: false,
        },
    );
}

fn modify_rows(args: &DataChunk, result: &mut Vector, modify: Modify) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(mut json) = document(&row[0])? else {
            return Ok(nothing(&json_type()));
        };
        for pair in row[1..].chunks(2) {
            if pair[0].is_null() {
                return Ok(nothing(&json_type()));
            }
            if pair[1].is_null() {
                return Ok(nothing(&json_type()));
            }
            let path = modify_path(&path_text(&pair[0])?)?;
            let value = inserted(&pair[1])?;
            set_path(&mut json, &path.steps, &value, modify);
        }
        Value::varchar(json.text()).reinterpret(&json_type())
    })
}

fn json_set(args: &DataChunk, result: &mut Vector) -> Result<()> {
    modify_rows(
        args,
        result,
        Modify {
            insert: true,
            replace: true,
        },
    )
}

fn json_insert(args: &DataChunk, result: &mut Vector) -> Result<()> {
    modify_rows(
        args,
        result,
        Modify {
            insert: true,
            replace: false,
        },
    )
}

fn json_replace(args: &DataChunk, result: &mut Vector) -> Result<()> {
    modify_rows(
        args,
        result,
        Modify {
            insert: false,
            replace: true,
        },
    )
}

/// `ParseModifyPath`: a path for the functions that change a document, which can't have wildcards.
fn modify_path(text: &str) -> Result<Path> {
    let path = parse_path(text)?;
    let wildcard = path.steps.iter().any(|step| {
        matches!(
            step,
            Step::AllKeys | Step::AllItems | Step::RecursiveWildcard | Step::RecursiveKey(_)
        )
    });
    if wildcard {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "JSON path wildcards are not supported in JSON modification functions",
        ));
    }
    Ok(path)
}

fn json_remove(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(mut json) = document(&row[0])? else {
            return Ok(nothing(&json_type()));
        };
        for path in &row[1..] {
            if path.is_null() {
                return Ok(nothing(&json_type()));
            }
            let path = modify_path(&path_text(path)?)?;
            if path.steps.is_empty() {
                return Ok(nothing(&json_type()));
            }
            remove_path(&mut json, &path.steps);
        }
        Value::varchar(json.text()).reinterpret(&json_type())
    })
}

fn json_pretty(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&LogicalType::VARCHAR));
        };
        let mut out = String::new();
        json.pretty(0, &mut out);
        Ok(Value::varchar(out))
    })
}

fn transform_rows(args: &DataChunk, result: &mut Vector, strict: bool) -> Result<()> {
    let ty = result.logical_type().clone();
    // `JSONCommon::NumbersAsRaw`: a DECIMAL reads numbers from their text
    let raw = contains_decimal(&ty);
    row_wise(args, result, |row| {
        let json = if raw && !row[0].is_null() && row[0].logical_type().id() == LogicalTypeId::Varchar {
            let text = row[0].try_to_string()?;
            Some(parse_with(&text, true).map_err(|e| invalid(parse_error_text(&text, &e)))?)
        } else {
            document(&row[0])?
        };
        let Some(json) = json else {
            return Ok(nothing(&ty));
        };
        // `JSONTransformOptions(strict, strict, strict, false)`: strictly, a key twice is an error
        let formats = Formats {
            error_duplicate_key: strict,
            ..Formats::default()
        };
        transform_with(&json, &ty, strict, &formats)
    })
}

fn json_transform(args: &DataChunk, result: &mut Vector) -> Result<()> {
    transform_rows(args, result, false)
}

fn json_transform_strict(args: &DataChunk, result: &mut Vector) -> Result<()> {
    transform_rows(args, result, true)
}

/// The overloads of `json_extract` and `json_extract_string`: a BIGINT, VARCHAR or VARCHAR[] path.
fn extract_overload(name: &str, children: &[BoundExpr]) -> Result<()> {
    use LogicalTypeId as T;
    let text = |id: T| matches!(id, T::Varchar | T::StringLiteral | T::SqlNull | T::Unknown);
    let path = &children[1].return_type;
    let path_fits = text(path.id())
        || path.is_integral()
        || matches!(path.id(), T::IntegerLiteral)
        || (path.id() == T::List && path.child_type().is_some_and(|c| text(c.id())));
    // (any document: what is no VARCHAR other overloads take, `->` of a STRUCT is its field)
    if path_fits {
        return Ok(());
    }
    let types: Vec<String> = children
        .iter()
        .map(|c| super::binder::expression_return_type(c).to_string())
        .collect();
    Err(binder_error(format!(
        "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
         casts.",
        types.join(", ")
    )))
}

fn expect_args(name: &str, children: &[BoundExpr], low: usize, high: usize) -> Result<()> {
    if children.len() < low || children.len() > high {
        return Err(binder_error(format!(
            "No function matches the given name and argument types of '{name}'. You might need to add explicit type \
             casts."
        )));
    }
    Ok(())
}

/// What a path argument makes of the result: `None` for a path known only at run time.
fn path_argument(child: &BoundExpr) -> Result<Option<(bool, bool)>> {
    let Some(value) = constant(child)? else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some((false, false)));
    }
    if let ValueData::List(items) = value.data() {
        for item in items {
            let path = parse_path(&path_text(item)?).map_err(|e| binder_error(e.message))?;
            // `JSONReadManyFunctionData::Bind`
            if path.is_wildcard() {
                return Err(binder_error(
                    "Cannot have wildcards in JSON path when supplying multiple paths",
                ));
            }
        }
        return Ok(Some((true, false)));
    }
    let path = parse_path(&path_text(&value)?).map_err(|e| binder_error(e.message))?;
    Ok(Some((false, path.is_wildcard())))
}

fn returning_paths(
    name: &str,
    children: Vec<BoundExpr>,
    ty: LogicalType,
    callback: super::ScalarCallback,
) -> Result<BoundExpr> {
    let list = children.len() > 1 && path_list(&children[1])?;
    let ty = if list { LogicalType::list(ty) } else { ty };
    Ok(bound_function(name, children, ty, callback))
}

fn path_list(child: &BoundExpr) -> Result<bool> {
    Ok(path_argument(child)?.is_some_and(|(list, wildcard)| list || wildcard)
        || child.return_type.id() == LogicalTypeId::List)
}

/// Binds a function of [`is_json_function`]. `parse_type` reads the types of a `json_transform`.
pub fn bind_function(
    name: &str,
    children: Vec<BoundExpr>,
    parse_type: &dyn Fn(&str) -> Result<LogicalType>,
) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    let boolean = LogicalType::BOOLEAN;
    match lower.as_str() {
        "json" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, json_type(), json_extract))
        }
        "json_valid" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, boolean, json_valid))
        }
        "json_type" => {
            expect_args(&lower, &children, 1, 2)?;
            returning_paths(&lower, children, LogicalType::VARCHAR, json_type_function)
        }
        "json_array_length" => {
            expect_args(&lower, &children, 1, 2)?;
            returning_paths(&lower, children, LogicalType::UBIGINT, json_array_length)
        }
        "json_exists" => {
            expect_args(&lower, &children, 2, 2)?;
            returning_paths(&lower, children, boolean, json_exists)
        }
        "json_keys" => {
            expect_args(&lower, &children, 1, 2)?;
            returning_paths(&lower, children, LogicalType::list(LogicalType::VARCHAR), json_keys)
        }
        "json_extract" | "json_extract_path" | "->" => {
            expect_args(&lower, &children, 2, 2)?;
            extract_overload("json_extract", &children)?;
            if path_argument(&children[1])?.is_some_and(|(_, wildcard)| wildcard) {
                let ty = LogicalType::list(json_type());
                return Ok(bound_function(&lower, children, ty, json_extract_wildcard));
            }
            returning_paths(&lower, children, json_type(), json_extract)
        }
        "json_extract_string" | "json_extract_path_text" | "->>" => {
            expect_args(&lower, &children, 2, 2)?;
            extract_overload("json_extract_string", &children)?;
            if path_argument(&children[1])?.is_some_and(|(_, wildcard)| wildcard) {
                let ty = LogicalType::list(LogicalType::VARCHAR);
                return Ok(bound_function(&lower, children, ty, json_extract_string_wildcard));
            }
            returning_paths(&lower, children, LogicalType::VARCHAR, json_extract_string)
        }
        "json_value" => {
            expect_args(&lower, &children, 2, 2)?;
            returning_paths(&lower, children, json_type(), json_value)
        }
        "json_contains" => {
            expect_args(&lower, &children, 2, 2)?;
            Ok(bound_function(&lower, children, boolean, json_contains))
        }
        "json_structure" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, json_type(), json_structure))
        }
        "json_object" => {
            if !children.len().is_multiple_of(2) {
                return Err(binder_error("json_object() requires an even number of arguments"));
            }
            if children
                .iter()
                .step_by(2)
                .any(|key| key.return_type.id() != LogicalTypeId::Varchar)
            {
                return Err(binder_error(
                    "json_object() keys must be VARCHAR, add an explicit cast to argument",
                ));
            }
            Ok(bound_function(&lower, children, json_type(), json_object))
        }
        "json_array" => Ok(bound_function(&lower, children, json_type(), json_array)),
        "to_json" | "json_quote" | "array_to_json" | "row_to_json" => {
            if children.len() != 1 {
                return Err(binder_error(format!("{lower}() takes exactly one argument")));
            }
            // `ArrayToJSONBind` and `RowToJSONBind`
            let id = children[0].return_type.id();
            if lower == "array_to_json"
                && !matches!(id, LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::SqlNull)
            {
                return Err(binder_error("array_to_json() argument type must be LIST or ARRAY"));
            }
            if lower == "row_to_json" && !matches!(id, LogicalTypeId::Struct | LogicalTypeId::SqlNull) {
                return Err(binder_error("row_to_json() argument type must be STRUCT"));
            }
            Ok(bound_function(&lower, children, json_type(), to_json))
        }
        "json_merge_patch" => {
            if children.len() < 2 {
                return Err(binder_error("json_merge_patch requires at least two parameters"));
            }
            Ok(bound_function(&lower, children, json_type(), json_merge_patch))
        }
        "json_deep_merge" => {
            if children.len() < 2 {
                return Err(binder_error("json_deep_merge requires at least two parameters"));
            }
            Ok(bound_function(&lower, children, json_type(), json_deep_merge))
        }
        "json_merge_patch_diff" => {
            expect_args(&lower, &children, 2, 2)?;
            Ok(bound_function(&lower, children, json_type(), json_merge_patch_diff))
        }
        "__json_each" | "__json_tree" => {
            expect_args(&lower, &children, 1, 2)?;
            let callback: super::ScalarCallback = if lower == "__json_tree" {
                json_tree_rows
            } else {
                json_each_rows
            };
            Ok(bound_function(&lower, children, table_in_out_type(), callback))
        }
        "json_normalize" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, LogicalType::VARCHAR, json_normalize))
        }
        "json_strip_nulls" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, json_type(), json_strip_nulls))
        }
        "json_remove" => {
            if children.len() < 2 {
                return Err(binder_error("json_remove requires at least one path"));
            }
            Ok(bound_function(&lower, children, json_type(), json_remove))
        }
        "json_set" | "json_insert" | "json_replace" => {
            if children.len() < 3 || children.len().is_multiple_of(2) {
                return Err(binder_error(format!("{lower} requires pairs of a path and a value")));
            }
            let callback: super::ScalarCallback = match lower.as_str() {
                "json_set" => json_set,
                "json_insert" => json_insert,
                _ => json_replace,
            };
            Ok(bound_function(&lower, children, json_type(), callback))
        }
        "json_pretty" => {
            expect_args(&lower, &children, 1, 1)?;
            Ok(bound_function(&lower, children, LogicalType::VARCHAR, json_pretty))
        }
        "json_transform" | "from_json" | "json_transform_strict" | "from_json_strict" => {
            expect_args(&lower, &children, 2, 2)?;
            let Some(structure) = constant(&children[1])? else {
                return Err(binder_error("JSON structure must be a constant!"));
            };
            // `JSONTransformBind`: a NULL structure gives NULL
            if structure.is_null() || children[1].return_type.id() == LogicalTypeId::SqlNull {
                return Ok(BoundExpr::constant(Value::sql_null()));
            }
            let text = structure.try_to_string()?;
            let json = parse(&text).map_err(|e| binder_error(parse_error_text(&text, &e)))?;
            let ty = structure_type(&json, parse_type)?;
            let strict = lower.ends_with("strict");
            let callback: super::ScalarCallback = if strict { json_transform_strict } else { json_transform };
            Ok(bound_function(&lower, children, ty, callback))
        }
        _ => Err(binder_error(format!("Unknown JSON function {lower}"))),
    }
}

/// How many values a document has from `json` on, keys counted (`yyjson_doc_get_val_count`): what the ids of
/// `json_each` and `json_tree` count.
fn value_count(json: &Json) -> u64 {
    match json {
        Json::Array(items) => 1 + items.iter().map(value_count).sum::<u64>(),
        Json::Object(items) => 1 + items.iter().map(|(_, value)| 1 + value_count(value)).sum::<u64>(),
        _ => 1,
    }
}

/// `AppendObjectPathElement`: `.key`, the key quoted unless it is a letter followed by letters, digits and `_`.
fn append_path_key(path: &mut String, key: &str) {
    path.push('.');
    let bytes = key.as_bytes();
    let plain = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..].iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_');
    if plain {
        path.push_str(key);
        return;
    }
    path.push('"');
    for c in key.chars() {
        if c == '"' || c == '\\' {
            path.push('\\');
        }
        path.push(c);
    }
    path.push('"');
}

/// The value at `steps` under `json` whose id is `id`, with its id.
fn find_with_id<'a>(json: &'a Json, id: u64, steps: &[Step]) -> Option<(&'a Json, u64)> {
    let Some((step, rest)) = steps.split_first() else {
        return Some((json, id));
    };
    match (step, json) {
        (Step::Key(key), Json::Object(items)) => {
            let mut at = id + 1;
            for (k, value) in items {
                if k == key {
                    return find_with_id(value, at + 1, rest);
                }
                at += 1 + value_count(value);
            }
            None
        }
        (Step::Index(_) | Step::ReverseIndex(_), Json::Array(items)) => {
            let index = match step {
                Step::Index(index) => *index,
                Step::ReverseIndex(back) => items.len().checked_sub(*back)?,
                _ => unreachable!("an index"),
            };
            let mut at = id + 1;
            for (i, item) in items.iter().enumerate() {
                if i == index {
                    return find_with_id(item, at, rest);
                }
                at += value_count(item);
            }
            None
        }
        _ => None,
    }
}

/// The rows `json_each` (or with `tree`, `json_tree`) makes of a document (`JSONTableInOutFunction`): `key`, `value`,
/// `type`, `atom`, `id`, `parent`, `fullkey` and `path`, then the hidden `json`, `root` and `rowid`.
pub fn table_in_out_rows(document_value: &Value, path_value: Option<&Value>, tree: bool) -> Result<Vec<Vec<Value>>> {
    let binder = |message: &str| Error::new(ErrorKind::Binder, message.to_string());
    let root_path = match path_value {
        Some(value) if value.is_null() => return Ok(Vec::new()),
        Some(value) => path_text(value)?,
        None => "$".to_string(),
    };
    let path = parse_path(&root_path)?;
    if path.is_wildcard() {
        return Err(binder("Wildcard JSON path not supported in json_each/json_tree"));
    }
    if !root_path.starts_with('$') {
        return Err(binder("JSON path must start with '$' for json_each/json_tree"));
    }
    let Some(json) = document(document_value)? else {
        return Ok(Vec::new());
    };
    let text = if document_value.logical_type().id() == LogicalTypeId::Varchar {
        document_value.try_to_string()?
    } else {
        json.text()
    };
    let Some((root, root_id)) = find_with_id(&json, 0, &path.steps) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    let container = matches!(root, Json::Array(_) | Json::Object(_));
    if !container || tree {
        rows.push(vec![
            Value::null(LogicalType::VARCHAR),
            extract_json(Some(root))?,
            Value::varchar(root.type_name()),
            extract_value(Some(root))?,
            Value::ubigint(root_id),
            Value::null(LogicalType::UBIGINT),
            Value::varchar(root_path.as_str()),
            Value::varchar(root_path.as_str()),
        ]);
    }
    if container {
        rows.extend(table_in_out_subtree(root, root_id, &root_path, tree)?);
    }
    let json_value = Value::varchar(text.as_str()).reinterpret(&json_type())?;
    for (rowid, row) in rows.iter_mut().enumerate() {
        row.push(json_value.clone());
        row.push(Value::varchar(root_path.as_str()));
        row.push(Value::bigint(rowid as i64));
    }
    Ok(rows)
}

/// The rows of the values in `parent`, a container with id `parent_id` at `parent_path`: with `tree`, of all the
/// values under it, depth first; the hidden columns are left for the caller.
fn table_in_out_subtree(parent: &Json, parent_id: u64, parent_path: &str, tree: bool) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    let mut at = parent_id + 1;
    let children: Vec<(Option<String>, String, &Json, u64)> = match parent {
        Json::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let id = at;
                at += value_count(item);
                (Some(i.to_string()), format!("{parent_path}[{i}]"), item, id)
            })
            .collect(),
        Json::Object(items) => items
            .iter()
            .map(|(key, value)| {
                let mut fullkey = parent_path.to_string();
                append_path_key(&mut fullkey, key);
                let id = at + 1;
                at += 1 + value_count(value);
                (Some(key.clone()), fullkey, value, id)
            })
            .collect(),
        _ => Vec::new(),
    };
    for (key, fullkey, value, id) in children {
        rows.push(vec![
            key.map_or(Value::null(LogicalType::VARCHAR), Value::varchar),
            extract_json(Some(value))?,
            Value::varchar(value.type_name()),
            extract_value(Some(value))?,
            Value::ubigint(id),
            if tree {
                Value::ubigint(parent_id)
            } else {
                Value::null(LogicalType::UBIGINT)
            },
            Value::varchar(fullkey.as_str()),
            Value::varchar(parent_path),
        ]);
        if tree && matches!(value, Json::Array(_) | Json::Object(_)) {
            rows.extend(table_in_out_subtree(value, id, &fullkey, true)?);
        }
    }
    Ok(rows)
}

/// The columns of `json_each` and `json_tree`, then their hidden ones, by name.
pub const TABLE_IN_OUT_COLUMNS: [&str; 11] = [
    "key", "value", "type", "atom", "id", "parent", "fullkey", "path", "json", "root", "rowid",
];

/// What `__json_each` and `__json_tree` give: the rows of `json_each` and `json_tree`, as a list of structs.
pub fn table_in_out_type() -> LogicalType {
    let types = [
        LogicalType::VARCHAR,
        json_type(),
        LogicalType::VARCHAR,
        json_type(),
        LogicalType::UBIGINT,
        LogicalType::UBIGINT,
        LogicalType::VARCHAR,
        LogicalType::VARCHAR,
        json_type(),
        LogicalType::VARCHAR,
        LogicalType::BIGINT,
    ];
    let fields = TABLE_IN_OUT_COLUMNS
        .iter()
        .zip(types)
        .map(|(name, ty)| (name.to_string(), ty))
        .collect();
    LogicalType::list(LogicalType::struct_type(fields))
}

fn table_in_out_list(args: &DataChunk, result: &mut Vector, tree: bool) -> Result<()> {
    let ty = table_in_out_type();
    let child = ty.child_type().cloned().expect("a list");
    row_wise(args, result, |row| {
        let rows = table_in_out_rows(&row[0], row.get(1), tree)?;
        let mut items = Vec::with_capacity(rows.len());
        for values in rows {
            let fields = TABLE_IN_OUT_COLUMNS
                .iter()
                .zip(values)
                .map(|(name, value)| (name.to_string(), value))
                .collect();
            items.push(Value::struct_value(fields));
        }
        Value::list(child.clone(), items)
    })
}

fn json_each_rows(args: &DataChunk, result: &mut Vector) -> Result<()> {
    table_in_out_list(args, result, false)
}

fn json_tree_rows(args: &DataChunk, result: &mut Vector) -> Result<()> {
    table_in_out_list(args, result, true)
}

/// `CoordinateReferenceSystem::TryParsePROJJSON`: of a PROJJSON CRS, its identifier (the
/// `authority:code` of its `id`, else its `name`) and the JSON written out again.
fn projjson_crs(text: &str) -> Option<(String, String)> {
    let json = parse(text).ok()?;
    let Json::Object(root) = &json else { return None };
    fn member<'a>(object: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
        object.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    let Some(Json::Str(kind)) = member(root, "type") else {
        return None;
    };
    const TYPES: [&str; 9] = [
        "GeographicCRS",
        "GeodeticCRS",
        "ProjectedCRS",
        "CompoundCRS",
        "BoundCRS",
        "VerticalCRS",
        "EngineeringCRS",
        "TemporalCRS",
        "ParametricCRS",
    ];
    if !TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind)) {
        return None;
    }
    let target = if kind.eq_ignore_ascii_case("BoundCRS") {
        match member(root, "transformation") {
            Some(Json::Object(transformation)) => transformation,
            _ => return None,
        }
    } else {
        root
    };
    let mut identifier = match member(target, "name") {
        Some(Json::Str(name)) => name.clone(),
        _ => String::new(),
    };
    if let Some(Json::Object(id)) = member(target, "id")
        && let Some(Json::Str(authority)) = member(id, "authority")
    {
        identifier = authority.clone();
        match member(id, "code") {
            Some(Json::Int(code)) => identifier += &format!(":{code}"),
            Some(Json::UInt(code)) => identifier += &format!(":{code}"),
            Some(Json::Str(code)) => identifier += &format!(":{code}"),
            _ => {}
        }
    }
    Some((identifier, json.text()))
}

/// Lets a GEOMETRY's CRS be PROJJSON, which the types can't read themselves.
pub fn register_projjson_parser() {
    pintail_common::geometry_crs::set_projjson_parser(projjson_crs);
}
