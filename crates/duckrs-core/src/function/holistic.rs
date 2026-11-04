// Ported from the semantics of duckdb/extension/core_functions/aggregate/**/*.cpp
// @ rust-port-baseline
//
//! The aggregates that keep their rows. C++ has a state per aggregate, and sketches for the
//! approximate ones; these gather the values and compute the result at the end, which is exact
//! and easy to get right, and costs memory until Phase 13 brings the states.

use super::aggregates::value_keys;
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{LogicalType, LogicalTypeId, MICROS_PER_DAY};
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
    Entropy,
    ArgMin,
    ArgMax,
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
            (Mode | BitAnd | BitOr | BitXor | ArgMin | ArgMax, Some(ty)) => ty.clone(),
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

/// A date or time as the number it interpolates by.
fn position(value: &Value) -> Option<f64> {
    match value.logical_type().id() {
        LogicalTypeId::Date => value.as_i128().map(|days| days as f64 * MICROS_PER_DAY as f64),
        _ => number(value),
    }
}

fn from_position(position: f64, input: &Value, ty: &LogicalType) -> Result<Value> {
    if input.logical_type().id() == LogicalTypeId::Date
        || matches!(
            ty.id(),
            LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz | LogicalTypeId::Time
        )
    {
        return Value::new(ty.clone(), ValueData::Int64(position.round() as i64));
    }
    cast(Value::double(position), ty)
}

/// `Interpolator`: the value at fraction `q` of the sorted `values`.
fn quantile(values: &[Value], q: f64, continuous: bool, ty: &LogicalType) -> Result<Value> {
    let count = values.len() as f64;
    if !continuous {
        let index = (count - (count - q * count).floor()).max(1.0) as usize - 1;
        return cast(values[index.min(values.len() - 1)].clone(), ty);
    }
    let rank = (count - 1.0) * q;
    let (low, high) = (rank.floor(), rank.ceil());
    let (first, second) = (&values[low as usize], &values[high as usize]);
    match (position(first), position(second)) {
        (Some(a), Some(b)) => {
            if low == high {
                return from_position(a, first, ty);
            }
            from_position(a + (b - a) * (rank - low), first, ty)
        }
        _ => cast(first.clone(), ty),
    }
}

fn fractions(row: &Value) -> Result<(Vec<f64>, bool)> {
    let parameter = fields(row).get(1).cloned().unwrap_or_else(|| Value::double(0.5));
    let (items, is_list) = match parameter.data() {
        ValueData::List(items) => (items.clone(), true),
        _ => (vec![parameter.clone()], false),
    };
    let mut fractions = Vec::with_capacity(items.len());
    for item in &items {
        let Some(fraction) = number(item) else {
            return Err(Error::new(ErrorKind::Binder, "QUANTILE parameter cannot be NULL"));
        };
        if !(-1.0..=1.0).contains(&fraction) {
            return Err(Error::new(
                ErrorKind::Binder,
                "QUANTILE can only take parameters in the range [-1, 1]",
            ));
        }
        fractions.push(if fraction < 0.0 { 1.0 + fraction } else { fraction });
    }
    Ok((fractions, is_list))
}

fn quantiles(rows: &[Value], ty: &LogicalType, continuous: bool) -> Result<Value> {
    let Some(first) = rows.first() else {
        return Ok(Value::null(ty.clone()));
    };
    let (fractions, is_list) = fractions(first)?;
    let values: Vec<Value> = rows
        .iter()
        .map(|row| fields(row)[0].clone())
        .filter(|v| !v.is_null())
        .collect();
    if values.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let values = sorted(values)?;
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
    let chosen: Vec<&Value> = rows.iter().filter(|row| !fields(row)[1].is_null()).collect();
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
    let double_or_null = |value: Option<f64>| value.map_or_else(null, Value::double);
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
        Skewness => {
            let values = numbers(rows);
            if values.len() <= 2 {
                return Ok(null());
            }
            let (n, _, square, cube, _) = moments(&values);
            if square == 0.0 {
                return Ok(null());
            }
            let g1 = (cube / n) / (square / n).powf(1.5);
            checked("SKEWNESS", (n * (n - 1.0)).sqrt() / (n - 2.0) * g1)?
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
            let finite = |value: f64| value.is_finite().then_some(value);
            match kind {
                Corr => double_or_null(finite(r.sxy / (r.sxx * r.syy).sqrt())),
                CovarPop => Value::double(r.sxy / r.n),
                CovarSamp if r.n < 2.0 => null(),
                CovarSamp => Value::double(r.sxy / (r.n - 1.0)),
                RegrSlope => double_or_null(finite(r.sxy / r.sxx)),
                RegrIntercept => double_or_null(finite(r.y - r.sxy / r.sxx * r.x)),
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
