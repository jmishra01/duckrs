// Ported from duckdb/extension/core_functions/scalar/date/*.cpp,
// duckdb/src/function/scalar/date/strftime.cpp, duckdb/src/function/scalar/strftime_format.cpp and
// the date operators of duckdb/src/function/scalar/operator/*.cpp @ rust-port-baseline
//
//! Date and time functions. They run row by row; the vectorized loops come with Phase 13. Time
//! zones wait for the ICU port: a TIMESTAMPTZ reads as UTC here.

use super::binder::bind_scalar_function;
use super::bit_functions::strict_rows;
use super::nested::bound_function;
use super::{BuiltinFunctions, ScalarCallback, ScalarFunction, ScalarFunctionSet};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::{
    Date, DatePart, Interval, LogicalType, LogicalTypeId, MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE,
    MICROS_PER_SEC, Time, TimeTz, Timestamp,
};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

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
        DatePart::Decade => year.div_euclid(10),
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
        return Err(unsupported_part(
            if id == LogicalTypeId::Date { "date" } else { "timestamp" },
            part,
        ));
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

fn date_part_call(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        part_value(parse_part(&row[0].try_to_string()?)?, &row[1], &ty)
    })
}

macro_rules! part_function {
    ($name:ident, $part:ident) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
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
        Ok(match moment(&row[0]).and_then(micros_of) {
            Some(micros) => Value::bigint((i128::from(micros) * scale / 1000) as i64),
            None => Value::null(ty.clone()),
        })
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

fn epoch_ms_to_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let millis = row[0].as_i128().expect("a BIGINT");
        let micros = i64::try_from(millis * 1000).map_err(|_| conversion("Epoch milliseconds out of range"))?;
        Ok(Value::timestamp(Timestamp(micros)))
    })
}

fn nanosecond(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
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
        let last = Date::from_ymd(year, month, Date::month_days(year, month)).expect("a day of the month");
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
    let start_of_day = |days: i32| i64::from(days) * MICROS_PER_DAY;
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
        DatePart::Week | DatePart::YearWeek => Ok(start_of_day(days - weekday_from_monday(days))),
        DatePart::IsoYear => {
            let monday = days - weekday_from_monday(days);
            Ok(start_of_day(monday - (iso_week(monday).1 - 1) * 7))
        }
        DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => {
            Ok(start_of_day(days))
        }
        DatePart::Hour => Ok(start_of_day(days) + time / MICROS_PER_HOUR * MICROS_PER_HOUR),
        DatePart::Minute => Ok(start_of_day(days) + time / MICROS_PER_MINUTE * MICROS_PER_MINUTE),
        DatePart::Second | DatePart::Epoch => Ok(start_of_day(days) + time / MICROS_PER_SEC * MICROS_PER_SEC),
        DatePart::Milliseconds => Ok(start_of_day(days) + time / 1000 * 1000),
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
        DatePart::Year => months_only(12),
        DatePart::Quarter => months_only(3),
        DatePart::Month => months_only(1),
        DatePart::Day => Interval {
            months,
            days,
            micros: 0,
        },
        DatePart::Hour => keep(MICROS_PER_HOUR),
        DatePart::Minute => keep(MICROS_PER_MINUTE),
        DatePart::Second => keep(MICROS_PER_SEC),
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
        let difference = end - start;
        let months = || whole_months(start, end);
        Ok(Value::bigint(match part {
            DatePart::Millennium => months() / 12000,
            DatePart::Century => months() / 1200,
            DatePart::Decade => months() / 120,
            DatePart::Year | DatePart::IsoYear => months() / 12,
            DatePart::Quarter => months() / 3,
            DatePart::Month => months(),
            DatePart::Week | DatePart::YearWeek => difference / (7 * MICROS_PER_DAY),
            DatePart::Day | DatePart::Dow | DatePart::IsoDow | DatePart::Doy | DatePart::JulianDay => {
                difference / MICROS_PER_DAY
            }
            DatePart::Hour => difference / MICROS_PER_HOUR,
            DatePart::Minute => difference / MICROS_PER_MINUTE,
            DatePart::Second | DatePart::Epoch => difference / MICROS_PER_SEC,
            DatePart::Milliseconds => difference / 1000,
            DatePart::Microseconds => difference,
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
    strict_rows(args, result, |row| {
        let (Some(later), Some(earlier)) = (moment(&row[0]).and_then(micros_of), moment(&row[1]).and_then(micros_of))
        else {
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
    })
}

fn make_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let part = |at: usize| row[at].as_i128().expect("a BIGINT");
        let (year, month, day) = (part(0), part(1), part(2));
        let date = i32::try_from(year)
            .ok()
            .zip(i32::try_from(month).ok())
            .zip(i32::try_from(day).ok())
            .and_then(|((y, m), d)| Date::from_ymd(y, m, d));
        date.map(Value::date)
            .ok_or_else(|| conversion(format!("Date out of range: {year}-{month}-{day}")))
    })
}

fn time_micros(hour: i128, minute: i128, seconds: f64) -> Result<i64> {
    if !seconds.is_finite() {
        return Err(conversion("Time out of range: seconds must be a finite number"));
    }
    let whole = seconds.trunc() as i64;
    let micros = ((seconds - seconds.trunc()) * 1e6).round() as i64;
    let valid = (0..24).contains(&hour) && (0..60).contains(&minute) && (0..60).contains(&whole);
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
        let date = i32::try_from(year)
            .ok()
            .zip(i32::try_from(month).ok())
            .zip(i32::try_from(day).ok())
            .and_then(|((y, m), d)| Date::from_ymd(y, m, d))
            .ok_or_else(|| conversion(format!("Date out of range: {year}-{month}-{day}")))?;
        let time = time_micros(part(3), part(4), seconds_of(&row[5]))?;
        let micros = i64::from(date.0)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .ok_or_else(|| conversion("Timestamp out of range"))?;
        Ok(Value::timestamp(Timestamp(micros)))
    })
}

fn to_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let seconds = seconds_of(&row[0]);
        if !seconds.is_finite() || seconds.abs() > 9.2e12 {
            return Err(out_of_range(format!(
                "Could not convert epoch seconds to TIMESTAMP WITH TIME ZONE: {seconds}"
            )));
        }
        Ok(Value::timestamp_tz(Timestamp((seconds * 1e6).round() as i64)))
    })
}

fn now_micros() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    elapsed.as_micros() as i64
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
    ($name:ident, $input:ident, $value:expr) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            strict_rows(args, result, |row| {
                let $input = row[0].as_i128().expect("an integer");
                let overflow = || out_of_range("Interval value out of range");
                let value: Interval = $value(overflow)?;
                Ok(Value::interval(value))
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

to_interval!(to_years, n, |e: fn() -> Error| months_interval(n * 12).ok_or_else(e));
to_interval!(to_quarters, n, |e: fn() -> Error| months_interval(n * 3).ok_or_else(e));
to_interval!(to_months, n, |e: fn() -> Error| months_interval(n).ok_or_else(e));
to_interval!(to_decades, n, |e: fn() -> Error| months_interval(n * 120).ok_or_else(e));
to_interval!(to_centuries, n, |e: fn() -> Error| months_interval(n * 1200)
    .ok_or_else(e));
to_interval!(to_millennia, n, |e: fn() -> Error| months_interval(n * 12000)
    .ok_or_else(e));
to_interval!(to_weeks, n, |e: fn() -> Error| days_interval(n * 7).ok_or_else(e));
to_interval!(to_days, n, |e: fn() -> Error| days_interval(n).ok_or_else(e));
to_interval!(to_hours, n, |e: fn() -> Error| micros_interval(
    n * i128::from(MICROS_PER_HOUR)
)
.ok_or_else(e));
to_interval!(to_minutes, n, |e: fn() -> Error| micros_interval(
    n * i128::from(MICROS_PER_MINUTE)
)
.ok_or_else(e));
to_interval!(to_microseconds, n, |e: fn() -> Error| micros_interval(n).ok_or_else(e));

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
    let mut micros = micros;
    if interval.months != 0 {
        let days = micros.div_euclid(MICROS_PER_DAY) as i32;
        let time = micros.rem_euclid(MICROS_PER_DAY);
        let (year, month, day) = Date(days).to_ymd();
        let total = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(interval.months);
        let year = i32::try_from(total.div_euclid(12)).map_err(|_| overflow())?;
        let month = total.rem_euclid(12) as i32 + 1;
        let day = day.min(Date::month_days(year, month));
        let date = Date::from_ymd(year, month, day).ok_or_else(overflow)?;
        micros = i64::from(date.0)
            .checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .ok_or_else(overflow)?;
    }
    micros
        .checked_add(
            i64::from(interval.days)
                .checked_mul(MICROS_PER_DAY)
                .ok_or_else(overflow)?,
        )
        .and_then(|m| m.checked_add(interval.micros))
        .ok_or_else(overflow)
}

fn negated(interval: Interval) -> Interval {
    Interval {
        months: -interval.months,
        days: -interval.days,
        micros: -interval.micros,
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
    if ty.id() == LogicalTypeId::Time {
        let time = value.as_i128().expect("a TIME") as i64;
        let moved = (time + interval.micros).rem_euclid(MICROS_PER_DAY);
        return Ok(Value::time(Time(moved)));
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
    strict_rows(args, result, |row| shifted(&row[0], negated(interval_of(&row[1])), &ty))
}

fn days_shifted(value: &Value, days: i128) -> Result<Value> {
    let raw = value.as_i128().expect("a DATE") as i32;
    if !Date(raw).is_finite() {
        return Ok(value.clone());
    }
    let moved = i128::from(raw) + days;
    i32::try_from(moved)
        .ok()
        .filter(|d| Date(*d).is_finite())
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

fn date_plus_time(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let days = i64::from(row[0].as_i128().expect("a DATE") as i32);
        let time = row[1].as_i128().expect("a TIME") as i64;
        days.checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .map(|m| Value::timestamp(Timestamp(m)))
            .ok_or_else(|| out_of_range("Date out of range"))
    })
}

fn time_plus_date(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let time = row[0].as_i128().expect("a TIME") as i64;
        let days = i64::from(row[1].as_i128().expect("a DATE") as i32);
        days.checked_mul(MICROS_PER_DAY)
            .and_then(|m| m.checked_add(time))
            .map(|m| Value::timestamp(Timestamp(m)))
            .ok_or_else(|| out_of_range("Date out of range"))
    })
}

fn timestamp_minus_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (Some(left), Some(right)) = (moment(&row[0]).and_then(micros_of), moment(&row[1]).and_then(micros_of))
        else {
            return Err(out_of_range("Timestamp difference is out of bounds"));
        };
        let difference = left
            .checked_sub(right)
            .ok_or_else(|| out_of_range("Timestamp difference is out of bounds"))?;
        Ok(Value::interval(Interval {
            months: 0,
            days: (difference / MICROS_PER_DAY) as i32,
            micros: difference % MICROS_PER_DAY,
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

fn interval_minus_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::interval(interval_arithmetic(
            interval_of(&row[0]),
            negated(interval_of(&row[1])),
        )?))
    })
}

fn scale_interval(interval: Interval, factor: i64) -> Result<Interval> {
    let overflow = || out_of_range("Interval value is out of range");
    Ok(Interval {
        months: i32::try_from(i64::from(interval.months) * factor).map_err(|_| overflow())?,
        days: i32::try_from(i64::from(interval.days) * factor).map_err(|_| overflow())?,
        micros: interval.micros.checked_mul(factor).ok_or_else(overflow)?,
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
            'n' => out += &format!("{:09}", fraction * 1000),
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
            'y' => out += &pad(i64::from(year.rem_euclid(100)), 2),
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
        Ok(Value::varchar(strftime_text(
            &format.try_to_string()?,
            days,
            time,
            zoned,
        )?))
    })
}

/// `StrpTimeFormat::Parse`.
pub(crate) fn strptime_micros(input: &str, format: &str) -> std::result::Result<i64, String> {
    strptime_parse(input, format, false)
}

/// `StrpTimeFormat::Parse` with `strict`: a year of one digit is no year.
pub(crate) fn strptime_strict(input: &str, format: &str) -> bool {
    strptime_parse(input, format, true).is_ok()
}

fn strptime_parse(input: &str, format: &str, strict: bool) -> std::result::Result<i64, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut at = 0;
    let (mut year, mut month, mut day) = (1900i64, 1i64, 1i64);
    let (mut hour, mut minute, mut second, mut micros) = (0i64, 0i64, 0i64, 0i64);
    let mut pm: Option<bool> = None;
    let mut offset = 0i64;
    let mut yday: Option<i64> = None;
    let number = |at: &mut usize, width: usize| -> Option<i64> {
        let start = *at;
        while *at < chars.len() && *at - start < width && chars[*at].is_ascii_digit() {
            *at += 1;
        }
        chars[start..*at].iter().collect::<String>().parse().ok()
    };
    let name = |at: &mut usize, names: &[&str], short: bool| -> Option<usize> {
        for (i, candidate) in names.iter().enumerate() {
            let text = if short { &candidate[..3] } else { candidate };
            let size = text.len();
            if *at + size <= chars.len() {
                let found: String = chars[*at..*at + size].iter().collect();
                if found.eq_ignore_ascii_case(text) {
                    *at += size;
                    return Some(i);
                }
            }
        }
        None
    };
    let mut pattern = format.chars().peekable();
    while let Some(c) = pattern.next() {
        if c.is_whitespace() {
            while at < chars.len() && chars[at].is_whitespace() {
                at += 1;
            }
            continue;
        }
        if c != '%' {
            if chars.get(at) != Some(&c) {
                return Err(format!("Literal does not match, expected {c}"));
            }
            at += 1;
            continue;
        }
        if pattern.peek() == Some(&'-') {
            pattern.next();
        }
        let specifier = pattern.next().ok_or("Trailing format character %")?;
        let bad = |what: &str| format!("Expected {what}");
        match specifier {
            'Y' => {
                let negative = chars.get(at) == Some(&'-');
                if negative {
                    at += 1;
                }
                // a number right after it has to leave its digits alone
                let mut following = pattern.clone();
                let adjacent = following.next() == Some('%')
                    && following
                        .next()
                        .is_some_and(|next| matches!(next, 'd' | 'e' | 'm' | 'H' | 'I' | 'M' | 'S' | 'y' | 'j'));
                let start = at;
                year = number(&mut at, if adjacent { 4 } else { 7 }).ok_or_else(|| bad("a number"))?;
                if strict && at - start < 2 {
                    return Err(bad("a year of two digits"));
                }
                if negative {
                    year = -year;
                }
            }
            'y' => {
                let start = at;
                let short = number(&mut at, 2).ok_or_else(|| bad("a number"))?;
                if strict && at - start < 2 {
                    return Err(bad("a year of two digits"));
                }
                year = if short < 69 { 2000 + short } else { 1900 + short };
            }
            'm' => month = number(&mut at, 2).ok_or_else(|| bad("a number"))?,
            'd' | 'e' => {
                while chars.get(at) == Some(&' ') {
                    at += 1;
                }
                day = number(&mut at, 2).ok_or_else(|| bad("a number"))?;
            }
            'H' | 'I' => hour = number(&mut at, 2).ok_or_else(|| bad("a number"))?,
            'M' => minute = number(&mut at, 2).ok_or_else(|| bad("a number"))?,
            'S' => second = number(&mut at, 2).ok_or_else(|| bad("a number"))?,
            'j' => yday = Some(number(&mut at, 3).ok_or_else(|| bad("a number"))?),
            'f' | 'g' | 'n' => {
                let width = match specifier {
                    'g' => 3,
                    'n' => 9,
                    _ => 6,
                };
                let start = at;
                let digits = number(&mut at, width).ok_or_else(|| bad("a number"))?;
                let read = (at - start) as u32;
                let scaled = digits * 10i64.pow(9 - read.min(9));
                micros = scaled / 1000;
            }
            'p' => {
                let text: String = chars.iter().skip(at).take(2).collect::<String>().to_uppercase();
                match text.as_str() {
                    "AM" => pm = Some(false),
                    "PM" => pm = Some(true),
                    _ => return Err(bad("AM/PM")),
                }
                at += 2;
            }
            'b' | 'h' => month = name(&mut at, &MONTH_NAMES, true).ok_or_else(|| bad("a month"))? as i64 + 1,
            'B' => month = name(&mut at, &MONTH_NAMES, false).ok_or_else(|| bad("a month"))? as i64 + 1,
            'a' => {
                name(&mut at, &DAY_NAMES, true).ok_or_else(|| bad("a day"))?;
            }
            'A' => {
                name(&mut at, &DAY_NAMES, false).ok_or_else(|| bad("a day"))?;
            }
            'z' => {
                let sign = match chars.get(at) {
                    Some('+') => 1,
                    Some('-') => -1,
                    Some('Z') => {
                        at += 1;
                        continue;
                    }
                    _ => return Err(bad("+HH[MM] or -HH[MM]")),
                };
                at += 1;
                let hours = number(&mut at, 2).ok_or_else(|| bad("a number"))?;
                if chars.get(at) == Some(&':') {
                    at += 1;
                }
                let minutes = number(&mut at, 2).unwrap_or(0);
                offset = sign * (hours * 60 + minutes);
            }
            '%' => {
                if chars.get(at) != Some(&'%') {
                    return Err(bad("%"));
                }
                at += 1;
            }
            other => return Err(format!("Unsupported format specifier %{other}")),
        }
    }
    if at < chars.len() {
        return Err("Full specifier did not match: trailing characters".into());
    }
    if let Some(is_pm) = pm {
        if !(1..=12).contains(&hour) {
            return Err("Hour out of range, expected a value between 1 and 12".into());
        }
        hour = hour % 12 + if is_pm { 12 } else { 0 };
    }
    let year = i32::try_from(year).map_err(|_| "Year out of range")?;
    if let Some(yday) = yday {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        if !(1..=if leap { 366 } else { 365 }).contains(&yday) {
            return Err("Day of the year out of range".into());
        }
    }
    let date = match yday {
        Some(yday) => Date::from_ymd(year, 1, 1).map(|d| Date(d.0 + yday as i32 - 1)),
        None => Date::from_ymd(year, month as i32, day as i32),
    }
    .ok_or("Date out of range")?;
    if !(0..=24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
        return Err("Time out of range".into());
    }
    let time = hour * MICROS_PER_HOUR + minute * MICROS_PER_MINUTE + second * MICROS_PER_SEC + micros;
    i64::from(date.0)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|m| m.checked_add(time - offset * MICROS_PER_MINUTE))
        .ok_or_else(|| "Timestamp out of range".to_string())
}

fn formats_of(value: &Value) -> Result<Vec<String>> {
    match value.data() {
        ValueData::List(items) => items.iter().map(Value::try_to_string).collect(),
        _ => Ok(vec![value.try_to_string()?]),
    }
}

fn strptime_row(row: &[Value], strict: bool) -> Result<Value> {
    let input = row[0].try_to_string()?;
    let formats = formats_of(&row[1])?;
    let mut last = String::new();
    for format in &formats {
        match strptime_micros(&input, format) {
            Ok(micros) => return Ok(Value::timestamp(Timestamp(micros))),
            Err(reason) => last = reason,
        }
    }
    if strict {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Could not parse string \"{input}\" according to format specifier \"{}\"\n{last}",
                formats.first().map_or("", String::as_str)
            ),
        ));
    }
    Ok(Value::null(LogicalType::TIMESTAMP))
}

fn strptime(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| strptime_row(row, true))
}

fn try_strptime(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| strptime_row(row, false))
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
    let part = part.clone().cast_to(&LogicalType::VARCHAR, false);
    let constant = if part.is_foldable() {
        let value = evaluate_scalar(&part)?;
        if value.is_null() {
            None
        } else {
            Some(parse_part(&value.try_to_string()?)?)
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
    for (name, callback, ty) in parts {
        add(
            name,
            inputs
                .iter()
                .map(|input| (vec![input.clone()], ty.clone(), *callback))
                .collect(),
        );
    }
    let mut epoch_overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)> = date_inputs
        .iter()
        .map(|input| (vec![input.clone()], i.clone(), epoch_ms as ScalarCallback))
        .collect();
    epoch_overloads.push((vec![i.clone()], L::TIMESTAMP, epoch_ms_to_timestamp));
    add("epoch_ms", epoch_overloads);
    for (name, callback) in [("epoch_us", epoch_us as ScalarCallback), ("epoch_ns", epoch_ns)] {
        add(
            name,
            date_inputs
                .iter()
                .map(|input| (vec![input.clone()], i.clone(), callback))
                .collect(),
        );
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
            (vec![tz.clone(), tz.clone()], L::INTERVAL, age),
        ],
    );
    add(
        "make_date",
        vec![(
            vec![i.clone(), i.clone(), i.clone()],
            L::DATE,
            make_date as ScalarCallback,
        )],
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
    for input in [L::DATE, L::TIMESTAMP, tz.clone()] {
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
    add(
        "+",
        vec![
            (vec![L::DATE, L::INTEGER], L::DATE, date_plus_integer as ScalarCallback),
            (vec![L::INTEGER, L::DATE], L::DATE, integer_plus_date),
            (vec![L::DATE, L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, L::DATE], ts.clone(), interval_plus_moment),
            (vec![ts.clone(), L::INTERVAL], ts.clone(), moment_plus_interval),
            (vec![L::INTERVAL, ts.clone()], ts.clone(), interval_plus_moment),
            (vec![tz.clone(), L::INTERVAL], tz.clone(), moment_plus_interval),
            (vec![L::INTERVAL, tz.clone()], tz.clone(), interval_plus_moment),
            (vec![L::TIME, L::INTERVAL], L::TIME, moment_plus_interval),
            (vec![L::INTERVAL, L::TIME], L::TIME, interval_plus_moment),
            (vec![L::INTERVAL, L::INTERVAL], L::INTERVAL, interval_plus_interval),
            (vec![L::DATE, L::TIME], ts.clone(), date_plus_time),
            (vec![L::TIME, L::DATE], ts.clone(), time_plus_date),
        ],
    );
    add(
        "-",
        vec![
            (vec![L::DATE, L::DATE], i.clone(), date_minus_date as ScalarCallback),
            (vec![L::DATE, L::INTEGER], L::DATE, date_minus_integer),
            (vec![L::DATE, L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![ts.clone(), L::INTERVAL], ts.clone(), moment_minus_interval),
            (vec![tz.clone(), L::INTERVAL], tz.clone(), moment_minus_interval),
            (vec![L::TIME, L::INTERVAL], L::TIME, moment_minus_interval),
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
