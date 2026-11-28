// Ported from duckdb/src/function/scalar/string/regexp.cpp and regexp/*.cpp @ rust-port-baseline
//
//! The regular expression functions, over the `regex` crate instead of RE2. Both are linear-time
//! engines without backreferences, and their syntax mostly agrees.

use super::bit_functions::strict_rows;
use super::nested::row_wise;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use pintail_common::types::LogicalType;
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};
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
        "unrecognized escape sequence"
        | "invalid hexadecimal digit"
        | "invalid Unicode scalar value"
        | "hexadecimal literal is not a Unicode scalar value" => {
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
    literal: bool,
    global: bool,
    /// `k`: the input when nothing matches, for `regexp_extract`
    keep: bool,
}

fn parse_options(options: &str) -> Result<Options> {
    let mut parsed = Options::default();
    for option in options.chars() {
        match option {
            'c' => parsed.case_insensitive = false,
            'i' => parsed.case_insensitive = true,
            'l' => parsed.literal = true,
            // newline-sensitive: `.` does not match a newline, as without an option
            'm' | 'n' | 'p' => parsed.dot_matches_newline = false,
            'g' => parsed.global = true,
            'k' => parsed.keep = true,
            's' => parsed.dot_matches_newline = true,
            ' ' | '\t' | '\n' => {}
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

/// RE2's Perl mode refuses a repetition operator right after another: `a**`, `a++` (a possessive
/// quantifier elsewhere), `a{2}*`; only `?` may follow, once, for a non-greedy repetition.
fn check_stacked_repetition(pattern: &str) -> Result<()> {
    let bytes = pattern.as_bytes();
    let mut last_repeat: Option<usize> = None;
    let mut at = 0;
    let counted = |at: usize| -> Option<usize> {
        // `{n}`, `{n,}`, `{n,m}`: the end of it
        let rest = &pattern[at..];
        let close = rest.find('}')?;
        let inner = &rest[1..close];
        let (low, high) = inner.split_once(',').unwrap_or((inner, "0"));
        (!low.is_empty() && low.bytes().all(|b| b.is_ascii_digit()) && high.bytes().all(|b| b.is_ascii_digit()))
            .then_some(at + close + 1)
    };
    while at < bytes.len() {
        let byte = bytes[at];
        let repeat_end = match byte {
            b'*' | b'+' | b'?' => Some(at + 1),
            b'{' => counted(at),
            _ => None,
        };
        if let Some(mut end) = repeat_end {
            if bytes.get(end) == Some(&b'?') {
                end += 1;
            }
            if let Some(start) = last_repeat {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("bad repetition operator: {}", &pattern[start..end]),
                ));
            }
            last_repeat = Some(at);
            at = end;
            continue;
        }
        last_repeat = None;
        match byte {
            b'\\' => at += 2,
            b'[' => {
                // a class up to its `]`, which may come first
                at += 1;
                if bytes.get(at) == Some(&b'^') {
                    at += 1;
                }
                if bytes.get(at) == Some(&b']') {
                    at += 1;
                }
                while at < bytes.len() && bytes[at] != b']' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at += 1;
            }
            b'(' if bytes.get(at + 1) == Some(&b'?') => at += 2,
            _ => at += 1,
        }
    }
    Ok(())
}

fn compile(pattern: &str, options: Options, anchored: bool) -> Result<Regex> {
    let key = format!("{options:?}{anchored}{pattern}");
    if let Some(regex) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Ok(regex);
    }
    if !options.literal {
        check_stacked_repetition(pattern)?;
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
        // RE2 reads `\123` as an octal character code
        .octal(true)
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

/// `ParseRegexOptions`: `g` only for `regexp_replace`, `k` only for `regexp_extract`.
fn options_for(row: &[Value], at: usize, global_ok: bool, keep_ok: bool) -> Result<Options> {
    let options = options_of(row, at)?;
    if options.global && !global_ok {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Option 'g' (global replace) is only valid for regexp_replace",
        ));
    }
    if options.keep && !keep_ok {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Option 'k' (keep input on no match) is only valid for regexp_extract",
        ));
    }
    Ok(options)
}

fn options_of(row: &[Value], at: usize) -> Result<Options> {
    match row.get(at) {
        Some(options) => parse_options(&text(options)),
        None => Ok(Options::default()),
    }
}

fn regexp_matches(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let regex = compile(&text(&row[1]), options_for(row, 2, false, false)?, false)?;
        Ok(Value::boolean(regex.is_match(&text(&row[0]))))
    })
}

fn regexp_full_match(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let regex = compile(&text(&row[1]), options_for(row, 2, false, false)?, true)?;
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

/// RE2's `CheckRewriteString`: a backslash takes a digit or another backslash, and the
/// groups it names exist.
fn check_rewrite(replacement: &str, groups: usize) -> Result<()> {
    let invalid = |message: String| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid replacement string for regexp_replace: {message}"),
        )
    };
    let mut max_token = 0;
    let mut bytes = replacement.bytes();
    while let Some(c) = bytes.next() {
        if c != b'\\' {
            continue;
        }
        match bytes.next() {
            None => return Err(invalid("Rewrite schema error: '\\' not allowed at end.".to_string())),
            Some(b'\\') => {}
            Some(digit) if digit.is_ascii_digit() => max_token = max_token.max(usize::from(digit - b'0')),
            Some(_) => {
                return Err(invalid(
                    "Rewrite schema error: '\\' must be followed by a digit or '\\'.".to_string(),
                ));
            }
        }
    }
    if max_token > groups {
        return Err(invalid(format!(
            "Rewrite schema requests {max_token} matches, but the regexp only has {groups} parenthesized \
             subexpressions."
        )));
    }
    Ok(())
}

fn regexp_replace(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let options = options_for(row, 3, true, false)?;
        let regex = compile(&text(&row[1]), options, false)?;
        let input = text(&row[0]);
        let replacement = text(&row[2]);
        check_rewrite(&replacement, regex.captures_len() - 1)?;
        let replacement = rewrite(&replacement);
        Ok(Value::varchar(if options.global {
            regex.replace_all(&input, replacement.as_str()).into_owned()
        } else {
            regex.replace(&input, replacement.as_str()).into_owned()
        }))
    })
}

/// The group argument: 0 without one, `None` when it is NULL.
fn group_of(row: &[Value]) -> Option<i128> {
    match row.get(2) {
        None => Some(0),
        Some(value) if value.logical_type().id() == pintail_common::types::LogicalTypeId::Varchar => Some(0),
        Some(value) => value.as_i128(),
    }
}

fn regexp_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        // a NULL group is no NULL result
        if row.iter().take(2).any(Value::is_null) {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let options = options_for(row, 3, false, true)?;
        let regex = compile(&text(&row[1]), options, false)?;
        // `RegexExtractBind`: a NULL group never captures
        let group = match group_of(row) {
            Some(group) if !(0..=9).contains(&group) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Group index must be between 0 and 9!",
                ));
            }
            Some(group) => group,
            None => -1,
        };
        let input = text(&row[0]);
        let no_match = || Value::varchar(if options.keep { input.as_str() } else { "" });
        // `ExtractCaptureGroup`: a group the pattern does not have is no match
        if group < 0 || group as usize >= regex.captures_len() {
            return Ok(no_match());
        }
        let Some(captures) = regex.captures(&input) else {
            return Ok(no_match());
        };
        Ok(Value::varchar(captures.get(group as usize).map_or("", |m| m.as_str())))
    })
}

fn regexp_extract_all(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().take(2).any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let regex = compile(&text(&row[1]), options_for(row, 3, false, false)?, false)?;
        // `GetGroupIndex`: a NULL group makes the result NULL
        let Some(group) = group_of(row) else {
            return Ok(Value::null(ty.clone()));
        };
        // `ExtractAllMatches`: a negative group matches nothing, and one the pattern does not have
        // is an error once something matches
        if group < 0 {
            return Value::list(LogicalType::VARCHAR, Vec::new());
        }
        let groups = regex.captures_len() - 1;
        if group as usize > groups {
            if regex.is_match(&text(&row[0])) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Pattern has {groups} groups. Cannot access group {group}"),
                ));
            }
            return Value::list(LogicalType::VARCHAR, Vec::new());
        }
        let group = group as usize;
        let input = text(&row[0]);
        // `ExtractAllMatches`: an empty match moves one character on, and one at the end is the last
        let mut items = Vec::new();
        let mut start = 0;
        while start <= input.len() {
            let Some(captures) = regex.captures_at(&input, start) else {
                break;
            };
            let matched = captures.get(0).expect("the whole match");
            // a group that took no part in the match is NULL
            items.push(
                captures
                    .get(group)
                    .map_or(Value::null(LogicalType::VARCHAR), |m| Value::varchar(m.as_str())),
            );
            start = matched.end();
            if matched.is_empty() {
                start += input[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        Value::new(ty.clone(), ValueData::List(items))
    })
}

/// `StringSplitter::Split` with `RegexpStringSplit`: a match that is empty where the part starts
/// splits off one character, and an empty pattern splits an empty string into nothing.
fn split_regex<'a>(input: &'a str, pattern: &str, regex: &regex::Regex) -> Vec<&'a str> {
    if input.is_empty() && pattern.is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < input.len() {
        let Some(found) = regex.find_at(input, start) else {
            break;
        };
        let (mut position, size) = (found.start(), found.len());
        if size == 0 && position == start {
            position += input[start..].chars().next().map_or(1, char::len_utf8);
            if position == input.len() {
                break;
            }
        }
        parts.push(&input[start..position]);
        start = position + size;
    }
    parts.push(&input[start..]);
    parts
}

fn regexp_split(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().take(2).any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        let pattern = text(&row[1]);
        let regex = compile(&pattern, options_for(row, 2, false, false)?, false)?;
        let input = text(&row[0]);
        let parts = split_regex(&input, &pattern, &regex);
        Value::new(
            ty.clone(),
            ValueData::List(parts.into_iter().map(Value::varchar).collect()),
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

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

/// `regexp_extract(string, pattern, ['a', 'b'][, options])` and the same of
/// `regexp_extract_all`: the groups as the fields of a STRUCT (`ParseGroupNameList`), of every
/// match for `regexp_extract_all`.
pub(crate) fn bind_regexp_struct(
    name: &str,
    children: Vec<crate::planner::expression::BoundExpr>,
) -> Result<crate::planner::expression::BoundExpr> {
    use crate::planner::expression::BoundExpr;
    let all = name == "regexp_extract_all";
    let fold = |expr: &BoundExpr| crate::execution::expression_executor::evaluate_scalar(expr);
    // `TryParseConstantPattern`: a NULL is no constant pattern either
    let pattern = if children[1].is_foldable() {
        Some(fold(&children[1].clone().cast_to(&LogicalType::VARCHAR, false))?).filter(|p| !p.is_null())
    } else {
        None
    };
    let Some(pattern) = pattern else {
        return Err(binder_error(if all {
            format!("{name} with LIST requires a constant pattern")
        } else {
            format!("{name} with LIST of group names requires a constant pattern")
        }));
    };
    let mut options = Options::default();
    if let Some(given) = children.get(3) {
        if !given.is_foldable() {
            return Err(binder_error(format!(
                "Argument #4 in function {name} must be a constant expression"
            )));
        }
        let given = fold(given)?;
        if given.is_null() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Regex options field must not be NULL",
            ));
        }
        options = parse_options(&given.try_to_string()?)?;
        if options.global || options.keep {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                if options.global {
                    "Option 'g' (global replace) is only valid for regexp_replace"
                } else {
                    "Option 'k' (keep input on no match) is only valid for regexp_extract"
                },
            ));
        }
    }
    if !children[2].is_foldable() {
        return Err(binder_error(format!(
            "Argument #3 in function {name} must be a constant expression"
        )));
    }
    let list = fold(&children[2])?;
    let ValueData::List(items) = list.data() else {
        return Err(binder_error("Group specification must be a non-NULL LIST"));
    };
    if items.is_empty() {
        return Err(binder_error("Group name list must be non-empty"));
    }
    let mut names: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        if item.is_null() {
            return Err(binder_error(format!("NULL group name in {name}")));
        }
        let group = item.try_to_string()?;
        if group.is_empty() {
            return Err(binder_error(format!("Empty group name in {name}")));
        }
        if names.iter().any(|n| n.eq_ignore_ascii_case(&group)) {
            return Err(binder_error(format!("Duplicate group name '{group}' in {name}")));
        }
        names.push(group);
    }
    let regex = compile(&text(&pattern), options, false)
        .map_err(|e| binder_error(format!("Pattern failed to parse: {}", e.message)))?;
    let groups = regex.captures_len() - 1;
    if groups < names.len() {
        return Err(binder_error(format!(
            "Not enough capturing groups ({groups}) for provided names ({})",
            names.len()
        )));
    }
    let fields = LogicalType::struct_type(names.into_iter().map(|n| (n, LogicalType::VARCHAR)).collect());
    let mut arguments = children;
    arguments[0] = arguments[0].clone().cast_to(&LogicalType::VARCHAR, false);
    arguments[1] = arguments[1].clone().cast_to(&LogicalType::VARCHAR, false);
    Ok(if all {
        super::nested::bound_function(name, arguments, LogicalType::list(fields), regexp_extract_all_struct)
    } else {
        super::nested::bound_function(name, arguments, fields, regexp_extract_struct)
    })
}

/// `RegexExtractStructFunction`: the groups of the first match, empty without one.
fn regexp_extract_struct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let count = ty.struct_children().len();
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let regex = compile(&text(&row[1]), options_for(row, 3, false, false)?, false)?;
        let input = text(&row[0]);
        let captures = regex.captures(&input);
        let fields = (1..=count)
            .map(|i| Value::varchar(captures.as_ref().and_then(|c| c.get(i)).map_or("", |m| m.as_str())))
            .collect();
        Value::new(ty.clone(), ValueData::Struct(fields))
    })
}

/// `ExtractStructAllSingleTuple`: the groups of every match, with an unmatched optional group
/// NULL.
fn regexp_extract_all_struct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let element = ty.child_type().expect("a list of structs").clone();
    let count = element.struct_children().len();
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let regex = compile(&text(&row[1]), options_for(row, 3, false, false)?, false)?;
        let input = text(&row[0]);
        let mut items = Vec::new();
        let mut start = 0;
        while start <= input.len() {
            let Some(captures) = regex.captures_at(&input, start) else {
                break;
            };
            let matched = captures.get(0).expect("the whole match");
            let fields = (1..=count)
                .map(|i| match captures.get(i) {
                    Some(m) => Value::varchar(m.as_str()),
                    None => Value::null(LogicalType::VARCHAR),
                })
                .collect();
            items.push(Value::new(element.clone(), ValueData::Struct(fields))?);
            start = matched.end();
            if matched.is_empty() {
                start += input[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        Value::new(ty.clone(), ValueData::List(items))
    })
}

/// `TryParseConstantPattern` in the bind of the regex functions: a constant pattern compiles
/// then, so a pattern that does not parse fails without rows.
pub(crate) fn check_constant_pattern(pattern: &str, options: Option<&str>) -> Result<()> {
    let options = match options {
        Some(options) => parse_options(options)?,
        None => Options::default(),
    };
    // only an error RE2 shares: the regex crate refuses some patterns RE2 takes
    match compile(pattern, options, false) {
        Err(error) if error.message.starts_with("Invalid regular expression:") => Ok(()),
        result => result.map(|_| ()),
    }
}
