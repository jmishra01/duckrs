// Ported from duckdb/src/function/cast/{default_casts,numeric_casts,string_cast,
// vector_cast_helpers}.cpp and duckdb/src/include/duckdb/common/operator/{numeric_cast,
// integer_cast_operator,cast_operators}.hpp @ rust-port-baseline
//
//! Vector casts (`CastFunctionSet::GetCastFunction` + the default cast functions): between the
//! numeric types and BOOLEAN, from anything to VARCHAR, and from VARCHAR to the numeric types
//! and BOOLEAN. [`super::time_casts`] has the temporal types, BLOB and UUID, and
//! [`super::bit_casts`] has BIT and BIGNUM. The casts of the types that are not ported yet fail
//! as not ported.

use super::{bit_casts, nested_casts, time_casts, variant};
use duckrs_common::executor;
use duckrs_common::string_cast::{format_double, format_float};
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType, PhysicalType};
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};

/// A number on its way between two numeric types.
#[derive(Clone, Copy)]
enum Number {
    Int(i128),
    UHuge(u128),
    Float(f32),
    Double(f64),
}

/// The numeric types (and BOOLEAN), with the conversions `TryCastWithOverflowCheck` performs.
trait CastNumber: NativeType {
    fn to_number(self) -> Number;
    fn from_number(n: Number) -> Option<Self>;
    /// `ConvertToString::Operation`, for error messages.
    fn render(self) -> String;
}

/// `TryCastWithOverflowCheckFloat`: floats round half to even (`nearbyint`), and the rounded
/// value must fit `[min, max)`.
fn round_into_range(value: f64, min: f64, max: f64) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    let rounded = value.round_ties_even();
    (rounded >= min && rounded < max).then_some(rounded)
}

macro_rules! integer_cast_number {
    ($($t:ty),*) => {$(
        impl CastNumber for $t {
            fn to_number(self) -> Number {
                Number::Int(self as i128)
            }
            fn from_number(n: Number) -> Option<Self> {
                // the exclusive bounds of the type, as floats
                #[allow(unused_comparisons)]
                let max = if <$t>::MIN < 0 { -(<$t>::MIN as f64) } else { <$t>::MAX as f64 + 1.0 };
                let min = <$t>::MIN as f64;
                match n {
                    Number::Int(v) => <$t>::try_from(v).ok(),
                    Number::UHuge(v) => <$t>::try_from(v).ok(),
                    Number::Float(v) => {
                        let (min, max) = (min as f32, max as f32);
                        if !v.is_finite() {
                            return None;
                        }
                        let rounded = v.round_ties_even();
                        (rounded >= min && rounded < max).then_some(rounded as $t)
                    }
                    Number::Double(v) => round_into_range(v, min, max).map(|r| r as $t),
                }
            }
            fn render(self) -> String {
                self.to_string()
            }
        }
    )*};
}
integer_cast_number!(i8, i16, i32, i64, i128, u8, u16, u32, u64);

impl CastNumber for u128 {
    fn to_number(self) -> Number {
        Number::UHuge(self)
    }
    fn from_number(n: Number) -> Option<Self> {
        let max = 2f64.powi(128);
        match n {
            Number::Int(v) => u128::try_from(v).ok(),
            Number::UHuge(v) => Some(v),
            Number::Float(v) => round_into_range(v as f64, 0.0, max).map(|r| r as u128),
            Number::Double(v) => round_into_range(v, 0.0, max).map(|r| r as u128),
        }
    }
    fn render(self) -> String {
        self.to_string()
    }
}

impl CastNumber for bool {
    fn to_number(self) -> Number {
        Number::Int(self as i128)
    }
    fn from_number(n: Number) -> Option<Self> {
        Some(match n {
            Number::Int(v) => v != 0,
            Number::UHuge(v) => v != 0,
            Number::Float(v) => v != 0.0,
            Number::Double(v) => v != 0.0,
        })
    }
    fn render(self) -> String {
        self.to_string()
    }
}

impl CastNumber for f32 {
    fn to_number(self) -> Number {
        Number::Float(self)
    }
    fn from_number(n: Number) -> Option<Self> {
        match n {
            // every integer rounds once
            Number::Int(v) => Some(v as f32),
            Number::UHuge(v) => Some(v as f32),
            Number::Float(v) => Some(v),
            Number::Double(v) => {
                let result = v as f32;
                // infinities and NaN stay what they are; a finite double must fit
                (!v.is_finite() || result.is_finite()).then_some(result)
            }
        }
    }
    fn render(self) -> String {
        format_float(self)
    }
}

impl CastNumber for f64 {
    fn to_number(self) -> Number {
        Number::Double(self)
    }
    fn from_number(n: Number) -> Option<Self> {
        Some(match n {
            Number::Int(v) => int_to_double(v),
            Number::UHuge(v) => uhuge_to_double(v),
            Number::Float(v) => v as f64,
            Number::Double(v) => v,
        })
    }
    fn render(self) -> String {
        format_double(self)
    }
}

/// `CastBigintToFloating`: a (U)HUGEINT as a double, rounded once.
fn huge_to_double(value: i128) -> f64 {
    value as f64
}

pub(super) fn uhuge_to_double(value: u128) -> f64 {
    value as f64
}

/// An integer as a double: 64-bit values convert directly, wider ones like `Hugeint::TryCast`.
pub(super) fn int_to_double(value: i128) -> f64 {
    match i64::try_from(value) {
        Ok(v) => v as f64,
        Err(_) => huge_to_double(value),
    }
}

/// How a failed row is handled: an error (CAST) or NULL (TRY_CAST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastMode {
    Strict,
    Try,
}

pub(super) fn conversion_error(message: String) -> Error {
    Error::new(ErrorKind::Conversion, message)
}

fn numeric_to_numeric<S: CastNumber, T: CastNumber>(
    source: &Vector,
    result: &mut Vector,
    count: usize,
    mode: CastMode,
) -> Result<()> {
    executor::unary_fallible::<S, T>(source, result, count, |v| match T::from_number(v.to_number()) {
        Some(r) => Ok(Some(r)),
        None if mode == CastMode::Try => Ok(None),
        None => Err(conversion_error(format!(
            "Type {} with value {} can't be cast because the value is out of range for the destination type {}",
            S::PHYSICAL,
            v.render(),
            T::PHYSICAL
        ))),
    })
}

/// Dispatches a numeric cast on the target's physical type.
fn numeric_to<S: CastNumber>(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    match result.physical_type() {
        PhysicalType::Bool => numeric_to_numeric::<S, bool>(source, result, count, mode),
        PhysicalType::Int8 => numeric_to_numeric::<S, i8>(source, result, count, mode),
        PhysicalType::Int16 => numeric_to_numeric::<S, i16>(source, result, count, mode),
        PhysicalType::Int32 => numeric_to_numeric::<S, i32>(source, result, count, mode),
        PhysicalType::Int64 => numeric_to_numeric::<S, i64>(source, result, count, mode),
        PhysicalType::Int128 => numeric_to_numeric::<S, i128>(source, result, count, mode),
        PhysicalType::UInt8 => numeric_to_numeric::<S, u8>(source, result, count, mode),
        PhysicalType::UInt16 => numeric_to_numeric::<S, u16>(source, result, count, mode),
        PhysicalType::UInt32 => numeric_to_numeric::<S, u32>(source, result, count, mode),
        PhysicalType::UInt64 => numeric_to_numeric::<S, u64>(source, result, count, mode),
        PhysicalType::UInt128 => numeric_to_numeric::<S, u128>(source, result, count, mode),
        PhysicalType::Float => numeric_to_numeric::<S, f32>(source, result, count, mode),
        PhysicalType::Double => numeric_to_numeric::<S, f64>(source, result, count, mode),
        _ => unreachable!("numeric_to with a non-numeric target"),
    }
}

/// The state of `IntegerCastLoop` with `IntegerDecimalCastOperation`: the integer part, the
/// fraction's digits and how many there are.
#[derive(Default)]
struct IntegerDecimal {
    result: i128,
    decimal: i128,
    decimal_digits: u32,
    /// `HugeIntCastData`: a fraction that does not fit a HUGEINT fails the cast instead of
    /// being cut short
    hugeint: bool,
}

impl IntegerDecimal {
    fn handle_digit(&mut self, digit: u8, negative: bool) -> bool {
        let digit = digit as i128;
        let next = self.result.checked_mul(10).and_then(|r| {
            if negative {
                r.checked_sub(digit)
            } else {
                r.checked_add(digit)
            }
        });
        match next {
            Some(r) => {
                self.result = r;
                true
            }
            None => false,
        }
    }

    fn handle_decimal(&mut self, digit: u8) -> bool {
        // digits past this position cannot affect the result for any i16 exponent
        const MAX_DECIMAL_DIGITS: u32 = 32768;
        if self.decimal_digits == MAX_DECIMAL_DIGITS {
            return true;
        }
        if self.decimal > (i128::MAX - digit as i128) / 10 {
            return !self.hugeint;
        }
        self.decimal_digits += 1;
        self.decimal = self.decimal * 10 + digit as i128;
        true
    }

    fn handle_exponent(&mut self, exponent: i16, negative: bool) -> bool {
        let mut e = exponent;
        if e < 0 {
            while e < 0 {
                e += 1;
                self.decimal = self.result % 10;
                self.result /= 10;
                if self.result == 0 && self.decimal == 0 {
                    break;
                }
            }
            self.decimal = self.decimal.abs();
            self.decimal_digits = 1;
            return self.finalize(negative);
        }
        while self.result != 0 && e > 0 {
            e -= 1;
            match self.result.checked_mul(10) {
                Some(r) => self.result = r,
                None => return false,
            }
        }
        if self.decimal == 0 {
            return self.finalize(negative);
        }
        // shift the fraction's digits that the exponent moves into the integer part
        let mut shift = exponent as i64 - self.decimal_digits as i64;
        let mut remainder = 0;
        if shift < 0 {
            if ((-shift) as u64) < 38 {
                let mut power: i128 = 1;
                while shift < 0 {
                    shift += 1;
                    power *= 10;
                }
                remainder = self.decimal % power;
                self.decimal /= power;
            } else {
                remainder = self.decimal;
                self.decimal = 0;
            }
        } else {
            while shift > 0 {
                shift -= 1;
                match self.decimal.checked_mul(10) {
                    Some(d) => self.decimal = d,
                    None => return false,
                }
            }
        }
        self.decimal_digits = self.decimal_digits.wrapping_sub(exponent as u32);
        let next = if negative {
            self.result.checked_sub(self.decimal)
        } else {
            self.result.checked_add(self.decimal)
        };
        match next {
            Some(r) => self.result = r,
            None => return false,
        }
        self.decimal = remainder;
        self.finalize(negative)
    }

    /// Rounds on the first fractional digit (half away from zero).
    fn finalize(&mut self, negative: bool) -> bool {
        while self.decimal > 10 {
            self.decimal /= 10;
            self.decimal_digits = self.decimal_digits.wrapping_sub(1);
        }
        if self.decimal_digits == 1 && self.decimal >= 5 {
            let next = if negative {
                self.result.checked_sub(1)
            } else {
                self.result.checked_add(1)
            };
            match next {
                Some(r) => self.result = r,
                None => return false,
            }
        }
        true
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `IntegerCastLoop<IntegerDecimalCastData, NEGATIVE, ALLOW_EXPONENT=true>` (non-strict).
fn integer_cast_loop(buf: &[u8], negative: bool, allow_exponent: bool, state: &mut IntegerDecimal) -> bool {
    let start = if negative || buf[0] == b'+' { 1 } else { 0 };
    let mut pos = start;
    let len = buf.len();
    while pos < len {
        let c = buf[pos];
        if !c.is_ascii_digit() {
            if c == b'.' {
                let number_before_period = pos > start;
                pos += 1;
                let start_digit = pos;
                while pos < len && buf[pos].is_ascii_digit() {
                    if allow_exponent {
                        if !state.handle_decimal(buf[pos] - b'0') {
                            return false;
                        }
                    } else {
                        // the exponent itself takes no decimals
                        return false;
                    }
                    pos += 1;
                    if pos != len && buf[pos] == b'_' {
                        pos += 1;
                        if pos == len || !buf[pos].is_ascii_digit() {
                            return false;
                        }
                    }
                }
                // "1." and ".1" are numbers, "." is not
                if !(number_before_period || pos > start_digit) {
                    return false;
                }
                if pos >= len {
                    break;
                }
            }
            if is_space(buf[pos]) {
                if pos == start {
                    return false;
                }
                // only trailing spaces may follow
                while pos + 1 < len {
                    pos += 1;
                    if !is_space(buf[pos]) {
                        return false;
                    }
                }
                pos = len;
                break;
            }
            if allow_exponent && (buf[pos] == b'e' || buf[pos] == b'E') {
                if pos == start {
                    return false;
                }
                pos += 1;
                if pos >= len {
                    return false;
                }
                let mut exponent = IntegerDecimal::default();
                let exponent_negative = buf[pos] == b'-';
                if !integer_cast_loop(&buf[pos..], exponent_negative, false, &mut exponent) {
                    return false;
                }
                let Ok(exponent) = i16::try_from(exponent.result) else {
                    return false;
                };
                return state.handle_exponent(exponent, negative);
            }
            return false;
        }
        if !state.handle_digit(c - b'0', negative) {
            return false;
        }
        pos += 1;
        if pos != len && buf[pos] == b'_' {
            pos += 1;
            if pos == len || !buf[pos].is_ascii_digit() {
                return false;
            }
        }
    }
    if allow_exponent && !state.finalize(negative) {
        return false;
    }
    pos > start
}

/// `IntegerHexCastLoop` / `IntegerBinaryCastLoop`: `buf` starts after the leading `0`.
fn integer_radix_loop(buf: &[u8], radix: u32) -> Option<i128> {
    let mut result: i128 = 0;
    let mut pos = 1;
    if pos >= buf.len() {
        return None;
    }
    while pos < buf.len() {
        let digit = (buf[pos] as char).to_digit(radix)?;
        pos += 1;
        if pos != buf.len() && buf[pos] == b'_' {
            pos += 1;
            if pos == buf.len() || (buf[pos] as char).to_digit(radix).is_none() {
                return None;
            }
        }
        result = result.checked_mul(radix as i128)?.checked_add(digit as i128)?;
    }
    Some(result)
}

/// A UHUGEINT from a string, for values no `i128` holds: a hex or binary literal, or digits with
/// an optional fraction and exponent, rounding half up.
fn try_cast_string_to_uhugeint(input: &[u8]) -> Option<u128> {
    let text = std::str::from_utf8(input)
        .ok()?
        .trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let radix_literal = |prefixes: [&str; 2]| prefixes.iter().find_map(|p| text.strip_prefix(p));
    // underscores only go between digits
    let separated = |digits: &str| !digits.starts_with('_') && !digits.ends_with('_') && !digits.contains("__");
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0b", "0B"], 2)] {
        if let Some(digits) = radix_literal(prefixes) {
            return separated(digits)
                .then(|| u128::from_str_radix(&digits.replace('_', ""), radix).ok())
                .flatten();
        }
    }
    let text = text.strip_prefix('+').unwrap_or(text);
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], text[i + 1..].parse::<i16>().ok()? as i32),
        None => (text, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: Vec<u8> = integer.bytes().chain(fraction.bytes()).filter(|c| *c != b'_').collect();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) || !separated(integer) || !separated(fraction) {
        return None;
    }
    // the exponent moves the decimal point
    let point = integer.bytes().filter(|c| *c != b'_').count() as i64 + exponent as i64;
    let mut value: u128 = 0;
    for position in 0..point.max(0) {
        let digit = digits.get(position as usize).map_or(0, |d| d - b'0');
        value = value.checked_mul(10)?.checked_add(digit as u128)?;
    }
    if point >= 0 && digits.get(point as usize).is_some_and(|d| *d >= b'5') {
        value = value.checked_add(1)?;
    }
    Some(value)
}

/// `TryIntegerCast` (non-strict): the integer a string spells, rounding a fraction.
pub fn try_cast_string_to_integer(input: &[u8]) -> Option<i128> {
    string_to_integer(input, false)
}

/// `TryCast::Operation(string_t, hugeint_t &)`.
pub fn try_cast_string_to_hugeint(input: &[u8]) -> Option<i128> {
    string_to_integer(input, true)
}

fn string_to_integer(input: &[u8], hugeint: bool) -> Option<i128> {
    let mut buf = input;
    while let [first, rest @ ..] = buf {
        if !is_space(*first) {
            break;
        }
        buf = rest;
    }
    if buf.is_empty() {
        return None;
    }
    let mut state = IntegerDecimal {
        hugeint,
        ..IntegerDecimal::default()
    };
    if buf[0] == b'-' {
        return integer_cast_loop(buf, true, true, &mut state).then_some(state.result);
    }
    if buf.len() > 1 && buf[0] == b'0' {
        match buf[1] {
            b'x' | b'X' => return integer_radix_loop(&buf[1..], 16),
            b'b' | b'B' => return integer_radix_loop(&buf[1..], 2),
            _ => {}
        }
    }
    integer_cast_loop(buf, false, true, &mut state).then_some(state.result)
}

/// `TryCastStringBool` (non-strict).
pub fn try_cast_string_to_bool(input: &[u8]) -> Option<bool> {
    let lower: Vec<u8> = input.iter().map(u8::to_ascii_lowercase).collect();
    match lower.as_slice() {
        b"t" | b"y" | b"1" | b"true" | b"yes" => Some(true),
        b"f" | b"n" | b"0" | b"false" | b"no" => Some(false),
        _ => None,
    }
}

/// `TryDoubleCast`: a number with surrounding spaces, `inf` or `nan`.
pub fn try_cast_string_to_double(input: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(input)
        .ok()?
        .trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    if text.is_empty() {
        return None;
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    // Rust also accepts "infinity"; DuckDB does too, but not other words or hex floats
    if unsigned.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
        && !["inf", "infinity", "nan"]
            .iter()
            .any(|w| unsigned.eq_ignore_ascii_case(w))
    {
        return None;
    }
    // `fast_float::parse_number_string`: one underscore between two digits is skipped
    if text.contains('_') {
        let bytes = text.as_bytes();
        let separated = bytes.iter().enumerate().all(|(i, b)| {
            *b != b'_'
                || (i > 0 && bytes[i - 1].is_ascii_digit() && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        });
        if !separated {
            return None;
        }
        return text.replace('_', "").parse::<f64>().ok();
    }
    text.parse::<f64>().ok()
}

fn string_to_number<T: CastNumber>(
    source: &Vector,
    result: &mut Vector,
    count: usize,
    mode: CastMode,
    parse: impl Fn(&[u8]) -> Option<Number>,
) -> Result<()> {
    let mut error = None;
    let mut nulls = Vec::new();
    {
        let u = source.to_unified(count);
        result.prepare_flat(count);
        let out = result.data_mut::<T>();
        for (i, slot) in out.iter_mut().enumerate().take(count) {
            let idx = u.sel.get(i);
            if !u.validity.is_valid(idx) {
                nulls.push(i);
                continue;
            }
            let bytes = u.str_at(i);
            match parse(bytes).and_then(T::from_number) {
                Some(v) => *slot = v,
                None if mode == CastMode::Try => nulls.push(i),
                None => {
                    error = Some(conversion_error(format!(
                        "Could not convert string '{}' to {}",
                        String::from_utf8_lossy(bytes),
                        T::PHYSICAL
                    )));
                    break;
                }
            }
        }
    }
    if let Some(e) = error {
        return Err(e);
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
    Ok(())
}

fn string_to<T: CastNumber>(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    match T::PHYSICAL {
        PhysicalType::Bool => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_bool(s).map(|b| Number::Int(b as i128))
        }),
        // a FLOAT is parsed as a float, so a value beyond its range becomes infinite
        PhysicalType::Float => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_double(s)?;
            let text = std::str::from_utf8(s).ok()?;
            let text = text.trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
            // the digit separators were checked with the double
            text.replace('_', "").parse::<f32>().ok().map(Number::Float)
        }),
        PhysicalType::Double => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_double(s).map(Number::Double)
        }),
        // only a UHUGEINT goes past what the integer loop holds
        PhysicalType::UInt128 => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_integer(s)
                .map(Number::Int)
                .or_else(|| try_cast_string_to_uhugeint(s).map(Number::UHuge))
        }),
        PhysicalType::Int128 => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_hugeint(s).map(Number::Int)
        }),
        _ => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_integer(s).map(Number::Int)
        }),
    }
}

/// Any value to VARCHAR (`StringCast` and friends), row by row.
fn to_varchar(source: &Vector, result: &mut Vector, count: usize) -> Result<()> {
    let constant = source.vector_type() == duckrs_common::vector::VectorType::Constant;
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows.max(1));
    for i in 0..rows {
        let value = source.get_value(i);
        if value.is_null() {
            result.set_null(i, true);
        } else {
            result.set_str(i, value.try_to_string()?.as_bytes());
        }
    }
    if constant {
        result.set_constant();
    }
    Ok(())
}

fn pow10(exponent: u8) -> i128 {
    10i128.pow(exponent as u32)
}

/// `TryDecimalStringCast`: the unscaled value a string spells at `scale`, rounding further
/// digits half away from zero; `None` if it is no number or exceeds the width.
fn string_to_decimal(input: &[u8], width: u8, scale: u8) -> Option<i128> {
    let text = std::str::from_utf8(input)
        .ok()?
        .trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], text[i + 1..].parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let valid = |part: &str| {
        part.is_empty()
            || (part.bytes().all(|c| c.is_ascii_digit() || c == b'_')
                && !part.starts_with('_')
                && !part.ends_with('_')
                && !part.contains("__"))
    };
    if !valid(integer) || !valid(fraction) || (integer.is_empty() && fraction.is_empty()) {
        return None;
    }
    let digits: Vec<u8> = integer
        .bytes()
        .chain(fraction.bytes())
        .filter(|c| *c != b'_')
        .map(|c| c - b'0')
        .collect();
    // the position of the decimal point in `digits`, after the exponent moved it
    let integer_digits = integer.bytes().filter(|c| *c != b'_').count() as i64;
    let point = integer_digits + exponent as i64;
    let end = point + scale as i64;
    let mut result: i128 = 0;
    let mut significant = 0i64;
    for position in 0..end.max(0) {
        let digit = digits.get(position as usize).copied().unwrap_or(0);
        if result != 0 || digit != 0 {
            significant += 1;
            if position < point && significant > (width - scale) as i64 {
                return None;
            }
            if significant > 38 {
                return None;
            }
        }
        result = result * 10 + digit as i128;
    }
    if end >= 0 && digits.get(end as usize).is_some_and(|d| *d >= 5) {
        result += 1;
    }
    (result < pow10(width)).then_some(if negative { -result } else { result })
}

/// A cast from or to DECIMAL of one row: its value, or the conversion error.
fn cast_decimal_value(value: &Value, target: &LogicalType) -> Result<Value> {
    let source = value.logical_type();
    let fail = |message: String| Err(conversion_error(message));
    if let Some((width, scale)) = source.decimal_properties() {
        let input = value.as_i128().expect("a DECIMAL value");
        if let Some((target_width, target_scale)) = target.decimal_properties() {
            // `DecimalDecimalCastSwitch`
            let out_of_range = || {
                conversion_error(format!(
                    "Casting value \"{}\" to type {target} failed: value is out of range!",
                    value.try_to_string().unwrap_or_default()
                ))
            };
            let unscaled = if target_scale >= scale {
                let difference = target_scale - scale;
                if target_width < difference {
                    if input != 0 {
                        return Err(out_of_range());
                    }
                    0
                } else {
                    let limit = pow10(target_width - difference);
                    if input >= limit || input <= -limit {
                        return Err(out_of_range());
                    }
                    input * pow10(difference)
                }
            } else {
                let factor = pow10(scale - target_scale);
                let limit = 10u128.pow(target_width as u32 + (scale - target_scale) as u32);
                let rounded = input.unsigned_abs()
                    + if (input % factor).unsigned_abs() >= (factor as u128) / 2 {
                        factor as u128
                    } else {
                        0
                    };
                if rounded >= limit {
                    return Err(out_of_range());
                }
                // round on the digit below the target scale, away from zero
                let halved = input / (factor / 2);
                (if input < 0 { halved - 1 } else { halved + 1 }) / 2
            };
            return Value::decimal(unscaled, target_width, target_scale);
        }
        return match target.physical_type() {
            PhysicalType::Bool => Ok(Value::boolean(input != 0)),
            PhysicalType::Float | PhysicalType::Double => {
                // the exact decimal, correctly rounded
                let text = duckrs_common::string_cast::format_decimal(input, width, scale);
                let number = text.parse::<f64>().expect("a decimal rendering");
                Ok(if target.physical_type() == PhysicalType::Float {
                    Value::float(text.parse::<f32>().expect("a decimal rendering"))
                } else {
                    Value::double(number)
                })
            }
            physical => {
                let power = pow10(scale);
                let rounding = if input < 0 { -power } else { power } / 2;
                let rounded = (input + rounding) / power;
                let (min, max) = super::cast_rules::integral_range(target.id());
                if !(min..=max).contains(&rounded) {
                    return fail(format!("Failed to cast decimal value {rounded} to type {physical}"));
                }
                integer_value(rounded, target)
            }
        };
    }
    let (width, scale) = target.decimal_properties().expect("a DECIMAL target");
    let decimal_error = |text: String| format!("Could not cast value {text} to DECIMAL({width},{scale})");
    let unscaled = match source.physical_type() {
        PhysicalType::Varchar => {
            let bytes = value.try_to_string()?;
            match string_to_decimal(bytes.as_bytes(), width, scale) {
                Some(unscaled) => unscaled,
                None => {
                    return fail(format!(
                        "Could not convert string \"{bytes}\" to DECIMAL({width},{scale})"
                    ));
                }
            }
        }
        PhysicalType::Bool => {
            let input = *value == Value::boolean(true);
            if input && width <= scale {
                return fail(decimal_error("true".into()));
            }
            if input { pow10(scale) } else { 0 }
        }
        PhysicalType::Float | PhysicalType::Double => {
            let input = match value.data() {
                duckrs_common::value::ValueData::Float(v) => *v as f64,
                duckrs_common::value::ValueData::Double(v) => *v,
                _ => unreachable!("a floating point value"),
            };
            let rounded = (input * 10f64.powi(scale as i32)).round();
            let limit = 10f64.powi(width as i32);
            if !rounded.is_finite() || rounded <= -limit || rounded >= limit {
                return fail(decimal_error(format!("{input:.6}")));
            }
            rounded as i128
        }
        _ => {
            let text = value.try_to_string()?;
            let input = match value.data() {
                duckrs_common::value::ValueData::UInt128(v) => i128::try_from(*v).unwrap_or(i128::MAX),
                _ => value.as_i128().expect("an integer value"),
            };
            let limit = pow10(width - scale);
            if input >= limit || input <= -limit {
                return fail(decimal_error(text));
            }
            input * pow10(scale)
        }
    };
    Value::decimal(unscaled, width, scale)
}

/// An integer as a value of the integer type `target`.
pub(super) fn integer_value(input: i128, target: &LogicalType) -> Result<Value> {
    use LogicalTypeId::*;
    Ok(match target.id() {
        TinyInt => Value::tinyint(input as i8),
        SmallInt => Value::smallint(input as i16),
        Integer => Value::integer(input as i32),
        BigInt => Value::bigint(input as i64),
        HugeInt => Value::hugeint(input),
        UTinyInt => Value::utinyint(input as u8),
        USmallInt => Value::usmallint(input as u16),
        UInteger => Value::uinteger(input as u32),
        UBigInt => Value::ubigint(input as u64),
        UHugeInt => Value::uhugeint(input as u128),
        other => unreachable!("{other:?} is not an integer type"),
    })
}

/// Casts from or to DECIMAL, row by row.
/// The value of row `index` of a decimal's data, whatever its physical type.
fn decimal_at(data: &duckrs_common::vector::UnifiedFormat, physical: PhysicalType, index: usize) -> i128 {
    match physical {
        PhysicalType::Int16 => i128::from(data.data::<i16>()[index]),
        PhysicalType::Int32 => i128::from(data.data::<i32>()[index]),
        PhysicalType::Int64 => i128::from(data.data::<i64>()[index]),
        _ => data.data::<i128>()[index],
    }
}

/// `DecimalScaleUpCheck` / `DecimalScaleUp` on whole vectors: a decimal to a decimal of the same or a
/// larger scale, as integers multiplied by a power of ten. `false` (and nothing written) when a value
/// does not fit, which the cast by value then reports.
fn decimal_scale_up(source: &Vector, result: &mut Vector, count: usize) -> Result<bool> {
    let (Some((_, from_scale)), Some((to_width, to_scale))) = (
        source.logical_type().decimal_properties(),
        result.logical_type().decimal_properties(),
    ) else {
        return Ok(false);
    };
    if to_scale < from_scale || to_width > 38 {
        return Ok(false);
    }
    let factor = 10i128.pow(u32::from(to_scale - from_scale));
    let limit = 10i128.pow(u32::from(to_width));
    let (source_physical, target_physical) = (source.physical_type(), result.physical_type());
    if !matches!(
        source_physical,
        PhysicalType::Int16 | PhysicalType::Int32 | PhysicalType::Int64 | PhysicalType::Int128
    ) {
        return Ok(false);
    }
    let constant = source.vector_type() == duckrs_common::vector::VectorType::Constant;
    let rows = if constant { 1 } else { count };
    let data = source.to_unified(rows);
    let mut values = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = data.sel.get(row);
        if !data.validity.is_valid(index) {
            values.push(None);
            continue;
        }
        match decimal_at(&data, source_physical, index).checked_mul(factor) {
            Some(value) if value.abs() < limit => values.push(Some(value)),
            _ => return Ok(false),
        }
    }
    result.prepare_flat(rows.max(1));
    macro_rules! write {
        ($t:ty) => {{
            let out = result.data_mut::<$t>();
            for (row, value) in values.iter().enumerate() {
                if let Some(value) = value {
                    out[row] = *value as $t;
                }
            }
        }};
    }
    match target_physical {
        PhysicalType::Int16 => write!(i16),
        PhysicalType::Int32 => write!(i32),
        PhysicalType::Int64 => write!(i64),
        PhysicalType::Int128 => write!(i128),
        _ => return Ok(false),
    }
    for (row, value) in values.iter().enumerate() {
        if value.is_none() {
            result.set_null(row, true);
        }
    }
    if constant {
        result.set_constant();
    }
    Ok(true)
}

fn decimal_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    if source.logical_type().id() == LogicalTypeId::Decimal
        && result.logical_type().id() == LogicalTypeId::Decimal
        && decimal_scale_up(source, result, count)?
    {
        return Ok(());
    }
    if decimal_to_double(source, result, count) {
        return Ok(());
    }
    let target = result.logical_type().clone();
    let constant = source.vector_type() == duckrs_common::vector::VectorType::Constant;
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows.max(1));
    for i in 0..rows {
        let value = source.get_value(i);
        if value.is_null() {
            result.set_null(i, true);
            continue;
        }
        match cast_decimal_value(&value, &target) {
            Ok(cast) => result.set_value(i, &cast)?,
            Err(_) if mode == CastMode::Try => result.set_null(i, true),
            Err(error) => return Err(error),
        }
    }
    if constant {
        result.set_constant();
    }
    Ok(())
}

/// A DECIMAL as a DOUBLE: its integer over a power of ten (`DecimalToDouble`), which is the
/// nearest DOUBLE where both are exact. False, with nothing done, where one is not.
fn decimal_to_double(source: &Vector, result: &mut Vector, count: usize) -> bool {
    let Some((_, scale)) = source.logical_type().decimal_properties() else {
        return false;
    };
    let physical = source.physical_type();
    if result.logical_type().id() != LogicalTypeId::Double
        || scale > 22
        || !matches!(
            physical,
            PhysicalType::Int16 | PhysicalType::Int32 | PhysicalType::Int64
        )
    {
        return false;
    }
    let constant = source.vector_type() == duckrs_common::vector::VectorType::Constant;
    let rows = if constant { 1 } else { count };
    let data = source.to_unified(rows);
    let divisor = 10f64.powi(i32::from(scale));
    let mut values = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = data.sel.get(row);
        if !data.validity.is_valid(index) {
            values.push(None);
            continue;
        }
        let value = decimal_at(&data, physical, index);
        if value.unsigned_abs() >= 1 << 53 {
            return false;
        }
        values.push(Some(value as f64 / divisor));
    }
    result.prepare_flat(rows.max(1));
    for (row, value) in values.iter().enumerate() {
        match value {
            Some(value) => result.data_mut::<f64>()[row] = *value,
            None => result.set_null(row, true),
        }
    }
    if constant {
        result.set_constant();
    }
    true
}

/// Whether [`decimal_cast`] covers a cast.
fn is_decimal_cast(source: &LogicalType, target: &LogicalType) -> bool {
    let plain = |ty: &LogicalType| {
        is_numeric_or_bool(ty) || ty.id() == LogicalTypeId::Decimal || ty.id() == LogicalTypeId::Varchar
    };
    (source.id() == LogicalTypeId::Decimal || target.id() == LogicalTypeId::Decimal) && plain(source) && plain(target)
}

fn is_numeric_or_bool(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    ty.id() == Boolean || (ty.is_numeric() && ty.id() != Decimal)
}

/// Whether every cast of `ty` is settled: it either exists, or is one only NULL survives.
fn is_ported_scalar(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(
        ty.id(),
        Boolean
            | TinyInt
            | SmallInt
            | Integer
            | BigInt
            | HugeInt
            | UTinyInt
            | USmallInt
            | UInteger
            | UBigInt
            | UHugeInt
            | Float
            | Double
            | Decimal
            | Date
            | Time
            | TimeNs
            | TimeTz
            | Timestamp
            | TimestampSec
            | TimestampMs
            | TimestampNs
            | TimestampTz
            | TimestampTzNs
            | Interval
            | Varchar
            | Blob
            | Uuid
            | Enum
            | Bit
            | Bignum
            | Variant
            | Geometry
    )
}

fn unimplemented_cast(source: &LogicalType, target: &LogicalType) -> Error {
    crate::unported(format!("casting {source} to {target}"))
}

/// Whether [`cast_vector`] can cast `source` to `target`.
pub fn has_cast(source: &LogicalType, target: &LogicalType) -> bool {
    source == target
        || source.id() == LogicalTypeId::SqlNull
        || target.id() == LogicalTypeId::SqlNull
        || target.id() == LogicalTypeId::Varchar
        || (is_numeric_or_bool(source) && is_numeric_or_bool(target))
        || (source.id() == LogicalTypeId::Varchar && is_numeric_or_bool(target))
        || is_decimal_cast(source, target)
        || (is_ported_scalar(source) && is_ported_scalar(target))
        || nested_casts::has_nested_cast(source, target, &is_ported_scalar)
}

/// `ReinterpretCast`, row by row.
fn reinterpret_cast(source: &Vector, result: &mut Vector, count: usize) -> Result<()> {
    let target = result.logical_type().clone();
    result.prepare_flat(count.max(1));
    for i in 0..count {
        result.set_value(i, &source.get_value(i).reinterpret(&target)?)?;
    }
    Ok(())
}

/// The cast of an ENUM value: through its string, except between ENUMs (`EnumCastSwitch`).
fn enum_value_cast(value: &Value, target: &LogicalType) -> std::result::Result<Value, Error> {
    let source = value.logical_type();
    let text = value.try_to_string()?;
    target
        .enum_values()
        .iter()
        .position(|v| *v == text)
        .map(|position| Value::enum_value(target, position))
        .ok_or_else(|| {
            conversion_error(if source.id() == LogicalTypeId::Enum {
                format!("Type {source} with value {text} can't be cast to the destination type {target}")
            } else {
                format!("Could not convert string '{text}' to {target}")
            })
        })
}

/// `StringEnumCast`, `EnumEnumCast` and `EnumToAnyCast`.
fn enum_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let (from, to) = (source.logical_type().clone(), result.logical_type().clone());
    if to.id() != LogicalTypeId::Enum {
        // the target is neither VARCHAR nor nested: both have been handled
        let mut strings = Vector::new(LogicalType::VARCHAR, count.max(1));
        to_varchar(source, &mut strings, count)?;
        return cast_vector(&strings, result, count, mode);
    }
    if !matches!(from.id(), LogicalTypeId::Enum | LogicalTypeId::Varchar) {
        return time_casts::null_cast(source, result, count, mode);
    }
    result.prepare_flat(count.max(1));
    for i in 0..count {
        let value = source.get_value(i);
        if value.is_null() {
            result.set_null(i, true);
            continue;
        }
        match enum_value_cast(&value, &to) {
            Ok(cast) => result.set_value(i, &cast)?,
            Err(_) if mode == CastMode::Try => result.set_null(i, true),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Casts `source` (of `count` rows) into `result`, whose type is the target type.
pub fn cast_vector(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let (from, to) = (source.logical_type().clone(), result.logical_type().clone());
    if from == to {
        *result = source.clone();
        return Ok(());
    }
    if from.id() == LogicalTypeId::SqlNull {
        *result = Vector::constant_null(to);
        return Ok(());
    }
    // `TryVectorNullCast`: only a NULL is a NULL
    if to.id() == LogicalTypeId::SqlNull {
        let rows = if source.vector_type() == duckrs_common::vector::VectorType::Constant {
            count.min(1)
        } else {
            count
        };
        let data = source.to_unified(rows.max(1));
        if mode == CastMode::Strict && (0..rows).any(|row| data.validity.is_valid(data.sel.get(row))) {
            return Err(conversion_error(format!(
                "Unimplemented type for cast ({from} -> {to})"
            )));
        }
        *result = Vector::constant_null(to);
        return Ok(());
    }
    if super::json::is_json_type(&to)
        || (super::json::is_json_type(&from) && to.alias().is_none() && to != LogicalType::VARCHAR)
    {
        return super::json::json_cast(source, result, count, mode);
    }
    if to.id() == LogicalTypeId::Varchar {
        return to_varchar(source, result, count);
    }
    if from.id() == LogicalTypeId::Variant || to.id() == LogicalTypeId::Variant {
        return variant::variant_cast(source, result, count, mode);
    }
    if from.id() == LogicalTypeId::Geometry || to.id() == LogicalTypeId::Geometry {
        return super::geometry::geometry_cast(source, result, count, mode);
    }
    if from.is_nested() || to.is_nested() {
        return nested_casts::nested_cast(source, result, count, mode);
    }
    if from.id() == LogicalTypeId::Enum || to.id() == LogicalTypeId::Enum {
        return enum_cast(source, result, count, mode);
    }
    let is_bit = |id| matches!(id, LogicalTypeId::Bit | LogicalTypeId::Bignum);
    if is_bit(from.id()) || is_bit(to.id()) {
        return bit_casts::bit_cast(source, result, count, mode);
    }
    if is_decimal_cast(&from, &to) {
        return decimal_cast(source, result, count, mode);
    }
    if from.id() == to.id() && from.id() != LogicalTypeId::Decimal {
        // types that differ only by an alias
        return reinterpret_cast(source, result, count);
    }
    if is_numeric_or_bool(&from) && is_numeric_or_bool(&to) {
        return match from.physical_type() {
            PhysicalType::Bool => numeric_to::<bool>(source, result, count, mode),
            PhysicalType::Int8 => numeric_to::<i8>(source, result, count, mode),
            PhysicalType::Int16 => numeric_to::<i16>(source, result, count, mode),
            PhysicalType::Int32 => numeric_to::<i32>(source, result, count, mode),
            PhysicalType::Int64 => numeric_to::<i64>(source, result, count, mode),
            PhysicalType::Int128 => numeric_to::<i128>(source, result, count, mode),
            PhysicalType::UInt8 => numeric_to::<u8>(source, result, count, mode),
            PhysicalType::UInt16 => numeric_to::<u16>(source, result, count, mode),
            PhysicalType::UInt32 => numeric_to::<u32>(source, result, count, mode),
            PhysicalType::UInt64 => numeric_to::<u64>(source, result, count, mode),
            PhysicalType::UInt128 => numeric_to::<u128>(source, result, count, mode),
            PhysicalType::Float => numeric_to::<f32>(source, result, count, mode),
            PhysicalType::Double => numeric_to::<f64>(source, result, count, mode),
            _ => Err(unimplemented_cast(&from, &to)),
        };
    }
    if from.id() == LogicalTypeId::Varchar && is_numeric_or_bool(&to) {
        return match to.physical_type() {
            PhysicalType::Bool => string_to::<bool>(source, result, count, mode),
            PhysicalType::Int8 => string_to::<i8>(source, result, count, mode),
            PhysicalType::Int16 => string_to::<i16>(source, result, count, mode),
            PhysicalType::Int32 => string_to::<i32>(source, result, count, mode),
            PhysicalType::Int64 => string_to::<i64>(source, result, count, mode),
            PhysicalType::Int128 => string_to::<i128>(source, result, count, mode),
            PhysicalType::UInt8 => string_to::<u8>(source, result, count, mode),
            PhysicalType::UInt16 => string_to::<u16>(source, result, count, mode),
            PhysicalType::UInt32 => string_to::<u32>(source, result, count, mode),
            PhysicalType::UInt64 => string_to::<u64>(source, result, count, mode),
            PhysicalType::UInt128 => string_to::<u128>(source, result, count, mode),
            PhysicalType::Float => string_to::<f32>(source, result, count, mode),
            PhysicalType::Double => string_to::<f64>(source, result, count, mode),
            _ => Err(unimplemented_cast(&from, &to)),
        };
    }
    if time_casts::has_temporal_cast(&from, &to) {
        return time_casts::temporal_cast(source, result, count, mode);
    }
    if is_ported_scalar(&from) && is_ported_scalar(&to) {
        return time_casts::null_cast(source, result, count, mode);
    }
    Err(unimplemented_cast(&from, &to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_to_integers_like_duckdb() {
        let cast = |s: &str| try_cast_string_to_integer(s.as_bytes());
        assert_eq!(cast("42"), Some(42));
        assert_eq!(cast("  -42  "), Some(-42));
        assert_eq!(cast("+7"), Some(7));
        assert_eq!(cast("1_000"), Some(1000));
        assert_eq!(cast("1__0"), None);
        assert_eq!(cast("1.5"), Some(2));
        assert_eq!(cast("1.49"), Some(1));
        assert_eq!(cast("-2.5"), Some(-3));
        assert_eq!(cast("1e3"), Some(1000));
        assert_eq!(cast("15e-1"), Some(2));
        assert_eq!(cast("0x1F"), Some(31));
        assert_eq!(cast("0b101"), Some(5));
        assert_eq!(cast("."), None);
        assert_eq!(cast("abc"), None);
        assert_eq!(cast(""), None);
        assert_eq!(cast("- 1"), None);
    }

    #[test]
    fn strings_to_uhugeint_like_duckdb() {
        let cast = |s: &str| try_cast_string_to_uhugeint(s.as_bytes());
        let max = u128::MAX;
        assert_eq!(cast(&max.to_string()), Some(max));
        assert_eq!(cast("340282366920938463463374607431768211455e0"), Some(max));
        assert_eq!(cast("34028236692093846346337460743176821145.5e1"), Some(max));
        assert_eq!(cast("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), Some(max));
        assert_eq!(cast("340282366920938463463374607431768211456"), None);
        assert_eq!(cast("340282366920938463463374607431768211455.5"), None);
        assert_eq!(cast("abc"), None);
    }

    #[test]
    fn decimals_cast_like_duckdb() {
        let cast = |value: Value, target: &LogicalType| {
            cast_decimal_value(&value, target)
                .map(|v| v.try_to_string().unwrap())
                .map_err(|e| e.to_string())
        };
        let d = |unscaled, width, scale| Value::decimal(unscaled, width, scale).unwrap();
        assert_eq!(cast(d(15, 2, 1), &LogicalType::DOUBLE).unwrap(), "1.5");
        assert_eq!(cast(d(15, 2, 1), &LogicalType::INTEGER).unwrap(), "2");
        assert_eq!(cast(d(-25, 2, 1), &LogicalType::INTEGER).unwrap(), "-3");
        assert_eq!(cast(d(1234, 4, 2), &LogicalType::decimal(4, 1)).unwrap(), "12.3");
        assert_eq!(cast(d(1235, 4, 2), &LogicalType::decimal(4, 1)).unwrap(), "12.4");
        assert_eq!(cast(d(15, 2, 1), &LogicalType::decimal(5, 3)).unwrap(), "1.500");
        assert_eq!(
            cast(d(1234, 4, 1), &LogicalType::decimal(3, 1)).unwrap_err(),
            "Conversion Error: Casting value \"123.4\" to type DECIMAL(3,1) failed: value is out of range!"
        );
        assert_eq!(cast(Value::integer(12), &LogicalType::decimal(4, 1)).unwrap(), "12.0");
        assert_eq!(
            cast(Value::integer(1000), &LogicalType::decimal(4, 1)).unwrap_err(),
            "Conversion Error: Could not cast value 1000 to DECIMAL(4,1)"
        );
        assert_eq!(cast(Value::double(1.25), &LogicalType::decimal(4, 1)).unwrap(), "1.3");
        assert_eq!(
            cast(Value::varchar(" 1.25 "), &LogicalType::decimal(4, 1)).unwrap(),
            "1.3"
        );
        assert_eq!(
            cast(Value::varchar("-1.2e1"), &LogicalType::decimal(4, 1)).unwrap(),
            "-12.0"
        );
        assert_eq!(
            cast(Value::varchar("abc"), &LogicalType::decimal(4, 1)).unwrap_err(),
            "Conversion Error: Could not convert string \"abc\" to DECIMAL(4,1)"
        );
    }

    #[test]
    fn floats_round_half_to_even() {
        assert_eq!(i32::from_number(Number::Double(2.5)), Some(2));
        assert_eq!(i32::from_number(Number::Double(3.5)), Some(4));
        assert_eq!(i8::from_number(Number::Double(127.6)), None);
        assert_eq!(i8::from_number(Number::Double(-128.4)), Some(-128));
        assert_eq!(i64::from_number(Number::Double(f64::NAN)), None);
        assert_eq!(f32::from_number(Number::Double(1e300)), None);
        // (U)HUGEINT rounds to FLOAT once, like DuckDB: 2^64 - 2^40 + 1 is nearest to 2^64 - 2^40
        let below = 18446742974197923840i128;
        assert_eq!(f32::from_number(Number::Int(below + 1)), Some(below as f32));
        assert_eq!(below as f32 as f64, 18446742974197923840.0);
    }
}
