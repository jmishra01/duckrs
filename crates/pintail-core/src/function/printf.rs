// Ported from extension/core_functions/scalar/string/printf.cpp and
// third_party/fmt/include/fmt/{format.h,printf.h} (fmt 6.1.2 as DuckDB bundles it) @ rust-port-baseline

//! `format` and `printf`, which are `duckdb_fmt::vformat` and `duckdb_fmt::vsprintf`.
//!
//! As in fmt, a format string is walked byte by byte, so a fill must be a single byte, and `%c`
//! writes one byte, which can leave the result without valid UTF-8.

use super::ScalarCallback;
use super::bit_functions::strict_rows;
use pintail_common::string_cast::{FloatFormat, FloatSpec, format_hex_float, format_with_spec};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};

fn invalid_input(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// The callback of a format function.
pub fn format_callback(name: &str) -> ScalarCallback {
    if name.eq_ignore_ascii_case("printf") {
        printf_function
    } else {
        format_function
    }
}

fn printf_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    run(args, result, printf)
}

fn format_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    run(args, result, format)
}

/// `PrintfFunction`.
fn run(args: &DataChunk, result: &mut Vector, function: fn(&[u8], &[Arg]) -> Result<Vec<u8>>) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::Bytes(bytes) = row[0].data() else {
            unreachable!("the format string is cast to VARCHAR");
        };
        // fmt gets a C string.
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let format_string = &bytes[..end];
        let args = row[1..].iter().map(Arg::of).collect::<Result<Vec<_>>>()?;
        let formatted = function(format_string, &args)?;
        match String::from_utf8(formatted) {
            Ok(text) => Ok(Value::varchar(text)),
            Err(_) => Err(invalid_input(format!(
                "Invalid UTF8 produced by format string \"{}\" - note that %c writes a single byte, use \
                 chr(...) to write a Unicode code point",
                String::from_utf8_lossy(format_string)
            ))),
        }
    })
}

/// `basic_format_arg`: the argument types that `PrintfResolveTypes` leaves.
#[derive(Clone)]
enum Arg {
    Bool(bool),
    Int(i64),
    UInt(u64),
    HugeInt(i128),
    UHugeInt(u128),
    Double(f64),
    Text(Vec<u8>),
    /// What `%c` makes of an integer.
    Char(u8),
}

impl Arg {
    fn of(value: &Value) -> Result<Self> {
        Ok(match value.data() {
            ValueData::Bool(v) => Self::Bool(*v),
            ValueData::Int128(v) => Self::HugeInt(*v),
            ValueData::UInt128(v) => Self::UHugeInt(*v),
            ValueData::UInt64(v) => Self::UInt(*v),
            ValueData::Double(v) => Self::Double(*v),
            ValueData::Float(v) => Self::Double(f64::from(*v)),
            _ => match value.as_i128() {
                Some(v) => Self::Int(v as i64),
                None => Self::Text(value.try_to_string()?.into_bytes()),
            },
        })
    }

    fn is_integral(&self) -> bool {
        !matches!(self, Self::Double(_) | Self::Text(_))
    }

    fn is_arithmetic(&self) -> bool {
        !matches!(self, Self::Text(_))
    }

    /// `is_zero_int`.
    fn is_zero_int(&self) -> bool {
        match self {
            Self::Bool(v) => !*v,
            Self::Int(v) => *v == 0,
            Self::UInt(v) => *v == 0,
            Self::HugeInt(v) => *v == 0,
            Self::UHugeInt(v) => *v == 0,
            Self::Char(v) => *v == 0,
            Self::Double(_) | Self::Text(_) => false,
        }
    }

    /// The value's two's complement bits.
    fn bits(&self) -> u128 {
        match self {
            Self::Bool(v) => u128::from(*v),
            Self::Int(v) => *v as i128 as u128,
            Self::UInt(v) => u128::from(*v),
            Self::HugeInt(v) => *v as u128,
            Self::UHugeInt(v) => *v,
            Self::Char(v) => u128::from(*v),
            Self::Double(_) | Self::Text(_) => 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Right,
    Center,
    Numeric,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sign {
    None,
    Minus,
    Plus,
    Space,
}

/// `basic_format_specs`.
#[derive(Clone, Copy)]
struct Specs {
    width: usize,
    /// `-1` without one.
    precision: i32,
    /// `0` without one.
    kind: u8,
    align: Align,
    sign: Sign,
    alt: bool,
    fill: u8,
    /// `0` without one.
    thousands: u8,
}

impl Default for Specs {
    fn default() -> Self {
        Self {
            width: 0,
            precision: -1,
            kind: 0,
            align: Align::None,
            sign: Sign::None,
            alt: false,
            fill: b' ',
            thousands: 0,
        }
    }
}

fn invalid_type_specifier(kind: u8, type_name: &str) -> Error {
    invalid_input(format!(
        "Invalid type specifier \"{}\" for formatting a value of type {type_name}",
        String::from_utf8_lossy(&[kind])
    ))
}

/// `basic_writer::write_padded`. `width` counts code points.
fn write_padded(out: &mut Vec<u8>, specs: &Specs, body: &[u8], width: usize) {
    let padding = specs.width.saturating_sub(width);
    let (left, right) = match specs.align {
        Align::Right => (padding, 0),
        Align::Center => (padding / 2, padding - padding / 2),
        _ => (0, padding),
    };
    out.extend(std::iter::repeat_n(specs.fill, left));
    out.extend_from_slice(body);
    out.extend(std::iter::repeat_n(specs.fill, right));
}

/// `basic_writer::write` for a string, with the precision in code points.
fn write_string(out: &mut Vec<u8>, specs: &Specs, text: &[u8]) {
    let starts = || {
        text.iter()
            .enumerate()
            .filter(|(_, b)| **b & 0xC0 != 0x80)
            .map(|(at, _)| at)
    };
    let text = match usize::try_from(specs.precision) {
        Ok(precision) => starts().nth(precision).map_or(text, |end| &text[..end]),
        Err(_) => text,
    };
    let width = text.iter().filter(|b| **b & 0xC0 != 0x80).count();
    write_padded(out, specs, text, width);
}

/// `basic_writer::write_int`: `<left-padding><prefix><numeric-padding><digits><right-padding>`.
fn write_int(out: &mut Vec<u8>, specs: &Specs, prefix: &[u8], digits: &[u8]) {
    let mut size = prefix.len() + digits.len();
    let mut fill = specs.fill;
    let mut padding = 0;
    if specs.align == Align::Numeric {
        if specs.width > size {
            padding = specs.width - size;
            size = specs.width;
        }
    } else if specs.precision > digits.len() as i32 {
        padding = specs.precision as usize - digits.len();
        size = prefix.len() + specs.precision as usize;
        fill = b'0';
    }
    let mut body = Vec::with_capacity(size);
    body.extend_from_slice(prefix);
    body.extend(std::iter::repeat_n(fill, padding));
    body.extend_from_slice(digits);
    let mut specs = *specs;
    if specs.align == Align::None {
        specs.align = Align::Right;
    }
    write_padded(out, &specs, &body, size);
}

/// `int_writer`.
fn write_integer(out: &mut Vec<u8>, specs: &Specs, negative: bool, abs_value: u128) -> Result<()> {
    let mut prefix = Vec::with_capacity(3);
    if negative {
        prefix.push(b'-');
    } else if specs.sign == Sign::Plus {
        prefix.push(b'+');
    } else if specs.sign == Sign::Space {
        prefix.push(b' ');
    }
    let decimal = || abs_value.to_string().into_bytes();
    let grouped = || {
        let digits = decimal();
        let mut grouped = Vec::with_capacity(digits.len() * 4 / 3);
        for (at, digit) in digits.iter().enumerate() {
            if at != 0 && (digits.len() - at).is_multiple_of(3) {
                grouped.push(specs.thousands);
            }
            grouped.push(*digit);
        }
        grouped
    };
    if specs.thousands != 0 {
        write_int(out, specs, &prefix, &grouped());
        return Ok(());
    }
    match specs.kind {
        0 | b'd' | b'n' | b'l' | b'L' => write_int(out, specs, &prefix, &decimal()),
        b'x' | b'X' => {
            if specs.alt {
                prefix.extend_from_slice(&[b'0', specs.kind]);
            }
            let digits = if specs.kind == b'x' {
                format!("{abs_value:x}")
            } else {
                format!("{abs_value:X}")
            };
            write_int(out, specs, &prefix, digits.as_bytes());
        }
        b'b' | b'B' => {
            if specs.alt {
                prefix.extend_from_slice(&[b'0', specs.kind]);
            }
            write_int(out, specs, &prefix, format!("{abs_value:b}").as_bytes());
        }
        b'o' => {
            let digits = format!("{abs_value:o}");
            // The prefix counts as a digit, so it only goes where the precision leaves room.
            if specs.alt && specs.precision <= digits.len() as i32 && abs_value != 0 {
                prefix.push(b'0');
            }
            write_int(out, specs, &prefix, digits.as_bytes());
        }
        other => return Err(invalid_type_specifier(other, "int")),
    }
    Ok(())
}

/// `basic_writer::write` for a double.
fn write_double(out: &mut Vec<u8>, specs: &Specs, value: f64) -> Result<()> {
    let mut specs = *specs;
    // `parse_float_type_spec`
    let mut locale = false;
    let (format, upper, keeps_zeros) = match specs.kind {
        0 => (Some(FloatFormat::General), false, true),
        b'g' => (Some(FloatFormat::General), false, false),
        b'G' => (Some(FloatFormat::General), true, false),
        b'e' => (Some(FloatFormat::Exp), false, true),
        b'E' => (Some(FloatFormat::Exp), true, true),
        b'f' => (Some(FloatFormat::Fixed), false, true),
        b'F' => (Some(FloatFormat::Fixed), true, true),
        b'a' => (None, false, false),
        b'A' => (None, true, false),
        b'n' | b'l' | b'L' => {
            locale = true;
            (Some(FloatFormat::General), false, false)
        }
        other => return Err(invalid_type_specifier(other, "float")),
    };
    let trailing_zeros = specs.alt || (keeps_zeros && specs.precision != 0);
    let mut sign: &[u8] = if value.is_sign_negative() {
        b"-"
    } else if specs.sign == Sign::Plus {
        b"+"
    } else if specs.sign == Sign::Space {
        b" "
    } else {
        b""
    };
    let abs_value = value.abs();

    if !value.is_finite() {
        let name = match (value.is_nan(), upper) {
            (true, false) => "nan",
            (true, true) => "NAN",
            (false, false) => "inf",
            (false, true) => "INF",
        };
        let body = [sign, name.as_bytes()].concat();
        write_padded(out, &specs, &body, body.len());
        return Ok(());
    }

    if specs.align == Align::None {
        specs.align = Align::Right;
    } else if specs.align == Align::Numeric {
        if !sign.is_empty() {
            out.extend_from_slice(sign);
            sign = b"";
            specs.width = specs.width.saturating_sub(1);
        }
        specs.align = Align::Right;
    }

    let precision = usize::try_from(specs.precision).ok();
    let digits = match format {
        None => format_hex_float(abs_value, precision, upper, specs.alt),
        Some(format) => {
            let precision = precision.or((specs.kind != 0).then_some(6));
            // DuckDB: formatting is not interruptible and is linear in the precision.
            if let Some(precision) = precision.filter(|p| *p > 1_000_000) {
                return Err(invalid_input(format!(
                    "Precision of {precision} is too large, the maximum is 1000000"
                )));
            }
            format_with_spec(
                abs_value,
                FloatSpec {
                    format,
                    upper,
                    trailing_zeros,
                    precision,
                    thousand_sep: specs.thousands,
                    // With a point for thousands, the decimal point turns into a comma.
                    decimal_point: if !locale && specs.thousands == b'.' { b',' } else { b'.' },
                },
            )
        }
    };
    let body = [sign, digits.as_bytes()].concat();
    write_padded(out, &specs, &body, body.len());
    Ok(())
}

/// The visitors over an argument. `printf_context` says which of fmt's two it is.
fn write_arg(out: &mut Vec<u8>, specs: &Specs, arg: &Arg, printf_context: bool) -> Result<()> {
    match arg {
        Arg::Bool(value) => {
            let as_text = if printf_context {
                specs.kind == b's'
            } else {
                specs.kind == 0
            };
            if as_text {
                let specs = Specs { kind: 0, ..*specs };
                let text: &[u8] = if *value { b"true" } else { b"false" };
                write_string(out, &specs, text);
                Ok(())
            } else {
                write_integer(out, specs, false, u128::from(*value))
            }
        }
        Arg::Int(value) => write_integer(out, specs, *value < 0, u128::from(value.unsigned_abs())),
        Arg::UInt(value) => write_integer(out, specs, false, u128::from(*value)),
        Arg::HugeInt(value) => write_integer(out, specs, *value < 0, value.unsigned_abs()),
        Arg::UHugeInt(value) => write_integer(out, specs, false, *value),
        Arg::Double(value) => write_double(out, specs, *value),
        Arg::Text(text) => {
            if specs.kind != 0 && specs.kind != b's' {
                return Err(invalid_type_specifier(specs.kind, "string"));
            }
            write_string(out, specs, text);
            Ok(())
        }
        Arg::Char(value) => {
            if specs.kind != 0 && specs.kind != b'c' {
                let value = i128::from(*value as i8);
                return write_integer(out, specs, value < 0, value.unsigned_abs());
            }
            let specs = Specs {
                sign: Sign::None,
                alt: false,
                align: Align::Right,
                ..*specs
            };
            write_padded(out, &specs, &[*value], 1);
            Ok(())
        }
    }
}

/// `parse_nonnegative_int`, with `s[*begin]` a digit.
fn parse_nonnegative_int(s: &[u8], begin: &mut usize) -> Result<usize> {
    if s[*begin] == b'0' {
        *begin += 1;
        return Ok(0);
    }
    let mut value: u64 = 0;
    while let Some(digit) = s.get(*begin).filter(|c| c.is_ascii_digit()) {
        value = value * 10 + u64::from(digit - b'0');
        if value > i32::MAX as u64 {
            return Err(invalid_input("number is too big"));
        }
        *begin += 1;
    }
    Ok(value as usize)
}

/// `basic_format_parse_context`'s argument numbering. Negative after a manual index.
struct ArgIds<'a> {
    args: &'a [Arg],
    next: i32,
}

impl<'a> ArgIds<'a> {
    fn next_arg(&mut self) -> Result<&'a Arg> {
        if self.next < 0 {
            return Err(invalid_input(
                "cannot switch from manual to automatic argument indexing",
            ));
        }
        self.next += 1;
        self.arg((self.next - 1) as usize)
    }

    fn indexed_arg(&mut self, index: usize) -> Result<&'a Arg> {
        if self.next > 0 {
            return Err(invalid_input(
                "cannot switch from automatic to manual argument indexing",
            ));
        }
        self.next = -1;
        self.arg(index)
    }

    fn arg(&self, index: usize) -> Result<&'a Arg> {
        self.args
            .get(index)
            .ok_or_else(|| invalid_input(format!("Argument index \"{index}\" out of range")))
    }

    /// The argument of an id, which `parse_arg_id` found.
    fn resolve(&mut self, id: ArgId<'_>) -> Result<&'a Arg> {
        match id {
            ArgId::Auto => self.next_arg(),
            ArgId::Index(index) => self.indexed_arg(index),
            ArgId::Name(name) => {
                let name = String::from_utf8_lossy(name);
                Err(invalid_input(format!(
                    "Argument with name \"{name}\" not found, did you mean to use it as a format specifier \
                     (e.g. {{:{name}}}"
                )))
            }
        }
    }
}

enum ArgId<'a> {
    Auto,
    Index(usize),
    Name(&'a [u8]),
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// `parse_arg_id`, for a position that is not at the end.
fn parse_arg_id<'s>(s: &'s [u8], begin: &mut usize) -> Result<ArgId<'s>> {
    let c = s[*begin];
    if c == b'}' || c == b':' {
        return Ok(ArgId::Auto);
    }
    if c.is_ascii_digit() {
        let index = parse_nonnegative_int(s, begin)?;
        return match s.get(*begin) {
            Some(b'}' | b':') => Ok(ArgId::Index(index)),
            _ => Err(invalid_input("invalid format string")),
        };
    }
    if !is_name_start(c) {
        return Err(invalid_input("invalid format string"));
    }
    let start = *begin;
    *begin += 1;
    while s.get(*begin).is_some_and(|c| is_name_start(*c) || c.is_ascii_digit()) {
        *begin += 1;
    }
    Ok(ArgId::Name(&s[start..*begin]))
}

/// `get_dynamic_spec`.
fn dynamic_spec(arg: &Arg, what: &str) -> Result<usize> {
    let (negative, value) = match arg {
        Arg::Int(v) => (*v < 0, *v as u64),
        Arg::UInt(v) => (false, *v),
        Arg::HugeInt(v) => (*v < 0, *v as u64),
        Arg::UHugeInt(v) => (false, *v as u64),
        _ => return Err(invalid_input(format!("{what} is not integer"))),
    };
    if negative {
        return Err(invalid_input(format!("negative {what}")));
    }
    if value > i32::MAX as u64 {
        return Err(invalid_input("number is too big"));
    }
    Ok(value as usize)
}

/// `numeric_specs_checker::require_numeric_argument`.
fn require_numeric(arg: &Arg) -> Result<()> {
    if arg.is_arithmetic() {
        Ok(())
    } else {
        Err(invalid_input("format specifier requires numeric argument"))
    }
}

/// `parse_format_specs` with `specs_checker`: `s` from `begin` follows the colon.
fn parse_format_specs(s: &[u8], begin: &mut usize, arg: &Arg, ids: &mut ArgIds<'_>) -> Result<Specs> {
    let mut specs = Specs::default();
    let end = s.len();
    if *begin == end || s[*begin] == b'}' {
        return Ok(specs);
    }

    // Fill and alignment.
    let align_of = |c: u8| match c {
        b'<' => Some(Align::Left),
        b'>' => Some(Align::Right),
        b'=' => Some(Align::Numeric),
        b'^' => Some(Align::Center),
        _ => None,
    };
    let filled = *begin + 1 != end;
    if filled && let Some(align) = align_of(s[*begin + 1]) {
        if s[*begin] == b'{' {
            return Err(invalid_input("invalid fill character '{'"));
        }
        specs.fill = s[*begin];
        *begin += 2;
        specs.align = align;
    } else if let Some(align) = align_of(s[*begin]) {
        *begin += 1;
        specs.align = align;
    }
    if specs.align == Align::Numeric {
        require_numeric(arg)?;
    }
    if *begin == end {
        return Ok(specs);
    }

    // Sign.
    let check_sign = || -> Result<()> {
        require_numeric(arg)?;
        if arg.is_integral() && !matches!(arg, Arg::Int(_)) {
            return Err(invalid_input("format specifier requires signed argument"));
        }
        Ok(())
    };
    match s[*begin] {
        b'+' => {
            check_sign()?;
            specs.sign = Sign::Plus;
            *begin += 1;
        }
        b'-' => {
            check_sign()?;
            specs.sign = Sign::Minus;
            *begin += 1;
        }
        b' ' => {
            check_sign()?;
            specs.sign = Sign::Space;
            *begin += 1;
        }
        separator @ (b',' | b'_' | b'\'') => {
            specs.thousands = separator;
            *begin += 1;
        }
        b't' => {
            *begin += 1;
            if *begin == end {
                return Ok(specs);
            }
            specs.thousands = s[*begin];
            *begin += 1;
        }
        _ => {}
    }
    if *begin == end {
        return Ok(specs);
    }

    if s[*begin] == b'#' {
        require_numeric(arg)?;
        specs.alt = true;
        *begin += 1;
        if *begin == end {
            return Ok(specs);
        }
    }

    if s[*begin] == b'0' {
        require_numeric(arg)?;
        specs.align = Align::Numeric;
        specs.fill = b'0';
        *begin += 1;
        if *begin == end {
            return Ok(specs);
        }
    }

    // Width.
    if s[*begin].is_ascii_digit() {
        specs.width = parse_nonnegative_int(s, begin)?;
    } else if s[*begin] == b'{' {
        *begin += 1;
        if *begin != end {
            let id = parse_arg_id(s, begin)?;
            specs.width = dynamic_spec(ids.resolve(id)?, "width")?;
        }
        if s.get(*begin) != Some(&b'}') {
            return Err(invalid_input("invalid format string"));
        }
        *begin += 1;
    }
    if *begin == end {
        return Ok(specs);
    }

    // Precision.
    if s[*begin] == b'.' {
        *begin += 1;
        match s.get(*begin) {
            Some(c) if c.is_ascii_digit() => specs.precision = parse_nonnegative_int(s, begin)? as i32,
            Some(b'{') => {
                *begin += 1;
                if *begin != end {
                    let id = parse_arg_id(s, begin)?;
                    specs.precision = dynamic_spec(ids.resolve(id)?, "precision")? as i32;
                }
                if s.get(*begin) != Some(&b'}') {
                    return Err(invalid_input("invalid format string"));
                }
                *begin += 1;
            }
            _ => return Err(invalid_input("missing precision specifier")),
        }
        if arg.is_integral() {
            return Err(invalid_input("precision not allowed for this argument type"));
        }
    }

    // Type.
    if *begin != end && s[*begin] != b'}' {
        specs.kind = s[*begin];
        *begin += 1;
    }
    Ok(specs)
}

/// `pfs_writer`: text, where only a doubled `}` may appear.
fn write_text(out: &mut Vec<u8>, text: &[u8]) -> Result<()> {
    let mut begin = 0;
    while let Some(at) = text[begin..].iter().position(|c| *c == b'}') {
        let after = begin + at + 1;
        if text.get(after) != Some(&b'}') {
            return Err(invalid_input("unmatched '}' in format string"));
        }
        out.extend_from_slice(&text[begin..after]);
        begin = after + 1;
    }
    out.extend_from_slice(&text[begin..]);
    Ok(())
}

/// `parse_format_string` with `format_handler`.
fn format(s: &[u8], args: &[Arg]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut ids = ArgIds { args, next: 0 };
    let end = s.len();
    let mut begin = 0;
    while begin != end {
        let Some(at) = s[begin..].iter().position(|c| *c == b'{') else {
            write_text(&mut out, &s[begin..])?;
            break;
        };
        let mut p = begin + at;
        write_text(&mut out, &s[begin..p])?;
        p += 1;
        if p == end {
            return Err(invalid_input("invalid format string"));
        }
        if s[p] == b'}' {
            write_arg(&mut out, &Specs::default(), ids.next_arg()?, false)?;
        } else if s[p] == b'{' {
            out.push(b'{');
        } else {
            let id = parse_arg_id(s, &mut p)?;
            let arg = ids.resolve(id)?;
            match s.get(p) {
                Some(b'}') => write_arg(&mut out, &Specs::default(), arg, false)?,
                Some(b':') => {
                    p += 1;
                    let specs = parse_format_specs(s, &mut p, arg, &mut ids)?;
                    match s.get(p) {
                        Some(b'}') => write_arg(&mut out, &specs, arg, false)?,
                        _ => return Err(invalid_input("missing '}' in format string")),
                    }
                }
                _ => return Err(invalid_input("missing '}' in format string")),
            }
        }
        begin = p + 1;
    }
    Ok(out)
}

/// The size of the integer that a length modifier asks for, if there is one.
#[derive(Clone, Copy)]
enum Length {
    Byte,
    Short,
    Long,
    Own,
}

/// `arg_converter`: the integer that `printf` formats, given the type specifier.
fn convert_arg(arg: &Arg, length: Length, kind: u8) -> Arg {
    if !arg.is_integral() || matches!(arg, Arg::Char(_)) {
        return arg.clone();
    }
    let is_unsigned_type = matches!(arg, Arg::UInt(_) | Arg::UHugeInt(_));
    let kind = if matches!(kind, b'd' | b'i') && is_unsigned_type {
        b'u'
    } else {
        kind
    };
    if matches!(arg, Arg::Bool(_)) && kind == b's' {
        return arg.clone();
    }
    let is_signed_type = matches!(arg, Arg::Int(_) | Arg::HugeInt(_));
    let is_signed = matches!(kind, b'd' | b'i')
        || (matches!(kind, b'a' | b'A' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G') && is_signed_type);
    let bits = arg.bits();
    let own_size = match arg {
        Arg::Bool(_) => 1,
        Arg::HugeInt(_) | Arg::UHugeInt(_) => 16,
        _ => 8,
    };
    let size = match length {
        Length::Byte => 1,
        Length::Short => 2,
        Length::Long => 8,
        Length::Own => own_size,
    };
    if size <= 4 {
        let (signed, unsigned) = match size {
            1 => (i64::from(bits as i8), u64::from(bits as u8)),
            _ => (i64::from(bits as i16), u64::from(bits as u16)),
        };
        return if is_signed {
            Arg::Int(signed)
        } else {
            Arg::UInt(unsigned)
        };
    }
    if is_signed {
        return if size > 8 {
            Arg::HugeInt(bits as i128)
        } else {
            Arg::Int(bits as i64)
        };
    }
    match arg {
        Arg::Bool(_) => arg.clone(),
        Arg::Int(_) | Arg::UInt(_) => Arg::UInt(bits as u64),
        _ => Arg::UHugeInt(bits),
    }
}

/// `basic_printf_context::parse_flags`.
fn parse_flags(specs: &mut Specs, s: &[u8], it: &mut usize) {
    while let Some(c) = s.get(*it) {
        match c {
            b'-' => specs.align = Align::Left,
            b'+' => specs.sign = Sign::Plus,
            b'0' => specs.fill = b'0',
            b' ' => specs.sign = Sign::Space,
            b'#' => specs.alt = true,
            b',' | b'\'' | b'_' => specs.thousands = *c,
            _ => return,
        }
        *it += 1;
    }
}

/// `printf_width_handler`.
fn printf_width(arg: &Arg, specs: &mut Specs) -> Result<usize> {
    if !arg.is_integral() {
        return Err(invalid_input("width is not integer"));
    }
    let width = match arg {
        Arg::Int(v) if *v < 0 => {
            specs.align = Align::Left;
            u128::from(v.unsigned_abs())
        }
        Arg::HugeInt(v) if *v < 0 => {
            specs.align = Align::Left;
            v.unsigned_abs()
        }
        _ => arg.bits(),
    };
    if width > i32::MAX as u128 {
        return Err(invalid_input("number is too big"));
    }
    Ok(width as usize)
}

/// `printf_precision_handler`.
fn printf_precision(arg: &Arg) -> Result<i32> {
    if !arg.is_integral() {
        return Err(invalid_input("precision is not integer"));
    }
    let fits = match arg {
        Arg::Int(v) => i32::try_from(*v).map(|_| ()).is_ok(),
        Arg::HugeInt(v) => i32::try_from(*v).map(|_| ()).is_ok(),
        _ => arg.bits() <= i32::MAX as u128,
    };
    if !fits {
        return Err(invalid_input("number is too big"));
    }
    Ok((arg.bits() as i32).max(0))
}

/// `basic_printf_context::parse_header`: the argument index, if there is one.
fn parse_header(s: &[u8], it: &mut usize, specs: &mut Specs, ids: &mut ArgIds<'_>) -> Result<Option<usize>> {
    let mut arg_index = None;
    let c = s.get(*it).copied().unwrap_or(0);
    if c.is_ascii_digit() {
        // An argument index if followed by '$', or a width possibly preceded with '0' flag(s).
        let value = parse_nonnegative_int(s, it)?;
        if s.get(*it) == Some(&b'$') {
            *it += 1;
            arg_index = Some(value);
        } else {
            if c == b'0' {
                specs.fill = b'0';
            }
            if value != 0 {
                specs.width = value;
                return Ok(arg_index);
            }
        }
    }
    parse_flags(specs, s, it);
    match s.get(*it) {
        Some(c) if c.is_ascii_digit() => specs.width = parse_nonnegative_int(s, it)?,
        Some(b'*') => {
            *it += 1;
            let arg = ids.next_arg()?;
            specs.width = printf_width(arg, specs)?;
        }
        _ => {}
    }
    Ok(arg_index)
}

/// `basic_printf_context::format`.
fn printf(s: &[u8], args: &[Arg]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut ids = ArgIds { args, next: 0 };
    let end = s.len();
    let mut start = 0;
    let mut it = 0;
    while it != end {
        let c = s[it];
        it += 1;
        if c != b'%' {
            continue;
        }
        if s.get(it) == Some(&c) {
            out.extend_from_slice(&s[start..it]);
            it += 1;
            start = it;
            continue;
        }
        out.extend_from_slice(&s[start..it - 1]);

        let mut specs = Specs {
            align: Align::Right,
            ..Specs::default()
        };

        // Argument index, flags and width.
        let arg_index = parse_header(s, &mut it, &mut specs, &mut ids)?;
        if arg_index == Some(0) {
            return Err(invalid_input("argument index out of range"));
        }

        // Precision.
        let mut empty_precision = false;
        if s.get(it) == Some(&b'.') {
            it += 1;
            match s.get(it) {
                Some(c) if c.is_ascii_digit() => specs.precision = parse_nonnegative_int(s, &mut it)? as i32,
                Some(b'*') => {
                    it += 1;
                    specs.precision = printf_precision(ids.next_arg()?)?;
                }
                _ => {
                    specs.precision = 0;
                    empty_precision = true;
                }
            }
        }

        let arg = match arg_index {
            Some(index) => ids.indexed_arg(index - 1)?,
            None => ids.next_arg()?,
        };
        if specs.alt && arg.is_zero_int() {
            specs.alt = false;
        }
        if specs.fill == b'0' {
            if arg.is_arithmetic() {
                specs.align = Align::Numeric;
            } else {
                specs.fill = b' '; // Ignore '0' flag for non-numeric types.
            }
        }

        // Length, which converts the argument to the required type.
        let c = s.get(it).copied().unwrap_or(0);
        if it != end {
            it += 1;
        }
        let mut t = s.get(it).copied().unwrap_or(0);
        let mut double_letter = |it: &mut usize, letter: u8| {
            let doubled = t == letter;
            if doubled {
                *it += 1;
                t = s.get(*it).copied().unwrap_or(0);
            }
            doubled
        };
        let mut arg = match c {
            b'h' => {
                let length = if double_letter(&mut it, b'h') {
                    Length::Byte
                } else {
                    Length::Short
                };
                convert_arg(arg, length, t)
            }
            b'l' => {
                double_letter(&mut it, b'l');
                convert_arg(arg, Length::Long, t)
            }
            b'j' | b'z' | b't' => convert_arg(arg, Length::Long, t),
            b'L' => arg.clone(),
            _ => {
                it -= 1;
                convert_arg(arg, Length::Own, c)
            }
        };

        // Type.
        if it == end {
            return Err(invalid_input("invalid format string"));
        }
        specs.kind = s[it];
        it += 1;
        if arg.is_integral() {
            match specs.kind {
                b'i' | b'u' => specs.kind = b'd',
                b'c' => arg = Arg::Char(arg.bits() as u8),
                _ => {}
            }
        }
        if specs.kind == b'd' && empty_precision {
            specs.thousands = b'.';
        }

        start = it;
        write_arg(&mut out, &specs, &arg, true)?;
    }
    out.extend_from_slice(&s[start..it]);
    Ok(out)
}
