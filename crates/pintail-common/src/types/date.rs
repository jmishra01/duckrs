// Ported from duckdb/src/common/types/date.cpp and cast_helpers.hpp (DateToStringCast)
// @ rust-port-baseline
//
// The C++ code converts days <-> (year, month, day) with 400-year lookup tables; this uses the
// equivalent closed-form proleptic Gregorian algorithm (H. Hinnant, "chrono-compatible low-level
// date algorithms"). Year 0 exists and is displayed as 1 BC.

use std::fmt;

/// `date_t`: days since 1970-01-01.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Date(pub i32);

/// Outcome of parsing a date (`DateCastResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateCastError {
    IncorrectFormat,
    Range,
}

/// What [`Date::try_convert`] parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDate {
    pub date: Date,
    /// Bytes consumed.
    pub pos: usize,
    /// `infinity`, `-infinity` or `epoch` rather than a calendar date.
    pub special: bool,
}

const NORMAL_DAYS: [i32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const LEAP_DAYS: [i32; 13] = [0, 31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

pub(crate) fn is_space(c: u8) -> bool {
    // StringUtil::CharacterIsSpace
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

impl Date {
    pub const INFINITY: Date = Date(i32::MAX);
    pub const NINFINITY: Date = Date(-i32::MAX);
    pub const EPOCH: Date = Date(0);

    pub const MIN_YEAR: i32 = -5877641;
    pub const MIN_MONTH: i32 = 6;
    pub const MIN_DAY: i32 = 25;
    pub const MAX_YEAR: i32 = 5881580;
    pub const MAX_MONTH: i32 = 7;
    pub const MAX_DAY: i32 = 10;

    pub fn is_finite(self) -> bool {
        self != Self::INFINITY && self != Self::NINFINITY
    }

    pub fn is_leap_year(year: i32) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }

    pub fn month_days(year: i32, month: i32) -> i32 {
        if Self::is_leap_year(year) {
            LEAP_DAYS[month as usize]
        } else {
            NORMAL_DAYS[month as usize]
        }
    }

    /// Whether `(year, month, day)` is a real date within DuckDB's range (`Date::IsValid`).
    pub fn is_valid(year: i32, month: i32, day: i32) -> bool {
        if !(1..=12).contains(&month) || day < 1 {
            return false;
        }
        if year < Self::MIN_YEAR
            || (year == Self::MIN_YEAR
                && (month < Self::MIN_MONTH || (month == Self::MIN_MONTH && day < Self::MIN_DAY)))
        {
            return false;
        }
        if year > Self::MAX_YEAR
            || (year == Self::MAX_YEAR
                && (month > Self::MAX_MONTH || (month == Self::MAX_MONTH && day > Self::MAX_DAY)))
        {
            return false;
        }
        day <= Self::month_days(year, month)
    }

    /// `Date::TryFromDate`.
    pub fn from_ymd(year: i32, month: i32, day: i32) -> Option<Date> {
        if !Self::is_valid(year, month, day) {
            return None;
        }
        let y = i64::from(year) - i64::from(month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = i64::from((month + 9) % 12);
        let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(Date((era * 146_097 + doe - 719_468) as i32))
    }

    /// `(year, month, day)` of a finite date (`Date::Convert`).
    pub fn to_ymd(self) -> (i32, i32, i32) {
        let z = i64::from(self.0) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        (year as i32, month as i32, day as i32)
    }

    /// Parses a date prefix of `buf` (`Date::TryConvertDate`): `[-]YYYY-MM-DD` with `-`, `/`, `\`
    /// or space separators, an optional ` (BC)` suffix, or `infinity`/`-infinity`/`epoch`. In
    /// strict mode only trailing spaces may follow; otherwise parsing stops before any non-digit.
    pub fn try_convert(buf: &[u8], strict: bool) -> Result<ParsedDate, DateCastError> {
        use DateCastError::*;
        let len = buf.len();
        let mut pos = 0;
        if len == 0 {
            return Err(IncorrectFormat);
        }
        while pos < len && is_space(buf[pos]) {
            pos += 1;
        }
        if pos >= len {
            return Err(IncorrectFormat);
        }
        let mut year_neg = false;
        if buf[pos] == b'-' {
            year_neg = true;
            pos += 1;
            if pos >= len {
                return Err(IncorrectFormat);
            }
        }
        if !buf[pos].is_ascii_digit() {
            let date = if let Some(p) = convert_special(buf, pos, b"infinity", 3) {
                pos = p;
                if year_neg { Self::NINFINITY } else { Self::INFINITY }
            } else if let Some(p) = convert_special(buf, pos, b"epoch", 5) {
                pos = p;
                Self::EPOCH
            } else {
                return Err(IncorrectFormat);
            };
            while pos < len && is_space(buf[pos]) {
                pos += 1;
            }
            return if pos == len {
                Ok(ParsedDate {
                    date,
                    pos,
                    special: true,
                })
            } else {
                Err(IncorrectFormat)
            };
        }
        let mut year: i32 = 0;
        let mut year_length = 0;
        while pos < len && buf[pos].is_ascii_digit() {
            if year >= 100_000_000 {
                return Err(Range);
            }
            year = year * 10 + i32::from(buf[pos] - b'0');
            year_length += 1;
            pos += 1;
        }
        if year_length < 2 && strict {
            return Err(IncorrectFormat);
        }
        if year_neg {
            year = -year;
        }
        if pos >= len {
            return Err(IncorrectFormat);
        }
        let sep = buf[pos];
        pos += 1;
        if !matches!(sep, b' ' | b'-' | b'/' | b'\\') {
            return Err(IncorrectFormat);
        }
        let month = parse_double_digit(buf, &mut pos).ok_or(IncorrectFormat)?;
        if pos >= len || buf[pos] != sep {
            return Err(IncorrectFormat);
        }
        pos += 1;
        if pos >= len {
            return Err(IncorrectFormat);
        }
        let day = parse_double_digit(buf, &mut pos).ok_or(IncorrectFormat)?;
        if len - pos >= 5
            && is_space(buf[pos])
            && buf[pos + 1] == b'('
            && buf[pos + 2].eq_ignore_ascii_case(&b'b')
            && buf[pos + 3].eq_ignore_ascii_case(&b'c')
            && buf[pos + 4] == b')'
        {
            if year_neg || year == 0 {
                return Err(IncorrectFormat);
            }
            year = -year + 1;
            pos += 5;
        }
        if strict {
            while pos < len && is_space(buf[pos]) {
                pos += 1;
            }
            if pos < len {
                return Err(IncorrectFormat);
            }
        } else if pos < len && buf[pos].is_ascii_digit() {
            return Err(IncorrectFormat);
        }
        let date = Self::from_ymd(year, month, day).ok_or(Range)?;
        Ok(ParsedDate {
            date,
            pos,
            special: false,
        })
    }

    /// Parses a complete date string (`Date::FromCString`), with DuckDB's error messages.
    pub fn parse(s: &str, strict: bool) -> crate::Result<Date> {
        Self::try_convert(s.as_bytes(), strict)
            .map(|parsed| parsed.date)
            .map_err(|e| date_error(s, e))
    }
}

/// `Date::FormatError` and `Date::RangeError`.
pub fn date_error(s: &str, e: DateCastError) -> crate::Error {
    let message = match e {
        DateCastError::IncorrectFormat => {
            format!("invalid date field format: \"{s}\", expected format is (YYYY-MM-DD)")
        }
        DateCastError::Range => format!("date field value out of range: \"{s}\""),
    };
    crate::Error::new(crate::ErrorKind::Conversion, message)
}

/// `Date::ParseDoubleDigit`: one or two digits.
pub(crate) fn parse_double_digit(buf: &[u8], pos: &mut usize) -> Option<i32> {
    if *pos < buf.len() && buf[*pos].is_ascii_digit() {
        let mut result = i32::from(buf[*pos] - b'0');
        *pos += 1;
        if *pos < buf.len() && buf[*pos].is_ascii_digit() {
            result = result * 10 + i32::from(buf[*pos] - b'0');
            *pos += 1;
        }
        return Some(result);
    }
    None
}

/// `Date::TryConvertDateSpecial`: matches `word` case-insensitively, or a prefix of it of
/// exactly `abbr` characters when the input ends early.
fn convert_special(buf: &[u8], pos: usize, word: &[u8], abbr: usize) -> Option<usize> {
    let mut p = pos;
    let mut matched = 0;
    while p < buf.len() && matched < word.len() {
        if buf[p].to_ascii_lowercase() != word[matched] {
            return None;
        }
        p += 1;
        matched += 1;
    }
    if matched < word.len() && p - pos != abbr {
        return None;
    }
    Some(p)
}

/// Writes `YYYY-MM-DD[ (BC)]` (`DateToStringCast`); years pad to at least four digits.
pub(crate) fn write_ymd(out: &mut String, year: i32, month: i32, day: i32) {
    use std::fmt::Write;
    let (year, bc) = if year <= 0 { (-year + 1, true) } else { (year, false) };
    let _ = write!(out, "{year:04}-{month:02}-{day:02}");
    if bc {
        out.push_str(" (BC)");
    }
}

impl fmt::Display for Date {
    /// `Date::ToString`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::INFINITY {
            return f.write_str("infinity");
        }
        if *self == Self::NINFINITY {
            return f.write_str("-infinity");
        }
        let (y, m, d) = self.to_ymd();
        let mut s = String::with_capacity(16);
        write_ymd(&mut s, y, m, d);
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Expected output of `SELECT d::VARCHAR, d - DATE '1970-01-01'` in DuckDB.
    #[test]
    fn matches_duckdb() {
        let cases = [
            (8035, "1992-01-01"),
            (-719_163, "0001-12-31 (BC)"),
            (-735_525, "0045-03-15 (BC)"),
            (2_147_483_646, "5881580-07-10"),
            (-2_147_483_646, "5877642-06-25 (BC)"),
        ];
        for (days, text) in cases {
            assert_eq!(Date(days).to_string(), text);
        }
        assert_eq!(Date::from_ymd(1992, 1, 1), Some(Date(8035)));
        assert_eq!(Date::from_ymd(-44, 3, 15), Some(Date(-735_525)));
        assert_eq!(Date::from_ymd(Date::MAX_YEAR, 7, 10), Some(Date(2_147_483_646)));
        assert_eq!(Date::from_ymd(Date::MAX_YEAR, 7, 11), None);
        assert_eq!(Date::from_ymd(Date::MIN_YEAR, 6, 25), Some(Date(-2_147_483_646)));
        assert_eq!(Date::from_ymd(2001, 2, 29), None);
        assert_eq!(Date::INFINITY.to_string(), "infinity");
        assert_eq!(Date::NINFINITY.to_string(), "-infinity");
        assert_eq!(Date::from_ymd(12345, 1, 1).unwrap().to_string(), "12345-01-01");
    }

    #[test]
    fn parsing() {
        let p = |s: &str| Date::parse(s, true);
        assert_eq!(p("1992-01-01").unwrap(), Date(8035));
        assert_eq!(p("  1992/1/1  ").unwrap(), Date(8035));
        assert_eq!(p("0045-03-15 (BC)").unwrap(), Date(-735_525));
        assert_eq!(p("-0044-03-15").unwrap(), Date(-735_525));
        assert_eq!(p("Infinity").unwrap(), Date::INFINITY);
        assert_eq!(p("-inf").unwrap(), Date::NINFINITY);
        assert_eq!(p("epoch").unwrap(), Date::EPOCH);
        assert_eq!(
            p("1992-13-01").unwrap_err().to_string(),
            "Conversion Error: date field value out of range: \"1992-13-01\""
        );
        assert_eq!(
            p("hello").unwrap_err().to_string(),
            "Conversion Error: invalid date field format: \"hello\", expected format is (YYYY-MM-DD)"
        );
        assert!(p("1992-01-01 12:00").is_err());
        assert!(p("1992-01/01").is_err());
        let loose = Date::try_convert(b"1992-01-01 12:00", false).unwrap();
        assert_eq!((loose.date, loose.pos), (Date(8035), 10));
    }

    /// An independent day-by-day reference around tricky boundaries.
    #[test]
    fn walks_consecutive_days() {
        let mut date = Date::from_ymd(1599, 1, 1).unwrap();
        let (mut y, mut m, mut d) = (1599, 1, 1);
        for _ in 0..(366 * 410) {
            assert_eq!(date.to_ymd(), (y, m, d));
            d += 1;
            if d > Date::month_days(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
            date = Date(date.0 + 1);
        }
    }

    proptest! {
        #[test]
        fn round_trips(days in -2_147_483_646i32..=2_147_483_646) {
            let date = Date(days);
            let (y, m, d) = date.to_ymd();
            prop_assert_eq!(Date::from_ymd(y, m, d), Some(date));
            prop_assert_eq!(Date::parse(&date.to_string(), true).unwrap(), date);
        }
    }
}
