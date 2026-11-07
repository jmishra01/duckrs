// Ported from duckdb/src/execution/operator/csv_scanner/sniffer/ and util/csv_error.cpp
// @ rust-port-baseline
//
//! `CSVSniffer`: finds the dialect of a CSV file, then its types, then whether it has a header.

use super::csv::{CsvReadOptions, is_null};
use super::csv_state_machine::{
    ColumnCountScanner, Dialect, Field, Machine, NewLine, Row, Scanned, Transitions, ValueScanner, detect_new_line,
    skip_rows,
};
use crate::function::cast::{CastMode, cast_vector};
use crate::function::dates::{strptime_micros, strptime_strict};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::keywords::{categories, category};
use std::collections::HashMap;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

/// `CSVReaderOptions::sniff_size`
const SNIFF_SIZE: usize = 2048;

const DATE: usize = 0;
const TIMESTAMP: usize = 1;

const DATE_TEMPLATES: [&str; 6] = ["%m-%d-%Y", "%m-%d-%y", "%d-%m-%Y", "%d-%m-%y", "%Y-%m-%d", "%y-%m-%d"];
const TIMESTAMP_TEMPLATES: [&str; 7] = [
    "%Y-%m-%d %H:%M:%S.%f",
    "%m-%d-%Y %I:%M:%S %p",
    "%m-%d-%y %I:%M:%S %p",
    "%d-%m-%Y %H:%M:%S",
    "%d-%m-%y %H:%M:%S",
    "%Y-%m-%d %H:%M:%S",
    "%y-%m-%d %H:%M:%S",
];

/// How values of a file are written: what casting needs on top of a type.
#[derive(Debug, Clone, Default)]
pub struct Parsing {
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    pub decimal_separator: u8,
    pub thousands: Option<u8>,
}

/// The default of `auto_type_candidates`, from the widest type.
pub fn default_type_candidates() -> Vec<LogicalType> {
    vec![
        LogicalType::VARCHAR,
        LogicalType::DOUBLE,
        LogicalType::new(LogicalTypeId::Bignum),
        LogicalType::HUGEINT,
        LogicalType::BIGINT,
        LogicalType::new(LogicalTypeId::TimestampTz),
        LogicalType::TIMESTAMP,
        LogicalType::DATE,
        LogicalType::TIME,
        LogicalType::BOOLEAN,
        LogicalType::SQLNULL,
    ]
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `TrySimpleIntegerCast`, strictly.
fn integer_text(text: &[u8]) -> Option<(bool, &[u8])> {
    let start = text.iter().position(|&c| !is_space(c))?;
    let end = text.iter().rposition(|&c| !is_space(c))? + 1;
    let text = &text[start..end];
    let (negative, digits) = match text.first()? {
        b'-' => (true, &text[1..]),
        b'+' => return None,
        _ => (false, text),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    Some((negative, digits))
}

/// `CSVSniffer::CanYouCastBignum`
fn can_cast_bignum(text: &[u8]) -> bool {
    let Some(start) = text.iter().position(|&c| !is_space(c)) else {
        return false;
    };
    if text[start] == b'+' {
        return false;
    }
    integer_text(text).is_some()
}

/// `TryDoubleCast`, strictly: no sign of plus, and no leading zeros.
fn can_cast_double(text: &str, ty: &LogicalType) -> bool {
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let bytes = trimmed.as_bytes();
    match bytes {
        [] | [b'+', ..] => return false,
        [b'0', next, ..] if next.is_ascii_digit() => return false,
        _ => {}
    }
    casts(text, ty)
}

/// `Time::TryConvertTime`, strictly: `h:mm`, seconds and fractions, and nothing else.
fn can_cast_time(text: &str, ty: &LogicalType) -> bool {
    let trimmed = text.trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let bytes = trimmed.as_bytes();
    let digits = |from: usize| bytes[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let hours = digits(0);
    if !(1..=9).contains(&hours) || bytes.get(hours) != Some(&b':') || digits(hours + 1) != 2 {
        return false;
    }
    let mut at = hours + 3;
    if bytes.get(at) == Some(&b':') {
        if bytes.get(at + 1..).is_none_or(|_| digits(at + 1) != 2) {
            return false;
        }
        at += 3;
        if bytes.get(at) == Some(&b'.') {
            at += 1;
            let fraction = digits(at);
            if fraction == 0 || fraction > 6 {
                return false;
            }
            at += fraction;
        }
    }
    at == bytes.len() && casts(trimmed, ty)
}

/// Whether `text` becomes a `ty` strictly, through a one value cast.
fn casts(text: &str, ty: &LogicalType) -> bool {
    let mut single = Vector::new(LogicalType::VARCHAR, 1);
    if single.set_value(0, &Value::varchar(text)).is_err() {
        return false;
    }
    let mut result = Vector::new(ty.clone(), 1);
    cast_vector(&single, &mut result, 1, CastMode::Try).is_ok() && !result.get_value(0).is_null()
}

/// Removes a thousands separator, and makes a decimal comma a point (`CSVCast` and
/// `BaseScanner::RemoveSeparator`). `None` if it can't be a number of `ty`.
pub fn numeric_text(text: &str, ty: &LogicalType, parsing: &Parsing) -> Option<String> {
    let mut result = if ty.id() == LogicalTypeId::Bignum {
        text.trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
            .to_string()
    } else {
        text.to_string()
    };
    if let Some(thousands) = parsing.thousands {
        result = result.replace(char::from(thousands), "");
    }
    if parsing.decimal_separator == b','
        && matches!(
            ty.id(),
            LogicalTypeId::Double | LogicalTypeId::Float | LogicalTypeId::Decimal
        )
    {
        if result.contains('.') {
            return None;
        }
        result = result.replace(',', ".");
    }
    Some(result)
}

/// `CSVSniffer::CanYouCastIt`, for a value that isn't NULL.
pub fn can_cast(text: &str, ty: &LogicalType, parsing: &Parsing) -> bool {
    let numeric = ty.is_numeric() || ty.id() == LogicalTypeId::Bignum;
    let stripped;
    let text = if numeric {
        match numeric_text(text, ty, parsing) {
            Some(result) => {
                stripped = result;
                stripped.as_str()
            }
            None => return false,
        }
    } else {
        text
    };
    let parse_int = |low: i128, high: i128| {
        integer_text(text.as_bytes()).is_some_and(|(negative, digits)| {
            std::str::from_utf8(digits)
                .ok()
                .and_then(|d| d.parse::<i128>().ok())
                .is_some_and(|value| (low..=high).contains(&if negative { -value } else { value }))
        })
    };
    match ty.id() {
        LogicalTypeId::Varchar => true,
        LogicalTypeId::Boolean => ["t", "f", "true", "false"]
            .iter()
            .any(|word| word.eq_ignore_ascii_case(text)),
        LogicalTypeId::TinyInt => parse_int(i8::MIN.into(), i8::MAX.into()),
        LogicalTypeId::SmallInt => parse_int(i16::MIN.into(), i16::MAX.into()),
        LogicalTypeId::Integer => parse_int(i32::MIN.into(), i32::MAX.into()),
        LogicalTypeId::BigInt => parse_int(i64::MIN.into(), i64::MAX.into()),
        LogicalTypeId::UTinyInt => parse_int(0, u8::MAX.into()),
        LogicalTypeId::USmallInt => parse_int(0, u16::MAX.into()),
        LogicalTypeId::UInteger => parse_int(0, u32::MAX.into()),
        LogicalTypeId::UBigInt => parse_int(0, u64::MAX.into()),
        LogicalTypeId::HugeInt => integer_text(text.as_bytes()).is_some() && casts(text, ty),
        LogicalTypeId::Bignum => can_cast_bignum(text.as_bytes()),
        LogicalTypeId::Date if parsing.date_format.is_some() => {
            strptime_micros(text, parsing.date_format.as_deref().unwrap_or_default()).is_ok()
        }
        LogicalTypeId::Timestamp if parsing.timestamp_format.is_some() => {
            strptime_micros(text, parsing.timestamp_format.as_deref().unwrap_or_default()).is_ok()
        }
        LogicalTypeId::SqlNull => false,
        LogicalTypeId::Double | LogicalTypeId::Float => can_cast_double(text, ty),
        LogicalTypeId::Time => can_cast_time(text, ty),
        // an offset makes it a TIMESTAMP WITH TIME ZONE
        LogicalTypeId::Timestamp => {
            let after_time = text.rfind(':').map_or("", |at| &text[at..]);
            !after_time.contains(['+', '-', 'Z', 'z']) && casts(text, ty)
        }
        // `Date::TryConvertDate` strictly stops at the date
        LogicalTypeId::Date => {
            let trimmed = text.trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8));
            let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
            let year = digits.bytes().take_while(u8::is_ascii_digit).count();
            let special = digits.bytes().next().is_some_and(|c| !c.is_ascii_digit());
            (year >= 2 || special) && !text.contains(':') && casts(text, ty)
        }
        _ => casts(text, ty),
    }
}

/// `StartsWithNumericDate`: the separator of a value that starts like a date.
fn numeric_date_separator(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let field1 = bytes.iter().position(|&c| !is_space(c))?;
    if !bytes[field1].is_ascii_digit() {
        return None;
    }
    let after = |from: usize, digits: bool| (from..bytes.len()).find(|&i| bytes[i].is_ascii_digit() == digits);
    let literal1 = after(field1, false)?;
    let field2 = after(literal1, true)?;
    let literal2 = after(field2, false)?;
    let field3 = after(literal2, true)?;
    if bytes[literal1..field2] != bytes[literal2..field3] {
        return None;
    }
    Some(value[literal1..field2].replace('%', "%%"))
}

/// `CandidateStats`
#[derive(Default)]
struct Stats {
    rows_read: usize,
    best_consistent_rows: usize,
    prev_padding_count: usize,
    min_ignored_rows: usize,
}

struct Candidate {
    scanner: ColumnCountScanner,
    skip: usize,
    columns: usize,
    rows_until_header: usize,
}

#[derive(Default, Clone)]
struct Formats {
    initialized: bool,
    had_match: bool,
    formats: Vec<String>,
    original_size: usize,
}

/// A column's candidate types; the last is the best guess.
type TypeCandidates = Vec<Vec<LogicalType>>;

/// A header value: its text, or NULL.
type HeaderValue = Option<String>;

/// The outcome of sniffing a file.
#[derive(Debug)]
pub struct Sniffed {
    pub dialect: Dialect,
    pub header: bool,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub skip: usize,
    pub rows_until_header: usize,
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    /// `sniffer_user_mismatch_error`
    pub mismatch: String,
}

/// A chunk of rows as text, cut to the columns.
struct Sample {
    rows: Vec<Vec<Option<String>>>,
    errors: usize,
}

/// `StringValueScanner::ParseChunk`, of VARCHAR columns.
fn parse_sample(scanner: &mut ValueScanner, data: &[u8], columns: usize, options: &CsvReadOptions) -> Sample {
    let mut result = Sample {
        rows: Vec::new(),
        errors: 0,
    };
    let dialect = scanner.machine.dialect.clone();
    let strict = dialect.strict;
    while result.rows.len() < SNIFF_SIZE {
        let row: Row<'_> = match scanner.next(data) {
            None => break,
            Some(Scanned::Row(row)) => row,
            Some(Scanned::EmptyLine) => {
                if columns == 1 && options.null_strs.iter().any(String::is_empty) {
                    result.rows.push(vec![None]);
                }
                continue;
            }
            Some(Scanned::Unterminated | Scanned::Invalid) => {
                result.errors += 1;
                scanner.skip_line(data);
                continue;
            }
        };
        let mut fields: Vec<Field<'_>> = row.fields;
        if fields.len() > columns {
            let trailing_null =
                fields.len() == columns + 1 && fields.last().is_some_and(|f| is_null(f, options, &dialect));
            if !strict {
                scanner.machine.used_unstrictness = true;
            } else if !trailing_null {
                result.errors += 1;
                continue;
            }
            fields.truncate(columns);
        } else if fields.len() < columns && !options.null_padding {
            result.errors += 1;
            continue;
        }
        let mut values = Vec::with_capacity(columns);
        let mut valid = true;
        for field in &fields {
            if is_null(field, options, &dialect) {
                values.push(None);
            } else if let Ok(text) = std::str::from_utf8(&field.text) {
                values.push(Some(text.to_string()));
            } else {
                valid = false;
                break;
            }
        }
        if valid {
            values.resize(columns, None);
            result.rows.push(values);
        }
    }
    result
}

struct Sniffer<'a> {
    data: &'a [u8],
    path: &'a str,
    options: &'a CsvReadOptions,
    set_names: Option<&'a [String]>,
    set_types: Option<&'a [LogicalType]>,
    start: usize,
    max_columns_found: usize,
    max_columns_found_error: usize,
    lines_sniffed: usize,
    all_fail_max_line_size: bool,
    long_line: Option<usize>,
    candidates: Vec<Candidate>,
}

impl Sniffer<'_> {
    fn candidate_unacceptable(&self, columns: usize, last_value_always_empty: bool) -> bool {
        let Some(set) = self.set_types else {
            return false;
        };
        if self.options.ignore_errors {
            return false;
        }
        let size = set.len();
        if columns == size || columns == size + usize::from(last_value_always_empty) {
            return false;
        }
        !(self.options.null_padding && columns > size)
    }

    fn set_size(&self) -> usize {
        self.set_types.map_or(0, <[LogicalType]>::len)
    }

    /// `DialectCandidates`, with its order.
    fn quote_escapes(&self) -> Vec<(Option<u8>, Option<u8>)> {
        const Q: Option<u8> = Some(b'"');
        const S: Option<u8> = Some(b'\'');
        const B: Option<u8> = Some(b'\\');
        let defaults = [
            (None, None),
            (Q, None),
            (Q, Q),
            (Q, S),
            (Q, B),
            (S, None),
            (S, S),
            (S, Q),
            (S, B),
        ];
        match (self.options.quote, self.options.escape) {
            (Some(quote), Some(escape)) => vec![(quote, escape)],
            (Some(quote), None) => {
                let found: Vec<_> = defaults.iter().copied().filter(|c| c.0 == quote).collect();
                if found.is_empty() {
                    vec![(quote, quote), (quote, None), (quote, B), (quote, Q), (quote, S)]
                } else {
                    found
                }
            }
            (None, Some(escape)) => {
                let found: Vec<_> = defaults.iter().copied().filter(|c| c.1 == escape).collect();
                if found.is_empty() {
                    vec![(escape, escape), (Q, escape), (S, escape)]
                } else {
                    found
                }
            }
            (None, None) => defaults.to_vec(),
        }
    }

    fn delimiters(&self) -> Vec<Vec<u8>> {
        match &self.options.delimiter {
            Some(delimiter) => vec![delimiter.clone()],
            None => [b",", b"|", b";", b"\t"].iter().map(|d| d.to_vec()).collect(),
        }
    }

    fn comments(&self) -> Vec<Option<u8>> {
        match self.options.comment {
            Some(comment) => vec![comment],
            None => vec![None, Some(b'#')],
        }
    }

    /// `DialectCandidates::Print`
    fn search_space(&self) -> String {
        let delimiters: Vec<String> = self
            .delimiters()
            .iter()
            .map(|d| format!("'{}'", String::from_utf8_lossy(d)))
            .collect();
        let shown = |byte: Option<u8>, empty: &str| byte.map_or(empty.to_string(), |b| char::from(b).to_string());
        let quotes: Vec<String> = self
            .quote_escapes()
            .iter()
            .map(|(q, e)| format!("['{}','{}']", shown(*q, "(no quote)"), shown(*e, "(no escape)")))
            .collect();
        let comments: Vec<String> = self
            .comments()
            .iter()
            .map(|c| format!("'{}'", shown(*c, "\0")))
            .collect();
        format!(
            "Delimiter Candidates: {}\nQuote/Escape Candidates: {}\nComment Candidates: {}\n",
            delimiters.join(", "),
            quotes.join(","),
            comments.join(", ")
        )
    }

    /// `CSVError::SniffingError`
    fn sniffing_error(&self, columns: usize, types: bool) -> Error {
        let options = self.options;
        let mut error = format!(
            "Error when sniffing file \"{}\".\nIt was not possible to automatically detect the CSV parsing {}\n\
             The search space used was:\n{}Encoding: utf-8\nPossible fixes:\n",
            self.path,
            if types { "types" } else { "dialect" },
            self.search_space()
        );
        if options.strict_mode {
            error += "* Disable the parser's strict mode (strict_mode=false) to allow reading rows that do not \
                      comply with the CSV standard.\n";
        }
        if let (Some(names), Some(types)) = (self.set_names, self.set_types) {
            let set: Vec<String> = names.iter().zip(types).map(|(n, t)| format!("'{n}': '{t}'")).collect();
            error += &format!(
                "* Columns are set as: \"{}\", and they contain: {} columns. It does not match the number of columns \
                 found by the sniffer: {columns}. Verify the columns parameter is correctly set.\n",
                set.join(", "),
                types.len()
            );
        }
        error += "* Make sure you are using the correct file encoding. If not, set it (e.g., encoding = 'utf-16').\n";
        error += &match &options.delimiter {
            None => "* Set delimiter (e.g., delim=',')\n".to_string(),
            Some(d) => format!(
                "* Delimiter is set to '{}'. Consider unsetting it.\n",
                String::from_utf8_lossy(d)
            ),
        };
        let shown = |byte: Option<u8>| byte.map_or("\0".to_string(), |b| char::from(b).to_string());
        error += &match options.quote {
            None => "* Set quote (e.g., quote='\"')\n".to_string(),
            Some(q) => format!("* Quote is set to '{}'. Consider unsetting it.\n", shown(q)),
        };
        error += &match options.escape {
            None => "* Set escape (e.g., escape='\"')\n".to_string(),
            Some(e) => format!("* Escape is set to '{}'. Consider unsetting it.\n", shown(e)),
        };
        error += &match options.comment {
            None => "* Set comment (e.g., comment='#')\n".to_string(),
            Some(c) => format!("* Comment is set to '{}'. Consider unsetting it.\n", shown(c)),
        };
        if options.skip.is_none() {
            error += "* Set skip (skip=${n}) to skip ${n} lines at the top of the file\n";
        }
        if !options.ignore_errors {
            error += "* Enable ignore errors (ignore_errors=true) to ignore potential errors\n";
        }
        if !options.null_padding {
            error += "* Enable null padding (null_padding=true) to pad missing columns with NULL values\n";
        }
        error += "* Check you are using the correct file compression, otherwise set it (e.g., compression = 'zstd')\n";
        error += "* Be sure that the maximum line size is set to an appropriate value, otherwise set it (e.g., \
                  max_line_size=10000000)\n";
        Error::new(ErrorKind::InvalidInput, error)
    }

    /// `CSVError::HeaderSniffingError`
    fn header_error(&self, header: &[HeaderValue], columns: usize, delimiter: &[u8]) -> Error {
        let options = self.options;
        let row: Vec<&str> = header.iter().map(|v| v.as_deref().unwrap_or("NULL")).collect();
        let mut error = format!(
            "Error when sniffing file \"{}\".\nIt was not possible to detect the CSV Header, due to the header \
             having less columns than expected\nNumber of expected columns: {columns}. Actual number of columns \
             {}\nDetected row as Header:\n{}\nPossible fixes:\n",
            self.path,
            header.len(),
            row.join(&format!("{} ", String::from_utf8_lossy(delimiter)))
        );
        if options.strict_mode {
            error += "* Disable the parser's strict mode (strict_mode=false) to allow reading rows that do not \
                      comply with the CSV standard.\n";
        }
        error += &match options.header {
            None => {
                "* Set header (header = true) if your CSV has a header, or (header = false) if it doesn't\n".to_string()
            }
            Some(header) => format!("* Header is set to '{}'. Consider unsetting it.\n", u8::from(header)),
        };
        error += &match options.skip {
            None => "* Set skip (skip=${n}) to skip ${n} lines at the top of the file\n".to_string(),
            Some(skip) => format!("* Skip is set to '{skip}'. Consider unsetting it.\n"),
        };
        if !options.ignore_errors {
            error += "* Enable ignore errors (ignore_errors=true) to ignore potential errors\n";
        }
        if !options.null_padding {
            error += "* Enable null padding (null_padding=true) to pad missing columns with NULL values\n";
        }
        Error::new(ErrorKind::InvalidInput, error)
    }

    /// `GenerateStateMachineSearchSpace`
    fn search(&self) -> Vec<ColumnCountScanner> {
        let new_line = self.options.new_line.unwrap_or_else(|| detect_new_line(self.data));
        let mut result = Vec::new();
        let mut first = None;
        for comment in self.comments() {
            for (quote, escape) in self.quote_escapes() {
                for delimiter in self.delimiters() {
                    let dialect = Dialect {
                        delimiter,
                        quote,
                        escape,
                        comment,
                        new_line,
                        strict: self.options.strict_mode,
                    };
                    let transitions = Arc::new(Transitions::new(&dialect));
                    let start = match self.options.skip {
                        Some(skip) => {
                            *first.get_or_insert_with(|| skip_rows(self.data, &dialect, &transitions, self.start, skip))
                        }
                        None => self.start,
                    };
                    let machine = Machine::with_transitions(dialect, transitions, start);
                    result.push(ColumnCountScanner::new(
                        machine,
                        SNIFF_SIZE,
                        self.options.maximum_line_size(),
                    ));
                }
            }
        }
        result
    }

    /// `AreCommentsAcceptable`
    fn comments_acceptable(&self, scanner: &ColumnCountScanner, columns: usize) -> bool {
        if self.options.comment.is_some() {
            return true;
        }
        let (mut detected, mut valid) = (0.0f64, 0.0f64);
        let mut full_line = false;
        for row in scanner.rows() {
            if row.is_comment || row.is_mid_comment {
                detected += 1.0;
                if row.columns != columns && row.is_comment {
                    full_line = true;
                    valid += 1.0;
                }
                if (row.columns == columns || (row.columns <= columns && self.options.null_padding))
                    && row.is_mid_comment
                {
                    valid += 1.0;
                }
            }
        }
        if valid == 0.0 || !full_line {
            return scanner.machine.dialect.comment.is_none();
        }
        valid / detected >= 0.6
    }

    /// `AnalyzeDialectCandidate`
    fn analyze(&mut self, mut scanner: ColumnCountScanner, stats: &mut Stats) {
        let options = self.options;
        scanner.parse_chunk(self.data);
        if scanner.error() {
            match scanner.long_line() {
                Some(size) => self.long_line = Some(size),
                None => self.all_fail_max_line_size = false,
            }
            return;
        }
        self.all_fail_max_line_size = false;
        let rows = scanner.rows().to_vec();
        let (mut dirty_notes, mut dirty_notes_minus_comments, mut empty_lines) = (0usize, 0usize, 0usize);
        let mut consistent_rows = 0usize;
        let mut num_cols = rows.first().map_or(1, |row| row.columns);
        let ignore_errors = options.ignore_errors;
        let use_most_frequent = ignore_errors && !options.null_padding;
        if use_most_frequent {
            num_cols = scanner.most_frequent_column_count();
        }
        let (mut padding_count, mut comment_rows, mut ignored_rows) = (0usize, 0usize, 0usize);
        let allow_padding = options.null_padding;
        let mut first_valid = false;
        let mut rows_until_header = 0usize;
        stats.rows_read = stats.rows_read.max(rows.len());
        let first_empty = rows.first().is_none_or(|row| row.last_value_always_empty);
        if self.candidate_unacceptable(num_cols, first_empty) {
            self.max_columns_found_error = self.max_columns_found_error.max(num_cols);
            return;
        }
        let skip_set = options.skip.is_some();
        let set = self.set_types.is_some();
        let mut header_idx = 0;
        for (row, count) in rows.iter().enumerate() {
            if self.candidate_unacceptable(count.columns, count.last_value_always_empty) {
                self.max_columns_found_error = self.max_columns_found_error.max(count.columns);
                return;
            }
            if count.is_comment {
                comment_rows += 1;
            } else if count.last_value_always_empty && count.columns == rows[header_idx].columns + 1 {
                // the first row may miss a column if the rest end on a delimiter, as C1|C2\n1|2|\n3|4|
                consistent_rows += 1;
            } else if num_cols < count.columns
                && (!skip_set || comment_rows > 0)
                && (!set || options.null_padding)
                && (!first_valid || !use_most_frequent)
            {
                if !first_valid {
                    first_valid = true;
                    rows_until_header = row;
                }
                padding_count = 0;
                num_cols = count.columns;
                dirty_notes = row + count.empty_lines;
                empty_lines = count.empty_lines;
                dirty_notes_minus_comments = dirty_notes - comment_rows;
                header_idx = row;
                consistent_rows = 1;
            } else if count.columns == num_cols || use_most_frequent {
                if !first_valid {
                    first_valid = true;
                    rows_until_header = row;
                    dirty_notes = row + count.empty_lines;
                    empty_lines = count.empty_lines;
                    dirty_notes_minus_comments = dirty_notes - comment_rows;
                    num_cols = count.columns;
                }
                if count.columns != num_cols {
                    ignored_rows += 1;
                }
                consistent_rows += 1;
            } else if num_cols >= count.columns {
                padding_count += 1;
            }
        }
        let user_skip = options.skip.unwrap_or(0);
        if skip_set {
            rows_until_header += user_skip;
        }
        consistent_rows += padding_count;
        let max_found = self.max_columns_found;
        let more_values = consistent_rows > stats.best_consistent_rows && num_cols >= max_found;
        let more_columns = consistent_rows == stats.best_consistent_rows && num_cols > max_found;
        let require_more_padding = padding_count > stats.prev_padding_count;
        let require_less_padding = padding_count < stats.prev_padding_count;
        let single_column_before = max_found < 2 && num_cols > max_found * self.candidates.len();
        // wrapping as in C++: skip counts against both sides
        let rows_consistent = consistent_rows
            .wrapping_add(dirty_notes_minus_comments.wrapping_sub(user_skip))
            .wrapping_add(comment_rows)
            .wrapping_sub(empty_lines)
            == rows.len().wrapping_sub(user_skip);
        let more_than_one_row = consistent_rows > 1;
        let more_than_one_column = num_cols > 1;
        let start_good = self.candidates.first().is_some_and(|c| dirty_notes <= c.skip);
        let invalid_padding = !allow_padding && padding_count > 0;
        let comments_acceptable = self.comments_acceptable(&scanner, num_cols);
        let ever_quoted = scanner.machine.ever_quoted;
        let ever_escaped = scanner.machine.ever_escaped;
        let quoted = ever_quoted && scanner.machine.dialect.quote.is_some();
        let comment_by_user = options.comment.is_some();
        let set_size = self.set_size();
        let columns_match_set = num_cols == set_size || (num_cols == set_size + 1 && first_empty) || !set;
        self.max_columns_found_error = self.max_columns_found_error.max(num_cols);
        let strict = scanner.machine.dialect.strict;
        let mut candidate = Candidate {
            scanner,
            skip: 0,
            columns: num_cols,
            rows_until_header,
        };
        if columns_match_set
            && (rows_consistent || (set && ignore_errors))
            && (single_column_before
                || ((more_values || more_columns) && !require_more_padding)
                || (more_than_one_column && require_less_padding)
                || (quoted && (comment_rows == 0 || comment_by_user)))
            && !invalid_padding
            && comments_acceptable
        {
            if let Some(front) = self.candidates.first()
                && set
                && max_found == set_size
                && consistent_rows <= stats.best_consistent_rows
                && (front.scanner.machine.ever_quoted || !ever_quoted)
            {
                return;
            }
            if let Some(front) = self.candidates.first()
                && front.scanner.machine.ever_quoted
            {
                if !ever_quoted {
                    return;
                }
                if !ever_escaped && front.scanner.machine.ever_escaped && strict {
                    return;
                }
                if stats.best_consistent_rows == consistent_rows && num_cols >= max_found {
                    if skip_set {
                        if dirty_notes != 0 && !options.null_padding && !ignore_errors {
                            return;
                        }
                        candidate.skip = user_skip;
                    } else if !options.null_padding {
                        candidate.skip = dirty_notes;
                    }
                    self.lines_sniffed = rows.len();
                    self.candidates.push(candidate);
                    self.max_columns_found = num_cols;
                    return;
                }
            }
            if max_found == num_cols && ignored_rows > stats.min_ignored_rows {
                return;
            }
            if max_found > 1
                && num_cols > max_found
                && consistent_rows < stats.best_consistent_rows / 2
                && (options.null_padding || ignore_errors)
            {
                return;
            }
            if quoted && num_cols < max_found {
                let beaten = if ever_escaped && strict {
                    self.candidates
                        .iter()
                        .any(|c| c.scanner.machine.ever_quoted && c.scanner.machine.ever_escaped)
                } else {
                    self.candidates.iter().any(|c| c.scanner.machine.ever_quoted)
                };
                if beaten {
                    return;
                }
            }
            stats.best_consistent_rows = consistent_rows;
            self.max_columns_found = num_cols;
            stats.prev_padding_count = padding_count;
            stats.min_ignored_rows = ignored_rows;
            if skip_set {
                if dirty_notes - empty_lines != 0 && !options.null_padding && !ignore_errors && comment_rows == 0 {
                    return;
                }
                candidate.skip = user_skip;
            } else if !options.null_padding {
                candidate.skip = dirty_notes_minus_comments;
            }
            self.candidates.clear();
            self.lines_sniffed = rows.len();
            self.candidates.push(candidate);
            return;
        }
        if columns_match_set
            && more_than_one_row
            && more_than_one_column
            && start_good
            && rows_consistent
            && !require_more_padding
            && !invalid_padding
            && num_cols == max_found
            && comments_acceptable
        {
            if skip_set {
                if dirty_notes != 0 && !options.null_padding && !ignore_errors {
                    return;
                }
                candidate.skip = user_skip;
            } else if !options.null_padding {
                candidate.skip = dirty_notes;
            }
            self.lines_sniffed = rows.len();
            self.candidates.push(candidate);
        }
    }

    /// `RefineCandidateNextChunk`
    fn refine_next_chunk(&self, candidate: &mut Candidate) -> bool {
        candidate.scanner.parse_chunk(self.data);
        let options = self.options;
        let lenient = options.null_padding || options.ignore_errors;
        for row in candidate.scanner.rows() {
            if self.set_types.is_some() {
                return !self.candidate_unacceptable(row.columns, row.last_value_always_empty);
            }
            if self.max_columns_found != row.columns && !lenient && !row.is_comment {
                return false;
            }
        }
        true
    }

    /// `RefineCandidates`
    fn refine(&mut self) {
        if self.candidates.len() <= 1 || self.candidates[0].scanner.finished() {
            return;
        }
        let chunks = self.options.sample_chunks();
        let mut i = 1;
        while i <= chunks {
            let mut kept = Vec::new();
            let mut done = self.candidates.is_empty();
            for mut candidate in std::mem::take(&mut self.candidates) {
                let finished = candidate.scanner.finished();
                if kept.is_empty() {
                    self.lines_sniffed += candidate.scanner.rows().len();
                }
                if finished || i == chunks {
                    if !candidate.scanner.error() {
                        kept.push(candidate);
                    }
                    done = true;
                    continue;
                }
                if self.refine_next_chunk(&mut candidate) && !candidate.scanner.error() {
                    kept.push(candidate);
                }
            }
            self.candidates = kept;
            if done {
                break;
            }
            i += 1;
        }
        let successful = std::mem::take(&mut self.candidates);
        let mut ever_quoted = false;
        for candidate in successful {
            if candidate.scanner.machine.ever_quoted {
                if candidate.scanner.machine.ever_escaped {
                    self.candidates = vec![candidate];
                    return;
                }
                if !ever_quoted {
                    ever_quoted = true;
                    self.candidates.clear();
                }
                self.candidates.push(candidate);
            } else if !ever_quoted {
                self.candidates.push(candidate);
            }
        }
        if self.candidates.len() > 1 {
            let mut successful = std::mem::take(&mut self.candidates);
            let at = successful
                .iter()
                .position(|c| c.scanner.machine.dialect.quote == c.scanner.machine.dialect.escape)
                .unwrap_or(0);
            self.candidates.push(successful.swap_remove(at));
        }
    }

    /// `CSVSniffer::DetectDialect`
    fn detect_dialect(&mut self) -> Result<()> {
        let mut stats = Stats::default();
        for scanner in self.search() {
            self.analyze(scanner, &mut stats);
        }
        self.refine();
        if self.candidates.is_empty() {
            if let (true, Some(size)) = (self.all_fail_max_line_size, self.long_line) {
                return Err(line_size_error(self.options, size));
            }
            return Err(self.sniffing_error(self.max_columns_found_error, false));
        }
        Ok(())
    }
}

/// `CSVError::LineSizeError`
pub fn line_size_error(options: &CsvReadOptions, size: usize) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!(
            "Maximum line size of {} bytes exceeded. Actual Size:{size} bytes.\nPossible Solution: Change the \
             maximum length size, e.g., max_line_size={}",
            options.maximum_line_size(),
            size + 2
        ),
    )
}

/// `CSVSniffer::GenerateColumnName`: padded to the width of the last number.
fn generated_name(total: usize, column: usize) -> String {
    let width = total.saturating_sub(1).to_string().len();
    format!("column{column:0width$}")
}

/// `TrimWhitespace`: of what Unicode calls space separators.
fn trim_whitespace(name: &str) -> &str {
    let is_zs = |c: char| {
        matches!(
            c,
            ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
        )
    };
    name.trim_matches(is_zs)
}

/// `NormalizeColumnName`
fn normalize_column_name(name: &str) -> String {
    let nfkd: String = name.nfkd().collect();
    let mut ascii = String::new();
    for c in nfkd.chars() {
        if c == '_' || c.is_ascii_alphanumeric() {
            ascii.push(c);
        } else if c.is_ascii() && is_space(c as u8) {
            ascii.push(' ');
        }
    }
    let trimmed = trim_whitespace(&ascii);
    let mut cleaned = String::new();
    let mut in_whitespace = false;
    for c in trimmed.chars() {
        if c == ' ' {
            if !in_whitespace {
                cleaned.push('_');
                in_whitespace = true;
            }
        } else {
            cleaned.push(c);
            in_whitespace = false;
        }
    }
    let cleaned = if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned.to_lowercase()
    };
    let keyword = categories(&cleaned) & (category::UNRESERVED | category::TYPE_FUNC | category::RESERVED) != 0;
    if keyword || cleaned.as_bytes()[0].is_ascii_digit() {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

fn empty_header(name: Option<&str>, normalize: bool) -> bool {
    match name {
        None | Some("") => true,
        Some(_) if normalize => false,
        Some(name) => name.bytes().all(is_space),
    }
}

/// The state of type detection across dialects.
#[derive(Default)]
struct TypeDetection {
    formats: [Formats; 2],
}

impl TypeDetection {
    /// `InitializeDateAndTimeStampDetection`
    fn initialize(&mut self, kind: usize, separator: &str, user: Option<&str>, current: &mut Option<String>) {
        let formats = &mut self.formats[kind];
        if !formats.initialized {
            formats.initialized = true;
            if let Some(user) = user {
                formats.formats.push(user.to_string());
            } else {
                let templates: &[&str] = if kind == DATE {
                    &DATE_TEMPLATES
                } else {
                    &TIMESTAMP_TEMPLATES
                };
                for template in templates {
                    let format = template.replace('-', separator);
                    if !format.contains("%Y-%m-%d") {
                        formats.formats.push(format);
                    }
                }
            }
            formats.original_size = formats.formats.len();
        }
        if let Some(last) = formats.formats.last() {
            *current = Some(last.clone());
        }
    }

    /// `DetectDateAndTimeStampFormats`
    fn detect(&mut self, kind: usize, separator: &str, value: &str, user: Option<&str>, current: &mut Option<String>) {
        if !matches!(separator, "-" | "." | "/" | " ") {
            return;
        }
        self.initialize(kind, separator, user, current);
        let formats = &mut self.formats[kind];
        let saved = formats.formats.clone();
        let had_candidates = !saved.is_empty();
        let initial = saved.len() == formats.original_size;
        while !formats.formats.is_empty() && user.is_none() {
            if strptime_strict(value, current.as_deref().unwrap_or_default()) {
                formats.had_match = true;
                break;
            }
            formats.formats.pop();
            if let Some(last) = formats.formats.last() {
                *current = Some(last.clone());
            }
        }
        if formats.formats.is_empty() && had_candidates {
            if initial && !formats.had_match {
                formats.initialized = false;
                *current = None;
                return;
            }
            formats.formats = saved;
            *current = formats.formats.last().cloned();
        }
    }
}

/// `CSVSniffer::SniffTypes`
fn sniff_types(
    rows: &[Vec<Option<String>>],
    start: usize,
    candidates: &mut TypeCandidates,
    parsing: &mut Parsing,
    detection: &mut TypeDetection,
    options: &CsvReadOptions,
) {
    let (mut has_date, mut has_timestamp) = (false, false);
    for (column, types) in candidates.iter_mut().enumerate() {
        for (row, values) in rows.iter().enumerate().skip(start) {
            let value = values.get(column).and_then(Option::as_deref);
            let top = types.last().cloned().unwrap_or(LogicalType::VARCHAR);
            while types.len() > 1 {
                let ty = types[types.len() - 1].clone();
                if let Some(text) = value
                    && ((ty.id() == LogicalTypeId::Timestamp && !has_timestamp)
                        || (ty.id() == LogicalTypeId::Date && !has_date))
                    && let Some(separator) = numeric_date_separator(text)
                {
                    if ty.id() == LogicalTypeId::Date {
                        detection.detect(
                            DATE,
                            &separator,
                            text,
                            options.date_format.as_deref(),
                            &mut parsing.date_format,
                        );
                    } else {
                        detection.detect(
                            TIMESTAMP,
                            &separator,
                            text,
                            options.timestamp_format.as_deref(),
                            &mut parsing.timestamp_format,
                        );
                    }
                }
                if ty.id() == LogicalTypeId::Varchar {
                    break;
                }
                if value.is_none_or(|text| can_cast(text, &ty, parsing)) {
                    break;
                }
                let before_top = types.len().checked_sub(2).map(|i| &types[i]);
                if row != start
                    && (top == LogicalType::BOOLEAN
                        || top == LogicalType::DATE
                        || top == LogicalType::TIME
                        || (top == LogicalType::TIMESTAMP
                            && before_top.is_some_and(|t| t.id() != LogicalTypeId::TimestampTz))
                        || top.id() == LogicalTypeId::TimestampTz)
                {
                    while types.last().is_some_and(|t| t.id() != LogicalTypeId::Varchar) && types.len() > 1 {
                        types.pop();
                    }
                    break;
                }
                types.pop();
            }
        }
        match types.last().map(LogicalType::id) {
            Some(LogicalTypeId::Date) => has_date = true,
            Some(LogicalTypeId::Timestamp) => has_timestamp = true,
            _ => {}
        }
    }
}

/// `DetectHeaderWithSetColumn`
fn detect_header_with_set_columns(
    header: &[HeaderValue],
    names: &[String],
    types: &[LogicalType],
    parsing: &Parsing,
) -> bool {
    if names.len() != header.len() && names.len() + 1 != header.len() {
        return false;
    }
    let mut matches = true;
    for (value, name) in header.iter().zip(names) {
        match value {
            None => return false,
            Some(value) if value != name => {
                matches = false;
                break;
            }
            _ => {}
        }
    }
    if matches {
        return true;
    }
    let (mut all_varchar, mut consistent) = (true, true);
    for (value, ty) in header.iter().zip(types) {
        if *ty != LogicalType::VARCHAR {
            all_varchar = false;
            if value.as_deref().is_some_and(|text| !can_cast(text, ty, parsing)) {
                consistent = false;
            }
        }
    }
    all_varchar || !consistent
}

/// `CSVSniffer::SniffCSV`. With `columns`, names and types are the user's.
pub fn sniff(
    data: &[u8],
    path: &str,
    options: &CsvReadOptions,
    columns: Option<(&[String], &[LogicalType])>,
) -> Result<Sniffed> {
    let start = if data.starts_with(&[0xef, 0xbb, 0xbf]) { 3 } else { 0 };
    let (set_names, set_types) = (columns.map(|c| c.0), columns.map(|c| c.1));
    let mut sniffer = Sniffer {
        data,
        path,
        options,
        set_names,
        set_types,
        start,
        max_columns_found: 0,
        max_columns_found_error: 0,
        lines_sniffed: 0,
        all_fail_max_line_size: true,
        long_line: None,
        candidates: Vec::new(),
    };
    if !options.auto_detect {
        let quote = options.quote.unwrap_or(Some(b'"'));
        let dialect = Dialect {
            delimiter: options.delimiter.clone().unwrap_or_else(|| b",".to_vec()),
            quote,
            escape: options.escape.unwrap_or(quote),
            comment: options.comment.flatten(),
            new_line: options.new_line.unwrap_or(NewLine::NotSet),
            strict: options.strict_mode,
        };
        let skip = options.skip.unwrap_or(0);
        if let Some((names, types)) = columns {
            return Ok(Sniffed {
                dialect,
                header: options.header.unwrap_or(false),
                names: names.to_vec(),
                types: types.to_vec(),
                skip,
                rows_until_header: 0,
                date_format: options.date_format.clone(),
                timestamp_format: options.timestamp_format.clone(),
                mismatch: String::new(),
            });
        }
        // `MinimalSniff`: the dialect is known, so the first row says how many columns there are
        let machine = Machine::new(dialect, start);
        let transitions = machine.transitions();
        let begin = skip_rows(data, &machine.dialect, &transitions, start, skip);
        let mut scanner = ColumnCountScanner::new(
            Machine::with_transitions(machine.dialect, transitions, begin),
            SNIFF_SIZE,
            options.maximum_line_size(),
        );
        scanner.parse_chunk(data);
        let columns = scanner.rows().first().map_or(1, |row| row.columns);
        sniffer.max_columns_found = columns;
        sniffer.lines_sniffed = scanner.rows().len();
        sniffer.candidates.push(Candidate {
            scanner,
            skip,
            columns,
            rows_until_header: skip,
        });
    } else {
        sniffer.detect_dialect()?;
    }

    // Type detection
    let type_candidates = options
        .auto_type_candidates
        .clone()
        .unwrap_or_else(default_type_candidates);
    let columns_found = sniffer.max_columns_found;
    let mut detection = TypeDetection::default();
    let base = Parsing {
        date_format: options.date_format.clone(),
        timestamp_format: options.timestamp_format.clone(),
        decimal_separator: options.decimal_separator,
        thousands: options.thousands,
    };
    let header_may_exist = options.header != Some(false);
    let mut min_varchar = columns_found + 1;
    let mut min_errors = usize::MAX;
    let mut best_is_strict = false;
    struct Best {
        scanner: ValueScanner,
        candidate: usize,
        types: TypeCandidates,
        parsing: Parsing,
        header_row: Vec<HeaderValue>,
        single_row: bool,
    }
    let mut best: Option<Best> = None;
    for (index, candidate) in sniffer.candidates.iter().enumerate() {
        let dialect = &candidate.scanner.machine.dialect;
        let transitions = candidate.scanner.machine.transitions();
        let begin = if candidate.scanner.start() != start && options.skip.is_some() {
            candidate.scanner.start()
        } else {
            skip_rows(data, dialect, &transitions, start, candidate.skip)
        };
        let mut scanner = ValueScanner::new(Machine::with_transitions(dialect.clone(), transitions, begin));
        let sample = parse_sample(&mut scanner, data, columns_found, options);
        if sample.errors > 0 && !options.ignore_errors {
            continue;
        }
        let mut parsing = base.clone();
        let mut types: TypeCandidates = vec![type_candidates.clone(); columns_found];
        let start_detection = usize::from(sample.rows.len() > 1 && header_may_exist);
        sniff_types(
            &sample.rows,
            start_detection,
            &mut types,
            &mut parsing,
            &mut detection,
            options,
        );
        let varchar_columns = types
            .iter()
            .filter(|t| t.last().is_some_and(|ty| *ty == LogicalType::VARCHAR))
            .count();
        let better_strictness = !best_is_strict || !scanner.machine.used_unstrictness;
        let acceptable = varchar_columns < min_varchar && better_strictness;
        if best.is_none() || (acceptable && (!options.ignore_errors || sample.errors < min_errors)) {
            min_errors = sample.errors;
            min_varchar = varchar_columns;
            best_is_strict = !scanner.machine.used_unstrictness;
            let header_row = sample.rows.first().cloned().unwrap_or_default();
            best = Some(Best {
                scanner,
                candidate: index,
                types,
                parsing,
                header_row,
                single_row: sample.rows.len() == 1,
            });
        }
    }
    let Some(mut best) = best else {
        return Err(sniffer.sniffing_error(columns_found, true));
    };
    let chosen = &sniffer.candidates[best.candidate];
    let (candidate_skip, rows_until_header, dialect, num_cols) = (
        chosen.skip,
        chosen.rows_until_header,
        chosen.scanner.machine.dialect.clone(),
        chosen.columns,
    );

    // Type refinement
    let mut detected: Vec<LogicalType> = vec![LogicalType::VARCHAR; num_cols.max(columns_found)];
    if !options.all_varchar {
        let chunks = options.sample_chunks();
        let mut i = 1;
        while i < chunks && !best.scanner.finished() {
            let sample = parse_sample(&mut best.scanner, data, columns_found, options);
            for (column, types) in best.types.iter_mut().enumerate() {
                let is_bool = types.last().is_some_and(|t| *t == LogicalType::BOOLEAN);
                while types.len() > 1 {
                    let ty = types[types.len() - 1].clone();
                    let fits = sample.rows.iter().all(|row| {
                        row[column]
                            .as_deref()
                            .is_none_or(|text| can_cast(text, &ty, &best.parsing))
                    });
                    if fits {
                        break;
                    }
                    if is_bool && ty == LogicalType::BOOLEAN {
                        types.truncate(1);
                        break;
                    }
                    types.pop();
                }
            }
            i += 1;
        }
        let finished = best.scanner.finished();
        detected = best
            .types
            .iter()
            .map(|types| {
                if types.len() == type_candidates.len() && !finished {
                    LogicalType::VARCHAR
                } else {
                    types.last().cloned().unwrap_or(LogicalType::VARCHAR)
                }
            })
            .collect();
    }

    // Header detection
    let header_row = std::mem::take(&mut best.header_row);
    let total = columns_found;
    let (names, has_header) = detect_header(&sniffer, &header_row, &best.types, &best.parsing, total, &dialect)?;
    let mut skip = candidate_skip;
    if has_header && options.null_padding && options.skip.is_none() && skip > 0 {
        skip -= 1;
    }
    if (best.single_row && has_header) || sniffer.lines_sniffed == 0 {
        detected = vec![LogicalType::SQLNULL; names.len()];
    }
    detected.resize(names.len(), LogicalType::VARCHAR);
    // `SetResultOptions`: a format counts if a sniffed column uses it, and only then
    let found = |id: LogicalTypeId| detected.iter().any(|ty| ty.id() == id);
    let keep = set_types.is_none();
    let date_format = best.parsing.date_format.filter(|_| keep || found(LogicalTypeId::Date));
    let timestamp_format = best
        .parsing
        .timestamp_format
        .filter(|_| keep || found(LogicalTypeId::Timestamp));
    let mut mismatch = String::new();
    if let Some(set) = set_types {
        if detected.len() == set.len() && detected.iter().zip(set).any(|(a, b)| a != b) {
            mismatch += "The Column types set by the user do not match the ones found by the sniffer. \n";
            for (i, (found, wanted)) in detected.iter().zip(set).enumerate() {
                if found != wanted {
                    mismatch += &format!("Column at position: {i} Set type: {wanted} Sniffed type: {found}\n");
                }
            }
        }
        detected = set.to_vec();
    }
    let names = set_names.map_or(names, <[String]>::to_vec);
    Ok(Sniffed {
        dialect,
        header: has_header,
        names,
        types: detected,
        skip,
        rows_until_header,
        date_format: date_format.or_else(|| options.date_format.clone()),
        timestamp_format: timestamp_format.or_else(|| options.timestamp_format.clone()),
        mismatch,
    })
}

/// `DetectHeaderInternal`, without the names a user gave: the binder puts those over the result.
fn detect_header(
    sniffer: &Sniffer<'_>,
    header: &[HeaderValue],
    types: &TypeCandidates,
    parsing: &Parsing,
    total: usize,
    dialect: &Dialect,
) -> Result<(Vec<String>, bool)> {
    let options = sniffer.options;
    let generated = || {
        (0..total)
            .map(|column| generated_name(total, column))
            .collect::<Vec<_>>()
    };
    if header.is_empty() {
        return Ok((generated(), false));
    }
    if !options.null_padding && types.len() != header.len() {
        if options.ignore_errors {
            return Ok((generated(), false));
        }
        return Err(sniffer.header_error(header, types.len(), &dialect.delimiter));
    }
    let mut has_header = if let (Some(names), Some(set)) = (sniffer.set_names, sniffer.set_types) {
        detect_header_with_set_columns(header, names, set, parsing)
    } else {
        let (mut consistent, mut all_varchar, mut nulls) = (true, true, true);
        for (column, value) in header.iter().enumerate() {
            nulls &= value.is_none();
            let ty = types[column].last().cloned().unwrap_or(LogicalType::VARCHAR);
            if ty != LogicalType::VARCHAR {
                all_varchar = false;
                if value.as_deref().is_some_and(|text| !can_cast(text, &ty, parsing)) {
                    consistent = false;
                }
            }
        }
        all_varchar || nulls || !consistent
    };
    if let Some(header) = options.header {
        has_header = header;
    }
    if !has_header {
        return Ok((generated(), false));
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut names = Vec::with_capacity(total);
    for (column, value) in header.iter().enumerate() {
        let mut name = match value.as_deref() {
            Some(name) if !empty_header(Some(name), options.normalize_names) => name.to_string(),
            _ => generated_name(total, column),
        };
        name = if options.normalize_names {
            normalize_column_name(&name)
        } else {
            trim_whitespace(&name).to_string()
        };
        while let Some(count) = seen.get_mut(&name.to_lowercase()) {
            *count += 1;
            name = format!("{name}_{count}");
        }
        seen.insert(name.to_lowercase(), 0);
        names.push(name);
    }
    for column in header.len()..total {
        names.push(generated_name(total, column));
    }
    Ok((names, true))
}
