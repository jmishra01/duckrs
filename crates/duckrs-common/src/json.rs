// Ported from duckdb/extension/json (yyjson's reader and writer) @ rust-port-baseline
//
//! A parsed JSON value, as yyjson reads and writes it: NaN, Infinity and trailing commas are read.

/// A parsed value.
#[derive(Debug)]
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

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    /// `YYJSON_READ_NUMBER_AS_RAW`: every number kept as its text
    numbers_as_raw: bool,
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

    /// A value that is not an array or an object.
    fn scalar(&mut self, c: u8) -> std::result::Result<Json, ParseError> {
        match c {
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

    /// A value, read without recursion as yyjson reads one: the open arrays and objects are a
    /// stack, so that no nesting is too deep.
    fn value(&mut self) -> std::result::Result<Json, ParseError> {
        enum Open {
            Array(Vec<Json>),
            Object(Vec<(String, Json)>, String),
        }
        let mut stack: Vec<Open> = Vec::new();
        'value: loop {
            // the start of a value
            self.skip_space();
            let Some(&c) = self.text.get(self.at) else {
                return self.fail("unexpected end of data");
            };
            let mut value = match c {
                b'[' => {
                    self.at += 1;
                    self.skip_space();
                    match self.text.get(self.at) {
                        Some(b']') => {
                            self.at += 1;
                            Json::Array(Vec::new())
                        }
                        None => return self.fail("unexpected end of data"),
                        _ => {
                            stack.push(Open::Array(Vec::new()));
                            continue 'value;
                        }
                    }
                }
                b'{' => {
                    self.at += 1;
                    match self.object_key()? {
                        None => Json::Object(Vec::new()),
                        Some(key) => {
                            stack.push(Open::Object(Vec::new(), key));
                            continue 'value;
                        }
                    }
                }
                c => self.scalar(c)?,
            };
            // the end of a value: it goes into what is open, which may end with it
            loop {
                let Some(open) = stack.last_mut() else {
                    return Ok(value);
                };
                self.skip_space();
                match open {
                    Open::Array(items) => {
                        items.push(value);
                        match self.text.get(self.at) {
                            Some(b',') => {
                                self.at += 1;
                                self.skip_space();
                                if self.text.get(self.at) != Some(&b']') {
                                    continue 'value;
                                }
                                self.at += 1;
                            }
                            Some(b']') => self.at += 1,
                            None => return self.fail("unexpected end of data"),
                            _ => return self.fail("unexpected character, expected a comma or a closing bracket"),
                        }
                    }
                    Open::Object(items, key) => {
                        items.push((std::mem::take(key), value));
                        match self.text.get(self.at) {
                            Some(b',') => {
                                self.at += 1;
                                if let Some(next) = self.object_key()? {
                                    *key = next;
                                    continue 'value;
                                }
                            }
                            Some(b'}') => self.at += 1,
                            None => return self.fail("unexpected end of data"),
                            _ => return self.fail("unexpected character, expected a comma or a closing brace"),
                        }
                    }
                }
                value = match stack.pop().expect("open") {
                    Open::Array(items) => Json::Array(items),
                    Open::Object(items, _) => Json::Object(items),
                };
            }
        }
    }

    /// After `{` or `,` in an object: the next key and its colon, or `None` at the closing brace.
    fn object_key(&mut self) -> std::result::Result<Option<String>, ParseError> {
        self.skip_space();
        match self.text.get(self.at) {
            Some(b'}') => {
                self.at += 1;
                return Ok(None);
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
        Ok(Some(key))
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
                let c = self.text[self.at];
                return match self.scalar(c)? {
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
        if self.numbers_as_raw {
            return Ok(Json::Raw(text.to_string()));
        }
        // `YYJSON_READ_BIGNUM_AS_RAW`: an integer too big for 64 bits, or a real too big for a
        // finite double, is kept as its text
        if !real {
            if negative {
                if let Ok(value) = text.parse::<i64>() {
                    return Ok(Json::Int(value));
                }
            } else if let Ok(value) = text.parse::<u64>() {
                return Ok(Json::UInt(value));
            }
            return Ok(Json::Raw(text.to_string()));
        }
        match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(Json::Real(value)),
            _ => Ok(Json::Raw(text.to_string())),
        }
    }
}

/// `yyjson_read` of one value, with nothing after it but space.
pub fn parse(text: &str) -> std::result::Result<Json, ParseError> {
    parse_with(text, false)
}

/// `parse`, with `numbers_as_raw` every number kept as its text.
pub fn parse_with(text: &str, numbers_as_raw: bool) -> std::result::Result<Json, ParseError> {
    let (value, end) = parse_prefix_with(text.as_bytes(), 0, numbers_as_raw)?;
    let mut parser = Parser {
        text: text.as_bytes(),
        at: end,
        numbers_as_raw: false,
    };
    parser.skip_space();
    if parser.at != text.len() {
        return parser.fail("unexpected content after document");
    }
    Ok(value)
}

/// One value from `start`, and where it ends.
pub fn parse_prefix(text: &[u8], start: usize) -> std::result::Result<(Json, usize), ParseError> {
    parse_prefix_with(text, start, false)
}

/// `parse_prefix`, with `numbers_as_raw` every number kept as its text (`YYJSON_READ_NUMBER_AS_RAW`,
/// what a DECIMAL needs).
pub fn parse_prefix_with(
    text: &[u8],
    start: usize,
    numbers_as_raw: bool,
) -> std::result::Result<(Json, usize), ParseError> {
    let mut parser = Parser {
        text,
        at: start,
        numbers_as_raw,
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
    /// A value that is not an array or an object, written.
    fn write_scalar(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Json::UInt(value) => out.push_str(&value.to_string()),
            Json::Int(value) => out.push_str(&value.to_string()),
            Json::Real(value) => write_real(out, *value),
            Json::Raw(text) => out.push_str(text),
            Json::Str(text) => write_string(out, text),
            Json::Array(_) | Json::Object(_) => unreachable!("containers are written by `write_with`"),
        }
    }

    /// `yyjson_write`, with `pretty` the indentation of `YYJSON_WRITE_PRETTY` from `depth`: without
    /// recursion, as yyjson writes, so that no nesting is too deep.
    fn write_with(&self, pretty: Option<usize>, out: &mut String) {
        enum Task<'a> {
            Value(&'a Json, usize),
            Key(&'a str),
            Text(&'static str),
            Pad(usize),
        }
        let pad = |out: &mut String, depth: usize| out.extend(std::iter::repeat_n("    ", depth));
        let mut tasks = vec![Task::Value(self, pretty.unwrap_or(0))];
        while let Some(task) = tasks.pop() {
            match task {
                Task::Text(text) => out.push_str(text),
                Task::Pad(depth) => pad(out, depth),
                Task::Key(key) => {
                    write_string(out, key);
                    out.push_str(if pretty.is_some() { ": " } else { ":" });
                }
                Task::Value(Json::Array(items), depth) if !items.is_empty() => {
                    out.push_str(if pretty.is_some() { "[\n" } else { "[" });
                    if pretty.is_some() {
                        tasks.push(Task::Text("]"));
                        tasks.push(Task::Pad(depth));
                    } else {
                        tasks.push(Task::Text("]"));
                    }
                    for (i, item) in items.iter().enumerate().rev() {
                        if pretty.is_some() {
                            tasks.push(Task::Text(if i + 1 < items.len() { ",\n" } else { "\n" }));
                            tasks.push(Task::Value(item, depth + 1));
                            tasks.push(Task::Pad(depth + 1));
                        } else {
                            tasks.push(Task::Value(item, depth));
                            if i > 0 {
                                tasks.push(Task::Text(","));
                            }
                        }
                    }
                }
                Task::Value(Json::Object(items), depth) if !items.is_empty() => {
                    out.push_str(if pretty.is_some() { "{\n" } else { "{" });
                    if pretty.is_some() {
                        tasks.push(Task::Text("}"));
                        tasks.push(Task::Pad(depth));
                    } else {
                        tasks.push(Task::Text("}"));
                    }
                    for (i, (key, value)) in items.iter().enumerate().rev() {
                        if pretty.is_some() {
                            tasks.push(Task::Text(if i + 1 < items.len() { ",\n" } else { "\n" }));
                            tasks.push(Task::Value(value, depth + 1));
                            tasks.push(Task::Key(key));
                            tasks.push(Task::Pad(depth + 1));
                        } else {
                            tasks.push(Task::Value(value, depth));
                            tasks.push(Task::Key(key));
                            if i > 0 {
                                tasks.push(Task::Text(","));
                            }
                        }
                    }
                }
                Task::Value(Json::Array(_), _) => out.push_str("[]"),
                Task::Value(Json::Object(_), _) => out.push_str("{}"),
                Task::Value(scalar, _) => scalar.write_scalar(out),
            }
        }
    }

    /// The items of an array, or the value itself if it is none.
    pub fn into_array(mut self) -> std::result::Result<Vec<Json>, Json> {
        match &mut self {
            Json::Array(items) => Ok(std::mem::take(items)),
            _ => Err(self),
        }
    }

    /// The members of an object, or the value itself if it is none.
    pub fn into_object(mut self) -> std::result::Result<Vec<(String, Json)>, Json> {
        match &mut self {
            Json::Object(items) => Ok(std::mem::take(items)),
            _ => Err(self),
        }
    }

    /// `yyjson_write`, without space.
    pub fn write(&self, out: &mut String) {
        self.write_with(None, out);
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// `YYJSON_WRITE_PRETTY`
    pub fn pretty(&self, depth: usize, out: &mut String) {
        self.write_with(Some(depth), out);
    }

    /// `self` with every array and object made anew by `f` once what is in it was: bottom up and
    /// without recursion.
    pub fn map_containers(self, mut f: impl FnMut(Json) -> Json) -> Json {
        enum Frame {
            Array(std::vec::IntoIter<Json>, Vec<Json>),
            Object(std::vec::IntoIter<(String, Json)>, Vec<(String, Json)>, String),
        }
        fn open(mut value: Json, stack: &mut Vec<Frame>) -> Option<Json> {
            match &mut value {
                Json::Array(items) => {
                    let items = std::mem::take(items);
                    let len = items.len();
                    stack.push(Frame::Array(items.into_iter(), Vec::with_capacity(len)));
                    None
                }
                Json::Object(items) => {
                    let items = std::mem::take(items);
                    let len = items.len();
                    stack.push(Frame::Object(items.into_iter(), Vec::with_capacity(len), String::new()));
                    None
                }
                _ => Some(value),
            }
        }
        let mut stack = Vec::new();
        let mut done = open(self, &mut stack);
        loop {
            let Some(frame) = stack.last_mut() else {
                return done.expect("a value");
            };
            if let Some(value) = done.take() {
                match frame {
                    Frame::Array(_, built) => built.push(value),
                    Frame::Object(_, built, key) => built.push((std::mem::take(key), value)),
                }
            }
            let next = match frame {
                Frame::Array(items, _) => items.next(),
                Frame::Object(items, _, key) => items.next().map(|(k, value)| {
                    *key = k;
                    value
                }),
            };
            match next {
                Some(value) => done = open(value, &mut stack),
                None => {
                    let finished = match stack.pop().expect("open") {
                        Frame::Array(_, built) => Json::Array(built),
                        Frame::Object(_, built, _) => Json::Object(built),
                    };
                    done = Some(f(finished));
                }
            }
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

/// Dropping a document takes its values apart one at a time: yyjson's documents are flat, and one
/// nested 200000 deep would overflow the stack of a recursive drop.
impl Drop for Json {
    fn drop(&mut self) {
        fn take(value: &mut Json, stack: &mut Vec<Json>) {
            match value {
                Json::Array(items) => stack.append(items),
                Json::Object(items) => stack.extend(items.drain(..).map(|(_, value)| value)),
                _ => {}
            }
        }
        if !matches!(self, Json::Array(_) | Json::Object(_)) {
            return;
        }
        let mut stack = Vec::new();
        take(self, &mut stack);
        while let Some(mut value) = stack.pop() {
            take(&mut value, &mut stack);
        }
    }
}

/// A copy made without recursion.
impl Clone for Json {
    fn clone(&self) -> Self {
        enum Frame<'a> {
            Array(std::slice::Iter<'a, Json>, Vec<Json>),
            Object(std::slice::Iter<'a, (String, Json)>, Vec<(String, Json)>, String),
        }
        // a copy of a value that is not a container, or its frame
        fn open<'a>(value: &'a Json, stack: &mut Vec<Frame<'a>>) -> Option<Json> {
            Some(match value {
                Json::Null => Json::Null,
                Json::Bool(flag) => Json::Bool(*flag),
                Json::UInt(x) => Json::UInt(*x),
                Json::Int(x) => Json::Int(*x),
                Json::Real(x) => Json::Real(*x),
                Json::Raw(text) => Json::Raw(text.clone()),
                Json::Str(text) => Json::Str(text.clone()),
                Json::Array(items) => {
                    stack.push(Frame::Array(items.iter(), Vec::with_capacity(items.len())));
                    return None;
                }
                Json::Object(items) => {
                    stack.push(Frame::Object(
                        items.iter(),
                        Vec::with_capacity(items.len()),
                        String::new(),
                    ));
                    return None;
                }
            })
        }
        let mut stack = Vec::new();
        let mut done = open(self, &mut stack);
        loop {
            let Some(frame) = stack.last_mut() else {
                return done.expect("a value");
            };
            if let Some(value) = done.take() {
                match frame {
                    Frame::Array(_, built) => built.push(value),
                    Frame::Object(_, built, key) => built.push((std::mem::take(key), value)),
                }
            }
            let next = match frame {
                Frame::Array(items, _) => items.next(),
                Frame::Object(items, _, key) => items.next().map(|(k, value)| {
                    key.clone_from(k);
                    value
                }),
            };
            match next {
                Some(value) => done = open(value, &mut stack),
                None => {
                    done = Some(match stack.pop().expect("open") {
                        Frame::Array(_, built) => Json::Array(built),
                        Frame::Object(_, built, _) => Json::Object(built),
                    });
                }
            }
        }
    }
}

/// Equality without recursion.
impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        let mut stack = vec![(self, other)];
        while let Some((a, b)) = stack.pop() {
            match (a, b) {
                (Json::Null, Json::Null) => {}
                (Json::Bool(x), Json::Bool(y)) if x == y => {}
                (Json::UInt(x), Json::UInt(y)) if x == y => {}
                (Json::Int(x), Json::Int(y)) if x == y => {}
                (Json::Real(x), Json::Real(y)) if x == y => {}
                (Json::Raw(x), Json::Raw(y)) | (Json::Str(x), Json::Str(y)) if x == y => {}
                (Json::Array(x), Json::Array(y)) if x.len() == y.len() => stack.extend(x.iter().zip(y)),
                (Json::Object(x), Json::Object(y)) if x.len() == y.len() => {
                    for ((kx, vx), (ky, vy)) in x.iter().zip(y) {
                        if kx != ky {
                            return false;
                        }
                        stack.push((vx, vy));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}
