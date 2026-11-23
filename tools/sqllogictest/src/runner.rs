// Ported from duckdb/test/sqlite/sqllogic_test_runner.cpp, sqllogic_command.cpp and
// result_helper.cpp (CheckQueryResult / CheckStatementResult) @ rust-port-baseline.

use crate::compare::{self, convert_result, hash_values, sort_values, values_equal};
use crate::engine::{Connection, Database, Engine, QueryOutput};
use crate::parser::{self, Comparison, Condition, ExpectedResult, Location, LoopKind, Record, SortStyle, TestFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// What duckrs says about SQL it does not run yet (`duckrs_core::unported`).
const UNPORTED_MARKER: &str = "is not yet ported to duckrs";

/// Settings shared by every file in a run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The DuckDB checkout; tests run with this as the working directory.
    pub root: PathBuf,
    /// Parent directory for per-file `{TEST_DIR}` scratch directories.
    pub scratch: PathBuf,
    /// Keep executing a file after its first failure (to count passing statements).
    pub continue_on_failure: bool,
    /// The engine's `STANDARD_VECTOR_SIZE`, for `require vector_size`.
    pub vector_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail {
        location: Location,
        message: String,
    },
    Skip(String),
    /// The file could not be parsed, or the harness itself failed.
    Error(String),
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail { .. } => "fail",
            Outcome::Skip(_) => "skip",
            Outcome::Error(_) => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileReport {
    pub path: String,
    pub outcome: Outcome,
    /// `statement`/`query` records executed (loop iterations count individually).
    pub executed: usize,
    pub passed: usize,
    pub mode_skipped: usize,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
struct Failure {
    location: Location,
    message: String,
}

impl Failure {
    fn new(location: &Location, message: impl Into<String>) -> Self {
        Self {
            location: location.clone(),
            message: message.into(),
        }
    }
}

/// How executing a record affects the rest of the file.
enum Flow {
    Next,
    /// `continue` inside a loop: skip the rest of this iteration.
    ContinueLoop,
    /// `halt`, an ignored error message, or a failed `require`: stop the file.
    Stop,
}

#[derive(Debug, Clone)]
struct ActiveLoop {
    name: String,
    value: String,
}

/// State that statements read while executing, possibly from concurrent-loop threads.
struct Shared {
    file_name: String,
    test_dir: PathBuf,
    working_dir: String,
    original_sqlite_test: bool,
    settings: RwLock<Settings>,
    labels: Mutex<HashMap<String, String>>,
    executed: AtomicUsize,
    passed: AtomicUsize,
    stop: AtomicBool,
    stop_reason: Mutex<Option<String>>,
    failure: Mutex<Option<Failure>>,
    continue_on_failure: bool,
}

#[derive(Debug, Clone, Default)]
struct Settings {
    hash_threshold: usize,
    ignore_error_messages: Vec<String>,
    always_fail_error_messages: Vec<String>,
    variables: HashMap<String, String>,
}

impl Shared {
    fn replace_keywords(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (name, value) in &self.settings.read().unwrap().variables {
            out = out
                .replace(&format!("${{{name}}}"), value)
                .replace(&format!("{{{name}}}"), value);
        }
        let test_dir = self.test_dir.to_string_lossy();
        let base_test_name = self.file_name.replace('/', "_");
        for (pattern, value) in [
            ("{TEST_DIR}", test_dir.as_ref()),
            ("__TEST_DIR__", test_dir.as_ref()),
            ("{WORKING_DIRECTORY}", self.working_dir.as_str()),
            ("__WORKING_DIRECTORY__", self.working_dir.as_str()),
            ("{TEST_NAME}", self.file_name.as_str()),
            ("{BASE_TEST_NAME}", base_test_name.as_str()),
            ("{BUILD_DIRECTORY}", "build/release"),
            ("__BUILD_DIRECTORY__", "build/release"),
        ] {
            out = out.replace(pattern, value);
        }
        if out.contains("{UUID}") {
            out = out.replace("{UUID}", &pseudo_uuid());
        }
        out
    }

    fn loop_replacement(&self, text: &str, loops: &[ActiveLoop]) -> String {
        let mut out = text.to_string();
        for active in loops {
            let replacement = self.replace_keywords(&active.value);
            let pairs: Vec<(&str, &str)> = if active.name.contains(',') {
                active.name.split(',').zip(replacement.split(',')).collect()
            } else {
                vec![(active.name.as_str(), replacement.as_str())]
            };
            for (name, value) in pairs {
                out = out
                    .replace(&format!("${{{name}}}"), value)
                    .replace(&format!("{{{name}}}"), value);
            }
        }
        out
    }

    fn record_failure(&self, failure: Failure) -> Flow {
        let mut slot = self.failure.lock().unwrap();
        if slot.is_none() {
            *slot = Some(failure);
        }
        if self.continue_on_failure {
            Flow::Next
        } else {
            self.stop.store(true, Ordering::SeqCst);
            Flow::Stop
        }
    }

    fn stop_with(&self, reason: String) -> Flow {
        self.stop.store(true, Ordering::SeqCst);
        self.stop_reason.lock().unwrap().get_or_insert(reason);
        Flow::Stop
    }

    fn skip_error(&self, message: &str) -> Option<String> {
        let settings = self.settings.read().unwrap();
        settings
            .ignore_error_messages
            .iter()
            .find(|m| message.contains(m.as_str()))
            .map(|m| format!("skip on error_message matching '{m}'"))
    }

    fn error_matches(&self, expected: &str, actual: &str) -> bool {
        if actual.contains(expected) || actual.contains(&self.replace_keywords(expected)) {
            return true;
        }
        compare::is_regex_comparison(expected)
            && compare::matches_regex(&format!("{actual}\n"), expected).unwrap_or(false)
    }

    fn run_statement(
        &self,
        con: &mut dyn Connection,
        location: &Location,
        expected: &ExpectedResult,
        sql: &str,
    ) -> Flow {
        let result = con.query(sql);
        let error = result.as_ref().err();
        if let Some(error) = error {
            let settings = self.settings.read().unwrap();
            if settings
                .always_fail_error_messages
                .iter()
                .any(|m| error.contains(m.as_str()))
            {
                drop(settings);
                return self.record_failure(Failure::new(
                    location,
                    format!("Query raised an internal error:\n{error}"),
                ));
            }
        }
        match (expected, error) {
            (ExpectedResult::DontCare, _) | (ExpectedResult::Success, None) | (ExpectedResult::Maybe(_), None) => {
                Flow::Next
            }
            (ExpectedResult::Success, Some(error)) => match self.skip_error(error) {
                Some(reason) => self.stop_with(reason),
                None => self.record_failure(Failure::new(location, format!("Query unexpectedly failed:\n{error}"))),
            },
            (ExpectedResult::Error(_), None) => {
                self.record_failure(Failure::new(location, "Query unexpectedly succeeded!"))
            }
            // a feature duckrs lacks never stands in for the error a test expects
            (ExpectedResult::Error(_), Some(error)) if error.contains(UNPORTED_MARKER) => self.record_failure(
                Failure::new(location, format!("Query failed on a missing feature:\n{error}")),
            ),
            (ExpectedResult::Error(expected) | ExpectedResult::Maybe(expected), Some(error)) => {
                if expected.is_empty() || self.error_matches(expected, error) {
                    return Flow::Next;
                }
                match self.skip_error(error) {
                    Some(reason) => self.stop_with(reason),
                    None => self.record_failure(Failure::new(
                        location,
                        format!("Query failed, but error message did not match expected error message:\nexpected: {expected}\nactual:   {error}"),
                    )),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_query(
        &self,
        con: &mut dyn Connection,
        location: &Location,
        column_count: usize,
        sort_style: SortStyle,
        label: Option<&str>,
        sql: &str,
        expected: &[String],
        loops: &[ActiveLoop],
    ) -> Flow {
        let output = match con.query(sql) {
            Ok(output) => output,
            Err(error) => {
                if let Some(reason) = self.skip_error(&error) {
                    return self.stop_with(reason);
                }
                return self.record_failure(Failure::new(location, format!("Query unexpectedly failed:\n{error}")));
            }
        };
        match self.check_query(&output, column_count, sort_style, label, expected, loops) {
            Ok(()) => Flow::Next,
            Err(message) => self.record_failure(Failure::new(location, message)),
        }
    }

    fn check_query(
        &self,
        output: &QueryOutput,
        expected_column_count: usize,
        sort_style: SortStyle,
        label: Option<&str>,
        expected: &[String],
        loops: &[ActiveLoop],
    ) -> Result<(), String> {
        let result_columns = output.column_count();
        let total_values = output.row_count() * result_columns;
        let hash_threshold = self.settings.read().unwrap().hash_threshold;
        let result_is_hash = expected.len() == 1 && compare::result_is_hash(&expected[0]);
        let compare_hash = label.is_some() || (hash_threshold > 0 && total_values > hash_threshold) || result_is_hash;

        let mut values = convert_result(output, self.original_sqlite_test);
        sort_values(sort_style, &mut values, result_columns)?;

        if compare_hash {
            let hash = hash_values(&values);
            if let Some(label) = label {
                let mut labels = self.labels.lock().unwrap();
                match labels.get(label) {
                    Some(previous) if *previous != hash => {
                        return Err(format!(
                            "Wrong result hash for label {label}:\nexpected {previous}\nactual   {hash}"
                        ));
                    }
                    Some(_) => {}
                    None => {
                        labels.insert(label.to_string(), hash.clone());
                    }
                }
            }
            if result_is_hash && expected[0] != hash {
                return Err(format!("Wrong result hash!\nexpected {}\nactual   {hash}", expected[0]));
            }
            return Ok(());
        }

        let mut expected_column_count = expected_column_count;
        let mut comparison: Vec<String> = match expected.first() {
            Some(first) if expected.len() == 1 && first.starts_with("<FILE>:") => {
                let path = self.loop_replacement(&self.replace_keywords(&first["<FILE>:".len()..]), loops);
                let (values, columns) = compare::load_result_file(&path)?;
                expected_column_count = columns;
                values
            }
            _ => expected.to_vec(),
        };

        let column_mismatch = expected_column_count != result_columns;
        if column_mismatch {
            expected_column_count = result_columns;
        }
        if expected_column_count == 0 {
            return Err("Query returned no columns".into());
        }
        let mut row_wise = expected_column_count > 1 && comparison.len() == output.row_count();
        if !row_wise {
            row_wise = comparison.iter().all(|v| v.contains('\t'));
        }
        let expected_rows = if row_wise {
            comparison.len()
        } else if !comparison.len().is_multiple_of(expected_column_count) {
            return Err(if column_mismatch {
                format!(
                    "Wrong column count in query!\nexpected {} columns, but got {result_columns} columns",
                    expected_column_count
                )
            } else {
                format!(
                    "Error in test! Column count mismatch: expected {expected_column_count} columns, but {} values were supplied",
                    comparison.len()
                )
            });
        } else {
            comparison.len() / expected_column_count
        };
        if expected_rows != output.row_count() {
            return Err(format!(
                "Wrong row count in query!\nexpected {expected_rows} rows, but got {} rows\n{}",
                output.row_count(),
                render_result(&values, result_columns)
            ));
        }
        if row_wise {
            let mut split = Vec::with_capacity(total_values);
            for (i, row) in comparison.iter().enumerate() {
                // StringUtil::Split drops empty pieces, so `a\t\tb` is two columns.
                let parts: Vec<&str> = row.split('\t').filter(|p| !p.is_empty()).collect();
                if parts.len() != expected_column_count {
                    return Err(format!(
                        "Error in test! Column count mismatch after splitting on tab on row {}: expected {expected_column_count} columns, but got {} columns",
                        i + 1,
                        parts.len()
                    ));
                }
                split.extend(parts.into_iter().map(str::to_string));
            }
            comparison = split;
        }
        for (i, (actual, wanted)) in values.iter().zip(&comparison).enumerate() {
            let column = i % expected_column_count;
            let ty = output.types[column];
            if !values_equal(actual, wanted, ty, |s| self.replace_keywords(s)) {
                return Err(format!(
                    "Wrong result in query!\nMismatch on row {}, column {}(index {})\n{actual} <> {wanted}\n{}",
                    i / expected_column_count + 1,
                    output.names.get(column).map_or("", String::as_str),
                    column + 1,
                    render_result(&values, result_columns)
                ));
            }
        }
        if column_mismatch {
            return Err(format!(
                "Wrong column count in query!\nexpected {} columns, but got {result_columns} columns (the values themselves are correct)",
                comparison.len() / output.row_count().max(1)
            ));
        }
        Ok(())
    }
}

fn render_result(values: &[String], columns: usize) -> String {
    if columns == 0 {
        return String::new();
    }
    let mut out = String::from("Actual result:\n");
    for row in values.chunks(columns).take(50) {
        out.push_str(&row.join("\t"));
        out.push('\n');
    }
    out
}

fn pseudo_uuid() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    let a = h.finish();
    h.write_u64(a);
    let b = h.finish();
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn conditions_hold(conditions: &[Condition], loops: &[ActiveLoop], location: &Location) -> Result<bool, Failure> {
    for condition in conditions {
        let Some(active) = loops.iter().rev().find(|l| l.name == condition.keyword) else {
            return Err(Failure::new(
                location,
                format!(
                    "Condition in onlyif/skipif not found: {} must be a loop iterator name",
                    condition.keyword
                ),
            ));
        };
        let holds = match condition.comparison {
            Comparison::Equal => active.value == condition.value,
            Comparison::NotEqual => active.value != condition.value,
            other => {
                let (Ok(l), Ok(r)) = (active.value.parse::<i64>(), condition.value.parse::<i64>()) else {
                    return Err(Failure::new(location, "loop condition comparison requires numbers"));
                };
                match other {
                    Comparison::Greater => l > r,
                    Comparison::GreaterEqual => l >= r,
                    Comparison::Less => l < r,
                    Comparison::LessEqual => l <= r,
                    Comparison::Equal | Comparison::NotEqual => unreachable!(),
                }
            }
        };
        if holds == condition.skip_if {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Expands `<integral>`, `<numeric>`, `!type` and friends in a `foreach` list (`ForEachTokenReplace`).
pub fn expand_foreach(tokens: &[String]) -> Vec<String> {
    const SIGNED: [&str; 5] = ["tinyint", "smallint", "integer", "bigint", "hugeint"];
    const UNSIGNED: [&str; 5] = ["utinyint", "usmallint", "uinteger", "ubigint", "uhugeint"];
    const COMPRESSION: [&str; 9] = [
        "none",
        "uncompressed",
        "rle",
        "bitpacking",
        "dictionary",
        "fsst",
        "dict_fsst",
        "alp",
        "alprd",
    ];
    // The columns of test_all_types().
    const ALL_TYPES_COLUMNS: [&str; 53] = [
        "bool",
        "tinyint",
        "smallint",
        "int",
        "bigint",
        "hugeint",
        "uhugeint",
        "utinyint",
        "usmallint",
        "uint",
        "ubigint",
        "date",
        "time",
        "timestamp",
        "timestamp_s",
        "timestamp_ms",
        "timestamp_ns",
        "time_tz",
        "timestamp_tz",
        "float",
        "double",
        "dec_4_1",
        "dec_9_4",
        "dec_18_6",
        "dec38_10",
        "uuid",
        "interval",
        "varchar",
        "blob",
        "bit",
        "small_enum",
        "medium_enum",
        "large_enum",
        "int_array",
        "double_array",
        "date_array",
        "timestamp_array",
        "timestamptz_array",
        "varchar_array",
        "nested_int_array",
        "struct",
        "struct_of_arrays",
        "array_of_structs",
        "map",
        "union",
        "fixed_int_array",
        "fixed_varchar_array",
        "fixed_nested_int_array",
        "fixed_nested_varchar_array",
        "fixed_struct_array",
        "struct_of_fixed_array",
        "fixed_array_of_int_list",
        "list_of_fixed_int_array",
    ];
    let mut result: Vec<String> = Vec::new();
    for token in tokens {
        let name = token.trim().to_lowercase();
        if let Some(removed) = token.strip_prefix('!') {
            if let Some(pos) = result.iter().position(|t| t == removed) {
                result.remove(pos);
                continue;
            }
            result.push(token.clone());
            continue;
        }
        let all = name == "<alltypes>";
        let numeric = all || name == "<numeric>";
        let integral = numeric || name == "<integral>";
        let signed = integral || name == "<signed>";
        let unsigned = integral || name == "<unsigned>";
        let mut collection = false;
        let mut extend = |items: &[&str]| {
            result.extend(items.iter().map(|s| s.to_string()));
            collection = true;
        };
        if signed {
            extend(&SIGNED);
        }
        if unsigned {
            extend(&UNSIGNED);
        }
        if numeric {
            extend(&["float", "double"]);
        }
        if all {
            extend(&["bool", "interval", "varchar"]);
        }
        if name == "<compression>" {
            extend(&COMPRESSION);
        }
        if name == "<all_types_columns>" {
            extend(&ALL_TYPES_COLUMNS);
        }
        if !collection {
            result.push(token.clone());
        }
    }
    result
}

/// Executes one parsed file.
/// `SQLLogicTestRunner::ConnectToDatabase`: every connection keeps its persistent secrets in the
/// test's own directory, not in the user's.
struct TestDatabase {
    inner: Box<dyn Database>,
    secret_directory: String,
}

impl Database for TestDatabase {
    fn connect(&self) -> Result<Box<dyn Connection>, String> {
        let mut con = self.inner.connect()?;
        // as in C++, a connection made once the secret manager is in use cannot change it
        let _ = con.query(&format!(
            "SET secret_directory='{}'",
            self.secret_directory.replace('\'', "''")
        ));
        Ok(con)
    }
}

struct FileRunner<'a> {
    config: &'a RunConfig,
    engine: &'a dyn Engine,
    shared: Shared,
    db: Option<Box<dyn Database>>,
    db_path: Option<PathBuf>,
    read_only: bool,
    /// the storage version of `load ... <version>`
    storage_version: Option<String>,
    con: Option<Box<dyn Connection>>,
    named: HashMap<String, Box<dyn Connection>>,
}

impl<'a> FileRunner<'a> {
    /// `storage_compatibility_version` of the database open now.
    fn current_storage_version(&mut self) -> Option<String> {
        let con = self.con.as_mut()?;
        let result = con
            .query("SELECT current_setting('storage_compatibility_version')")
            .ok()?;
        result.rows.first()?.first()?.clone()
    }

    fn open(&mut self) -> Result<(), String> {
        self.named.clear();
        self.con = None;
        self.db = None;
        let db =
            self.engine
                .open_with_version(self.db_path.as_deref(), self.read_only, self.storage_version.as_deref())?;
        let db: Box<dyn Database> = Box::new(TestDatabase {
            inner: db,
            secret_directory: self.shared.test_dir.join("test_secret_dir").to_string_lossy().into_owned(),
        });
        self.con = Some(db.connect()?);
        self.db = Some(db);
        Ok(())
    }

    /// `RestartCommand`: the database opened again, with the settings and the search path of the connection
    /// before.
    fn restart(&mut self) -> Result<(), String> {
        // all of them, as C++ copies `DBConfig::options`: the deprecated and debug ones too
        const SETTINGS: &str = "SELECT name, value FROM duckdb_settings(include_all := true)";
        let read = |con: &mut Box<dyn Connection>| -> Vec<(String, Option<String>)> {
            con.query(SETTINGS)
                .map(|output| {
                    output
                        .rows
                        .into_iter()
                        .filter_map(|row| Some((row.first()?.clone()?, row.get(1)?.clone())))
                        .collect()
                })
                .unwrap_or_default()
        };
        let (before, search_path) = match self.con.as_mut() {
            Some(con) => {
                let search_path = con
                    .query("SELECT current_setting('search_path')")
                    .ok()
                    .and_then(|output| output.rows.first()?.first()?.clone());
                (read(con), search_path)
            }
            None => (Vec::new(), None),
        };
        self.open()?;
        let con = self.con.as_mut().expect("database is open");
        let fresh = read(con);
        let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
        for (name, value) in before {
            let Some(value) = value else { continue };
            let unchanged = fresh
                .iter()
                .any(|(n, v)| *n == name && v.as_deref() == Some(value.as_str()));
            if !unchanged {
                // what the new database does not take stays as it is
                let _ = con.query(&format!("SET {name} = {}", quote(&value)));
            }
        }
        if let Some(path) = search_path.filter(|path| !path.is_empty()) {
            let _ = con.query(&format!("SET search_path = {}", quote(&path)));
        }
        Ok(())
    }

    fn run_records(&mut self, records: &[Record], loops: &mut Vec<ActiveLoop>) -> Flow {
        for record in records {
            if self.shared.stop.load(Ordering::SeqCst) {
                return Flow::Stop;
            }
            match self.run_record(record, loops) {
                Flow::Next => {}
                other => return other,
            }
        }
        Flow::Next
    }

    fn run_record(&mut self, record: &Record, loops: &mut Vec<ActiveLoop>) -> Flow {
        let shared = &self.shared;
        match record {
            Record::Statement {
                location,
                expected,
                sql,
                connection,
                conditions,
            } => {
                match conditions_hold(conditions, loops, location) {
                    Ok(true) => {}
                    Ok(false) => return Flow::Next,
                    Err(failure) => return shared.record_failure(failure),
                }
                let sql = shared.loop_replacement(&shared.replace_keywords(sql), loops);
                shared.executed.fetch_add(1, Ordering::Relaxed);
                let failed_before = shared.failure.lock().unwrap().is_some();
                let shared = &self.shared;
                let con = match connection_for(
                    &mut self.con,
                    &mut self.named,
                    self.db.as_deref(),
                    connection.as_deref(),
                ) {
                    Ok(con) => con,
                    Err(e) => return shared.record_failure(Failure::new(location, e)),
                };
                let flow = shared.run_statement(con, location, expected, &sql);
                count_pass(shared, failed_before);
                flow
            }
            Record::Query {
                location,
                column_count,
                sort_style,
                label,
                sql,
                expected,
                connection,
                conditions,
            } => {
                match conditions_hold(conditions, loops, location) {
                    Ok(true) => {}
                    Ok(false) => return Flow::Next,
                    Err(failure) => return shared.record_failure(failure),
                }
                let sql = shared.loop_replacement(&shared.replace_keywords(sql), loops);
                shared.executed.fetch_add(1, Ordering::Relaxed);
                let failed_before = shared.failure.lock().unwrap().is_some();
                let shared = &self.shared;
                let con = match connection_for(
                    &mut self.con,
                    &mut self.named,
                    self.db.as_deref(),
                    connection.as_deref(),
                ) {
                    Ok(con) => con,
                    Err(e) => return shared.record_failure(Failure::new(location, e)),
                };
                let flow = shared.run_query(
                    con,
                    location,
                    *column_count,
                    *sort_style,
                    label.as_deref(),
                    &sql,
                    expected,
                    loops,
                );
                count_pass(shared, failed_before);
                flow
            }
            Record::Loop {
                location,
                iterator,
                kind,
                parallel,
                body,
            } => {
                let values: Vec<String> = match kind {
                    LoopKind::Range { start, end } => (*start..*end).map(|i| i.to_string()).collect(),
                    LoopKind::Foreach(tokens) => expand_foreach(tokens),
                };
                if *parallel {
                    return self.run_concurrent_loop(location, iterator, &values, body, loops);
                }
                for value in values {
                    loops.push(ActiveLoop {
                        name: iterator.clone(),
                        value,
                    });
                    let flow = self.run_records(body, loops);
                    loops.pop();
                    match flow {
                        Flow::Next | Flow::ContinueLoop => {}
                        stop @ Flow::Stop => return stop,
                    }
                }
                Flow::Next
            }
            Record::Continue { location, conditions } => match conditions_hold(conditions, loops, location) {
                Ok(true) => Flow::ContinueLoop,
                Ok(false) => Flow::Next,
                Err(failure) => shared.record_failure(failure),
            },
            Record::HashThreshold(t) => {
                shared.settings.write().unwrap().hash_threshold = *t;
                Flow::Next
            }
            Record::Halt => shared.stop_with("halt".into()),
            Record::Mode(_) => Flow::Next,
            Record::SetIgnoreErrorMessages(m) => {
                shared.settings.write().unwrap().ignore_error_messages = m.clone();
                Flow::Next
            }
            Record::SetAlwaysFailErrorMessages(m) => {
                shared.settings.write().unwrap().always_fail_error_messages = m.clone();
                Flow::Next
            }
            Record::SetSeed(seed) => {
                let con = self.con.as_deref_mut().expect("database is open");
                match con.query(&format!("SELECT SETSEED({seed})")) {
                    Ok(_) => Flow::Next,
                    Err(e) => self.shared.record_failure(Failure {
                        location: Location {
                            file: self.shared.file_name.as_str().into(),
                            line: 0,
                        },
                        message: format!("Failed to set seed: {e}"),
                    }),
                }
            }
            Record::SetVariable { name, value } => {
                shared
                    .settings
                    .write()
                    .unwrap()
                    .variables
                    .insert(name.clone(), value.clone());
                Flow::Next
            }
            Record::ResetLabel(label) => {
                shared.labels.lock().unwrap().remove(label);
                Flow::Next
            }
            Record::Require { location, params } => self.require(location, params),
            Record::RequireEnv { exclude, params, .. } => {
                let reason = format!(
                    "{} {}",
                    if *exclude { "require-env-not" } else { "require-env" },
                    params.join(" ")
                );
                let defined = shared.settings.read().unwrap().variables.get(&params[0]).cloned();
                let actual = defined.or_else(|| std::env::var(&params[0]).ok());
                let accepted = match (&actual, params.len()) {
                    (None, _) => *exclude,
                    (Some(_), 1) => !*exclude,
                    (Some(v), _) => params[1..].contains(v) != *exclude,
                };
                if !accepted {
                    return shared.stop_with(reason);
                }
                if let Some(v) = actual {
                    shared.settings.write().unwrap().variables.insert(params[0].clone(), v);
                }
                Flow::Next
            }
            Record::TestEnv { name, default } => {
                let defined = shared.settings.read().unwrap().variables.get(name).cloned();
                let value = defined.unwrap_or_else(|| default.clone());
                shared.settings.write().unwrap().variables.insert(name.clone(), value);
                Flow::Next
            }
            Record::Load {
                location,
                path,
                read_only,
                version,
            } => {
                let path = path
                    .as_ref()
                    .map(|p| PathBuf::from(shared.loop_replacement(&shared.replace_keywords(p), loops)));
                if let (Some(p), false) = (&path, read_only) {
                    remove_database(p);
                }
                self.db_path = path;
                self.read_only = *read_only;
                // `LoadCommand`: a load without a version takes that of the database before it
                self.storage_version = match version {
                    Some(version) => Some(version.clone()),
                    // only a new file is made in it: a read-only load opens what is there
                    None if !*read_only => self.current_storage_version(),
                    None => None,
                };
                match self.open() {
                    Ok(()) => Flow::Next,
                    Err(e) => self
                        .shared
                        .record_failure(Failure::new(location, format!("Failed to load database: {e}"))),
                }
            }
            Record::Restart | Record::Reconnect => {
                let result = if matches!(record, Record::Restart) {
                    self.restart()
                } else {
                    let db = self.db.as_ref().expect("database is open");
                    db.connect().map(|con| self.con = Some(con))
                };
                match result {
                    Ok(()) => Flow::Next,
                    Err(e) => self.shared.stop_with(format!("harness error: {e}")),
                }
            }
            Record::Sleep(duration) => {
                std::thread::sleep(*duration);
                Flow::Next
            }
            Record::Unzip {
                location,
                input,
                output,
            } => {
                let input = shared.replace_keywords(input);
                let file_name = Path::new(&input).file_name().map(|f| f.to_string_lossy().into_owned());
                let default = format!(
                    "{}/{}",
                    shared.test_dir.display(),
                    file_name.unwrap_or_default().trim_end_matches(".gz")
                );
                let output = output.as_ref().map_or(default, |o| shared.replace_keywords(o));
                match gunzip(&input, &output) {
                    Ok(()) => Flow::Next,
                    Err(e) => shared.record_failure(Failure::new(location, e)),
                }
            }
        }
    }

    fn require(&mut self, location: &Location, params: &[String]) -> Flow {
        let param = params[0].to_lowercase();
        let arg = params.get(1).map(String::as_str);
        let present = match param.as_str() {
            "notmusl"
            | "notmingw"
            | "nothreadsan"
            | "strinline"
            | "no_alternative_verify"
            | "no_latest_storage"
            | "no_vector_verification"
            | "noforcestorage"
            | "no_force_storage" => true,
            "mingw" => false,
            "notwindows" => !cfg!(windows),
            "windows" => cfg!(windows),
            "linux" => cfg!(target_os = "linux"),
            "longdouble" => false,
            "64bit" => cfg!(target_pointer_width = "64"),
            "skip_reload" | "no_extension_autoloading" | "allow_unsigned_extensions" => true,
            "vector_size" => arg
                .and_then(|a| a.parse::<usize>().ok())
                .is_some_and(|v| self.config.vector_size >= v),
            "exact_vector_size" => arg
                .and_then(|a| a.parse::<usize>().ok())
                .is_some_and(|v| self.config.vector_size == v),
            // Resource checks (ram, disk_space, block_size, ...) are assumed satisfied for now.
            "ram" | "disk_space" | "block_size" | "vacuum_rebuild_indexes" => true,
            extension => self
                .con
                .as_deref_mut()
                .expect("database is open")
                .load_extension(extension),
        };
        if present {
            Flow::Next
        } else {
            let _ = location;
            self.shared.stop_with(format!("require {}", params.join(" ")))
        }
    }

    fn run_concurrent_loop(
        &mut self,
        location: &Location,
        iterator: &str,
        values: &[String],
        body: &[Record],
        loops: &[ActiveLoop],
    ) -> Flow {
        let shared = &self.shared;
        let Some(db) = self.db.as_deref() else {
            return Flow::Next;
        };
        if !body.iter().all(supports_concurrent) {
            return shared.record_failure(Failure::new(
                location,
                "Concurrent loop is not supported over this command",
            ));
        }
        std::thread::scope(|scope| {
            for value in values {
                let mut loops = loops.to_vec();
                loops.push(ActiveLoop {
                    name: iterator.to_string(),
                    value: value.clone(),
                });
                scope.spawn(move || match db.connect() {
                    Ok(mut con) => {
                        run_concurrent_records(shared, con.as_mut(), body, &mut loops);
                    }
                    Err(e) => {
                        shared.record_failure(Failure::new(location, e));
                    }
                });
            }
        });
        if shared.stop.load(Ordering::SeqCst) {
            Flow::Stop
        } else {
            Flow::Next
        }
    }
}

/// The commands a `concurrentloop` body may contain (`SupportsConcurrent` in C++).
fn supports_concurrent(record: &Record) -> bool {
    match record {
        Record::Statement { .. }
        | Record::Query { .. }
        | Record::Continue { .. }
        | Record::Sleep(_)
        | Record::ResetLabel(_) => true,
        Record::Loop { parallel, body, .. } => !parallel && body.iter().all(supports_concurrent),
        _ => false,
    }
}

/// Executes a `concurrentloop` iteration on its own connection.
fn run_concurrent_records(
    shared: &Shared,
    con: &mut dyn Connection,
    records: &[Record],
    loops: &mut Vec<ActiveLoop>,
) -> Flow {
    for record in records {
        if shared.stop.load(Ordering::SeqCst) {
            return Flow::Stop;
        }
        let flow = match record {
            Record::Statement {
                location,
                expected,
                sql,
                conditions,
                ..
            } => match conditions_hold(conditions, loops, location) {
                Ok(false) => Flow::Next,
                Err(f) => shared.record_failure(f),
                Ok(true) => {
                    let sql = shared.loop_replacement(&shared.replace_keywords(sql), loops);
                    shared.executed.fetch_add(1, Ordering::Relaxed);
                    let failed_before = shared.failure.lock().unwrap().is_some();
                    let flow = shared.run_statement(con, location, expected, &sql);
                    count_pass(shared, failed_before);
                    flow
                }
            },
            Record::Query {
                location,
                column_count,
                sort_style,
                label,
                sql,
                expected,
                conditions,
                ..
            } => match conditions_hold(conditions, loops, location) {
                Ok(false) => Flow::Next,
                Err(f) => shared.record_failure(f),
                Ok(true) => {
                    let sql = shared.loop_replacement(&shared.replace_keywords(sql), loops);
                    shared.executed.fetch_add(1, Ordering::Relaxed);
                    let failed_before = shared.failure.lock().unwrap().is_some();
                    let flow = shared.run_query(
                        con,
                        location,
                        *column_count,
                        *sort_style,
                        label.as_deref(),
                        &sql,
                        expected,
                        loops,
                    );
                    count_pass(shared, failed_before);
                    flow
                }
            },
            Record::Loop {
                iterator, kind, body, ..
            } => {
                let values: Vec<String> = match kind {
                    LoopKind::Range { start, end } => (*start..*end).map(|i| i.to_string()).collect(),
                    LoopKind::Foreach(tokens) => expand_foreach(tokens),
                };
                let mut flow = Flow::Next;
                for value in values {
                    loops.push(ActiveLoop {
                        name: iterator.clone(),
                        value,
                    });
                    let inner = run_concurrent_records(shared, con, body, loops);
                    loops.pop();
                    if let Flow::Stop = inner {
                        flow = Flow::Stop;
                        break;
                    }
                }
                flow
            }
            Record::Continue { location, conditions } => match conditions_hold(conditions, loops, location) {
                Ok(true) => Flow::ContinueLoop,
                Ok(false) => Flow::Next,
                Err(f) => shared.record_failure(f),
            },
            Record::Sleep(duration) => {
                std::thread::sleep(*duration);
                Flow::Next
            }
            Record::ResetLabel(label) => {
                shared.labels.lock().unwrap().remove(label);
                Flow::Next
            }
            _ => unreachable!("checked by supports_concurrent"),
        };
        match flow {
            Flow::Next => {}
            other => return other,
        }
    }
    Flow::Next
}

fn connection_for<'c>(
    con: &'c mut Option<Box<dyn Connection>>,
    named: &'c mut HashMap<String, Box<dyn Connection>>,
    db: Option<&dyn Database>,
    name: Option<&str>,
) -> Result<&'c mut (dyn Connection + 'static), String> {
    match name {
        None => con.as_deref_mut().ok_or_else(|| "no open connection".to_string()),
        Some(name) => {
            if !named.contains_key(name) {
                let db = db.ok_or_else(|| "no open database".to_string())?;
                named.insert(name.to_string(), db.connect()?);
            }
            Ok(named.get_mut(name).unwrap().as_mut())
        }
    }
}

/// Counts a statement as passed if it did not record the file's first failure. With
/// `continue_on_failure`, later failures are not tracked individually, so this is approximate.
fn count_pass(shared: &Shared, failed_before: bool) {
    let failed_now = shared.failure.lock().unwrap().is_some();
    if failed_before || !failed_now {
        shared.passed.fetch_add(1, Ordering::Relaxed);
    }
}

fn remove_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_file(wal);
}

fn gunzip(input: &str, output: &str) -> Result<(), String> {
    let status = std::process::Command::new("gzip")
        .args(["-dc", input])
        .stdout(std::fs::File::create(output).map_err(|e| format!("Cannot open the file \"{output}\": {e}"))?)
        .status()
        .map_err(|e| format!("failed to run gzip: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Cannot open the file \"{input}\""))
    }
}

/// The variables the C++ runner predefines (`TestConfiguration::UpdateEnvironment`).
fn standard_variables(config: &RunConfig, test_dir: &Path) -> HashMap<String, String> {
    let root = config.root.to_string_lossy().into_owned();
    let data_dir = format!("{root}/data");
    let temp_dir = test_dir.to_string_lossy().into_owned();
    let uuid = pseudo_uuid();
    [
        ("TEST_UUID", uuid.clone()),
        ("BUILD_DIR", format!("{root}/build/release")),
        ("WORKING_DIR", root),
        ("DATA_DIR", data_dir.clone()),
        ("LOCAL_DATA_DIR", data_dir),
        ("TEMP_DIR", temp_dir.clone()),
        ("TEMP_DIR_ABSOLUTE", temp_dir.clone()),
        ("LOCAL_TEMP_DIR", temp_dir.clone()),
        ("TEMP_DIR_ROOT", config.scratch.to_string_lossy().into_owned()),
        ("RUN_ID", std::process::id().to_string()),
        ("CATALOG_DIR", format!("{temp_dir}/{uuid}")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Runs one test file (path relative to `config.root`) and reports the outcome.
pub fn run_file(config: &RunConfig, engine: &dyn Engine, name: &str, file_index: usize) -> FileReport {
    let start = Instant::now();
    let report = |outcome, executed, passed, mode_skipped| FileReport {
        path: name.to_string(),
        outcome,
        executed,
        passed,
        mode_skipped,
        duration: start.elapsed(),
    };
    let file: TestFile = match parser::parse_file(name, &config.root) {
        Ok(file) => file,
        Err(e) => return report(Outcome::Error(format!("parse error: {e}")), 0, 0, 0),
    };
    let test_dir = config.scratch.join(file_index.to_string());
    let _ = std::fs::remove_dir_all(&test_dir);
    if let Err(e) = std::fs::create_dir_all(&test_dir) {
        return report(
            Outcome::Error(format!("cannot create {}: {e}", test_dir.display())),
            0,
            0,
            file.mode_skipped,
        );
    }
    let mut runner = FileRunner {
        config,
        engine,
        shared: Shared {
            file_name: name.to_string(),
            test_dir: test_dir.clone(),
            working_dir: config.root.to_string_lossy().into_owned(),
            original_sqlite_test: parser::is_original_sqlite_test(name),
            settings: RwLock::new(Settings {
                always_fail_error_messages: vec!["INTERNAL".to_string()],
                variables: standard_variables(config, &test_dir),
                ..Settings::default()
            }),
            labels: Mutex::new(HashMap::new()),
            executed: AtomicUsize::new(0),
            passed: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
            failure: Mutex::new(None),
            continue_on_failure: config.continue_on_failure,
        },
        db: None,
        db_path: None,
        read_only: false,
        storage_version: None,
        con: None,
        named: HashMap::new(),
    };
    if let Err(e) = runner.open() {
        return report(
            Outcome::Error(format!("cannot open database: {e}")),
            0,
            0,
            file.mode_skipped,
        );
    }
    runner.run_records(&file.records, &mut Vec::new());
    // Close connections before deleting the scratch directory that may hold database files.
    runner.named.clear();
    runner.con = None;
    runner.db = None;
    let _ = std::fs::remove_dir_all(&test_dir);

    let shared = runner.shared;
    let executed = shared.executed.load(Ordering::Relaxed);
    let passed = shared.passed.load(Ordering::Relaxed);
    let outcome = if let Some(failure) = shared.failure.into_inner().unwrap() {
        Outcome::Fail {
            location: failure.location,
            message: failure.message,
        }
    } else {
        match shared.stop_reason.into_inner().unwrap() {
            Some(reason) if reason.starts_with("require") => Outcome::Skip(reason),
            _ => Outcome::Pass,
        }
    };
    report(outcome, executed, passed, file.mode_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreach_expansion() {
        let tokens: Vec<String> = ["<integral>", "!hugeint", "varchar"].map(String::from).to_vec();
        let expanded = expand_foreach(&tokens);
        assert_eq!(expanded.len(), 10);
        assert!(!expanded.contains(&"hugeint".to_string()));
        assert_eq!(expanded.last().unwrap(), "varchar");
    }

    #[test]
    fn loop_conditions() {
        let loc = Location {
            file: "x".into(),
            line: 1,
        };
        let loops = vec![ActiveLoop {
            name: "i".into(),
            value: "2".into(),
        }];
        let cond = |cmp, skip_if| {
            vec![Condition {
                keyword: "i".into(),
                value: "1".into(),
                comparison: cmp,
                skip_if,
            }]
        };
        assert!(conditions_hold(&cond(Comparison::Greater, false), &loops, &loc).unwrap());
        assert!(!conditions_hold(&cond(Comparison::Greater, true), &loops, &loc).unwrap());
        assert!(!conditions_hold(&cond(Comparison::Equal, false), &loops, &loc).unwrap());
    }
}
