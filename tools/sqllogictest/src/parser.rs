// Ported from duckdb/test/sqlite/sqllogic_parser.cpp and the parsing half of
// sqllogic_test_runner.cpp (ExecuteScript) @ rust-port-baseline.
//
// Unlike the C++ runner, which parses and executes in one streaming pass, this parses a whole
// file into a record tree first. Loops become nested records, and parse-time directives
// (`skipif <system>`, `mode skip`) are resolved here.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: Arc<str>,
    /// 1-based line number.
    pub line: usize,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

#[derive(Debug)]
pub struct ParseError {
    pub location: Location,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortStyle {
    NoSort,
    RowSort,
    ValueSort,
}

impl SortStyle {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nosort" | "none" => Some(Self::NoSort),
            "rowsort" | "sort" => Some(Self::RowSort),
            "valuesort" => Some(Self::ValueSort),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    NotEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}

/// A `skipif`/`onlyif` condition on a loop variable, e.g. `skipif i=2`.
#[derive(Debug, Clone)]
pub struct Condition {
    pub keyword: String,
    pub value: String,
    pub comparison: Comparison,
    pub skip_if: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedResult {
    Success,
    /// `statement error`, optionally with an expected message (empty = any error).
    Error(String),
    /// `statement maybe`: success or the given error.
    Maybe(String),
    /// `statement debug`: anything goes.
    DontCare,
}

#[derive(Debug, Clone)]
pub enum LoopKind {
    Range { start: i64, end: i64 },
    Foreach(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum Record {
    Statement {
        location: Location,
        expected: ExpectedResult,
        sql: String,
        connection: Option<String>,
        conditions: Vec<Condition>,
    },
    Query {
        location: Location,
        column_count: usize,
        sort_style: SortStyle,
        label: Option<String>,
        sql: String,
        expected: Vec<String>,
        connection: Option<String>,
        conditions: Vec<Condition>,
    },
    Loop {
        location: Location,
        iterator: String,
        kind: LoopKind,
        parallel: bool,
        body: Vec<Record>,
    },
    HashThreshold(usize),
    Halt,
    /// `mode output_hash`, `mode output_result`, `mode debug` and friends.
    Mode(String),
    SetIgnoreErrorMessages(Vec<String>),
    SetAlwaysFailErrorMessages(Vec<String>),
    SetSeed(String),
    /// `set query_timeout <seconds>`: how long one statement may run, 0 for no limit.
    SetQueryTimeout(f64),
    SetVariable {
        name: String,
        value: String,
    },
    ResetLabel(String),
    Require {
        location: Location,
        params: Vec<String>,
    },
    RequireEnv {
        location: Location,
        exclude: bool,
        params: Vec<String>,
    },
    TestEnv {
        name: String,
        default: String,
    },
    Load {
        location: Location,
        path: Option<String>,
        read_only: bool,
        /// the storage version a new database file is written in
        version: Option<String>,
    },
    Restart,
    Reconnect,
    Sleep(std::time::Duration),
    Unzip {
        location: Location,
        input: String,
        output: Option<String>,
    },
    Continue {
        location: Location,
        conditions: Vec<Condition>,
    },
}

/// A parsed test file.
#[derive(Debug, Clone)]
pub struct TestFile {
    pub path: PathBuf,
    pub records: Vec<Record>,
    pub tags: Vec<String>,
    /// Number of `statement`/`query` records dropped by `mode skip` blocks.
    pub mode_skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    SkipIf,
    OnlyIf,
    Statement,
    Query,
    HashThreshold,
    Halt,
    Mode,
    Set,
    Reset,
    Loop,
    Foreach,
    ConcurrentLoop,
    ConcurrentForeach,
    EndLoop,
    Require,
    RequireEnv,
    RequireEnvNot,
    TestEnv,
    Load,
    Restart,
    Reconnect,
    Sleep,
    Unzip,
    Tags,
    Continue,
    Include,
}

impl Token {
    fn from_command(s: &str) -> Option<Self> {
        Some(match s {
            "skipif" => Self::SkipIf,
            "onlyif" => Self::OnlyIf,
            "statement" => Self::Statement,
            "query" => Self::Query,
            "hash-threshold" => Self::HashThreshold,
            "halt" => Self::Halt,
            "mode" => Self::Mode,
            "set" => Self::Set,
            "reset" => Self::Reset,
            "loop" => Self::Loop,
            "concurrentloop" => Self::ConcurrentLoop,
            "foreach" => Self::Foreach,
            "concurrentforeach" => Self::ConcurrentForeach,
            "endloop" => Self::EndLoop,
            "require" => Self::Require,
            "require-env" => Self::RequireEnv,
            "require-env-not" => Self::RequireEnvNot,
            "test-env" => Self::TestEnv,
            "load" => Self::Load,
            "restart" => Self::Restart,
            "reconnect" => Self::Reconnect,
            "sleep" => Self::Sleep,
            "unzip" => Self::Unzip,
            "tags" => Self::Tags,
            "continue" => Self::Continue,
            "include" => Self::Include,
            _ => return None,
        })
    }

    fn is_single_line(self) -> bool {
        !matches!(self, Self::SkipIf | Self::OnlyIf | Self::Statement | Self::Query)
    }

    fn is_test_command(self) -> bool {
        matches!(self, Self::Statement | Self::Query)
    }
}

/// Line cursor over one file (`SQLLogicParser` in C++).
struct Lines {
    file: Arc<str>,
    lines: Vec<String>,
    current: usize,
    seen_statement: bool,
}

fn empty_or_comment(line: &str) -> bool {
    line.is_empty() || line.starts_with('#')
}

impl Lines {
    fn new(file: Arc<str>, text: &str) -> Self {
        let lines = text.split('\n').map(|l| l.replace('\r', "")).collect::<Vec<_>>();
        Self {
            file,
            lines,
            current: 0,
            seen_statement: false,
        }
    }

    fn location(&self) -> Location {
        Location {
            file: self.file.clone(),
            line: self.current + 1,
        }
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            location: self.location(),
            message: message.into(),
        })
    }

    fn line(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(String::as_str)
    }

    fn next_statement(&mut self) -> bool {
        if self.seen_statement {
            while self.line(self.current).is_some_and(|l| !empty_or_comment(l)) {
                self.current += 1;
            }
        }
        self.seen_statement = true;
        while self.line(self.current).is_some_and(empty_or_comment) {
            self.current += 1;
        }
        self.current < self.lines.len()
    }

    fn next_line_empty_or_comment(&self) -> bool {
        self.line(self.current + 1).is_none_or(empty_or_comment)
    }

    fn tokenize(&self) -> Result<(Token, Vec<String>), ParseError> {
        let line = self.line(self.current).unwrap_or("");
        let mut args = line.split(|c: char| c.is_ascii_whitespace()).filter(|s| !s.is_empty());
        let Some(command) = args.next() else {
            return self.fail("Empty line!?");
        };
        let Some(token) = Token::from_command(command) else {
            return self.fail(format!("Unrecognized parameter {command}"));
        };
        Ok((token, args.map(str::to_string).collect()))
    }

    fn extract_statement(&mut self) -> String {
        let mut parts = Vec::new();
        while let Some(line) = self.line(self.current) {
            if empty_or_comment(line) || line == "----" {
                break;
            }
            parts.push(line.to_string());
            self.current += 1;
        }
        parts.join("\n")
    }

    fn extract_expected_result(&mut self) -> Vec<String> {
        if self.line(self.current) == Some("----") {
            self.current += 1;
        }
        let mut result = Vec::new();
        while let Some(line) = self.line(self.current) {
            if line.is_empty() {
                break;
            }
            result.push(line.to_string());
            self.current += 1;
        }
        result
    }

    fn extract_expected_error(
        &mut self,
        expects_message: bool,
        original_sqlite_test: bool,
    ) -> Result<String, ParseError> {
        if self.line(self.current) != Some("----") {
            if expects_message && !original_sqlite_test {
                return self.fail(
                    "Failed to parse statement: statement error and maybe needs to have an expected error message",
                );
            }
            return Ok(String::new());
        }
        if !expects_message {
            return self.fail(
                "Failed to parse statement: only statement error or maybe can have an expected error message, not statement ok",
            );
        }
        self.current += 1;
        Ok(self.extract_expected_result().join("\n"))
    }
}

/// Splits a `set ignore_error_messages a, b c, d` parameter list on commas.
fn parse_message_list(params: &[String]) -> Vec<String> {
    params
        .join(" ")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn try_parse_conditions(
    lines: &Lines,
    text: &str,
    skip_if: bool,
    out: &mut Vec<Condition>,
) -> Result<bool, ParseError> {
    if !text.contains(['=', '>', '<']) {
        return Ok(false);
    }
    const COMPARATORS: [(&str, Comparison); 6] = [
        ("<>", Comparison::NotEqual),
        (">=", Comparison::GreaterEqual),
        (">", Comparison::Greater),
        ("<=", Comparison::LessEqual),
        ("<", Comparison::Less),
        ("=", Comparison::Equal),
    ];
    for condition in text.split("&&") {
        let Some((op, comparison)) = COMPARATORS.iter().find(|(op, _)| condition.contains(op)) else {
            return lines.fail("skipif/onlyif must be in the form of x=y or x>y, potentially separated by &&");
        };
        // StringUtil::Split drops empty pieces, so `x==1` parses like `x=1`.
        let splits: Vec<&str> = condition.split(op).filter(|p| !p.is_empty()).collect();
        if splits.len() != 2 {
            return lines.fail("skipif/onlyif must be in the form of x=y or x>y, potentially separated by &&");
        }
        out.push(Condition {
            keyword: splits[0].trim().to_string(),
            value: splits[1].trim().to_string(),
            comparison: *comparison,
            skip_if,
        });
    }
    Ok(true)
}

struct FileParser {
    original_sqlite_test: bool,
    tags: Vec<String>,
    tags_seen: bool,
    test_command_seen: bool,
    skip_level: usize,
    mode_skipped: usize,
    /// Open loops: (location, iterator, kind, parallel, body collected so far).
    loops: Vec<(Location, String, LoopKind, bool, Vec<Record>)>,
    records: Vec<Record>,
}

impl FileParser {
    fn push(&mut self, record: Record) {
        match self.loops.last_mut() {
            Some(open) => open.4.push(record),
            None => self.records.push(record),
        }
    }

    fn parse_lines(&mut self, lines: &mut Lines, root: &Path) -> Result<(), ParseError> {
        while lines.next_statement() {
            let (mut token, mut params) = lines.tokenize()?;
            if token.is_single_line() && !lines.next_line_empty_or_comment() {
                return lines.fail("all test statements need to be separated by an empty line");
            }
            if token.is_test_command() {
                self.test_command_seen = true;
            }

            let mut conditions = Vec::new();
            let mut skip_statement = false;
            while matches!(token, Token::SkipIf | Token::OnlyIf) {
                let skip_if = token == Token::SkipIf;
                let Some(system) = params.first().map(|p| p.to_lowercase()) else {
                    return lines.fail("skipif/onlyif requires a single parameter (e.g. skipif duckdb)");
                };
                let is_system =
                    self.original_sqlite_test || !try_parse_conditions(lines, &system, skip_if, &mut conditions)?;
                if is_system {
                    let mut ours = system == "duckdb";
                    if self.original_sqlite_test {
                        ours |= system == "postgresql";
                    }
                    if ours == skip_if {
                        skip_statement = true;
                        break;
                    }
                }
                lines.current += 1;
                (token, params) = lines.tokenize()?;
            }
            if skip_statement {
                continue;
            }
            if self.skip_level > 0 && token != Token::Mode {
                if token.is_test_command() {
                    self.mode_skipped += 1;
                }
                continue;
            }
            self.parse_command(lines, root, token, params, conditions)?;
        }
        Ok(())
    }

    fn parse_command(
        &mut self,
        lines: &mut Lines,
        root: &Path,
        token: Token,
        params: Vec<String>,
        conditions: Vec<Condition>,
    ) -> Result<(), ParseError> {
        let location = lines.location();
        match token {
            Token::SkipIf | Token::OnlyIf => unreachable!("handled by the caller"),
            Token::Statement => {
                let Some(kind) = params.first() else {
                    return lines.fail("statement requires at least one parameter (statement ok/error)");
                };
                let mut expected = match kind.as_str() {
                    "ok" => ExpectedResult::Success,
                    "error" => ExpectedResult::Error(String::new()),
                    "maybe" => ExpectedResult::Maybe(String::new()),
                    "debug" => ExpectedResult::DontCare,
                    "debug_skip" => {
                        self.skip_level += 1;
                        ExpectedResult::DontCare
                    }
                    _ => return lines.fail("statement argument should be 'ok' or 'error"),
                };
                lines.current += 1;
                let sql = lines.extract_statement();
                if sql.is_empty() {
                    return lines.fail("Unexpected empty statement text");
                }
                let expects_message = matches!(expected, ExpectedResult::Error(_) | ExpectedResult::Maybe(_));
                let message = lines.extract_expected_error(expects_message, self.original_sqlite_test)?;
                match &mut expected {
                    ExpectedResult::Error(m) | ExpectedResult::Maybe(m) => *m = message,
                    _ => {}
                }
                self.push(Record::Statement {
                    location,
                    expected,
                    sql,
                    connection: params.get(1).cloned(),
                    conditions,
                });
            }
            Token::Query => {
                let Some(column_text) = params.first() else {
                    return lines.fail("query requires at least one parameter (query III)");
                };
                if let Some(c) = column_text.chars().find(|c| !matches!(c, 'T' | 'I' | 'R')) {
                    return lines.fail(format!(
                        "unknown type character '{c}' in string, expected T, I or R only"
                    ));
                }
                let column_count = column_text.chars().count();
                let (sort_style, connection) = match params.get(1) {
                    Some(p) => match SortStyle::parse(p) {
                        Some(style) => (style, None),
                        None => (SortStyle::NoSort, Some(p.clone())),
                    },
                    None => (SortStyle::NoSort, None),
                };
                lines.current += 1;
                let sql = lines.extract_statement();
                let expected = lines.extract_expected_result();
                self.push(Record::Query {
                    location,
                    column_count,
                    sort_style,
                    label: params.get(2).cloned(),
                    sql,
                    expected,
                    connection,
                    conditions,
                });
            }
            Token::HashThreshold => {
                let threshold = match params.as_slice() {
                    [p] => p.parse().or_else(|_| lines.fail("hash-threshold must be a number"))?,
                    _ => return lines.fail("hash-threshold requires a parameter"),
                };
                self.push(Record::HashThreshold(threshold));
            }
            Token::Halt => {
                // Stop parsing: nothing after `halt` runs.
                lines.current = lines.lines.len();
                self.push(Record::Halt);
            }
            Token::Mode => {
                let Some(mode) = params.first() else {
                    return lines.fail("mode requires at least one parameter");
                };
                match mode.as_str() {
                    "skip" => self.skip_level += 1,
                    "unskip" => self.skip_level = self.skip_level.saturating_sub(1),
                    _ => self.push(Record::Mode(mode.clone())),
                }
            }
            Token::Set => {
                let Some(kind) = params.first() else {
                    return lines.fail("set requires at least 1 parameter (e.g. set ignore_error_messages HTTP Error)");
                };
                let record = match kind.as_str() {
                    "ignore_error_messages" => Record::SetIgnoreErrorMessages(parse_message_list(&params[1..])),
                    "always_fail_error_messages" => {
                        Record::SetAlwaysFailErrorMessages(parse_message_list(&params[1..]))
                    }
                    "seed" => match params.as_slice() {
                        [_, seed] => Record::SetSeed(seed.clone()),
                        _ => return lines.fail("set seed requires a single seed value"),
                    },
                    // `SQLLogicTestRunner::ExecuteFile`: seconds between 0 and 3600, NaN and infinity refused
                    "query_timeout" => match params.as_slice() {
                        [_, seconds] => match seconds.trim().parse::<f64>() {
                            Ok(timeout) if (0.0..=3600.0).contains(&timeout) => Record::SetQueryTimeout(timeout),
                            _ => {
                                return lines.fail(
                                    "set query_timeout requires a number of seconds between 0 and 3600 (0 disables)",
                                );
                            }
                        },
                        _ => return lines.fail("set query_timeout requires a single value (seconds, 0 disables)"),
                    },
                    "variable" => match params.as_slice() {
                        [_, name, value] => Record::SetVariable {
                            name: name.clone(),
                            value: value.clone(),
                        },
                        _ => {
                            return lines.fail("set variable requires two parameters (name value)");
                        }
                    },
                    other => return lines.fail(format!("unrecognized set parameter: {other}")),
                };
                self.push(record);
            }
            Token::Reset => match params.as_slice() {
                [kind, label] if kind.eq_ignore_ascii_case("label") => self.push(Record::ResetLabel(label.clone())),
                [kind, _] => return lines.fail(format!("unrecognized reset parameter: {kind}")),
                _ => return lines.fail("Expected reset [type] [name] (e.g reset label my_label)"),
            },
            Token::Loop | Token::ConcurrentLoop => {
                let [iterator, start, end] = params.as_slice() else {
                    return lines.fail("Expected loop [iterator_name] [start] [end] (e.g. loop i 1 300)");
                };
                let (Ok(start), Ok(end)) = (start.parse(), end.parse()) else {
                    return lines.fail("loop_start and loop_end must be a number");
                };
                let kind = LoopKind::Range { start, end };
                self.loops.push((
                    location,
                    iterator.clone(),
                    kind,
                    token == Token::ConcurrentLoop,
                    Vec::new(),
                ));
            }
            Token::Foreach | Token::ConcurrentForeach => {
                if params.len() < 2 {
                    return lines.fail(
                        "expected foreach [iterator_name] [m1] [m2] [etc...] (e.g. foreach type integer smallint float)",
                    );
                }
                let kind = LoopKind::Foreach(params[1..].to_vec());
                let parallel = token == Token::ConcurrentForeach;
                self.loops
                    .push((location, params[0].clone(), kind, parallel, Vec::new()));
            }
            Token::EndLoop => {
                let Some((location, iterator, kind, parallel, body)) = self.loops.pop() else {
                    return lines.fail("endloop without active loop!");
                };
                self.push(Record::Loop {
                    location,
                    iterator,
                    kind,
                    parallel,
                    body,
                });
            }
            Token::Require => {
                if params.is_empty() {
                    return lines.fail("require requires a single parameter");
                }
                self.push(Record::Require { location, params });
            }
            Token::RequireEnv | Token::RequireEnvNot => {
                let exclude = token == Token::RequireEnvNot;
                if !self.loops.is_empty() {
                    return lines.fail("require-env cannot be called in a loop");
                }
                if params.is_empty() {
                    return lines.fail("require-env requires <env name> [expected value ...]");
                }
                self.push(Record::RequireEnv {
                    location,
                    exclude,
                    params,
                });
            }
            Token::TestEnv => {
                if !self.loops.is_empty() {
                    return lines.fail("test-env cannot be called in a loop");
                }
                let [name, default] = params.as_slice() else {
                    return lines.fail("test-env requires 2 arguments: <env name> <default env val>");
                };
                self.push(Record::TestEnv {
                    name: name.clone(),
                    default: default.clone(),
                });
            }
            Token::Load => {
                let read_only = match params.get(1) {
                    None => false,
                    Some(p) if p.eq_ignore_ascii_case("readonly") => true,
                    Some(p) if p.eq_ignore_ascii_case("readwrite") => false,
                    Some(p) => {
                        return lines.fail(format!(
                            "parameter to 'load' is invalid, received '{p}', accepted options are 'readonly' and 'readwrite"
                        ));
                    }
                };
                self.push(Record::Load {
                    location,
                    path: params.first().cloned(),
                    read_only,
                    version: params.get(2).cloned(),
                });
            }
            Token::Restart => self.push(Record::Restart),
            Token::Reconnect => self.push(Record::Reconnect),
            Token::Sleep => {
                let [amount, unit] = params.as_slice() else {
                    return lines.fail("sleep requires two parameter (e.g. sleep 1 second)");
                };
                let Ok(amount) = amount.parse::<u64>() else {
                    return lines.fail("sleep duration must be a number");
                };
                let duration = match unit.as_str() {
                    "second" | "seconds" | "sec" => std::time::Duration::from_secs(amount),
                    "millisecond" | "milliseconds" | "milli" => std::time::Duration::from_millis(amount),
                    "microsecond" | "microseconds" | "micro" => std::time::Duration::from_micros(amount),
                    "nanosecond" | "nanoseconds" | "nano" => std::time::Duration::from_nanos(amount),
                    _ => {
                        return lines.fail(format!(
                            "Unrecognized sleep mode - expected second/millisecond/microescond/nanosecond: {unit}"
                        ));
                    }
                };
                self.push(Record::Sleep(duration));
            }
            Token::Unzip => {
                if !matches!(params.len(), 1 | 2) {
                    return lines.fail(
                        "unzip requires 1 argument: <path/to/file.db.gz> [optional: <path/to/unzipped_file.db>]",
                    );
                }
                if !params[0].ends_with(".gz") {
                    return lines.fail("unzip: input has not a GZIP extension");
                }
                let output = params.get(1).filter(|p| *p != "NULL").cloned();
                self.push(Record::Unzip {
                    location,
                    input: params[0].clone(),
                    output,
                });
            }
            Token::Tags => {
                if self.test_command_seen {
                    return lines.fail("tags expression must precede test commands");
                }
                if self.tags_seen {
                    return lines.fail("tags may be only specified once");
                }
                if params.is_empty() {
                    return lines.fail("tags requires >= 1 argument, e.g.: <tag1> [tag2 .. tagN]");
                }
                self.tags_seen = true;
                self.tags.splice(0..0, params);
            }
            Token::Continue => {
                if self.loops.is_empty() {
                    return lines.fail("continue cannot be called outside of a loop");
                }
                self.push(Record::Continue { location, conditions });
            }
            Token::Include => {
                let [path] = params.as_slice() else {
                    return lines.fail("Expected include <path>");
                };
                let full = root.join(path);
                let text = std::fs::read_to_string(&full)
                    .or_else(|e| lines.fail(format!("Failed to open include file {path}: {e}")))?;
                let mut include = Lines::new(Arc::from(path.as_str()), &text);
                self.parse_lines(&mut include, root)?;
            }
        }
        Ok(())
    }
}

/// Parses `text` as the test file `name` (a path relative to `root`, the DuckDB checkout).
pub fn parse_str(name: &str, text: &str, root: &Path) -> Result<TestFile, ParseError> {
    let mut tags = Vec::new();
    if name.ends_with(".test_slow") {
        tags.push("slow".to_string());
    }
    if name.ends_with(".test_coverage") {
        tags.push("coverage".to_string());
    }
    let mut parser = FileParser {
        original_sqlite_test: is_original_sqlite_test(name),
        tags,
        tags_seen: false,
        test_command_seen: false,
        skip_level: 0,
        mode_skipped: 0,
        loops: Vec::new(),
        records: Vec::new(),
    };
    let mut lines = Lines::new(Arc::from(name), text);
    parser.parse_lines(&mut lines, root)?;
    if !parser.loops.is_empty() {
        return lines.fail("Missing endloop!");
    }
    Ok(TestFile {
        path: PathBuf::from(name),
        records: parser.records,
        tags: parser.tags,
        mode_skipped: parser.mode_skipped,
    })
}

/// Parses the test file at `root/name`.
pub fn parse_file(name: &str, root: &Path) -> Result<TestFile, ParseError> {
    let text = std::fs::read(root.join(name)).map_err(|e| ParseError {
        location: Location {
            file: Arc::from(name),
            line: 0,
        },
        message: format!("could not read file: {e}"),
    })?;
    parse_str(name, &String::from_utf8_lossy(&text), root)
}

/// Mirrors the C++ runner: original SQLite tests convert floats to integers and allow
/// `statement error` without a message.
pub fn is_original_sqlite_test(name: &str) -> bool {
    name.contains("test/sqlite/select") || name.contains("third_party/sqllogictest")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> TestFile {
        parse_str("test/sql/example.test", text, Path::new(".")).unwrap()
    }

    #[test]
    fn statements_and_queries() {
        let file = parse(
            "# name: test/sql/example.test\n\nstatement ok\nCREATE TABLE t(i INTEGER)\n\n\
             query I rowsort label\nSELECT i\nFROM t\n----\n1\n2\n\nstatement error\nSELECT x\n----\nBinder Error\n",
        );
        assert_eq!(file.records.len(), 3);
        match &file.records[1] {
            Record::Query {
                column_count,
                sort_style,
                label,
                sql,
                expected,
                location,
                ..
            } => {
                assert_eq!(*column_count, 1);
                assert_eq!(*sort_style, SortStyle::RowSort);
                assert_eq!(label.as_deref(), Some("label"));
                assert_eq!(sql, "SELECT i\nFROM t");
                assert_eq!(expected, &["1", "2"]);
                assert_eq!(location.line, 6);
            }
            other => panic!("unexpected record {other:?}"),
        }
        match &file.records[2] {
            Record::Statement {
                expected: ExpectedResult::Error(msg),
                ..
            } => assert_eq!(msg, "Binder Error"),
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    fn loops_nest_and_conditions_attach() {
        let file = parse(
            "loop i 0 3\n\nforeach t int bigint\n\nonlyif i=1\nstatement ok\nSELECT ${t}\n\nendloop\n\nendloop\n",
        );
        let Record::Loop {
            body,
            kind: LoopKind::Range { start: 0, end: 3 },
            ..
        } = &file.records[0]
        else {
            panic!("expected outer loop");
        };
        let Record::Loop {
            body: inner,
            kind: LoopKind::Foreach(items),
            ..
        } = &body[0]
        else {
            panic!("expected inner foreach");
        };
        assert_eq!(items, &["int", "bigint"]);
        let Record::Statement { conditions, .. } = &inner[0] else {
            panic!("expected statement")
        };
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].comparison, Comparison::Equal);
        assert!(!conditions[0].skip_if);
    }

    #[test]
    fn skipif_duckdb_and_mode_skip_drop_records() {
        let file = parse(
            "skipif duckdb\nstatement ok\nSELECT 1\n\nonlyif sqlite\nstatement ok\nSELECT 2\n\n\
             mode skip\n\nstatement ok\nSELECT 3\n\nmode unskip\n\nstatement ok\nSELECT 4\n",
        );
        assert_eq!(file.records.len(), 1);
        assert_eq!(file.mode_skipped, 1);
    }

    #[test]
    fn single_line_commands_need_separation() {
        let err = parse_str("x.test", "require parquet\nstatement ok\nSELECT 1\n", Path::new(".")).unwrap_err();
        assert!(err.message.contains("separated by an empty line"));
    }

    #[test]
    fn statement_error_requires_message() {
        let err = parse_str("x.test", "statement error\nSELECT 1\n", Path::new(".")).unwrap_err();
        assert!(err.message.contains("expected error message"));
    }
}
