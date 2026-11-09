// Ported from duckdb/extension/json @ rust-port-baseline
//
//! JSON: the `JSON` type, an alias of VARCHAR, and its functions. A value is parsed into [`Json`] per
//! row, as yyjson does in C++, with its flags: NaN, Infinity and trailing commas are read.

use super::cast::{CastMode, cast_vector};
use super::nested::{bound_function, row_wise};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::json_scan::{Formats, formatted_value};
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

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

/// A parsed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    UInt(u64),
    Int(i64),
    Real(f64),
    /// a number that is too big for the others, or text that is JSON already
    Raw(String),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Where and why text isn't JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
    pub message: &'static str,
}

/// How deep values can nest (`YYJSON_READER_MAX_DEPTH`).
const MAX_DEPTH: usize = 4096;

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn fail<T>(&self, message: &'static str) -> std::result::Result<T, ParseError> {
        Err(ParseError {
            position: self.at,
            message,
        })
    }

    fn skip_space(&mut self) {
        while matches!(self.text.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> std::result::Result<Json, ParseError> {
        if self.text[self.at..].len() >= word.len()
            && self.text[self.at..self.at + word.len()].eq_ignore_ascii_case(word.as_bytes())
        {
            self.at += word.len();
            return Ok(value);
        }
        self.fail("invalid literal")
    }

    fn value(&mut self) -> std::result::Result<Json, ParseError> {
        self.skip_space();
        let Some(&c) = self.text.get(self.at) else {
            return self.fail("unexpected end of data");
        };
        match c {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b'-' | b'0'..=b'9' => self.number(),
            b't' | b'T' => self.literal("true", Json::Bool(true)),
            b'f' | b'F' => self.literal("false", Json::Bool(false)),
            b'n' | b'N' => match self.text.get(self.at + 1) {
                Some(b'a' | b'A') => self.literal("nan", Json::Real(f64::NAN)),
                _ => self.literal("null", Json::Null),
            },
            b'I' | b'i' => match self.literal("infinity", Json::Real(f64::INFINITY)) {
                Ok(value) => Ok(value),
                Err(_) => self.literal("inf", Json::Real(f64::INFINITY)),
            },
            _ => self.fail("unexpected character"),
        }
    }

    fn descend(&mut self) -> std::result::Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fail("nesting too deep");
        }
        Ok(())
    }

    fn array(&mut self) -> std::result::Result<Json, ParseError> {
        self.descend()?;
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            match self.text.get(self.at) {
                Some(b']') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Json::Array(items));
                }
                None => return self.fail("unexpected end of data"),
                _ => {}
            }
            items.push(self.value()?);
            self.skip_space();
            match self.text.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b']') => {}
                None => return self.fail("unexpected end of data"),
                _ => return self.fail("unexpected character, expected a comma or a closing bracket"),
            }
        }
    }

    fn object(&mut self) -> std::result::Result<Json, ParseError> {
        self.descend()?;
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            match self.text.get(self.at) {
                Some(b'}') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Json::Object(items));
                }
                Some(b'"') => {}
                None => return self.fail("unexpected end of data"),
                _ => return self.fail("unexpected character, expected a string for object key"),
            }
            let key = self.string()?;
            self.skip_space();
            match self.text.get(self.at) {
                Some(b':') => self.at += 1,
                None => return self.fail("unexpected end of data"),
                _ => return self.fail("unexpected character, expected a colon after object key"),
            }
            let value = self.value()?;
            items.push((key, value));
            self.skip_space();
            match self.text.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {}
                None => return self.fail("unexpected end of data"),
                _ => return self.fail("unexpected character, expected a comma or a closing brace"),
            }
        }
    }

    fn hex4(&mut self) -> std::result::Result<u32, ParseError> {
        let digits = self.text.get(self.at..self.at + 4);
        let value = digits
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok());
        match value {
            Some(value) => {
                self.at += 4;
                Ok(value)
            }
            None => self.fail("invalid escaped sequence in string"),
        }
    }

    fn string(&mut self) -> std::result::Result<String, ParseError> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.text.get(self.at) else {
                return self.fail("unexpected end of data");
            };
            self.at += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(&escape) = self.text.get(self.at) else {
                        return self.fail("unexpected end of data");
                    };
                    self.at += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => out.push(escape),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xd800..0xdc00).contains(&code) {
                                if self.text.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return self.fail("no low surrogate in string");
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&low) {
                                    return self.fail("invalid low surrogate in string");
                                }
                                code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                            }
                            let Some(ch) = char::from_u32(code) else {
                                return self.fail("invalid high surrogate in string");
                            };
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        _ => return self.fail("invalid escaped character in string"),
                    }
                }
                0..=0x1f => return self.fail("unexpected control character in string"),
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| ParseError {
            position: self.at,
            message: "invalid utf-8 encoding in string",
        })
    }

    fn number(&mut self) -> std::result::Result<Json, ParseError> {
        let start = self.at;
        let negative = self.text[self.at] == b'-';
        if negative {
            self.at += 1;
            if matches!(self.text.get(self.at), Some(b'I' | b'i' | b'N' | b'n')) {
                return match self.value()? {
                    Json::Real(x) => Ok(Json::Real(-x)),
                    _ => self.fail("invalid number"),
                };
            }
        }
        let digits = self.at;
        while matches!(self.text.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if self.at == digits {
            return self.fail("no digit after minus sign");
        }
        if self.text[digits] == b'0' && self.at - digits > 1 {
            self.at = digits + 1;
            return self.fail("number with leading zero is not allowed");
        }
        let mut real = false;
        if self.text.get(self.at) == Some(&b'.') {
            real = true;
            self.at += 1;
            let fraction = self.at;
            while matches!(self.text.get(self.at), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == fraction {
                return self.fail("no digit after decimal point");
            }
        }
        if matches!(self.text.get(self.at), Some(b'e' | b'E')) {
            real = true;
            self.at += 1;
            if matches!(self.text.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            let exponent = self.at;
            while matches!(self.text.get(self.at), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == exponent {
                return self.fail("no digit after exponent sign");
            }
        }
        let text = std::str::from_utf8(&self.text[start..self.at]).unwrap_or_default();
        if !real {
            if negative {
                if let Ok(value) = text.parse::<i64>() {
                    return Ok(Json::Int(value));
                }
            } else if let Ok(value) = text.parse::<u64>() {
                return Ok(Json::UInt(value));
            }
        }
        Ok(Json::Real(text.parse().unwrap_or(f64::NAN)))
    }
}

/// `yyjson_read` of one value, with nothing after it but space.
pub fn parse(text: &str) -> std::result::Result<Json, ParseError> {
    let (value, end) = parse_prefix(text.as_bytes(), 0)?;
    let mut parser = Parser {
        text: text.as_bytes(),
        at: end,
        depth: 0,
    };
    parser.skip_space();
    if parser.at != text.len() {
        return parser.fail("unexpected content after document");
    }
    Ok(value)
}

/// One value from `start`, and where it ends.
pub fn parse_prefix(text: &[u8], start: usize) -> std::result::Result<(Json, usize), ParseError> {
    let mut parser = Parser {
        text,
        at: start,
        depth: 0,
    };
    let value = parser.value()?;
    Ok((value, parser.at))
}

/// `JSONCommon::FormatParseError`
pub fn parse_error_text(text: &str, error: &ParseError) -> String {
    let input: String = text.chars().take(50).collect();
    format!(
        "Malformed JSON at byte {} of input: {}.  Input: \"{input}\"",
        error.position, error.message
    )
}

fn parsed(text: &str) -> Result<Json> {
    parse(text).map_err(|e| invalid(parse_error_text(text, &e)))
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_real(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str(if value.is_sign_negative() { "-NaN" } else { "NaN" });
    } else if value.is_infinite() {
        out.push_str(if value < 0.0 { "-Infinity" } else { "Infinity" });
    } else {
        out.push_str(&format!("{value:?}"));
    }
}

impl Json {
    /// `yyjson_write`, without space.
    pub fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Json::UInt(value) => out.push_str(&value.to_string()),
            Json::Int(value) => out.push_str(&value.to_string()),
            Json::Real(value) => write_real(out, *value),
            Json::Raw(text) => out.push_str(text),
            Json::Str(text) => write_string(out, text),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(items) => {
                out.push('{');
                for (i, (key, value)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, key);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// `YYJSON_WRITE_PRETTY`
    fn pretty(&self, depth: usize, out: &mut String) {
        let pad = |out: &mut String, depth: usize| out.extend(std::iter::repeat_n("    ", depth));
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, depth + 1);
                    item.pretty(depth + 1, out);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Object(items) if !items.is_empty() => {
                out.push_str("{\n");
                for (i, (key, value)) in items.iter().enumerate() {
                    pad(out, depth + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.pretty(depth + 1, out);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
            other => other.write(out),
        }
    }

    /// What an error calls this value.
    pub fn kind_name(&self) -> &'static str {
        self.type_name()
    }

    /// `JSONCommon::ValTypeToString`
    fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "NULL",
            Json::Bool(_) => "BOOLEAN",
            Json::UInt(_) => "UBIGINT",
            Json::Int(_) => "BIGINT",
            Json::Real(_) | Json::Raw(_) => "DOUBLE",
            Json::Str(_) => "VARCHAR",
            Json::Array(_) => "ARRAY",
            Json::Object(_) => "OBJECT",
        }
    }
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
                let index = text[start..at].parse::<usize>().ok();
                let (Some(index), Some(b']')) = (index, bytes.get(at)) else {
                    return Err(path_error(text, at));
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

fn collect_recursive<'a>(value: &'a Json, rest: &[Step], out: &mut Vec<&'a Json>) {
    walk(value, rest, out);
    match value {
        Json::Array(items) => {
            for item in items {
                collect_recursive(item, rest, out);
            }
        }
        Json::Object(items) => {
            for (_, item) in items {
                collect_recursive(item, rest, out);
            }
        }
        _ => {}
    }
}

fn collect_key<'a>(value: &'a Json, key: &str, rest: &[Step], out: &mut Vec<&'a Json>) {
    if let Json::Object(items) = value
        && let Some((_, found)) = items.iter().find(|(k, _)| k == key)
    {
        walk(found, rest, out);
    }
    match value {
        Json::Array(items) => {
            for item in items {
                collect_key(item, key, rest, out);
            }
        }
        Json::Object(items) => {
            for (_, item) in items {
                collect_key(item, key, rest, out);
            }
        }
        _ => {}
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
        (LogicalTypeId::Union, ValueData::Struct(fields)) => {
            let tag = fields.first().and_then(Value::as_i128).unwrap_or(0) as usize;
            match fields.get(tag + 1) {
                Some(member) => value_to_json(member)?,
                None => Json::Null,
            }
        }
        _ => Json::Str(value.try_to_string()?),
    })
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
    if let Json::Str(text) = json
        && let Some(value) = formatted_value(text, ty, formats)
    {
        return Ok(value);
    }
    let transform = |json: &Json, ty: &LogicalType, strict: bool| transform_with(json, ty, strict, formats);
    let fail = |json: &Json| -> Result<Value> {
        if strict {
            Err(invalid(format!("Unable to cast '{}' to {ty}", json.text())))
        } else {
            Ok(Value::null(ty.clone()))
        }
    };
    if matches!(json, Json::Null) {
        return Ok(Value::null(ty.clone()));
    }
    if is_json_type(ty) {
        return Value::varchar(json.text()).reinterpret(ty);
    }
    match (ty.id(), json) {
        (LogicalTypeId::SqlNull, _) => Ok(Value::null(ty.clone())),
        (LogicalTypeId::Varchar, Json::Str(text)) => Ok(Value::varchar(text.as_str())),
        (LogicalTypeId::Varchar, other) => Ok(Value::varchar(other.text())),
        (LogicalTypeId::Struct, Json::Object(items)) => {
            let mut fields = Vec::with_capacity(ty.struct_children().len());
            for (name, child) in ty.struct_children() {
                let found = if formats.case_insensitive {
                    items.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))
                } else {
                    items.iter().find(|(k, _)| k == name)
                };
                match found {
                    Some((_, item)) => fields.push(transform(item, child, strict)?),
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
                return fail(json);
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
            for (key, item) in items {
                let key = cast_value(&Value::varchar(key.as_str()), &children[0].1)
                    .ok_or_else(|| invalid(format!("Failed to cast key {key} to {}", children[0].1)))?;
                let item = transform(item, &children[1].1, strict)?;
                values.push(Value::new(entry.clone(), ValueData::Struct(vec![key, item]))?);
            }
            Value::new(ty.clone(), ValueData::List(values))
        }
        (LogicalTypeId::Struct | LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map, _) => fail(json),
        (_, Json::Array(_) | Json::Object(_)) => fail(json),
        (_, Json::Bool(flag)) => match cast_value(&Value::boolean(*flag), ty) {
            Some(value) => Ok(value),
            None => fail(json),
        },
        (_, Json::Str(text)) => match cast_value(&Value::varchar(text.as_str()), ty) {
            Some(value) => Ok(value),
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
            let json = parsed(&text)?;
            transform(&json, &to, !matches!(mode, CastMode::Try))?
        };
        result.set_value(row, &converted)?;
    }
    Ok(())
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
                    element = element.merge(Structure::of(item));
                }
                Structure::List(Box::new(element))
            }
            Json::Object(items) => {
                let mut fields: Vec<(String, Structure)> = Vec::with_capacity(items.len());
                for (key, value) in items {
                    let structure = Structure::of(value);
                    match fields.iter_mut().find(|(k, _)| k == key) {
                        Some((_, existing)) => *existing = existing.clone().merge(structure),
                        None => fields.push((key.clone(), structure)),
                    }
                }
                Structure::Object(fields)
            }
        }
    }

    pub fn merge(self, other: Structure) -> Structure {
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
            (Structure::List(a), Structure::List(b)) => Structure::List(Box::new(a.merge(*b))),
            (Structure::Object(mut a), Structure::Object(b)) => {
                for (key, value) in b {
                    match a.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, existing)) => *existing = existing.clone().merge(value),
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
            Structure::Object(fields) => Json::Object(fields.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
            Structure::Mixed => Json::Str("JSON".to_string()),
        }
    }

    /// `JSONStructure::StructureToType`
    pub fn logical_type(&self, depth: i64) -> LogicalType {
        if depth == 0 {
            return json_type();
        }
        match self {
            Structure::Null | Structure::Mixed => json_type(),
            Structure::Scalar("BOOLEAN") => LogicalType::BOOLEAN,
            // `refine_type` makes those that don't fit a HUGEINT
            Structure::Scalar("UBIGINT" | "BIGINT") => LogicalType::BIGINT,
            Structure::Scalar("DOUBLE") => LogicalType::DOUBLE,
            Structure::Scalar(_) => LogicalType::VARCHAR,
            Structure::List(element) => LogicalType::list(element.logical_type(depth - 1)),
            Structure::Object(fields) if fields.is_empty() => json_type(),
            Structure::Object(fields) => LogicalType::struct_type(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.logical_type(depth - 1)))
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
            _ => Err(binder_error(
                "Too many items in JSON structure: arrays must have exactly one item",
            )),
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
        let text = value.try_to_string()?;
        return Ok(parse(&text).unwrap_or(Json::Str(text)));
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
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&ty));
        };
        if row.len() == 1 {
            return f(Some(&json));
        }
        if row[1].is_null() {
            return Ok(nothing(&ty));
        }
        map_paths(&row[1], |path| each_found(&json, path, &ty, f), &ty)
    })
}

/// What one path gives: a list of all it finds, if it can find several.
fn each_found(json: &Json, path: &Path, ty: &LogicalType, f: impl Fn(Option<&Json>) -> Result<Value>) -> Result<Value> {
    if !path.is_wildcard() {
        return f(single(json, path));
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
        let (Some(json), false) = (document(&row[0])?, row[1].is_null()) else {
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

/// `yyjson_merge_patch`: what the patch doesn't name, then what it does.
fn merge_patch(target: Json, patch: &Json) -> Json {
    let Json::Object(wanted) = patch else {
        return patch.clone();
    };
    let mut existing = match target {
        Json::Object(items) => items,
        _ => Vec::new(),
    };
    let mut merged = Vec::with_capacity(existing.len() + wanted.len());
    let named = |key: &str| wanted.iter().any(|(k, _)| k == key);
    let mut patched = Vec::new();
    for (key, value) in existing.drain(..) {
        if named(&key) {
            patched.push((key, value));
        } else {
            merged.push((key, value));
        }
    }
    for (key, value) in wanted {
        if matches!(value, Json::Null) {
            continue;
        }
        let old = patched.iter().position(|(k, _)| k == key).map(|i| patched.remove(i).1);
        merged.push((key.clone(), merge_patch(old.unwrap_or(Json::Null), value)));
    }
    Json::Object(merged)
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

/// `DeepMerge`: a merge where a NULL in the patch keeps what was there, and objects merge all the way down.
fn deep_merge(original: Json, patch: Json) -> Json {
    let (Json::Object(original), Json::Object(patch)) = (original.clone(), &patch) else {
        return if matches!(patch, Json::Null) { original } else { patch };
    };
    let find = |items: &[(String, Json)], key: &str| items.iter().position(|(k, _)| k == key);
    let mut merged = Vec::with_capacity(original.len() + patch.len());
    for (key, value) in &original {
        match find(patch, key) {
            Some(at) if !matches!(patch[at].1, Json::Null) => {}
            _ => merged.push((key.clone(), value.clone())),
        }
    }
    for (key, value) in patch {
        if matches!(value, Json::Null) {
            continue;
        }
        let old = find(&original, key).map(|at| original[at].1.clone());
        let value = match old {
            Some(old @ Json::Object(_)) if matches!(value, Json::Object(_)) => deep_merge(old, value.clone()),
            _ => value.clone(),
        };
        merged.push((key.clone(), value));
    }
    Json::Object(merged)
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

/// `ComputeDiff`: the merge patch that makes `old` into `new`, both objects; `None` if they are the same.
fn object_diff(old: &[(String, Json)], new: &[(String, Json)]) -> Option<Json> {
    let mut diff = Vec::new();
    for (key, _) in old {
        if !new.iter().any(|(k, _)| k == key) {
            diff.push((key.clone(), Json::Null));
        }
    }
    for (key, value) in new {
        let before = old.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        match (before, value) {
            (Some(Json::Object(before)), Json::Object(after)) => {
                if let Some(child) = object_diff(before, after) {
                    diff.push((key.clone(), child));
                }
            }
            (Some(before), after) if before == after => {}
            _ => diff.push((key.clone(), value.clone())),
        }
    }
    (!diff.is_empty()).then_some(Json::Object(diff))
}

fn json_merge_patch_diff(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(new) = document(&row[1])? else {
            return Ok(nothing(&json_type()));
        };
        let diff = match (document(&row[0])?, &new) {
            (Some(Json::Object(old)), Json::Object(after)) => {
                object_diff(&old, after).unwrap_or(Json::Object(Vec::new()))
            }
            _ => new,
        };
        Value::varchar(diff.text()).reinterpret(&json_type())
    })
}

/// `SortKeys`: the keys of every object in byte order.
fn sort_keys(json: Json) -> Json {
    match json {
        Json::Object(items) => {
            let mut items: Vec<(String, Json)> =
                items.into_iter().map(|(key, value)| (key, sort_keys(value))).collect();
            items.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
            Json::Object(items)
        }
        Json::Array(items) => Json::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
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
    match json {
        Json::Object(items) => Json::Object(
            items
                .into_iter()
                .filter(|(_, value)| !matches!(value, Json::Null))
                .map(|(key, value)| (key, strip_nulls(value)))
                .collect(),
        ),
        Json::Array(items) => Json::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
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

fn set_path(json: &mut Json, steps: &[Step], value: &Json, modify: Modify) {
    let Some((step, rest)) = steps.split_first() else {
        if modify.replace {
            *json = value.clone();
        }
        return;
    };
    match (json, step) {
        (Json::Object(items), Step::Key(key)) => match items.iter().position(|(k, _)| k == key) {
            Some(position) => set_path(&mut items[position].1, rest, value, modify),
            None if modify.insert => {
                // the objects on the way to it come into being
                let mut inner = value.clone();
                for step in rest.iter().rev() {
                    let Step::Key(key) = step else {
                        return;
                    };
                    inner = Json::Object(vec![(key.clone(), inner)]);
                }
                items.push((key.clone(), inner));
            }
            None => {}
        },
        (Json::Array(items), Step::Append) if rest.is_empty() && modify.insert => items.push(value.clone()),
        (Json::Array(items), Step::Index(_) | Step::ReverseIndex(_)) => {
            let position = match step {
                Step::Index(index) => Some(*index),
                Step::ReverseIndex(index) => items.len().checked_sub(*index),
                _ => None,
            };
            match position {
                Some(position) if position < items.len() => set_path(&mut items[position], rest, value, modify),
                Some(_) if rest.is_empty() && modify.insert => items.push(value.clone()),
                _ => {}
            }
        }
        _ => {}
    }
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
    row_wise(args, result, |row| {
        let Some(json) = document(&row[0])? else {
            return Ok(nothing(&ty));
        };
        transform(&json, &ty, strict)
    })
}

fn json_transform(args: &DataChunk, result: &mut Vector) -> Result<()> {
    transform_rows(args, result, false)
}

fn json_transform_strict(args: &DataChunk, result: &mut Vector) -> Result<()> {
    transform_rows(args, result, true)
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
            parse_path(&path_text(item)?).map_err(|e| binder_error(e.message))?;
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
            if path_argument(&children[1])?.is_some_and(|(_, wildcard)| wildcard) {
                let ty = LogicalType::list(json_type());
                return Ok(bound_function(&lower, children, ty, json_extract_wildcard));
            }
            returning_paths(&lower, children, json_type(), json_extract)
        }
        "json_extract_string" | "json_extract_path_text" | "->>" => {
            expect_args(&lower, &children, 2, 2)?;
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
                .any(|key| !matches!(key.return_type.id(), LogicalTypeId::Varchar | LogicalTypeId::SqlNull))
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
            if structure.is_null() {
                return Err(binder_error("JSON structure must not be NULL!"));
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
