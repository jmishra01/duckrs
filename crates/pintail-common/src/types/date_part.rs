// Ported from duckdb/src/common/enums/date_part_specifier.cpp @ rust-port-baseline

/// A date/time component, as named in `date_part`, `extract` and interval literals
/// (`DatePartSpecifier`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatePart {
    Year,
    Month,
    Day,
    Decade,
    Century,
    Millennium,
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
    Epoch,
    Dow,
    IsoDow,
    Week,
    Doy,
    Quarter,
    YearWeek,
    IsoYear,
    Era,
    Timezone,
    TimezoneHour,
    TimezoneMinute,
    JulianDay,
}

impl DatePart {
    /// Parses a component name case-insensitively (`TryGetDatePartSpecifier`).
    pub fn parse(name: &str) -> Option<DatePart> {
        use DatePart::*;
        Some(match name.to_ascii_lowercase().as_str() {
            "year" | "yr" | "y" | "years" | "yrs" => Year,
            "month" | "mon" | "months" | "mons" => Month,
            "day" | "days" | "d" | "dayofmonth" => Day,
            "decade" | "dec" | "decades" | "decs" => Decade,
            "century" | "cent" | "centuries" | "c" => Century,
            "millennium" | "mil" | "millenniums" | "millennia" | "mils" | "millenium" => Millennium,
            "microseconds" | "microsecond" | "us" | "usec" | "usecs" | "usecond" | "useconds" => Microseconds,
            "milliseconds" | "millisecond" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => Milliseconds,
            "second" | "sec" | "seconds" | "secs" | "s" => Second,
            "minute" | "min" | "minutes" | "mins" | "m" => Minute,
            "hour" | "hr" | "hours" | "hrs" | "h" => Hour,
            "epoch" => Epoch,
            "dow" | "dayofweek" | "weekday" => Dow,
            "isodow" => IsoDow,
            "week" | "weeks" | "w" | "weekofyear" => Week,
            "doy" | "dayofyear" => Doy,
            "quarter" | "quarters" => Quarter,
            "yearweek" => YearWeek,
            "isoyear" => IsoYear,
            "era" => Era,
            "timezone" => Timezone,
            "timezone_hour" => TimezoneHour,
            "timezone_minute" => TimezoneMinute,
            "julian" | "jd" => JulianDay,
            _ => return None,
        })
    }
}
