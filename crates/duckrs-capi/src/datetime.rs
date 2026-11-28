// Ported from duckdb/src/main/capi/v1/datetime-c.cpp and hugeint-c.cpp @ rust-port-baseline

//! Dates, times and timestamps taken apart and put together, and the conversions of HUGEINT and DECIMAL values.

use crate::types::*;
use duckrs::{LogicalType, Value};
use duckrs_common::types::{Date, Time, TimeTz, Timestamp};
use duckrs_common::value::ValueData;

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_from_date(date: duckdb_date) -> duckdb_date_struct {
    let (year, month, day) = Date(date.days).to_ymd();
    duckdb_date_struct {
        year,
        month: month as i8,
        day: day as i8,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_to_date(date: duckdb_date_struct) -> duckdb_date {
    let days = Date::from_ymd(date.year, date.month.into(), date.day.into()).map_or(0, |d| d.0);
    duckdb_date { days }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_is_finite_date(date: duckdb_date) -> bool {
    Date(date.days).is_finite()
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_from_time(time: duckdb_time) -> duckdb_time_struct {
    let (hour, min, sec, micros) = Time(time.micros).to_hms();
    duckdb_time_struct {
        hour: hour as i8,
        min: min as i8,
        sec: sec as i8,
        micros,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_from_time_tz(input: duckdb_time_tz) -> duckdb_time_tz_struct {
    let time_tz = TimeTz(input.bits);
    duckdb_time_tz_struct {
        time: duckdb_from_time(duckdb_time {
            micros: time_tz.time().0,
        }),
        offset: time_tz.offset(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_time_tz(micros: i64, offset: i32) -> duckdb_time_tz {
    duckdb_time_tz {
        bits: TimeTz::new(Time(micros), offset).0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_to_time(time: duckdb_time_struct) -> duckdb_time {
    duckdb_time {
        micros: Time::from_hms(time.hour.into(), time.min.into(), time.sec.into(), time.micros).0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_from_timestamp(ts: duckdb_timestamp) -> duckdb_timestamp_struct {
    let timestamp = Timestamp(ts.micros);
    // an infinite timestamp has no time of day: it gets the parts of the infinite date
    let time = if timestamp.is_finite() {
        timestamp.time()
    } else {
        Time(0)
    };
    duckdb_timestamp_struct {
        date: duckdb_from_date(duckdb_date {
            days: timestamp.date().0,
        }),
        time: duckdb_from_time(duckdb_time { micros: time.0 }),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_to_timestamp(ts: duckdb_timestamp_struct) -> duckdb_timestamp {
    let time = Time(duckdb_to_time(ts.time).micros);
    let micros = Date::from_ymd(ts.date.year, ts.date.month.into(), ts.date.day.into())
        .and_then(|date| Timestamp::from_datetime(date, time))
        .map_or(0, |ts| ts.0);
    duckdb_timestamp { micros }
}

fn finite(value: i64) -> bool {
    value != i64::MAX && value != -i64::MAX
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_is_finite_timestamp(ts: duckdb_timestamp) -> bool {
    finite(ts.micros)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_is_finite_timestamp_s(ts: duckdb_timestamp_s) -> bool {
    finite(ts.seconds)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_is_finite_timestamp_ms(ts: duckdb_timestamp_ms) -> bool {
    finite(ts.millis)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_is_finite_timestamp_ns(ts: duckdb_timestamp_ns) -> bool {
    finite(ts.nanos)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_hugeint_to_double(value: duckdb_hugeint) -> f64 {
    value.to_i128() as f64
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_uhugeint_to_double(value: duckdb_uhugeint) -> f64 {
    value.to_u128() as f64
}

/// `Hugeint::TryConvert<double>`: the integer part of a finite `value` in range, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_double_to_hugeint(value: f64) -> duckdb_hugeint {
    const LIMIT: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    if !value.is_finite() || !(-LIMIT..LIMIT).contains(&value) {
        return duckdb_hugeint::default();
    }
    duckdb_hugeint::from_i128(value as i128)
}

/// `Uhugeint::TryConvert<double>`.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_double_to_uhugeint(value: f64) -> duckdb_uhugeint {
    const LIMIT: f64 = 340_282_366_920_938_463_463_374_607_431_768_211_456.0;
    if !value.is_finite() || !(0.0..LIMIT).contains(&value) {
        return duckdb_uhugeint::default();
    }
    duckdb_uhugeint::from_u128(value as u128)
}

/// `TryCastToDecimal` of a double, as `CAST(value AS DECIMAL(width, scale))`; the zero decimal when it doesn't fit.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_double_to_decimal(value: f64, width: u8, scale: u8) -> duckdb_decimal {
    if scale > width || width > 38 || width == 0 {
        return duckdb_decimal::default();
    }
    let cast = crate::ffi::guard(None, || {
        duckrs::cast_value(&Value::double(value), &LogicalType::decimal(width, scale)).ok()
    });
    match cast.as_ref().map(Value::data) {
        Some(ValueData::Null) | None => duckdb_decimal::default(),
        Some(_) => duckdb_decimal {
            width,
            scale,
            value: duckdb_hugeint::from_i128(cast.and_then(|v| v.as_i128()).unwrap_or_default()),
        },
    }
}

/// `TryCastFromDecimal` to a double: the unscaled value divided by `10^scale`.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_decimal_to_double(value: duckdb_decimal) -> f64 {
    value.value.to_i128() as f64 / 10f64.powi(value.scale.into())
}
