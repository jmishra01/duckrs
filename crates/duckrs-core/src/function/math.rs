// Ported from duckdb/extension/core_functions/scalar/math/numeric.cpp @ rust-port-baseline
//
//! The math functions. They run row by row; the vectorized loops come with Phase 13.

use super::bit_functions::strict_rows;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

fn out_of_range(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

fn float(value: &Value) -> f64 {
    match value.data() {
        ValueData::Double(v) => *v,
        ValueData::Float(v) => f64::from(*v),
        _ => value.as_i128().expect("a number") as f64,
    }
}

fn double_set(name: &str, callback: ScalarCallback) -> ScalarFunctionSet {
    let mut set = ScalarFunctionSet::new(name);
    set.add(ScalarFunction::new(
        name,
        vec![LogicalType::DOUBLE],
        LogicalType::DOUBLE,
        callback,
    ));
    set
}

fn binary_set(name: &str, callback: ScalarCallback) -> ScalarFunctionSet {
    let mut set = ScalarFunctionSet::new(name);
    set.add(ScalarFunction::new(
        name,
        vec![LogicalType::DOUBLE, LogicalType::DOUBLE],
        LogicalType::DOUBLE,
        callback,
    ));
    set
}

/// `NoInfiniteDoubleWrapper`: an infinite input is an error, a NaN goes through.
fn finite(value: f64) -> Result<f64> {
    if value.is_infinite() {
        return Err(out_of_range(format!(
            "input value {value} is out of range for numeric function"
        )));
    }
    Ok(value)
}

macro_rules! unary {
    ($name:ident, $input:ident => $body:expr) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            strict_rows(args, result, |row| {
                let $input = float(&row[0]);
                let value: f64 = $body;
                Ok(Value::double(value))
            })
        }
    };
}

unary!(sqrt, x => {
    if x < 0.0 {
        return Err(out_of_range("cannot take square root of a negative number"));
    }
    x.sqrt()
});
unary!(cbrt, x => x.cbrt());
unary!(exp, x => x.exp());
unary!(sin, x => finite(x)?.sin());
unary!(cos, x => finite(x)?.cos());
unary!(tan, x => finite(x)?.tan());
unary!(cot, x => {
    let tangent = finite(x)?.tan();
    if tangent == 0.0 || tangent.is_nan() {
        return Err(out_of_range("cannot take cotangent of zero"));
    }
    1.0 / tangent
});
unary!(asin, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range("ASIN is undefined outside [-1,1]"));
    }
    x.asin()
});
unary!(acos, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range("ACOS is undefined outside [-1,1]"));
    }
    x.acos()
});
unary!(atan, x => finite(x)?.atan());
unary!(sinh, x => x.sinh());
unary!(cosh, x => x.cosh());
unary!(tanh, x => x.tanh());
unary!(asinh, x => x.asinh());
unary!(acosh, x => {
    if x < 1.0 {
        return Err(out_of_range("ACOSH is undefined below 1"));
    }
    x.acosh()
});
unary!(atanh, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range("ATANH is undefined outside [-1,1]"));
    }
    x.atanh()
});
unary!(degrees, x => x.to_degrees());
unary!(radians, x => x.to_radians());
unary!(ln, x => logarithm(x)?.ln());
unary!(log10, x => logarithm(x)?.log10());
unary!(log2, x => logarithm(x)?.log2());

fn logarithm(x: f64) -> Result<f64> {
    if x < 0.0 {
        return Err(out_of_range("cannot take logarithm of a negative number"));
    }
    if x == 0.0 {
        return Err(out_of_range("cannot take logarithm of zero"));
    }
    Ok(x)
}

fn log_base(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (base, x) = (float(&row[0]), float(&row[1]));
        let base = logarithm(base)?;
        if base == 1.0 {
            return Err(out_of_range("divison by zero in based logarithm"));
        }
        Ok(Value::double(logarithm(x)?.ln() / base.ln()))
    })
}

fn atan2(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::double(float(&row[0]).atan2(float(&row[1]))))
    })
}

fn pow(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (base, exponent) = (float(&row[0]), float(&row[1]));
        if base == 0.0 && exponent < 0.0 {
            return Err(out_of_range("zero raised to a negative power is undefined"));
        }
        Ok(Value::double(base.powf(exponent)))
    })
}

fn pi(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        result.set_value(row, &Value::double(std::f64::consts::PI))?;
    }
    Ok(())
}

fn is_nan(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(float(&row[0]).is_nan())))
}

fn is_inf(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(float(&row[0]).is_infinite())))
}

fn is_finite(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::boolean(float(&row[0]).is_finite())))
}

/// Rounds half away from zero by the exact value of `input`, which a product with a power of ten
/// can land on a midpoint it isn't on: the stored 2.675 lies below it.
fn round_exact(input: f64, precision: usize) -> f64 {
    let text = format!("{:.*}", precision + 1100, input.abs());
    let point = text.find('.').expect("a fraction was asked for");
    let cut = point + 1 + precision;
    let mut digits: Vec<u8> = text.as_bytes()[..cut].to_vec();
    if text.as_bytes()[cut] >= b'5' {
        let mut at = digits.len();
        loop {
            if at == 0 {
                digits.insert(0, b'1');
                break;
            }
            at -= 1;
            match digits[at] {
                b'.' => {}
                b'9' => digits[at] = b'0',
                digit => {
                    digits[at] = digit + 1;
                    break;
                }
            }
        }
    }
    let rounded: f64 = String::from_utf8_lossy(&digits).parse().unwrap_or(input.abs());
    rounded.copysign(input)
}

/// `RoundOperatorPrecision`.
fn round_double(input: f64, precision: i128, nearest: fn(f64) -> f64) -> f64 {
    if precision < 0 {
        let modifier = 10f64.powi((-precision).min(400) as i32);
        if modifier.is_infinite() {
            return if input.is_finite() { 0.0 } else { input };
        }
        let rounded = nearest(input / modifier) * modifier;
        if rounded.is_finite() { rounded } else { input }
    } else {
        let modifier = 10f64.powi(precision.min(400) as i32);
        let rounded = nearest(input * modifier) / modifier;
        if rounded.is_finite() { rounded } else { input }
    }
}

fn round_half_even(value: f64) -> f64 {
    value.round_ties_even()
}

fn round_precision(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (input, precision) = (float(&row[0]), row[1].as_i128().expect("an INTEGER"));
        if (1..=22).contains(&precision) && input.is_finite() {
            return Ok(Value::double(round_exact(input, precision as usize)));
        }
        Ok(Value::double(round_double(input, precision, f64::round)))
    })
}

fn round_even_precision(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let precision = row[1].as_i128().expect("an INTEGER");
        Ok(Value::double(round_double(float(&row[0]), precision, round_half_even)))
    })
}

fn trunc_precision(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let precision = row[1].as_i128().expect("an INTEGER");
        Ok(Value::double(round_double(float(&row[0]), precision, f64::trunc)))
    })
}

fn round_even_unary(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::double(round_half_even(float(&row[0])))))
}

/// `RoundIntegerOperator`: only a negative precision changes an integer.
fn round_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let value = row[0].as_i128().expect("an integer");
        let precision = row[1].as_i128().expect("an INTEGER");
        if precision >= 0 {
            return Ok(row[0].clone());
        }
        if precision < -38 {
            return Value::new(ty.clone(), ValueData::Int64(0));
        }
        let power = 10i128.pow((-precision) as u32);
        let half = power / 2;
        let rounded = if value >= 0 {
            (value + half) / power * power
        } else {
            (value - half) / power * power
        };
        i64::try_from(rounded)
            .map_err(|_| out_of_range("Overflow in round of integer"))
            .and_then(|v| Value::new(ty.clone(), ValueData::Int64(v)))
    })
}

fn gcd_of(left: i128, right: i128) -> i128 {
    let (mut a, mut b) = (left.abs(), right.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn gcd(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (a, b) = (row[0].as_i128().expect("a BIGINT"), row[1].as_i128().expect("a BIGINT"));
        Ok(Value::bigint(gcd_of(a, b) as i64))
    })
}

fn lcm(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (a, b) = (row[0].as_i128().expect("a BIGINT"), row[1].as_i128().expect("a BIGINT"));
        if a == 0 || b == 0 {
            return Ok(Value::bigint(0));
        }
        let result = (a / gcd_of(a, b) * b).abs();
        i64::try_from(result)
            .map(Value::bigint)
            .map_err(|_| out_of_range("lcm value is out of range"))
    })
}

pub fn math_functions() -> Vec<ScalarFunctionSet> {
    let mut sets: Vec<ScalarFunctionSet> = [
        ("sqrt", sqrt as ScalarCallback),
        ("cbrt", cbrt),
        ("exp", exp),
        ("sin", sin),
        ("cos", cos),
        ("tan", tan),
        ("cot", cot),
        ("asin", asin),
        ("acos", acos),
        ("atan", atan),
        ("sinh", sinh),
        ("cosh", cosh),
        ("tanh", tanh),
        ("asinh", asinh),
        ("acosh", acosh),
        ("atanh", atanh),
        ("degrees", degrees),
        ("radians", radians),
        ("ln", ln),
        ("log10", log10),
        ("log2", log2),
        ("log", log10),
        ("isnan", is_nan),
        ("isinf", is_inf),
        ("isfinite", is_finite),
        ("round_even", round_even_unary),
        ("roundbankers", round_even_unary),
    ]
    .into_iter()
    .map(|(name, callback)| double_set(name, callback))
    .collect();
    for (name, callback) in [
        ("atan2", atan2 as ScalarCallback),
        ("pow", pow),
        ("power", pow),
        ("**", pow),
        ("^", pow),
        ("log", log_base),
    ] {
        sets.push(binary_set(name, callback));
    }
    let mut pi_set = ScalarFunctionSet::new("pi");
    pi_set.add(ScalarFunction::new("pi", vec![], LogicalType::DOUBLE, pi));
    sets.push(pi_set);
    let mut round = ScalarFunctionSet::new("round");
    for ty in [LogicalType::BIGINT, LogicalType::HUGEINT] {
        round.add(ScalarFunction::new(
            "round",
            vec![ty.clone(), LogicalType::INTEGER],
            ty,
            round_integer,
        ));
    }
    round.add(ScalarFunction::new(
        "round",
        vec![LogicalType::DOUBLE, LogicalType::INTEGER],
        LogicalType::DOUBLE,
        round_precision,
    ));
    sets.push(round);
    let mut trunc = ScalarFunctionSet::new("trunc");
    trunc.add(ScalarFunction::new(
        "trunc",
        vec![LogicalType::DOUBLE, LogicalType::INTEGER],
        LogicalType::DOUBLE,
        trunc_precision,
    ));
    sets.push(trunc);
    for name in ["round_even", "roundbankers"] {
        let mut set = ScalarFunctionSet::new(name);
        set.add(ScalarFunction::new(
            name,
            vec![LogicalType::DOUBLE, LogicalType::INTEGER],
            LogicalType::DOUBLE,
            round_even_precision,
        ));
        sets.push(set);
    }
    for (names, callback) in [
        (["gcd", "greatest_common_divisor"], gcd as ScalarCallback),
        (["lcm", "least_common_multiple"], lcm),
    ] {
        for name in names {
            let mut set = ScalarFunctionSet::new(name);
            set.add(ScalarFunction::new(
                name,
                vec![LogicalType::BIGINT, LogicalType::BIGINT],
                LogicalType::BIGINT,
                callback,
            ));
            sets.push(set);
        }
    }
    sets
}
