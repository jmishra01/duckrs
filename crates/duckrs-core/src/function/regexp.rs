// Ported from duckdb/src/function/scalar/string/regexp.cpp and regexp/*.cpp @ rust-port-baseline
//
//! The regular expression functions, over the `regex` crate instead of RE2. Both are linear-time
//! engines without backreferences, and their syntax mostly agrees.

use super::bit_functions::strict_rows;
use super::nested::row_wise;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use regex::{Regex, RegexBuilder};
use std::cell::RefCell;
use std::collections::HashMap;

/// The message RE2 has for what the `regex` crate reports as `error`, which shows the pattern with
/// carets under the part it means.
fn regex_error(error: &str, pattern: &str) -> String {
    let lines: Vec<&str> = error.lines().collect();
    let reason = lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("error: "))
        .unwrap_or("");
    let span = lines
        .iter()
        .position(|line| line.trim_start().starts_with('^'))
        .filter(|at| *at > 0)
        .and_then(|at| {
            let (text, carets) = (lines[at - 1].trim_start(), lines[at]);
            let indent = lines[at - 1].len() - text.len();
            let start = carets.find('^')?.checked_sub(indent)?;
            let end = carets.rfind('^')?.checked_sub(indent)? + 1;
            text.chars().skip(start).take(end - start).collect::<String>().into()
        })
        .unwrap_or_else(|| pattern.to_string());
    match reason {
        "unclosed group" => format!("missing ): {pattern}"),
        "unclosed character class" => format!("missing ]: {span}"),
        "repetition operator missing expression" => format!("no argument for repetition operator: {span}"),
        "unrecognized escape sequence" | "invalid hexadecimal digit" | "invalid Unicode scalar value" => {
            format!("invalid escape sequence: {span}")
        }
        _ => format!("Invalid regular expression: {error}"),
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

/// How the option letters of `ParseRegexOptions` read.
#[derive(Debug, Default, Clone, Copy)]
struct Options {
    case_insensitive: bool,
    dot_matches_newline: bool,
    multi_line: bool,
    literal: bool,
    global: bool,
}

fn parse_options(options: &str) -> Result<Options> {
    let mut parsed = Options::default();
    for option in options.chars() {
        match option {
            'c' => parsed.case_insensitive = false,
            'i' => parsed.case_insensitive = true,
            'l' => parsed.literal = true,
            'm' | 'n' | 'p' => parsed.multi_line = true,
            'g' => parsed.global = true,
            's' => parsed.dot_matches_newline = true,
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Unrecognized Regex option {other}"),
                ));
            }
        }
    }
    Ok(parsed)
}

fn compile(pattern: &str, options: Options, anchored: bool) -> Result<Regex> {
    let key = format!("{options:?}{anchored}{pattern}");
    if let Some(regex) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Ok(regex);
    }
    let source = if options.literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let source = if anchored { format!("^(?:{source})$") } else { source };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(options.case_insensitive)
        .dot_matches_new_line(options.dot_matches_newline)
        .multi_line(options.multi_line)
        .build()
        .map_err(|error| Error::new(ErrorKind::InvalidInput, regex_error(&error.to_string(), pattern)))?;
    CACHE.with(|cache| cache.borrow_mut().insert(key, regex.clone()));
    Ok(regex)
}

fn text(value: &Value) -> String {
    match value.data() {
        ValueData::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => unreachable!("a string"),
    }
}

fn options_of(row: &[Value], at: usize) -> Result<Options> {
    match row.get(at) {
        Some(options) => parse_options(&text(options)),
        None => Ok(Options::default()),
    }
}

fn regexp_matches(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let regex = compile(&text(&row[1]), options_of(row, 2)?, false)?;
        Ok(Value::boolean(regex.is_match(&text(&row[0]))))
    })
}

fn regexp_full_match(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let regex = compile(&text(&row[1]), options_of(row, 2)?, true)?;
        Ok(Value::boolean(regex.is_match(&text(&row[0]))))
    })
}

/// RE2's `\1` in a rewrite string, as the `regex` crate wants it.
fn rewrite(replacement: &str) -> String {
    let mut out = String::with_capacity(replacement.len());
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(digit) if digit.is_ascii_digit() => out += &format!("${{{digit}}}"),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            },
            '$' => out += "$$",
            other => out.push(other),
        }
    }
    out
}

fn regexp_replace(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let options = options_of(row, 3)?;
        let regex = compile(&text(&row[1]), options, false)?;
        let input = text(&row[0]);
        let replacement = rewrite(&text(&row[2]));
        Ok(Value::varchar(if options.global {
            regex.replace_all(&input, replacement.as_str()).into_owned()
        } else {
            regex.replace(&input, replacement.as_str()).into_owned()
        }))
    })
}

fn group_of(row: &[Value]) -> Result<usize> {
    let group = row.get(2).and_then(Value::as_i128).unwrap_or(0);
    usize::try_from(group).map_err(|_| Error::new(ErrorKind::InvalidInput, "Group index must be non-negative"))
}

fn regexp_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let regex = compile(&text(&row[1]), options_of(row, 3)?, false)?;
        let group = group_of(row)?;
        if group >= regex.captures_len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Group index field field must be less than or equal to the number of capture groups",
            ));
        }
        let input = text(&row[0]);
        let found = regex
            .captures(&input)
            .and_then(|c| c.get(group).map(|m| m.as_str().to_string()));
        Ok(Value::varchar(found.unwrap_or_default()))
    })
}

fn regexp_extract_all(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().take(2).any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let regex = compile(&text(&row[1]), options_of(row, 3)?, false)?;
        let group = group_of(row)?;
        if group >= regex.captures_len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Group index field field must be less than or equal to the number of capture groups",
            ));
        }
        let input = text(&row[0]);
        let items = regex
            .captures_iter(&input)
            .map(|c| Value::varchar(c.get(group).map_or("", |m| m.as_str())))
            .collect();
        Value::new(ty.clone(), ValueData::List(items))
    })
}

fn regexp_split(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().take(2).any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let regex = compile(&text(&row[1]), options_of(row, 2)?, false)?;
        let input = text(&row[0]);
        Value::new(
            ty.clone(),
            ValueData::List(regex.split(&input).map(Value::varchar).collect()),
        )
    })
}

fn regexp_escape(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let mut out = String::new();
        for c in text(&row[0]).chars() {
            if !c.is_alphanumeric() && c != '_' {
                out.push('\\');
            }
            out.push(c);
        }
        Ok(Value::varchar(out))
    })
}

pub fn regexp_functions() -> Vec<ScalarFunctionSet> {
    let v = LogicalType::VARCHAR;
    let b = LogicalType::BOOLEAN;
    let i = LogicalType::INTEGER;
    let list = LogicalType::list(LogicalType::VARCHAR);
    let mut sets = Vec::new();
    let mut add = |name: &str, overloads: Vec<(Vec<LogicalType>, LogicalType, ScalarCallback)>| {
        let mut set = ScalarFunctionSet::new(name);
        for (arguments, return_type, callback) in overloads {
            set.add(ScalarFunction::new(name, arguments, return_type, callback));
        }
        sets.push(set);
    };
    for (name, callback) in [
        ("regexp_matches", regexp_matches as ScalarCallback),
        ("regexp_full_match", regexp_full_match),
    ] {
        add(
            name,
            vec![
                (vec![v.clone(), v.clone()], b.clone(), callback),
                (vec![v.clone(), v.clone(), v.clone()], b.clone(), callback),
            ],
        );
    }
    add(
        "regexp_replace",
        vec![
            (
                vec![v.clone(), v.clone(), v.clone()],
                v.clone(),
                regexp_replace as ScalarCallback,
            ),
            (
                vec![v.clone(), v.clone(), v.clone(), v.clone()],
                v.clone(),
                regexp_replace,
            ),
        ],
    );
    add(
        "regexp_extract",
        vec![
            (vec![v.clone(), v.clone()], v.clone(), regexp_extract as ScalarCallback),
            (vec![v.clone(), v.clone(), i.clone()], v.clone(), regexp_extract),
            (
                vec![v.clone(), v.clone(), i.clone(), v.clone()],
                v.clone(),
                regexp_extract,
            ),
        ],
    );
    add(
        "regexp_extract_all",
        vec![
            (
                vec![v.clone(), v.clone()],
                list.clone(),
                regexp_extract_all as ScalarCallback,
            ),
            (vec![v.clone(), v.clone(), i.clone()], list.clone(), regexp_extract_all),
            (
                vec![v.clone(), v.clone(), i.clone(), v.clone()],
                list.clone(),
                regexp_extract_all,
            ),
        ],
    );
    for name in ["string_split_regex", "str_split_regex", "regexp_split_to_array"] {
        add(
            name,
            vec![
                (vec![v.clone(), v.clone()], list.clone(), regexp_split as ScalarCallback),
                (vec![v.clone(), v.clone(), v.clone()], list.clone(), regexp_split),
            ],
        );
    }
    add(
        "regexp_escape",
        vec![(vec![v.clone()], v.clone(), regexp_escape as ScalarCallback)],
    );
    sets
}
