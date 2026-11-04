// Ported from duckdb/src/common/types/time.cpp, datetime.hpp (dtime_tz_t), and string_cast.cpp /
// cast_helpers.hpp (TimeToStringCast) @ rust-port-baseline

use super::date::{is_space, parse_double_digit};
use super::timestamp::{Timestamp, parse_utc_offset};
use std::fmt;
use std::fmt::Write;

pub const MICROS_PER_SEC: i64 = 1_000_000;
pub const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SEC;
pub const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
pub const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;
pub const NANOS_PER_MICRO: i64 = 1000;
pub const NANOS_PER_DAY: i64 = MICROS_PER_DAY * NANOS_PER_MICRO;

/// `dtime_t`: microseconds since midnight, `0..=24:00:00`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Time(pub i64);

impl Time {
    /// `Time::FromTime`.
    pub fn from_hms(hour: i32, minute: i32, second: i32, micros: i32) -> Time {
        let mut v = i64::from(hour);
        v = v * 60 + i64::from(minute);
        v = v * 60 + i64::from(second);
        Time(v * MICROS_PER_SEC + i64::from(micros))
    }

    /// `(hour, minute, second, micros)` (`Time::Convert`).
    pub fn to_hms(self) -> (i32, i32, i32, i32) {
        let mut t = self.0;
        let hour = t / MICROS_PER_HOUR;
        t -= hour * MICROS_PER_HOUR;
        let min = t / MICROS_PER_MINUTE;
        t -= min * MICROS_PER_MINUTE;
        let sec = t / MICROS_PER_SEC;
        t -= sec * MICROS_PER_SEC;
        (hour as i32, min as i32, sec as i32, t as i32)
    }

    /// Parses `H+:MM[:SS[.fraction]]` (`Time::TryConvertInternal`), allowing up to ten hour
    /// digits so intervals can reuse it. With `nanos`, nine fractional digits are kept and the
    /// sub-microsecond part is returned there. Returns the time and the bytes consumed.
    pub fn try_convert_interval(
        buf: &[u8],
        strict: bool,
        nanos: Option<&mut i32>,
        negative: bool,
    ) -> Option<(Time, usize)> {
        let len = buf.len();
        let mut pos = 0;
        while pos < len && is_space(buf[pos]) {
            pos += 1;
        }
        if pos >= len || !buf[pos].is_ascii_digit() {
            return None;
        }
        let mut hour: i64 = 0;
        let mut digits = 10;
        while pos < len && buf[pos].is_ascii_digit() {
            if digits == 0 {
                return None;
            }
            digits -= 1;
            hour = hour * 10 + i64::from(buf[pos] - b'0');
            pos += 1;
        }
        if pos >= len || buf[pos] != b':' {
            return None;
        }
        pos += 1;
        let sep_pos = pos;
        let min = if pos == len && !strict {
            0
        } else {
            let m = parse_double_digit(buf, &mut pos)?;
            if !(0..60).contains(&m) {
                return None;
            }
            m
        };
        let sec = if pos == len {
            if strict && sep_pos + 2 != pos {
                return None;
            }
            0
        } else {
            if buf[pos] != b':' {
                return None;
            }
            pos += 1;
            if pos == len && !strict {
                0
            } else {
                let s = parse_double_digit(buf, &mut pos)?;
                if !(0..60).contains(&s) {
                    return None;
                }
                s
            }
        };
        let mut micros: i32 = 0;
        if pos < len && buf[pos] == b'.' {
            pos += 1;
            let mut mult: i32 = if nanos.is_some() { 100_000_000 } else { 100_000 };
            while pos < len && buf[pos].is_ascii_digit() {
                if mult > 0 {
                    micros += i32::from(buf[pos] - b'0') * mult;
                }
                mult /= 10;
                pos += 1;
            }
            if let Some(n) = nanos {
                *n = micros % NANOS_PER_MICRO as i32;
                micros /= NANOS_PER_MICRO as i32;
            }
        }
        if strict {
            while pos < len && is_space(buf[pos]) {
                pos += 1;
            }
            if pos < len {
                return None;
            }
        }
        let sub_hour = Time::from_hms(0, min, sec, micros).0;
        let total = if negative {
            hour.checked_mul(-MICROS_PER_HOUR)?.checked_sub(sub_hour)?
        } else {
            hour.checked_mul(MICROS_PER_HOUR)?.checked_add(sub_hour)?
        };
        Some((Time(total), pos))
    }

    /// Parses a TIME (`Time::TryConvertTime`). Non-strict parsing also accepts a timestamp and
    /// takes its time of day.
    pub fn try_convert(buf: &[u8], strict: bool, nanos: Option<&mut i32>) -> Option<Time> {
        let mut nanos = nanos;
        match Self::try_convert_interval(buf, strict, nanos.as_deref_mut(), false) {
            Some((t, _)) => (t.0 <= MICROS_PER_DAY).then_some(t),
            None if !strict => {
                let ts = Timestamp::try_convert(buf, nanos).ok()?;
                ts.is_finite().then(|| ts.time())
            }
            None => None,
        }
    }

    /// Parses a complete TIME string (`Time::FromCString`).
    pub fn parse(s: &str, strict: bool) -> crate::Result<Time> {
        Self::try_convert(s.as_bytes(), strict, None).ok_or_else(|| time_error(s))
    }
}

/// `Time::ConversionError`.
pub fn time_error(s: &str) -> crate::Error {
    crate::Error::new(
        crate::ErrorKind::Conversion,
        format!("time field value out of range: \"{s}\", expected format is ([YYYY-MM-DD ]HH:MM:SS[.MS])"),
    )
}

/// Appends `digits` zero-padded to `width`, dropping trailing zeros (`TimeToStringCast::FormatMicros`).
fn push_fraction(out: &mut String, value: i64, width: usize) {
    let s = format!("{value:0width$}");
    out.push_str(s.trim_end_matches('0'));
}

/// Writes `HH:MM:SS[.ffffff]` for a time of day, plus `extra_nanos` (0..1000) of sub-microsecond
/// precision (`StringAsTime`): trailing zeros are trimmed, but microseconds are written in full
/// when nanoseconds follow.
pub(crate) fn write_time(out: &mut String, time: Time, extra_nanos: i64) {
    let (h, m, s, us) = time.to_hms();
    let _ = write!(out, "{h:02}:{m:02}:{s:02}");
    if extra_nanos != 0 {
        let _ = write!(out, ".{us:06}");
        push_fraction(out, extra_nanos, 3);
    } else if us != 0 {
        out.push('.');
        push_fraction(out, i64::from(us), 6);
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::with_capacity(15);
        write_time(&mut s, *self, 0);
        f.write_str(&s)
    }
}

/// Formats a TIME_NS value (nanoseconds since midnight).
pub fn time_ns_to_string(nanos: i64) -> String {
    let mut s = String::with_capacity(18);
    write_time(&mut s, Time(nanos / NANOS_PER_MICRO), nanos % NANOS_PER_MICRO);
    s
}

/// `dtime_tz_t`: a time of day plus a UTC offset in seconds, packed into 64 bits so that the
/// integer order sorts by time and then by offset (`micros << 24 | (MAX_OFFSET - offset)`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TimeTz(pub u64);

impl TimeTz {
    const OFFSET_BITS: u32 = 24;
    const OFFSET_MASK: u64 = u64::MAX >> 40;
    /// ±15:59:59.
    pub const MAX_OFFSET: i32 = 16 * 60 * 60 - 1;
    pub const MIN_OFFSET: i32 = -Self::MAX_OFFSET;

    pub fn new(time: Time, offset_seconds: i32) -> TimeTz {
        TimeTz(((time.0 as u64) << Self::OFFSET_BITS) | (Self::MAX_OFFSET - offset_seconds) as u64)
    }

    pub fn time(self) -> Time {
        Time((self.0 >> Self::OFFSET_BITS) as i64)
    }

    pub fn offset(self) -> i32 {
        Self::MAX_OFFSET - (self.0 & Self::OFFSET_MASK) as i32
    }

    /// Parses `time[±HH[:MM[:SS]]]` (`Time::TryConvertTimeTZ`).
    pub fn try_convert(buf: &[u8], strict: bool) -> Option<TimeTz> {
        let Some((time, mut pos)) = Time::try_convert_interval(buf, false, None, false) else {
            if strict {
                return None;
            }
            let ts = Timestamp::try_convert_with_offset(buf, None).ok()?;
            return ts.is_finite().then(|| TimeTz::new(ts.time(), 0));
        };
        if time.0 > MICROS_PER_DAY {
            return None;
        }
        let len = buf.len();
        while pos < len && is_space(buf[pos]) {
            pos += 1;
        }
        let (mut hh, mut mm, mut ss) = (0, 0, 0);
        if pos < len {
            (hh, mm, ss) = parse_utc_offset(buf, &mut pos, true)?;
        }
        let offset = (hh * 60 + mm) * 60 + ss;
        if !(Self::MIN_OFFSET..=Self::MAX_OFFSET).contains(&offset) {
            return None;
        }
        if strict {
            while pos < len && is_space(buf[pos]) {
                pos += 1;
            }
            if pos < len {
                return None;
            }
        }
        Some(TimeTz::new(time, offset))
    }
}

impl fmt::Display for TimeTz {
    /// `StringCast::Operation(dtime_tz_t)`: the time, then `±HH[:MM[:SS]]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::with_capacity(24);
        write_time(&mut s, self.time(), 0);
        let offset = self.offset();
        s.push(if offset < 0 { '-' } else { '+' });
        let mut secs = offset.abs();
        let hh = secs / 3600;
        secs %= 3600;
        let (mm, ss) = (secs / 60, secs % 60);
        let _ = write!(s, "{hh:02}");
        if mm != 0 || ss != 0 {
            let _ = write!(s, ":{mm:02}");
        }
        if ss != 0 {
            let _ = write!(s, ":{ss:02}");
        }
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_matches_duckdb() {
        assert_eq!(Time(0).to_string(), "00:00:00");
        assert_eq!(Time::from_hms(23, 59, 59, 999_999).to_string(), "23:59:59.999999");
        assert_eq!(Time::from_hms(12, 30, 0, 500_000).to_string(), "12:30:00.5");
        assert_eq!(Time(MICROS_PER_DAY).to_string(), "24:00:00");
        assert_eq!(time_ns_to_string(1), "00:00:00.000000001");
        assert_eq!(time_ns_to_string(123_456_789), "00:00:00.123456789");
        assert_eq!(time_ns_to_string(5_000_000_000), "00:00:05");
        let t = Time::from_hms(10, 11, 12, 0);
        assert_eq!(TimeTz::new(t, 5 * 3600 + 30 * 60).to_string(), "10:11:12+05:30");
        assert_eq!(TimeTz::new(t, -8 * 3600).to_string(), "10:11:12-08");
        assert_eq!(
            TimeTz::new(Time::from_hms(10, 11, 12, 250_000), 3723).to_string(),
            "10:11:12.25+01:02:03"
        );
    }

    #[test]
    fn parsing() {
        assert_eq!(
            Time::parse("12:30:00.5", true).unwrap(),
            Time::from_hms(12, 30, 0, 500_000)
        );
        assert_eq!(Time::parse(" 01:02", true).unwrap(), Time::from_hms(1, 2, 0, 0));
        // As in C++, a space after HH:MM is not a valid seconds separator.
        assert!(Time::parse("01:02 ", true).is_err());
        assert_eq!(Time::parse("24:00:00", true).unwrap(), Time(MICROS_PER_DAY));
        assert!(Time::parse("24:00:01", true).is_err());
        assert!(Time::parse("12:60:00", true).is_err());
        assert_eq!(
            Time::parse("1992-01-01 10:00:00", false).unwrap(),
            Time::from_hms(10, 0, 0, 0)
        );
        assert!(Time::parse("1992-01-01 10:00:00", true).is_err());
        let mut nanos = 0;
        let t = Time::try_convert(b"00:00:00.123456789", true, Some(&mut nanos)).unwrap();
        assert_eq!((t.0, nanos), (123_456, 789));
        let tz = TimeTz::try_convert(b"10:11:12+05:30", true).unwrap();
        assert_eq!((tz.time(), tz.offset()), (Time::from_hms(10, 11, 12, 0), 19_800));
        assert!(TimeTz::try_convert(b"10:11:12+16", true).is_none());
    }

    #[test]
    fn time_tz_orders_by_time_then_offset() {
        let a = TimeTz::new(Time::from_hms(10, 0, 0, 0), 3600);
        let b = TimeTz::new(Time::from_hms(10, 0, 0, 0), -3600);
        let c = TimeTz::new(Time::from_hms(11, 0, 0, 0), 3600);
        assert!(a < b && b < c);
        assert_eq!(b.offset(), -3600);
    }
}
