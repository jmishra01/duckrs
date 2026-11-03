// Ported from duckdb/src/common/types/timestamp.cpp and string_cast.cpp (StringFromTimestamp)
// @ rust-port-baseline

use super::date::{Date, DateCastError, is_space, write_ymd};
use super::time::{
    MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SEC, NANOS_PER_DAY, NANOS_PER_MICRO, Time,
    write_time,
};
use std::fmt;

/// `timestamp_t`: microseconds since 1970-01-01 00:00:00 UTC. TIMESTAMP_S, TIMESTAMP_MS and
/// TIMESTAMP_NS store seconds, milliseconds and nanoseconds in the same 64 bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Timestamp(pub i64);

/// Why a timestamp failed to parse (`TimestampCastResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampCastError {
    IncorrectFormat,
    Range,
    /// A time zone other than UTC, which needs the ICU extension.
    NonUtcTimezone,
}

impl Timestamp {
    pub const INFINITY: Timestamp = Timestamp(i64::MAX);
    pub const NINFINITY: Timestamp = Timestamp(-i64::MAX);
    pub const EPOCH: Timestamp = Timestamp(0);

    pub fn is_finite(self) -> bool {
        self != Self::INFINITY && self != Self::NINFINITY
    }

    /// `Timestamp::TryFromDatetime`.
    pub fn from_datetime(date: Date, time: Time) -> Option<Timestamp> {
        let ts = combine_units(i64::from(date.0), time.0, MICROS_PER_DAY)?;
        let ts = Timestamp(ts);
        ts.is_finite().then_some(ts)
    }

    /// The date part (`Timestamp::GetDate`); infinities map to date infinities.
    pub fn date(self) -> Date {
        if self == Self::INFINITY {
            return Date::INFINITY;
        }
        if self == Self::NINFINITY {
            return Date::NINFINITY;
        }
        Date(self.0.div_euclid(MICROS_PER_DAY) as i32)
    }

    /// The time-of-day part of a finite timestamp (`Timestamp::GetTime`).
    pub fn time(self) -> Time {
        Time(self.0.rem_euclid(MICROS_PER_DAY))
    }

    /// Parses a timestamp (`Timestamp::TryConvertTimestamp` with `use_offset = false`): a UTC
    /// offset or `Z` is accepted but not applied, and only `UTC` is accepted as a zone name.
    pub fn try_convert(buf: &[u8], nanos: Option<&mut i32>) -> Result<Timestamp, TimestampCastError> {
        Self::convert(buf, false, nanos)
    }

    /// Like [`Self::try_convert`], but applies a parsed UTC offset (`use_offset = true`).
    pub fn try_convert_with_offset(buf: &[u8], nanos: Option<&mut i32>) -> Result<Timestamp, TimestampCastError> {
        Self::convert(buf, true, nanos)
    }

    fn convert(buf: &[u8], use_offset: bool, nanos: Option<&mut i32>) -> Result<Timestamp, TimestampCastError> {
        let (result, zone) = convert_tz(buf, use_offset, nanos)?;
        match zone {
            None => Ok(result),
            Some(z) if z.eq_ignore_ascii_case(b"utc") => Ok(result),
            Some(_) if use_offset => Err(TimestampCastError::NonUtcTimezone),
            Some(_) => Ok(result),
        }
    }

    /// Parses a complete TIMESTAMP string (`Timestamp::FromCString`) with DuckDB's errors.
    pub fn parse(s: &str) -> crate::Result<Timestamp> {
        Self::try_convert(s.as_bytes(), None).map_err(|e| timestamp_error(s, e))
    }

    /// Parses a TIMESTAMP_NS string, keeping nine fractional digits.
    pub fn parse_ns(s: &str) -> crate::Result<i64> {
        let mut nanos = 0;
        let micros = Self::try_convert(s.as_bytes(), Some(&mut nanos)).map_err(|e| timestamp_error(s, e))?;
        if !micros.is_finite() {
            return Ok(micros.0);
        }
        micros
            .0
            .checked_mul(NANOS_PER_MICRO)
            .and_then(|v| v.checked_add(i64::from(nanos)))
            .filter(|v| *v != i64::MAX && *v != -i64::MAX)
            .ok_or_else(|| timestamp_error(s, TimestampCastError::IncorrectFormat))
    }
}

fn timestamp_error(s: &str, e: TimestampCastError) -> crate::Error {
    let message = match e {
        TimestampCastError::IncorrectFormat => format!(
            "invalid timestamp field format: \"{s}\", expected format is (YYYY-MM-DD HH:MM[:SS[.US]][±HH[:MM[:SS]]| ZONE])"
        ),
        TimestampCastError::Range => format!("timestamp field value out of range: \"{s}\""),
        TimestampCastError::NonUtcTimezone => format!(
            "timestamp field value \"{s}\" has a timestamp that is not UTC.\nUse the TIMESTAMPTZ type with the ICU extension loaded to handle non-UTC timestamps."
        ),
    };
    crate::Error::new(crate::ErrorKind::Conversion, message)
}

/// `Timestamp::TryCombineUnits`: `whole * unit + part`, also when `whole * unit` alone
/// overflows but the sum does not (negative values near the minimum).
fn combine_units(whole: i64, part: i64, unit: i64) -> Option<i64> {
    if let Some(w) = whole.checked_mul(unit) {
        return w.checked_add(part);
    }
    if whole >= 0 {
        return None;
    }
    (whole + 1).checked_mul(unit)?.checked_add(part - unit)
}

fn can_start_suffix(c: u8) -> bool {
    c == b'Z' || c == b'+' || c == b'-' || is_space(c)
}

/// `CharacterIsTimeZone`: `[A-Za-z0-9/_]`.
fn is_tz_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'/' || c == b'_'
}

/// `Timestamp::TryConvertTimestampTZ`: returns the timestamp and any trailing zone name.
fn convert_tz<'a>(
    buf: &'a [u8],
    use_offset: bool,
    nanos: Option<&mut i32>,
) -> Result<(Timestamp, Option<&'a [u8]>), TimestampCastError> {
    let parsed = Date::try_convert(buf, false).map_err(|e| match e {
        DateCastError::IncorrectFormat => TimestampCastError::IncorrectFormat,
        DateCastError::Range => TimestampCastError::Range,
    })?;
    let len = buf.len();
    let mut pos = parsed.pos;
    let date = parsed.date;
    if pos == len {
        if date == Date::INFINITY {
            return Ok((Timestamp::INFINITY, None));
        }
        if date == Date::NINFINITY {
            return Ok((Timestamp::NINFINITY, None));
        }
        return Timestamp::from_datetime(date, Time(0))
            .map(|t| (t, None))
            .ok_or(TimestampCastError::Range);
    }
    if buf[pos] == b' ' || buf[pos] == b'T' {
        pos += 1;
    }
    let rest = &buf[pos..];
    let mut time_len = 0;
    while time_len < rest.len() && is_space(rest[time_len]) {
        time_len += 1;
    }
    while time_len < rest.len() && !can_start_suffix(rest[time_len]) {
        time_len += 1;
    }
    let (time, time_pos) = Time::try_convert_interval(&rest[..time_len], false, nanos, false)
        .ok_or(TimestampCastError::IncorrectFormat)?;
    if time_pos != time_len {
        return Err(TimestampCastError::IncorrectFormat);
    }
    if time.0 > MICROS_PER_DAY {
        return Err(TimestampCastError::Range);
    }
    pos += time_pos;
    let mut result = Timestamp::from_datetime(date, time).ok_or(TimestampCastError::Range)?;
    let mut zone = None;
    if pos < len {
        if buf[pos] == b'Z' {
            pos += 1;
        } else if let Some((hh, mm, ss)) = parse_utc_offset(buf, &mut pos, false) {
            let delta =
                i64::from(hh) * MICROS_PER_HOUR + i64::from(mm) * MICROS_PER_MINUTE + i64::from(ss) * MICROS_PER_SEC;
            if use_offset {
                result.0 = result.0.checked_sub(delta).ok_or(TimestampCastError::Range)?;
                if !result.is_finite() {
                    return Err(TimestampCastError::Range);
                }
            }
        } else {
            if buf[pos] != b' ' {
                return Err(TimestampCastError::NonUtcTimezone);
            }
            pos += 1;
            let start = pos;
            while pos < len && is_tz_char(buf[pos]) {
                pos += 1;
            }
            if pos > start {
                zone = Some(&buf[start..pos]);
            }
        }
        while pos < len && is_space(buf[pos]) {
            pos += 1;
        }
        if pos < len {
            return Err(TimestampCastError::IncorrectFormat);
        }
    }
    Ok((result, zone))
}

/// Parses `±H[H][[:]MM[:SS]]` at `pos` (`Timestamp::TryParseUTCOffset`); every part carries the
/// sign. On success `pos` moves past the offset.
pub(crate) fn parse_utc_offset(buf: &[u8], pos: &mut usize, strict: bool) -> Option<(i32, i32, i32)> {
    let len = buf.len();
    let at = |i: usize| buf.get(i).copied().unwrap_or(0);
    let mut cur = *pos;
    let minlen = if strict { 2 } else { 1 };
    if cur + minlen + 1 > len {
        return if strict { None } else { Some((0, 0, 0)) };
    }
    let sign = buf[cur];
    if sign != b'+' && sign != b'-' {
        return None;
    }
    let negate = |v: i32| if sign == b'-' { -v } else { v };
    cur += 1;
    if !at(cur).is_ascii_digit() {
        return None;
    }
    let mut hh = i32::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        hh = hh * 10 + i32::from(at(cur) - b'0');
        cur += 1;
    } else if strict {
        return None;
    }
    let hh = negate(hh);
    if cur >= len {
        *pos = cur;
        return Some((hh, 0, 0));
    }
    let colons = at(cur) == b':';
    if colons {
        cur += 1;
    }
    if cur + minlen > len || !at(cur).is_ascii_digit() {
        *pos = cur;
        return (!colons).then_some((hh, 0, 0));
    }
    let mut mm = i32::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        mm = mm * 10 + i32::from(at(cur) - b'0');
        cur += 1;
    } else if strict {
        return None;
    }
    let mm = negate(mm);
    if cur >= len || !colons || at(cur) != b':' {
        *pos = cur;
        return Some((hh, mm, 0));
    }
    cur += 1;
    if cur + minlen > len || !at(cur).is_ascii_digit() {
        *pos = cur;
        return None;
    }
    let mut ss = i32::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        ss = ss * 10 + i32::from(at(cur) - b'0');
        cur += 1;
    } else if strict {
        return None;
    }
    *pos = cur;
    Some((hh, mm, negate(ss)))
}

/// Formats `date time[.fraction][+00]` (`StringFromTimestamp`), or `infinity`/`-infinity`.
fn format_timestamp(date: Date, time: Time, extra_nanos: i64, utc_suffix: bool) -> String {
    let mut s = String::with_capacity(32);
    let (y, m, d) = date.to_ymd();
    write_ymd(&mut s, y, m, d);
    s.push(' ');
    write_time(&mut s, time, extra_nanos);
    if utc_suffix {
        s.push_str("+00");
    }
    s
}

fn infinity_string(value: i64) -> Option<String> {
    match value {
        i64::MAX => Some("infinity".into()),
        v if v == -i64::MAX => Some("-infinity".into()),
        _ => None,
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&timestamp_to_string(self.0, false))
    }
}

/// TIMESTAMP (or with `tz`, TIMESTAMP WITH TIME ZONE, printed in UTC as without ICU).
pub fn timestamp_to_string(micros: i64, tz: bool) -> String {
    if let Some(s) = infinity_string(micros) {
        return s;
    }
    let ts = Timestamp(micros);
    format_timestamp(ts.date(), ts.time(), 0, tz)
}

/// TIMESTAMP_NS (or with `tz`, TIMESTAMPTZ_NS).
pub fn timestamp_ns_to_string(nanos: i64, tz: bool) -> String {
    if let Some(s) = infinity_string(nanos) {
        return s;
    }
    let date = Date(nanos.div_euclid(NANOS_PER_DAY) as i32);
    let time_nanos = nanos.rem_euclid(NANOS_PER_DAY);
    format_timestamp(
        date,
        Time(time_nanos / NANOS_PER_MICRO),
        time_nanos % NANOS_PER_MICRO,
        tz,
    )
}

/// TIMESTAMP_MS: converted to microseconds, then formatted.
pub fn timestamp_ms_to_string(millis: i64) -> String {
    match infinity_string(millis) {
        Some(s) => s,
        None => timestamp_to_string(millis * 1000, false),
    }
}

/// TIMESTAMP_S: converted to microseconds, then formatted.
pub fn timestamp_sec_to_string(seconds: i64) -> String {
    match infinity_string(seconds) {
        Some(s) => s,
        None => timestamp_to_string(seconds * MICROS_PER_SEC, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_matches_duckdb() {
        let ts = Timestamp::from_datetime(Date(8035), Time::from_hms(12, 0, 0, 123_000)).unwrap();
        assert_eq!(ts.to_string(), "1992-01-01 12:00:00.123");
        assert_eq!(Timestamp(-1).to_string(), "1969-12-31 23:59:59.999999");
        assert_eq!(Timestamp::INFINITY.to_string(), "infinity");
        assert_eq!(Timestamp::NINFINITY.to_string(), "-infinity");
        assert_eq!(
            timestamp_ns_to_string(1_577_836_800_123_456_789, false),
            "2020-01-01 00:00:00.123456789"
        );
        assert_eq!(
            timestamp_ns_to_string(1_577_836_800_000_000_001, false),
            "2020-01-01 00:00:00.000000001"
        );
        assert_eq!(timestamp_ms_to_string(1_577_836_800_123), "2020-01-01 00:00:00.123");
        assert_eq!(timestamp_sec_to_string(1_577_873_472), "2020-01-01 10:11:12");
        assert_eq!(
            timestamp_to_string(1_577_873_472_500_000, true),
            "2020-01-01 10:11:12.5+00"
        );
    }

    #[test]
    fn parsing() {
        let p = |s: &str| Timestamp::parse(s).map(|t| t.to_string());
        assert_eq!(p("1992-01-01 12:00:00.123").unwrap(), "1992-01-01 12:00:00.123");
        assert_eq!(p("1992-01-01T12:00:00Z").unwrap(), "1992-01-01 12:00:00");
        assert_eq!(p("1992-01-01").unwrap(), "1992-01-01 00:00:00");
        assert_eq!(p("1992-01-01 12:00").unwrap(), "1992-01-01 12:00:00");
        assert_eq!(p("infinity").unwrap(), "infinity");
        // Without use_offset the offset is parsed but not applied.
        assert_eq!(p("1992-01-01 12:00:00+05").unwrap(), "1992-01-01 12:00:00");
        let applied = Timestamp::try_convert_with_offset(b"1992-01-01 12:00:00+05:30", None).unwrap();
        assert_eq!(applied.to_string(), "1992-01-01 06:30:00");
        assert!(Timestamp::try_convert_with_offset(b"1992-01-01 12:00:00 Europe/Paris", None).is_err());
        assert_eq!(p("1992-01-01 12:00:00 UTC").unwrap(), "1992-01-01 12:00:00");
        assert_eq!(
            p("1992-01-01 25:00:00").unwrap_err().to_string(),
            "Conversion Error: timestamp field value out of range: \"1992-01-01 25:00:00\""
        );
        assert!(
            p("1992-01-01 12:00:00 x").is_ok(),
            "non-UTC zones are ignored unless applied"
        );
        assert!(p("1992-01-01 12:00:00 +").is_err());
        assert_eq!(
            Timestamp::parse_ns("2020-01-01 00:00:00.123456789").unwrap(),
            1_577_836_800_123_456_789
        );
    }

    #[test]
    fn negative_timestamps_split_correctly() {
        let ts = Timestamp(-1);
        assert_eq!(ts.date(), Date(-1));
        assert_eq!(ts.time(), Time(MICROS_PER_DAY - 1));
        assert_eq!(Timestamp::from_datetime(ts.date(), ts.time()), Some(ts));
    }
}
