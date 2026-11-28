// Ported from duckdb/extension/core_functions/scalar/date/*.cpp,
// duckdb/src/function/scalar/date/strftime.cpp, duckdb/src/function/scalar/strftime_format.cpp and
// the date operators of duckdb/src/function/scalar/operator/*.cpp @ rust-port-baseline
//
//! Date and time functions. They run row by row; the vectorized loops come with Phase 13. Time
//! zones wait for the ICU port: a TIMESTAMPTZ reads as UTC here.

use super::binder::bind_scalar_function;
use super::bit_functions::strict_rows;
use super::nested::bound_function;
use super::strptime::{ParseError, Spec, StrpFormat, format_error};
use super::{BuiltinFunctions, ScalarCallback, ScalarFunction, ScalarFunctionSet};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use pintail_common::types::{
    Date, DatePart, Interval, LogicalType, LogicalTypeId, MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE,
    MICROS_PER_SEC, Time, TimeTz, Timestamp,
};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};

fn out_of_range(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

fn conversion(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Conversion, message)
}

/// A date or time in one of its forms. `None` from [`moment`] means infinity.
#[derive(Debug, Clone, Copy)]
enum Moment {
    Date(i32),
    Timestamp(i64),
    Time(i64),
    Interval(Interval),
}

fn moment(value: &Value) -> Option<Moment> {
    let int = value.as_i128();
    Some(match value.logical_type().id() {
        LogicalTypeId::Date => {
            let days = int? as i32;
            if !Date(days).is_finite() {
                return None;
            }
            Moment::Date(days)
        }
        LogicalTypeId::Time => Moment::Time(int? as i64),
        LogicalTypeId::TimeNs => Moment::Time(int? as i64 / 1000),
        LogicalTypeId::TimeTz => Moment::Time(TimeTz(int? as u64).time().0),
        LogicalTypeId::Interval => {
            let ValueData::Interval(interval) = value.data() else {
                unreachable!("an INTERVAL");
            };
            Moment::Interval(*interval)
        }
        LogicalTypeId::TimestampSec | LogicalTypeId::TimestampMs | LogicalTypeId::TimestampNs => {
            let raw = int? as i64;
            Moment::Timestamp(match value.logical_type().id() {
                LogicalTypeId::TimestampSec => raw.saturating_mul(MICROS_PER_SEC),
                LogicalTypeId::TimestampMs => raw.saturating_mul(1000),
                _ => raw.div_euclid(1000),
            })
        }
        _ => {
            let micros = int? as i64;
            if !Timestamp(micros).is_finite() {
                return None;
            }
            Moment::Timestamp(micros)
        }
    })
}

/// The date and the time of day.
fn split(moment: Moment) -> Option<(i32, i64)> {
    match moment {
        Moment::Date(days) => Some((days, 0)),
        Moment::Timestamp(micros) => Some((
            micros.div_euclid(MICROS_PER_DAY) as i32,
            micros.rem_euclid(MICROS_PER_DAY),
        )),
        _ => None,
    }
}

fn weekday_from_sunday(days: i32) -> i32 {
    (i64::from(days) + 4).rem_euclid(7) as i32
}

fn weekday_from_monday(days: i32) -> i32 {
    (i64::from(days) + 3).rem_euclid(7) as i32
}

/// The days of January 1st, which can lie before the first date there is: dates repeat every
/// 400 years.
fn first_of_year(year: i32) -> i32 {
    match Date::from_ymd(year, 1, 1) {
        Some(date) => date.0,
        None if year < 0 => Date::from_ymd(year + 400, 1, 1).expect("in range").0 - 146_097,
        None => Date::from_ymd(year - 400, 1, 1).expect("in range").0 + 146_097,
    }
}

/// The ISO year and week.
fn iso_week(days: i32) -> (i32, i32) {
    let thursday = days - weekday_from_monday(days) + 3;
    let (year, _, _) = Date(thursday).to_ymd();
    (year, (thursday - first_of_year(year)) / 7 + 1)
}

enum Part {
    Integer(i64),
    Double(f64),
}

/// `DatePart::*Operator` for dates and timestamps.
fn date_part_of(part: DatePart, days: i32, time: i64) -> Result<Part> {
    let (year, month, day) = Date(days).to_ymd();
    let (year, month, day) = (i64::from(year), i64::from(month), i64::from(day));
    let seconds = (time / MICROS_PER_SEC) % 60;
    let fraction = time % MICROS_PER_SEC;
    let bc_aware = |size: i64| {
        if year > 0 {
            (year - 1) / size + 1
        } else {
            year / size - 1
        }
    };
    Ok(Part::Integer(match part {
        DatePart::Year => year,
        DatePart::Month => month,
        DatePart::Day => day,
        // `DecadeOperator`: the year divided by 10, truncated
        DatePart::Decade => year / 10,
        DatePart::Century => bc_aware(100),
        DatePart::Millennium => bc_aware(1000),
        DatePart::Quarter => (month - 1) / 3 + 1,
        DatePart::Dow => i64::from(weekday_from_sunday(days)),
        DatePart::IsoDow => i64::from(weekday_from_monday(days)) + 1,
        DatePart::Doy => i64::from(days - first_of_year(year as i32)) + 1,
        DatePart::Week => i64::from(iso_week(days).1),
        DatePart::IsoYear => i64::from(iso_week(days).0),
        DatePart::YearWeek => {
            let (year, week) = iso_week(days);
            i64::from(year) * 100 + i64::from(if year > 0 { week } else { -week })
        }
        DatePart::Era => i64::from(year > 0),
        DatePart::Hour => time / MICROS_PER_HOUR,
        DatePart::Minute => (time / MICROS_PER_MINUTE) % 60,
        DatePart::Second => seconds,
        DatePart::Milliseconds => seconds * 1000 + fraction / 1000,
        DatePart::Microseconds => seconds * MICROS_PER_SEC + fraction,
        DatePart::Timezone | DatePart::TimezoneHour | DatePart::TimezoneMinute => 0,
        DatePart::Epoch => {
            let micros = i64::from(days) * MICROS_PER_DAY + time;
            return Ok(Part::Double(micros as f64 / MICROS_PER_SEC as f64));
        }
        DatePart::JulianDay => {
            let julian = f64::from(days) + 2_440_588.0 + time as f64 / MICROS_PER_DAY as f64;
            return Ok(Part::Double(julian));
        }
    }))
}

fn time_part_of(part: DatePart, time: i64) -> Result<Part> {
    let seconds = (time / MICROS_PER_SEC) % 60;
    let fraction = time % MICROS_PER_SEC;
    Ok(Part::Integer(match part {
        DatePart::Hour => time / MICROS_PER_HOUR,
        DatePart::Minute => (time / MICROS_PER_MINUTE) % 60,
        DatePart::Second => seconds,
        DatePart::Milliseconds => seconds * 1000 + fraction / 1000,
        DatePart::Microseconds => seconds * MICROS_PER_SEC + fraction,
        DatePart::Epoch => return Ok(Part::Double(time as f64 / MICROS_PER_SEC as f64)),
        DatePart::Timezone | DatePart::TimezoneHour | DatePart::TimezoneMinute => 0,
        other => return Err(unsupported_part("time", other)),
    }))
}

/// `GetDatePartSpecifier`'s check of a constant specifier against the type it is taken of.
fn check_part_for_type(name: &str, part: DatePart, ty: &LogicalType) -> Result<()> {
    use DatePart as P;
    let type_name = match ty.id() {
        LogicalTypeId::Time => "time",
        LogicalTypeId::TimeNs => "time_ns",
        LogicalTypeId::TimeTz => "time_tz",
        LogicalTypeId::Interval => "interval",
        _ => return Ok(()),
    };
    let time = matches!(
        part,
        P::Microseconds | P::Milliseconds | P::Second | P::Minute | P::Hour | P::Epoch
    );
    let allowed = if type_name == "interval" {
        time || matches!(
            part,
            P::Year | P::Month | P::Day | P::Decade | P::Century | P::Quarter | P::Millennium
        )
    } else {
        time || matches!(part, P::Timezone | P::TimezoneHour | P::TimezoneMinute)
    };
    if allowed {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::NotImplemented,
        format!("\"{type_name}\" units \"{name}\" not recognized"),
    ))
}

fn unsupported_part(kind: &str, part: DatePart) -> Error {
    Error::new(
        ErrorKind::NotImplemented,
        format!("\"{kind}\" units \"{part:?}\" not recognized"),
    )
}

fn interval_part_of(part: DatePart, interval: Interval) -> Result<Part> {
    let months = i64::from(interval.months);
    let micros = interval.micros;
    let seconds = (micros / MICROS_PER_SEC) % 60;
    let fraction = micros % MICROS_PER_SEC;
    Ok(Part::Integer(match part {
        DatePart::Year => months / 12,
        DatePart::Month => months % 12,
        DatePart::Day => i64::from(interval.days),
        DatePart::Decade => months / 120,
        DatePart::Century => months / 1200,
        DatePart::Millennium => months / 12000,
        DatePart::Quarter => (months % 12) / 3 + 1,
        DatePart::Hour => micros / MICROS_PER_HOUR,
        DatePart::Minute => (micros / MICROS_PER_MINUTE) % 60,
        DatePart::Second => seconds,
        DatePart::Milliseconds => seconds * 1000 + fraction / 1000,
        DatePart::Microseconds => seconds * MICROS_PER_SEC + fraction,
        DatePart::Epoch => {
            // a year is 365.25 days here, as in `Interval::SECS_PER_YEAR`
            let days = (months % 12) * 30 + i64::from(interval.days);
            return Ok(Part::Double(
                (months / 12) as f64 * 31_557_600.0 + days as f64 * 86_400.0 + micros as f64 / MICROS_PER_SEC as f64,
            ));
        }
        other => return Err(unsupported_part("interval", other)),
    }))
}

fn part_of(part: DatePart, moment: Moment) -> Result<Part> {
    match moment {
        Moment::Time(time) => time_part_of(part, time),
        Moment::Interval(interval) => interval_part_of(part, interval),
        other => {
            let (days, time) = split(other).expect("a date or a timestamp");
            date_part_of(part, days, time)
        }
    }
}

fn parse_part(name: &str) -> Result<DatePart> {
    DatePart::parse(name).ok_or_else(|| conversion(format!("extract specifier \"{name}\" not recognized")))
}

fn part_value(part: DatePart, value: &Value, ty: &LogicalType) -> Result<Value> {
    let id = value.logical_type().id();
    if matches!(
        part,
        DatePart::Timezone | DatePart::TimezoneHour | DatePart::TimezoneMinute
    ) && !matches!(
        id,
        LogicalTypeId::TimestampTz | LogicalTypeId::TimestampTzNs | LogicalTypeId::TimeTz
    ) {
        // `TimezoneOperator`: a DATE or an INTERVAL has no time zone to ask about, anything else is at +00; an
        // infinity has no parts at all
        if !value.is_null() && id != LogicalTypeId::Interval && moment(value).is_none() {
            return Ok(Value::null(ty.clone()));
        }
        if matches!(id, LogicalTypeId::Date | LogicalTypeId::Interval) {
            let name = match part {
                DatePart::Timezone => "timezone",
                DatePart::TimezoneHour => "timezone_hour",
                _ => "timezone_minute",
            };
            let kind = if id == LogicalTypeId::Date { "date" } else { "interval" };
            return Err(Error::new(
                ErrorKind::NotImplemented,
                format!("\"{kind}\" units \"{name}\" not recognized"),
            ));
        }
        if moment(value).is_none() {
            return Ok(Value::null(ty.clone()));
        }
        return Ok(Value::bigint(0));
    }
    if id == LogicalTypeId::TimeTz
        && let Some(raw) = value.as_i128()
    {
        let offset = i64::from(TimeTz(raw as u64).offset());
        match part {
            DatePart::Timezone => return Ok(Value::bigint(offset)),
            DatePart::TimezoneHour => return Ok(Value::bigint(offset / 3600)),
            DatePart::TimezoneMinute => return Ok(Value::bigint((offset % 3600) / 60)),
            _ => {}
        }
    }
    let Some(moment) = moment(value) else {
        return Ok(Value::null(ty.clone()));
    };
    Ok(match (part_of(part, moment)?, ty.id()) {
        (Part::Integer(v), LogicalTypeId::Double) => Value::double(v as f64),
        (Part::Double(v), LogicalTypeId::Double) => Value::double(v),
        (Part::Integer(v), _) => Value::bigint(v),
        (Part::Double(v), _) => Value::bigint(v as i64),
    })
}

/// `StructDatePart::Bind`: `date_part(['year', 'month'], x)` is a STRUCT of the parts.
fn bind_struct_date_part(parts: &BoundExpr, value: &BoundExpr) -> Result<BoundExpr> {
    if !parts.is_foldable() {
        return Err(Error::new(
            ErrorKind::Binder,
            "date_part can only take constant lists of part names",
        ));
    }
    let list = evaluate_scalar(&parts.clone().cast_to(&LogicalType::list(LogicalType::VARCHAR), false))?;
    let ValueData::List(names) = list.data() else {
        return Err(Error::new(
            ErrorKind::Binder,
            "date_part can only take constant lists of part names",
        ));
    };
    if names.is_empty() {
        return Err(Error::new(
            ErrorKind::Binder,
            "date_part requires non-empty lists of part names",
        ));
    }
    let mut fields: Vec<(String, LogicalType)> = Vec::with_capacity(names.len());
    for name in names {
        if name.is_null() {
            return Err(Error::new(ErrorKind::Binder, "NULL struct entry name in date_part"));
        }
        let name = name.try_to_string()?;
        let part = parse_part(&name)?;
        if fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Duplicate struct entry name \"{name}\" in date_part"),
            ));
        }
        let ty = match part {
            DatePart::Epoch | DatePart::JulianDay => LogicalType::DOUBLE,
            _ => LogicalType::BIGINT,
        };
        fields.push((name, ty));
    }
    let id = value.return_type.id();
    let value = match id {
        LogicalTypeId::Date
        | LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampNs
        | LogicalTypeId::Time
        | LogicalTypeId::TimeNs
        | LogicalTypeId::TimeTz
        | LogicalTypeId::Interval
        | LogicalTypeId::TimestampTz => value.clone(),
        _ => value.clone().cast_to(&LogicalType::TIMESTAMP, false),
    };
    let names = Value::list(
        LogicalType::VARCHAR,
        fields.iter().map(|(n, _)| Value::varchar(n.clone())).collect(),
    )?;
    Ok(bound_function(
        "date_part",
        vec![BoundExpr::constant(names), value],
        LogicalType::struct_type(fields),
        struct_date_part,
    ))
}

fn struct_date_part(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let ValueData::List(names) = args.column(0).get_value(0).data().clone() else {
        return Ok(());
    };
    let parts = names
        .iter()
        .map(|n| parse_part(&n.try_to_string()?))
        .collect::<Result<Vec<_>>>()?;
    let field_types: Vec<LogicalType> = ty.struct_children().iter().map(|(_, t)| t.clone()).collect();
    strict_rows(args, result, |row| {
        if row[1].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let mut values = Vec::with_capacity(parts.len());
        for (part, field) in parts.iter().zip(&field_types) {
            // `DatePart::StructOperator`: a DATE has its time zone parts as 0, where the scalar parts refuse them
            let zone = matches!(
                part,
                DatePart::Timezone | DatePart::TimezoneHour | DatePart::TimezoneMinute
            );
            if zone && row[1].logical_type().id() == LogicalTypeId::Date {
                values.push(if moment(&row[1]).is_some() {
                    Value::bigint(0)
                } else {
                    Value::null(field.clone())
                });
                continue;
            }
            values.push(part_value(*part, &row[1], field)?);
        }
        Value::new(ty.clone(), ValueData::Struct(values))
    })
}

fn date_part_call(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        // an infinity has no parts, whatever the specifier says
        if !row[1].is_null() && moment(&row[1]).is_none() {
            return Ok(Value::null(ty.clone()));
        }
        part_value(parse_part(&row[0].try_to_string()?)?, &row[1], &ty)
    })
}

/// `DatePart::YearOperator` and the like on a whole vector of finite dates into BIGINT: the parts
/// from the days, without a value per row. `false` (nothing written) for anything else.
fn date_parts_of_dates(args: &DataChunk, result: &mut Vector, part: DatePart) -> Result<bool> {
    use pintail_common::types::Date;
    if !matches!(
        part,
        DatePart::Year | DatePart::Month | DatePart::Day | DatePart::Quarter
    ) || args.columns()[0].logical_type().id() != LogicalTypeId::Date
        || result.logical_type().id() != LogicalTypeId::BigInt
    {
        return Ok(false);
    }
    let input = &args.columns()[0];
    let constant = input.vector_type() == pintail_common::vector::VectorType::Constant;
    let rows = if constant { 1 } else { args.len() };
    let data = input.to_unified(rows);
    let days = data.data::<i32>();
    let mut values = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = data.sel.get(row);
        if !data.validity.is_valid(index) {
            values.push(None);
            continue;
        }
        let date = Date(days[index]);
        if !date.is_finite() {
            return Ok(false);
        }
        let (year, month, day) = date.to_ymd();
        values.push(Some(i64::from(match part {
            DatePart::Year => year,
            DatePart::Month => month,
            DatePart::Day => day,
            _ => (month - 1) / 3 + 1,
        })));
    }
    result.prepare_flat(rows.max(1));
    let out = result.data_mut::<i64>();
    for (row, value) in values.iter().enumerate() {
        if let Some(value) = value {
            out[row] = *value;
        }
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

macro_rules! part_function {
    ($name:ident, $part:ident) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            if date_parts_of_dates(args, result, DatePart::$part)? {
                return Ok(());
            }
            let ty = result.logical_type().clone();
            strict_rows(args, result, |row| part_value(DatePart::$part, &row[0], &ty))
        }
    };
}

part_function!(year, Year);
part_function!(month, Month);
part_function!(day, Day);
part_function!(decade, Decade);
part_function!(century, Century);
part_function!(millennium, Millennium);
part_function!(quarter, Quarter);
part_function!(dayofweek, Dow);
part_function!(isodow, IsoDow);
part_function!(dayofyear, Doy);
part_function!(week, Week);
part_function!(yearweek, YearWeek);
part_function!(isoyear, IsoYear);
part_function!(era, Era);
part_function!(hour, Hour);
part_function!(minute, Minute);
part_function!(second, Second);
part_function!(millisecond, Milliseconds);
part_function!(microsecond, Microseconds);
part_function!(timezone, Timezone);
part_function!(timezone_hour, TimezoneHour);
part_function!(timezone_minute, TimezoneMinute);
part_function!(epoch, Epoch);
part_function!(julian, JulianDay);

fn micros_of(moment: Moment) -> Option<i64> {
    split(moment).map(|(days, time)| i64::from(days) * MICROS_PER_DAY + time)
}

fn epoch_scaled(args: &DataChunk, result: &mut Vector, scale: i128) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let id = row[0].logical_type().id();
        // a TIMESTAMP_NS keeps its nanoseconds
        if scale == 1_000_000
            && matches!(id, LogicalTypeId::TimestampNs | LogicalTypeId::TimestampTzNs)
            && let Some(raw) = row[0].as_i128().map(|raw| raw as i64)
        {
            if raw == i64::MAX || raw == -i64::MAX {
                return Ok(Value::null(ty.clone()));
            }
            return Ok(Value::bigint(raw));
        }
        let Some(m) = moment(&row[0]) else {
            return Ok(Value::null(ty.clone()));
        };
        // `Date::EpochMilliseconds`, and the `TryMultiplyOperator` of the finer units
        if let Moment::Date(days) = m {
            let (per_day, unit) = match scale {
                1 => (MICROS_PER_DAY / 1000, "milliseconds"),
                1000 => (MICROS_PER_DAY, "microseconds"),
                _ => (MICROS_PER_DAY * 1000, "nanoseconds"),
            };
            return i64::from(days)
                .checked_mul(per_day)
                .map(Value::bigint)
                .ok_or_else(|| conversion(format!("Could not convert DATE ({}) to {unit}", Date(days))));
        }
        let Some(micros) = micros_of(m) else {
            return Ok(Value::null(ty.clone()));
        };
        Ok(Value::bigint(match scale {
            // `CastTimestampPrecisionOperation` to milliseconds rounds away from the epoch
            1 if !matches!(m, Moment::Date(_)) => {
                let half = micros / 500;
                (if half < 0 { half - 1 } else { half + 1 }) / 2
            }
            1 => micros / 1000,
            1000 => micros,
            _ => micros.checked_mul(1000).ok_or_else(|| match m {
                Moment::Date(days) => conversion(format!("Could not convert DATE ({}) to nanoseconds", Date(days))),
                _ => conversion("Could not convert Timestamp(US) to Timestamp(NS)".to_string()),
            })?,
        }))
    })
}

fn epoch_ms(args: &DataChunk, result: &mut Vector) -> Result<()> {
    epoch_scaled(args, result, 1)
}

fn epoch_us(args: &DataChunk, result: &mut Vector) -> Result<()> {
    epoch_scaled(args, result, 1000)
}

fn epoch_ns(args: &DataChunk, result: &mut Vector) -> Result<()> {
    epoch_scaled(args, result, 1_000_000)
}

/// `EpochMillisOperator`, `EpochMicrosecondsOperator` and `EpochNanosecondsOperator` of an
/// INTERVAL (`Interval::GetMilli`, `GetMicro`, `GetNanoseconds`) or a TIME.
fn epoch_of_interval_or_time(value: &Value, unit: i64) -> Result<Value> {
    if value.logical_type().id() == LogicalTypeId::TimeNs {
        let nanos = value.as_i128().expect("a TIME_NS") as i64;
        return Ok(Value::bigint(match unit {
            1 => nanos / 1_000_000,
            1000 => nanos / 1000,
            _ => nanos,
        }));
    }
    // of a TIMETZ its time of day, at its own offset
    let time = match (value.logical_type().id(), value.data()) {
        (LogicalTypeId::Time, _) => Some(value.as_i128().expect("a TIME") as i64),
        (LogicalTypeId::TimeTz, ValueData::Int64(bits)) => Some(TimeTz(*bits as u64).time().0),
        _ => None,
    };
    if let Some(micros) = time {
        return Ok(Value::bigint(match unit {
            1 => micros / 1000,
            1000 => micros,
            _ => micros * 1000,
        }));
    }
    let interval = interval_of(value);
    Ok(Value::bigint(match unit {
        1 => {
            let failed = || conversion("Could not convert Interval to Milliseconds".to_string());
            let months = i64::from(interval.months)
                .checked_mul(Interval::DAYS_PER_MONTH * MICROS_PER_DAY / 1000)
                .ok_or_else(failed)?;
            let days = i64::from(interval.days)
                .checked_mul(MICROS_PER_DAY / 1000)
                .ok_or_else(failed)?;
            (interval.micros / 1000)
                .checked_add(months)
                .and_then(|m| m.checked_add(days))
                .ok_or_else(failed)?
        }
        1000 => super::holistic::interval_micros(&interval)?,
        _ => super::holistic::interval_micros(&interval)?
            .checked_mul(1000)
            .ok_or_else(|| conversion("Could not convert Interval to Nanoseconds".to_string()))?,
    }))
}

fn epoch_ms_of_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| epoch_of_interval_or_time(&row[0], 1))
}

fn epoch_us_of_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| epoch_of_interval_or_time(&row[0], 1000))
}

fn epoch_ns_of_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| epoch_of_interval_or_time(&row[0], 1_000_000))
}

/// `MakeDateFromEpoch`: the days since 1970-01-01, as they are.
fn make_date_from_epoch(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(date_value(row[0].as_i128().expect("an INTEGER") as i32))
    })
}

fn epoch_ms_to_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let millis = row[0].as_i128().expect("a BIGINT");
        let micros =
            i64::try_from(millis * 1000).map_err(|_| conversion("Could not convert Timestamp(MS) to Timestamp(US)"))?;
        Ok(Value::timestamp(Timestamp(micros)))
    })
}

fn nanosecond(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        // `NanosecondsOperator`: the nanoseconds of the minute, exactly for the types that keep them
        if matches!(
            row[0].logical_type().id(),
            LogicalTypeId::TimeNs | LogicalTypeId::TimestampNs
        ) && let Some(nanos) = row[0].as_i128()
        {
            return Ok(Value::bigint(nanos.rem_euclid(60_000_000_000) as i64));
        }
        Ok(match moment(&row[0]) {
            Some(m) => match part_of(DatePart::Microseconds, m)? {
                Part::Integer(v) => Value::bigint(v * 1000),
                Part::Double(v) => Value::bigint(v as i64),
            },
            None => Value::null(LogicalType::BIGINT),
        })
    })
}

fn date_value(days: i32) -> Value {
    Value::date(Date(days))
}

fn last_day(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let Some((days, _)) = moment(&row[0]).and_then(split) else {
            return Ok(Value::null(LogicalType::DATE));
        };
        let (year, month, _) = Date(days).to_ymd();
        // `LastDayOperator`: the day before the first of the next month, which must be a date itself
        let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
        if Date::from_ymd(next_year, next_month, 1).is_none() {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                format!("Date out of range: {next_year}-{next_month}-1"),
            ));
        }
        let last = Date::from_ymd(year, month, Date::month_days(year, month))
            .ok_or_else(|| Error::new(ErrorKind::OutOfRange, format!("Date out of range: {year}-{month}")))?;
        Ok(Value::date(last))
    })
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn monthname(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(match moment(&row[0]).and_then(split) {
            Some((days, _)) => Value::varchar(MONTH_NAMES[Date(days).to_ymd().1 as usize - 1]),
            None => Value::null(LogicalType::VARCHAR),
        })
    })
}

fn dayname(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(match moment(&row[0]).and_then(split) {
            Some((days, _)) => Value::varchar(DAY_NAMES[weekday_from_sunday(days) as usize]),
            None => Value::null(LogicalType::VARCHAR),
        })
    })
}

/// `TruncateElement`: the micros since the epoch of the start of `part`.
fn truncate(part: DatePart, micros: i64) -> Result<i64> {
    let days = micros.div_euclid(MICROS_PER_DAY) as i32;
    let time = micros.rem_euclid(MICROS_PER_DAY);
    // the start of a day before the first timestamp is out of range
    let start_of_day = |days: i32| {
        i64::from(days)
            .checked_mul(MICROS_PER_DAY)
            .filter(|&m| Timestamp(m).is_finite())
            .ok_or_else(|| conversion("Date out of range".to_string()))
    };
    let (year, month, _) = Date(days).to_ymd();
    let ymd = |year: i32, month: i32| {
        Date::from_ymd(year, month, 1)
            .and_then(|d| i64::from(d.0).checked_mul(MICROS_PER_DAY))
            .ok_or_else(|| conversion("Date out of range".to_string()))
    };
    let floor_to = |value: i32, size: i32| value.div_euclid(size) * size;
    match part {
        DatePart::Millennium => ymd(floor_to(year, 1000), 1),
        DatePart::Century => ymd(floor_to(year, 100), 1),
        DatePart::Decade => ymd(floor_to(year, 10), 1),
        DatePart::Year => ymd(year, 1),
        DatePart::Quarter => ymd(year, (month - 1) / 3 * 3 + 1),
        DatePart::Month => ymd(year, month),
        DatePart::Week | DatePart::YearWeek => start_of_day(days - weekday_from_monday(days)),
        DatePart::IsoYear => {
            let monday = days - weekday_from_monday(days);
            start_of_day(monday - (iso_week(monday).1 - 1) * 7)
        }
        DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => start_of_day(days),
        DatePart::Hour => Ok(start_of_day(days)? + time / MICROS_PER_HOUR * MICROS_PER_HOUR),
        DatePart::Minute => Ok(start_of_day(days)? + time / MICROS_PER_MINUTE * MICROS_PER_MINUTE),
        DatePart::Second | DatePart::Epoch => Ok(start_of_day(days)? + time / MICROS_PER_SEC * MICROS_PER_SEC),
        DatePart::Milliseconds => Ok(start_of_day(days)? + time / 1000 * 1000),
        DatePart::Microseconds => Ok(micros),
        other => Err(unsupported_part("timestamp", other)),
    }
}

fn date_trunc(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let part = parse_part(&row[0].try_to_string()?)?;
        let micros = match moment(&row[1]) {
            Some(Moment::Interval(interval)) => {
                return Ok(Value::interval(truncate_interval(part, interval)?));
            }
            Some(Moment::Date(days)) => i64::from(days).checked_mul(MICROS_PER_DAY).ok_or_else(|| {
                conversion(match part {
                    DatePart::Hour
                    | DatePart::Minute
                    | DatePart::Second
                    | DatePart::Epoch
                    | DatePart::Milliseconds
                    | DatePart::Microseconds => format!("Date {} not in timestamp range", Date(days).0),
                    _ => "Date out of range".to_string(),
                })
            })?,
            Some(m) => micros_of(m).expect("a date or a timestamp"),
            None => {
                let raw = row[1].as_i128().unwrap_or(0);
                return Ok(Value::timestamp(if raw > 0 {
                    Timestamp::INFINITY
                } else {
                    Timestamp::NINFINITY
                }));
            }
        };
        let truncated = truncate(part, micros)?;
        Value::new(ty.clone(), ValueData::Int64(truncated))
    })
}

fn truncate_interval(part: DatePart, interval: Interval) -> Result<Interval> {
    let Interval { months, days, micros } = interval;
    let keep = |micros_unit: i64| Interval {
        months,
        days,
        micros: micros / micros_unit * micros_unit,
    };
    let months_only = |size: i32| Interval {
        months: months / size * size,
        days: 0,
        micros: 0,
    };
    Ok(match part {
        DatePart::Millennium => months_only(12000),
        DatePart::Century => months_only(1200),
        DatePart::Decade => months_only(120),
        DatePart::Year | DatePart::IsoYear => months_only(12),
        DatePart::Quarter => months_only(3),
        DatePart::Month => months_only(1),
        DatePart::Week | DatePart::YearWeek => Interval {
            months,
            days: days / 7 * 7,
            micros: 0,
        },
        DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => Interval {
            months,
            days,
            micros: 0,
        },
        DatePart::Hour => keep(MICROS_PER_HOUR),
        DatePart::Minute => keep(MICROS_PER_MINUTE),
        DatePart::Second | DatePart::Epoch => keep(MICROS_PER_SEC),
        DatePart::Milliseconds => keep(1000),
        DatePart::Microseconds => interval,
        other => return Err(unsupported_part("interval", other)),
    })
}

/// `DateDiff`: how many boundaries of `part` lie between two moments.
fn date_diff(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let part = parse_part(&row[0].try_to_string()?)?;
        let (Some(start), Some(end)) = (moment(&row[1]), moment(&row[2])) else {
            return Ok(Value::null(ty.clone()));
        };
        if let (Moment::Time(start), Moment::Time(end)) = (start, end) {
            return Ok(Value::bigint(diff_micros(part, start, end, "time")?));
        }
        let ((first, first_time), (last, last_time)) = (
            split(start).expect("a date or a timestamp"),
            split(end).expect("a date or a timestamp"),
        );
        let start = i64::from(first)
            .saturating_mul(MICROS_PER_DAY)
            .saturating_add(first_time);
        let end = i64::from(last).saturating_mul(MICROS_PER_DAY).saturating_add(last_time);
        let (y1, m1, _) = Date(first).to_ymd();
        let (y2, m2, _) = Date(last).to_ymd();
        let (y1, m1, y2, m2) = (i64::from(y1), i64::from(m1), i64::from(y2), i64::from(m2));
        Ok(Value::bigint(match part {
            DatePart::Year => y2 - y1,
            DatePart::Month => (y2 * 12 + m2) - (y1 * 12 + m1),
            DatePart::Quarter => (y2 * 4 + (m2 - 1) / 3) - (y1 * 4 + (m1 - 1) / 3),
            DatePart::Decade => y2 / 10 - y1 / 10,
            DatePart::Century => y2 / 100 - y1 / 100,
            DatePart::Millennium => y2 / 1000 - y1 / 1000,
            DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => {
                i64::from(last) - i64::from(first)
            }
            DatePart::Week | DatePart::YearWeek => (i64::from(last) - i64::from(first)) / 7,
            DatePart::IsoYear => i64::from(iso_week(last).0 - iso_week(first).0),
            DatePart::Era => i64::from(y2 > 0) - i64::from(y1 > 0),
            DatePart::Microseconds => {
                let moment = |days: i32, time: i64| i128::from(days) * i128::from(MICROS_PER_DAY) + i128::from(time);
                i64::try_from(moment(last, last_time) - moment(first, first_time))
                    .map_err(|_| Error::new(ErrorKind::OutOfRange, "Overflow in microsecond subtraction"))?
            }
            other => diff_micros(other, start, end, "timestamp")?,
        }))
    })
}

fn diff_micros(part: DatePart, start: i64, end: i64, kind: &str) -> Result<i64> {
    let floor = |unit: i64| end.div_euclid(unit) - start.div_euclid(unit);
    Ok(match part {
        DatePart::Hour => floor(MICROS_PER_HOUR),
        DatePart::Minute => floor(MICROS_PER_MINUTE),
        DatePart::Second | DatePart::Epoch => floor(MICROS_PER_SEC),
        DatePart::Milliseconds => floor(1000),
        DatePart::Microseconds => end - start,
        other => return Err(unsupported_part(kind, other)),
    })
}

/// `DateSub`: how many whole `part`s lie between two moments.
fn date_sub(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let part = parse_part(&row[0].try_to_string()?)?;
        let (Some(start), Some(end)) = (moment(&row[1]), moment(&row[2])) else {
            return Ok(Value::null(ty.clone()));
        };
        let (start, end) = match (start, end) {
            (Moment::Time(start), Moment::Time(end)) => (start, end),
            (start, end) => (
                micros_of(start).expect("a date or a timestamp"),
                micros_of(end).expect("a date or a timestamp"),
            ),
        };
        // `Interval::GetDifferenceInUnits`: in HUGEINT where the microseconds do not fit
        let difference = i128::from(end) - i128::from(start);
        let units = |per_unit: i64| {
            i64::try_from(difference / i128::from(per_unit))
                .map_err(|_| out_of_range("Overflow in the difference of two timestamps"))
        };
        let months = || whole_months(start, end);
        Ok(Value::bigint(match part {
            DatePart::Millennium => months() / 12000,
            DatePart::Century => months() / 1200,
            DatePart::Decade => months() / 120,
            DatePart::Year | DatePart::IsoYear => months() / 12,
            DatePart::Quarter => months() / 3,
            DatePart::Month => months(),
            DatePart::Week | DatePart::YearWeek => units(7 * MICROS_PER_DAY)?,
            DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => {
                units(MICROS_PER_DAY)?
            }
            DatePart::Hour => units(MICROS_PER_HOUR)?,
            DatePart::Minute => units(MICROS_PER_MINUTE)?,
            DatePart::Second | DatePart::Epoch => units(MICROS_PER_SEC)?,
            DatePart::Milliseconds => units(1000)?,
            DatePart::Microseconds => units(1)?,
            other => return Err(unsupported_part("timestamp", other)),
        }))
    })
}

fn whole_months(start: i64, end: i64) -> i64 {
    let (start, end, sign) = if end < start { (end, start, -1) } else { (start, end, 1) };
    let (first, first_time) = (
        start.div_euclid(MICROS_PER_DAY) as i32,
        start.rem_euclid(MICROS_PER_DAY),
    );
    let (last, last_time) = (end.div_euclid(MICROS_PER_DAY) as i32, end.rem_euclid(MICROS_PER_DAY));
    let (y1, m1, d1) = Date(first).to_ymd();
    let (y2, m2, d2) = Date(last).to_ymd();
    let mut months = (i64::from(y2) * 12 + i64::from(m2)) - (i64::from(y1) * 12 + i64::from(m1));
    // the last day of a month counts as every later day of a month
    let month_end = d2 == Date::month_days(y2, m2);
    if (d2, last_time) < (d1, first_time) && !(month_end && last_time >= first_time) {
        months -= 1;
    }
    months * sign
}

/// `Interval::GetAge`.
fn age(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| age_of(&row[0], &row[1]))
}

/// `AgeFunctionStandard`: the age from today at midnight, NULL for an infinity.
fn age_from_today(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let today = Value::timestamp(Timestamp((now_micros() / MICROS_PER_DAY) * MICROS_PER_DAY));
    strict_rows(args, result, |row| age_of(&today, &row[0]))
}

/// `Interval::GetAge`.
fn age_of(left: &Value, right: &Value) -> Result<Value> {
    {
        let (Some(later), Some(earlier)) = (moment(left).and_then(micros_of), moment(right).and_then(micros_of)) else {
            return Ok(Value::null(LogicalType::INTERVAL));
        };
        let (flip, later, earlier) = if later < earlier {
            (true, earlier, later)
        } else {
            (false, later, earlier)
        };
        let (d1, t1) = (
            later.div_euclid(MICROS_PER_DAY) as i32,
            later.rem_euclid(MICROS_PER_DAY),
        );
        let (d2, t2) = (
            earlier.div_euclid(MICROS_PER_DAY) as i32,
            earlier.rem_euclid(MICROS_PER_DAY),
        );
        let (y1, m1, day1) = Date(d1).to_ymd();
        let (y2, m2, day2) = Date(d2).to_ymd();
        let mut micros = t1 - t2;
        let mut days = day1 - day2;
        let mut months = (y1 - y2) * 12 + (m1 - m2);
        if micros < 0 {
            micros += MICROS_PER_DAY;
            days -= 1;
        }
        if days < 0 {
            days += Date::month_days(y2, m2);
            months -= 1;
        }
        let sign = if flip { -1 } else { 1 };
        Ok(Value::interval(Interval {
            months: months * sign,
            days: days * sign,
            micros: micros * i64::from(sign),
        }))
    }
}

/// `Cast::Operation<int64_t, int32_t>` of a date part.
fn int32_part(value: i128) -> Result<i32> {
    i32::try_from(value).map_err(|_| {
        conversion(format!(
            "Type INT64 with value {value} can't be cast because the value is out of range for the destination type \
             INT32"
        ))
    })
}

/// `MakeDateOperator`: the parts are cast to INT32 first (`Date::FromDate`).
fn date_from_parts(year: i128, month: i128, day: i128) -> Result<Value> {
    let (y, m, d) = (int32_part(year)?, int32_part(month)?, int32_part(day)?);
    Date::from_ymd(y, m, d)
        .map(Value::date)
        .ok_or_else(|| conversion(format!("Date out of range: {year}-{month}-{day}")))
}

fn make_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let part = |at: usize| row[at].as_i128().expect("a BIGINT");
        date_from_parts(part(0), part(1), part(2))
    })
}

/// `ExecuteStructMakeDate`: NULL if the struct or any of its parts is.
fn make_date_from_struct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::Struct(fields) = row[0].data() else {
            return Ok(Value::null(LogicalType::DATE));
        };
        match (fields[0].as_i128(), fields[1].as_i128(), fields[2].as_i128()) {
            (Some(year), Some(month), Some(day)) => date_from_parts(year, month, day),
            _ => Ok(Value::null(LogicalType::DATE)),
        }
    })
}

fn time_micros(hour: i128, minute: i128, seconds: f64) -> Result<i64> {
    if !seconds.is_finite() {
        return Err(conversion("Time out of range: seconds must be a finite number"));
    }
    // `MakeTimeOperator`: the seconds are floored, and `Time::IsValidTime` takes a leap second as long
    // as the time does not exceed 24:00:00
    let whole = seconds.floor() as i64;
    let micros = ((seconds - whole as f64) * 1e6).round() as i64;
    let total = ((hour as i64 * 60 + minute as i64) * 60 + whole) * MICROS_PER_SEC + micros;
    let valid = (0..24).contains(&hour)
        && (0..60).contains(&minute)
        && (0..=60).contains(&whole)
        && (0..=1_000_000).contains(&micros)
        && total <= MICROS_PER_DAY;
    let at_midnight = hour == 24 && minute == 0 && whole == 0 && micros == 0;
    if !(valid || at_midnight) {
        return Err(conversion(format!(
            "Time out of range: {hour}:{minute}:{whole}.{micros:06}"
        )));
    }
    Ok(hour as i64 * MICROS_PER_HOUR + minute as i64 * MICROS_PER_MINUTE + whole * MICROS_PER_SEC + micros)
}

fn seconds_of(value: &Value) -> f64 {
    match value.data() {
        ValueData::Double(v) => *v,
        _ => value.as_i128().expect("a number") as f64,
    }
}

fn make_time(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let micros = time_micros(
            row[0].as_i128().expect("a BIGINT"),
            row[1].as_i128().expect("a BIGINT"),
            seconds_of(&row[2]),
        )?;
        Ok(Value::time(Time(micros)))
    })
}

/// `MakeTimestampNsFun`: nanoseconds since the epoch.
fn make_timestamp_ns(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let nanos = i64::try_from(row[0].as_i128().expect("a BIGINT"))
            .map_err(|_| conversion("Timestamp nanoseconds out of range"))?;
        // `MakeTimestampOperator`: an infinity is out of range
        if !Timestamp(nanos).is_finite() {
            return Err(conversion(format!("Timestamp microseconds out of range: {nanos}")));
        }
        Value::new(LogicalType::new(LogicalTypeId::TimestampNs), ValueData::Int64(nanos))
    })
}

fn make_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        if row.len() == 1 {
            let micros = i64::try_from(row[0].as_i128().expect("a BIGINT"))
                .map_err(|_| conversion("Timestamp microseconds out of range"))?;
            if !Timestamp(micros).is_finite() {
                return Err(conversion(format!("Timestamp microseconds out of range: {micros}")));
            }
            return Ok(Value::timestamp(Timestamp(micros)));
        }
        let part = |at: usize| row[at].as_i128().expect("a BIGINT");
        let (year, month, day) = (part(0), part(1), part(2));
        let (y, m, d) = (int32_part(year)?, int32_part(month)?, int32_part(day)?);
        let date =
            Date::from_ymd(y, m, d).ok_or_else(|| conversion(format!("Date out of range: {year}-{month}-{day}")))?;
        let time = time_micros(part(3), part(4), seconds_of(&row[5]))?;
        let micros = i64::from(date.0)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .filter(|&m| Timestamp(m).is_finite())
            .ok_or_else(|| conversion("Date and time not in timestamp range"))?;
        Ok(Value::timestamp(Timestamp(micros)))
    })
}

fn to_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        // `EpochSecOperator`: the microseconds have to fit a BIGINT
        let micros = (seconds_of(&row[0]) * 1e6).round_ties_even();
        if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&micros) {
            return Err(conversion("Epoch seconds out of range for TIMESTAMP WITH TIME ZONE"));
        }
        Ok(Value::timestamp_tz(Timestamp(micros as i64)))
    })
}

/// `MetaTransaction::start_timestamp`: the time the transaction started (`CurrentTimestampFunction`).
fn now_micros() -> i64 {
    crate::execution::temporary::current().transaction_start()
}

fn now(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        result.set_value(row, &Value::timestamp_tz(Timestamp(now_micros())))?;
    }
    Ok(())
}

fn current_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    for row in 0..args.len() {
        result.set_value(row, &date_value((now_micros() / MICROS_PER_DAY) as i32))?;
    }
    Ok(())
}

macro_rules! to_interval {
    ($name:ident, $unit:expr, $input:ident, $value:expr) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            strict_rows(args, result, |row| {
                let $input = row[0].as_i128().expect("an integer");
                // `To*Operator`: "Interval value <n> <unit> out of range"
                let value: Option<Interval> = $value;
                value
                    .map(Value::interval)
                    .ok_or_else(|| out_of_range(&format!("Interval value {} {} out of range", $input, $unit)))
            })
        }
    };
}

fn months_interval(months: i128) -> Option<Interval> {
    Some(Interval {
        months: i32::try_from(months).ok()?,
        days: 0,
        micros: 0,
    })
}

fn days_interval(days: i128) -> Option<Interval> {
    Some(Interval {
        months: 0,
        days: i32::try_from(days).ok()?,
        micros: 0,
    })
}

fn micros_interval(micros: i128) -> Option<Interval> {
    Some(Interval {
        months: 0,
        days: 0,
        micros: i64::try_from(micros).ok()?,
    })
}

to_interval!(to_years, "years", n, months_interval(n * 12));
to_interval!(to_quarters, "quarters", n, months_interval(n * 3));
to_interval!(to_months, "months", n, months_interval(n));
to_interval!(to_decades, "decades", n, months_interval(n * 120));
to_interval!(to_centuries, "centuries", n, months_interval(n * 1200));
to_interval!(to_millennia, "millennia", n, months_interval(n * 12000));
to_interval!(to_weeks, "weeks", n, days_interval(n * 7));
to_interval!(to_days, "days", n, days_interval(n));
to_interval!(to_hours, "hours", n, micros_interval(n * i128::from(MICROS_PER_HOUR)));
to_interval!(
    to_minutes,
    "minutes",
    n,
    micros_interval(n * i128::from(MICROS_PER_MINUTE))
);
to_interval!(to_microseconds, "microseconds", n, micros_interval(n));

fn to_seconds(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let micros = (seconds_of(&row[0]) * 1e6).round();
        micros_interval(micros as i128)
            .map(Value::interval)
            .ok_or_else(|| out_of_range("Interval value out of range"))
    })
}

fn to_milliseconds(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let micros = (seconds_of(&row[0]) * 1e3).round();
        micros_interval(micros as i128)
            .map(Value::interval)
            .ok_or_else(|| out_of_range("Interval value out of range"))
    })
}

/// `Interval::Add` for a timestamp.
pub(crate) fn add_interval(micros: i64, interval: Interval) -> Result<i64> {
    let overflow = || out_of_range("Overflow in interval addition");
    // `Timestamp::FromDatetime`: the moment has to fit, and be finite
    let out_of_timestamp_range = || Error::new(ErrorKind::Conversion, "Date and time not in timestamp range");
    // `Interval::Add(date_t, interval_t)`
    let date_out_of_range = || out_of_range("Date out of range");
    let mut micros = micros;
    if interval.months != 0 {
        let days = micros.div_euclid(MICROS_PER_DAY) as i32;
        let time = micros.rem_euclid(MICROS_PER_DAY);
        let (year, month, day) = Date(days).to_ymd();
        let total = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(interval.months);
        let year = i32::try_from(total.div_euclid(12)).map_err(|_| date_out_of_range())?;
        let month = total.rem_euclid(12) as i32 + 1;
        let day = day.min(Date::month_days(year, month));
        let date = Date::from_ymd(year, month, day).ok_or_else(date_out_of_range)?;
        micros = i64::from(date.0)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .ok_or_else(out_of_timestamp_range)?;
    }
    micros
        .checked_add(
            i64::from(interval.days)
                .checked_mul(MICROS_PER_DAY)
                .ok_or_else(overflow)?,
        )
        .and_then(|m| m.checked_add(interval.micros))
        .filter(|m| Timestamp(*m).is_finite())
        .ok_or_else(out_of_timestamp_range)
}

fn negated(interval: Interval) -> Interval {
    Interval {
        months: interval.months.wrapping_neg(),
        days: interval.days.wrapping_neg(),
        micros: interval.micros.wrapping_neg(),
    }
}

fn interval_of(value: &Value) -> Interval {
    match value.data() {
        ValueData::Interval(interval) => *interval,
        _ => unreachable!("an INTERVAL"),
    }
}

/// A moment moved by an interval, with infinities staying where they are.
fn shifted(value: &Value, interval: Interval, ty: &LogicalType) -> Result<Value> {
    // `Interval::Add(dtime_tz_t)`: the time moves, the offset stays
    if ty.id() == LogicalTypeId::TimeTz {
        let ValueData::Int64(bits) = value.data() else {
            return Ok(Value::null(ty.clone()));
        };
        let value = pintail_common::types::TimeTz(*bits as u64);
        let moved = (value.time().0 + interval.micros).rem_euclid(MICROS_PER_DAY);
        return Ok(Value::time_tz(pintail_common::types::TimeTz::new(
            Time(moved),
            value.offset(),
        )));
    }
    if ty.id() == LogicalTypeId::Time {
        let time = value.as_i128().expect("a TIME") as i64;
        let moved = (time + interval.micros).rem_euclid(MICROS_PER_DAY);
        return Ok(Value::time(Time(moved)));
    }
    // `Interval::TryAdd(timestamp_ns_t)`: the microseconds move, the nanoseconds stay
    if ty.id() == LogicalTypeId::TimestampNs {
        let Some(raw) = value.as_i128().map(|raw| raw as i64) else {
            return Ok(Value::null(ty.clone()));
        };
        if raw == i64::MAX || raw == -i64::MAX {
            return Value::new(ty.clone(), ValueData::Int64(raw));
        }
        let failed = || out_of_range("Cannot add interval to timestamp_ns");
        let (micros, nanos) = (raw.div_euclid(1000), raw.rem_euclid(1000));
        let moved = add_interval(micros, interval).map_err(|_| failed())?;
        // `Timestamp::TryCombineUnits`
        let combined = moved.checked_mul(1000).and_then(|w| w.checked_add(nanos)).or_else(|| {
            if moved >= 0 {
                return None;
            }
            (moved + 1).checked_mul(1000).and_then(|w| w.checked_add(nanos - 1000))
        });
        return Value::new(ty.clone(), ValueData::Int64(combined.ok_or_else(failed)?));
    }
    let Some(moment) = moment(value) else {
        return match value.as_i128() {
            Some(raw) => {
                let positive = raw > 0;
                Ok(Value::timestamp(if positive {
                    Timestamp::INFINITY
                } else {
                    Timestamp::NINFINITY
                }))
            }
            None => Ok(Value::null(ty.clone())),
        };
    };
    let micros = micros_of(moment).expect("a date or a timestamp");
    Value::new(ty.clone(), ValueData::Int64(add_interval(micros, interval)?))
}

fn moment_plus_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| shifted(&row[0], interval_of(&row[1]), &ty))
}

fn interval_plus_moment(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| shifted(&row[1], interval_of(&row[0]), &ty))
}

fn moment_minus_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let interval = interval_of(&row[1]);
        // `Interval::Invert`, of which a time only takes the micros (`SubtractTimeOperator`)
        let time = matches!(ty.id(), LogicalTypeId::Time | LogicalTypeId::TimeTz);
        if !time && interval.days == i32::MIN {
            return Err(out_of_range("Interval days value out of range"));
        }
        if interval.micros == i64::MIN {
            return Err(out_of_range("Interval micros value out of range"));
        }
        if !time && interval.months == i32::MIN {
            return Err(out_of_range("Interval months value out of range"));
        }
        shifted(&row[0], negated(interval), &ty)
    })
}

fn days_shifted(value: &Value, days: i128) -> Result<Value> {
    let raw = value.as_i128().expect("a DATE") as i32;
    if !Date(raw).is_finite() {
        return Ok(value.clone());
    }
    // `TryAddOperator`: dates below -infinity are out of range, as input and as result
    if raw < Date::NINFINITY.0 {
        return Err(out_of_range("Date out of range"));
    }
    let moved = i128::from(raw) + days;
    i32::try_from(moved)
        .ok()
        .filter(|d| *d > Date::NINFINITY.0 && *d < Date::INFINITY.0)
        .map(date_value)
        .ok_or_else(|| out_of_range("Date out of range"))
}

fn date_plus_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        days_shifted(&row[0], row[1].as_i128().expect("an INTEGER"))
    })
}

fn integer_plus_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        days_shifted(&row[1], row[0].as_i128().expect("an INTEGER"))
    })
}

fn date_minus_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        days_shifted(&row[0], -row[1].as_i128().expect("an INTEGER"))
    })
}

fn date_minus_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (left, right) = (row[0].as_i128().expect("a DATE"), row[1].as_i128().expect("a DATE"));
        if !Date(left as i32).is_finite() || !Date(right as i32).is_finite() {
            return Err(out_of_range("Date difference of infinite dates"));
        }
        Ok(Value::bigint((left - right) as i64))
    })
}

/// `AddOperator::Operation(date_t, dtime_t)`: an infinite date stays infinite.
fn date_and_time(date: &Value, time: &Value) -> Result<Value> {
    let days = date.as_i128().expect("a DATE") as i32;
    if days == Date::INFINITY.0 {
        return Ok(Value::timestamp(Timestamp::INFINITY));
    }
    if days == Date::NINFINITY.0 {
        return Ok(Value::timestamp(Timestamp::NINFINITY));
    }
    let time = time.as_i128().expect("a TIME") as i64;
    i64::from(days)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|m| m.checked_add(time))
        .map(Timestamp)
        .filter(|t| t.is_finite())
        .map(Value::timestamp)
        .ok_or_else(|| out_of_range("Timestamp out of range"))
}

/// `AddOperator::Operation(date_t, dtime_tz_t)`: the instant of the time at its offset that day.
fn date_and_time_tz(date: &Value, time: &Value) -> Result<Value> {
    let tz = LogicalType::new(LogicalTypeId::TimestampTz);
    let days = date.as_i128().expect("a DATE") as i32;
    if days == Date::INFINITY.0 || days == Date::NINFINITY.0 {
        let infinity = if days > 0 {
            Timestamp::INFINITY
        } else {
            Timestamp::NINFINITY
        };
        return Value::new(tz, ValueData::Int64(infinity.0));
    }
    let ValueData::Int64(bits) = time.data() else {
        return Ok(Value::null(tz));
    };
    let time = pintail_common::types::TimeTz(*bits as u64);
    i64::from(days)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|m| m.checked_add(time.time().0))
        .and_then(|m| m.checked_sub(i64::from(time.offset()) * 1_000_000))
        .filter(|m| Timestamp(*m).is_finite())
        .map(|m| Value::new(tz, ValueData::Int64(m)))
        .transpose()?
        .ok_or_else(|| out_of_range("Timestamp with time zone out of range"))
}

fn date_plus_time_tz(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| date_and_time_tz(&row[0], &row[1]))
}

fn time_tz_plus_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| date_and_time_tz(&row[1], &row[0]))
}

fn date_plus_time(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| date_and_time(&row[0], &row[1]))
}

fn time_plus_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| date_and_time(&row[1], &row[0]))
}

fn timestamp_minus_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        // `Interval::GetDifference`
        let (Some(left), Some(right)) = (moment(&row[0]).and_then(micros_of), moment(&row[1]).and_then(micros_of))
        else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot subtract infinite timestamps",
            ));
        };
        // the days between two finite timestamps always fit, even when their microseconds do not
        let difference = i128::from(left) - i128::from(right);
        let per_day = i128::from(MICROS_PER_DAY);
        let out_of_bounds = || Error::new(ErrorKind::Conversion, "Timestamp difference is out of bounds");
        Ok(Value::interval(Interval {
            months: 0,
            days: i32::try_from(difference / per_day).map_err(|_| out_of_bounds())?,
            micros: i64::try_from(difference % per_day).map_err(|_| out_of_bounds())?,
        }))
    })
}

fn interval_arithmetic(left: Interval, right: Interval) -> Result<Interval> {
    let overflow = || out_of_range("Interval value is out of range");
    Ok(Interval {
        months: left.months.checked_add(right.months).ok_or_else(overflow)?,
        days: left.days.checked_add(right.days).ok_or_else(overflow)?,
        micros: left.micros.checked_add(right.micros).ok_or_else(overflow)?,
    })
}

fn interval_plus_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::interval(interval_arithmetic(
            interval_of(&row[0]),
            interval_of(&row[1]),
        )?))
    })
}

/// `SubtractOperator::Operation(interval_t, interval_t)`: each part on its own.
fn interval_minus_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (left, right) = (interval_of(&row[0]), interval_of(&row[1]));
        let failed = |part: &str| out_of_range(format!("Interval {part} subtraction out of range"));
        Ok(Value::interval(Interval {
            months: left.months.checked_sub(right.months).ok_or_else(|| failed("months"))?,
            days: left.days.checked_sub(right.days).ok_or_else(|| failed("days"))?,
            micros: left.micros.checked_sub(right.micros).ok_or_else(|| failed("micros"))?,
        }))
    })
}

/// `MultiplyOperator::Operation(interval_t, int64_t)`: the factor is cast to INT32 for the months
/// and the days, each part checked as `MultiplyOperatorOverflowCheck` does.
fn scale_interval(interval: Interval, factor: i64) -> Result<Interval> {
    let factor32 = i32::try_from(factor).map_err(|_| {
        Error::new(
            ErrorKind::Conversion,
            format!(
                "Type INT64 with value {factor} can't be cast because the value is out of range for the destination \
                 type INT32"
            ),
        )
    })?;
    let overflow = |name: &str, left: String, right: String| {
        out_of_range(format!("Overflow in multiplication of {name} ({left} * {right})!"))
    };
    Ok(Interval {
        months: interval
            .months
            .checked_mul(factor32)
            .ok_or_else(|| overflow("INT32", interval.months.to_string(), factor32.to_string()))?,
        days: interval
            .days
            .checked_mul(factor32)
            .ok_or_else(|| overflow("INT32", interval.days.to_string(), factor32.to_string()))?,
        micros: interval
            .micros
            .checked_mul(factor)
            .ok_or_else(|| overflow("INT64", interval.micros.to_string(), factor.to_string()))?,
    })
}

fn interval_times_bigint(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let factor = row[1].as_i128().expect("a BIGINT") as i64;
        Ok(Value::interval(scale_interval(interval_of(&row[0]), factor)?))
    })
}

fn bigint_times_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let factor = row[0].as_i128().expect("a BIGINT") as i64;
        Ok(Value::interval(scale_interval(interval_of(&row[1]), factor)?))
    })
}

/// `TryMultiplyOperator::Operation(interval_t, double)` (PostgreSQL's `interval_mul`): the whole
/// months and days, with the fractions cascading down into days and then microseconds.
fn scale_interval_double(interval: Interval, factor: f64) -> Result<Interval> {
    let overflow = || out_of_range("Overflow in multiplication of INTERVAL.");
    // `PGTimestampRound`
    let round = |j: f64| (j * 1_000_000.0).round_ties_even() / 1_000_000.0;
    let fits = |d: f64| !d.is_nan() && d >= f64::from(i32::MIN) && d <= f64::from(i32::MAX);
    let months = f64::from(interval.months) * factor;
    if !fits(months) {
        return Err(overflow());
    }
    let result_months = months as i32;
    let days = f64::from(interval.days) * factor;
    if !fits(days) {
        return Err(overflow());
    }
    let mut result_days = days as i32;
    let month_remainder = round((months - f64::from(result_months)) * 30.0);
    let day_remainder = month_remainder as i32;
    const SECS_PER_DAY: f64 = 86_400.0;
    let mut sec_remainder =
        round((days - f64::from(result_days) + month_remainder - f64::from(day_remainder)) * SECS_PER_DAY);
    // there can be 24 hours or more, from rounding or what cascades down
    if sec_remainder.abs() >= SECS_PER_DAY {
        let extra_days = (sec_remainder / SECS_PER_DAY) as i32;
        result_days = result_days.checked_add(extra_days).ok_or_else(overflow)?;
        sec_remainder -= f64::from(extra_days) * SECS_PER_DAY;
    }
    result_days = result_days.checked_add(day_remainder).ok_or_else(overflow)?;
    let micros = (interval.micros as f64 * factor + sec_remainder * 1_000_000.0).round_ties_even();
    if micros.is_nan() || micros < i64::MIN as f64 || micros >= i64::MAX as f64 {
        return Err(overflow());
    }
    Ok(Interval {
        months: result_months,
        days: result_days,
        micros: micros as i64,
    })
}

fn interval_times_double(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::interval(scale_interval_double(
            interval_of(&row[0]),
            seconds_of(&row[1]),
        )?))
    })
}

fn double_times_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::interval(scale_interval_double(
            interval_of(&row[1]),
            seconds_of(&row[0]),
        )?))
    })
}

/// `DivideOperator::Operation(interval_t, double)`: a multiplication by the inverse.
fn interval_divided(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let interval = interval_of(&row[0]);
        scale_interval_double(interval, 1.0 / seconds_of(&row[1]))
            .map(Value::interval)
            .map_err(|_| out_of_range("Overflow in INTERVAL division"))
    })
}

/// `StrfTimeFormat`.
pub(crate) fn strftime_text(format: &str, days: i32, time: i64, zoned: bool) -> Result<String> {
    strftime_text_nanos(format, days, time, 0, zoned)
}

/// `StrfTimeFormat` with the nanoseconds past the microsecond of a TIMESTAMP_NS.
pub(crate) fn strftime_text_nanos(format: &str, days: i32, time: i64, nanos: i64, zoned: bool) -> Result<String> {
    let (year, month, day) = Date(days).to_ymd();
    let hour = time / MICROS_PER_HOUR;
    let minute = (time / MICROS_PER_MINUTE) % 60;
    let second = (time / MICROS_PER_SEC) % 60;
    let fraction = time % MICROS_PER_SEC;
    let hour12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    let doy = days - first_of_year(year) + 1;
    let sunday_week = (doy + 6 - weekday_from_sunday(days)) / 7;
    let monday_week = (doy + 6 - weekday_from_monday(days)) / 7;
    let (iso_year, iso) = iso_week(days);
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let padded = chars.peek() != Some(&'-');
        if !padded {
            chars.next();
        }
        let Some(specifier) = chars.next() else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Failed to parse format specifier {format}: Trailing format character %"),
            ));
        };
        let pad = |value: i64, width: usize| {
            if padded {
                format!("{value:0width$}")
            } else {
                value.to_string()
            }
        };
        match specifier {
            'a' => out += &DAY_NAMES[weekday_from_sunday(days) as usize][..3],
            'A' => out += DAY_NAMES[weekday_from_sunday(days) as usize],
            'b' | 'h' => out += &MONTH_NAMES[month as usize - 1][..3],
            'B' => out += MONTH_NAMES[month as usize - 1],
            'c' => out += &strftime_text("%Y-%m-%d %H:%M:%S", days, time, zoned)?,
            'x' => out += &strftime_text("%Y-%m-%d", days, time, zoned)?,
            'X' => out += &strftime_text("%H:%M:%S", days, time, zoned)?,
            'd' => out += &pad(i64::from(day), 2),
            'e' => out += &format!("{day:2}"),
            'f' => out += &format!("{fraction:06}"),
            'g' => out += &format!("{:03}", fraction / 1000),
            'n' => out += &format!("{:09}", fraction * 1000 + nanos),
            'H' => out += &pad(hour, 2),
            'I' => out += &pad(hour12, 2),
            'j' => out += &pad(i64::from(doy), 3),
            'm' => out += &pad(i64::from(month), 2),
            'M' => out += &pad(minute, 2),
            'p' => out += if hour < 12 { "AM" } else { "PM" },
            'S' => out += &pad(second, 2),
            'u' => out += &(weekday_from_monday(days) + 1).to_string(),
            'w' => out += &weekday_from_sunday(days).to_string(),
            'U' => out += &pad(i64::from(sunday_week), 2),
            'W' => out += &pad(i64::from(monday_week), 2),
            'V' => out += &pad(i64::from(iso), 2),
            'G' if (0..=9999).contains(&iso_year) => out += &format!("{iso_year:04}"),
            'G' => out += &iso_year.to_string(),
            // `AbsValue(year) % 100`
            'y' => out += &pad(i64::from(year.unsigned_abs() % 100), 2),
            'Y' if year < 0 => out += &year.to_string(),
            'Y' => out += &format!("{year:04}"),
            'z' => out += "+00",
            'Z' if zoned => out += "UTC",
            'Z' => {}
            '%' => out.push('%'),
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Failed to parse format specifier {format}: Unrecognized format for strftime/strptime: %{other}"
                    ),
                ));
            }
        }
    }
    Ok(out)
}

fn strftime(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        // both orders of the arguments are spelled the same
        let (value, format) = if row[0].logical_type().id() == LogicalTypeId::Varchar {
            (&row[1], &row[0])
        } else {
            (&row[0], &row[1])
        };
        let Some((days, time)) = moment(value).and_then(split) else {
            return Ok(Value::varchar(value.try_to_string()?));
        };
        let zoned = value.logical_type().id() == LogicalTypeId::TimestampTz;
        let nanos = match value.logical_type().id() {
            LogicalTypeId::TimestampNs => value.as_i128().map_or(0, |raw| (raw as i64).rem_euclid(1000)),
            _ => 0,
        };
        Ok(Value::varchar(strftime_text_nanos(
            &format.try_to_string()?,
            days,
            time,
            nanos,
            zoned,
        )?))
    })
}

/// `StrpTimeFormat::Parse` and `ParseResult::TryToTimestamp`.
pub(crate) fn strptime_micros(input: &str, format: &str) -> std::result::Result<i64, String> {
    let format = StrpFormat::new(format).map_err(|e| format!("Failed to parse format specifier {format}: {e}"))?;
    let mut error = ParseError::default();
    let parsed = format.parse(input, false, &mut error).ok_or(error.message)?;
    parsed.try_timestamp().ok_or_else(|| "Date out of range".to_string())
}

/// `StrpTimeFormat::Parse` with `strict`: a year of one digit is no year.
pub(crate) fn strptime_strict(input: &str, format: &str) -> bool {
    let Ok(format) = StrpFormat::new(format) else {
        return false;
    };
    format
        .parse(input, true, &mut ParseError::default())
        .and_then(|parsed| parsed.try_timestamp())
        .is_some()
}

fn formats_of(value: &Value) -> Result<Vec<StrpFormat>> {
    let texts = match value.data() {
        ValueData::List(items) => {
            if items.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "strptime format list must not be empty",
                ));
            }
            items.iter().map(Value::try_to_string).collect::<Result<Vec<_>>>()?
        }
        _ => vec![value.try_to_string()?],
    };
    texts
        .iter()
        .map(|text| {
            StrpFormat::new(text).map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Failed to parse format specifier {text}: {e}"),
                )
            })
        })
        .collect()
}

/// `StrpTimeFunction::Bind`: a format with nanoseconds makes a TIMESTAMP_NS, one with a UTC offset
/// a TIMESTAMP WITH TIME ZONE.
pub(crate) fn strptime_return_type(format: &Value) -> Result<Option<LogicalType>> {
    if format.is_null() {
        return Ok(None);
    }
    let formats = formats_of(format)?;
    let nanos = formats.iter().any(|f| f.has(Spec::NanosecondPadded));
    let offset = formats.iter().any(|f| f.has(Spec::UtcOffset));
    Ok(match (nanos, offset) {
        (true, true) => Some(LogicalType::new(LogicalTypeId::TimestampTzNs)),
        (true, false) => Some(LogicalType::new(LogicalTypeId::TimestampNs)),
        (false, true) => Some(LogicalType::new(LogicalTypeId::TimestampTz)),
        (false, false) => None,
    })
}

fn strptime_row(row: &[Value], strict: bool, ty: &LogicalType) -> Result<Value> {
    let input = row[0].try_to_string()?;
    let formats = formats_of(&row[1])?;
    let nanos = matches!(ty.id(), LogicalTypeId::TimestampNs | LogicalTypeId::TimestampTzNs);
    let mut error = ParseError::default();
    for format in &formats {
        let Some(parsed) = format.parse(&input, false, &mut error) else {
            continue;
        };
        let value = if strict {
            Some(if nanos {
                parsed.timestamp_ns().map_err(conversion)?
            } else {
                parsed.timestamp().map_err(|(_, message)| conversion(message))?
            })
        } else if nanos {
            parsed.try_timestamp_ns()
        } else {
            parsed.try_timestamp()
        };
        if let Some(value) = value {
            return Value::new(ty.clone(), ValueData::Int64(value));
        }
    }
    if strict {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format_error(&input, &formats[0].text, &error),
        ));
    }
    Ok(Value::null(ty.clone()))
}

fn strptime(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| strptime_row(row, true, &ty))
}

fn try_strptime(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| strptime_row(row, false, &ty))
}

/// The function of its own that a `date_part` of `name` amounts to, if it has one. An unknown name
/// has none, so it fails where `date_part` would.
pub fn date_part_call_of(name: &str, value: BoundExpr) -> Option<BoundExpr> {
    let function = match DatePart::parse(name)? {
        DatePart::Year => "year",
        DatePart::Month => "month",
        DatePart::Day => "day",
        DatePart::Decade => "decade",
        DatePart::Century => "century",
        DatePart::Millennium => "millennium",
        DatePart::Quarter => "quarter",
        DatePart::Week => "week",
        DatePart::YearWeek => "yearweek",
        DatePart::Dow => "dayofweek",
        DatePart::IsoDow => "isodow",
        DatePart::Doy => "dayofyear",
        DatePart::Microseconds => "microsecond",
        DatePart::Milliseconds => "millisecond",
        DatePart::Second => "second",
        DatePart::Minute => "minute",
        DatePart::Hour => "hour",
        _ => return None,
    };
    let set = BuiltinFunctions::get().scalar(function)?;
    let ty = value.return_type.clone();
    let call = bind_scalar_function(set, vec![value], false, &|_| None).ok()?;
    // without an overload for its type the value gets a cast, which date_part does not have and
    // which can change it: TIMESTAMP_NS rounds to microseconds
    match &call.kind {
        BoundExprKind::Function { children, .. } if children[0].return_type == ty => Some(call),
        _ => None,
    }
}

/// Whether [`bind_date_function`] binds `name`.
pub fn is_date_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "date_part" | "datepart" | "date_trunc" | "datetrunc"
    )
}

/// `date_part` and `date_trunc`, whose return types follow from their first argument.
pub fn bind_date_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    let [part, value] = children.as_slice() else {
        return Err(Error::new(
            ErrorKind::Binder,
            format!(
                "No function matches the given name and argument types '{lower}'. You might need to add explicit type casts."
            ),
        ));
    };
    if (lower == "date_part" || lower == "datepart") && part.return_type.id() == LogicalTypeId::List {
        return bind_struct_date_part(part, value);
    }
    let part = part.clone().cast_to(&LogicalType::VARCHAR, false);
    let constant = if part.is_foldable() {
        let specifier = evaluate_scalar(&part)?;
        if specifier.is_null() {
            None
        } else {
            let name = specifier.try_to_string()?;
            let part = parse_part(&name)?;
            if lower == "date_part" || lower == "datepart" {
                check_part_for_type(&name, part, &value.return_type)?;
            }
            Some(part)
        }
    } else {
        None
    };
    let id = value.return_type.id();
    let value = match id {
        LogicalTypeId::Date
        | LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampTz
        | LogicalTypeId::Time
        | LogicalTypeId::TimeTz
        | LogicalTypeId::TimeNs
        | LogicalTypeId::Interval
        | LogicalTypeId::TimestampSec
        | LogicalTypeId::TimestampMs
        | LogicalTypeId::TimestampNs
        | LogicalTypeId::TimestampTzNs => value.clone(),
        _ => value.clone().cast_to(&LogicalType::TIMESTAMP, false),
    };
    let args = vec![part, value];
    if lower == "date_part" || lower == "datepart" {
        let ty = match constant {
            Some(DatePart::Epoch | DatePart::JulianDay) => LogicalType::DOUBLE,
            _ => LogicalType::BIGINT,
        };
        return Ok(bound_function("date_part", args, ty, date_part_call));
    }
    let ty = match args[1].return_type.id() {
        LogicalTypeId::Interval => LogicalType::INTERVAL,
        LogicalTypeId::TimestampTz => args[1].return_type.clone(),
        _ => LogicalType::TIMESTAMP,
    };
    Ok(bound_function("date_trunc", args, ty, date_trunc))
}

/// `TimezoneOperator::Operation(interval_t, dtime_tz_t)`: the time at +00, moved by the interval, at that offset.
fn timezone_of_time_tz(args: &DataChunk, result: &mut Vector) -> Result<()> {
    const MAX_OFFSET: i64 = 16 * 60 * 60 - 1;
    strict_rows(args, result, |row| {
        let (ValueData::Interval(interval), ValueData::Int64(bits)) = (row[0].data(), row[1].data()) else {
            return Ok(Value::null(LogicalType::new(LogicalTypeId::TimeTz)));
        };
        let offset = interval.micros / MICROS_PER_SEC;
        if !(-MAX_OFFSET..=MAX_OFFSET).contains(&offset) {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                format!(
                    "Timezone offset out of range: {}",
                    Value::interval(*interval).try_to_string()?
                ),
            ));
        }
        let value = TimeTz(*bits as u64);
        let utc = (value.time().0 - i64::from(value.offset()) * MICROS_PER_SEC).rem_euclid(MICROS_PER_DAY);
        let time = (utc + interval.micros).rem_euclid(MICROS_PER_DAY);
        Ok(Value::time_tz(TimeTz::new(
            pintail_common::types::Time(time),
            offset as i32,
        )))
    })
}

pub fn date_functions() -> Vec<ScalarFunctionSet> {
    use LogicalType as L;
    let tz = L::new(LogicalTypeId::TimestampTz);
    let time_tz = L::new(LogicalTypeId::TimeTz);
    let v = L::VARCHAR;
    let i = L::BIGINT;
    let d = L::DOUBLE;
    let inputs = [L::DATE, L::TIMESTAMP, tz.clone(), L::TIME, time_tz.clone(), L::INTERVAL];
    let date_inputs = [L::DATE, L::TIMESTAMP, tz.clone()];
    let mut sets = Vec::new();
    let mut add = |name: &str, overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)>| {
        let mut set = ScalarFunctionSet::new(name);
        for (arguments, return_type, callback) in overloads {
            set.add(ScalarFunction::new(name, arguments, return_type, callback));
        }
        sets.push(set);
    };
    let parts: &[(&str, ScalarCallback, LogicalType)] = &[
        ("year", year, i.clone()),
        ("month", month, i.clone()),
        ("day", day, i.clone()),
        ("dayofmonth", day, i.clone()),
        ("decade", decade, i.clone()),
        ("century", century, i.clone()),
        ("millennium", millennium, i.clone()),
        ("quarter", quarter, i.clone()),
        ("dayofweek", dayofweek, i.clone()),
        ("weekday", dayofweek, i.clone()),
        ("isodow", isodow, i.clone()),
        ("dayofyear", dayofyear, i.clone()),
        ("week", week, i.clone()),
        ("weekofyear", week, i.clone()),
        ("yearweek", yearweek, i.clone()),
        ("isoyear", isoyear, i.clone()),
        ("era", era, i.clone()),
        ("hour", hour, i.clone()),
        ("minute", minute, i.clone()),
        ("second", second, i.clone()),
        ("millisecond", millisecond, i.clone()),
        ("microsecond", microsecond, i.clone()),
        ("nanosecond", nanosecond, i.clone()),
        ("timezone", timezone, i.clone()),
        ("timezone_hour", timezone_hour, i.clone()),
        ("timezone_minute", timezone_minute, i.clone()),
        ("epoch", epoch, d.clone()),
        ("julian", julian, d.clone()),
    ];
    // `GetTimePartFunction`: the parts of a time of day take a TIME_NS too
    let time_parts = [
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
        "epoch",
        "timezone",
        "timezone_hour",
        "timezone_minute",
    ];
    for (name, callback, ty) in parts {
        // `GetGenericDatePartFunction`: the parts of a date take no time of day
        let mut overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = inputs
            .iter()
            .filter(|input| {
                time_parts.contains(name) || !matches!(input.id(), LogicalTypeId::Time | LogicalTypeId::TimeTz)
            })
            .map(|input| (vec![input.clone()], ty.clone(), *callback))
            .collect();
        if time_parts.contains(name) {
            overloads.push((vec![L::new(LogicalTypeId::TimestampNs)], ty.clone(), *callback));
            overloads.push((vec![L::new(LogicalTypeId::TimeNs)], ty.clone(), *callback));
        }
        // PG also defines timezone(INTERVAL, TIME_TZ) => TIME_TZ
        if *name == "timezone" {
            overloads.push((vec![L::INTERVAL, time_tz.clone()], time_tz.clone(), timezone_of_time_tz));
        }
        add(name, overloads);
    }
    let mut epoch_overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = date_inputs
        .iter()
        .map(|input| (vec![input.clone()], i.clone(), epoch_ms as ScalarCallback))
        .collect();
    epoch_overloads.push((vec![L::new(LogicalTypeId::TimestampNs)], i.clone(), epoch_ms));
    epoch_overloads.push((vec![i.clone()], L::TIMESTAMP, epoch_ms_to_timestamp));
    // `GetTimePartFunction`: INTERVAL and TIME too
    for ty in [L::INTERVAL, L::TIME, L::new(LogicalTypeId::TimeNs), time_tz.clone()] {
        epoch_overloads.push((vec![ty], i.clone(), epoch_ms_of_interval));
    }
    add("epoch_ms", epoch_overloads);
    for (name, callback, of_interval) in [
        (
            "epoch_us",
            epoch_us as ScalarCallback,
            epoch_us_of_interval as ScalarCallback,
        ),
        ("epoch_ns", epoch_ns, epoch_ns_of_interval),
    ] {
        let mut overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = date_inputs
            .iter()
            .map(|input| (vec![input.clone()], i.clone(), callback))
            .collect();
        overloads.push((vec![L::new(LogicalTypeId::TimestampNs)], i.clone(), callback));
        if name == "epoch_ns" {
            overloads.push((vec![L::new(LogicalTypeId::TimestampTzNs)], i.clone(), callback));
        }
        for ty in [L::INTERVAL, L::TIME, L::new(LogicalTypeId::TimeNs), time_tz.clone()] {
            overloads.push((vec![ty], i.clone(), of_interval));
        }
        add(name, overloads);
    }
    for (name, callback, ty) in [
        ("last_day", last_day as ScalarCallback, L::DATE),
        ("monthname", monthname, v.clone()),
        ("dayname", dayname, v.clone()),
    ] {
        add(
            name,
            date_inputs
                .iter()
                .map(|input| (vec![input.clone()], ty.clone(), callback))
                .collect(),
        );
    }
    for (names, callback) in [
        (["date_diff", "datediff"], date_diff as ScalarCallback),
        (["date_sub", "datesub"], date_sub),
    ] {
        for name in names {
            let mut overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = date_inputs
                .iter()
                .map(|input| (vec![v.clone(), input.clone(), input.clone()], i.clone(), callback))
                .collect();
            overloads.push((vec![v.clone(), L::TIME, L::TIME], i.clone(), callback));
            add(name, overloads);
        }
    }
    add(
        "age",
        vec![
            (vec![L::TIMESTAMP, L::TIMESTAMP], L::INTERVAL, age as ScalarCallback),
            (vec![L::TIMESTAMP], L::INTERVAL, age_from_today),
            (vec![tz.clone(), tz.clone()], L::INTERVAL, age),
        ],
    );
    add(
        "make_date",
        vec![
            (
                vec![i.clone(), i.clone(), i.clone()],
                L::DATE,
                make_date as ScalarCallback,
            ),
            (vec![L::INTEGER], L::DATE, make_date_from_epoch),
            (
                vec![L::struct_type(vec![
                    ("year".to_string(), i.clone()),
                    ("month".to_string(), i.clone()),
                    ("day".to_string(), i.clone()),
                ])],
                L::DATE,
                make_date_from_struct,
            ),
        ],
    );
    add(
        "make_time",
        vec![(
            vec![i.clone(), i.clone(), d.clone()],
            L::TIME,
            make_time as ScalarCallback,
        )],
    );
    add(
        "make_timestamp",
        vec![
            (
                vec![i.clone(), i.clone(), i.clone(), i.clone(), i.clone(), d.clone()],
                L::TIMESTAMP,
                make_timestamp as ScalarCallback,
            ),
            (vec![i.clone()], L::TIMESTAMP, make_timestamp),
        ],
    );
    // `MakeTimestampMsFun`: `EpochMillisOperator::Inverse`
    add(
        "make_timestamp_ms",
        vec![(vec![L::BIGINT], L::TIMESTAMP, epoch_ms_to_timestamp as ScalarCallback)],
    );
    add(
        "make_timestamp_ns",
        vec![(
            vec![L::BIGINT],
            L::new(LogicalTypeId::TimestampNs),
            make_timestamp_ns as ScalarCallback,
        )],
    );
    add(
        "to_timestamp",
        vec![(vec![d.clone()], tz.clone(), to_timestamp as ScalarCallback)],
    );
    for name in [
        "now",
        "get_current_timestamp",
        "transaction_timestamp",
        "current_timestamp",
    ] {
        add(name, vec![(vec![], tz.clone(), now as ScalarCallback)]);
    }
    for name in ["current_date", "today"] {
        add(name, vec![(vec![], L::DATE, current_date as ScalarCallback)]);
    }
    for (name, callback) in [
        ("to_years", to_years as ScalarCallback),
        ("to_quarters", to_quarters),
        ("to_months", to_months),
        ("to_decades", to_decades),
        ("to_centuries", to_centuries),
        ("to_millennia", to_millennia),
        ("to_weeks", to_weeks),
        ("to_days", to_days),
        ("to_hours", to_hours),
        ("to_minutes", to_minutes),
        ("to_microseconds", to_microseconds),
    ] {
        add(name, vec![(vec![i.clone()], L::INTERVAL, callback)]);
    }
    add(
        "to_seconds",
        vec![(vec![d.clone()], L::INTERVAL, to_seconds as ScalarCallback)],
    );
    add(
        "to_milliseconds",
        vec![(vec![d.clone()], L::INTERVAL, to_milliseconds as ScalarCallback)],
    );
    let mut format_overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = Vec::new();
    for input in [L::DATE, L::TIMESTAMP, L::new(LogicalTypeId::TimestampNs), tz.clone()] {
        format_overloads.push((vec![input.clone(), v.clone()], v.clone(), strftime as ScalarCallback));
        format_overloads.push((vec![v.clone(), input], v.clone(), strftime));
    }
    add("strftime", format_overloads);
    let formats = L::list(L::VARCHAR);
    add(
        "strptime",
        vec![
            (vec![v.clone(), v.clone()], L::TIMESTAMP, strptime as ScalarCallback),
            (vec![v.clone(), formats.clone()], L::TIMESTAMP, strptime),
        ],
    );
    add(
        "try_strptime",
        vec![
            (vec![v.clone(), v.clone()], L::TIMESTAMP, try_strptime as ScalarCallback),
            (vec![v.clone(), formats], L::TIMESTAMP, try_strptime),
        ],
    );
    let ts = L::TIMESTAMP;
    let ts_ns = L::new(LogicalTypeId::TimestampNs);
    let ts_s = L::new(LogicalTypeId::TimestampSec);
    let ts_ms = L::new(LogicalTypeId::TimestampMs);
    let time_tz = LogicalType::new(LogicalTypeId::TimeTz);
    add(
        "+",
        vec![
            (vec![L::DATE, L::INTEGER], L::DATE, date_plus_integer as ScalarCallback),
            (vec![L::INTEGER, L::DATE], L::DATE, integer_plus_date),
            (vec![L::DATE, L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, L::DATE], ts.clone(), interval_plus_moment),
            (vec![ts.clone(), L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, ts.clone()], ts.clone(), interval_plus_moment),
            (vec![ts_s.clone(), L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, ts_s.clone()], ts.clone(), interval_plus_moment),
            (vec![ts_ms.clone(), L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, ts_ms.clone()], ts.clone(), interval_plus_moment),
            (vec![ts_ns.clone(), L::INTERVAL], ts_ns.clone(), moment_plus_interval),
            (vec![L::INTERVAL, ts_ns.clone()], ts_ns.clone(), interval_plus_moment),
            (vec![tz.clone(), L::INTERVAL], tz.clone(), moment_plus_interval),
            (vec![L::INTERVAL, tz.clone()], tz.clone(), interval_plus_moment),
            (vec![L::TIME, L::INTERVAL], L::TIME, moment_plus_interval),
            (vec![L::INTERVAL, L::TIME], L::TIME, interval_plus_moment),
            (vec![L::INTERVAL, L::INTERVAL], L::INTERVAL, interval_plus_interval),
            (vec![L::DATE, L::TIME], ts.clone(), date_plus_time),
            (vec![L::TIME, L::DATE], ts.clone(), time_plus_date),
            (
                vec![time_tz.clone(), L::INTERVAL],
                time_tz.clone(),
                moment_plus_interval,
            ),
            (
                vec![L::INTERVAL, time_tz.clone()],
                time_tz.clone(),
                interval_plus_moment,
            ),
            (vec![L::DATE, time_tz.clone()], tz.clone(), date_plus_time_tz),
            (vec![time_tz.clone(), L::DATE], tz.clone(), time_tz_plus_date),
        ],
    );
    add(
        "-",
        vec![
            (vec![L::DATE, L::DATE], i.clone(), date_minus_date as ScalarCallback),
            (vec![L::DATE, L::INTEGER], L::DATE, date_minus_integer),
            (vec![L::DATE, L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![ts.clone(), L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![ts_s.clone(), L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![ts_ms.clone(), L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![ts_ns.clone(), L::INTERVAL], ts_ns.clone(), moment_minus_interval),
            (vec![tz.clone(), L::INTERVAL], tz.clone(), moment_minus_interval),
            (vec![L::TIME, L::INTERVAL], L::TIME, moment_minus_interval),
            (
                vec![time_tz.clone(), L::INTERVAL],
                time_tz.clone(),
                moment_minus_interval,
            ),
            (vec![L::INTERVAL, L::INTERVAL], L::INTERVAL, interval_minus_interval),
            (vec![ts.clone(), ts.clone()], L::INTERVAL, timestamp_minus_timestamp),
            (vec![tz.clone(), tz.clone()], L::INTERVAL, timestamp_minus_timestamp),
        ],
    );
    add(
        "*",
        vec![
            (
                vec![L::INTERVAL, i.clone()],
                L::INTERVAL,
                interval_times_bigint as ScalarCallback,
            ),
            (vec![i.clone(), L::INTERVAL], L::INTERVAL, bigint_times_interval),
            (vec![L::INTERVAL, d.clone()], L::INTERVAL, interval_times_double),
            (vec![d.clone(), L::INTERVAL], L::INTERVAL, double_times_interval),
        ],
    );
    add(
        "/",
        vec![(
            vec![L::INTERVAL, d.clone()],
            L::INTERVAL,
            interval_divided as ScalarCallback,
        )],
    );
    sets
}
