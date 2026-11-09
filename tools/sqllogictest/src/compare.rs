// Ported from duckdb/test/sqlite/result_helper.cpp @ rust-port-baseline.

use crate::engine::{ColumnType, QueryOutput};
use crate::parser::SortStyle;
use md5::{Digest, Md5};
use regex::RegexBuilder;

/// Converts one cell the way `SQLLogicTestConvertValue` does.
pub fn convert_value(cell: Option<&str>, ty: ColumnType, original_sqlite_test: bool) -> String {
    let Some(value) = cell else {
        return "NULL".to_string();
    };
    if original_sqlite_test
        && matches!(ty, ColumnType::Float | ColumnType::Double | ColumnType::Decimal { .. })
        && let Ok(f) = value.trim().parse::<f64>()
    {
        // DuckDB's DOUBLE -> BIGINT cast rounds half away from zero.
        return format!("{}", f.round() as i64);
    }
    match ty {
        ColumnType::Boolean => if value == "true" { "1" } else { "0" }.to_string(),
        _ if value.is_empty() => "(empty)".to_string(),
        _ => value.replace('\0', "\\0"),
    }
}

/// Converts a whole result into row-major strings (`DuckDBConvertResult`).
pub fn convert_result(output: &QueryOutput, original_sqlite_test: bool) -> Vec<String> {
    let mut values = Vec::with_capacity(output.row_count() * output.column_count());
    for row in &output.rows {
        for (cell, ty) in row.iter().zip(&output.types) {
            values.push(convert_value(cell.as_deref(), *ty, original_sqlite_test));
        }
    }
    values
}

pub fn sort_values(style: SortStyle, values: &mut Vec<String>, ncols: usize) -> Result<(), String> {
    match style {
        SortStyle::NoSort => Ok(()),
        SortStyle::ValueSort => {
            values.sort();
            Ok(())
        }
        SortStyle::RowSort => {
            if ncols == 0 || !values.len().is_multiple_of(ncols) {
                return Err(format!(
                    "Failed to sort query result - result is not aligned. Found {} rows with {} columns",
                    values.len(),
                    ncols
                ));
            }
            let mut rows: Vec<Vec<String>> = values.chunks(ncols).map(<[String]>::to_vec).collect();
            rows.sort();
            *values = rows.into_iter().flatten().collect();
            Ok(())
        }
    }
}

pub fn hash_values(values: &[String]) -> String {
    let mut md5 = Md5::new();
    for v in values {
        md5.update(v.as_bytes());
        md5.update(b"\n");
    }
    let digest = md5.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{} values hashing to {hex}", values.len())
}

pub fn result_is_hash(s: &str) -> bool {
    const MIDDLE: &str = " values hashing to ";
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || s.len() != digits + MIDDLE.len() + 32 {
        return false;
    }
    let rest = &s[digits..];
    rest.starts_with(MIDDLE)
        && rest[MIDDLE.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

pub fn is_regex_comparison(expected: &str) -> bool {
    expected.starts_with("<REGEX>:") || expected.starts_with("<!REGEX>:")
}

/// `<REGEX>:pattern` must fully match; `<!REGEX>:pattern` must not. `.` also matches newlines.
pub fn matches_regex(actual: &str, expected: &str) -> Result<bool, String> {
    let want_match = expected.starts_with("<REGEX>:");
    let pattern = literal_braces(&expected.replace("<REGEX>:", "").replace("<!REGEX>:", ""));
    let regex = RegexBuilder::new(&format!("^(?:{pattern})$"))
        .dot_matches_new_line(true)
        .build()
        .map_err(|e| format!("Failed to parse regex: {e}"))?;
    Ok(regex.is_match(actual) == want_match)
}

/// Braces that are no repetition, as `std::regex` reads them: literal, where the regex crate rejects them.
fn literal_braces(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut result = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                result.push(c);
                result.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => {
                // `{n}`, `{n,}` or `{n,m}` after something to repeat
                let end = chars[i..].iter().position(|&c| c == '}').map(|e| i + e);
                let quantifier = i > 0
                    && end.is_some_and(|end| {
                        let inner: String = chars[i + 1..end].iter().collect();
                        let mut parts = inner.splitn(2, ',');
                        let first = parts.next().unwrap_or("");
                        !first.is_empty()
                            && first.bytes().all(|b| b.is_ascii_digit())
                            && parts.next().is_none_or(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
                    });
                if quantifier {
                    let end = end.unwrap_or(i);
                    result.extend(&chars[i..=end]);
                    i = end + 1;
                    continue;
                }
                result.push_str("\\{");
                i += 1;
                continue;
            }
            '}' if !in_class => {
                result.push_str("\\}");
                i += 1;
                continue;
            }
            _ => {}
        }
        result.push(c);
        i += 1;
    }
    result
}

/// `ApproxEqual` from duckdb/src/common/types.cpp. `expected` sets the tolerance.
fn approx_equal(actual: f64, expected: f64) -> bool {
    if actual.is_nan() && expected.is_nan() {
        return true;
    }
    if !actual.is_finite() || !expected.is_finite() {
        return actual == expected;
    }
    let epsilon = expected.abs() * 0.01 + 0.000_000_01;
    (actual - expected).abs() <= epsilon
}

fn parse_double(s: &str) -> Option<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" | "-nan" => Some(f64::NAN),
        "inf" | "infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => t.parse().ok(),
    }
}

/// Parses a string into an integer the way a VARCHAR -> integer cast does: decimals round.
fn parse_integer(s: &str) -> Option<i128> {
    let t = s.trim();
    if let Ok(i) = t.parse::<i128>() {
        return Some(i);
    }
    parse_scaled(t, 0)
}

/// Parses a decimal string into an integer scaled by 10^scale, rounding half away from zero.
fn parse_scaled(s: &str, scale: u8) -> Option<i128> {
    let t = s.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.contains(['e', 'E']) {
        let f: f64 = t.parse().ok()?;
        return Some((f * 10f64.powi(i32::from(scale))).round() as i128);
    }
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = usize::from(scale);
    let mut value: i128 = 0;
    for b in int_part
        .bytes()
        .chain(frac_part.bytes().chain(std::iter::repeat(b'0')).take(scale))
    {
        value = value.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
    }
    if frac_part.as_bytes().get(scale).is_some_and(|b| *b >= b'5') {
        value = value.checked_add(1)?;
    }
    Some(if negative { -value } else { value })
}

/// Compares one result value with one expected value (`CompareValues`).
pub fn values_equal(actual: &str, expected: &str, ty: ColumnType, replace_keywords: impl Fn(&str) -> String) -> bool {
    if actual == expected || actual == replace_keywords(expected) {
        return true;
    }
    if is_regex_comparison(expected) && matches_regex(actual, expected).unwrap_or(false) {
        return true;
    }
    if ty.is_numeric() {
        let null = |s: &str| s == "NULL";
        if null(actual) || null(expected) {
            return null(actual) && null(expected);
        }
        return match ty {
            ColumnType::Integer => {
                matches!((parse_integer(actual), parse_integer(expected)), (Some(a), Some(b)) if a == b)
            }
            ColumnType::Decimal { scale } => {
                matches!((parse_scaled(actual, scale), parse_scaled(expected, scale)), (Some(a), Some(b)) if a == b)
            }
            ColumnType::Float => match (parse_double(actual), parse_double(expected)) {
                (Some(a), Some(b)) => approx_equal(f64::from(a as f32), f64::from(b as f32)),
                _ => false,
            },
            _ => match (parse_double(actual), parse_double(expected)) {
                (Some(a), Some(b)) => approx_equal(a, b),
                _ => false,
            },
        };
    }
    if ty == ColumnType::Boolean {
        let as_bool = |s: &str| match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        };
        return as_bool(actual) == as_bool(expected);
    }
    false
}

/// Reads an expected-result file referenced with `<FILE>:path` (a `|`-separated CSV with a
/// header). Empty fields are NULL.
pub fn load_result_file(path: &str) -> Result<(Vec<String>, usize), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Could not read CSV File \"{path}\": {e}"))?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("Could not read CSV File \"{path}\": empty file"))?;
    let column_count = split_csv_line(header).len();
    let mut values = Vec::new();
    for line in lines {
        let fields = split_csv_line(line);
        if fields.len() != column_count {
            return Err(format!(
                "Could not read CSV File \"{path}\": expected {column_count} columns, found {}",
                fields.len()
            ));
        }
        values.extend(fields.into_iter().map(|f| f.unwrap_or_else(|| "NULL".to_string())));
    }
    Ok((values, column_count))
}

fn split_csv_line(line: &str) -> Vec<Option<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '|' if !quoted => {
                fields.push(finish_field(&mut current, &mut was_quoted));
            }
            _ => current.push(c),
        }
    }
    fields.push(finish_field(&mut current, &mut was_quoted));
    fields
}

fn finish_field(current: &mut String, was_quoted: &mut bool) -> Option<String> {
    let field = std::mem::take(current);
    let quoted = std::mem::take(was_quoted);
    if field.is_empty() && !quoted { None } else { Some(field) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn converts_values_like_the_cpp_runner() {
        assert_eq!(convert_value(None, ColumnType::Integer, false), "NULL");
        assert_eq!(convert_value(Some(""), ColumnType::Other, false), "(empty)");
        assert_eq!(convert_value(Some("true"), ColumnType::Boolean, false), "1");
        assert_eq!(convert_value(Some("a\0b"), ColumnType::Other, false), "a\\0b");
        assert_eq!(convert_value(Some("2.5"), ColumnType::Double, true), "3");
    }

    #[test]
    fn hashes_match_duckdb() {
        // From `SELECT 42` under `query I` with hash-threshold 0: md5("42\n").
        assert_eq!(
            hash_values(&["42".into()]),
            "1 values hashing to 50a2fabfdd276f573ff97ace8b11c5f4"
        );
        assert!(result_is_hash("1 values hashing to 50a2fabfdd276f573ff97ace8b11c5f4"));
        assert!(!result_is_hash("values hashing to 50a2fabfdd276f573ff97ace8b11c5f4"));
    }

    #[test]
    fn numeric_comparisons() {
        assert!(values_equal("3.0001", "3", ColumnType::Double, same));
        assert!(!values_equal("3.1", "3", ColumnType::Double, same));
        assert!(values_equal("1.25", "1.3", ColumnType::Decimal { scale: 1 }, same));
        assert!(values_equal("10", "10.0", ColumnType::Integer, same));
        assert!(values_equal("true", "1", ColumnType::Boolean, same));
        assert!(!values_equal("abc", "abd", ColumnType::Other, same));
        assert!(values_equal("NULL", "NULL", ColumnType::Integer, same));
    }

    #[test]
    fn regex_matches_fully() {
        assert!(
            matches_regex(
                "Binder Error: column x not found\n",
                "<REGEX>:Binder Error:.*not found.*"
            )
            .unwrap()
        );
        assert!(!matches_regex("abc", "<REGEX>:b").unwrap());
        assert!(matches_regex("abc", "<!REGEX>:b").unwrap());
    }

    #[test]
    fn row_sort() {
        let mut v: Vec<String> = ["b", "1", "a", "2"].map(String::from).to_vec();
        sort_values(SortStyle::RowSort, &mut v, 2).unwrap();
        assert_eq!(v, ["a", "2", "b", "1"]);
    }

    #[test]
    fn csv_fields() {
        assert_eq!(
            split_csv_line(r#"a|"b|c"||"""#),
            [Some("a".into()), Some("b|c".into()), None, Some("".into())]
        );
    }
}
