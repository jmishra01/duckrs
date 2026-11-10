// Ported from duckdb/extension/json (yyjson's reader and writer) @ rust-port-baseline
//
//! A parsed JSON value, as yyjson reads and writes it: NaN, Infinity and trailing commas are read.

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

pub fn write_string(out: &mut String, text: &str) {
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

pub fn write_real(out: &mut String, value: f64) {
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
    pub fn pretty(&self, depth: usize, out: &mut String) {
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
    pub fn type_name(&self) -> &'static str {
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
