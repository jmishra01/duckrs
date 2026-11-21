//! `StrpTimeFormat`: parsing dates and times by a strftime format.

use duckrs_common::types::Date;

const MICROS_PER_DAY: i64 = 86_400_000_000;
const NANOS_PER_DAY: i64 = MICROS_PER_DAY * 1000;

/// `StrTimeSpecifier`, of the specifiers strptime knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Spec {
    AbbreviatedWeekdayName,
    FullWeekdayName,
    WeekdayDecimal,
    WeekdayIso,
    DayOfMonthPadded,
    DayOfMonth,
    AbbreviatedMonthName,
    FullMonthName,
    MonthDecimalPadded,
    MonthDecimal,
    YearWithoutCenturyPadded,
    YearWithoutCentury,
    YearDecimal,
    YearIso,
    Hour24Padded,
    Hour24Decimal,
    Hour12Padded,
    Hour12Decimal,
    AmPm,
    MinutePadded,
    MinuteDecimal,
    SecondPadded,
    SecondDecimal,
    NanosecondPadded,
    MicrosecondPadded,
    MillisecondPadded,
    UtcOffset,
    TzName,
    DayOfYearPadded,
    DayOfYearDecimal,
    WeekNumberPaddedSunFirst,
    WeekNumberPaddedMonFirst,
    WeekNumberIso,
}

impl Spec {
    /// `StrpTimeFormat::NumericSpecifierWidth`
    fn numeric_width(self) -> usize {
        use Spec::*;
        match self {
            WeekdayDecimal | WeekdayIso => 1,
            DayOfMonthPadded | DayOfMonth | MonthDecimalPadded | MonthDecimal | YearWithoutCenturyPadded
            | YearWithoutCentury | Hour24Padded | Hour24Decimal | Hour12Padded | Hour12Decimal | MinutePadded
            | MinuteDecimal | SecondPadded | SecondDecimal | WeekNumberPaddedSunFirst | WeekNumberPaddedMonFirst
            | WeekNumberIso => 2,
            MillisecondPadded | DayOfYearPadded | DayOfYearDecimal => 3,
            YearDecimal | YearIso => 4,
            MicrosecondPadded => 6,
            NanosecondPadded => 9,
            _ => 0,
        }
    }
}

/// A parsed strptime format: a literal before each specifier, and one after the last.
#[derive(Clone, Debug)]
pub(crate) struct StrpFormat {
    pub text: String,
    literals: Vec<Vec<u8>>,
    specifiers: Vec<Spec>,
}

/// `StrTimeFormat::ParseFormatSpecifier`, of strptime; the error is without its prefix.
fn parse_specifiers(format: &[u8]) -> Result<(Vec<Vec<u8>>, Vec<Spec>), String> {
    use Spec::*;
    if format.is_empty() {
        return Err("Empty format string".into());
    }
    let mut literals = Vec::new();
    let mut specifiers = Vec::new();
    let mut pos = 0;
    let mut current: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < format.len() {
        if format[i] != b'%' {
            i += 1;
            continue;
        }
        if i + 1 == format.len() {
            return Err("Trailing format character %".into());
        }
        if i > pos {
            current.extend_from_slice(&format[pos..i]);
        }
        i += 1;
        let mut c = format[i];
        if c == b'%' {
            pos = i;
            i += 1;
            continue;
        }
        let unrecognized = |prefix: &str, c: u8| {
            format!(
                "Unrecognized format for strftime/strptime: %{prefix}{}",
                char::from(c)
            )
        };
        let spec = if c == b'-' && i + 1 < format.len() {
            i += 1;
            c = format[i];
            match c {
                b'd' => DayOfMonth,
                b'm' => MonthDecimal,
                b'y' => YearWithoutCentury,
                b'H' => Hour24Decimal,
                b'I' => Hour12Decimal,
                b'M' => MinuteDecimal,
                b'S' => SecondDecimal,
                b'j' => DayOfYearDecimal,
                _ => return Err(unrecognized("-", c)),
            }
        } else {
            match c {
                b'a' => AbbreviatedWeekdayName,
                b'A' => FullWeekdayName,
                b'w' => WeekdayDecimal,
                b'u' => WeekdayIso,
                b'd' => DayOfMonthPadded,
                b'h' | b'b' => AbbreviatedMonthName,
                b'B' => FullMonthName,
                b'm' => MonthDecimalPadded,
                b'y' => YearWithoutCenturyPadded,
                b'Y' => YearDecimal,
                b'G' => YearIso,
                b'H' => Hour24Padded,
                b'I' => Hour12Padded,
                b'p' => AmPm,
                b'M' => MinutePadded,
                b'S' => SecondPadded,
                b'n' => NanosecondPadded,
                b'f' => MicrosecondPadded,
                b'g' => MillisecondPadded,
                b'z' => UtcOffset,
                b'Z' => TzName,
                b'j' => DayOfYearPadded,
                b'U' => WeekNumberPaddedSunFirst,
                b'W' => WeekNumberPaddedMonFirst,
                b'V' => WeekNumberIso,
                b'c' | b'x' | b'X' | b'T' => {
                    let sub: &[u8] = match c {
                        b'c' => b"%Y-%m-%d %H:%M:%S",
                        b'x' => b"%Y-%m-%d",
                        _ => b"%H:%M:%S",
                    };
                    let (mut sub_literals, sub_specifiers) = parse_specifiers(sub)?;
                    let mut first = std::mem::take(&mut current);
                    first.extend_from_slice(&sub_literals[0]);
                    sub_literals[0] = first;
                    for (literal, spec) in sub_literals.into_iter().zip(sub_specifiers) {
                        literals.push(literal);
                        specifiers.push(spec);
                    }
                    i += 1;
                    pos = i;
                    continue;
                }
                _ => return Err(unrecognized("", c)),
            }
        };
        literals.push(std::mem::take(&mut current));
        specifiers.push(spec);
        i += 1;
        pos = i;
    }
    if pos < format.len() {
        current.extend_from_slice(&format[pos..]);
    }
    literals.push(current);
    Ok((literals, specifiers))
}

/// What `StrpTimeFormat::Parse` makes of a string: year, month, day, hour, minute, second,
/// nanoseconds and the UTC offset in seconds, or a special date.
#[derive(Clone, Debug, Default)]
pub(crate) struct Parsed {
    data: [i32; 8],
    special: Option<i32>,
}

/// `StrpTimeFormat::ParseResult`'s error: the message and where in the (trimmed) input.
#[derive(Clone, Debug, Default)]
pub(crate) struct ParseError {
    pub message: String,
    pub position: Option<usize>,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `Date::TryConvertDateSpecial`
fn date_special(data: &[u8], pos: &mut usize, special: &[u8], abbr: usize) -> bool {
    let mut p = *pos;
    let mut s = 0;
    while p < data.len() && s < special.len() {
        if data[p].to_ascii_lowercase() != special[s] {
            return false;
        }
        p += 1;
        s += 1;
    }
    if s < special.len() && p - *pos != abbr {
        return false;
    }
    *pos = p;
    true
}

/// `Timestamp::TryParseUTCOffset` (strict)
fn utc_offset(data: &[u8], pos: &mut usize) -> Option<(i32, i32, i32)> {
    let len = data.len();
    let digit = |at: usize| data.get(at).is_some_and(u8::is_ascii_digit);
    let mut cur = *pos;
    if cur + 3 > len {
        return None;
    }
    let sign = data[cur];
    if sign != b'+' && sign != b'-' {
        return None;
    }
    cur += 1;
    if !digit(cur) {
        return None;
    }
    let mut hh = i32::from(data[cur] - b'0');
    cur += 1;
    if !digit(cur) {
        return None;
    }
    hh = hh * 10 + i32::from(data[cur] - b'0');
    cur += 1;
    let negate = |v: i32| if sign == b'-' { -v } else { v };
    let (mut mm, mut ss) = (0, 0);
    if cur >= len {
        *pos = cur;
        return Some((negate(hh), mm, ss));
    }
    let colons = data[cur] == b':';
    if colons {
        cur += 1;
    }
    if cur + 2 > len || !digit(cur) {
        *pos = cur;
        return (!colons).then_some((negate(hh), mm, ss));
    }
    mm = i32::from(data[cur] - b'0');
    cur += 1;
    if !digit(cur) {
        return None;
    }
    mm = mm * 10 + i32::from(data[cur] - b'0');
    cur += 1;
    if cur >= len || !colons || data[cur] != b':' {
        *pos = cur;
        return Some((negate(hh), negate(mm), ss));
    }
    cur += 1;
    if cur + 2 > len || !digit(cur) {
        *pos = cur;
        return None;
    }
    ss = i32::from(data[cur] - b'0');
    cur += 1;
    if !digit(cur) {
        return None;
    }
    ss = ss * 10 + i32::from(data[cur] - b'0');
    cur += 1;
    *pos = cur;
    Some((negate(hh), negate(mm), negate(ss)))
}

const DAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
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

/// `StrpTimeFormat::TryParseCollection`
fn collection(data: &[u8], pos: &mut usize, names: &[&str], abbreviated: bool) -> Option<usize> {
    for (index, name) in names.iter().enumerate() {
        let entry = if abbreviated { &name.as_bytes()[..3] } else { name.as_bytes() };
        if *pos + entry.len() > data.len() {
            continue;
        }
        if entry
            .iter()
            .zip(&data[*pos..])
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            *pos += entry.len();
            return Some(index);
        }
    }
    None
}

/// `Date::GetMondayOfCurrentWeek`
fn monday_of(days: i32) -> i32 {
    let isodow = (i64::from(days) + 3).rem_euclid(7) as i32 + 1;
    days - (isodow - 1)
}

fn ymd(days: i32) -> (i32, i32, i32) {
    Date(days).to_ymd()
}

impl StrpFormat {
    pub fn new(format: &str) -> Result<StrpFormat, String> {
        let (literals, specifiers) = parse_specifiers(format.as_bytes())?;
        Ok(StrpFormat {
            text: format.to_string(),
            literals,
            specifiers,
        })
    }

    pub fn has(&self, spec: Spec) -> bool {
        self.specifiers.contains(&spec)
    }

    /// `StrpTimeFormat::Parse`; the error position counts from the first non-space character.
    pub fn parse(&self, input: &str, strict: bool, error: &mut ParseError) -> Option<Parsed> {
        use Spec::*;
        let mut data = input.as_bytes();
        while let Some((&first, rest)) = data.split_first() {
            if !is_space(first) {
                break;
            }
            data = rest;
        }
        let size = data.len();
        let mut fail = |message: String, position: Option<usize>| {
            error.message = message;
            if position.is_some() {
                error.position = position;
            }
        };
        let mut result = Parsed {
            data: [1900, 1, 1, 0, 0, 0, 0, 0],
            special: None,
        };
        let mut pos = 0;
        if size > 4 {
            if data[0].is_ascii_alphabetic() {
                if date_special(data, &mut pos, b"infinity", 3) {
                    result.special = Some(Date::INFINITY.0);
                } else if date_special(data, &mut pos, b"epoch", 5) {
                    result.special = Some(0);
                }
            } else if data[0] == b'-' && date_special(data, &mut pos, b"-infinity", 4) {
                result.special = Some(Date::NINFINITY.0);
            }
        }
        if result.special.is_some() {
            while pos < size && is_space(data[pos]) {
                pos += 1;
            }
            if pos != size {
                fail("Special timestamp did not match: trailing characters".into(), Some(pos));
                return None;
            }
            return Some(result);
        }
        let r = &mut result.data;
        // 0: none, 1: AM, 2: PM
        let mut ampm = 0;
        let mut offset_specifier = WeekdayDecimal;
        let (mut weekno, mut weekday, mut yearday) = (0u64, 0u64, 0u64);
        let mut has_weekday = false;
        let (mut iso_year, mut iso_week, mut iso_weekday) = (10000u64, 54u64, 8u64);
        let mut i = 0;
        loop {
            let literal = &self.literals[i];
            let mut l = 0;
            while l < literal.len() {
                if is_space(literal[l]) {
                    if pos >= size || !is_space(data[pos]) {
                        fail(
                            format!("Space does not match, expected {}", String::from_utf8_lossy(literal)),
                            Some(pos),
                        );
                        return None;
                    }
                    pos += 1;
                    while pos < size && is_space(data[pos]) {
                        pos += 1;
                    }
                    l += 1;
                    while l < literal.len() && is_space(literal[l]) {
                        l += 1;
                    }
                    continue;
                }
                if pos >= size || data[pos] != literal[l] {
                    fail(
                        format!("Literal does not match, expected {}", String::from_utf8_lossy(literal)),
                        Some(pos),
                    );
                    return None;
                }
                pos += 1;
                l += 1;
            }
            if i == self.specifiers.len() {
                break;
            }
            let spec = self.specifiers[i];
            let width = spec.numeric_width();
            if width > 0 {
                let mut number: u64 = 0;
                let mut digits = 0;
                let start = pos;
                let end = start + width;
                while pos < size && pos < end && data[pos].is_ascii_digit() {
                    number = number * 10 + u64::from(data[pos] - b'0');
                    pos += 1;
                    digits += 1;
                }
                if pos == start {
                    fail("Expected a number".into(), Some(start));
                    return None;
                }
                let at = Some(start);
                match spec {
                    DayOfMonthPadded | DayOfMonth => {
                        if !(1..=31).contains(&number) {
                            fail("Day out of range, expected a value between 1 and 31".into(), at);
                            return None;
                        }
                        r[2] = number as i32;
                        offset_specifier = spec;
                    }
                    MonthDecimalPadded | MonthDecimal => {
                        if !(1..=12).contains(&number) {
                            fail("Month out of range, expected a value between 1 and 12".into(), at);
                            return None;
                        }
                        r[1] = number as i32;
                        offset_specifier = spec;
                    }
                    YearWithoutCenturyPadded | YearWithoutCentury => {
                        if matches!(offset_specifier, YearIso | WeekNumberIso | WeekdayDecimal) {
                            offset_specifier = spec;
                        }
                        if pos - start < 2 && strict {
                            return None;
                        }
                        if number >= 100 {
                            fail(
                                "Year without century out of range, expected a value between 0 and 99".into(),
                                at,
                            );
                            return None;
                        }
                        r[0] = if number >= 69 { 1900 + number as i32 } else { 2000 + number as i32 };
                    }
                    YearDecimal => {
                        if matches!(offset_specifier, YearIso | WeekNumberIso | WeekdayDecimal) {
                            offset_specifier = spec;
                        }
                        if pos - start < 2 && strict {
                            return None;
                        }
                        r[0] = number as i32;
                    }
                    YearIso => {
                        match offset_specifier {
                            DayOfMonthPadded | DayOfMonth | MonthDecimalPadded | MonthDecimal
                            | YearWithoutCenturyPadded | YearWithoutCentury | YearDecimal => {}
                            WeekdayDecimal => offset_specifier = spec,
                            YearIso | WeekNumberIso => {
                                if iso_year <= 9999 {
                                    fail("Multiple ISO year offsets specified".into(), at);
                                    return None;
                                }
                            }
                            _ => {
                                fail("Incompatible ISO year offset specified".into(), at);
                                return None;
                            }
                        }
                        if number > 9999 {
                            fail(
                                "ISO Year out of range, expected a value between 0000 and 9999".into(),
                                at,
                            );
                            return None;
                        }
                        iso_year = number;
                    }
                    Hour24Padded | Hour24Decimal => {
                        if number >= 24 {
                            fail("Hour out of range, expected a value between 0 and 23".into(), at);
                            return None;
                        }
                        r[3] = number as i32;
                    }
                    Hour12Padded | Hour12Decimal => {
                        if !(1..=12).contains(&number) {
                            fail("Hour12 out of range, expected a value between 1 and 12".into(), at);
                            return None;
                        }
                        r[3] = number as i32;
                    }
                    MinutePadded | MinuteDecimal => {
                        if number >= 60 {
                            fail("Minutes out of range, expected a value between 0 and 59".into(), at);
                            return None;
                        }
                        r[4] = number as i32;
                    }
                    SecondPadded | SecondDecimal => {
                        if number >= 60 {
                            fail("Seconds out of range, expected a value between 0 and 59".into(), at);
                            return None;
                        }
                        r[5] = number as i32;
                    }
                    NanosecondPadded | MicrosecondPadded | MillisecondPadded => {
                        while digits < width {
                            number *= 10;
                            digits += 1;
                        }
                        r[6] = match spec {
                            NanosecondPadded => number as i32,
                            MicrosecondPadded => (number * 1000) as i32,
                            _ => (number * 1_000_000) as i32,
                        };
                    }
                    WeekNumberPaddedSunFirst | WeekNumberPaddedMonFirst => {
                        match offset_specifier {
                            DayOfMonthPadded | DayOfMonth | MonthDecimalPadded | MonthDecimal => {}
                            YearWithoutCenturyPadded | YearWithoutCentury | YearDecimal | WeekdayDecimal => {
                                offset_specifier = spec;
                            }
                            _ => {
                                fail("Multiple week offsets specified".into(), at);
                                return None;
                            }
                        }
                        if number > 53 {
                            fail("Week out of range, expected a value between 0 and 53".into(), at);
                            return None;
                        }
                        weekno = number;
                    }
                    WeekdayDecimal => {
                        if number > 6 {
                            fail("Weekday out of range, expected a value between 0 and 6".into(), at);
                            return None;
                        }
                        has_weekday = true;
                        weekday = number;
                    }
                    WeekNumberIso => {
                        match offset_specifier {
                            YearWithoutCenturyPadded | YearWithoutCentury | YearDecimal => {
                                fail(
                                    "ISO week offsets are incompatible with non-ISO year specifiers. Use '%G' instead"
                                        .into(),
                                    at,
                                );
                                return None;
                            }
                            DayOfMonthPadded | DayOfMonth | MonthDecimalPadded | MonthDecimal => {}
                            WeekdayDecimal => offset_specifier = spec,
                            WeekNumberIso | YearIso => {
                                if iso_week <= 53 {
                                    fail("Multiple ISO week offsets specified".into(), at);
                                    return None;
                                }
                            }
                            _ => {
                                fail("Incompatible ISO week offset specified".into(), at);
                                return None;
                            }
                        }
                        if !(1..=53).contains(&number) {
                            fail(
                                "ISO week offset out of range, expected a value between 1 and 53".into(),
                                at,
                            );
                            return None;
                        }
                        iso_week = number;
                    }
                    WeekdayIso => {
                        if iso_weekday <= 7 {
                            fail("Multiple ISO weekday offsets specified".into(), at);
                            return None;
                        }
                        if !(1..=7).contains(&number) {
                            fail(
                                "ISO weekday offset out of range, expected a value between 1 and 7".into(),
                                at,
                            );
                            return None;
                        }
                        iso_weekday = number;
                    }
                    DayOfYearPadded | DayOfYearDecimal => {
                        match offset_specifier {
                            DayOfMonthPadded | DayOfMonth | MonthDecimalPadded | MonthDecimal => {}
                            YearWithoutCenturyPadded | YearWithoutCentury | YearDecimal | WeekdayDecimal => {
                                offset_specifier = spec;
                            }
                            _ => {
                                fail("Multiple year offsets specified".into(), at);
                                return None;
                            }
                        }
                        if !(1..=366).contains(&number) {
                            fail("Year day out of range, expected a value between 1 and 366".into(), at);
                            return None;
                        }
                        yearday = number;
                    }
                    _ => unreachable!("{spec:?} is not numeric"),
                }
            } else {
                match spec {
                    AmPm => {
                        let lower = |at: usize| data[at].to_ascii_lowercase();
                        if pos + 2 > size || lower(pos + 1) != b'm' || !matches!(lower(pos), b'a' | b'p') {
                            fail("Expected AM/PM".into(), Some(pos));
                            return None;
                        }
                        ampm = if lower(pos) == b'p' { 2 } else { 1 };
                        pos += 2;
                    }
                    AbbreviatedWeekdayName => {
                        if collection(data, &mut pos, &DAY_NAMES, true).is_none() {
                            fail(
                                "Expected an abbreviated day name (Mon, Tue, Wed, Thu, Fri, Sat, Sun)".into(),
                                Some(pos),
                            );
                            return None;
                        }
                    }
                    FullWeekdayName => {
                        if collection(data, &mut pos, &DAY_NAMES, false).is_none() {
                            fail("Expected a full day name (Monday, Tuesday, etc...)".into(), Some(pos));
                            return None;
                        }
                    }
                    AbbreviatedMonthName => match collection(data, &mut pos, &MONTH_NAMES, true) {
                        Some(month) => r[1] = month as i32 + 1,
                        None => {
                            fail(
                                "Expected an abbreviated month name (Jan, Feb, Mar, etc..)".into(),
                                Some(pos),
                            );
                            return None;
                        }
                    },
                    FullMonthName => match collection(data, &mut pos, &MONTH_NAMES, false) {
                        Some(month) => r[1] = month as i32 + 1,
                        None => {
                            fail(
                                "Expected a full month name (January, February, etc...)".into(),
                                Some(pos),
                            );
                            return None;
                        }
                    },
                    UtcOffset => match utc_offset(data, &mut pos) {
                        Some((hh, mm, ss)) => r[7] = (hh * 60 + mm) * 60 + ss,
                        None => {
                            fail("Expected ±HH[MM] or -HH[:MM[:SS]]".into(), Some(pos));
                            return None;
                        }
                    },
                    TzName => {
                        while pos < size && is_space(data[pos]) {
                            pos += 1;
                        }
                        let begin = pos;
                        while pos < size
                            && (data[pos].is_ascii_alphanumeric() || matches!(data[pos], b'_' | b'/' | b'+' | b'-' | b':'))
                        {
                            pos += 1;
                        }
                        if pos == begin {
                            fail("Empty Time Zone name".into(), Some(begin));
                            return None;
                        }
                    }
                    _ => unreachable!("{spec:?} is numeric"),
                }
            }
            i += 1;
        }
        while pos < size && is_space(data[pos]) {
            pos += 1;
        }
        if pos != size {
            fail("Full specifier did not match: trailing characters".into(), Some(pos));
            return None;
        }
        if ampm != 0 {
            if r[3] > 12 {
                fail(
                    format!(
                        "Invalid hour: {} AM/PM, expected an hour within the range [0..12]",
                        r[3]
                    ),
                    None,
                );
                return None;
            }
            if ampm == 1 {
                if r[3] == 12 {
                    r[3] = 0;
                }
            } else if r[3] != 12 {
                r[3] += 12;
            }
        }
        match offset_specifier {
            YearIso | WeekNumberIso => {
                let iso_year = if iso_year > 9999 { 1900 } else { iso_year as i32 };
                let iso_week = if iso_week > 53 { 1 } else { iso_week as i32 };
                let iso_weekday = if iso_weekday > 7 { 1 } else { iso_weekday as i32 };
                let jan4 = Date::from_ymd(iso_year, 1, 4).expect("a year of four digits").0;
                let date = monday_of(jan4) + (iso_week - 1) * 7 + (iso_weekday - 1);
                (r[0], r[1], r[2]) = ymd(date);
            }
            WeekNumberPaddedSunFirst | WeekNumberPaddedMonFirst => {
                if has_weekday {
                    weekday = (weekday + 7 - u64::from(offset_specifier == WeekNumberPaddedMonFirst)) % 7;
                }
                let Some(jan1) = Date::from_ymd(r[0], 1, 1) else {
                    fail(format!("Date out of range: {}-1-1", r[0]), None);
                    return None;
                };
                let jan1 = jan1.0;
                let mut date = monday_of(jan1);
                date -= i32::from(offset_specifier == WeekNumberPaddedSunFirst);
                if date >= jan1 {
                    date -= 7;
                }
                date += (weekno * 7 + weekday) as i32;
                (r[0], r[1], r[2]) = ymd(date);
            }
            DayOfYearPadded | DayOfYearDecimal => {
                let year = r[0];
                let Some(jan1) = Date::from_ymd(year, 1, 1) else {
                    fail(format!("Date out of range: {year}-1-1"), None);
                    return None;
                };
                (r[0], r[1], r[2]) = ymd(jan1.0 + yearday as i32 - 1);
                if r[0] != year {
                    fail("Year day out of range for the year".into(), Some(0));
                    return None;
                }
            }
            _ => {}
        }
        Some(result)
    }
}

/// `Timestamp::TryCombineUnits`
fn combine_units(whole: i64, part: i64, unit: i64) -> Option<i64> {
    if let Some(units) = whole.checked_mul(unit) {
        return units.checked_add(part);
    }
    if whole >= 0 {
        return None;
    }
    (whole + 1).checked_mul(unit)?.checked_add(part - unit)
}

fn finite(value: i64) -> bool {
    value != i64::MAX && value != -i64::MAX
}

impl Parsed {
    /// `ParseResult::TryToDate`
    pub fn date(&self) -> Option<i32> {
        match self.special {
            Some(days) => Some(days),
            None => Date::from_ymd(self.data[0], self.data[1], self.data[2]).map(|d| d.0),
        }
    }

    fn offset(&self) -> (i64, i64, i64) {
        let offset = self.data[7];
        let hh = offset / 3600;
        let rest = offset % 3600;
        (i64::from(hh), i64::from(rest / 60), i64::from(rest % 60))
    }

    /// `ParseResult::ToTime`: the microseconds of the day, rounded, less the UTC offset.
    fn time(&self) -> i64 {
        let (hh, mm, ss) = self.offset();
        let d = &self.data;
        let micros = (i64::from(d[6]) + 500) / 1000;
        ((i64::from(d[3]) - hh) * 60 + i64::from(d[4]) - mm) * 60 * 1_000_000
            + (i64::from(d[5]) - ss) * 1_000_000
            + micros
    }

    /// `ParseResult::ToTimeNS`
    fn time_ns(&self) -> i64 {
        let (hh, mm, ss) = self.offset();
        let d = &self.data;
        (((i64::from(d[3]) - hh) * 60 + i64::from(d[4]) - mm) * 60 + i64::from(d[5]) - ss) * 1_000_000_000
            + i64::from(d[6])
    }

    fn special_micros(days: i32) -> i64 {
        if days == Date::INFINITY.0 {
            i64::MAX
        } else if days == Date::NINFINITY.0 {
            -i64::MAX
        } else {
            i64::from(days) * MICROS_PER_DAY
        }
    }

    /// `ParseResult::TryToTimestamp`
    pub fn try_timestamp(&self) -> Option<i64> {
        if let Some(days) = self.special {
            return Some(Self::special_micros(days));
        }
        let date = self.date()?;
        combine_units(i64::from(date), self.time(), MICROS_PER_DAY).filter(|v| finite(*v))
    }

    /// `ParseResult::ToTimestamp`
    pub fn timestamp(&self) -> Result<i64, (bool, String)> {
        if let Some(days) = self.special {
            return Ok(Self::special_micros(days));
        }
        let d = &self.data;
        let date = self
            .date()
            .ok_or_else(|| (true, format!("Date out of range: {}-{}-{}", d[0], d[1], d[2])))?;
        combine_units(i64::from(date), self.time(), MICROS_PER_DAY)
            .filter(|v| finite(*v))
            .ok_or_else(|| (true, "Date and time not in timestamp range".to_string()))
    }

    fn special_nanos(days: i32) -> i64 {
        if days == Date::INFINITY.0 {
            i64::MAX
        } else if days == Date::NINFINITY.0 {
            -i64::MAX
        } else {
            i64::from(days).wrapping_mul(NANOS_PER_DAY)
        }
    }

    /// `ParseResult::TryToTimestampNS`
    pub fn try_timestamp_ns(&self) -> Option<i64> {
        if let Some(days) = self.special {
            return Some(Self::special_nanos(days));
        }
        let date = self.date()?;
        combine_units(i64::from(date), self.time_ns(), NANOS_PER_DAY).filter(|v| finite(*v))
    }

    /// `ParseResult::ToTimestampNS`; every error is a conversion error.
    pub fn timestamp_ns(&self) -> Result<i64, String> {
        if let Some(days) = self.special {
            return Ok(Self::special_nanos(days));
        }
        let d = &self.data;
        let date = self
            .date()
            .ok_or_else(|| format!("Date out of range: {}-{}-{}", d[0], d[1], d[2]))?;
        let value = combine_units(i64::from(date), self.time_ns(), NANOS_PER_DAY)
            .ok_or_else(|| format!("Date out of nanosecond range: {}-{}-{}", d[0], d[1], d[2]))?;
        if !finite(value) {
            return Err("Overflow exception in date/time -> timestamp_ns conversion".into());
        }
        Ok(value)
    }
}

/// `ParseResult::FormatError`
pub(crate) fn format_error(input: &str, format: &str, error: &ParseError) -> String {
    let pointer = match error.position {
        Some(position) => format!("{input}\n{}^", " ".repeat(position)),
        None => String::new(),
    };
    format!(
        "Could not parse string \"{input}\" according to format specifier \"{format}\"\n{pointer}\nError: {}",
        error.message
    )
}
