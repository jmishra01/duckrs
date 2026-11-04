// Ported from duckdb/src/function/cast/{default_casts,numeric_casts,string_cast,
// vector_cast_helpers}.cpp and duckdb/src/include/duckdb/common/operator/{numeric_cast,
// integer_cast_operator,cast_operators}.hpp @ rust-port-baseline
//
//! Vector casts (`CastFunctionSet::GetCastFunction` + the default cast functions): between the
//! numeric types and BOOLEAN, from anything to VARCHAR, and from VARCHAR to the numeric types
//! and BOOLEAN. The other casts arrive with Phase 4.

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
                // `Hugeint::TryConvert`: -2^127 itself is out of range for floating point input
                .filter(|r| !(<$t>::BITS == 128 && *r == <$t>::MIN && matches!(n, Number::Float(_) | Number::Double(_))))
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
            // 64-bit integers convert directly, (U)HUGEINT through a double
            Number::Int(v) => Some(match i64::try_from(v) {
                Ok(v) => v as f32,
                Err(_) => huge_to_double(v) as f32,
            }),
            Number::UHuge(v) => Some(uhuge_to_double(v) as f32),
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

/// `CastBigintToFloating`: a (U)HUGEINT as a double, the way DuckDB computes it (which rounds
/// some values differently from a direct conversion).
fn huge_to_double(value: i128) -> f64 {
    let lower = value as u64;
    let upper = (value >> 64) as i64;
    if upper == -1 {
        // special case for upper = -1 to avoid rounding issues in small negative numbers
        -((u64::MAX - lower) as f64) - 1.0
    } else {
        lower as f64 + upper as f64 * (u64::MAX as f64 + 1.0)
    }
}

pub(super) fn uhuge_to_double(value: u128) -> f64 {
    (value as u64) as f64 + ((value >> 64) as u64) as f64 * (u64::MAX as f64 + 1.0)
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

fn conversion_error(message: String) -> Error {
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

    fn handle_decimal(&mut self, digit: u8) {
        // digits past this position cannot affect the result for any i16 exponent
        const MAX_DECIMAL_DIGITS: u32 = 32768;
        if self.decimal_digits == MAX_DECIMAL_DIGITS || self.decimal > (i128::MAX - digit as i128) / 10 {
            return;
        }
        self.decimal_digits += 1;
        self.decimal = self.decimal * 10 + digit as i128;
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
                        state.handle_decimal(buf[pos] - b'0');
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

/// A UHUGEINT from a plain digit string, for values no `i128` holds.
fn try_cast_string_to_uhugeint(input: &[u8]) -> Option<u128> {
    let text = std::str::from_utf8(input)
        .ok()?
        .trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let (integer, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value = integer.strip_prefix('+').unwrap_or(integer).parse::<u128>().ok()?;
    if fraction.as_bytes().first().is_some_and(|d| *d >= b'5') {
        value.checked_add(1)
    } else {
        Some(value)
    }
}

/// `TryIntegerCast` (non-strict): the integer a string spells, rounding a fraction.
pub fn try_cast_string_to_integer(input: &[u8]) -> Option<i128> {
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
    let mut state = IntegerDecimal::default();
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
            text.parse::<f32>().ok().map(Number::Float)
        }),
        PhysicalType::Double => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_double(s).map(Number::Double)
        }),
        _ => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_integer(s)
                .map(Number::Int)
                .or_else(|| try_cast_string_to_uhugeint(s).map(Number::UHuge))
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
fn integer_value(input: i128, target: &LogicalType) -> Result<Value> {
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
fn decimal_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
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

fn unimplemented_cast(source: &LogicalType, target: &LogicalType) -> Error {
    crate::unported(format!("casting {source} to {target}"))
}

/// Whether [`cast_vector`] can cast `source` to `target`.
pub fn has_cast(source: &LogicalType, target: &LogicalType) -> bool {
    source == target
        || source.id() == LogicalTypeId::SqlNull
        || target.id() == LogicalTypeId::Varchar
        || (is_numeric_or_bool(source) && is_numeric_or_bool(target))
        || (source.id() == LogicalTypeId::Varchar && is_numeric_or_bool(target))
        || is_decimal_cast(source, target)
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
    if to.id() == LogicalTypeId::Varchar {
        return to_varchar(source, result, count);
    }
    if is_decimal_cast(&from, &to) {
        return decimal_cast(source, result, count, mode);
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
        // (U)HUGEINT converts through a double, like DuckDB
        assert_eq!(f32::from_number(Number::Int(18446743523953736704)), Some(2f32.powi(64)));
    }
}
