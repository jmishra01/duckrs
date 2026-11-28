// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/date.cpp @ rust-port-baseline

//! Dates as dsdgen keeps them: a calendar date and its Julian day number.

const M_DAYS: [[i32; 13]; 2] = [
    [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334],
    [0, 0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335],
];

pub(crate) const WEEKDAY_NAMES: [&[u8]; 8] = [
    b"",
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];

pub(crate) const OP_FIRST_DOM: i32 = 0x01;
pub(crate) const OP_LAST_DOM: i32 = 0x02;
pub(crate) const OP_SAME_LY: i32 = 0x03;
pub(crate) const OP_SAME_LQ: i32 = 0x04;

/// `date_t`
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct DateT {
    pub(crate) year: i32,
    pub(crate) month: i32,
    pub(crate) day: i32,
    pub(crate) julian: i32,
}

/// `jtodt`: the date of Julian day `src`.
pub(crate) fn jtodt(src: i32) -> DateT {
    let src64 = i64::from(src);
    let mut l = src64 + 68569;
    let n = (4 * l) / 146097;
    l -= (146097 * n + 3) / 4;
    let i = (4000 * (l + 1)) / 1461001;
    l = l - (1461 * i) / 4 + 31;
    let j = (80 * l) / 2447;
    let day = l - (2447 * j) / 80;
    l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    DateT {
        year: year as i32,
        month: month as i32,
        day: day as i32,
        julian: src,
    }
}

/// `dttoj`: the Julian day of a date's year, month and day.
pub(crate) fn dttoj(dt: &DateT) -> i32 {
    let mut y = dt.year;
    let mut m = dt.month;
    if m <= 2 {
        m += 12;
        y -= 1;
    }
    dt.day + (153 * m - 457) / 5 + 365 * y + y / 4 - y / 100 + y / 400 + 1721118 + 1
}

/// A date from its parts, with its Julian day, as `strtodt` makes it.
pub(crate) fn mk_date(year: i32, month: i32, day: i32) -> DateT {
    let mut d = DateT {
        year,
        month,
        day,
        julian: 0,
    };
    d.julian = dttoj(&d);
    d
}

/// `strtodt`: a `yyyy-m-d` date.
pub(crate) fn strtodt(s: &str) -> DateT {
    let mut parts = s
        .split('-')
        .map(|p| p.parse::<i32>().expect("dsdgen: Invalid string to date conversion"));
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        panic!("dsdgen: Invalid string to date conversion in strtodt");
    };
    mk_date(year, month, day)
}

/// `is_leap`
pub(crate) fn is_leap(year: i32) -> i32 {
    if year % 100 == 0 {
        i32::from((year % 400) % 2 == 0)
    } else {
        i32::from(year % 4 == 0)
    }
}

/// `day_number`: the day of the year.
pub(crate) fn day_number(d: &DateT) -> i32 {
    M_DAYS[is_leap(d.year) as usize][d.month as usize] + d.day
}

/// `set_dow`: the day of the week, 0 for Sunday.
pub(crate) fn set_dow(d: &DateT) -> i32 {
    const DOOMSDAY: [i32; 4] = [3, 2, 0, 5];
    let mut known = [0, 3, 0, 0, 4, 9, 6, 11, 8, 5, 10, 7, 12];
    if is_leap(d.year) != 0 {
        known[1] = 4;
        known[2] = 1;
    } else {
        known[1] = 3;
        known[2] = 0;
    }
    let mut dday = d.year / 100;
    dday -= 15;
    dday %= 4;
    dday = DOOMSDAY[dday as usize];
    let mut q = d.year % 100;
    let r = q % 12;
    q /= 12;
    let s = r / 4;
    dday += q + r + s;
    dday %= 7;

    let mut res = d.day;
    res -= known[d.month as usize];
    while res < 0 {
        res += 7;
    }
    while res > 6 {
        res -= 7;
    }
    res += dday;
    res %= 7;
    res
}

/// `date_t_op`
pub(crate) fn date_t_op(op: i32, d1: &DateT) -> DateT {
    match op {
        OP_FIRST_DOM => jtodt(d1.julian - d1.day + 1),
        OP_LAST_DOM => jtodt(d1.julian - d1.day + M_DAYS[is_leap(d1.year) as usize][d1.month as usize]),
        OP_SAME_LY => {
            if is_leap(d1.year) != 0 && d1.month == 2 && d1.day == 29 {
                mk_date(d1.year - 1, 2, 28)
            } else {
                mk_date(d1.year - 1, d1.month, d1.day)
            }
        }
        OP_SAME_LQ => {
            // the quarter starts: 01-01, 04-01, 07-01, 10-01
            let (this_start, last_start) = match d1.month {
                1..=3 => (mk_date(d1.year, 1, 1), mk_date(d1.year - 1, 10, 1)),
                4..=6 => (mk_date(d1.year, 4, 1), mk_date(d1.year, 1, 1)),
                7..=9 => (mk_date(d1.year, 7, 1), mk_date(d1.year, 4, 1)),
                _ => (mk_date(d1.year, 10, 1), mk_date(d1.year, 7, 1)),
            };
            jtodt(d1.julian - this_start.julian + last_start.julian)
        }
        _ => DateT::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn julian_round_trip() {
        let d = strtodt("1970-01-01");
        assert_eq!(d.julian, 2440588);
        assert_eq!(jtodt(2440588), d);
        assert_eq!(set_dow(&strtodt("1776-07-04")), 4);
        assert_eq!(set_dow(&strtodt("2000-01-01")), 6);
    }
}
