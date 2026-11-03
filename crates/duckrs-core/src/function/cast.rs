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
            Number::Int(v) => v as f64,
            Number::UHuge(v) => v as f64,
            Number::Float(v) => v as f64,
            Number::Double(v) => v,
        })
    }
    fn render(self) -> String {
        format_double(self)
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
            let bytes = u.str_at(idx);
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
        PhysicalType::Float | PhysicalType::Double => string_to_number::<T>(source, result, count, mode, |s| {
            try_cast_string_to_double(s).map(Number::Double)
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
    fn floats_round_half_to_even() {
        assert_eq!(i32::from_number(Number::Double(2.5)), Some(2));
        assert_eq!(i32::from_number(Number::Double(3.5)), Some(4));
        assert_eq!(i8::from_number(Number::Double(127.6)), None);
        assert_eq!(i8::from_number(Number::Double(-128.4)), Some(-128));
        assert_eq!(i64::from_number(Number::Double(f64::NAN)), None);
        assert_eq!(f32::from_number(Number::Double(1e300)), None);
    }
}
