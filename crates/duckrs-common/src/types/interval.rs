// Ported from duckdb/src/include/duckdb/common/types/interval.hpp @ rust-port-baseline

/// `interval_t`: months, days and microseconds are kept separately, as in DuckDB.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    pub const DAYS_PER_MONTH: i64 = 30;
    pub const MICROS_PER_DAY: i64 = 86_400_000_000;
    pub const MICROS_PER_MONTH: i64 = Self::DAYS_PER_MONTH * Self::MICROS_PER_DAY;

    pub const fn new(months: i32, days: i32, micros: i64) -> Self {
        Self { months, days, micros }
    }

    /// Normalizes to `(months, days, micros)` with `0 <= days < 30` and `0 <= micros < 1 day`,
    /// carrying micros into days and days into months and borrowing so remainders are never
    /// negative (`interval_t::Normalize`). Comparisons use this form, so
    /// `INTERVAL 30 DAY = INTERVAL 1 MONTH`.
    pub fn normalize(self) -> (i64, i64, i64) {
        let mut micros = self.micros;
        let mut carry_days = micros / Self::MICROS_PER_DAY;
        micros -= carry_days * Self::MICROS_PER_DAY;
        if micros < 0 {
            carry_days -= 1;
            micros += Self::MICROS_PER_DAY;
        }
        let mut days = i64::from(self.days) + carry_days;
        let mut carry_months = days / Self::DAYS_PER_MONTH;
        days -= carry_months * Self::DAYS_PER_MONTH;
        if days < 0 {
            carry_months -= 1;
            days += Self::DAYS_PER_MONTH;
        }
        (i64::from(self.months) + carry_months, days, micros)
    }

    /// The normalized interval, its fields saturated where they overflow and the rest borrowed into the next
    /// smaller one (`interval_t::Normalize` with `Borrow`): its days or micros can then lie out of their ranges.
    pub fn normalize_saturating(self) -> Self {
        fn borrow(msf: i64, lsf: &mut i64, scale: i64) -> i32 {
            let field = msf.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            if field != msf {
                let remainder = msf - field;
                let max_units = i64::MAX / scale - 1;
                *lsf = if remainder > max_units {
                    i64::MAX
                } else if remainder < -max_units {
                    i64::MIN
                } else {
                    *lsf + remainder * scale
                };
            }
            field as i32
        }
        let (months, mut days, mut micros) = self.normalize();
        let months = borrow(months, &mut days, Self::DAYS_PER_MONTH);
        let days = borrow(days, &mut micros, Self::MICROS_PER_DAY);
        Self { months, days, micros }
    }

    /// Equality after normalization (`interval_t::operator==`).
    pub fn equals(self, other: Self) -> bool {
        (self.months == other.months && self.days == other.days && self.micros == other.micros)
            || self.normalize() == other.normalize()
    }

    /// Ordering after normalization (`interval_t::operator>` and friends).
    pub fn compare(self, other: Self) -> std::cmp::Ordering {
        self.normalize().cmp(&other.normalize())
    }
}

/// Why an interval string failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntervalParseError {
    /// Not an interval; the cast reports a generic conversion error.
    Invalid,
    /// A specific error, as DuckDB reports it.
    Error(crate::ErrorKind, String),
}

fn out_of_range(message: &str) -> IntervalParseError {
    IntervalParseError::Error(crate::ErrorKind::OutOfRange, message.into())
}

/// `IntervalTryAddition`: `target += input * multiplier`, then `round(fraction * multiplier)`.
fn try_add<T>(target: &mut T, input: i64, multiplier: i64, fraction: f64) -> Result<(), IntervalParseError>
where
    T: TryFrom<i64> + Copy + std::ops::Add<Output = T> + num_like::CheckedAdd,
{
    let addition = input
        .checked_mul(multiplier)
        .ok_or_else(|| out_of_range("interval value is out of range"))?;
    let cast = |v: i64| {
        T::try_from(v).map_err(|_| {
            IntervalParseError::Error(
                crate::ErrorKind::InvalidInput,
                format!("Type INT64 with value {v} can't be cast because the value is out of range for the destination type {}", T::NAME),
            )
        })
    };
    *target = target
        .checked_add(cast(addition)?)
        .ok_or_else(|| out_of_range("interval value is out of range"))?;
    if fraction.abs() > 1e-10 {
        let extra = (fraction * multiplier as f64).round() as i64;
        *target = target
            .checked_add(cast(extra)?)
            .ok_or_else(|| out_of_range("interval fraction is out of range"))?;
    }
    Ok(())
}

mod num_like {
    pub trait CheckedAdd: Sized {
        const NAME: &'static str;
        fn checked_add(self, other: Self) -> Option<Self>;
    }
    impl CheckedAdd for i32 {
        const NAME: &'static str = "INT32";
        fn checked_add(self, other: Self) -> Option<Self> {
            i32::checked_add(self, other)
        }
    }
    impl CheckedAdd for i64 {
        const NAME: &'static str = "INT64";
        fn checked_add(self, other: Self) -> Option<Self> {
            i64::checked_add(self, other)
        }
    }
}

impl Interval {
    pub const MONTHS_PER_YEAR: i64 = 12;

    /// Parses an interval literal (`Interval::FromCString`): `[@]` followed by
    /// `<number[.fraction]> <unit>` pairs, an optional `HH:MM[:SS[.ffffff]]` time, a bare
    /// number of seconds, and an optional trailing `ago` that negates everything.
    pub fn parse(s: &str) -> Result<Interval, IntervalParseError> {
        use super::DatePart;
        use super::time::{MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SEC};
        let buf = s.as_bytes();
        let len = buf.len();
        let mut result = Interval::default();
        if len == 0 {
            return Err(IntervalParseError::Invalid);
        }
        let mut pos = 0;
        match buf[0] {
            b'@' => pos = 1,
            b'P' | b'p' => return Err(IntervalParseError::Invalid),
            _ => {}
        }
        let is_ws = |c: u8| c == b' ' || c == b'\t' || c == b'\n';
        let mut found_any = false;
        loop {
            // standard_interval: skip spaces, then a number, "ago", or the end.
            while pos < len && is_ws(buf[pos]) {
                pos += 1;
            }
            if pos == len {
                break;
            }
            let negative = match buf[pos] {
                b'0'..=b'9' => false,
                b'-' => {
                    pos += 1;
                    true
                }
                b'a' | b'A' => {
                    // "ago" negates everything parsed so far.
                    if len - pos < 3
                        || !buf[pos + 1].eq_ignore_ascii_case(&b'g')
                        || !buf[pos + 2].eq_ignore_ascii_case(&b'o')
                    {
                        return Err(IntervalParseError::Invalid);
                    }
                    if buf[pos + 3..].iter().any(|&c| !is_ws(c)) {
                        return Err(IntervalParseError::Invalid);
                    }
                    if result.months == i32::MIN || result.days == i32::MIN || result.micros == i64::MIN {
                        return Err(out_of_range("AGO interval value is out of range"));
                    }
                    result = Interval::new(-result.months, -result.days, -result.micros);
                    break;
                }
                _ => return Err(IntervalParseError::Invalid),
            };
            // interval_parse_number
            let start = pos;
            while pos < len && buf[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos < len && buf[pos] == b':' {
                // A time: parse the rest of the string as HH:MM[:SS[.ffffff]].
                let (time, _) = super::Time::try_convert_interval(&buf[start..], false, None, negative)
                    .ok_or(IntervalParseError::Invalid)?;
                try_add(&mut result.micros, time.0, 1, 0.0)?;
                found_any = true;
                break;
            }
            if pos == start {
                return Err(IntervalParseError::Invalid);
            }
            let digits = &s[start..pos];
            let mut number: i64 = digits.parse().map_err(|_| {
                IntervalParseError::Error(
                    crate::ErrorKind::InvalidInput,
                    format!("Could not convert string '{digits}' to INT64"),
                )
            })?;
            let mut fraction = 0.0;
            if pos < len && buf[pos] == b'.' {
                pos += 1;
                let frac_start = pos;
                while pos < len && buf[pos].is_ascii_digit() {
                    pos += 1;
                }
                if pos > frac_start {
                    fraction = s[frac_start - 1..pos].parse().unwrap_or(0.0);
                }
            }
            if negative {
                number = -number;
                fraction = -fraction;
            }
            // interval_parse_identifier
            while pos < len && is_ws(buf[pos]) {
                pos += 1;
            }
            let id_start = pos;
            while pos < len && buf[pos].is_ascii_alphabetic() {
                pos += 1;
            }
            let unit = &s[id_start..pos];
            if unit.is_empty() && !found_any {
                // A bare number is seconds.
                try_add(&mut result.micros, number, MICROS_PER_SEC, 0.0)?;
                try_add(&mut result.micros, 0, MICROS_PER_SEC, fraction)?;
                if buf[pos..].iter().any(|&c| !is_ws(c)) {
                    return Err(IntervalParseError::Invalid);
                }
                found_any = true;
                break;
            }
            let Some(part) = DatePart::parse(unit) else {
                return Err(IntervalParseError::Error(
                    crate::ErrorKind::Conversion,
                    format!("extract specifier \"{unit}\" not recognized"),
                ));
            };
            let dpm = Self::DAYS_PER_MONTH;
            match part {
                DatePart::Millennium => try_add(&mut result.months, number, 12_000, fraction)?,
                DatePart::Century => try_add(&mut result.months, number, 1200, fraction)?,
                DatePart::Decade => try_add(&mut result.months, number, 120, fraction)?,
                DatePart::Year => try_add(&mut result.months, number, 12, fraction)?,
                DatePart::Quarter => {
                    try_add(&mut result.months, number, 3, 0.0)?;
                    let month = (fraction * 3.0) as i32;
                    try_add(&mut result.months, i64::from(month), 1, 0.0)?;
                    try_add(&mut result.days, 0, dpm, fraction * 3.0 - f64::from(month))?;
                }
                DatePart::Month => {
                    try_add(&mut result.months, number, 1, 0.0)?;
                    let day = (fraction * dpm as f64) as i32;
                    try_add(&mut result.days, i64::from(day), 1, 0.0)?;
                    try_add(
                        &mut result.micros,
                        0,
                        MICROS_PER_DAY,
                        fraction * dpm as f64 - f64::from(day),
                    )?;
                }
                DatePart::Day => {
                    try_add(&mut result.days, number, 1, 0.0)?;
                    try_add(&mut result.micros, 0, MICROS_PER_DAY, fraction)?;
                }
                DatePart::Week => {
                    try_add(&mut result.days, number, 7, 0.0)?;
                    let day = (fraction * 7.0) as i32;
                    try_add(&mut result.days, i64::from(day), 1, 0.0)?;
                    try_add(&mut result.micros, 0, MICROS_PER_DAY, fraction * 7.0 - f64::from(day))?;
                }
                DatePart::Microseconds => try_add(&mut result.micros, number, 1, 0.0)?,
                DatePart::Milliseconds => try_add(&mut result.micros, number, 1000, fraction)?,
                DatePart::Second => try_add(&mut result.micros, number, MICROS_PER_SEC, fraction)?,
                DatePart::Minute => try_add(&mut result.micros, number, MICROS_PER_MINUTE, fraction)?,
                DatePart::Hour => try_add(&mut result.micros, number, MICROS_PER_HOUR, fraction)?,
                _ => {
                    return Err(IntervalParseError::Error(
                        crate::ErrorKind::Conversion,
                        format!("extract specifier \"{unit}\" not supported for interval"),
                    ));
                }
            }
            found_any = true;
        }
        if found_any {
            Ok(result)
        } else {
            Err(IntervalParseError::Invalid)
        }
    }
}

impl std::fmt::Display for Interval {
    /// `IntervalToStringCast::Format`: `1 year 2 months 3 days 04:05:06.789`, or `00:00:00`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        let mut out = String::with_capacity(32);
        let part = |out: &mut String, value: i64, name: &str| {
            if value == 0 {
                return;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{value} {name}");
            if value != 1 && value != -1 {
                out.push('s');
            }
        };
        if self.months != 0 {
            let years = self.months / 12;
            part(&mut out, i64::from(years), "year");
            part(&mut out, i64::from(self.months - years * 12), "month");
        }
        part(&mut out, i64::from(self.days), "day");
        if self.micros != 0 {
            if !out.is_empty() {
                out.push(' ');
            }
            if self.micros < 0 {
                out.push('-');
            }
            // unsigned_abs avoids overflow for i64::MIN.
            let micros = self.micros.unsigned_abs();
            let hours = micros / 3_600_000_000;
            let minutes = micros / 60_000_000 % 60;
            let seconds = micros / 1_000_000 % 60;
            let fraction = micros % 1_000_000;
            let _ = write!(out, "{hours:02}:{minutes:02}:{seconds:02}");
            if fraction != 0 {
                let digits = format!("{fraction:06}");
                let _ = write!(out, ".{}", digits.trim_end_matches('0'));
            }
        } else if out.is_empty() {
            out.push_str("00:00:00");
        }
        f.write_str(&out)
    }
}

impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.equals(*other)
    }
}

impl Eq for Interval {}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.compare(*other)
    }
}

impl std::hash::Hash for Interval {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.normalize().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_matches_duckdb() {
        let hms = |h: i64, m: i64, s: i64, us: i64| ((h * 60 + m) * 60 + s) * 1_000_000 + us;
        assert_eq!(
            Interval::new(14, 3, hms(4, 5, 6, 789_000)).to_string(),
            "1 year 2 months 3 days 04:05:06.789"
        );
        assert_eq!(Interval::new(-1, 0, 0).to_string(), "-1 month");
        assert_eq!(Interval::new(0, 0, 0).to_string(), "00:00:00");
        assert_eq!(Interval::new(0, -1, hms(1, 0, 0, 0)).to_string(), "-1 day 01:00:00");
        assert_eq!(Interval::new(0, 0, -hms(1, 30, 0, 0)).to_string(), "-01:30:00");
        assert_eq!(Interval::new(0, 0, 1).to_string(), "00:00:00.000001");
        assert_eq!(Interval::new(13, 0, 0).to_string(), "1 year 1 month");
        assert_eq!(Interval::new(0, 0, hms(200, 0, 0, 0)).to_string(), "200:00:00");
        assert_eq!(Interval::new(-14, 0, 0).to_string(), "-1 year -2 months");
    }

    /// Expected values come from `SELECT INTERVAL '...'` in DuckDB.
    #[test]
    fn parsing_matches_duckdb() {
        let p = |s: &str| Interval::parse(s).map(|i| i.to_string());
        let cases = [
            (
                "1 year 2 months 3 days 04:05:06.789",
                "1 year 2 months 3 days 04:05:06.789",
            ),
            ("2 hours 30 minutes", "02:30:00"),
            ("1.5 years", "1 year 6 months"),
            ("1.5 months", "1 month 15 days"),
            ("1.25 days", "1 day 06:00:00"),
            ("1.5 weeks", "10 days 12:00:00"),
            ("1.5 quarters", "4 months 15 days"),
            ("3 days ago", "-3 days"),
            ("@ 1 day 2 hours", "1 day 02:00:00"),
            ("-1 day 1 hour", "-1 day 01:00:00"),
            ("90", "00:01:30"),
            ("1.5", "00:00:01.5"),
            ("-01:30", "-01:30:00"),
            ("10:00:00", "10:00:00"),
            ("1 century", "100 years"),
            ("500 ms", "00:00:00.5"),
            ("7 us", "00:00:00.000007"),
            ("2 YEARS 1 Mon", "2 years 1 month"),
        ];
        for (input, expected) in cases {
            assert_eq!(p(input).unwrap(), expected, "{input}");
        }
        assert_eq!(Interval::parse(""), Err(IntervalParseError::Invalid));
        assert_eq!(Interval::parse("hello"), Err(IntervalParseError::Invalid));
        assert_eq!(Interval::parse("P1D"), Err(IntervalParseError::Invalid));
        assert_eq!(Interval::parse("3 days agox"), Err(IntervalParseError::Invalid));
        assert!(matches!(
            Interval::parse("1 fortnight"),
            Err(IntervalParseError::Error(_, m)) if m == "extract specifier \"fortnight\" not recognized"
        ));
        assert!(matches!(
            Interval::parse("1 dow"),
            Err(IntervalParseError::Error(_, m)) if m == "extract specifier \"dow\" not supported for interval"
        ));
        assert!(matches!(
            Interval::parse("3000000000 days"),
            Err(IntervalParseError::Error(crate::ErrorKind::InvalidInput, _))
        ));
    }

    #[test]
    fn normalized_equality() {
        let month = Interval::new(1, 0, 0);
        assert!(month.equals(Interval::new(0, 30, 0)));
        assert_eq!(month, Interval::new(0, 30, 0));
        assert!(month.equals(Interval::new(0, 0, Interval::MICROS_PER_MONTH)));
        assert!(!month.equals(Interval::new(0, 29, 0)));
        assert_eq!(Interval::new(0, 31, 0).normalize(), (1, 1, 0));
        // 1 month - 4 days borrows: it is 26 days, not "1 month, -4 days".
        assert_eq!(Interval::new(1, -4, 0).normalize(), (0, 26, 0));
        assert_eq!(
            Interval::new(1, -4, 0).compare(Interval::new(0, 27, 0)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            Interval::new(0, 0, -1).normalize(),
            (-1, 29, Interval::MICROS_PER_DAY - 1)
        );
    }
}
