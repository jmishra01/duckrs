// Ported from duckdb/extension/core_functions/scalar/math/numeric.cpp @ rust-port-baseline
//
//! The math functions. They run row by row; the vectorized loops come with Phase 13.

use super::binder::constant_parameter;
use super::bit_functions::strict_rows;
use super::nested::bound_function;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{LogicalType, LogicalTypeId};
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
            "input value {value:.6} is out of range for numeric function"
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
// `NoInfiniteNoZeroDoubleWrapper` over `CoTanOperator`
unary!(cot, x => {
    let x = finite(x)?;
    if x == 0.0 {
        return Err(out_of_range(format!(
            "input value {x:.6} is out of range for numeric function cotangent"
        )));
    }
    1.0 / x.tan()
});
unary!(asin, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(Error::new(ErrorKind::InvalidInput, "ASIN is undefined outside [-1,1]"));
    }
    x.asin()
});
unary!(acos, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(Error::new(ErrorKind::InvalidInput, "ACOS is undefined outside [-1,1]"));
    }
    x.acos()
});
unary!(atan, x => x.atan());
unary!(sinh, x => x.sinh());
unary!(cosh, x => x.cosh());
unary!(tanh, x => x.tanh());
unary!(asinh, x => x.asinh());
// `AcoshOperator`: NaN below 1
unary!(acosh, x => x.acosh());
unary!(atanh, x => {
    if !(-1.0..=1.0).contains(&x) {
        return Err(Error::new(ErrorKind::InvalidInput, "ATANH is undefined outside [-1,1]"));
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
            return Err(out_of_range("division by zero in based logarithm"));
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecimalRounding {
    Round,
    RoundEven,
    Trunc,
}

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

/// `ProductError`: the error of the rounded product `a * b`, by Dekker's algorithm.
fn product_error(a: f64, b: f64, product: f64) -> f64 {
    const SPLITTER: f64 = 134_217_729.0; // 2^27 + 1
    let a_scaled = SPLITTER * a;
    let a_high = a_scaled - (a_scaled - a);
    let a_low = a - a_high;
    let b_scaled = SPLITTER * b;
    let b_high = b_scaled - (b_scaled - b);
    let b_low = b - b_high;
    ((a_high * b_high - product) + a_high * b_low + a_low * b_high) + a_low * b_low
}

/// `RoundOperatorPrecision`. With `exact_midpoints`, a product that rounds onto a midpoint the
/// input is not on (2.675 * 100 is 267.5, the stored 2.675 is below it) goes the way of the exact
/// product.
fn round_double(input: f64, precision: i128, nearest: fn(f64) -> f64, exact_midpoints: bool) -> f64 {
    if precision < 0 {
        let modifier = 10f64.powi((-precision).min(400) as i32);
        if modifier.is_infinite() {
            return if input.is_finite() { 0.0 } else { input };
        }
        let rounded = nearest(input / modifier) * modifier;
        if rounded.is_finite() { rounded } else { input }
    } else {
        let modifier = 10f64.powi(precision.min(400) as i32);
        let scaled = input * modifier;
        let mut error = 0.0;
        if exact_midpoints && (1..=22).contains(&precision) && (scaled - scaled.trunc()).abs() == 0.5 {
            error = product_error(input, modifier, scaled);
        }
        let rounded_scaled = if error != 0.0 {
            if error < 0.0 { scaled.floor() } else { scaled.ceil() }
        } else {
            nearest(scaled)
        };
        let rounded = rounded_scaled / modifier;
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
        Ok(Value::double(round_double(input, precision, f64::round, true)))
    })
}

fn round_even_precision(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let precision = row[1].as_i128().expect("an INTEGER");
        Ok(Value::double(round_double(
            float(&row[0]),
            precision,
            round_half_even,
            true,
        )))
    })
}

fn trunc_precision(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let precision = row[1].as_i128().expect("an INTEGER");
        Ok(Value::double(round_double(
            float(&row[0]),
            precision,
            f64::trunc,
            false,
        )))
    })
}

/// `RoundDivide`: `input / power`, rounded away from zero from one half up. With `even`, a tie
/// goes to the even quotient.
fn round_divide(input: i128, power: i128, even: bool) -> i128 {
    let (quotient, remainder) = (input / power, (input % power).abs());
    let half = power / 2;
    let away = remainder > half || (remainder == half && (!even || quotient % 2 != 0));
    if !away {
        quotient
    } else if input < 0 {
        quotient - 1
    } else {
        quotient + 1
    }
}

/// `DecimalRoundNegativePrecisionOperator` and `DecimalRoundPositivePrecisionOperator`, and the
/// same for `trunc`.
fn decimal_round_row(row: &[Value], result: &LogicalType, policy: DecimalRounding) -> Result<Value> {
    let (width, scale) = row[0].logical_type().decimal_properties().expect("a DECIMAL");
    let (width, scale) = (i32::from(width), i32::from(scale));
    let value = row[0].as_i128().expect("a DECIMAL is stored as an integer");
    let target = row[1].as_i128().expect("an INTEGER") as i32;
    let divide = |input: i128, power: i128| match policy {
        DecimalRounding::Round => round_divide(input, power, false),
        DecimalRounding::RoundEven => round_divide(input, power, true),
        DecimalRounding::Trunc => input / power,
    };
    let (result_width, result_scale) = result.decimal_properties().expect("a DECIMAL");
    let rounded = if target < 0 {
        let too_big = match policy {
            DecimalRounding::Trunc => target <= -(width - scale),
            _ => target < -(width - scale),
        };
        if too_big {
            0
        } else {
            let name = if policy == DecimalRounding::RoundEven {
                "ROUND_EVEN"
            } else {
                "ROUND"
            };
            let overflow = || Error::new(ErrorKind::OutOfRange, format!("Overflow in {name} of DECIMAL(38)"));
            let rounded = divide(value, 10i128.pow((scale - target) as u32))
                .checked_mul(10i128.pow((-target) as u32))
                .ok_or_else(overflow)?;
            if rounded.unsigned_abs() >= 10u128.pow(38) {
                return Err(overflow());
            }
            rounded
        }
    } else if target >= scale {
        value
    } else {
        divide(value, 10i128.pow((scale - target) as u32))
    };
    Value::decimal(rounded, result_width, result_scale)
}

fn decimal_round(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| decimal_round_row(row, &ty, DecimalRounding::Round))
}

fn decimal_round_even(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        decimal_round_row(row, &ty, DecimalRounding::RoundEven)
    })
}

fn decimal_trunc(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| decimal_round_row(row, &ty, DecimalRounding::Trunc))
}

/// `ResolveDecimalRoundPrecision`: `round(DECIMAL, precision)` stays a DECIMAL, of a scale that
/// depends on the constant precision. `None` when the call is not such a call.
pub fn bind_decimal_round(name: &str, children: &[BoundExpr]) -> Result<Option<BoundExpr>> {
    let [decimal, precision] = children else {
        return Ok(None);
    };
    let Some((width, scale)) = decimal.return_type.decimal_properties() else {
        return Ok(None);
    };
    let (policy, callback): (DecimalRounding, ScalarCallback) = match name {
        "round" => (DecimalRounding::Round, decimal_round),
        "round_even" | "roundbankers" => (DecimalRounding::RoundEven, decimal_round_even),
        "trunc" => (DecimalRounding::Trunc, decimal_trunc),
        _ => return Ok(None),
    };
    // an untyped NULL folds the call before its bind runs
    if precision.return_type.id() == LogicalTypeId::SqlNull {
        return Ok(Some(BoundExpr::constant(Value::sql_null())));
    }
    if !precision.is_foldable() {
        return Err(Error::new(
            ErrorKind::Binder,
            format!(
                "No function matches the given name and argument types '{name}({}, {})'. You might need to add \
                 explicit type casts.",
                decimal.return_type, precision.return_type
            ),
        ));
    }
    let Some(value) = constant_parameter(name, precision, 1, false)? else {
        return Ok(None);
    };
    let value = evaluate_scalar(&BoundExpr::constant(value).cast_to(&LogicalType::INTEGER, false))?;
    let target = value.as_i128().expect("an INTEGER") as i32;
    let mut result_width = width;
    let result_scale = if target < 0 {
        let can_carry = policy != DecimalRounding::Trunc;
        if can_carry && scale == 0 && target >= -i32::from(width) && result_width < 38 {
            result_width += 1;
        }
        0
    } else if target >= i32::from(scale) {
        scale
    } else {
        target as u8
    };
    let return_type = LogicalType::decimal(result_width, result_scale);
    let arguments = vec![decimal.clone(), BoundExpr::constant(value)];
    Ok(Some(bound_function(name, arguments, return_type, callback)))
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
        // `Hugeint::CACHED_POWERS_OF_TEN`
        if precision <= -39 {
            return super::cast::integer_value(0, &ty);
        }
        let power = 10i128.pow((-precision) as u32);
        let half = power / 2;
        let (mut rounded, remainder) = (value / power, value % power);
        if remainder >= half {
            rounded += 1;
        } else if remainder <= -half {
            rounded -= 1;
        }
        let overflow = || out_of_range("Overflow in round of integer");
        let rounded = rounded.checked_mul(power).ok_or_else(overflow)?;
        let (min, max) = match ty.id() {
            LogicalTypeId::TinyInt => (i128::from(i8::MIN), i128::from(i8::MAX)),
            LogicalTypeId::SmallInt => (i128::from(i16::MIN), i128::from(i16::MAX)),
            LogicalTypeId::Integer => (i128::from(i32::MIN), i128::from(i32::MAX)),
            LogicalTypeId::BigInt => (i128::from(i64::MIN), i128::from(i64::MAX)),
            _ => (i128::MIN, i128::MAX),
        };
        if !(min..=max).contains(&rounded) {
            return Err(overflow());
        }
        super::cast::integer_value(rounded, &ty)
    })
}

/// `TryAbsOperator`: the minimum of the type has no absolute value.
fn try_abs(value: i128, minimum: i128) -> Result<i128> {
    if value == minimum {
        return Err(out_of_range(format!("Overflow on abs({value})")));
    }
    Ok(value.abs())
}

/// `GreatestCommonDivisor<T>`, for the type whose minimum is `minimum`.
fn typed_gcd(left: i128, right: i128, minimum: i128) -> Result<i128> {
    if (left == minimum && right == -1) || (left == -1 && right == minimum) {
        return Ok(1);
    }
    let (mut a, mut b) = (left, right);
    loop {
        if a == 0 {
            return try_abs(b, minimum);
        }
        b %= a;
        if b == 0 {
            return try_abs(a, minimum);
        }
        a %= b;
    }
}

/// The range of the BIGINT or HUGEINT the function gives.
fn integer_range(result: &Vector) -> (i128, i128) {
    if result.logical_type().id() == duckrs_common::types::LogicalTypeId::HugeInt {
        (i128::MIN, i128::MAX)
    } else {
        (i128::from(i64::MIN), i128::from(i64::MAX))
    }
}

fn integer_value(result_type: &LogicalType, value: i128) -> Value {
    if result_type.id() == duckrs_common::types::LogicalTypeId::HugeInt {
        Value::hugeint(value)
    } else {
        Value::bigint(value as i64)
    }
}

fn gcd(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let (minimum, _) = integer_range(result);
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let (a, b) = (
            row[0].as_i128().expect("an integer"),
            row[1].as_i128().expect("an integer"),
        );
        Ok(integer_value(&ty, typed_gcd(a, b, minimum)?))
    })
}

/// `LeastCommonMultipleOperator`.
fn lcm(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let (minimum, maximum) = integer_range(result);
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let (a, b) = (
            row[0].as_i128().expect("an integer"),
            row[1].as_i128().expect("an integer"),
        );
        if a == 0 || b == 0 {
            return Ok(integer_value(&ty, 0));
        }
        let product = a
            .checked_mul(b / typed_gcd(a, b, minimum)?)
            .filter(|p| (minimum..=maximum).contains(p))
            .ok_or_else(|| out_of_range("lcm value is out of range"))?;
        Ok(integer_value(&ty, try_abs(product, minimum)?))
    })
}

unary!(ieee_sqrt, x => x.sqrt());
unary!(ieee_sin, x => x.sin());
unary!(ieee_cos, x => x.cos());
unary!(ieee_tan, x => x.tan());
unary!(ieee_cot, x => 1.0 / x.tan());
unary!(ieee_asin, x => x.asin());
unary!(ieee_acos, x => x.acos());
unary!(ieee_atanh, x => x.atanh());
unary!(ieee_ln, x => x.ln());
unary!(ieee_log10, x => x.log10());
unary!(ieee_log2, x => x.log2());

fn ieee_log_base(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (base, x) = (float(&row[0]), float(&row[1]));
        Ok(Value::double(x.log10() / base.log10()))
    })
}

fn ieee_pow(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::double(float(&row[0]).powf(float(&row[1]))))
    })
}

// `IEEEGammaOperator` and `IEEELogGammaOperator`
unary!(ieee_gamma, x => libm::tgamma(x));
unary!(ieee_lgamma, x => libm::lgamma(x));

/// `BindIEEEFloatingUnary` / `BindIEEEFloatingBinary`: with `ieee_floating_point_ops` on, the function of this name
/// and arity gives NaN and infinities where it would otherwise raise.
pub fn ieee_variant(name: &str, arity: usize) -> Option<ScalarCallback> {
    Some(match (name, arity) {
        ("sqrt", 1) => ieee_sqrt,
        ("sin", 1) => ieee_sin,
        ("cos", 1) => ieee_cos,
        ("tan", 1) => ieee_tan,
        ("cot", 1) => ieee_cot,
        ("asin", 1) => ieee_asin,
        ("acos", 1) => ieee_acos,
        ("atanh", 1) => ieee_atanh,
        ("ln", 1) => ieee_ln,
        ("log10" | "log", 1) => ieee_log10,
        ("log2", 1) => ieee_log2,
        ("log", 2) => ieee_log_base,
        ("pow" | "power" | "**" | "^", 2) => ieee_pow,
        ("gamma", 1) => ieee_gamma,
        ("lgamma", 1) => ieee_lgamma,
        _ => return None,
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
    for ty in [
        LogicalType::TINYINT,
        LogicalType::SMALLINT,
        LogicalType::INTEGER,
        LogicalType::BIGINT,
        LogicalType::HUGEINT,
    ] {
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
            for ty in [LogicalType::BIGINT, LogicalType::HUGEINT] {
                set.add(ScalarFunction::new(name, vec![ty.clone(), ty.clone()], ty, callback));
            }
            sets.push(set);
        }
    }
    sets
}
