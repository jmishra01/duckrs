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
        _ => value.as_i128().map(|x| x as f64),
    }
}

const STRING_FUNCTIONS: [&str; 21] = [
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

const OTHER_FUNCTIONS: [&str; 46] = [
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
    "struct_insert",
    "struct_update",
    "timetz_byte_comparable",
    "struct_contains",
    "struct_has",
    "struct_position",
    "struct_indexof",
    "struct_keys",
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

fn path_join(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let mut joined = String::new();
        for value in row {
            let ValueData::List(parts) = value.data() else {
                if value.is_null() {
                    return Ok(Value::null(LogicalType::VARCHAR));
                }
                let part = text(value)?;
                if !joined.is_empty() && !joined.ends_with('/') {
                    joined.push('/');
                }
                joined += &part;
                continue;
            };
            for part in parts {
                let part = text(part)?;
                if !joined.is_empty() && !joined.ends_with('/') {
                    joined.push('/');
                }
                joined += &part;
            }
        }
        Ok(Value::varchar(joined))
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
fn damerau(a: &[u8], b: &[u8]) -> i64 {
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
    let mut last = [0usize; 256];
    for i in 1..=n {
        let mut last_match = 0;
        for j in 1..=m {
            let (k, l) = (last[b[j - 1] as usize], last_match);
            let cost = usize::from(a[i - 1] != b[j - 1]);
            if cost == 0 {
                last_match = j;
            }
            table[i + 1][j + 1] = (table[i][j] + cost)
                .min(table[i + 1][j] + 1)
                .min(table[i][j + 1] + 1)
                .min(table[k][l] + (i - k - 1) + 1 + (j - l - 1));
        }
        last[a[i - 1] as usize] = i;
    }
    table[n + 1][m + 1] as i64
}

fn damerau_levenshtein(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::BIGINT));
        }
        Ok(Value::bigint(damerau(
            text(&row[0])?.as_bytes(),
            text(&row[1])?.as_bytes(),
        )))
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
        let set = |s: &str| s.bytes().collect::<std::collections::BTreeSet<u8>>();
        let (a, b) = (set(&a), set(&b));
        let common = a.intersection(&b).count() as f64;
        let all = a.union(&b).count() as f64;
        Ok(Value::double(if all == 0.0 { 0.0 } else { common / all }))
    })
}

fn jaro(a: &[u8], b: &[u8]) -> f64 {
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

fn jaro_winkler(a: &[u8], b: &[u8]) -> f64 {
    let similarity = jaro(a, b);
    if similarity <= 0.7 {
        return similarity;
    }
    let prefix = a.iter().zip(b).take(4).take_while(|(x, y)| x == y).count() as f64;
    similarity + prefix * 0.1 * (1.0 - similarity)
}

fn similarity(args: &DataChunk, result: &mut Vector, f: fn(&[u8], &[u8]) -> f64) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::DOUBLE));
        }
        let score = f(text(&row[0])?.as_bytes(), text(&row[1])?.as_bytes());
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

fn bar(args: &DataChunk, result: &mut Vector) -> Result<()> {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    row_wise(args, result, |row| {
        let Some(x) = number(&row[0]) else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let (Some(low), Some(high)) = (number(&row[1]), number(&row[2])) else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let width = row.get(3).and_then(Value::as_i128).unwrap_or(80);
        if !(1..=1000).contains(&width) {
            return Err(Error::new(ErrorKind::OutOfRange, "Max bar width is 1000"));
        }
        if x.is_nan() || low.is_nan() || high.is_nan() || x.is_infinite() || low.is_infinite() || high.is_infinite() {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                "Bar function arguments must be finite.",
            ));
        }
        if low > high {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                "Bar function arguments max must be greater than min.",
            ));
        }
        let ratio = if high == low {
            0.0
        } else {
            ((x - low) / (high - low)).clamp(0.0, 1.0)
        };
        let eighths = (ratio * width as f64 * 8.0).round() as usize;
        let (full, part) = (eighths / 8, eighths % 8);
        let mut out = "█".repeat(full);
        out += PARTS[part];
        let used = full + usize::from(part > 0);
        out += &" ".repeat(width as usize - used.min(width as usize));
        Ok(Value::varchar(out))
    })
}

/// `StringUtil::BytesToHumanReadableString`
fn human_bytes(bytes: i128, unit: f64, names: [&str; 6]) -> String {
    let sign = if bytes < 0 { "-" } else { "" };
    let mut amount = bytes.unsigned_abs() as f64;
    if amount < unit {
        return format!("{sign}{} bytes", bytes.unsigned_abs());
    }
    let mut index = 0;
    amount /= unit;
    while amount >= unit && index + 1 < names.len() {
        amount /= unit;
        index += 1;
    }
    format!("{sign}{amount:.1} {}", names[index])
}

fn format_bytes(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(bytes) = row[0].as_i128() else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        Ok(Value::varchar(human_bytes(
            bytes,
            1024.0,
            ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"],
        )))
    })
}

fn format_decimal_bytes(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let Some(bytes) = row[0].as_i128() else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        Ok(Value::varchar(human_bytes(
            bytes,
            1000.0,
            ["kB", "MB", "GB", "TB", "PB", "EB"],
        )))
    })
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
        let micros = micros_of(&row[1]).unwrap_or(0);
        let (origin, offset) = match row.get(2).map(Value::data) {
            Some(ValueData::Interval(offset)) => (None, Some(*offset)),
            Some(_) => (micros_of(&row[2]), None),
            None => (None, None),
        };
        let bucket = time_bucket_value(*width, micros, origin, offset)?;
        Ok(match ty.id() {
            LogicalTypeId::Date => Value::date(Date(bucket.div_euclid(MICROS_PER_DAY) as i32)),
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
        let (Some(left), Some(right)) = (list_values(&row[0]), list_values(&row[1])) else {
            return Ok(Value::null(ty.clone()));
        };
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
            "{name}: list dimensions must be equal, got left length {} and right length {}",
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
        let score = f(&a, &b);
        Ok(if ty.id() == LogicalTypeId::Float {
            Value::float(score as f32)
        } else {
            Value::double(score)
        })
    })
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let bottom = (inner(a, a) * inner(b, b)).sqrt();
    if bottom == 0.0 {
        0.0
    } else {
        (inner(a, b) / bottom).clamp(-1.0, 1.0)
    }
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

fn map_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
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
fn vector_arguments(name: &str, children: Vec<BoundExpr>) -> Result<(Vec<BoundExpr>, LogicalType)> {
    count_args(name, &children, 2, 2)?;
    let element = |child: &BoundExpr| child.return_type.child_type().cloned();
    let numeric = |child: &BoundExpr| {
        matches!(child.return_type.id(), LogicalTypeId::List | LogicalTypeId::Array)
            && element(child).is_some_and(|ty| ty.is_numeric())
    };
    if !children.iter().all(numeric) {
        return Err(binder_error(format!(
            "{name}: Could not choose a best candidate function"
        )));
    }
    let float = element(&children[0]).is_some_and(|ty| ty.id() == LogicalTypeId::Float)
        && element(&children[1]).is_some_and(|ty| ty.id() == LogicalTypeId::Float);
    let ty = if float { LogicalType::FLOAT } else { LogicalType::DOUBLE };
    Ok((children, ty))
}

/// Binds a function of [`is_extra_function`]; `names` are the names of its arguments.
pub fn bind_function(name: &str, names: &[String], mut children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
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
                LogicalTypeId::Date | LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz
            ) {
                return Err(binder_error(format!(
                    "No function matches the given name and argument types 'time_bucket(INTERVAL, {ty})'."
                )));
            }
            Ok(bound_function(&lower, children, ty, time_bucket))
        }
        "normalized_interval" => simple(1, 1, LogicalType::INTERVAL, normalized_interval, children),
        "timetz_byte_comparable" => simple(1, 1, LogicalType::UBIGINT, timetz_byte_comparable, children),
        "uuid_extract_version" => simple(1, 1, LogicalType::UINTEGER, uuid_extract_version, children),
        "uuid_extract_timestamp" => simple(
            1,
            1,
            LogicalType::new(LogicalTypeId::TimestampTz),
            uuid_extract_timestamp,
            children,
        ),
        "list_intersect" | "array_intersect" => {
            count_args(&lower, &children, 2, 2)?;
            let ty = children[0].return_type.clone();
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
        "array_cross_product" => {
            count_args(&lower, &children, 2, 2)?;
            let ty = children[0].return_type.clone();
            Ok(bound_function(&lower, children, ty, array_cross_product))
        }
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
        "struct_keys" => simple(1, 1, list_of_text, struct_keys, children),
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
            let wanted = children.remove(1).cast_to(&key, false);
            children.push(wanted);
            Ok(match lower.as_str() {
                "map_contains" => bound_function(&lower, children, LogicalType::BOOLEAN, map_contains),
                "map_extract_value" => bound_function(&lower, children, value, map_extract_value),
                _ => bound_function(&lower, children, LogicalType::list(value), map_extract),
            })
        }
        "map_concat" => {
            let Some(ty) = children.first().map(|c| c.return_type.clone()) else {
                return Err(binder_error("map_concat expects at least one argument"));
            };
            Ok(bound_function(&lower, casted(children, &ty), ty, map_concat))
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
