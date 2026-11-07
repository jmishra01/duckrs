// Ported from the semantics of duckdb/extension/core_functions/aggregate/**/*.cpp
// @ rust-port-baseline
//
//! The aggregates that keep their rows. C++ has a state per aggregate, and sketches for the
//! approximate ones; these gather the values and compute the result at the end, which is exact
//! and easy to get right, and costs memory until Phase 13 brings the states.

use super::aggregates::value_keys;
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{Interval, LogicalType, LogicalTypeId, bit};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holistic {
    StringAgg,
    BoolAnd,
    BoolOr,
    BitAnd,
    BitOr,
    BitXor,
    Product,
    Mode,
    Median,
    QuantileCont,
    QuantileDisc,
    Mad,
    StddevSamp,
    StddevPop,
    VarSamp,
    VarPop,
    Skewness,
    Kurtosis,
    KurtosisPop,
    Sem,
    AvgInterval,
    AvgTimestamp,
    Entropy,
    ArgMin,
    ArgMax,
    /// `arg_min_null`: the row of the extreme, whether its value is NULL or not
    ArgMinNull,
    ArgMaxNull,
    /// `arg_min_nulls_last`: NULL values only if nothing else
    ArgMinNullsLast,
    ArgMaxNullsLast,
    /// `min(x, n)`
    MinN,
    MaxN,
    ArgMinN,
    ArgMaxN,
    ApproxTopK,
    BitstringAgg,
    WeightedAvg,
    /// `combine_aggr`: a state of the states of its rows
    CombineAggr,
    CountIf,
    Histogram,
    ApproxCountDistinct,
    Fsum,
    Favg,
    GeoMean,
    Corr,
    CovarPop,
    CovarSamp,
    RegrCount,
    RegrSlope,
    RegrIntercept,
    RegrR2,
    RegrAvgX,
    RegrAvgY,
    RegrSxx,
    RegrSyy,
    RegrSxy,
}

/// The type of `quantile_cont` and `median` of a column of type `input`.
fn interpolated_type(input: &LogicalType) -> LogicalType {
    match input.id() {
        LogicalTypeId::Date => LogicalType::TIMESTAMP,
        LogicalTypeId::Float | LogicalTypeId::Double | LogicalTypeId::Decimal | LogicalTypeId::Timestamp => {
            input.clone()
        }
        LogicalTypeId::TimestampTz | LogicalTypeId::Time | LogicalTypeId::Interval => input.clone(),
        id if LogicalType::new(id).is_integral() => LogicalType::DOUBLE,
        _ => input.clone(),
    }
}

impl Holistic {
    /// The return type for arguments of these types.
    pub fn return_type(self, declared: &LogicalType, arguments: &[LogicalType]) -> LogicalType {
        use Holistic::*;
        let input = arguments.first();
        let quantile = |base: LogicalType| match arguments.get(1).map(LogicalType::id) {
            Some(LogicalTypeId::List | LogicalTypeId::Array) => LogicalType::list(base),
            _ => base,
        };
        match (self, input) {
            (Median, Some(ty)) => interpolated_type(ty),
            (QuantileCont, Some(ty)) => quantile(interpolated_type(ty)),
            (QuantileDisc, Some(ty)) => quantile(ty.clone()),
            (Histogram, Some(ty)) => LogicalType::map(ty.clone(), LogicalType::UBIGINT),
            (
                Mode | BitAnd | BitOr | BitXor | ArgMin | ArgMax | ArgMinNull | ArgMaxNull | ArgMinNullsLast
                | ArgMaxNullsLast | AvgTimestamp,
                Some(ty),
            ) => ty.clone(),
            (CombineAggr, Some(ty)) => ty.clone(),
            (MinN | MaxN | ArgMinN | ArgMaxN | ApproxTopK, Some(ty)) => LogicalType::list(ty.clone()),
            _ => declared.clone(),
        }
    }
}

fn out_of_range(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

fn number(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Null => None,
        ValueData::Double(v) => Some(*v),
        ValueData::Float(v) => Some(f64::from(*v)),
        _ if value.logical_type().id() == LogicalTypeId::Decimal => value.try_to_string().ok()?.parse().ok(),
        _ => value.as_i128().map(|v| v as f64),
    }
}

fn numbers(rows: &[Value]) -> Vec<f64> {
    rows.iter().filter_map(number).collect()
}

fn cast(value: Value, ty: &LogicalType) -> Result<Value> {
    if value.logical_type() == ty {
        return Ok(value);
    }
    evaluate_scalar(&BoundExpr::constant(value).cast_to(ty, false))
}

/// The members of a row of an aggregate of several arguments, which arrive as a STRUCT.
fn fields(row: &Value) -> &[Value] {
    match row.data() {
        ValueData::Struct(fields) => fields,
        _ => std::slice::from_ref(row),
    }
}

/// Ascending sort keys of `values`, all of type `ty`.
fn sort_keys(values: &[Value], ty: &LogicalType) -> Result<Vec<Vec<u8>>> {
    let mut vector = Vector::new(ty.clone(), values.len().max(1));
    for (row, value) in values.iter().enumerate() {
        vector.set_value(row, value)?;
    }
    value_keys(&vector, values.len())
}

fn sorted(values: Vec<Value>) -> Result<Vec<Value>> {
    let Some(first) = values.first() else {
        return Ok(values);
    };
    let keys = sort_keys(&values, &first.logical_type().clone())?;
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
    Ok(order.into_iter().map(|at| values[at].clone()).collect())
}

/// `InterpolateInt64`.
fn interpolate_i64(lo: i64, d: f64, hi: i64) -> i64 {
    let (lower, upper) = (lo.min(hi), lo.max(hi));
    let offset = ((i128::from(hi) - i128::from(lo)) as f64 * d).round() as i128;
    (i128::from(lo) + offset).clamp(i128::from(lower), i128::from(upper)) as i64
}

/// `Interval::GetMicro`.
fn interval_micros(interval: &Interval) -> Result<i64> {
    let per_day = Interval::MICROS_PER_DAY;
    let conversion = |from: &str| {
        Error::new(
            ErrorKind::Conversion,
            format!("Could not convert {from} to Microseconds"),
        )
    };
    let months = i64::from(interval.months)
        .checked_mul(Interval::DAYS_PER_MONTH * per_day)
        .ok_or_else(|| conversion("Month"))?;
    let days = i64::from(interval.days)
        .checked_mul(per_day)
        .ok_or_else(|| conversion("Day"))?;
    months
        .checked_add(days)
        .and_then(|sum| sum.checked_add(interval.micros))
        .ok_or_else(|| conversion("Interval"))
}

/// `InterpolateOperator`: the value a fraction `d` of the way from `lo` to `hi`, both of type `ty`.
fn interpolate(lo: &Value, d: f64, hi: &Value, ty: &LogicalType) -> Result<Value> {
    match (lo.data(), hi.data()) {
        (ValueData::Interval(lo), ValueData::Interval(hi)) => {
            let (lo, hi) = (i128::from(interval_micros(lo)?), i128::from(interval_micros(hi)?));
            let micros = lo + ((hi - lo) as f64 * d).round() as i128;
            let per_day = i128::from(Interval::MICROS_PER_DAY);
            Ok(Value::interval(Interval::new(
                0,
                (micros / per_day) as i32,
                (micros % per_day) as i64,
            )))
        }
        (ValueData::Int64(a), ValueData::Int64(b)) if ty.id() != LogicalTypeId::Double => {
            if a == b {
                return Ok(lo.clone());
            }
            if matches!(
                ty.id(),
                LogicalTypeId::Timestamp
                    | LogicalTypeId::TimestampTz
                    | LogicalTypeId::TimestampNs
                    | LogicalTypeId::TimestampMs
                    | LogicalTypeId::TimestampSec
            ) && [a, b].iter().any(|v| v.abs() == i64::MAX)
            {
                return Err(Error::new(
                    ErrorKind::Conversion,
                    "Cannot interpolate infinite TIMESTAMP values",
                ));
            }
            Value::new(ty.clone(), ValueData::Int64(interpolate_i64(*a, d, *b)))
        }
        _ if ty.id() == LogicalTypeId::Decimal => {
            // `InterpolateOperator`: over the integers a DECIMAL stores, and truncating
            let (Some(a), Some(b)) = (lo.as_i128(), hi.as_i128()) else {
                return Ok(lo.clone());
            };
            let (width, scale) = ty.decimal_properties().expect("a DECIMAL");
            Value::decimal((a as f64 + (b as f64 - a as f64) * d) as i128, width, scale)
        }
        _ => {
            let (Some(a), Some(b)) = (number(lo), number(hi)) else {
                return Ok(lo.clone());
            };
            let mixed = if ty.id() == LogicalTypeId::Double {
                a * (1.0 - d) + b * d
            } else {
                a + (b - a) * d
            };
            cast(Value::double(mixed), ty)
        }
    }
}

/// `QuantileInterpolator<true>::Index`: `ceil(n * q)`, snapped to an integer that the rounding of
/// `q` missed.
fn discrete_index(count: usize, q: f64) -> usize {
    let n = count as f64;
    let scaled = n * q;
    let nearest = scaled.round();
    let tolerance = n * 4.0 * f64::EPSILON;
    let ceiled = if (scaled - nearest).abs() <= tolerance {
        nearest
    } else {
        scaled.ceil()
    };
    (ceiled as usize).max(1) - 1
}

/// `QuantileInterpolator`: the value at fraction `q` of `values`, sorted in the quantile's order.
fn quantile(values: &[Value], q: f64, continuous: bool, ty: &LogicalType) -> Result<Value> {
    if !continuous {
        return cast(
            values[discrete_index(values.len(), q).min(values.len() - 1)].clone(),
            ty,
        );
    }
    let rank = (values.len() as f64 - 1.0) * q;
    let (low, high) = (rank.floor(), rank.ceil());
    let lo = cast(values[low as usize].clone(), ty)?;
    if low == high {
        return Ok(lo);
    }
    let hi = cast(values[high as usize].clone(), ty)?;
    interpolate(&lo, rank - low, &hi, ty)
}

/// `NegatePercentileFractions`: a descending ordered-set aggregate is a negative fraction. The
/// sign of a zero fraction has to survive too.
pub fn negate_fractions(parameter: &Value, descending: bool) -> Result<Value> {
    let negate = |value: &Value| match number(value) {
        Some(fraction) if !(0.0..=1.0).contains(&fraction) => Err(Error::new(
            ErrorKind::Binder,
            "PERCENTILEs can only take parameters in the range [0, 1]",
        )),
        Some(fraction) if descending => Ok(Value::double(-fraction)),
        _ => Ok(value.clone()),
    };
    match parameter.data() {
        ValueData::List(items) => {
            let negated = items.iter().map(negate).collect::<Result<Vec<_>>>()?;
            let child = negated
                .first()
                .map_or(LogicalType::DOUBLE, |v| v.logical_type().clone());
            Value::new(LogicalType::list(child), ValueData::List(negated))
        }
        _ => negate(parameter),
    }
}

/// `CheckQuantile` over a constant parameter of a quantile aggregate.
pub fn check_quantile_parameter(parameter: &Value) -> Result<()> {
    let items = match parameter.data() {
        ValueData::List(items) => items.as_slice(),
        _ => std::slice::from_ref(parameter),
    };
    let (mut ascending, mut descending) = (false, false);
    for item in items {
        let Some(fraction) = number(item) else {
            return Err(Error::new(ErrorKind::Binder, "QUANTILE parameter cannot be NULL"));
        };
        if !(-1.0..=1.0).contains(&fraction) && !fraction.is_nan() {
            return Err(Error::new(
                ErrorKind::Binder,
                "QUANTILE can only take parameters in the range [-1, 1]",
            ));
        }
        if fraction.is_nan() {
            return Err(Error::new(ErrorKind::Binder, "QUANTILE parameter cannot be NaN"));
        }
        ascending |= fraction > 0.0;
        descending |= fraction.is_sign_negative();
    }
    if ascending && descending {
        return Err(Error::new(
            ErrorKind::Binder,
            "QUANTILE parameters must have consistent signs",
        ));
    }
    Ok(())
}

/// The fractions of a quantile, whether they are a list, and whether the order is descending.
fn fractions(row: &Value) -> Result<(Vec<f64>, bool, bool)> {
    let parameter = fields(row).get(1).cloned().unwrap_or_else(|| Value::double(0.5));
    let (items, is_list) = match parameter.data() {
        ValueData::List(items) => (items.clone(), true),
        _ => (vec![parameter.clone()], false),
    };
    check_quantile_parameter(&parameter)?;
    let fractions: Vec<f64> = items.iter().filter_map(number).collect();
    let descending = fractions.iter().any(|q| q.is_sign_negative());
    Ok((fractions.iter().map(|q| q.abs()).collect(), is_list, descending))
}

/// `QuantileInterpolator<true>::Index` on a DECIMAL fraction computes `n * 10^scale` with an
/// overflow check, which a scale above 18 can trip.
fn check_decimal_rank(row: &Value, count: usize) -> Result<()> {
    let Some(parameter) = fields(row).get(1) else {
        return Ok(());
    };
    let parameter = match parameter.data() {
        ValueData::List(items) if !items.is_empty() => &items[0],
        _ => parameter,
    };
    let Some((_, scale)) = parameter.logical_type().decimal_properties() else {
        return Ok(());
    };
    let limit = 10i128.pow(38);
    match (count as i128).checked_mul(10i128.pow(scale.into())) {
        Some(scaled) if scaled < limit => Ok(()),
        _ => Err(out_of_range(format!(
            "Overflow in multiplication of DECIMAL(38) ({count} * {})",
            10i128.pow(scale.into())
        ))),
    }
}

fn quantiles(rows: &[Value], ty: &LogicalType, continuous: bool) -> Result<Value> {
    let Some(first) = rows.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let (fractions, is_list, descending) = fractions(first)?;
    let values: Vec<Value> = rows
        .iter()
        .map(|row| fields(row)[0].clone())
        .filter(|v| !v.is_null())
        .collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    if !continuous {
        check_decimal_rank(first, values.len())?;
    }
    let mut values = sorted(values)?;
    if descending {
        values.reverse();
    }
    if is_list {
        let child = ty.child_type().expect("a list of quantiles");
        let items = fractions
            .iter()
            .map(|q| quantile(&values, *q, continuous, child))
            .collect::<Result<Vec<_>>>()?;
        return Value::new(ty.clone(), ValueData::List(items));
    }
    quantile(&values, fractions[0], continuous, ty)
}

fn median_of(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

/// The count, mean and central moments up to the 4th.
fn moments(values: &[f64]) -> (f64, f64, f64, f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let central = |power: i32| values.iter().map(|v| (v - mean).powi(power)).sum::<f64>();
    (n, mean, central(2), central(3), central(4))
}

fn checked(name: &str, value: f64) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::double(value))
    } else {
        Err(out_of_range(format!("{name} is out of range!")))
    }
}

/// The pairs of an aggregate of (y, x), without those with a NULL.
fn pairs(rows: &[Value]) -> Vec<(f64, f64)> {
    rows.iter()
        .filter_map(|row| {
            let values = fields(row);
            Some((number(values.first()?)?, number(values.get(1)?)?))
        })
        .collect()
}

struct Regression {
    n: f64,
    y: f64,
    x: f64,
    sxx: f64,
    syy: f64,
    sxy: f64,
}

fn regression(pairs: &[(f64, f64)]) -> Regression {
    let n = pairs.len() as f64;
    let y = pairs.iter().map(|p| p.0).sum::<f64>() / n;
    let x = pairs.iter().map(|p| p.1).sum::<f64>() / n;
    Regression {
        n,
        y,
        x,
        sxx: pairs.iter().map(|p| (p.1 - x).powi(2)).sum(),
        syy: pairs.iter().map(|p| (p.0 - y).powi(2)).sum(),
        sxy: pairs.iter().map(|p| (p.1 - x) * (p.0 - y)).sum(),
    }
}

fn arg_extreme(rows: &[Value], ty: &LogicalType, greatest: bool) -> Result<Value> {
    let chosen: Vec<&Value> = rows
        .iter()
        .filter(|row| !fields(row)[0].is_null() && !fields(row)[1].is_null())
        .collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[1].clone()).collect();
    let keys = sort_keys(&by, fields(first)[1].logical_type())?;
    let mut best = 0;
    for at in 1..keys.len() {
        let better = if greatest {
            keys[at] > keys[best]
        } else {
            keys[at] < keys[best]
        };
        if better {
            best = at;
        }
    }
    Ok(fields(chosen[best])[0].clone())
}

/// `arg_min_null` and `arg_min_nulls_last`: the rows with a key; the value may be NULL.
fn arg_extreme_nulls(rows: &[Value], ty: &LogicalType, greatest: bool, nulls_last: bool) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[1].is_null()).collect();
    let Some(first) = chosen.first() else {
        // NULL keys come last, so one of them is what is left
        return Ok(match rows.first() {
            Some(row) if nulls_last => fields(row)[0].clone(),
            _ => Value::null(ty.clone()),
        });
    };
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[1].clone()).collect();
    let keys = sort_keys(&by, fields(first)[1].logical_type())?;
    let mut best = 0;
    for at in 1..keys.len() {
        if (greatest && keys[at] > keys[best]) || (!greatest && keys[at] < keys[best]) {
            best = at;
        }
    }
    Ok(fields(chosen[best])[0].clone())
}

fn positive_n(value: &Value, name: &str) -> Result<usize> {
    let n = value.as_i128().unwrap_or(0);
    if n <= 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid input for {name}: n value must be > 0"),
        ));
    }
    if n >= 1_000_000 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid input for {name}: n value must be < 1000000"),
        ));
    }
    Ok(n as usize)
}

/// `min(x, n)` and `arg_min(a, b, n)`: the `n` values with the smallest keys, smallest first.
fn extremes(rows: &[Value], ty: &LogicalType, greatest: bool, value: usize, key: usize, count: usize) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[key].is_null()).collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let name = if value == key { "MIN/MAX" } else { "arg_min/arg_max" };
    let n = positive_n(&fields(first)[count], name)?;
    let by: Vec<Value> = chosen.iter().map(|row| fields(row)[key].clone()).collect();
    let keys = sort_keys(&by, fields(first)[key].logical_type())?;
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|a, b| {
        if greatest {
            keys[*b].cmp(&keys[*a])
        } else {
            keys[*a].cmp(&keys[*b])
        }
    });
    let items = order
        .into_iter()
        .take(n)
        .map(|at| fields(chosen[at])[value].clone())
        .collect();
    Value::new(ty.clone(), ValueData::List(items))
}

/// `approx_top_k`, exactly: the most frequent values.
fn top_k(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[0].is_null()).collect();
    let Some(first) = chosen.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let Some(k) = fields(first)[1].as_i128() else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value cannot be NULL",
        ));
    };
    if k <= 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value must be > 0",
        ));
    }
    if k >= 1_000_000 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid input for approx_top_k: k value must be < 1000000",
        ));
    }
    let values: Vec<Value> = chosen.iter().map(|row| fields(row)[0].clone()).collect();
    let keys = sort_keys(&values, values[0].logical_type())?;
    let mut counts: HashMap<&[u8], (usize, usize)> = HashMap::new();
    for (at, key) in keys.iter().enumerate() {
        counts.entry(key.as_slice()).or_insert((0, at)).0 += 1;
    }
    let mut ranked: Vec<(usize, usize)> = counts.into_values().collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let items = ranked
        .into_iter()
        .take(k as usize)
        .map(|(_, at)| values[at].clone())
        .collect();
    Value::new(ty.clone(), ValueData::List(items))
}

/// `bitstring_agg(x, min, max)`: a BIT with a bit set for each value, from `min` to `max`.
fn bitstring(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let numbers: Vec<(i128, Option<(i128, i128)>)> = rows
        .iter()
        .filter(|row| !fields(row)[0].is_null())
        .map(|row| {
            let parts = fields(row);
            let range = match parts {
                [_, low, high] => low.as_i128().zip(high.as_i128()),
                _ => None,
            };
            (parts[0].as_i128().unwrap_or(0), range)
        })
        .collect();
    let Some((_, given)) = numbers.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let (low, high) = match given {
        Some(range) => *range,
        None => (
            numbers.iter().map(|n| n.0).min().unwrap_or(0),
            numbers.iter().map(|n| n.0).max().unwrap_or(0),
        ),
    };
    let size = high - low + 1;
    if !(1..=100_000_000).contains(&size) {
        return Err(out_of_range(format!(
            "The range between min and max value ({low} <-> {high}) is too large for bitstring aggregation"
        )));
    }
    let mut text = vec![b'0'; size as usize];
    for (value, _) in &numbers {
        if *value < low || *value > high {
            return Err(out_of_range(format!(
                "Value {value} is outside of provided min and max range ({low} <-> {high})"
            )));
        }
        text[(*value - low) as usize] = b'1';
    }
    let text = String::from_utf8(text).unwrap_or_default();
    let mut source = Vector::new(LogicalType::VARCHAR, 1);
    source.set_value(0, &Value::varchar(text))?;
    let mut target = Vector::new(ty.clone(), 1);
    super::cast::cast_vector(&source, &mut target, 1, super::cast::CastMode::Strict)?;
    Ok(target.get_value(0))
}

fn mode(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let keys = sort_keys(&values, ty)?;
    let mut counts: HashMap<&Vec<u8>, (usize, usize)> = HashMap::new();
    for (at, key) in keys.iter().enumerate() {
        counts.entry(key).or_insert((0, at)).0 += 1;
    }
    let (_, first) = counts
        .values()
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .expect("there are values");
    Ok(values[*first].clone())
}

fn histogram(rows: &[Value], ty: &LogicalType) -> Result<Value> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let values = sorted(values)?;
    let key_type = values[0].logical_type().clone();
    let keys = sort_keys(&values, &key_type)?;
    let entry_type = ty.child_type().expect("a MAP has entries").clone();
    let mut entries = Vec::new();
    let mut at = 0;
    while at < values.len() {
        let same = keys[at..].iter().take_while(|k| **k == keys[at]).count();
        entries.push(Value::new(
            entry_type.clone(),
            ValueData::Struct(vec![values[at].clone(), Value::ubigint(same as u64)]),
        )?);
        at += same;
    }
    Value::new(ty.clone(), ValueData::List(entries))
}

fn distinct_count(rows: &[Value]) -> Result<i64> {
    let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
    let Some(first) = values.first() else {
        return Ok(0);
    };
    let mut keys = sort_keys(&values, &first.logical_type().clone())?;
    keys.sort();
    keys.dedup();
    Ok(keys.len() as i64)
}

/// `bit_and` and the like over BIT: every value must have the same length.
fn bit_strings(rows: &[Value], ty: &LogicalType, name: &str, combine: fn(u8, u8) -> u8) -> Result<Value> {
    let mut result: Option<Vec<u8>> = None;
    for row in rows {
        let ValueData::Bytes(value) = row.data() else {
            continue;
        };
        result = Some(match result {
            None => value.clone(),
            Some(bits) if bit::bit_length(&bits) == bit::bit_length(value) => bit::bitwise(&bits, value, combine),
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Cannot {name} bit strings of different sizes"),
                ));
            }
        });
    }
    result.map_or_else(
        || Ok(Value::null(ty.clone())),
        |bits| Value::new(ty.clone(), ValueData::Bytes(bits)),
    )
}

fn bits(rows: &[Value], ty: &LogicalType, combine: fn(u128, u128) -> u128) -> Result<Value> {
    let mut result: Option<u128> = None;
    for row in rows {
        if let Some(value) = row.as_i128() {
            let value = value as u128;
            result = Some(result.map_or(value, |bits| combine(bits, value)));
        }
    }
    match result {
        Some(bits) => cast(Value::hugeint(bits as i128), ty),
        None => Ok(Value::null(ty.clone())),
    }
}

/// `StateFinalize`.
pub fn finish(kind: Holistic, rows: &[Value], ty: &LogicalType) -> Result<Value> {
    use Holistic::*;
    let null = || Value::null(ty.clone());
    Ok(match kind {
        StringAgg => {
            let mut out: Option<String> = None;
            for row in rows {
                let values = fields(row);
                if values[0].is_null() {
                    continue;
                }
                let text = values[0].try_to_string()?;
                match &mut out {
                    None => out = Some(text),
                    Some(joined) => {
                        match values.get(1) {
                            Some(separator) if separator.is_null() => {}
                            Some(separator) => *joined += &separator.try_to_string()?,
                            None => joined.push(','),
                        }
                        *joined += &text;
                    }
                }
            }
            out.map_or_else(null, Value::varchar)
        }
        BoolAnd | BoolOr => {
            let flags: Vec<bool> = rows
                .iter()
                .filter_map(|row| match row.data() {
                    ValueData::Bool(flag) => Some(*flag),
                    _ => None,
                })
                .collect();
            if flags.is_empty() {
                null()
            } else if kind == BoolAnd {
                Value::boolean(flags.iter().all(|f| *f))
            } else {
                Value::boolean(flags.iter().any(|f| *f))
            }
        }
        BitAnd if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "AND", |a, b| a & b)?,
        BitOr if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "OR", |a, b| a | b)?,
        BitXor if ty.id() == LogicalTypeId::Bit => bit_strings(rows, ty, "XOR", |a, b| a ^ b)?,
        BitAnd => bits(rows, ty, |a, b| a & b)?,
        BitOr => bits(rows, ty, |a, b| a | b)?,
        BitXor => bits(rows, ty, |a, b| a ^ b)?,
        Product => {
            let values = numbers(rows);
            if values.is_empty() {
                null()
            } else {
                Value::double(values.iter().product())
            }
        }
        Mode => mode(rows, ty)?,
        Median | QuantileCont => quantiles(rows, ty, true)?,
        QuantileDisc => quantiles(rows, ty, false)?,
        Mad => {
            let mut values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let median = median_of(&mut values);
            let mut deviations: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
            Value::double(median_of(&mut deviations))
        }
        StddevSamp | StddevPop | VarSamp | VarPop => {
            let values = numbers(rows);
            let sample = matches!(kind, StddevSamp | VarSamp);
            if values.is_empty() || (sample && values.len() < 2) {
                return Ok(null());
            }
            let (n, _, square, _, _) = moments(&values);
            let variance = square / if sample { n - 1.0 } else { n };
            match kind {
                StddevSamp => checked("STDDEV_SAMP", variance.sqrt())?,
                StddevPop => checked("STDDEV_POP", variance.sqrt())?,
                VarSamp => checked("VARSAMP", variance)?,
                _ => checked("VARPOP", variance)?,
            }
        }
        Sem => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let (n, _, square, _, _) = moments(&values);
            checked("SEM", (square / n).sqrt() / n.sqrt())?
        }
        AvgInterval => {
            let (mut months, mut days, mut micros, mut count) = (0i32, 0i32, 0i64, 0i64);
            for row in rows {
                let ValueData::Interval(value) = row.data() else {
                    continue;
                };
                let overflow = || out_of_range("Overflow in addition of INT32");
                months = months.checked_add(value.months).ok_or_else(overflow)?;
                days = days.checked_add(value.days).ok_or_else(overflow)?;
                micros = micros
                    .checked_add(value.micros)
                    .ok_or_else(|| out_of_range("Overflow in addition of INT64"))?;
                count += 1;
            }
            if count == 0 {
                return Ok(null());
            }
            let (months, days) = (i64::from(months), i64::from(days));
            let months_remainder = (months % count) * Interval::DAYS_PER_MONTH;
            let day_total = days / count + months_remainder / count;
            let days_remainder = days % count + months_remainder % count;
            let shifted = i128::from(days_remainder) * i128::from(Interval::MICROS_PER_DAY) / i128::from(count);
            let micro_total = micros / count + micros % count + shifted as i64;
            Value::interval(Interval::new((months / count) as i32, day_total as i32, micro_total))
        }
        AvgTimestamp => {
            let values: Vec<i64> = rows
                .iter()
                .filter_map(|row| match row.data() {
                    ValueData::Int64(value) => Some(*value),
                    _ => None,
                })
                .collect();
            if values.is_empty() {
                return Ok(null());
            }
            let positive = values.contains(&i64::MAX);
            let negative = values.contains(&-i64::MAX);
            let average = match (positive, negative) {
                (true, true) => return Ok(null()),
                (true, false) => i64::MAX,
                (false, true) => -i64::MAX,
                _ => {
                    let sum: i128 = values.iter().map(|v| i128::from(*v)).sum();
                    let count = values.len() as i128;
                    (sum / count + i128::from(sum % count > count / 2)) as i64
                }
            };
            Value::new(ty.clone(), ValueData::Int64(average))?
        }
        Skewness => {
            let values = numbers(rows);
            if values.len() <= 2 {
                return Ok(null());
            }
            // the running central moments stay exactly 0 over constant input
            let (mut seen, mut mean, mut m2, mut m3) = (0.0, 0.0, 0.0, 0.0);
            for value in &values {
                let previous = seen;
                seen += 1.0;
                let delta = value - mean;
                let delta_n = delta / seen;
                let term = delta * delta_n * previous;
                mean += delta_n;
                m3 += term * delta_n * (seen - 2.0) - 3.0 * delta_n * m2;
                m2 += term;
            }
            if m2 <= 0.0 {
                return Ok(null());
            }
            let n = seen;
            let g1 = n.sqrt() * (m3 / m2) / m2.sqrt();
            checked("SKEW", (n * (n - 1.0)).sqrt() / (n - 2.0) * g1)?
        }
        Kurtosis => {
            let values = numbers(rows);
            if values.len() <= 3 {
                return Ok(null());
            }
            let (n, _, square, _, fourth) = moments(&values);
            if square == 0.0 {
                return Ok(null());
            }
            let g2 = (fourth / n) / (square / n).powi(2) - 3.0;
            checked("KURTOSIS", (n - 1.0) / ((n - 2.0) * (n - 3.0)) * ((n + 1.0) * g2 + 6.0))?
        }
        Entropy => {
            let values: Vec<Value> = rows.iter().filter(|v| !v.is_null()).cloned().collect();
            if values.is_empty() {
                return Ok(Value::double(0.0));
            }
            let keys = sort_keys(&values, &values[0].logical_type().clone())?;
            let mut counts: HashMap<&Vec<u8>, f64> = HashMap::new();
            for key in &keys {
                *counts.entry(key).or_insert(0.0) += 1.0;
            }
            let total = keys.len() as f64;
            Value::double(counts.values().map(|c| -(c / total) * (c / total).log2()).sum())
        }
        ArgMin => arg_extreme(rows, ty, false)?,
        ArgMax => arg_extreme(rows, ty, true)?,
        ArgMinNull => arg_extreme_nulls(rows, ty, false, false)?,
        ArgMaxNull => arg_extreme_nulls(rows, ty, true, false)?,
        ArgMinNullsLast => arg_extreme_nulls(rows, ty, false, true)?,
        ArgMaxNullsLast => arg_extreme_nulls(rows, ty, true, true)?,
        MinN => extremes(rows, ty, false, 0, 0, 1)?,
        MaxN => extremes(rows, ty, true, 0, 0, 1)?,
        ArgMinN => extremes(rows, ty, false, 0, 1, 2)?,
        ArgMaxN => extremes(rows, ty, true, 0, 1, 2)?,
        ApproxTopK => top_k(rows, ty)?,
        BitstringAgg => bitstring(rows, ty)?,
        CombineAggr => {
            let mut combined = null();
            for row in rows {
                // a state counts as often as its multiplicity says
                let (state, times) = match row.data() {
                    ValueData::Struct(parts) => (&parts[0], parts[1].as_i128().unwrap_or(0)),
                    _ => (row, 1),
                };
                if times < 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "combine_aggr: the multiplicity must not be negative",
                    ));
                }
                let scaled = super::aggregates::repeat_state(state, times, ty)?;
                combined = super::aggregates::combine_state_values(&combined, &scaled, ty)?;
            }
            combined
        }
        WeightedAvg => {
            let (mut sum, mut weights) = (0.0, 0.0);
            for row in rows {
                let parts = fields(row);
                if let (Some(x), Some(w)) = (number(&parts[0]), number(&parts[1])) {
                    sum += x * w;
                    weights += w;
                }
            }
            if weights == 0.0 {
                null()
            } else {
                Value::double(sum / weights)
            }
        }
        KurtosisPop => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let (n, _, square, _, fourth) = moments(&values);
            if square == 0.0 {
                return Ok(null());
            }
            checked("KURTOSIS", (fourth / n) / (square / n).powi(2) - 3.0)?
        }
        CountIf if rows.is_empty() => null(),
        CountIf => {
            let count = rows
                .iter()
                .filter(|r| matches!(r.data(), ValueData::Bool(true)))
                .count();
            Value::hugeint(count as i128)
        }
        Histogram => histogram(rows, ty)?,
        ApproxCountDistinct => Value::bigint(distinct_count(rows)?),
        Fsum | Favg => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            let (mut sum, mut compensation) = (0.0, 0.0);
            for value in &values {
                let next = sum + value;
                compensation += if f64::abs(sum) >= f64::abs(*value) {
                    (sum - next) + value
                } else {
                    (value - next) + sum
                };
                sum = next;
            }
            let total = sum + compensation;
            Value::double(if kind == Fsum {
                total
            } else {
                total / values.len() as f64
            })
        }
        GeoMean => {
            let values = numbers(rows);
            if values.is_empty() {
                return Ok(null());
            }
            Value::double((values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp())
        }
        Corr | CovarPop | CovarSamp | RegrSlope | RegrIntercept | RegrR2 | RegrAvgX | RegrAvgY | RegrSxx | RegrSyy
        | RegrSxy => {
            let pairs = pairs(rows);
            if pairs.is_empty() {
                return Ok(null());
            }
            let r = regression(&pairs);
            // a single pair has no spread at all, whatever rounding leaves in the sums
            let (var_x, var_y) = if r.n > 1.0 {
                (r.sxx / r.n, r.syy / r.n)
            } else {
                (0.0, 0.0)
            };
            let slope = if var_x != 0.0 { (r.sxy / r.n) / var_x } else { f64::NAN };
            match kind {
                Corr => {
                    let spread = var_x.sqrt() * var_y.sqrt();
                    Value::double(if spread != 0.0 {
                        (r.sxy / r.n) / spread
                    } else {
                        f64::NAN
                    })
                }
                CovarPop => Value::double(r.sxy / r.n),
                CovarSamp if r.n < 2.0 => null(),
                CovarSamp => Value::double(r.sxy / (r.n - 1.0)),
                RegrSlope => Value::double(slope),
                RegrIntercept if slope.is_nan() => null(),
                RegrIntercept => Value::double(r.y - slope * r.x),
                RegrR2 if r.sxx == 0.0 => null(),
                RegrR2 if r.syy == 0.0 => Value::double(1.0),
                RegrR2 => Value::double(r.sxy * r.sxy / (r.sxx * r.syy)),
                RegrAvgX => Value::double(r.x),
                RegrAvgY => Value::double(r.y),
                RegrSxx => Value::double(r.sxx),
                RegrSyy => Value::double(r.syy),
                _ => Value::double(r.sxy),
            }
        }
        RegrCount => Value::uinteger(pairs(rows).len() as u32),
    })
}
