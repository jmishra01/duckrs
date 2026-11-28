// Ported from duckdb/extension/core_functions/scalar/generic/binning.cpp @ rust-port-baseline
//
//! `equi_width_bins(min, max, bin_count, nice_rounding)`: the upper boundaries of `bin_count` bins
//! of equal width between `min` and `max`, made round when `nice_rounding` asks for it.

use super::nested::{bound_function, row_wise};
use crate::execution::parquet::cast_value;
use crate::planner::expression::BoundExpr;
use pintail_common::types::{Date, Interval, LogicalType, LogicalTypeId, Time, Timestamp};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};

const MAX_BIN_COUNT: i64 = 1_000_000;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

#[derive(Clone, Copy, PartialEq)]
enum Rounding {
    Ceiling,
    Round,
}

fn previous_power_of_ten_int(input: i128) -> i128 {
    let mut power = 1i128;
    while power < input {
        power *= 10;
    }
    power / 10
}

fn round_to_number_int(input: i128, num: i128, rounding: Rounding) -> i128 {
    if num == 0 {
        return input;
    }
    match rounding {
        Rounding::Round => (input + num / 2) / num * num,
        Rounding::Ceiling => (input + (num - 1)) / num * num,
    }
}

fn make_nice_int(input: i128, step: i128, rounding: Rounding) -> i128 {
    let power = previous_power_of_ten_int(step);
    let mut two = power * 2;
    let mut five = power;
    if power * 3 <= step {
        two *= 5;
    }
    if power * 2 <= step {
        five *= 5;
    }
    let to_two = round_to_number_int(input, two, rounding);
    let to_five = round_to_number_int(input, five, rounding);
    if (input - to_two).abs() < (input - to_five).abs() {
        to_two
    } else {
        to_five
    }
}

fn previous_power_of_ten(input: f64) -> f64 {
    let mut power = 1.0;
    if input < 1.0 {
        while power > input {
            power /= 10.0;
        }
        return power;
    }
    while power < input {
        power *= 10.0;
    }
    power / 10.0
}

fn round_to_number(input: f64, num: f64, rounding: Rounding) -> f64 {
    let result = match rounding {
        Rounding::Round => (input / num).round() * num,
        Rounding::Ceiling => (input / num).ceil() * num,
    };
    if result.is_finite() { result } else { input }
}

fn make_nice(input: f64, step: f64, rounding: Rounding) -> f64 {
    if input == 0.0 {
        return 0.0;
    }
    let power = previous_power_of_ten(step);
    let mut two = power * 2.0;
    let mut five = power;
    if power * 3.0 <= step {
        two *= 5.0;
    }
    if power * 2.0 <= step {
        five *= 5.0;
    }
    let to_two = round_to_number(input, two, rounding);
    let to_five = round_to_number(input, five, rounding);
    if (input - to_two).abs() < (input - to_five).abs() {
        to_two
    } else {
        to_five
    }
}

/// `EquiWidthBinsInteger`
fn bins_integer(input_min: i64, input_max: i64, mut bin_count: usize, nice: bool) -> Vec<i64> {
    const FACTOR: i128 = 1000;
    let min = i128::from(input_min) * FACTOR;
    let mut max = i128::from(input_max) * FACTOR;
    let span = max - min;
    let mut step = span / bin_count as i128;
    if step == 0 {
        step = 1;
    }
    if nice {
        let new_step = make_nice_int(step, step, Rounding::Round);
        let new_max = round_to_number_int(max, new_step, Rounding::Ceiling);
        if new_max != min && new_step != 0 {
            max = new_max;
            step = new_step;
        }
        bin_count *= 2;
    }
    let mut result: Vec<i64> = Vec::new();
    let mut boundary = max;
    while boundary > min {
        let real = (boundary / FACTOR).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        boundary -= step;
        if let Some(&last) = result.last() {
            if real < input_min || result.len() >= bin_count {
                break;
            }
            if real == last {
                continue;
            }
        }
        result.push(real);
    }
    result
}

/// `EquiWidthBinsDouble`
fn bins_double(min: f64, input_max: f64, mut bin_count: usize, nice: bool) -> Result<Vec<f64>> {
    let mut max = input_max;
    if !min.is_finite() || !max.is_finite() {
        return Err(invalid(
            "equi_width_bucket does not support infinite or nan as min/max value",
        ));
    }
    let span = max - min;
    let mut step = if span.is_finite() {
        span / bin_count as f64
    } else {
        max / bin_count as f64 - min / bin_count as f64
    };
    let step_power = previous_power_of_ten(step);
    if nice {
        step = make_nice(step, step, Rounding::Round);
        max = round_to_number(input_max, step, Rounding::Ceiling);
        bin_count *= 2;
    }
    let multiplication = 10.0 / step_power;
    if max - step >= max || (nice && !multiplication.is_finite()) {
        return Ok(vec![max]);
    }
    let mut result: Vec<f64> = Vec::new();
    let mut boundary = max;
    while boundary > min {
        let real = if nice {
            (boundary * multiplication).round() / multiplication
        } else {
            boundary
        };
        boundary -= step;
        if result.last() == Some(&real) {
            continue;
        }
        if real <= min || result.len() >= bin_count {
            break;
        }
        result.push(real);
    }
    Ok(result)
}

struct Parts {
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    micros: i32,
}

fn parts(timestamp: i64) -> Parts {
    let ts = Timestamp(timestamp);
    let (year, month, day) = ts.date().to_ymd();
    let (hour, minute, second, micros) = ts.time().to_hms();
    Parts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    }
}

fn next_month(p: &mut Parts) {
    p.month += 1;
    if p.month == 13 {
        p.year += 1;
        p.month = 1;
    }
}

fn next_day(p: &mut Parts) {
    p.day += 1;
    if !Date::is_valid(p.year, p.month, p.day) {
        next_month(p);
        p.day = 1;
    }
}

fn next_hour(p: &mut Parts) {
    p.hour += 1;
    if p.hour >= 24 {
        next_day(p);
        p.hour = 0;
    }
}

fn next_minute(p: &mut Parts) {
    p.minute += 1;
    if p.minute >= 60 {
        next_hour(p);
        p.minute = 0;
    }
}

fn next_second(p: &mut Parts) {
    p.second += 1;
    if p.second >= 60 {
        next_minute(p);
        p.second = 0;
    }
}

const MICROS_PER_SEC: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SEC;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;

/// `MakeTimestampNice`
fn nice_timestamp(mut p: Parts, step: Interval) -> Result<i64> {
    if step.months >= 12 {
        if p.day > 1 || p.hour > 0 || p.minute > 0 || p.second > 0 || p.micros > 0 {
            next_month(&mut p);
            p.hour = 0;
            p.minute = 0;
            p.second = 0;
            p.micros = 0;
            p.day = 1;
        }
    } else if step.months > 0 || step.days >= 1 {
        if p.hour > 0 || p.minute > 0 || p.second > 0 || p.micros > 0 {
            next_day(&mut p);
            p.hour = 0;
            p.minute = 0;
            p.second = 0;
            p.micros = 0;
        }
    } else if step.days > 0 || step.micros >= MICROS_PER_HOUR {
        if p.minute > 0 || p.second > 0 || p.micros > 0 {
            next_hour(&mut p);
            p.minute = 0;
            p.second = 0;
            p.micros = 0;
        }
    } else if step.micros >= MICROS_PER_MINUTE {
        if p.second > 0 || p.micros > 0 {
            next_minute(&mut p);
            p.second = 0;
            p.micros = 0;
        }
    } else if step.micros >= MICROS_PER_SEC && p.micros > 0 {
        next_second(&mut p);
        p.micros = 0;
    }
    let date = Date::from_ymd(p.year, p.month, p.day).ok_or_else(|| invalid("Date out of range"))?;
    let time = Time::from_hms(p.hour, p.minute, p.second, p.micros);
    Timestamp::from_datetime(date, time)
        .map(|t| t.0)
        .ok_or_else(|| invalid("Timestamp out of range"))
}

fn round_to_divisor(number: i64, divisor: i64) -> i64 {
    (number + divisor / 2) / divisor * divisor
}

/// `MakeIntervalNice`
fn nice_interval(mut interval: Interval) -> Interval {
    if interval.months >= 6 {
        interval.days = 0;
        interval.micros = 0;
    } else if interval.months > 0 || interval.days >= 5 {
        interval.micros = 0;
    } else if interval.days > 0 || interval.micros >= 6 * MICROS_PER_HOUR {
        interval.micros = round_to_divisor(interval.micros, MICROS_PER_HOUR);
    } else if interval.micros >= MICROS_PER_HOUR {
        interval.micros = round_to_divisor(interval.micros, MICROS_PER_MINUTE * 15);
    } else if interval.micros >= MICROS_PER_MINUTE * 10 {
        interval.micros = round_to_divisor(interval.micros, MICROS_PER_MINUTE);
    } else if interval.micros >= MICROS_PER_MINUTE {
        interval.micros = round_to_divisor(interval.micros, MICROS_PER_SEC * 15);
    } else if interval.micros >= MICROS_PER_SEC * 10 {
        interval.micros = round_to_divisor(interval.micros, MICROS_PER_SEC);
    }
    interval
}

/// `EquiWidthBinsTimestamp`
fn bins_timestamp(input_min: i64, input_max: i64, bin_count: usize, nice: bool) -> Result<Vec<i64>> {
    if !Timestamp(input_min).is_finite() || !Timestamp(input_max).is_finite() {
        return Err(invalid(
            "equi_width_bucket does not support infinite or nan as min/max value",
        ));
    }
    if !nice {
        return Ok(bins_integer(input_min, input_max, bin_count, false));
    }
    let (min, max) = (parts(input_min), parts(input_max));
    let months = (max.year - min.year) * 12 + (max.month - min.month);
    let days = max.day - min.day;
    let micros = i64::from(max.hour - min.hour) * MICROS_PER_HOUR
        + i64::from(max.minute - min.minute) * MICROS_PER_MINUTE
        + i64::from(max.second - min.second) * MICROS_PER_SEC
        + i64::from(max.micros - min.micros);
    let step_months = f64::from(months) / bin_count as f64;
    let mut step_days = f64::from(days) / f64::from(bin_count as i32);
    let mut step_micros = micros as f64 / bin_count as f64;
    if step_months > 0.0 {
        step_days += (step_months - step_months.floor()) * Interval::DAYS_PER_MONTH as f64;
    }
    if step_days > 0.0 {
        step_micros += (step_days - step_days.floor()) * Interval::MICROS_PER_DAY as f64;
    }
    let mut step = nice_interval(Interval::new(step_months as i32, step_days as i32, step_micros as i64));
    let mut value = nice_timestamp(max, step)?;
    if step.months <= 0 && step.days <= 0 && step.micros <= 0 {
        step = Interval::new(0, 0, 1);
    }
    let negative = Interval::new(-step.months, -step.days, -step.micros);
    let mut result = Vec::new();
    while value >= input_min && result.len() < bin_count {
        result.push(value);
        value = super::dates::add_interval(value, negative)?;
    }
    Ok(result)
}

#[derive(Clone, Copy)]
enum Kind {
    Integer,
    Double,
    Timestamp,
}

/// Binds `equi_width_bins`: the overload of `EquiWidthBinsFun`, and `EquiWidthResolveTypes`.
pub(super) fn bind(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if children.len() != 4 {
        return Err(Error::new(
            ErrorKind::Binder,
            format!(
                "No function matches the given name and argument types 'equi_width_bins({})'. You might need to add \
                 explicit type casts.",
                children
                    .iter()
                    .map(|c| super::binder::expression_return_type(c).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let max_type = children[1].return_type.clone();
    let kind_of = |ty: &LogicalType| match ty.id() {
        LogicalTypeId::SqlNull => None,
        LogicalTypeId::Float | LogicalTypeId::Double | LogicalTypeId::Decimal => Some(Some(Kind::Double)),
        _ if ty.is_integral() => Some(Some(Kind::Integer)),
        LogicalTypeId::Date | LogicalTypeId::Timestamp => Some(Some(Kind::Timestamp)),
        _ => Some(None),
    };
    let kinds = [kind_of(&children[0].return_type), kind_of(&max_type)];
    let kind = match kinds {
        // `UnsupportedEquiWidth`: refused when it runs, so a branch that never does is no error
        [Some(None), _] | [_, Some(None)] => {
            let ty = if matches!(kinds[0], Some(None)) {
                children[0].return_type.clone()
            } else {
                max_type.clone()
            };
            let element = if max_type.id() == LogicalTypeId::SqlNull {
                ty.clone()
            } else {
                max_type.clone()
            };
            let mut args = children;
            args[0] = BoundExpr::constant(Value::varchar(ty.to_string()));
            return Ok(bound_function(
                "equi_width_bins",
                args,
                LogicalType::list(element),
                unsupported_equi_width,
            ));
        }
        [Some(Some(Kind::Double)), _] | [_, Some(Some(Kind::Double))] => Kind::Double,
        [Some(Some(Kind::Timestamp)), _] | [_, Some(Some(Kind::Timestamp))] => Kind::Timestamp,
        _ => Kind::Integer,
    };
    let compute = match kind {
        Kind::Integer => LogicalType::BIGINT,
        Kind::Double => LogicalType::DOUBLE,
        Kind::Timestamp => LogicalType::TIMESTAMP,
    };
    let element = match max_type.id() {
        LogicalTypeId::SqlNull => compute.clone(),
        LogicalTypeId::Decimal => LogicalType::DOUBLE,
        _ => max_type.clone(),
    };
    let mut args = children;
    args[0] = args[0].clone().cast_to(&compute, false);
    args[1] = args[1].clone().cast_to(&compute, false);
    args[2] = args[2].clone().cast_to(&LogicalType::BIGINT, false);
    args[3] = args[3].clone().cast_to(&LogicalType::BOOLEAN, false);
    let callback: super::ScalarCallback = match kind {
        Kind::Integer => equi_width_integer,
        Kind::Double => equi_width_double,
        Kind::Timestamp => equi_width_timestamp,
    };
    Ok(bound_function(
        "equi_width_bins",
        args,
        LogicalType::list(element),
        callback,
    ))
}

fn unsupported_equi_width(args: &DataChunk, _: &mut Vector) -> Result<()> {
    let ty = args.column(0).get_value(0).try_to_string()?;
    Err(Error::new(
        ErrorKind::Binder,
        format!("Unsupported type \"{ty}\" for equi_width_bins"),
    ))
}

/// The checks of `EquiWidthBinFunction`, and the bin count.
fn checked(row: &[Value], less: bool) -> Result<Option<usize>> {
    if row.iter().any(Value::is_null) {
        return Ok(None);
    }
    if less {
        return Err(invalid(
            "Invalid input for bin function - max value is smaller than min value",
        ));
    }
    let bins = match row[2].data() {
        ValueData::Int64(bins) => *bins,
        _ => 0,
    };
    if bins <= 0 {
        return Err(invalid("Invalid input for bin function - there must be > 0 bins"));
    }
    if bins > MAX_BIN_COUNT {
        return Err(invalid(format!(
            "Invalid input for bin function - max bin count of {MAX_BIN_COUNT} exceeded"
        )));
    }
    Ok(Some(bins as usize))
}

fn nice_of(row: &[Value]) -> bool {
    matches!(row[3].data(), ValueData::Bool(true))
}

/// The boundaries, in ascending order and ending at `max`, as a list of the result's type.
fn finish<T: Copy + PartialOrd>(
    mut values: Vec<T>,
    max: T,
    equal: bool,
    make: impl Fn(T) -> Value,
    result_type: &LogicalType,
) -> Result<Value> {
    if equal {
        values = vec![max];
    } else {
        if values.first().is_some_and(|first| *first < max) {
            values[0] = max;
        }
        values.reverse();
    }
    let element = result_type.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
    let items = values
        .into_iter()
        .map(|v| {
            let value = make(v);
            if value.logical_type() == &element {
                Ok(value)
            } else {
                cast_value(&value, &element)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Value::new(result_type.clone(), ValueData::List(items))
}

fn equi_width_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let int = |v: &Value| match v.data() {
            ValueData::Int64(v) => *v,
            _ => 0,
        };
        let (min, max) = (int(&row[0]), int(&row[1]));
        let Some(bins) = checked(row, max < min)? else {
            return Ok(Value::null(ty.clone()));
        };
        let values = if max == min {
            Vec::new()
        } else {
            bins_integer(min, max, bins, nice_of(row))
        };
        finish(values, max, max == min, Value::bigint, &ty)
    })
}

fn equi_width_double(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let float = |v: &Value| match v.data() {
            ValueData::Double(v) => *v,
            _ => 0.0,
        };
        let (min, max) = (float(&row[0]), float(&row[1]));
        let Some(bins) = checked(row, max < min)? else {
            return Ok(Value::null(ty.clone()));
        };
        let values = if max == min {
            Vec::new()
        } else {
            bins_double(min, max, bins, nice_of(row))?
        };
        finish(values, max, max == min, Value::double, &ty)
    })
}

fn equi_width_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let micros = |v: &Value| match v.data() {
            ValueData::Int64(v) => *v,
            _ => 0,
        };
        let (min, max) = (micros(&row[0]), micros(&row[1]));
        let Some(bins) = checked(row, max < min)? else {
            return Ok(Value::null(ty.clone()));
        };
        let values = if max == min {
            Vec::new()
        } else {
            bins_timestamp(min, max, bins, nice_of(row))?
        };
        finish(values, max, max == min, |v| Value::timestamp(Timestamp(v)), &ty)
    })
}
