// Ported from duckdb/src/function/scalar/string/*.cpp and
// duckdb/extension/core_functions/scalar/string/*.cpp @ rust-port-baseline
//
//! The string functions. They run row by row over `Value`s; the vectorized loops come with
//! Phase 13. Lengths count characters, except in the `_grapheme` functions.

use super::bit_functions::strict_rows;
use super::nested::row_wise;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, UnifiedFormat, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use unicode_segmentation::UnicodeSegmentation;

fn invalid_input(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn bytes_of(value: &Value) -> &[u8] {
    match value.data() {
        ValueData::Bytes(bytes) => bytes,
        _ => unreachable!("a string"),
    }
}

fn text(value: &Value) -> String {
    String::from_utf8_lossy(bytes_of(value)).into_owned()
}

fn integer(value: &Value) -> i64 {
    value.as_i128().expect("an integer") as i64
}

const SUBSTRING_UPPER_BOUND: i64 = u32::MAX as i64;
const SUBSTRING_LOWER_BOUND: i64 = -SUBSTRING_UPPER_BOUND - 1;

/// `AssertInSupportedRange`.
pub(super) fn check_substring_range(size: usize, offset: i64, length: i64) -> Result<()> {
    let out_of_range = |message: String| Err(Error::new(ErrorKind::OutOfRange, message));
    if size as i64 > SUBSTRING_UPPER_BOUND {
        return out_of_range(format!("Substring input size is too large (> {SUBSTRING_UPPER_BOUND})"));
    }
    for (name, value) in [("offset", offset), ("length", length)] {
        if value < SUBSTRING_LOWER_BOUND {
            return out_of_range(format!(
                "Substring {name} outside of supported range (< {SUBSTRING_LOWER_BOUND})"
            ));
        }
        if value > SUBSTRING_UPPER_BOUND {
            return out_of_range(format!(
                "Substring {name} outside of supported range (> {SUBSTRING_UPPER_BOUND})"
            ));
        }
    }
    Ok(())
}

/// `SubstringStartEnd` and `SubstringASCIIBounds`, on a string of `size` units.
fn substring_bounds(size: i64, offset: i64, length: i64) -> Option<(usize, usize)> {
    if length == 0 {
        return None;
    }
    let mut length = length;
    let mut start = offset;
    if offset > 0 {
        start = offset - 1;
    } else if offset == 0 {
        length -= 1;
        if length <= 0 {
            return None;
        }
    }
    let mut end = start + length;
    if length < 0 {
        end = start;
        start += length;
    }
    if offset < 0 {
        start += size;
        end += size;
    }
    let (start, end) = (start.clamp(0, size), end.clamp(0, size));
    (start < end).then_some((start as usize, end as usize))
}

fn substring(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let count = args.len();
    let input = args.column(0).to_unified(count);
    let offsets = args.column(1).to_unified(count);
    let lengths = (args.column_count() > 2).then(|| args.column(2).to_unified(count));
    let integer_at = |format: &UnifiedFormat, row: usize| format.data::<i64>()[format.sel.get(row)];
    result.prepare_flat(count.max(1));
    for row in 0..count {
        if !input.is_valid(row) || !offsets.is_valid(row) || lengths.as_ref().is_some_and(|l| !l.is_valid(row)) {
            result.set_null(row, true);
            continue;
        }
        let bytes = input.str_at(row);
        let offset = integer_at(&offsets, row);
        let length = lengths.as_ref().map_or(i64::from(u32::MAX), |l| integer_at(l, row));
        check_substring_range(bytes.len(), offset, length)?;
        if bytes.is_ascii() {
            let part =
                substring_bounds(bytes.len() as i64, offset, length).map_or(&[][..], |(from, to)| &bytes[from..to]);
            result.set_str(row, part);
        } else {
            let text = String::from_utf8_lossy(bytes);
            let chars: Vec<char> = text.chars().collect();
            let part: String = substring_bounds(chars.len() as i64, offset, length)
                .map_or_else(String::new, |(from, to)| chars[from..to].iter().collect());
            result.set_str(row, part.as_bytes());
        }
    }
    Ok(())
}

/// `SubstringGrapheme`.
fn substring_grapheme(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = text(&row[0]);
        let (offset, length) = (integer(&row[1]), row.get(2).map_or(i64::from(u32::MAX), integer));
        check_substring_range(input.len(), offset, length)?;
        let graphemes: Vec<&str> = input.graphemes(true).collect();
        Ok(Value::varchar(
            match substring_bounds(graphemes.len() as i64, offset, length) {
                Some((start, end)) => graphemes[start..end].concat(),
                None => String::new(),
            },
        ))
    })
}

fn replace(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (input, from, to) = (text(&row[0]), text(&row[1]), text(&row[2]));
        Ok(Value::varchar(if from.is_empty() {
            input
        } else {
            input.replace(&from, &to)
        }))
    })
}

fn reverse(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::varchar(text(&row[0]).graphemes(true).rev().collect::<String>()))
    })
}

fn trim_with(row: &[Value], left: bool, right: bool) -> Value {
    let input = text(&row[0]);
    let characters: Option<Vec<char>> = row.get(1).map(|v| text(v).chars().collect());
    let in_set = |c: char| match &characters {
        Some(set) => set.contains(&c),
        None => c.is_whitespace(),
    };
    let mut out = input.as_str();
    if left {
        out = out.trim_start_matches(in_set);
    }
    if right {
        out = out.trim_end_matches(in_set);
    }
    Value::varchar(out)
}

fn trim(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(trim_with(row, true, true)))
}

fn ltrim(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(trim_with(row, true, false)))
}

fn rtrim(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(trim_with(row, false, true)))
}

/// `LeftScalarFunction` over grapheme clusters.
fn left_grapheme(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = text(&row[0]);
        let graphemes: Vec<&str> = input.graphemes(true).collect();
        let (count, size) = (integer(&row[1]), graphemes.len() as i64);
        let take = if count >= 0 {
            count.min(size)
        } else {
            (size + count).max(0)
        };
        Ok(Value::varchar(graphemes[..take as usize].concat()))
    })
}

/// `RightScalarFunction` over grapheme clusters.
fn right_grapheme(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = text(&row[0]);
        let graphemes: Vec<&str> = input.graphemes(true).collect();
        let (count, size) = (integer(&row[1]), graphemes.len() as i64);
        let take = if count >= 0 {
            count.min(size)
        } else {
            (size + count).max(0)
        };
        Ok(Value::varchar(graphemes[(size - take) as usize..].concat()))
    })
}

fn left(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let chars: Vec<char> = text(&row[0]).chars().collect();
        let count = integer(&row[1]);
        let size = chars.len() as i64;
        let take = if count >= 0 {
            count.min(size)
        } else {
            (size + count).max(0)
        };
        Ok(Value::varchar(chars[..take as usize].iter().collect::<String>()))
    })
}

fn right(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let chars: Vec<char> = text(&row[0]).chars().collect();
        let count = integer(&row[1]);
        let size = chars.len() as i64;
        let take = if count >= 0 {
            count.min(size)
        } else {
            (size + count).max(0)
        };
        Ok(Value::varchar(
            chars[(size - take) as usize..].iter().collect::<String>(),
        ))
    })
}

/// A test of two strings on their bytes, row by row; a NULL on either side gives NULL. False, with
/// nothing done, if an argument is not a vector of strings.
pub(super) fn string_predicate(args: &DataChunk, result: &mut Vector, f: impl Fn(&[u8], &[u8]) -> bool) -> bool {
    use duckrs_common::types::PhysicalType;
    let is_strings = |v: &Vector| v.physical_type() == PhysicalType::Varchar;
    if args.column_count() != 2 || !is_strings(args.column(0)) || !is_strings(args.column(1)) {
        return false;
    }
    let count = args.len();
    let (left, right) = (args.column(0).to_unified(count), args.column(1).to_unified(count));
    result.prepare_flat(count.max(1));
    for row in 0..count {
        if left.is_valid(row) && right.is_valid(row) {
            result.data_mut::<bool>()[row] = f(left.str_at(row), right.str_at(row));
        } else {
            result.set_null(row, true);
        }
    }
    true
}

fn starts_with(args: &DataChunk, result: &mut Vector) -> Result<()> {
    if string_predicate(args, result, |text, prefix| text.starts_with(prefix)) {
        return Ok(());
    }
    strict_rows(args, result, |row| {
        Ok(Value::boolean(bytes_of(&row[0]).starts_with(bytes_of(&row[1]))))
    })
}

fn ends_with(args: &DataChunk, result: &mut Vector) -> Result<()> {
    if string_predicate(args, result, |text, suffix| text.ends_with(suffix)) {
        return Ok(());
    }
    strict_rows(args, result, |row| {
        Ok(Value::boolean(bytes_of(&row[0]).ends_with(bytes_of(&row[1]))))
    })
}

fn instr(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (input, needle) = (text(&row[0]), text(&row[1]));
        Ok(Value::bigint(match input.find(&needle) {
            Some(at) => input[..at].chars().count() as i64 + 1,
            None => 0,
        }))
    })
}

fn ascii(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::integer(text(&row[0]).chars().next().map_or(0, |c| c as i32)))
    })
}

// Ported from duckdb/extension/core_functions/scalar/string/parse_formatted_bytes.cpp @ rust-port-baseline
/// `parse_formatted_bytes('1.5 KiB')`: the bytes a size as `memory_limit` spells it stands for.
fn parse_formatted_bytes(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        crate::execution::temporary::parse_formatted_bytes(&text(&row[0]))
            .map(Value::ubigint)
            .map_err(|message| duckrs_common::Error::new(duckrs_common::ErrorKind::InvalidInput, message))
    })
}

fn unicode(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::integer(text(&row[0]).chars().next().map_or(-1, |c| c as i32)))
    })
}

fn split_on(input: &str, separator: &str) -> Vec<Value> {
    if separator.is_empty() {
        return input.chars().map(|c| Value::varchar(c.to_string())).collect();
    }
    input.split(separator).map(Value::varchar).collect()
}

fn string_split(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let parts = if row[1].is_null() {
            vec![row[0].clone()]
        } else {
            split_on(&text(&row[0]), &text(&row[1]))
        };
        Value::new(ty.clone(), ValueData::List(parts))
    })
}

fn split_part(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let parts = split_on(&text(&row[0]), &text(&row[1]));
        let index = integer(&row[2]);
        let size = parts.len() as i64;
        let at = if index > 0 {
            index - 1
        } else if index < 0 {
            size + index
        } else {
            -1
        };
        Ok(usize::try_from(at)
            .ok()
            .and_then(|at| parts.get(at).cloned())
            .unwrap_or_else(|| Value::varchar("")))
    })
}

fn translate(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let from: Vec<char> = text(&row[1]).chars().collect();
        let to: Vec<char> = text(&row[2]).chars().collect();
        let mapped: String = text(&row[0])
            .chars()
            .filter_map(|c| match from.iter().position(|f| *f == c) {
                Some(at) => to.get(at).copied(),
                None => Some(c),
            })
            .collect();
        Ok(Value::varchar(mapped))
    })
}

fn hex_text(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn hex(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::varchar(hex_text(bytes_of(&row[0])))))
}

/// `HexIntegralOperator` (BIGINT, UBIGINT) and `HexHugeIntOperator` (HUGEINT): a negative value
/// prints as two's complement of the width of its type.
fn hex_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let text = match row[0].data() {
            ValueData::Int128(v) => format!("{:X}", *v as u128),
            ValueData::UInt64(v) => format!("{v:X}"),
            _ => format!("{:X}", row[0].as_i128().expect("an integer") as i64 as u64),
        };
        Ok(Value::varchar(text))
    })
}

fn hex_uhugeint(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::UInt128(value) = row[0].data() else {
            unreachable!("a UHUGEINT")
        };
        Ok(Value::varchar(format!("{value:X}")))
    })
}

fn bin_uhugeint(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let ValueData::UInt128(value) = row[0].data() else {
            unreachable!("a UHUGEINT")
        };
        Ok(Value::varchar(format!("{value:b}")))
    })
}

/// `WriteBignumDigits`: a BIGNUM has no fixed width, so a negative value is a sign and its
/// magnitude rather than two's complement.
fn bignum_digits(value: &Value, bits_per_digit: u32) -> String {
    use duckrs_common::types::bignum;
    let bytes = bytes_of(value);
    let magnitude = bignum::magnitude(bytes);
    let Some(start) = magnitude.iter().position(|b| *b != 0) else {
        return "0".to_string();
    };
    let mut out = String::new();
    if bignum::is_negative(bytes) {
        out.push('-');
    }
    let per_byte = 8 / bits_per_digit;
    let mask = (1u8 << bits_per_digit) - 1;
    for (i, byte) in magnitude[start..].iter().enumerate() {
        let digits = if i == 0 {
            (8 - byte.leading_zeros()).div_ceil(bits_per_digit)
        } else {
            per_byte
        };
        for d in (1..=digits).rev() {
            let digit = (byte >> ((d - 1) * bits_per_digit)) & mask;
            out.push(
                char::from_digit(u32::from(digit), 16)
                    .expect("a digit")
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

fn hex_bignum(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::varchar(bignum_digits(&row[0], 4))))
}

fn bin_bignum(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::varchar(bignum_digits(&row[0], 1))))
}

fn bin(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let text: String = bytes_of(&row[0]).iter().map(|b| format!("{b:08b}")).collect();
        Ok(Value::varchar(text))
    })
}

fn bin_integer(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let text = match row[0].data() {
            ValueData::Int128(v) => format!("{:b}", *v as u128),
            ValueData::UInt64(v) => format!("{v:b}"),
            _ => format!("{:b}", row[0].as_i128().expect("an integer") as i64 as u64),
        };
        Ok(Value::varchar(text))
    })
}

fn unhex(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = text(&row[0]);
        let digits: Vec<char> = input.chars().collect();
        let digit = |c: char| {
            c.to_digit(16)
                .ok_or_else(|| invalid_input(format!("Invalid input for hex digit: {c}")))
        };
        let first = digits.len() % 2;
        let mut bytes = Vec::with_capacity(digits.len() / 2 + 1);
        if first == 1 {
            bytes.push(digit(digits[0])? as u8);
        }
        for pair in digits[first..].chunks(2) {
            bytes.push((digit(pair[0])? * 16 + digit(pair[1])?) as u8);
        }
        Ok(Value::blob(bytes))
    })
}

/// `EditDistance`.
fn levenshtein(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let left: Vec<char> = text(&row[0]).chars().collect();
        let right: Vec<char> = text(&row[1]).chars().collect();
        let mut previous: Vec<usize> = (0..=right.len()).collect();
        for (i, l) in left.iter().enumerate() {
            let mut current = vec![i + 1];
            for (j, r) in right.iter().enumerate() {
                let substitute = previous[j] + usize::from(l != r);
                current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
            }
            previous = current;
        }
        Ok(Value::bigint(previous[right.len()] as i64))
    })
}

fn hamming(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let left: Vec<char> = text(&row[0]).chars().collect();
        let right: Vec<char> = text(&row[1]).chars().collect();
        if left.len() != right.len() {
            return Err(invalid_input("Mismatch Function: Strings must be of equal length!"));
        }
        if left.is_empty() {
            return Err(invalid_input("Mismatch Function: Strings must be of length > 0!"));
        }
        let distance = left.iter().zip(&right).filter(|(l, r)| l != r).count();
        Ok(Value::bigint(distance as i64))
    })
}

/// `TemplatedLikeOperator`: an escape at the end of the pattern is an error when the match gets
/// there.
fn like_match(text: &[char], pattern: &[char], escape: Option<char>) -> Result<bool> {
    let ends_with_escape = || Error::new(ErrorKind::Syntax, "Like pattern must not end with escape character!");
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let mut matched = false;
        if p < pattern.len() {
            let c = pattern[p];
            if Some(c) == escape {
                if p + 1 == pattern.len() {
                    return Err(ends_with_escape());
                }
                if pattern[p + 1] == text[t] {
                    t += 1;
                    p += 2;
                    matched = true;
                }
            } else if c == '%' {
                p += 1;
                star = Some((p, t));
                continue;
            } else if c == '_' || c == text[t] {
                t += 1;
                p += 1;
                matched = true;
            }
        }
        if matched {
            continue;
        }
        let Some((star_pattern, star_text)) = star else {
            return Ok(false);
        };
        star = Some((star_pattern, star_text + 1));
        p = star_pattern;
        t = star_text + 1;
    }
    // a trailing '%' only matches an empty suffix when it is not escaped
    while p < pattern.len() {
        if Some(pattern[p]) == escape {
            if p + 1 == pattern.len() {
                return Err(ends_with_escape());
            }
            // the escape sequence needs a character to match, and there is none left
            break;
        }
        if pattern[p] != '%' {
            break;
        }
        p += 1;
    }
    Ok(p == pattern.len())
}

fn escape_of(value: &Value) -> Result<Option<char>> {
    let escape = text(value);
    let mut chars = escape.chars();
    let first = chars.next();
    if chars.next().is_some() {
        return Err(Error::new(
            ErrorKind::Syntax,
            "Invalid escape string. Escape string must be empty or one character.",
        ));
    }
    Ok(first)
}

fn like_escape(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input: Vec<char> = text(&row[0]).chars().collect();
        let pattern: Vec<char> = text(&row[1]).chars().collect();
        let escape = escape_of(&row[2])?;
        Ok(Value::boolean(like_match(&input, &pattern, escape)?))
    })
}

fn ilike_escape(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input: Vec<char> = text(&row[0]).to_lowercase().chars().collect();
        let pattern: Vec<char> = text(&row[1]).to_lowercase().chars().collect();
        let escape = escape_of(&row[2])?;
        Ok(Value::boolean(like_match(&input, &pattern, escape)?))
    })
}

/// Whether a `[...]` at the start of `pattern` takes `c`, and the size of the class.
fn glob_class(pattern: &[char], c: char) -> Option<(bool, usize)> {
    let close = pattern.iter().skip(2).position(|p| *p == ']')? + 2;
    let (negated, set) = match pattern[1] {
        '!' => (true, &pattern[2..close]),
        _ => (false, &pattern[1..close]),
    };
    let mut found = false;
    let mut at = 0;
    while at < set.len() {
        if at + 2 < set.len() && set[at + 1] == '-' {
            found |= (set[at]..=set[at + 2]).contains(&c);
            at += 3;
        } else {
            found |= set[at] == c;
            at += 1;
        }
    }
    Some((found != negated, close + 1))
}

/// `GlobFun`: one star is all that needs revisiting, the last one.
fn glob_match(text: &[char], pattern: &[char]) -> bool {
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let step = match pattern.get(p) {
            Some('*') => {
                p += 1;
                star = Some((p, t));
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match glob_class(&pattern[p..], text[t]) {
                Some((true, size)) => Some(size),
                Some((false, _)) => None,
                None => return false,
            },
            Some('\\') if pattern.get(p + 1) == Some(&text[t]) => Some(2),
            Some(c) if *c == text[t] => Some(1),
            _ => None,
        };
        if let Some(step) = step {
            p += step;
            t += 1;
        } else if let Some((star_pattern, star_text)) = star {
            star = Some((star_pattern, star_text + 1));
            p = star_pattern;
            t = star_text + 1;
        } else {
            return false;
        }
    }
    while pattern.get(p) == Some(&'*') {
        p += 1;
    }
    p == pattern.len()
}

fn glob(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input: Vec<char> = text(&row[0]).chars().collect();
        let pattern: Vec<char> = text(&row[1]).chars().collect();
        Ok(Value::boolean(glob_match(&input, &pattern)))
    })
}

/// RFC 1321.
fn md5_digest(input: &[u8]) -> [u8; 16] {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    let table: Vec<u32> = (0..64)
        .map(|i| (f64::from(i + 1).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) = (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    for block in message.chunks(64) {
        let words: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(table[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
            (a, d, c, b) = (d, c, b, b.wrapping_add(rotated));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut digest = [0; 16];
    for (i, word) in [a0, b0, c0, d0].iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

/// `MD5Number128Operator`: the digest's bytes as a little-endian UHUGEINT.
fn md5_number(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::uhugeint(u128::from_le_bytes(md5_digest(bytes_of(&row[0])))))
    })
}

/// `OverlayFunction`: `replacement` in place of `count` characters (by default as many as it has) from
/// character `start`.
fn overlay(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row.iter().any(Value::is_null) {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let input = row[0].try_to_string()?;
        let replacement = row[1].try_to_string()?;
        let start = row[2].as_i128().unwrap_or(0) as i64;
        let mut count = replacement.chars().count() as i64;
        if let Some(given) = row.get(3).and_then(Value::as_i128)
            && given >= 0
        {
            count = given as i64;
        }
        // `CharPosToByte`: the byte where the 1-based character starts, clamped to the string
        let byte_of = |position: i64| -> usize {
            if position <= 1 {
                return 0;
            }
            input
                .char_indices()
                .nth((position - 1) as usize)
                .map_or(input.len(), |(at, _)| at)
        };
        let end = if count > 0 && start > i64::MAX - count { i64::MAX } else { start + count };
        let (from, to) = (byte_of(start), byte_of(end));
        Ok(Value::varchar(format!("{}{replacement}{}", &input[..from], &input[to..])))
    })
}

fn md5(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::varchar(hex_text(&md5_digest(bytes_of(&row[0]))).to_lowercase()))
    })
}

fn encode(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(Value::blob(bytes_of(&row[0]).to_vec())))
}

const DECODE_ERROR: &str = "Failure in decode: could not convert blob to UTF8 string, the blob contained invalid UTF8 \
     characters. \nUse try(decode(BLOB)) to return NULL and continue instead of returning an error. Specify \
     decode(BLOB, 'replace') to replace invalid characters with '?'. Specify decode(BLOB, 'ignore') to remove invalid \
     characters when encountered.";

fn decode(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let text =
            std::str::from_utf8(bytes_of(&row[0])).map_err(|_| Error::new(ErrorKind::Conversion, DECODE_ERROR))?;
        Ok(Value::varchar(text))
    })
}

/// `BinaryDecodeFunction`: invalid UTF-8 an error, replaced by `?` or left out, as the second argument says.
fn decode_with(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let bytes = bytes_of(&row[0]);
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(Value::varchar(text));
        }
        let option = row[1].try_to_string()?;
        match option.to_lowercase().as_str() {
            "strict" => Err(Error::new(ErrorKind::Conversion, DECODE_ERROR)),
            "replace" => Ok(Value::varchar(crate::execution::csv::make_valid(bytes, '?'))),
            "ignore" => Ok(Value::varchar(crate::execution::csv::remove_invalid(bytes))),
            _ => Err(Error::new(
                ErrorKind::Conversion,
                format!("decode error behavior specifier \"{option}\" not recognized"),
            )),
        }
    })
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn to_base64(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let mut out = String::new();
        for group in bytes_of(&row[0]).chunks(3) {
            let bits = u32::from(group[0]) << 16
                | u32::from(*group.get(1).unwrap_or(&0)) << 8
                | u32::from(*group.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= group.len() {
                    out.push(BASE64[(bits >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        Ok(Value::varchar(out))
    })
}

fn from_base64(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = text(&row[0]);
        if !input.len().is_multiple_of(4) {
            return Err(Error::new(
                ErrorKind::Conversion,
                format!("Could not decode string \"{input}\" as base64: length must be a multiple of 4"),
            ));
        }
        let mut out = Vec::with_capacity(input.len() / 4 * 3);
        let final_group = input.len().saturating_sub(4);
        for (index, group) in input.as_bytes().chunks(4).enumerate() {
            let base = index * 4;
            let last = base == final_group;
            if last && group[2] == b'=' && group[3] != b'=' {
                return Err(Error::new(
                    ErrorKind::Conversion,
                    format!(
                        "Could not decode string \"{input}\" as base64: data after the padding at position {}",
                        base + 3
                    ),
                ));
            }
            let mut bits = 0u32;
            let mut padding = 0;
            for (at, c) in group.iter().enumerate() {
                bits <<= 6;
                if last && at >= 2 && *c == b'=' {
                    padding += 1;
                } else {
                    let Some(digit) = BASE64.iter().position(|b| b == c) else {
                        return Err(Error::new(
                            ErrorKind::Conversion,
                            format!(
                                "Could not decode string \"{input}\" as base64: invalid byte value '{c}' at \
                                 position {}",
                                base + at
                            ),
                        ));
                    };
                    bits |= digit as u32;
                }
            }
            let bytes = bits.to_be_bytes();
            out.extend_from_slice(&bytes[1..4 - padding]);
        }
        Ok(Value::blob(out))
    })
}

fn set_of(name: &str, overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)>) -> ScalarFunctionSet {
    let mut set = ScalarFunctionSet::new(name);
    for (arguments, return_type, callback) in overloads {
        set.add(ScalarFunction::new(name, arguments, return_type, callback));
    }
    set
}

pub fn string_functions() -> Vec<ScalarFunctionSet> {
    let v = LogicalType::VARCHAR;
    let i = LogicalType::BIGINT;
    let b = LogicalType::BOOLEAN;
    let blob = LogicalType::BLOB;
    let list = LogicalType::list(LogicalType::VARCHAR);
    let mut sets = Vec::new();
    for name in ["substring", "substr", "substring_grapheme"] {
        let callback: ScalarCallback = if name == "substring_grapheme" {
            substring_grapheme
        } else {
            substring
        };
        sets.push(set_of(
            name,
            vec![
                (vec![v.clone(), i.clone()], v.clone(), callback),
                (vec![v.clone(), i.clone(), i.clone()], v.clone(), callback),
            ],
        ));
    }
    for name in ["trim", "ltrim", "rtrim"] {
        let callback: ScalarCallback = match name {
            "trim" => trim,
            "ltrim" => ltrim,
            _ => rtrim,
        };
        sets.push(set_of(
            name,
            vec![
                (vec![v.clone()], v.clone(), callback),
                (vec![v.clone(), v.clone()], v.clone(), callback),
            ],
        ));
    }
    for name in ["string_split", "str_split", "string_to_array", "split"] {
        sets.push(set_of(
            name,
            vec![(vec![v.clone(), v.clone()], list.clone(), string_split as ScalarCallback)],
        ));
    }
    let simple: Vec<(&str, Vec<LogicalType>, LogicalType, ScalarCallback)> = vec![
        ("replace", vec![v.clone(), v.clone(), v.clone()], v.clone(), replace),
        ("reverse", vec![v.clone()], v.clone(), reverse),
        ("left", vec![v.clone(), i.clone()], v.clone(), left),
        ("left_grapheme", vec![v.clone(), i.clone()], v.clone(), left_grapheme),
        ("right", vec![v.clone(), i.clone()], v.clone(), right),
        ("right_grapheme", vec![v.clone(), i.clone()], v.clone(), right_grapheme),
        ("starts_with", vec![v.clone(), v.clone()], b.clone(), starts_with),
        ("prefix", vec![v.clone(), v.clone()], b.clone(), starts_with),
        ("^@", vec![v.clone(), v.clone()], b.clone(), starts_with),
        ("suffix", vec![v.clone(), v.clone()], b.clone(), ends_with),
        ("ends_with", vec![v.clone(), v.clone()], b.clone(), ends_with),
        ("instr", vec![v.clone(), v.clone()], i.clone(), instr),
        ("strpos", vec![v.clone(), v.clone()], i.clone(), instr),
        ("position", vec![v.clone(), v.clone()], i.clone(), instr),
        ("ascii", vec![v.clone()], LogicalType::INTEGER, ascii),
        (
            "parse_formatted_bytes",
            vec![v.clone()],
            LogicalType::UBIGINT,
            parse_formatted_bytes,
        ),
        ("unicode", vec![v.clone()], LogicalType::INTEGER, unicode),
        ("ord", vec![v.clone()], LogicalType::INTEGER, unicode),
        (
            "split_part",
            vec![v.clone(), v.clone(), i.clone()],
            v.clone(),
            split_part,
        ),
        ("translate", vec![v.clone(), v.clone(), v.clone()], v.clone(), translate),
        ("levenshtein", vec![v.clone(), v.clone()], i.clone(), levenshtein),
        ("editdist3", vec![v.clone(), v.clone()], i.clone(), levenshtein),
        ("hamming", vec![v.clone(), v.clone()], i.clone(), hamming),
        ("mismatches", vec![v.clone(), v.clone()], i.clone(), hamming),
        (
            "like_escape",
            vec![v.clone(), v.clone(), v.clone()],
            b.clone(),
            like_escape,
        ),
        (
            "ilike_escape",
            vec![v.clone(), v.clone(), v.clone()],
            b.clone(),
            ilike_escape,
        ),
        ("md5", vec![v.clone()], v.clone(), md5),
        ("md5", vec![blob.clone()], v.clone(), md5),
        ("md5_number", vec![v.clone()], LogicalType::UHUGEINT, md5_number),
        ("md5_number", vec![blob.clone()], LogicalType::UHUGEINT, md5_number),
        ("overlay", vec![v.clone(), v.clone(), LogicalType::BIGINT], v.clone(), overlay),
        (
            "overlay",
            vec![v.clone(), v.clone(), LogicalType::BIGINT, LogicalType::BIGINT],
            v.clone(),
            overlay,
        ),
        ("~~~", vec![v.clone(), v.clone()], b.clone(), glob),
        ("encode", vec![v.clone()], blob.clone(), encode),
        ("to_base64", vec![blob.clone()], v.clone(), to_base64),
        ("base64", vec![blob.clone()], v.clone(), to_base64),
        ("from_base64", vec![v.clone()], blob.clone(), from_base64),
        ("unhex", vec![v.clone()], blob.clone(), unhex),
        ("from_hex", vec![v.clone()], blob.clone(), unhex),
    ];
    for (name, arguments, return_type, callback) in simple {
        sets.push(set_of(name, vec![(arguments, return_type, callback)]));
    }
    sets.push(set_of(
        "decode",
        vec![
            (vec![blob.clone()], v.clone(), decode as ScalarCallback),
            (vec![blob.clone(), v.clone()], v.clone(), decode_with),
        ],
    ));
    for name in ["hex", "to_hex"] {
        sets.push(set_of(
            name,
            vec![
                (vec![v.clone()], v.clone(), hex as ScalarCallback),
                (vec![blob.clone()], v.clone(), hex),
                (vec![LogicalType::BIGINT], v.clone(), hex_integer),
                (vec![LogicalType::UBIGINT], v.clone(), hex_integer),
                (vec![LogicalType::HUGEINT], v.clone(), hex_integer),
                (vec![LogicalType::UHUGEINT], v.clone(), hex_uhugeint),
                (vec![LogicalType::new(LogicalTypeId::Bignum)], v.clone(), hex_bignum),
            ],
        ));
    }
    for name in ["bin", "to_binary"] {
        sets.push(set_of(
            name,
            vec![
                (vec![v.clone()], v.clone(), bin as ScalarCallback),
                (vec![blob.clone()], v.clone(), bin),
                (vec![LogicalType::BIGINT], v.clone(), bin_integer),
                (vec![LogicalType::UBIGINT], v.clone(), bin_integer),
                (vec![LogicalType::HUGEINT], v.clone(), bin_integer),
                (vec![LogicalType::UHUGEINT], v.clone(), bin_uhugeint),
                (vec![LogicalType::new(LogicalTypeId::Bignum)], v.clone(), bin_bignum),
            ],
        ));
    }
    sets
}
