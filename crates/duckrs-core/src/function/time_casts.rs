// Ported from duckdb/src/function/cast/{time_casts,uuid_casts,blob_cast,string_cast}.cpp,
// duckdb/src/common/operator/cast_operators.cpp and duckdb/src/common/types/blob.cpp
// @ rust-port-baseline
//
//! Casts between the temporal types, and from VARCHAR to the temporal types, INTERVAL, UUID and
//! BLOB. As in DuckDB, a pair of types without a case here is a cast that only NULL survives.

use super::cast::{CastMode, conversion_error};
use duckrs_common::executor;
use duckrs_common::types::{
    Date, DateCastError, Interval, IntervalParseError, LogicalType, LogicalTypeId, MICROS_PER_SEC, NANOS_PER_DAY,
    NANOS_PER_MICRO, NativeType, Time, TimeTz, Timestamp, TimestampCastError, date_error, time_error, timestamp_error,
    uuid_from_str,
};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::Vector;
use duckrs_common::{Error, Result};

const SECONDS: i64 = 1;
const MILLIS: i64 = 1_000;
const MICROS: i64 = MICROS_PER_SEC;
const NANOS: i64 = 1_000_000_000;

/// The ticks per second of a timestamp type (`PRECISION`).
fn precision(id: LogicalTypeId) -> Option<i64> {
    use LogicalTypeId::*;
    match id {
        TimestampSec => Some(SECONDS),
        TimestampMs => Some(MILLIS),
        Timestamp | TimestampTz => Some(MICROS),
        TimestampNs | TimestampTzNs => Some(NANOS),
        _ => None,
    }
}

/// `TryCastTimestampBase` and `TryCastTimeBase`: rounds away from the epoch; infinities stay.
fn rescale(value: i64, from: i64, to: i64) -> Option<i64> {
    if from == to || value == i64::MAX || value == -i64::MAX {
        return Some(value);
    }
    if from < to {
        return value.checked_mul(to / from);
    }
    let scaled = value / (from / to / 2);
    let result = (if scaled < 0 { scaled - 1 } else { scaled + 1 }) / 2;
    // rounding can go past the largest (or smallest) timestamp
    if to < MICROS && !Timestamp(result.checked_mul(MICROS / to)?).is_finite() {
        return None;
    }
    Some(result)
}

/// `TimestampCastErrorMessage`.
fn timestamp_cast_message(value: i64, target: LogicalTypeId) -> String {
    let finite = value != i64::MAX && value != -i64::MAX;
    match target {
        LogicalTypeId::Time | LogicalTypeId::TimeTz if !finite => "Can't get TIME of infinite TIMESTAMP".to_string(),
        LogicalTypeId::TimeNs if !finite => "Can't get TIME_NS of infinite TIMESTAMP".to_string(),
        LogicalTypeId::Date => "Could not convert Timestamp to INT32.".to_string(),
        _ => "Could not convert Timestamp to higher precision.".to_string(),
    }
}

/// `TryCastLoop`'s error (`CastExceptionText`) for a temporal source.
fn cast_exception_text<S: NativeType>(input: Value, target: &LogicalType) -> String {
    format!(
        "Type {} with value {} can't be cast to the destination type {}",
        S::PHYSICAL,
        input.try_to_string().unwrap_or_default(),
        target.physical_type()
    )
}

fn cast_loop<S: NativeType, T: NativeType>(
    source: &Vector,
    result: &mut Vector,
    count: usize,
    mode: CastMode,
    cast: impl Fn(S) -> Option<T>,
    message: impl Fn(S) -> String,
) -> Result<()> {
    executor::unary_fallible::<S, T>(source, result, count, |v| match cast(v) {
        Some(r) => Ok(Some(r)),
        None if mode == CastMode::Try => Ok(None),
        None => Err(conversion_error(message(v))),
    })
}

/// Whether a failed timestamp cast has its own message (`TryCastErrorLoop`).
fn has_own_message(from: LogicalTypeId, to: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    match (from, to) {
        (_, TimeNs) | (_, TimeTz) | (_, Time) => true,
        (TimestampMs | TimestampSec, TimestampSec) => false,
        (TimestampMs | TimestampSec, _) => true,
        (Timestamp | TimestampTz, TimestampNs | TimestampTzNs) => true,
        _ => false,
    }
}

/// Whether DuckDB has a cast from the timestamp type `from` to the temporal type `to`.
fn timestamp_cast_exists(from: LogicalTypeId, to: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    let targets: &[LogicalTypeId] = match from {
        Timestamp => &[
            Date,
            Time,
            TimeTz,
            TimestampTz,
            TimestampNs,
            TimestampTzNs,
            TimestampMs,
            TimestampSec,
        ],
        TimestampTz => &[TimeTz, Timestamp, TimestampNs, TimestampTzNs, TimestampMs, TimestampSec],
        TimestampTzNs => &[TimeTz, TimestampNs, TimestampTz, Timestamp, TimestampMs, TimestampSec],
        TimestampNs => &[Date, Time, TimeNs, Timestamp, TimestampTz, TimestampMs],
        TimestampMs => &[
            Date,
            Time,
            Timestamp,
            TimestampTz,
            TimestampNs,
            TimestampTzNs,
            TimestampSec,
        ],
        TimestampSec => &[
            Date,
            Time,
            TimestampMs,
            Timestamp,
            TimestampTz,
            TimestampNs,
            TimestampTzNs,
        ],
        _ => &[],
    };
    targets.contains(&to)
}

fn timestamp_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    use LogicalTypeId as L;
    let (from_type, target) = (source.logical_type().clone(), result.logical_type().clone());
    let (from, to) = (from_type.id(), target.id());
    let ticks = precision(from).expect("a timestamp source");
    let own_message = has_own_message(from, to);
    let message = |v: i64| {
        if own_message {
            timestamp_cast_message(v, to)
        } else {
            cast_exception_text::<i64>(raw_value(&from_type, ValueData::Int64(v)), &target)
        }
    };
    let finite = |v: i64| v != i64::MAX && v != -i64::MAX;
    match to {
        L::Date => cast_loop::<i64, i32>(
            source,
            result,
            count,
            mode,
            |v| Some(Timestamp(rescale(v, ticks, MICROS)?).date().0),
            message,
        ),
        L::Time => cast_loop::<i64, i64>(
            source,
            result,
            count,
            mode,
            |v| {
                finite(v)
                    .then(|| rescale(v, ticks, MICROS))
                    .flatten()
                    .map(|us| Timestamp(us).time().0)
            },
            message,
        ),
        L::TimeTz => cast_loop::<i64, i64>(
            source,
            result,
            count,
            mode,
            |v| {
                let micros = finite(v).then(|| rescale(v, ticks, MICROS)).flatten()?;
                Some(TimeTz::new(Timestamp(micros).time(), 0).0 as i64)
            },
            message,
        ),
        L::TimeNs => cast_loop::<i64, i64>(
            source,
            result,
            count,
            mode,
            |v| finite(v).then(|| v.rem_euclid(NANOS_PER_DAY)),
            message,
        ),
        _ => {
            let target_ticks = precision(to).expect("a timestamp target");
            cast_loop::<i64, i64>(
                source,
                result,
                count,
                mode,
                |v| rescale(v, ticks, target_ticks),
                message,
            )
        }
    }
}

/// A value of `ty` from its physical data.
fn raw_value(ty: &LogicalType, data: ValueData) -> Value {
    Value::new(ty.clone(), data).expect("the physical data of the type")
}

/// `TryCast::Operation(date_t, timestamp_t)`.
fn date_to_micros(date: Date) -> Option<i64> {
    if !date.is_finite() {
        return Some(if date == Date::INFINITY {
            Timestamp::INFINITY.0
        } else {
            Timestamp::NINFINITY.0
        });
    }
    Timestamp::from_datetime(date, Time(0)).map(|t| t.0)
}

fn date_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let target = result.logical_type().clone();
    let ticks = precision(target.id()).expect("a timestamp target");
    cast_loop::<i32, i64>(
        source,
        result,
        count,
        mode,
        |v| rescale(date_to_micros(Date(v))?, MICROS, ticks),
        |v| cast_exception_text::<i32>(Value::date(Date(v)), &target),
    )
}

/// The casts from TIME, TIME_NS and TIME WITH TIME ZONE.
fn time_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    use LogicalTypeId as L;
    let (from_type, target) = (source.logical_type().clone(), result.logical_type().clone());
    let message = |v: i64| cast_exception_text::<i64>(raw_value(&from_type, ValueData::Int64(v)), &target);
    match (from_type.id(), target.id()) {
        (L::Time, L::TimeNs) => cast_loop::<i64, i64>(source, result, count, mode, |v| v.checked_mul(1000), message),
        (L::TimeNs, L::Time) => {
            cast_loop::<i64, i64>(source, result, count, mode, |v| rescale(v, NANOS, MICROS), message)
        }
        (L::Time, L::TimeTz) => cast_loop::<i64, i64>(
            source,
            result,
            count,
            mode,
            |v| Some(TimeTz::new(Time(v), 0).0 as i64),
            message,
        ),
        (L::TimeTz, L::Time) => cast_loop::<i64, i64>(
            source,
            result,
            count,
            mode,
            |v| Some(TimeTz(v as u64).time().0),
            message,
        ),
        (from, to) => unreachable!("no cast from {from:?} to {to:?}"),
    }
}

/// `TryConvertDateCast`: a date, or a timestamp whose date is kept.
fn string_to_date(input: &[u8]) -> std::result::Result<Date, DateCastError> {
    let parsed = Date::try_convert(input, false)?;
    let is_space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    if input[parsed.pos.min(input.len())..].iter().all(is_space) {
        return Ok(parsed.date);
    }
    match Timestamp::try_convert(input, None) {
        Ok(_) => Ok(parsed.date),
        Err(TimestampCastError::Range) => {
            // the date may be what is out of range for a timestamp: check the rest of it
            let mut placeholder = b"2000-01-01".to_vec();
            placeholder.extend_from_slice(&input[parsed.pos.min(input.len())..]);
            match Timestamp::try_convert(&placeholder, None) {
                Ok(_) => Ok(parsed.date),
                Err(TimestampCastError::Range) => Err(DateCastError::Range),
                Err(_) => Err(DateCastError::IncorrectFormat),
            }
        }
        Err(_) => Err(DateCastError::IncorrectFormat),
    }
}

/// `Timestamp::TryFromTimestampNanos`.
fn timestamp_to_nanos(micros: Timestamp, nanos: i32) -> Option<i64> {
    if !micros.is_finite() {
        return Some(micros.0);
    }
    let whole = micros.0;
    let nanos = i64::from(nanos);
    // `Timestamp::TryCombineUnits`: a negative value may start from the next whole microsecond
    whole
        .checked_mul(NANOS_PER_MICRO)
        .and_then(|w| w.checked_add(nanos))
        .or_else(|| {
            if whole >= 0 {
                return None;
            }
            (whole + 1)
                .checked_mul(NANOS_PER_MICRO)?
                .checked_add(nanos - NANOS_PER_MICRO)
        })
        .filter(|v| *v != i64::MAX && *v != -i64::MAX)
}

fn string_loop<T: NativeType>(
    source: &Vector,
    result: &mut Vector,
    count: usize,
    mode: CastMode,
    parse: impl Fn(&[u8], &str) -> Result<T>,
) -> Result<()> {
    let mut failure = None;
    let mut nulls = Vec::new();
    {
        let u = source.to_unified(count);
        result.prepare_flat(count);
        let out = result.data_mut::<T>();
        for (i, slot) in out.iter_mut().enumerate().take(count) {
            if !u.validity.is_valid(u.sel.get(i)) {
                nulls.push(i);
                continue;
            }
            let bytes = u.str_at(i);
            match parse(bytes, &String::from_utf8_lossy(bytes)) {
                Ok(v) => *slot = v,
                Err(_) if mode == CastMode::Try => nulls.push(i),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
    Ok(())
}

/// VARCHAR to a type stored as bytes.
fn string_to_bytes(
    source: &Vector,
    result: &mut Vector,
    count: usize,
    mode: CastMode,
    convert: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    let u = source.to_unified(count);
    result.prepare_flat(count.max(1));
    for i in 0..count {
        if !u.validity.is_valid(u.sel.get(i)) {
            result.set_null(i, true);
            continue;
        }
        match convert(u.str_at(i)) {
            Ok(bytes) => result.set_str(i, &bytes),
            Err(_) if mode == CastMode::Try => result.set_null(i, true),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn hex_digit(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `Blob::TryGetBlobSize` + `Blob::ToBlob`: `\xAA` escapes and ASCII.
pub(super) fn string_to_blob(input: &[u8]) -> Result<Vec<u8>> {
    let text = || String::from_utf8_lossy(input);
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'\\' => {
                if i + 3 >= input.len() {
                    return Err(conversion_error(format!(
                        "Invalid hex escape code encountered in string -> blob conversion of string \"{}\": \
                         unterminated escape code at end of blob",
                        text()
                    )));
                }
                match (input[i + 1], hex_digit(input[i + 2]), hex_digit(input[i + 3])) {
                    (b'x', Some(a), Some(b)) => out.push((a << 4) + b),
                    _ => {
                        return Err(conversion_error(format!(
                            "Invalid hex escape code encountered in string -> blob conversion of string \"{}\": {}",
                            text(),
                            String::from_utf8_lossy(&input[i..i + 4])
                        )));
                    }
                }
                i += 4;
            }
            c if c <= 127 => {
                out.push(c);
                i += 1;
            }
            _ => {
                return Err(conversion_error(format!(
                    "Invalid byte encountered in STRING -> BLOB conversion of string \"{}\". All non-ascii \
                     characters must be escaped with hex codes (e.g. \\xAA)",
                    text()
                )));
            }
        }
    }
    Ok(out)
}

/// `TryCast` from VARCHAR to a timestamp with a `CastExceptionText` error.
fn string_to_scaled_timestamp(input: &[u8], text: &str, ticks: i64) -> Result<i64> {
    let parsed = if ticks == NANOS {
        let mut nanos = 0;
        Timestamp::try_convert(input, Some(&mut nanos))
            .ok()
            .and_then(|t| timestamp_to_nanos(t, nanos))
    } else {
        Timestamp::try_convert(input, None)
            .ok()
            .and_then(|t| rescale(t.0, MICROS, ticks))
    };
    parsed.ok_or_else(|| conversion_error(format!("Could not convert string '{text}' to INT64")))
}

/// Whether [`string_cast`] handles VARCHAR to `target`.
fn is_string_target(target: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    precision(target).is_some() || matches!(target, Date | Time | TimeNs | TimeTz | Interval | Uuid | Blob)
}

fn string_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    use LogicalTypeId as L;
    let target = result.logical_type().clone();
    match target.id() {
        L::Date => string_loop::<i32>(source, result, count, mode, |input, text| {
            string_to_date(input).map(|d| d.0).map_err(|e| date_error(text, e))
        }),
        L::Time => string_loop::<i64>(source, result, count, mode, |input, text| {
            Time::try_convert(input, false, None)
                .map(|t| t.0)
                .ok_or_else(|| time_error(text))
        }),
        L::TimeNs => string_loop::<i64>(source, result, count, mode, |input, text| {
            let mut nanos = 0;
            Time::try_convert(input, false, Some(&mut nanos))
                .and_then(|t| t.0.checked_mul(1000)?.checked_add(i64::from(nanos)))
                // the nanoseconds can push e.g. 24:00:00 past the end of the day
                .filter(|v| *v <= NANOS_PER_DAY)
                .ok_or_else(|| time_error(text))
        }),
        L::TimeTz => string_loop::<i64>(source, result, count, mode, |input, text| {
            TimeTz::try_convert(input, false)
                .map(|t| t.0 as i64)
                .ok_or_else(|| time_error(text))
        }),
        L::Timestamp => string_loop::<i64>(source, result, count, mode, |input, text| {
            Timestamp::try_convert(input, None)
                .map(|t| t.0)
                .map_err(|e| timestamp_error(text, e))
        }),
        L::TimestampTz => string_loop::<i64>(source, result, count, mode, |input, text| {
            Timestamp::try_convert_with_offset(input, None)
                .map(|t| t.0)
                .map_err(|e| timestamp_error(text, e))
        }),
        L::TimestampTzNs => string_loop::<i64>(source, result, count, mode, |input, text| {
            let mut nanos = 0;
            let micros =
                Timestamp::try_convert_with_offset(input, Some(&mut nanos)).map_err(|e| timestamp_error(text, e))?;
            timestamp_to_nanos(micros, nanos).ok_or_else(|| timestamp_error(text, TimestampCastError::IncorrectFormat))
        }),
        L::TimestampNs | L::TimestampMs | L::TimestampSec => {
            let ticks = precision(target.id()).expect("a timestamp target");
            string_loop::<i64>(source, result, count, mode, |input, text| {
                string_to_scaled_timestamp(input, text, ticks)
            })
        }
        L::Interval => string_loop::<Interval>(source, result, count, mode, |_, text| {
            Interval::parse(text).map_err(|e| match e {
                IntervalParseError::Invalid => {
                    conversion_error(format!("Could not convert string '{text}' to INTERVAL"))
                }
                IntervalParseError::Error(kind, message) => Error::new(kind, message),
            })
        }),
        L::Uuid => string_loop::<i128>(source, result, count, mode, |_, text| {
            uuid_from_str(text, false)
                .ok_or_else(|| conversion_error(format!("Could not convert string '{text}' to {target}")))
        }),
        L::Blob => string_to_bytes(source, result, count, mode, string_to_blob),
        other => unreachable!("no cast from VARCHAR to {other:?}"),
    }
}

const UUID_FLIP: u128 = 1 << 127;

/// The casts from and to UUID, other than through VARCHAR.
fn uuid_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    use LogicalTypeId as L;
    match (source.logical_type().id(), result.logical_type().id()) {
        (L::Uuid, L::UHugeInt) => {
            executor::unary_fallible::<i128, u128>(source, result, count, |v| Ok(Some(v as u128 ^ UUID_FLIP)))
        }
        (L::UHugeInt, L::Uuid) => {
            executor::unary_fallible::<u128, i128>(source, result, count, |v| Ok(Some((v ^ UUID_FLIP) as i128)))
        }
        (L::Uuid, L::Blob) => {
            let u = source.to_unified(count);
            result.prepare_flat(count.max(1));
            for i in 0..count {
                let idx = u.sel.get(i);
                if u.validity.is_valid(idx) {
                    let stored = u.data::<i128>()[idx];
                    result.set_str(i, &(stored as u128 ^ UUID_FLIP).to_be_bytes());
                } else {
                    result.set_null(i, true);
                }
            }
            Ok(())
        }
        (L::Blob, L::Uuid) => string_loop::<i128>(source, result, count, mode, |input, _| {
            let bytes: [u8; 16] = input
                .try_into()
                .map_err(|_| conversion_error("BLOB must be exactly 16 bytes to convert to UUID".to_string()))?;
            Ok((u128::from_be_bytes(bytes) ^ UUID_FLIP) as i128)
        }),
        (from, to) => unreachable!("no cast from {from:?} to {to:?}"),
    }
}

/// The cast from `source` to `target` this module implements, if there is one.
#[allow(clippy::type_complexity)]
fn route(
    source: LogicalTypeId,
    target: LogicalTypeId,
) -> Option<fn(&Vector, &mut Vector, usize, CastMode) -> Result<()>> {
    use LogicalTypeId as L;
    match (source, target) {
        (L::Varchar, target) if is_string_target(target) => Some(string_cast),
        (L::Date, target) if precision(target).is_some() => Some(date_cast),
        (L::Time, L::TimeNs | L::TimeTz) | (L::TimeNs | L::TimeTz, L::Time) => Some(time_cast),
        (L::Uuid, L::UHugeInt | L::Blob) | (L::UHugeInt | L::Blob, L::Uuid) => Some(uuid_cast),
        (source, target) if timestamp_cast_exists(source, target) => Some(timestamp_cast),
        _ => None,
    }
}

/// Whether [`temporal_cast`] casts `source` to `target`.
pub(super) fn has_temporal_cast(source: &LogicalType, target: &LogicalType) -> bool {
    route(source.id(), target.id()).is_some()
}

/// Casts between the types of this module; see [`has_temporal_cast`].
pub(super) fn temporal_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let cast = route(source.logical_type().id(), result.logical_type().id()).expect("a temporal cast");
    cast(source, result, count, mode)
}

/// `TryVectorNullCast`: there is no such cast, so only NULL gets through.
pub(super) fn null_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let u = source.to_unified(count);
    if mode == CastMode::Strict && (0..count).any(|i| u.validity.is_valid(u.sel.get(i))) {
        return Err(conversion_error(format!(
            "Unimplemented type for cast ({} -> {})",
            source.logical_type(),
            result.logical_type()
        )));
    }
    *result = Vector::constant_null(result.logical_type().clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_away_from_the_epoch() {
        assert_eq!(rescale(1_500, MILLIS, SECONDS), Some(2));
        assert_eq!(rescale(-1_500, MILLIS, SECONDS), Some(-2));
        assert_eq!(rescale(1_499, MILLIS, SECONDS), Some(1));
        assert_eq!(rescale(2, SECONDS, MILLIS), Some(2_000));
        assert_eq!(rescale(i64::MAX, MICROS, NANOS), Some(i64::MAX));
        assert_eq!(rescale(i64::MAX - 1, MICROS, NANOS), None);
    }

    #[test]
    fn dates_take_the_date_of_a_timestamp() {
        let date = |s: &str| string_to_date(s.as_bytes()).map(|d| d.to_string());
        assert_eq!(date("1992-01-01").unwrap(), "1992-01-01");
        assert_eq!(date(" 1992-01-01 ").unwrap(), "1992-01-01");
        assert_eq!(date("1992-01-01 12:00:00").unwrap(), "1992-01-01");
        assert_eq!(date("nonsense"), Err(DateCastError::IncorrectFormat));
        assert_eq!(date("1992-13-01"), Err(DateCastError::Range));
    }

    #[test]
    fn blobs_take_escapes_and_ascii() {
        assert_eq!(string_to_blob(b"a\\x00\\xFFb").unwrap(), b"a\x00\xFFb");
        assert_eq!(
            string_to_blob(b"\\xZ0").unwrap_err().to_string(),
            "Conversion Error: Invalid hex escape code encountered in string -> blob conversion of string \
             \"\\xZ0\": \\xZ0"
        );
        assert!(string_to_blob(b"\\x0").is_err());
        assert!(string_to_blob("é".as_bytes()).is_err());
    }
}
