// Ported from duckdb/extension/core_functions/scalar and duckdb/src/function/scalar @ rust-port-baseline
//
//! The scalar functions that came late: their arguments decide their types, so they bind by name,
//! and each works a row at a time through `Value`s.

use super::nested::{bound_function, row_wise};
use crate::planner::expression::BoundExpr;
use duckrs_common::types::{Date, Interval, LogicalType, LogicalTypeId, Timestamp};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use unicode_segmentation::UnicodeSegmentation;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

fn text(value: &Value) -> Result<String> {
    value.try_to_string()
}

fn number(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Double(x) => Some(*x),
        ValueData::Float(x) => Some(f64::from(*x)),
        // a DECIMAL is its unscaled integer and its scale
        _ => {
            let unscaled = value.as_i128()? as f64;
            let scale = value.logical_type().decimal_properties().map_or(0, |(_, scale)| scale);
            Some(unscaled / 10f64.powi(i32::from(scale)))
        }
    }
}

const STRING_FUNCTIONS: [&str; 22] = [
    "hive_partition_component",
    "parse_filename",
    "parse_path",
    "parse_dirname",
    "parse_dirpath",
    "strip_accents",
    "nfc_normalize",
    "length_grapheme",
    "sha1",
    "sha256",
    "damerau_levenshtein",
    "jaccard",
    "jaro_similarity",
    "jaro_winkler_similarity",
    "to_base",
    "bar",
    "format_bytes",
    "formatreadablesize",
    "formatreadabledecimalsize",
    "url_encode",
    "url_decode",
    "path_join",
];

const OTHER_FUNCTIONS: [&str; 52] = [
    "finalize",
    "combine",
    "gamma",
    "lgamma",
    "nextafter",
    "fmod",
    "factorial",
    "even",
    "time_bucket",
    "normalized_interval",
    "uuid_extract_version",
    "uuid_extract_timestamp",
    "list_intersect",
    "array_intersect",
    "list_cosine_similarity",
    "array_cosine_similarity",
    "list_cosine_distance",
    "array_cosine_distance",
    "list_distance",
    "array_distance",
    "list_inner_product",
    "array_inner_product",
    "list_dot_product",
    "array_dot_product",
    "list_negative_inner_product",
    "array_negative_inner_product",
    "list_negative_dot_product",
    "array_negative_dot_product",
    "array_cross_product",
    "<->",
    "<=>",
    "struct_insert",
    "remap_struct",
    "equi_width_bins",
    "is_histogram_other_bin",
    "struct_update",
    "timetz_byte_comparable",
    "struct_contains",
    "struct_has",
    "struct_position",
    "struct_indexof",
    "struct_keys",
    "struct_values",
    "struct_concat",
    "map_extract",
    "element_at",
    "map_extract_value",
    "map_contains",
    "map_concat",
    "cast_to_type",
    "can_cast_implicitly",
    "from_binary",
];

/// Whether [`bind_function`] binds the function `name`.
pub fn is_extra_function(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    STRING_FUNCTIONS.contains(&lower.as_str()) || OTHER_FUNCTIONS.contains(&lower.as_str()) || lower == "unbin"
}

/// `parse_path` and its kin: the separator of an argument, as a set of characters.
fn separators(row: &[Value], at: usize) -> Result<&'static [char]> {
    match row.get(at) {
        None => Ok(&['/', '\\']),
        Some(value) => match value.try_to_string()?.as_str() {
            "system" | "forward_slash" => Ok(&['/']),
            "backslash" => Ok(&['\\']),
            "both_slash" => Ok(&['/', '\\']),
            other => Err(invalid(format!(
                "Invalid separator: {other}. Valid separators are: 'system', 'both_slash', 'backslash', \
                 'forward_slash'"
            ))),
        },
    }
}

fn parse_filename(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let path = text(&row[0])?;
        let (trim, separator) = match row.get(1).map(|v| v.data()) {
            Some(ValueData::Bool(trim)) => (*trim, 2),
            Some(_) => (false, 1),
            None => (false, 1),
        };
        let split = separators(row, separator)?;
        let name = path.rsplit(split).next().unwrap_or_default();
        let name = if trim {
            name.rsplit_once('.').map_or(name, |(stem, _)| stem)
        } else {
            name
        };
        Ok(Value::varchar(name))
    })
}

fn parse_path(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let list = LogicalType::list(LogicalType::VARCHAR);
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(list.clone()));
        }
        let path = text(&row[0])?;
        let split = separators(row, 1)?;
        let mut parts = Vec::new();
        if path.starts_with(split) {
            parts.push(Value::varchar(&path[..1]));
        }
        parts.extend(path.split(split).filter(|p| !p.is_empty()).map(Value::varchar));
        Value::list(LogicalType::VARCHAR, parts)
    })
}

fn parse_dirname(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let path = text(&row[0])?;
        let split = separators(row, 1)?;
        let parts: Vec<&str> = path.split(split).filter(|p| !p.is_empty()).collect();
        if path.starts_with(split) && parts.len() <= 1 {
            return Ok(Value::varchar(&path[..1]));
        }
        if parts.len() <= 1 {
            return Ok(Value::varchar(""));
        }
        Ok(Value::varchar(parts[parts.len() - 2]))
    })
}

fn parse_dirpath(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let path = text(&row[0])?;
        let split = separators(row, 1)?;
        let trimmed = path.trim_end_matches(split);
        Ok(match trimmed.rfind(split) {
            Some(0) => Value::varchar(&path[..1]),
            Some(at) => Value::varchar(trimmed[..at].trim_end_matches(split)),
            None => Value::varchar(""),
        })
    })
}

/// `PathJoinFunction`: each path joined onto the ones before it (`Path::Join`).
/// `HivePartitionComponentFunction`: `name=value`, escaped as the hive layout of COPY is.
fn hive_partition_component(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let name = if row[0].is_null() {
            "NULL".to_string()
        } else {
            row[0].try_to_string()?
        };
        let value = if row[1].is_null() {
            "__HIVE_DEFAULT_PARTITION__".to_string()
        } else {
            crate::execution::parquet::hive_escape_value(&row[1].try_to_string()?)
        };
        Ok(Value::varchar(format!(
            "{}={value}",
            crate::execution::parquet::hive_escape(&name)
        )))
    })
}

fn path_join(args: &DataChunk, result: &mut Vector) -> Result<()> {
    use duckrs_common::path::Path;
    row_wise(args, result, |row| {
        let mut parts = Vec::new();
        for value in row {
            if value.is_null() {
                return Ok(Value::null(LogicalType::VARCHAR));
            }
            match value.data() {
                ValueData::List(items) => {
                    for item in items {
                        parts.push(text(item)?);
                    }
                }
                _ => parts.push(text(value)?),
            }
        }
        let Some((first, rest)) = parts.split_first() else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let mut path = Path::from_string(first)?;
        for part in rest {
            path = path.join_str(part)?;
        }
        Ok(Value::varchar(path.to_path_string()))
    })
}

fn over_text(args: &DataChunk, result: &mut Vector, f: fn(&str) -> Result<Value>) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        f(&text(&row[0])?)
    })
}

fn strip_accents(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        Ok(Value::varchar(
            input.nfd().filter(|c| !is_combining_mark(*c)).collect::<String>(),
        ))
    })
}

fn nfc_normalize(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        Ok(Value::varchar(input.nfc().collect::<String>()))
    })
}

fn length_grapheme(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        Ok(Value::bigint(input.graphemes(true).count() as i64))
    })
}

fn bytes_of(value: &Value) -> Result<Vec<u8>> {
    match value.data() {
        ValueData::Bytes(bytes) => Ok(bytes.clone()),
        _ => Ok(text(value)?.into_bytes()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha1_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    use sha1::Digest;
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        Ok(Value::varchar(hex(&sha1::Sha1::digest(bytes_of(&row[0])?))))
    })
}

fn sha256_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    use sha2::Digest;
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        Ok(Value::varchar(hex(&sha2::Sha256::digest(bytes_of(&row[0])?))))
    })
}

/// `damerau_levenshtein`: true transpositions, over bytes as in C++.
/// The Damerau-Levenshtein distance of the characters of two strings.
fn damerau(a: &[char], b: &[char]) -> i64 {
    let (n, m) = (a.len(), b.len());
    let big = n + m;
    let mut table = vec![vec![0usize; m + 2]; n + 2];
    table[0][0] = big;
    for i in 0..=n {
        table[i + 1][0] = big;
        table[i + 1][1] = i;
    }
    for j in 0..=m {
        table[0][j + 1] = big;
        table[1][j + 1] = j;
    }
    // the last row each character was seen in
    let mut last: std::collections::HashMap<char, usize> = std::collections::HashMap::new();
    for i in 1..=n {
        let mut last_match = 0;
        for j in 1..=m {
            let (k, l) = (last.get(&b[j - 1]).copied().unwrap_or(0), last_match);
            let cost = usize::from(a[i - 1] != b[j - 1]);
            if cost == 0 {
                last_match = j;
            }
            table[i + 1][j + 1] = (table[i][j] + cost)
                .min(table[i + 1][j] + 1)
                .min(table[i][j + 1] + 1)
                .min(table[k][l] + (i - k - 1) + 1 + (j - l - 1));
        }
        last.insert(a[i - 1], i);
    }
    table[n + 1][m + 1] as i64
}

fn damerau_levenshtein(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::BIGINT));
        }
        let chars = |value: &Value| text(value).map(|s| s.chars().collect::<Vec<char>>());
        Ok(Value::bigint(damerau(&chars(&row[0])?, &chars(&row[1])?)))
    })
}

fn jaccard(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::DOUBLE));
        }
        let (a, b) = (text(&row[0])?, text(&row[1])?);
        if a.is_empty() || b.is_empty() {
            return Err(invalid("Jaccard Function: An argument too short!"));
        }
        let set = |s: &str| s.chars().collect::<std::collections::BTreeSet<char>>();
        let (a, b) = (set(&a), set(&b));
        let common = a.intersection(&b).count() as f64;
        let all = a.union(&b).count() as f64;
        Ok(Value::double(if all == 0.0 { 0.0 } else { common / all }))
    })
}

/// The Jaro similarity of the characters of two strings.
fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut taken = vec![false; b.len()];
    let mut matched_a = Vec::new();
    for (i, &c) in a.iter().enumerate() {
        let low = i.saturating_sub(window);
        let high = (i + window + 1).min(b.len());
        for j in low..high {
            if !taken[j] && b[j] == c {
                taken[j] = true;
                matched_a.push(c);
                break;
            }
        }
    }
    let matches = matched_a.len() as f64;
    if matches == 0.0 {
        return 0.0;
    }
    let matched_b = b.iter().zip(&taken).filter(|(_, t)| **t).map(|(c, _)| *c);
    let transpositions = matched_a.iter().zip(matched_b).filter(|(x, y)| **x != *y).count() as f64 / 2.0;
    (matches / a.len() as f64 + matches / b.len() as f64 + (matches - transpositions) / matches) / 3.0
}

fn jaro_winkler(a: &[char], b: &[char]) -> f64 {
    let similarity = jaro(a, b);
    if similarity <= 0.7 {
        return similarity;
    }
    let prefix = a.iter().zip(b).take(4).take_while(|(x, y)| x == y).count() as f64;
    similarity + prefix * 0.1 * (1.0 - similarity)
}

fn similarity(args: &DataChunk, result: &mut Vector, f: fn(&[char], &[char]) -> f64) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::DOUBLE));
        }
        let chars = |value: &Value| text(value).map(|s| s.chars().collect::<Vec<char>>());
        let score = f(&chars(&row[0])?, &chars(&row[1])?);
        let cutoff = row.get(2).and_then(number).unwrap_or(0.0);
        Ok(Value::double(if score >= cutoff { score } else { 0.0 }))
    })
}

fn jaro_similarity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    similarity(args, result, jaro)
}

fn jaro_winkler_similarity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    similarity(args, result, jaro_winkler)
}

fn to_base(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row.iter().any(Value::is_null) {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let mut value = row[0].as_i128().unwrap_or(0);
        let radix = row[1].as_i128().unwrap_or(0);
        let min_length = row.get(2).and_then(Value::as_i128).unwrap_or(0);
        if value < 0 {
            return Err(invalid("'to_base' number must be greater than or equal to 0"));
        }
        if !(2..=36).contains(&radix) {
            return Err(invalid("'to_base' radix must be between 2 and 36"));
        }
        if !(0..=64).contains(&min_length) {
            return Err(invalid("'to_base' min_length must be between 0 and 64"));
        }
        let mut digits = Vec::new();
        while value > 0 {
            digits.push(
                char::from_digit((value % radix) as u32, radix as u32)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
            value /= radix;
        }
        while (digits.len() as i128) < min_length.max(1) {
            digits.push('0');
        }
        Ok(Value::varchar(digits.iter().rev().collect::<String>()))
    })
}

/// `BarScalarFunction`: whole blocks, then the eighth of a block that is left over, padded to the
/// width with spaces.
fn bar(args: &DataChunk, result: &mut Vector) -> Result<()> {
    // `UnicodeBar::PartialBlocks`
    const PARTIAL_BLOCKS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let out_of_range = |message: &str| Error::new(ErrorKind::OutOfRange, message);
    row_wise(args, result, |row| {
        let values: Option<Vec<f64>> = row.iter().map(number).collect();
        let Some(values) = values else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let (x, min, max) = (values[0], values[1], values[2]);
        let max_width = values.get(3).copied().unwrap_or(80.0);
        if !max_width.is_finite() {
            return Err(out_of_range("Max bar width must not be NaN or infinity"));
        }
        if max_width < 1.0 {
            return Err(out_of_range("Max bar width must be >= 1"));
        }
        if max_width > 1000.0 {
            return Err(out_of_range("Max bar width must be <= 1000"));
        }
        let width = if x.is_nan() || min.is_nan() || max.is_nan() || x <= min {
            0.0
        } else if x >= max {
            max_width
        } else {
            max_width * (x - min) / (max - min)
        };
        if !width.is_finite() {
            return Err(out_of_range("Bar width must not be NaN or infinity"));
        }
        let eighths = (width * PARTIAL_BLOCKS.len() as f64) as u32 as usize;
        let full = eighths / PARTIAL_BLOCKS.len();
        let remaining = eighths % PARTIAL_BLOCKS.len();
        let mut out = "█".repeat(full);
        let mut used = full;
        if remaining > 0 {
            used += 1;
            out += PARTIAL_BLOCKS[remaining];
        }
        let integer_width = max_width as usize;
        if used < integer_width {
            out += &" ".repeat(integer_width - used);
        }
        Ok(Value::varchar(out))
    })
}

/// `StringUtil::BytesToHumanReadableString`
/// `StringUtil::BytesToHumanReadableString`: the largest unit with a whole amount, and the tenths
/// truncated.
fn bytes_to_human(bytes: u64, multiplier: u64) -> String {
    let units = if multiplier == 1000 {
        ["bytes", "kB", "MB", "GB", "TB", "PB"]
    } else {
        ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"]
    };
    let mut parts = [0u64; 6];
    parts[0] = bytes;
    for i in 1..6 {
        parts[i] = parts[i - 1] / multiplier;
        parts[i - 1] %= multiplier;
    }
    for i in (1..6).rev() {
        if parts[i] != 0 {
            let fractional = parts[i - 1] * 10 / multiplier;
            return format!("{}.{fractional} {}", parts[i], units[i]);
        }
    }
    format!("{}{}", parts[0], if bytes == 1 { " byte" } else { " bytes" })
}

/// `FormatBytesFunction`: a BIGINT, its sign in front.
fn format_bytes_with(args: &DataChunk, result: &mut Vector, multiplier: u64) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(bytes) = row[0].as_i128() else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let sign = if bytes < 0 { "-" } else { "" };
        Ok(Value::varchar(format!(
            "{sign}{}",
            bytes_to_human(bytes.unsigned_abs() as u64, multiplier)
        )))
    })
}

fn format_bytes(args: &DataChunk, result: &mut Vector) -> Result<()> {
    format_bytes_with(args, result, 1024)
}

fn format_decimal_bytes(args: &DataChunk, result: &mut Vector) -> Result<()> {
    format_bytes_with(args, result, 1000)
}

fn url_encode(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        let mut out = String::new();
        for byte in input.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
                _ => out += &format!("%{byte:02X}"),
            }
        }
        Ok(Value::varchar(out))
    })
}

fn url_decode(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let high = bytes.get(at + 1).and_then(|c| char::from(*c).to_digit(16));
            let low = bytes.get(at + 2).and_then(|c| char::from(*c).to_digit(16));
            match (bytes[at], high, low) {
                (b'%', Some(high), Some(low)) => {
                    out.push((high * 16 + low) as u8);
                    at += 3;
                }
                (b'+', ..) => {
                    out.push(b' ');
                    at += 1;
                }
                (byte, ..) => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        String::from_utf8(out)
            .map(Value::varchar)
            .map_err(|_| invalid("Failed to decode string: invalid UTF-8"))
    })
}

fn unary_double(args: &DataChunk, result: &mut Vector, f: fn(f64) -> f64) -> Result<()> {
    row_wise(args, result, |row| {
        Ok(number(&row[0]).map_or(Value::null(LogicalType::DOUBLE), |x| Value::double(f(x))))
    })
}

fn gamma(args: &DataChunk, result: &mut Vector) -> Result<()> {
    unary_double(args, result, libm::tgamma)
}

fn lgamma(args: &DataChunk, result: &mut Vector) -> Result<()> {
    unary_double(args, result, libm::lgamma)
}

fn even(args: &DataChunk, result: &mut Vector) -> Result<()> {
    unary_double(args, result, |x| {
        let away = x.abs().ceil().copysign(x);
        let rounded = (away / 2.0).trunc() * 2.0;
        if rounded == away {
            away
        } else {
            rounded + 2f64.copysign(x)
        }
    })
}

fn nextafter(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        match (row[0].data(), row[1].data()) {
            (ValueData::Float(x), ValueData::Float(y)) => Ok(Value::float(libm::nextafterf(*x, *y))),
            _ => Ok(Value::double(libm::nextafter(
                number(&row[0]).unwrap_or(f64::NAN),
                number(&row[1]).unwrap_or(f64::NAN),
            ))),
        }
    })
}

fn fmod(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let (Some(x), Some(y)) = (number(&row[0]), number(&row[1])) else {
            return Ok(Value::null(LogicalType::DOUBLE));
        };
        Ok(Value::double(x % y))
    })
}

fn factorial(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(n) = row[0].as_i128() else {
            return Ok(Value::null(LogicalType::HUGEINT));
        };
        if n < 0 {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                "factorial of a negative number is undefined",
            ));
        }
        let mut product: i128 = 1;
        for factor in 2..=n {
            product = product
                .checked_mul(factor)
                .ok_or_else(|| Error::new(ErrorKind::OutOfRange, "Overflow in HUGEINT multiplication"))?;
        }
        Ok(Value::hugeint(product))
    })
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// `range` and `generate_series` of dates and timestamps.
fn temporal_range(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let child = result
        .logical_type()
        .child_type()
        .cloned()
        .unwrap_or(LogicalType::TIMESTAMP);
    row_wise(args, result, |row| temporal_values(row, &child, false))
}

fn temporal_series(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let child = result
        .logical_type()
        .child_type()
        .cloned()
        .unwrap_or(LogicalType::TIMESTAMP);
    row_wise(args, result, |row| temporal_values(row, &child, true))
}

fn temporal_values(row: &[Value], child: &LogicalType, inclusive: bool) -> Result<Value> {
    if row.iter().any(Value::is_null) {
        return Ok(Value::null(LogicalType::list(child.clone())));
    }
    let (Some(start), Some(stop)) = (micros_of(&row[0]), micros_of(&row[1])) else {
        return Err(invalid("range needs a start and an end"));
    };
    let ValueData::Interval(step) = row[2].data() else {
        return Err(invalid("range needs an INTERVAL as its increment"));
    };
    let forward = step.months > 0 || step.days > 0 || step.micros > 0;
    let backward = step.months < 0 || step.days < 0 || step.micros < 0;
    if !forward && !backward {
        return Err(invalid("Interval step cannot be zero"));
    }
    if forward && backward {
        return Err(invalid("Interval with mix of negative/positive entries not supported"));
    }
    let mut items = Vec::new();
    let mut current = start;
    while (forward && (current < stop || (inclusive && current == stop)))
        || (backward && (current > stop || (inclusive && current == stop)))
    {
        items.push(if child.id() == LogicalTypeId::TimestampTz {
            Value::timestamp_tz(Timestamp(current))
        } else {
            Value::timestamp(Timestamp(current))
        });
        if items.len() > 100_000_000 {
            return Err(invalid("Lists larger than 2^32 elements are not supported"));
        }
        current = super::dates::add_interval(current, *step)?;
    }
    Value::list(child.clone(), items)
}

/// `range(start, stop, interval)`, or `None` if these are other arguments.
pub fn bind_temporal_range(
    name: &str,
    children: Vec<BoundExpr>,
) -> Result<std::result::Result<BoundExpr, Vec<BoundExpr>>> {
    let temporal = |ty: &LogicalType| {
        matches!(
            ty.id(),
            LogicalTypeId::Date | LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz
        )
    };
    if children.len() != 3 || !temporal(&children[0].return_type) {
        return Ok(Err(children));
    }
    let child = if children[0].return_type.id() == LogicalTypeId::TimestampTz {
        LogicalType::new(LogicalTypeId::TimestampTz)
    } else {
        LogicalType::TIMESTAMP
    };
    let lower = name.to_ascii_lowercase();
    let callback: super::ScalarCallback = if lower == "generate_series" {
        temporal_series
    } else {
        temporal_range
    };
    Ok(Ok(bound_function(&lower, children, LogicalType::list(child), callback)))
}

/// `time_bucket`'s origins: 2000-01-03 for days and less, 2000-01-01 for months.
fn time_bucket_value(width: Interval, micros: i64, origin: Option<i64>, offset: Option<Interval>) -> Result<i64> {
    let parts = i32::from(width.months != 0) + i32::from(width.days != 0 || width.micros != 0);
    if parts != 1 {
        return Err(invalid(
            "Only ICU supports time_bucket with month and day or time bucket widths",
        ));
    }
    let shift = offset.map_or(0, |o| i64::from(o.days) * MICROS_PER_DAY + o.micros);
    let micros = micros - shift;
    let bucketed = if width.months != 0 {
        if width.months < 0 {
            return Err(Error::new(ErrorKind::OutOfRange, "Period must be greater than 0"));
        }
        let default_origin = Date::from_ymd(2000, 1, 1).map_or(0, |d| i64::from(d.0) * MICROS_PER_DAY);
        let origin = origin.unwrap_or(default_origin);
        let month_index = |value: i64| {
            let (year, month, _) = Date(value.div_euclid(MICROS_PER_DAY) as i32).to_ymd();
            i64::from(year) * 12 + i64::from(month) - 1
        };
        let months = month_index(micros) - month_index(origin);
        let period = i64::from(width.months);
        let bucket = months.div_euclid(period) * period + month_index(origin);
        let (year, month) = (bucket.div_euclid(12), bucket.rem_euclid(12) + 1);
        Date::from_ymd(year as i32, month as i32, 1).map_or(0, |d| i64::from(d.0) * MICROS_PER_DAY)
    } else {
        let period = i64::from(width.days) * MICROS_PER_DAY + width.micros;
        if period <= 0 {
            return Err(Error::new(ErrorKind::OutOfRange, "Period must be greater than 0"));
        }
        let default_origin = Date::from_ymd(2000, 1, 3).map_or(0, |d| i64::from(d.0) * MICROS_PER_DAY);
        let origin = origin.unwrap_or(default_origin);
        (micros - origin).div_euclid(period) * period + origin
    };
    Ok(bucketed + shift)
}

fn micros_of(value: &Value) -> Option<i64> {
    match value.logical_type().id() {
        LogicalTypeId::Date => value.as_i128().map(|days| days as i64 * MICROS_PER_DAY),
        _ => value.as_i128().map(|micros| micros as i64),
    }
}

fn time_bucket(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let ValueData::Interval(width) = row[0].data() else {
            return Err(invalid("time_bucket needs an INTERVAL as its width"));
        };
        // an infinite date or timestamp is its own bucket, and an infinite origin has none
        let finite = |value: &Value| match value.logical_type().id() {
            LogicalTypeId::Date => value.as_i128().is_none_or(|days| Date(days as i32).is_finite()),
            LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz => value
                .as_i128()
                .is_none_or(|micros| Timestamp(micros as i64).is_finite()),
            _ => true,
        };
        if !finite(&row[1]) {
            let positive = row[1].as_i128().is_some_and(|v| v > 0);
            return Ok(match ty.id() {
                LogicalTypeId::Date => Value::date(if positive { Date::INFINITY } else { Date::NINFINITY }),
                id => {
                    let ts = if positive {
                        Timestamp::INFINITY
                    } else {
                        Timestamp::NINFINITY
                    };
                    if id == LogicalTypeId::TimestampTz {
                        Value::timestamp_tz(ts)
                    } else {
                        Value::timestamp(ts)
                    }
                }
            });
        }
        if row.get(2).is_some_and(|origin| !finite(origin)) {
            return Ok(Value::null(ty.clone()));
        }
        let micros = micros_of(&row[1]).unwrap_or(0);
        let (origin, offset) = match row.get(2).map(Value::data) {
            Some(ValueData::Interval(offset)) => (None, Some(*offset)),
            Some(_) => (micros_of(&row[2]), None),
            None => (None, None),
        };
        let bucket = time_bucket_value(*width, micros, origin, offset)?;
        Ok(match ty.id() {
            LogicalTypeId::Date => Value::date(Date(bucket.div_euclid(MICROS_PER_DAY) as i32)),
            // a TIME buckets as that time on 1970-01-01, and is the time of its bucket
            LogicalTypeId::Time => Value::time(duckrs_common::types::Time(bucket.rem_euclid(MICROS_PER_DAY))),
            LogicalTypeId::TimestampTz => Value::timestamp_tz(Timestamp(bucket)),
            _ => Value::timestamp(Timestamp(bucket)),
        })
    })
}

/// `TimeTZSortKeyFun`: the sort key of a TIME WITH TIME ZONE.
fn timetz_byte_comparable(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        Ok(match row[0].data() {
            ValueData::Int64(bits) => Value::ubigint(duckrs_common::types::TimeTz(*bits as u64).sort_key()),
            _ => Value::null(LogicalType::UBIGINT),
        })
    })
}

fn normalized_interval(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let ValueData::Interval(value) = row[0].data() else {
            return Ok(Value::null(LogicalType::INTERVAL));
        };
        let days = i64::from(value.days) + value.micros / MICROS_PER_DAY;
        let micros = value.micros % MICROS_PER_DAY;
        let months = i64::from(value.months) + days / 30;
        Ok(Value::interval(Interval {
            months: months as i32,
            days: (days % 30) as i32,
            micros,
        }))
    })
}

fn uuid_bytes(value: &Value) -> Option<[u8; 16]> {
    value
        .as_i128()
        .map(|stored| ((stored as u128) ^ (1 << 127)).to_be_bytes())
}

fn uuid_extract_version(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        Ok(uuid_bytes(&row[0]).map_or(Value::null(LogicalType::UINTEGER), |bytes| {
            Value::uinteger(u32::from(bytes[6] >> 4))
        }))
    })
}

fn uuid_extract_timestamp(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(bytes) = uuid_bytes(&row[0]) else {
            return Ok(Value::null(LogicalType::new(LogicalTypeId::TimestampTz)));
        };
        let version = bytes[6] >> 4;
        if version != 7 {
            return Err(invalid(format!("Given UUID is with version {version}, not version 7.")));
        }
        let millis = bytes[..6].iter().fold(0i64, |sum, b| sum << 8 | i64::from(*b));
        Ok(Value::timestamp_tz(Timestamp(millis * 1000)))
    })
}

fn list_values(value: &Value) -> Option<&[Value]> {
    match value.data() {
        ValueData::List(items) => Some(items),
        _ => None,
    }
}

fn list_intersect(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
    row_wise(args, result, |row| {
        // NULL on the left is NULL, and on the right has nothing in common with it
        let Some(left) = list_values(&row[0]) else {
            return Ok(Value::null(ty.clone()));
        };
        let right = list_values(&row[1]).unwrap_or(&[]);
        let mut kept: Vec<Value> = Vec::new();
        for item in left {
            if !item.is_null() && right.contains(item) && !kept.contains(item) {
                kept.push(item.clone());
            }
        }
        Value::new(ty.clone(), ValueData::List(kept)).or_else(|_| Value::list(child.clone(), Vec::new()))
    })
}

fn doubles(value: &Value, name: &str, side: &str) -> Result<Option<Vec<f64>>> {
    let Some(items) = list_values(value) else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(x) = number(item) else {
            return Err(invalid(format!("{name}: {side} argument can not contain NULL values")));
        };
        out.push(x);
    }
    Ok(Some(out))
}

fn pair_of_lists(row: &[Value], name: &str) -> Result<Option<(Vec<f64>, Vec<f64>)>> {
    let (Some(a), Some(b)) = (doubles(&row[0], name, "left")?, doubles(&row[1], name, "right")?) else {
        return Ok(None);
    };
    if a.len() != b.len() {
        return Err(invalid(format!(
            "{name}: list dimensions must be equal, got left length '{}' and right length '{}'",
            a.len(),
            b.len()
        )));
    }
    Ok(Some((a, b)))
}

fn inner(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn over_pairs(args: &DataChunk, result: &mut Vector, name: &'static str, f: fn(&[f64], &[f64]) -> f64) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let Some((a, b)) = pair_of_lists(row, name)? else {
            return Ok(Value::null(ty.clone()));
        };
        // `ALLOW_EMPTY`: the cosine of nothing is nothing
        if a.is_empty() && name.contains("cosine") {
            return Ok(Value::null(ty.clone()));
        }
        let score = f(&a, &b);
        Ok(if ty.id() == LogicalTypeId::Float {
            Value::float(score as f32)
        } else {
            Value::double(score)
        })
    })
}

/// `CosineSimilarityOp`: rescaled by the largest values when the sums overflow or underflow.
fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let clamp = |v: f64| if v.is_nan() { v } else { v.clamp(-1.0, 1.0) };
    let (distance, left, right) = (inner(a, b), inner(a, a), inner(b, b));
    let product = left * right;
    if distance.is_finite() && product.is_finite() && product >= f64::MIN_POSITIVE {
        return clamp(distance / product.sqrt());
    }
    let max = |v: &[f64]| v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    let (max_l, max_r) = (max(a), max(b));
    if max_l == 0.0 || max_r == 0.0 || !max_l.is_finite() || !max_r.is_finite() {
        return f64::NAN;
    }
    let a: Vec<f64> = a.iter().map(|x| x / max_l).collect();
    let b: Vec<f64> = b.iter().map(|x| x / max_r).collect();
    clamp(inner(&a, &b) / (inner(&a, &a) * inner(&b, &b)).sqrt())
}

fn cosine_similarity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_pairs(args, result, "list_cosine_similarity", cosine)
}

fn cosine_distance(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_pairs(args, result, "list_cosine_distance", |a, b| 1.0 - cosine(a, b))
}

fn distance(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_pairs(args, result, "list_distance", |a, b| {
        a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
    })
}

fn inner_product(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_pairs(args, result, "list_inner_product", inner)
}

fn negative_inner_product(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_pairs(args, result, "list_negative_inner_product", |a, b| -inner(a, b))
}

fn array_cross_product(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let child = ty.child_type().cloned().unwrap_or(LogicalType::DOUBLE);
    row_wise(args, result, |row| {
        let Some((a, b)) = pair_of_lists(row, "array_cross_product")? else {
            return Ok(Value::null(ty.clone()));
        };
        if a.len() != 3 {
            return Err(invalid("array_cross_product: the arrays must have 3 elements"));
        }
        let cross = [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let items = cross
            .iter()
            .map(|x| {
                if child.id() == LogicalTypeId::Float {
                    Value::float(*x as f32)
                } else {
                    Value::double(*x)
                }
            })
            .collect();
        Value::new(ty.clone(), ValueData::List(items))
    })
}

fn struct_fields(value: &Value) -> &[Value] {
    match value.data() {
        ValueData::Struct(fields) => fields,
        _ => &[],
    }
}

/// A STRUCT from all its arguments: what the result type says goes where.
fn rebuild_struct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let mut named: Vec<(String, Value)> = Vec::new();
        for value in row {
            let names = value.logical_type().struct_children();
            for ((name, _), field) in names.iter().zip(struct_fields(value)) {
                match named.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    Some(slot) => slot.1 = field.clone(),
                    None => named.push((name.clone(), field.clone())),
                }
            }
        }
        let mut fields = Vec::with_capacity(ty.struct_children().len());
        for (name, child) in ty.struct_children() {
            let found = named.iter().find(|(n, _)| n.eq_ignore_ascii_case(name));
            fields.push(found.map_or_else(|| Value::null(child.clone()), |(_, v)| v.clone()));
        }
        Value::new(ty.clone(), ValueData::Struct(fields))
    })
}

// Ported from duckdb/extension/core_functions/aggregate/nested/binned_histogram.cpp @ rust-port-baseline
/// `IsHistogramOtherBinFunction`: whether a value is the key of the "other values" bucket of a histogram.
fn is_histogram_other_bin(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = args.column(0).logical_type().clone();
    let other = super::holistic::other_bucket_value(&ty)?;
    row_wise(args, result, |row| {
        let Some(other) = &other else {
            // a NULL constant is NULL whatever the function
            if ty.id() == LogicalTypeId::SqlNull {
                return Ok(Value::null(LogicalType::BOOLEAN));
            }
            return Ok(Value::boolean(false));
        };
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        Ok(Value::boolean(super::holistic::not_distinct(&row[0], other, &ty)?))
    })
}

/// `struct_values`: the fields of a STRUCT, as a TUPLE.
fn struct_values(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        Value::new(ty.clone(), ValueData::Struct(struct_fields(&row[0]).to_vec()))
    })
}

fn struct_keys(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let list = LogicalType::list(LogicalType::VARCHAR);
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(list.clone()));
        }
        let names = row[0].logical_type().struct_children();
        Value::list(
            LogicalType::VARCHAR,
            names.iter().map(|(n, _)| Value::varchar(n.as_str())).collect(),
        )
    })
}

/// `StructContainsResolveTypes`: the value and the members it can be compared with are cast to
/// the type all of them can be cast to. The others never match.
fn struct_search_types(name: &str, mut children: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    let struct_type = children[0].return_type.clone();
    let members = struct_type.struct_children().to_vec();
    if struct_type.id() == LogicalTypeId::SqlNull || members.is_empty() {
        return Ok(children);
    }
    if struct_type.id() != LogicalTypeId::Tuple {
        return Err(binder_error(format!("\"{name}\" can only be used on unnamed structs")));
    }
    let mut target = children[1].return_type.clone();
    for (_, member) in &members {
        if let Some(max) = super::combine_types::try_max_logical_type(member, &target) {
            target = max;
        }
    }
    let cast: Vec<LogicalType> = members
        .iter()
        .map(
            |(_, member)| match super::combine_types::try_max_logical_type(member, &target) {
                Some(max) if max == target => target.clone(),
                _ => member.clone(),
            },
        )
        .collect();
    let value = children.pop().expect("two arguments").cast_to(&target, false);
    let row = children
        .pop()
        .expect("two arguments")
        .cast_to(&LogicalType::tuple(cast), false);
    Ok(vec![row, value])
}

/// `TemplatedStructSearch`: the position of the member that is the value, of those of its type,
/// and with `find_nulls` a NULL member for a NULL value.
fn struct_search(row: &[Value], find_nulls: bool) -> Option<Option<usize>> {
    if row[0].is_null() {
        return None;
    }
    let target = &row[1];
    let mut candidates = struct_fields(&row[0])
        .iter()
        .enumerate()
        .filter(|(_, member)| member.logical_type() == target.logical_type())
        .peekable();
    if target.is_null() && !find_nulls {
        return None;
    }
    if candidates.peek().is_none() {
        return Some(None);
    }
    Some(candidates.find_map(|(index, member)| {
        let found = if target.is_null() {
            member.is_null()
        } else {
            !member.is_null() && member == target
        };
        found.then_some(index)
    }))
}

fn struct_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        Ok(match struct_search(row, false) {
            None => Value::null(LogicalType::BOOLEAN),
            Some(found) => Value::boolean(found.is_some()),
        })
    })
}

fn struct_position(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        Ok(match struct_search(row, true).flatten() {
            Some(index) => Value::integer(index as i32 + 1),
            None => Value::null(LogicalType::INTEGER),
        })
    })
}

fn map_entries(value: &Value) -> &[Value] {
    match value.data() {
        ValueData::List(entries) => entries,
        _ => &[],
    }
}

fn map_find<'a>(map: &'a Value, key: &Value) -> Option<&'a Value> {
    map_entries(map).iter().find_map(|entry| {
        let parts = struct_fields(entry);
        (parts[0] == *key).then(|| &parts[1])
    })
}

fn map_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
        let found: Vec<Value> = map_find(&row[0], &row[1]).cloned().into_iter().collect();
        Value::list(child, found)
    })
}

fn map_extract_value(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        Ok(map_find(&row[0], &row[1])
            .cloned()
            .unwrap_or_else(|| Value::null(ty.clone())))
    })
}

/// `MapContainsFun`: `list_contains` of the keys, so an empty map has no NULL key either.
fn map_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        if map_entries(&row[0]).is_empty() {
            return Ok(Value::boolean(false));
        }
        if row[1].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        Ok(Value::boolean(map_find(&row[0], &row[1]).is_some()))
    })
}

fn map_concat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let mut entries: Vec<Value> = Vec::new();
        for map in row {
            for entry in map_entries(map) {
                let key = &struct_fields(entry)[0];
                match entries.iter().position(|e| struct_fields(e)[0] == *key) {
                    Some(at) => entries[at] = entry.clone(),
                    None => entries.push(entry.clone()),
                }
            }
        }
        Value::new(ty.clone(), ValueData::List(entries))
    })
}

fn from_binary(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_text(args, result, |input| {
        let digits = input.as_bytes();
        if digits.iter().any(|d| !matches!(d, b'0' | b'1')) {
            return Err(invalid(format!("Invalid input for binary digit: {input}")));
        }
        let mut out = vec![0u8; digits.len().div_ceil(8)];
        for (i, digit) in digits.iter().rev().enumerate() {
            if *digit == b'1' {
                let position = out.len() - 1 - i / 8;
                out[position] |= 1 << (i % 8);
            }
        }
        Ok(Value::blob(out))
    })
}

fn state_finalize(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = args.column(0).logical_type().clone();
    row_wise(args, result, |row| super::aggregates::finalize_state(&row[0], &ty))
}

fn state_combine(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = args.column(0).logical_type().clone();
    row_wise(args, result, |row| {
        // `AggregateStateCombine` combines the first state into the second
        if ty.is_aggregate_state() {
            return super::aggregates::combine_state_values(&row[1], &row[0], &ty);
        }
        super::aggregates::combine_state_values(&row[0], &row[1], &ty)
    })
}

fn count_args(name: &str, children: &[BoundExpr], low: usize, high: usize) -> Result<()> {
    if children.len() < low || children.len() > high {
        return Err(binder_error(format!(
            "No function matches the given name and argument types of '{name}'. You might need to add explicit type \
             casts."
        )));
    }
    Ok(())
}

fn casted(children: Vec<BoundExpr>, ty: &LogicalType) -> Vec<BoundExpr> {
    children.into_iter().map(|c| c.cast_to(ty, false)).collect()
}

/// A list or array of numbers, as a list of what they are computed in.
/// The overload of a distance function of `extension/core_functions/scalar/list/list_distance.cpp` and
/// `scalar/array/array_functions.cpp`: FLOAT for lists of FLOAT, DOUBLE otherwise.
fn vector_arguments(name: &str, children: Vec<BoundExpr>) -> Result<(Vec<BoundExpr>, LogicalType)> {
    count_args(name, &children, 2, 2)?;
    #[derive(PartialEq)]
    enum Fit {
        Ambiguous,
        Null,
        Float,
        Other,
        No,
    }
    let fit = |child: &BoundExpr| match super::binder::expression_return_type(child).id() {
        LogicalTypeId::StringLiteral | LogicalTypeId::Unknown => Fit::Ambiguous,
        LogicalTypeId::SqlNull => Fit::Null,
        LogicalTypeId::List | LogicalTypeId::Array => match child.return_type.child_type() {
            Some(ty) if ty.id() == LogicalTypeId::SqlNull => Fit::Null,
            Some(ty) if ty.id() == LogicalTypeId::Float => Fit::Float,
            Some(ty) if ty.is_numeric() => Fit::Other,
            _ => Fit::No,
        },
        _ => Fit::No,
    };
    let fits: Vec<Fit> = children.iter().map(fit).collect();
    if fits.contains(&Fit::No) {
        return Err(binder_error(format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
             casts.",
            children
                .iter()
                .map(|c| super::binder::expression_return_type(c).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if fits.contains(&Fit::Ambiguous) {
        return Err(binder_error(format!(
            "{name}: Could not choose a best candidate function"
        )));
    }
    // `ArrayGenericBinaryBind`: arrays of one size
    if name.starts_with("array_") {
        let sizes: Vec<Option<u32>> = children.iter().map(|c| c.return_type.array_size()).collect();
        if let [Some(left), Some(right)] = sizes.as_slice()
            && left != right
        {
            return Err(binder_error(format!(
                "{name}: Array arguments must be of the same size"
            )));
        }
    }
    let ty = if fits.contains(&Fit::Other) {
        LogicalType::DOUBLE
    } else {
        LogicalType::FLOAT
    };
    Ok((children, ty))
}

/// Binds a function of [`is_extra_function`]; `names` are the names of its arguments.
pub fn bind_function(name: &str, names: &[String], mut children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = match name.to_ascii_lowercase().as_str() {
        "<->" => "list_distance".to_string(),
        "<=>" => "list_cosine_distance".to_string(),
        other => other.to_string(),
    };
    let (v, d) = (LogicalType::VARCHAR, LogicalType::DOUBLE);
    let list_of_text = LogicalType::list(LogicalType::VARCHAR);
    let simple = |low, high, ty: LogicalType, callback: super::ScalarCallback, children: Vec<BoundExpr>| {
        count_args(&lower, &children, low, high)?;
        Ok(bound_function(&lower, children, ty, callback))
    };
    match lower.as_str() {
        "parse_filename" => simple(1, 3, v, parse_filename, children),
        "parse_path" => simple(1, 2, list_of_text, parse_path, children),
        "parse_dirname" => simple(1, 2, v, parse_dirname, children),
        "parse_dirpath" => simple(1, 2, v, parse_dirpath, children),
        "path_join" => simple(1, usize::MAX, v, path_join, children),
        "hive_partition_component" => {
            count_args(&lower, &children, 2, 2)?;
            let mut children = children;
            children[0] = children[0].clone().cast_to(&v, false);
            Ok(bound_function(&lower, children, v, hive_partition_component))
        }
        "strip_accents" => simple(1, 1, v, strip_accents, children),
        "nfc_normalize" => simple(1, 1, v, nfc_normalize, children),
        "length_grapheme" => simple(1, 1, LogicalType::BIGINT, length_grapheme, children),
        "sha1" | "sha256" => {
            count_args(&lower, &children, 1, 1)?;
            let id = children[0].return_type.id();
            if !matches!(
                id,
                LogicalTypeId::Varchar | LogicalTypeId::Blob | LogicalTypeId::SqlNull
            ) {
                return Err(binder_error(format!(
                    "No function matches the given name and argument types '{lower}({})'. You might need to add \
                     explicit type casts.",
                    children[0].return_type
                )));
            }
            let callback: super::ScalarCallback = if lower == "sha1" {
                sha1_function
            } else {
                sha256_function
            };
            Ok(bound_function(&lower, children, v, callback))
        }
        "damerau_levenshtein" => simple(2, 2, LogicalType::BIGINT, damerau_levenshtein, children),
        "jaccard" => simple(2, 2, d, jaccard, children),
        "jaro_similarity" => simple(2, 3, d, jaro_similarity, children),
        "jaro_winkler_similarity" => simple(2, 3, d, jaro_winkler_similarity, children),
        "to_base" => simple(2, 3, v, to_base, children),
        "bar" => simple(3, 4, v, bar, children),
        "format_bytes" | "formatreadablesize" => simple(1, 1, v, format_bytes, children),
        "formatreadabledecimalsize" => simple(1, 1, v, format_decimal_bytes, children),
        "url_encode" => simple(1, 1, v, url_encode, children),
        "url_decode" => simple(1, 1, v, url_decode, children),
        "from_binary" | "unbin" => simple(1, 1, LogicalType::BLOB, from_binary, children),
        "gamma" => simple(1, 1, d.clone(), gamma, casted(children, &d)),
        "lgamma" => simple(1, 1, d.clone(), lgamma, casted(children, &d)),
        "even" => simple(1, 1, d.clone(), even, casted(children, &d)),
        "fmod" => simple(2, 2, d.clone(), fmod, casted(children, &d)),
        "factorial" => simple(
            1,
            1,
            LogicalType::HUGEINT,
            factorial,
            casted(children, &LogicalType::INTEGER),
        ),
        "nextafter" => {
            let float = children.iter().all(|c| c.return_type.id() == LogicalTypeId::Float);
            let ty = if float { LogicalType::FLOAT } else { d };
            simple(2, 2, ty.clone(), nextafter, casted(children, &ty))
        }
        "time_bucket" => {
            count_args(&lower, &children, 2, 3)?;
            let ty = children[1].return_type.clone();
            if !matches!(
                ty.id(),
                LogicalTypeId::Date | LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz | LogicalTypeId::Time
            ) {
                return Err(binder_error(format!(
                    "No function matches the given name and argument types 'time_bucket(INTERVAL, {ty})'."
                )));
            }
            Ok(bound_function(&lower, children, ty, time_bucket))
        }
        "normalized_interval" => simple(1, 1, LogicalType::INTERVAL, normalized_interval, children),
        "timetz_byte_comparable" => simple(1, 1, LogicalType::UBIGINT, timetz_byte_comparable, children),
        // their argument is a UUID
        "uuid_extract_version" | "uuid_extract_timestamp" => {
            count_args(&lower, &children, 1, 1)?;
            let children = vec![
                children[0]
                    .clone()
                    .cast_to(&LogicalType::new(LogicalTypeId::Uuid), false),
            ];
            if lower == "uuid_extract_version" {
                Ok(bound_function(
                    &lower,
                    children,
                    LogicalType::UINTEGER,
                    uuid_extract_version,
                ))
            } else {
                let ty = LogicalType::new(LogicalTypeId::TimestampTz);
                Ok(bound_function(&lower, children, ty, uuid_extract_timestamp))
            }
        }
        "list_intersect" | "array_intersect" => {
            count_args(&lower, &children, 2, 2)?;
            // `LIST(T)` twice: the elements meet in one type
            let elements: Vec<LogicalType> = children
                .iter()
                .map(|c| match c.return_type.id() {
                    LogicalTypeId::List | LogicalTypeId::Array => c.return_type.child_type().cloned(),
                    LogicalTypeId::SqlNull => Some(LogicalType::SQLNULL),
                    _ => None,
                })
                .collect::<Option<_>>()
                .ok_or_else(|| super::lists::no_match(&lower, &children))?;
            let element = super::combine_types::try_max_logical_type(&elements[0], &elements[1])
                .ok_or_else(|| super::lists::no_match(&lower, &children))?;
            let ty = LogicalType::list(element);
            let children = children.into_iter().map(|c| c.cast_to(&ty, false)).collect();
            Ok(bound_function(&lower, children, ty, list_intersect))
        }
        "list_cosine_similarity"
        | "array_cosine_similarity"
        | "list_cosine_distance"
        | "array_cosine_distance"
        | "list_distance"
        | "array_distance"
        | "list_inner_product"
        | "array_inner_product"
        | "list_dot_product"
        | "array_dot_product"
        | "list_negative_inner_product"
        | "array_negative_inner_product"
        | "list_negative_dot_product"
        | "array_negative_dot_product" => {
            let (children, ty) = vector_arguments(&lower, children)?;
            let callback: super::ScalarCallback = match lower.trim_start_matches("list_").trim_start_matches("array_") {
                "cosine_similarity" => cosine_similarity,
                "cosine_distance" => cosine_distance,
                "distance" => distance,
                "negative_inner_product" | "negative_dot_product" => negative_inner_product,
                _ => inner_product,
            };
            Ok(bound_function(&lower, children, ty, callback))
        }
        // `ArrayCrossProductFun`: FLOAT[3] or DOUBLE[3]
        "array_cross_product" => {
            let (children, element) = vector_arguments(&lower, children)?;
            let ty = LogicalType::array(element, 3);
            let children = children.into_iter().map(|c| c.cast_to(&ty, false)).collect();
            Ok(bound_function(&lower, children, ty, array_cross_product))
        }
        "remap_struct" => super::remap_struct::bind(children),
        "equi_width_bins" => super::binning::bind(children),
        "is_histogram_other_bin" => simple(1, 1, LogicalType::BOOLEAN, is_histogram_other_bin, children),
        "struct_insert" | "struct_update" => {
            if children.is_empty() || children[0].return_type.id() != LogicalTypeId::Struct {
                return Err(binder_error(format!("{lower} expects a STRUCT as its first argument")));
            }
            let mut fields = children[0].return_type.struct_children().to_vec();
            let added: Vec<(String, BoundExpr)> = names.iter().skip(1).cloned().zip(children.drain(1..)).collect();
            let mut packed = Vec::with_capacity(added.len());
            for (name, child) in added {
                if name.is_empty() {
                    return Err(binder_error(format!("{lower}: arguments must be named")));
                }
                match fields.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    Some(_) if lower == "struct_insert" => {
                        return Err(binder_error(format!("Duplicate struct entry name \"{name}\"")));
                    }
                    Some(at) => fields[at].1 = child.return_type.clone(),
                    None => fields.push((name.clone(), child.return_type.clone())),
                }
                packed.push((name, child));
            }
            let (names, values): (Vec<String>, Vec<BoundExpr>) = packed.into_iter().unzip();
            let extra = super::nested::bind_struct_pack(names, values)?;
            children.push(extra);
            Ok(bound_function(
                &lower,
                children,
                LogicalType::struct_type(fields),
                rebuild_struct,
            ))
        }
        "struct_concat" => {
            if children.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "struct_concat: At least one argument is required",
                ));
            }
            let mut fields: Vec<(String, LogicalType)> = Vec::new();
            for child in &children {
                if child.return_type.id() != LogicalTypeId::Struct {
                    return Err(binder_error("struct_concat expects STRUCTs"));
                }
                for (name, ty) in child.return_type.struct_children() {
                    if fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                        return Err(binder_error(format!("Duplicate struct entry name \"{name}\"")));
                    }
                    fields.push((name.clone(), ty.clone()));
                }
            }
            Ok(bound_function(
                &lower,
                children,
                LogicalType::struct_type(fields),
                rebuild_struct,
            ))
        }
        "struct_keys" => {
            if children
                .first()
                .is_some_and(|c| !matches!(c.return_type.id(), LogicalTypeId::Struct | LogicalTypeId::Unknown))
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "struct_keys() expects a STRUCT argument",
                ));
            }
            simple(1, 1, list_of_text, struct_keys, children)
        }
        // Ported from duckdb/extension/core_functions/scalar/struct/struct_values.cpp @ rust-port-baseline
        "struct_values" => {
            count_args(&lower, &children, 1, 1)?;
            let ty = children[0].return_type.clone();
            match ty.id() {
                LogicalTypeId::SqlNull => Ok(BoundExpr::constant(Value::sql_null())),
                LogicalTypeId::Struct | LogicalTypeId::Tuple => {
                    let tuple = LogicalType::tuple(ty.struct_children().iter().map(|(_, t)| t.clone()).collect());
                    Ok(bound_function(&lower, children, tuple, struct_values))
                }
                _ => Err(binder_error(format!(
                    "No function matches the given name and argument types 'struct_values({})'. You might need \
                     to add explicit type casts.\n\tCandidate functions:\n\tstruct_values(STRUCT) -> TUPLE\n",
                    super::binder::expression_return_type(&children[0])
                ))),
            }
        }
        "struct_contains" | "struct_has" | "struct_position" | "struct_indexof" => {
            count_args(&lower, &children, 2, 2)?;
            let position = matches!(lower.as_str(), "struct_position" | "struct_indexof");
            let (ty, function): (LogicalType, super::ScalarCallback) = if position {
                (LogicalType::INTEGER, struct_position)
            } else {
                (LogicalType::BOOLEAN, struct_contains)
            };
            let children = struct_search_types(&lower, children)?;
            Ok(bound_function(&lower, children, ty, function))
        }
        "map_extract" | "element_at" | "map_extract_value" | "map_contains" => {
            count_args(&lower, &children, 2, 2)?;
            let map = children[0].return_type.clone();
            let entry = map.child_type().cloned();
            let Some(entry) = entry.filter(|_| map.id() == LogicalTypeId::Map) else {
                return Err(binder_error(format!("{lower} expects a MAP as its first argument")));
            };
            let parts = entry.struct_children();
            let (key, value) = (parts[0].1.clone(), parts[1].1.clone());
            // the key of an empty map is no type to cast to
            let wanted = children.remove(1);
            let wanted = if key.id() == LogicalTypeId::SqlNull {
                wanted
            } else {
                wanted.cast_to(&key, false)
            };
            children.push(wanted);
            Ok(match lower.as_str() {
                "map_contains" => bound_function(&lower, children, LogicalType::BOOLEAN, map_contains),
                "map_extract_value" => bound_function(&lower, children, value, map_extract_value),
                _ => bound_function(&lower, children, LogicalType::list(value), map_extract),
            })
        }
        // `MapConcatBind`: the type of the maps that are neither NULL nor empty
        "map_concat" => {
            if children.len() < 2 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "The provided amount of arguments is incorrect, please provide 2 or more maps",
                ));
            }
            let mut expected = LogicalType::SQLNULL;
            let mut is_null = true;
            for child in &children {
                let map = &child.return_type;
                if map.id() == LogicalTypeId::SqlNull {
                    continue;
                }
                if map.id() != LogicalTypeId::Map {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "MAP_CONCAT only takes map arguments",
                    ));
                }
                is_null = false;
                let empty = map.child_type().is_some_and(|entry| {
                    entry
                        .struct_children()
                        .iter()
                        .all(|(_, t)| t.id() == LogicalTypeId::SqlNull)
                });
                if empty {
                    continue;
                }
                if expected.id() == LogicalTypeId::SqlNull {
                    expected = map.clone();
                } else if *map != expected {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "'value' type of map differs between arguments, expected '{expected}', found '{map}' instead"
                        ),
                    ));
                }
            }
            if expected.id() == LogicalTypeId::SqlNull && !is_null {
                expected = LogicalType::map(LogicalType::SQLNULL, LogicalType::SQLNULL);
            }
            if expected.id() == LogicalTypeId::SqlNull {
                return Ok(BoundExpr::constant(Value::sql_null()));
            }
            Ok(bound_function(
                &lower,
                casted(children, &expected),
                expected,
                map_concat,
            ))
        }
        "finalize" => {
            count_args(&lower, &children, 1, 1)?;
            let ty = children[0].return_type.clone();
            let Some((.., result)) = ty.aggregate_state_info() else {
                return Err(binder_error(format!("Can only finalize {ty}, not AGGREGATE_STATE")));
            };
            let result = result.clone();
            Ok(bound_function(&lower, children, result, state_finalize))
        }
        "combine" => {
            count_args(&lower, &children, 2, 2)?;
            let ty = children[0].return_type.clone();
            let other = children[1].return_type.clone();
            if ty.aggregate_state_info().is_none() {
                return Err(binder_error(format!("Can only combine {ty}, not AGGREGATE_STATE")));
            }
            if other.id() != LogicalTypeId::SqlNull && ty != other {
                let name = |t: &LogicalType| t.aggregate_state_info().map_or(t.to_string(), |i| i.0.to_string());
                return Err(binder_error(format!(
                    "Cannot COMBINE aggregate states from different functions, {} <> {}",
                    name(&ty),
                    name(&other)
                )));
            }
            Ok(bound_function(&lower, children, ty, state_combine))
        }
        "cast_to_type" => {
            count_args(&lower, &children, 2, 2)?;
            let ty = children[1].return_type.clone();
            Ok(children.remove(0).cast_to(&ty, false))
        }
        "can_cast_implicitly" => {
            count_args(&lower, &children, 2, 2)?;
            let cost = super::cast_rules::implicit_cast(&children[0].return_type, &children[1].return_type);
            Ok(BoundExpr::constant(Value::boolean(cost >= 0)))
        }
        _ => Err(binder_error(format!("{lower} is not bound here"))),
    }
}
