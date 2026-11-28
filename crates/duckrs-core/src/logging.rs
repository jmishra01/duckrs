// Ported from duckdb/src/logging/{log_manager,logger,log_storage,log_types}.cpp and
// duckdb/src/include/duckdb/logging/{logging,log_type}.hpp @ rust-port-baseline
//
//! DuckDB's logging: a log manager per database holds the configuration and the storage of the
//! log, and loggers write entries to it under a registered context (database, connection or
//! thread). A connection's logger is made anew for every statement, with the configuration of
//! that moment, so a statement that turns logging on is not logged itself.
//!
//! The in-memory storage buffers entries and contexts and moves them to what scans see when it
//! flushes: when a buffer is full, and when a statement starts and ends. A statement therefore
//! does not see the entry of its own query. Ported: the `memory` and `stdout` storages. Not
//! ported: the `file` storage.

use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::Timestamp;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// `LogLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 10,
    Debug = 20,
    Info = 30,
    Warning = 40,
    Error = 50,
    Fatal = 60,
}

const LEVELS: [(LogLevel, &str); 6] = [
    (LogLevel::Trace, "TRACE"),
    (LogLevel::Debug, "DEBUG"),
    (LogLevel::Info, "INFO"),
    (LogLevel::Warning, "WARNING"),
    (LogLevel::Error, "ERROR"),
    (LogLevel::Fatal, "FATAL"),
];

/// `LogMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    LevelOnly,
    DisableSelected,
    EnableSelected,
}

const MODES: [(LogMode, &str); 3] = [
    (LogMode::LevelOnly, "LEVEL_ONLY"),
    (LogMode::DisableSelected, "DISABLE_SELECTED"),
    (LogMode::EnableSelected, "ENABLE_SELECTED"),
];

/// `LogContextScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogScope {
    Database,
    Connection,
    Thread,
}

impl LogScope {
    fn name(self) -> &'static str {
        match self {
            LogScope::Database => "DATABASE",
            LogScope::Connection => "CONNECTION",
            LogScope::Thread => "THREAD",
        }
    }
}

/// `StringUtil::StringToEnum`: the value named `text`, case-insensitively.
pub fn enum_from<T: Copy>(values: &[(T, &str)], enum_name: &str, text: &str) -> Result<T> {
    if let Some((value, _)) = values.iter().find(|(_, name)| name.eq_ignore_ascii_case(text)) {
        return Ok(*value);
    }
    let scores = values
        .iter()
        .map(|(_, name)| (name.to_string(), similarity_rating(name, text)))
        .collect();
    Err(Error::new(
        ErrorKind::NotImplemented,
        format!(
            "Enum value: unrecognized value \"{text}\" for enum \"{enum_name}\"\n{}",
            candidates_message(&top_n_strings(scores), "Candidates")
        ),
    ))
}

fn enum_name<T: Copy + PartialEq>(values: &[(T, &'static str)], value: T) -> &'static str {
    values
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, name)| *name)
        .expect("every value has a name")
}

impl LogLevel {
    pub fn parse(text: &str) -> Result<Self> {
        enum_from(&LEVELS, "LogLevel", text)
    }

    pub fn name(self) -> &'static str {
        enum_name(&LEVELS, self)
    }
}

impl LogMode {
    pub fn parse(text: &str) -> Result<Self> {
        enum_from(&MODES, "LogMode", text)
    }

    pub fn name(self) -> &'static str {
        enum_name(&MODES, self)
    }
}

/// `LogManager::RegisterDefaultLogTypes`: the log types and the level each logs at.
const LOG_TYPES: [(&str, LogLevel); 12] = [
    ("", LogLevel::Info),
    ("FileSystem", LogLevel::Trace),
    ("HTTP", LogLevel::Debug),
    ("QueryLog", LogLevel::Info),
    ("PhysicalOperator", LogLevel::Debug),
    ("Metrics", LogLevel::Info),
    ("AdaptiveFilter", LogLevel::Debug),
    ("ParquetPrefetch", LogLevel::Debug),
    ("AsyncTaskSchedule", LogLevel::Debug),
    ("ReadAheadBudget", LogLevel::Debug),
    ("ExternalResource", LogLevel::Info),
    ("ProgressVerification", LogLevel::Info),
];

/// The `GetLogType` of the structured log types, as SQL: what `parse_duckdb_log_message` makes
/// of their messages. The others are plain text.
pub fn log_type_schema(name: &str) -> Option<&'static str> {
    Some(match name {
        "FileSystem" => "STRUCT(fs VARCHAR, path VARCHAR, op VARCHAR, bytes BIGINT, pos BIGINT)",
        "HTTP" => {
            "STRUCT(request STRUCT(type VARCHAR, url VARCHAR, start_time TIMESTAMP WITH TIME ZONE, duration_ms \
             BIGINT, duration_us BIGINT, request_body_length UBIGINT, headers MAP(VARCHAR, VARCHAR)), response \
             STRUCT(status VARCHAR, reason VARCHAR, headers MAP(VARCHAR, VARCHAR)))"
        }
        "PhysicalOperator" => {
            "STRUCT(operator_type VARCHAR, parameters MAP(VARCHAR, VARCHAR), class VARCHAR, event VARCHAR, info \
             MAP(VARCHAR, VARCHAR))"
        }
        "Metrics" => {
            "STRUCT(metric VARCHAR, value VARCHAR, start_time TIMESTAMP WITH TIME ZONE, end_time TIMESTAMP WITH \
             TIME ZONE)"
        }
        "AdaptiveFilter" => "STRUCT(event VARCHAR, file_path VARCHAR, permutation VARCHAR, info MAP(VARCHAR, VARCHAR))",
        "ParquetPrefetch" => {
            "STRUCT(file_path VARCHAR, row_group_id BIGINT, fully_filtered BOOLEAN, strategy VARCHAR, \
             prefetch_groups VARCHAR[][], minimal_filters VARCHAR[], accepted_column_gap UBIGINT)"
        }
        "AsyncTaskSchedule" => "STRUCT(pool VARCHAR, task_count BIGINT)",
        "ReadAheadBudget" => "STRUCT(pending_bytes UBIGINT, budget_bytes UBIGINT, active_jobs UBIGINT)",
        "ProgressVerification" => "STRUCT(invariant VARCHAR, operator VARCHAR, pipeline VARCHAR, detail VARCHAR)",
        "ExternalResource" => {
            "STRUCT(resource_type VARCHAR, resource_name VARCHAR, operation VARCHAR, outcome VARCHAR, error \
             VARCHAR, extra_info MAP(VARCHAR, VARCHAR))"
        }
        _ => return None,
    })
}

/// `LogManager::LookupLogType`, case-insensitively: the type's name and level.
pub fn lookup_log_type(name: &str) -> Option<(&'static str, LogLevel)> {
    LOG_TYPES.iter().copied().find(|(n, _)| n.eq_ignore_ascii_case(name))
}

pub const MEMORY_STORAGE: &str = "memory";
pub const STDOUT_STORAGE: &str = "stdout";
pub const FILE_STORAGE: &str = "file";

/// `LogConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub enabled: bool,
    pub mode: LogMode,
    pub level: LogLevel,
    pub storage: String,
    pub enabled_log_types: BTreeSet<String>,
    pub disabled_log_types: BTreeSet<String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: LogMode::LevelOnly,
            level: LogLevel::Info,
            storage: MEMORY_STORAGE.into(),
            enabled_log_types: BTreeSet::new(),
            disabled_log_types: BTreeSet::new(),
        }
    }
}

impl LogConfig {
    /// `ThreadSafeLogger::ShouldLog`.
    fn should_log(&self, log_type: &str, level: LogLevel) -> bool {
        if !self.enabled || self.level > level {
            return false;
        }
        match self.mode {
            LogMode::LevelOnly => true,
            LogMode::EnableSelected => self.enabled_log_types.contains(log_type),
            LogMode::DisableSelected => !self.disabled_log_types.contains(log_type),
        }
    }
}

/// `LoggingContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggingContext {
    pub scope: LogScope,
    pub connection_id: Option<u64>,
    pub transaction_id: Option<u64>,
    pub query_id: Option<u64>,
    pub thread_id: Option<u64>,
}

impl LoggingContext {
    pub fn new(scope: LogScope) -> Self {
        Self {
            scope,
            connection_id: None,
            transaction_id: None,
            query_id: None,
            thread_id: None,
        }
    }
}

/// `RegisteredLoggingContext`.
#[derive(Debug, Clone, Copy)]
pub struct RegisteredContext {
    pub id: u64,
    pub context: LoggingContext,
}

/// A row of `duckdb_logs()`: `LoggingTargetTable::LOG_ENTRIES`.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub context_id: u64,
    /// microseconds since the epoch
    pub timestamp: i64,
    pub log_type: String,
    pub level: LogLevel,
    pub message: String,
}

impl LogEntry {
    pub fn values(&self) -> Vec<Value> {
        vec![
            Value::ubigint(self.context_id),
            Value::timestamp_tz(Timestamp(self.timestamp)),
            Value::varchar(self.log_type.clone()),
            Value::varchar(self.level.name()),
            Value::varchar(self.message.clone()),
        ]
    }
}

/// The values of `LoggingTargetTable::LOG_CONTEXTS` (`WriteLoggingContextsToChunk`).
pub fn context_values(context: &RegisteredContext) -> Vec<Value> {
    let id = |value: Option<u64>| {
        value.map_or_else(
            || Value::null(duckrs_common::types::LogicalType::UBIGINT),
            Value::ubigint,
        )
    };
    let c = &context.context;
    vec![
        Value::ubigint(context.id),
        Value::varchar(c.scope.name()),
        id(c.connection_id),
        id(c.transaction_id),
        id(c.query_id),
        id(c.thread_id),
    ]
}

/// `STANDARD_VECTOR_SIZE`: the buffer size of the in-memory storage.
const DEFAULT_BUFFER_SIZE: usize = 2048;

/// `BufferingLogStorage` with `InMemoryLogStorage`'s `FlushChunk`: entries and contexts are
/// buffered, and what scans see is what was flushed.
#[derive(Debug)]
struct MemoryStorage {
    buffer_limit: usize,
    only_flush_on_full_buffer: bool,
    entries: Vec<LogEntry>,
    contexts: Vec<RegisteredContext>,
    registered: HashSet<u64>,
    flush_contexts_on_next_entry_flush: bool,
    flushed_entries: Vec<LogEntry>,
    flushed_contexts: Vec<RegisteredContext>,
}

impl MemoryStorage {
    fn new() -> Self {
        Self {
            buffer_limit: DEFAULT_BUFFER_SIZE,
            only_flush_on_full_buffer: false,
            entries: Vec::new(),
            contexts: Vec::new(),
            registered: HashSet::new(),
            flush_contexts_on_next_entry_flush: false,
            flushed_entries: Vec::new(),
            flushed_contexts: Vec::new(),
        }
    }

    fn flush_entries(&mut self) {
        self.flushed_entries.append(&mut self.entries);
    }

    fn flush_contexts(&mut self) {
        self.flushed_contexts.append(&mut self.contexts);
    }

    /// `BufferingLogStorage::WriteLogEntry`.
    fn write(&mut self, entry: LogEntry, context: &RegisteredContext) -> Result<()> {
        if self.entries.len() >= self.buffer_limit && self.buffer_limit != 0 {
            return Err(Error::new(
                ErrorKind::Internal,
                "Log buffer limit exceeded: code should have flushed before",
            ));
        }
        if self.registered.insert(context.id) {
            // `WriteLoggingContext`
            if self.contexts.len() + 1 > self.buffer_limit {
                self.flush_contexts();
            }
            self.contexts.push(*context);
            self.flush_contexts_on_next_entry_flush = true;
        }
        self.entries.push(entry);
        if self.entries.len() >= self.buffer_limit {
            self.flush_entries();
            if self.flush_contexts_on_next_entry_flush {
                self.flush_contexts();
                self.flush_contexts_on_next_entry_flush = false;
            }
        }
        Ok(())
    }

    /// `BufferingLogStorage::FlushAll`.
    fn flush_all(&mut self) {
        if !self.only_flush_on_full_buffer {
            self.flush_entries();
            self.flush_contexts();
        }
    }

    /// `ResetAllBuffers`: forgets everything, flushed or not.
    fn reset(&mut self) {
        self.entries.clear();
        self.contexts.clear();
        self.registered.clear();
        self.flushed_entries.clear();
        self.flushed_contexts.clear();
    }

    /// `BufferingLogStorage::UpdateConfigInternal`.
    fn update_config(&mut self, config: &[(String, Value)]) -> Result<()> {
        for (key, value) in config {
            if key.eq_ignore_ascii_case("buffer_size") {
                self.buffer_limit = unsigned_option(value)? as usize;
                self.reset();
            } else if key.eq_ignore_ascii_case("only_flush_on_full_buffer") {
                self.only_flush_on_full_buffer = value.try_to_string()?.eq_ignore_ascii_case("true");
            } else if key.eq_ignore_ascii_case("normalize") {
                return Err(Error::new(
                    ErrorKind::Internal,
                    "'normalize' setting should be handled in child class",
                ));
            } else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Unrecognized log storage config option for storage: 'InMemoryLogStorage': '{key}'"),
                ));
            }
        }
        Ok(())
    }
}

fn unsigned_option(value: &Value) -> Result<u64> {
    value.as_i128().and_then(|v| u64::try_from(v).ok()).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid value for a log storage option: {value:?}"),
        )
    })
}

/// `StdOutLogStorage`: every entry, with its context, printed as a tab-separated line.
fn print_entry(entry: &LogEntry, context: &RegisteredContext) {
    let fields: Vec<String> = context_values(context)
        .iter()
        .chain(&entry.values()[1..])
        .map(csv_field)
        .collect();
    println!("{}", fields.join("\t"));
}

/// `LoggingTargetTable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogTable {
    Entries,
    Contexts,
    All,
}

const ENTRY_COLUMNS: [&str; 5] = ["context_id", "timestamp", "type", "log_level", "message"];
const CONTEXT_COLUMNS: [&str; 6] = [
    "context_id",
    "scope",
    "connection_id",
    "transaction_id",
    "query_id",
    "thread_id",
];

impl LogTable {
    /// `LogStorage::GetColumnNames`.
    fn columns(self) -> Vec<&'static str> {
        match self {
            LogTable::Entries => ENTRY_COLUMNS.to_vec(),
            LogTable::Contexts => CONTEXT_COLUMNS.to_vec(),
            LogTable::All => CONTEXT_COLUMNS.iter().chain(&ENTRY_COLUMNS[1..]).copied().collect(),
        }
    }
}

/// `CSVWriter::WriteQuotedString` with `"` as quote and escape: a field quoted when it is empty
/// (as NULL is written as nothing) or holds a comma, a quote or a line break. The characters
/// that need quotes are those of the default `,` delimiter: the log storages work them out
/// before they set their own delimiter (`CSVWriterOptions` of `CSVLogStorage`).
fn csv_field(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    let text = value.try_to_string().unwrap_or_default();
    if text.is_empty() || text.contains([',', '"', '\n', '\r']) {
        return format!("\"{}\"", text.replace('"', "\"\""));
    }
    text
}

/// A file of the file storage (`FileLogStorage::TableWriter`).
#[derive(Debug, Default)]
struct TableFile {
    path: String,
    initialized: bool,
}

/// `FileLogStorage`: the buffering of [`MemoryStorage`], flushed to CSV files: one for the
/// entries and one for the contexts (`normalize`), or one with both.
#[derive(Debug)]
struct FileStorage {
    normalize: bool,
    buffer_limit: usize,
    only_flush_on_full_buffer: bool,
    base_path: String,
    delimiter: String,
    entries: Vec<LogEntry>,
    contexts: Vec<RegisteredContext>,
    all: Vec<(LogEntry, RegisteredContext)>,
    registered: HashSet<u64>,
    flush_contexts_on_next_entry_flush: bool,
    files: HashMap<LogTable, TableFile>,
}

fn io_error(path: &str, error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("Could not write log file \"{path}\": {error}"))
}

impl FileStorage {
    fn new() -> Self {
        Self {
            normalize: true,
            buffer_limit: DEFAULT_BUFFER_SIZE,
            only_flush_on_full_buffer: false,
            base_path: String::new(),
            delimiter: ",".into(),
            entries: Vec::new(),
            contexts: Vec::new(),
            all: Vec::new(),
            registered: HashSet::new(),
            flush_contexts_on_next_entry_flush: false,
            files: [LogTable::Entries, LogTable::Contexts, LogTable::All]
                .into_iter()
                .map(|t| (t, TableFile::default()))
                .collect(),
        }
    }

    /// `BufferingLogStorage::IsEnabledInternal`.
    fn is_enabled(&self, table: LogTable) -> bool {
        if self.normalize {
            matches!(table, LogTable::Entries | LogTable::Contexts)
        } else {
            table == LogTable::All
        }
    }

    fn write(&mut self, entry: LogEntry, context: &RegisteredContext) -> Result<()> {
        let size = if self.normalize {
            self.entries.len()
        } else {
            self.all.len()
        };
        if size >= self.buffer_limit && self.buffer_limit != 0 {
            return Err(Error::new(
                ErrorKind::Internal,
                "Log buffer limit exceeded: code should have flushed before",
            ));
        }
        if self.registered.insert(context.id) {
            if self.normalize {
                if self.contexts.len() + 1 > self.buffer_limit {
                    self.flush(LogTable::Contexts)?;
                }
                self.contexts.push(*context);
            }
            self.flush_contexts_on_next_entry_flush = true;
        }
        if self.normalize {
            self.entries.push(entry);
        } else {
            self.all.push((entry, *context));
        }
        if size + 1 >= self.buffer_limit {
            if self.normalize {
                self.flush(LogTable::Entries)?;
                if self.flush_contexts_on_next_entry_flush {
                    self.flush(LogTable::Contexts)?;
                    self.flush_contexts_on_next_entry_flush = false;
                }
            } else {
                self.flush(LogTable::All)?;
            }
        }
        Ok(())
    }

    fn flush_all(&mut self) -> Result<()> {
        if self.only_flush_on_full_buffer {
            return Ok(());
        }
        if self.normalize {
            self.flush(LogTable::Entries)?;
            self.flush(LogTable::Contexts)
        } else {
            self.flush(LogTable::All)
        }
    }

    /// `FileLogStorage::InitializeFile`: the file, made with its header if it is new or empty.
    fn initialize(&mut self, table: LogTable) -> Result<()> {
        let delimiter = self.delimiter.clone();
        let file = self.files.get_mut(&table).expect("every table has a file");
        if file.initialized {
            return Ok(());
        }
        if file.path.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Failed to initialize file log storage table, path wasn't set",
            ));
        }
        let path = file.path.clone();
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| io_error(&path, e))?;
        }
        let empty = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        if empty {
            std::io::Write::write_all(&mut handle, table.columns().join(&delimiter).as_bytes())
                .map_err(|e| io_error(&path, e))?;
        }
        file.initialized = true;
        Ok(())
    }

    /// `CSVLogStorage::FlushChunk`: the buffered rows of `table`, appended to its file, each
    /// after a line break (`CSVNewLineMode::WRITE_BEFORE`).
    fn flush(&mut self, table: LogTable) -> Result<()> {
        if !self.is_enabled(table) {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Cannot flush disabled logging target",
            ));
        }
        self.initialize(table)?;
        let rows: Vec<Vec<Value>> = match table {
            LogTable::Entries => self.entries.drain(..).map(|e| e.values()).collect(),
            LogTable::Contexts => self.contexts.drain(..).map(|c| context_values(&c)).collect(),
            LogTable::All => self
                .all
                .drain(..)
                .map(|(entry, context)| {
                    let mut row = context_values(&context);
                    row.extend(entry.values().into_iter().skip(1));
                    row
                })
                .collect(),
        };
        if rows.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        for row in rows {
            text.push('\n');
            let fields: Vec<String> = row.iter().map(csv_field).collect();
            text.push_str(&fields.join(&self.delimiter));
        }
        let path = &self.files[&table].path;
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| io_error(path, e))?;
        std::io::Write::write_all(&mut handle, text.as_bytes()).map_err(|e| io_error(path, e))
    }

    fn reset_buffers(&mut self) {
        self.entries.clear();
        self.contexts.clear();
        self.all.clear();
        self.registered.clear();
    }

    /// `FileLogStorage::Truncate`: the files keep only their headers.
    fn truncate(&mut self) -> Result<()> {
        self.reset_buffers();
        for (table, file) in &self.files {
            if !file.initialized {
                continue;
            }
            std::fs::write(&file.path, table.columns().join(&self.delimiter)).map_err(|e| io_error(&file.path, e))?;
        }
        Ok(())
    }

    /// `FileLogStorage::SetPaths`.
    fn set_paths(&mut self, base: &str) {
        for file in self.files.values_mut() {
            file.path.clear();
        }
        let join = |name: &str| std::path::Path::new(base).join(name).to_string_lossy().into_owned();
        let mut set = |table: LogTable, path: String| {
            self.files.get_mut(&table).expect("every table has a file").path = path;
        };
        if self.normalize {
            set(LogTable::Contexts, join("duckdb_log_contexts.csv"));
            set(LogTable::Entries, join("duckdb_log_entries.csv"));
        } else if base.ends_with(".csv") {
            set(LogTable::All, base.to_string());
        } else {
            set(LogTable::Entries, join("duckdb_log_entries.csv"));
        }
    }

    /// `FileLogStorage::UpdateConfigInternal`.
    fn update_config(&mut self, config: &[(String, Value)]) -> Result<()> {
        let mut new_path = String::new();
        let mut normalize = self.normalize;
        let mut normalize_set = false;
        let mut reinitialize = false;
        let mut rest = Vec::new();
        for (key, value) in config {
            match key.to_lowercase().as_str() {
                "path" => {
                    let path = if value.is_null() {
                        String::new()
                    } else {
                        value.try_to_string()?
                    };
                    if !normalize_set && path.ends_with(".csv") {
                        normalize = false;
                    }
                    new_path = path;
                }
                "normalize" => {
                    normalize_set = true;
                    normalize = value.try_to_string()?.eq_ignore_ascii_case("true");
                }
                "delim" => {
                    reinitialize = true;
                    self.delimiter = value.try_to_string()?;
                }
                _ => rest.push((key.clone(), value.clone())),
            }
        }
        if new_path.ends_with(".csv") && normalize {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                format!(
                    "Can not set path to '{new_path}' while normalize is true. Normalize will make DuckDB write \
                     multiple log files to more efficiently store log entries. Please specify a directory path \
                     instead of a csv file path, or set normalize to false."
                ),
            ));
        }
        if self.files.values().any(|f| f.initialized) {
            self.flush_all()?;
        }
        reinitialize |= self.normalize != normalize;
        self.normalize = normalize;
        if reinitialize {
            self.reset_buffers();
            for file in self.files.values_mut() {
                file.initialized = false;
            }
        }
        if new_path != self.base_path {
            self.base_path.clone_from(&new_path);
            self.set_paths(&new_path);
            // the files are opened anew where the path now says
            for file in self.files.values_mut() {
                file.initialized = false;
            }
        }
        for (key, value) in rest {
            if key.eq_ignore_ascii_case("buffer_size") {
                self.buffer_limit = unsigned_option(&value)? as usize;
                self.reset_buffers();
            } else if key.eq_ignore_ascii_case("only_flush_on_full_buffer") {
                self.only_flush_on_full_buffer = value.try_to_string()?.eq_ignore_ascii_case("true");
            } else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Unrecognized log storage config option for storage: 'FileLogStorage': '{key}'"),
                ));
            }
        }
        Ok(())
    }

    /// `FileLogStorage::BindReplace`: the SQL that reads `table` from its file, after flushing
    /// it, or `None` if the storage does not keep that table.
    fn scan_sql(&mut self, table: LogTable) -> Result<Option<String>> {
        if !self.is_enabled(table) {
            return Ok(None);
        }
        self.flush(table)?;
        let path = self.files[&table].path.replace('\'', "''");
        let entries = "'context_id': 'UBIGINT', 'timestamp': 'TIMESTAMP WITH TIME ZONE', 'type': 'VARCHAR', \
                       'log_level': 'VARCHAR' , 'message': 'VARCHAR'";
        let contexts = "'context_id': 'UBIGINT', 'scope': 'VARCHAR', 'connection_id': 'UBIGINT', \
                        'transaction_id': 'UBIGINT', 'query_id': 'UBIGINT', 'thread_id': 'UBIGINT'";
        let (select, columns) = match table {
            LogTable::Entries => ("SELECT *".to_string(), entries.to_string()),
            LogTable::Contexts => ("SELECT *".to_string(), contexts.to_string()),
            LogTable::All => (
                "SELECT context_id, scope, connection_id, transaction_id, query_id, thread_id, timestamp, type, \
                 log_level, message "
                    .to_string(),
                format!(
                    "{contexts}, 'timestamp': 'TIMESTAMP WITH TIME ZONE', 'type': 'VARCHAR', 'log_level': \
                     'VARCHAR' , 'message': 'VARCHAR'"
                ),
            ),
        };
        Ok(Some(format!(
            "{select} FROM read_csv_auto('{path}', columns={{{columns}}})"
        )))
    }
}

/// The join of `DuckDBLogBindReplace` for a storage that keeps entries and contexts apart.
pub const DENORMALIZED_JOIN: &str = "SELECT l.context_id, scope, connection_id, transaction_id, query_id, thread_id, \
     timestamp, type, log_level, message FROM (SELECT row_number() OVER () AS rowid, * FROM duckdb_logs()) as l \
     JOIN duckdb_log_contexts() as c ON l.context_id=c.context_id order by timestamp, l.rowid;";

/// A log storage a client registered (`duckdb_register_log_storage`): each entry goes to its callback, and it keeps
/// nothing to scan (`CallbackLogStorage`).
pub trait LogStorageCallback: Send + Sync {
    /// `WriteLogEntry`: an entry, its time in microseconds since the epoch.
    fn write_entry(&self, timestamp: i64, level: LogLevel, log_type: &str, message: &str);
}

/// A registered [`LogStorageCallback`].
#[derive(Clone)]
pub struct CallbackStorage(pub Arc<dyn LogStorageCallback>);

impl std::fmt::Debug for CallbackStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallbackStorage")
    }
}

#[derive(Debug)]
enum Storage {
    Memory(MemoryStorage),
    Stdout,
    File(FileStorage),
    Callback(CallbackStorage),
}

impl Storage {
    /// The storage `name` (lower case) names: a built-in one, or one a client registered.
    fn create(name: &str, registered: &HashMap<String, CallbackStorage>) -> Result<Self> {
        match name {
            MEMORY_STORAGE => Ok(Storage::Memory(MemoryStorage::new())),
            STDOUT_STORAGE => Ok(Storage::Stdout),
            FILE_STORAGE => Ok(Storage::File(FileStorage::new())),
            _ => match registered.get(name) {
                Some(callback) => Ok(Storage::Callback(callback.clone())),
                None => Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Log storage '{name}' is not yet registered"),
                )),
            },
        }
    }
}

/// `LogManager`.
#[derive(Debug)]
pub struct LogManager {
    config: LogConfig,
    storage: Storage,
    next_context_id: u64,
    /// the context of the database's own logger (`GlobalLogger`)
    global_context: RegisteredContext,
    /// `registered_log_storages` beyond the built-in ones, by lower-case name
    registered: HashMap<String, CallbackStorage>,
}

impl Default for LogManager {
    fn default() -> Self {
        let mut manager = Self {
            config: LogConfig::default(),
            storage: Storage::Memory(MemoryStorage::new()),
            registered: HashMap::new(),
            next_context_id: 0,
            global_context: RegisteredContext {
                id: 0,
                context: LoggingContext::new(LogScope::Database),
            },
        };
        // `LogManager::Initialize`
        manager.global_context = manager.register(LoggingContext::new(LogScope::Database));
        manager
    }
}

/// A log manager that loggers and the client share.
pub type SharedLogManager = Arc<Mutex<LogManager>>;

impl LogManager {
    pub fn config(&self) -> &LogConfig {
        &self.config
    }

    /// `RegisterLoggingContextInternal`.
    fn register(&mut self, context: LoggingContext) -> RegisteredContext {
        let registered = RegisteredContext {
            id: self.next_context_id,
            context,
        };
        self.next_context_id += 1;
        registered
    }

    /// `LogManager::CreateLogger`: a logger with the configuration of now.
    pub fn create_logger(&mut self, context: LoggingContext) -> Logger {
        let context = self.register(context);
        Logger {
            config: self.config.clone(),
            context,
        }
    }

    /// `LogManager::WriteLogEntry`. A warning is an error under `warnings_as_errors`.
    pub fn write_entry(
        &mut self,
        log_type: &str,
        level: LogLevel,
        message: &str,
        context: &RegisteredContext,
        warnings_as_errors: bool,
    ) -> Result<()> {
        if level == LogLevel::Warning && warnings_as_errors {
            return Err(Error::new(ErrorKind::InvalidInput, message));
        }
        let entry = LogEntry {
            context_id: context.id,
            timestamp: now_micros(),
            log_type: log_type.to_string(),
            level,
            message: message.to_string(),
        };
        match &mut self.storage {
            Storage::Memory(storage) => storage.write(entry, context),
            Storage::File(storage) => storage.write(entry, context),
            Storage::Stdout => {
                print_entry(&entry, context);
                Ok(())
            }
            Storage::Callback(callback) => {
                callback
                    .0
                    .write_entry(entry.timestamp, entry.level, &entry.log_type, &entry.message);
                Ok(())
            }
        }
    }

    /// Writes with the database's own logger, which follows the configuration as it changes.
    pub fn write_global(
        &mut self,
        log_type: &str,
        level: LogLevel,
        message: &str,
        warnings_as_errors: bool,
    ) -> Result<()> {
        if !self.config.should_log(log_type, level) {
            return Ok(());
        }
        let context = self.global_context;
        self.write_entry(log_type, level, message, &context, warnings_as_errors)
    }

    /// `LogManager::Flush`.
    pub fn flush(&mut self) -> Result<()> {
        match &mut self.storage {
            Storage::Memory(storage) => storage.flush_all(),
            Storage::File(storage) => storage.flush_all()?,
            Storage::Stdout | Storage::Callback(_) => {}
        }
        Ok(())
    }

    /// `LogManager::SetLogStorage` (`SET logging_storage`).
    pub fn set_storage(&mut self, name: &str) -> Result<()> {
        let lower = name.to_lowercase();
        if lower == FILE_STORAGE && self.config.storage != lower {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Cannot select 'file' log storage via 'SET logging_storage' because it requires a path. Use CALL \
                 enable_logging(storage='file', storage_path='...') instead.",
            ));
        }
        self.set_storage_internal(name)
    }

    /// `LogManager::SetLogStorageInternal`.
    fn set_storage_internal(&mut self, name: &str) -> Result<()> {
        let lower = name.to_lowercase();
        if self.config.storage == lower {
            return Ok(());
        }
        let storage = Storage::create(&lower, &self.registered).map_err(|error| {
            if error.kind == ErrorKind::InvalidInput {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Log storage '{name}' is not yet registered"),
                )
            } else {
                error
            }
        })?;
        self.flush()?;
        self.storage = storage;
        self.config.storage = lower;
        Ok(())
    }

    /// `LogManager::RegisterLogStorage`: false when a storage has the name already.
    pub fn register_storage(&mut self, name: &str, storage: Arc<dyn LogStorageCallback>) -> bool {
        let lower = name.to_lowercase();
        if [MEMORY_STORAGE, STDOUT_STORAGE, FILE_STORAGE].contains(&lower.as_str()) || self.registered.contains_key(&lower)
        {
            return false;
        }
        self.registered.insert(lower, CallbackStorage(storage));
        true
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    pub fn set_mode(&mut self, mode: LogMode) {
        self.config.mode = mode;
    }

    pub fn set_level(&mut self, level: LogLevel) {
        self.config.level = level;
    }

    pub fn set_enabled_log_types(&mut self, types: BTreeSet<String>) {
        self.config.enabled_log_types = types;
    }

    pub fn set_disabled_log_types(&mut self, types: BTreeSet<String>) {
        self.config.disabled_log_types = types;
    }

    /// `LogManager::SetConfig`: switches the storage first.
    pub fn set_config(&mut self, config: LogConfig) -> Result<()> {
        self.set_storage_internal(&config.storage)?;
        let storage = self.config.storage.clone();
        self.config = LogConfig { storage, ..config };
        Ok(())
    }

    /// `LogManager::SetEnableStructuredLoggers`: logs only these types, from the lowest level of them.
    pub fn enable_structured_loggers(&mut self, types: &[String]) -> Result<()> {
        let mut config = self.config.clone();
        config.enabled_log_types.clear();
        let mut level = LogLevel::Fatal;
        for name in types {
            let (name, type_level) = lookup_log_type(name)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("Unknown log type: '{name}'")))?;
            config.enabled_log_types.insert(name.to_string());
            level = level.min(type_level);
        }
        config.level = level;
        config.mode = LogMode::EnableSelected;
        config.enabled = true;
        self.config = config;
        Ok(())
    }

    /// `EnableLogging`: applies `config`, the log types (all if none) and the storage options,
    /// and puts back the previous configuration and storage if any of it fails
    /// (`GetConfigAndStorage` and `RestoreConfig`).
    pub fn enable_logging(
        &mut self,
        config: LogConfig,
        types: &[String],
        storage_config: &[(String, Value)],
    ) -> Result<()> {
        let previous_config = self.config.clone();
        let name = config.storage.to_lowercase();
        let mut previous_storage = None;
        if name != self.config.storage {
            let storage = Storage::create(&name, &self.registered).map_err(|error| {
                if error.kind == ErrorKind::InvalidInput {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!("Log storage '{}' is not yet registered", config.storage),
                    )
                } else {
                    error
                }
            })?;
            self.flush()?;
            previous_storage = Some(std::mem::replace(&mut self.storage, storage));
        }
        self.config = LogConfig {
            storage: name,
            ..config
        };
        let mut result = if types.is_empty() {
            self.config.enabled = true;
            self.config.mode = LogMode::LevelOnly;
            Ok(())
        } else {
            self.enable_structured_loggers(types)
        };
        if result.is_ok() && !storage_config.is_empty() {
            result = self.update_storage_config(storage_config);
        }
        if result.is_err() {
            if let Some(storage) = previous_storage {
                self.storage = storage;
            }
            self.config = previous_config;
        }
        result
    }

    /// `LogManager::UpdateLogStorageConfig`.
    pub fn update_storage_config(&mut self, config: &[(String, Value)]) -> Result<()> {
        match &mut self.storage {
            Storage::Memory(storage) => storage.update_config(config),
            Storage::File(storage) => storage.update_config(config),
            // `CSVLogStorage::UpdateConfigInternal`: what the printed lines look like and when
            // they are written out, which a line per entry makes moot here
            Storage::Stdout => {
                for (key, _) in config {
                    if !["delim", "buffer_size", "only_flush_on_full_buffer"]
                        .iter()
                        .any(|k| k.eq_ignore_ascii_case(key))
                    {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!("Unrecognized log storage config option for storage: 'StdOutLogStorage': '{key}'"),
                        ));
                    }
                }
                Ok(())
            }
            // `LogStorage::UpdateConfig`
            Storage::Callback(_) if !config.is_empty() => Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Log storage '{}' does not support passing configuration",
                    self.config.storage
                ),
            )),
            Storage::Callback(_) => Ok(()),
        }
    }

    /// `LogManager::TruncateLogStorage`.
    pub fn truncate(&mut self) -> Result<()> {
        match &mut self.storage {
            Storage::Memory(storage) => storage.reset(),
            Storage::File(storage) => storage.truncate()?,
            Storage::Stdout | Storage::Callback(_) => {}
        }
        Ok(())
    }

    /// The SQL that reads `table` of the file storage, `Ok(None)` for another storage or a table
    /// the file storage does not keep.
    pub fn file_scan_sql(&mut self, table: LogTable) -> Result<Option<String>> {
        match &mut self.storage {
            Storage::File(storage) => storage.scan_sql(table),
            _ => Ok(None),
        }
    }

    pub fn is_file_storage(&self) -> bool {
        matches!(self.storage, Storage::File(_))
    }

    /// `DuckDBLogInit` and `DuckDBLogContextInit`: what a scan sees, if the storage can be scanned.
    pub fn scan(&self) -> Result<(&[LogEntry], &[RegisteredContext])> {
        match &self.storage {
            Storage::Memory(storage) => Ok((&storage.flushed_entries, &storage.flushed_contexts)),
            Storage::Stdout | Storage::File(_) | Storage::Callback(_) => Err(Error::new(
                ErrorKind::InvalidConfiguration,
                format!(
                    "Log storage '{}' does not support this query. Select a queryable storage, such as 'memory' or \
                     'file', before generating logs.",
                    self.config.storage
                ),
            )),
        }
    }

    /// `duckdb_logs(denormalized_table = true)`: each entry with its context, by time.
    pub fn denormalized_rows(&self) -> Result<Vec<Vec<Value>>> {
        let (entries, contexts) = self.scan()?;
        let by_id: HashMap<u64, &RegisteredContext> = contexts.iter().map(|c| (c.id, c)).collect();
        let mut rows: Vec<(i64, usize, Vec<Value>)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let Some(context) = by_id.get(&entry.context_id) else {
                continue;
            };
            let mut row = context_values(context);
            row.extend(entry.values().into_iter().skip(1));
            rows.push((entry.timestamp, index, row));
        }
        rows.sort_by_key(|(timestamp, index, _)| (*timestamp, *index));
        Ok(rows.into_iter().map(|(_, _, row)| row).collect())
    }
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as i64)
}

/// `ThreadSafeLogger`: writes under its context with the configuration it was made with.
#[derive(Debug, Clone)]
pub struct Logger {
    config: LogConfig,
    context: RegisteredContext,
}

impl Logger {
    pub fn should_log(&self, log_type: &str, level: LogLevel) -> bool {
        self.config.should_log(log_type, level)
    }

    /// `DUCKDB_LOG_INTERNAL`.
    pub fn log(
        &self,
        manager: &SharedLogManager,
        log_type: &str,
        level: LogLevel,
        message: &str,
        warnings_as_errors: bool,
    ) -> Result<()> {
        if !self.should_log(log_type, level) {
            return Ok(());
        }
        let mut manager = manager.lock().expect("the log manager is not poisoned");
        manager.write_entry(log_type, level, message, &self.context, warnings_as_errors)
    }
}

/// What `write_log` writes with: the log manager and the logger of a connection, which the
/// connection replaces for every statement.
#[derive(Debug)]
pub struct LogTarget {
    pub manager: SharedLogManager,
    pub logger: Mutex<Logger>,
    pub warnings_as_errors: std::sync::atomic::AtomicBool,
}

static TARGETS: Mutex<Option<HashMap<u64, std::sync::Weak<LogTarget>>>> = Mutex::new(None);
static NEXT_TARGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Makes `target` reachable by the id returned, for as long as it lives.
pub fn register_target(target: &Arc<LogTarget>) -> u64 {
    let id = NEXT_TARGET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut targets = TARGETS.lock().expect("not poisoned");
    let targets = targets.get_or_insert_with(HashMap::new);
    targets.retain(|_, target| target.strong_count() > 0);
    targets.insert(id, Arc::downgrade(target));
    id
}

pub fn target(id: u64) -> Option<Arc<LogTarget>> {
    TARGETS.lock().expect("not poisoned").as_ref()?.get(&id)?.upgrade()
}

/// `WriteLogFunction`: logs each message of column 0 with the connection's logger (`connection`
/// or `file_opener` scope) or the database's (`database`). Columns 1 to 5 are the constants the
/// binder added: the logger's id, the level, the type, the scope and whether not to log; a 7th
/// column is the value to give back, and without one the result is NULL.
pub fn write_log(args: &duckrs_common::vector::DataChunk, result: &mut duckrs_common::vector::Vector) -> Result<()> {
    let columns = args.columns();
    let count = args.len();
    let constant = |column: usize| columns[column].get_value(0);
    let disabled = matches!(constant(5).data(), duckrs_common::value::ValueData::Bool(true));
    if !disabled {
        let target_id = constant(1).as_i128().unwrap_or(0) as u64;
        let level = LogLevel::parse(&constant(2).try_to_string()?)?;
        let log_type = constant(3).try_to_string()?;
        let scope = constant(4).try_to_string()?;
        let target =
            target(target_id).ok_or_else(|| Error::new(ErrorKind::Internal, "write_log: the connection is gone"))?;
        let warnings_as_errors = target.warnings_as_errors.load(std::sync::atomic::Ordering::Relaxed);
        for row in 0..count {
            let message = columns[0].get_value(row);
            let message = if message.is_null() {
                String::new()
            } else {
                message.try_to_string()?
            };
            match scope.as_str() {
                "" | "connection" | "file_opener" => {
                    let logger = target.logger.lock().expect("not poisoned").clone();
                    logger.log(&target.manager, &log_type, level, &message, warnings_as_errors)?;
                }
                "database" => {
                    let mut manager = target.manager.lock().expect("not poisoned");
                    manager.write_global(&log_type, level, &message, warnings_as_errors)?;
                }
                other => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "write_log: 'scope' argument unknown: '{other}'. Valid values are [connection, database, \
                             file_opener]"
                        ),
                    ));
                }
            }
        }
    }
    if let Some(value) = columns.get(6) {
        *result = value.clone();
    } else {
        *result = duckrs_common::vector::Vector::constant_null(result.logical_type().clone());
    }
    Ok(())
}

thread_local! {
    /// The logger of the statement this thread runs, for the file system (`FileHandle::logger`).
    static FILE_LOGGER: std::cell::RefCell<Option<Arc<LogTarget>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `target` as the logger of the files it opens.
pub fn with_file_logging<R>(target: &Arc<LogTarget>, f: impl FnOnce() -> R) -> R {
    let previous = FILE_LOGGER.with(|logger| logger.replace(Some(target.clone())));
    let result = f();
    FILE_LOGGER.with(|logger| *logger.borrow_mut() = previous);
    result
}

/// `FileSystemLogType::ConstructLogMessage`.
fn file_system_message(path: &str, op: &str, bytes: Option<(i64, u64)>) -> String {
    match bytes {
        Some((bytes, pos)) => format!(
            "{{\"fs\":\"LocalFileSystem\",\"path\":\"{path}\",\"op\":\"{op}\",\"bytes\":\"{bytes}\",\"pos\":\"{pos}\"}}"
        ),
        None => format!("{{\"fs\":\"LocalFileSystem\",\"path\":\"{path}\",\"op\":\"{op}\"}}"),
    }
}

/// `DUCKDB_LOG_TRACE` with the database's logger (`Logger::Get(db)`), of the default log type: for
/// what the storage layer logs while the statement of this thread runs.
pub fn log_database_trace(message: &str) {
    let Some(target) = current_file_logger() else {
        return;
    };
    let mut manager = target.manager.lock().expect("not poisoned");
    let _ = manager.write_global("", LogLevel::Trace, message, false);
}

/// `DUCKDB_LOG_FILE_SYSTEM`: logs `op` on the file `path` with the logger of the statement, or
/// the database's (`FileHandle::TryAddLogger`). Errors of logging do not fail the file access.
pub fn log_file_system(path: &str, op: &str, bytes: Option<(i64, u64)>) {
    log_file_system_to(current_file_logger().as_ref(), path, op, bytes);
}

/// The logger the files this thread opens now log to, to keep for a file that is opened later
/// on another thread.
pub fn current_file_logger() -> Option<Arc<LogTarget>> {
    FILE_LOGGER.with(|logger| logger.borrow().clone())
}

/// [`log_file_system`] with the logger kept for the file.
pub fn log_file_system_to(target: Option<&Arc<LogTarget>>, path: &str, op: &str, bytes: Option<(i64, u64)>) {
    let Some(target) = target else {
        return;
    };
    let (name, level) = ("FileSystem", LogLevel::Trace);
    let logger = target.logger.lock().expect("not poisoned").clone();
    let message = file_system_message(path, op, bytes);
    if logger.should_log(name, level) {
        let _ = logger.log(&target.manager, name, level, &message, false);
        return;
    }
    let mut manager = target.manager.lock().expect("not poisoned");
    let _ = manager.write_global(name, level, &message, false);
}

/// `DUCKDB_LOG(context, ExternalResourceLogType, ...)`: one lifecycle operation of an external resource, `error`
/// empty when it succeeded (`ExternalResourceLogType::ConstructLogMessage`).
pub fn log_external_resource(
    target: Option<&Arc<LogTarget>>,
    resource_type: &str,
    resource_name: &str,
    operation: &str,
    error: &str,
    extra_info: &[(&str, &str)],
) {
    let Some(target) = target else {
        return;
    };
    let (name, level) = ("ExternalResource", LogLevel::Info);
    let logger = target.logger.lock().expect("not poisoned").clone();
    if !logger.should_log(name, level) {
        return;
    }
    use duckrs_common::types::LogicalType;
    use duckrs_common::value::{Value, ValueData};
    let nullable = |text: &str| {
        if text.is_empty() {
            Value::null(LogicalType::VARCHAR)
        } else {
            Value::varchar(text)
        }
    };
    let entries = extra_info
        .iter()
        .map(|(key, value)| {
            Value::struct_value(vec![
                ("key".to_string(), Value::varchar(*key)),
                ("value".to_string(), Value::varchar(*value)),
            ])
        })
        .collect();
    let Ok(extra) = Value::new(
        LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
        ValueData::List(entries),
    ) else {
        return;
    };
    // `Value::STRUCT(...).ToString()`
    let message = Value::struct_value(vec![
        ("resource_type".to_string(), nullable(resource_type)),
        ("resource_name".to_string(), nullable(resource_name)),
        ("operation".to_string(), Value::varchar(operation)),
        ("outcome".to_string(), Value::varchar(if error.is_empty() { "ok" } else { "error" })),
        ("error".to_string(), nullable(error)),
        ("extra_info".to_string(), extra),
    ]);
    let Ok(message) = message.try_to_string() else {
        return;
    };
    let _ = logger.log(&target.manager, name, level, &message, false);
}

/// `PhysicalOperatorLogType::ConstructLogMessage` of a table scan: `class` and `event` with the pairs of `info`.
fn physical_operator_message(class: &str, event: &str, info: &[(&str, String)]) -> String {
    let info = info
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{{'operator_type': TABLE_SCAN, 'parameters': {{}}, 'class': {class}, 'event': {event}, 'info': {{{info}}}}}"
    )
}

/// `DUCKDB_LOG(context, PhysicalOperatorLogType, ...)` of a table scan, with the logger of the statement it runs
/// in. Errors of logging do not fail the scan.
pub fn log_physical_operator(target: Option<&Arc<LogTarget>>, class: &str, event: &str, info: &[(&str, String)]) {
    let Some(target) = target else {
        return;
    };
    let (name, level) = ("PhysicalOperator", LogLevel::Debug);
    let logger = target.logger.lock().expect("not poisoned").clone();
    if logger.should_log(name, level) {
        let message = physical_operator_message(class, event, info);
        let _ = logger.log(&target.manager, name, level, &message, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_storage_writes_csv() {
        let directory = std::env::temp_dir().join(format!("duckrs-log-test-{}", std::process::id()));
        let path = directory.join("log.csv").to_string_lossy().into_owned();
        let mut storage = FileStorage::new();
        storage
            .update_config(&[("path".into(), Value::varchar(path.clone()))])
            .unwrap();
        let context = RegisteredContext {
            id: 3,
            context: LoggingContext::new(LogScope::Connection),
        };
        let entry = LogEntry {
            context_id: 3,
            timestamp: 0,
            log_type: "QueryLog".into(),
            level: LogLevel::Info,
            message: "SELECT 'a,b'".into(),
        };
        storage.write(entry, &context).unwrap();
        storage.flush_all().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("context_id,scope,connection_id,transaction_id,query_id,thread_id,timestamp,type,log_level,message")
        );
        let row = lines.next().unwrap();
        assert!(row.starts_with("3,CONNECTION,,,,,"), "{row}");
        assert!(row.ends_with(",QueryLog,INFO,\"SELECT 'a,b'\""), "{row}");
        assert!(
            storage
                .scan_sql(LogTable::All)
                .unwrap()
                .unwrap()
                .contains("read_csv_auto")
        );
        assert!(storage.scan_sql(LogTable::Entries).unwrap().is_none());
        storage.truncate().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        let err = storage
            .update_config(&[
                ("path".into(), Value::varchar(path.clone())),
                ("normalize".into(), Value::boolean(true)),
            ])
            .unwrap_err();
        assert!(err.to_string().contains("while normalize is true"), "{err}");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn entries_show_after_a_flush() {
        let manager: SharedLogManager = Arc::new(Mutex::new(LogManager::default()));
        let logger = {
            let mut m = manager.lock().unwrap();
            m.set_enabled(true);
            let mut context = LoggingContext::new(LogScope::Connection);
            context.connection_id = Some(7);
            m.create_logger(context)
        };
        logger
            .log(&manager, "QueryLog", LogLevel::Info, "SELECT 1", false)
            .unwrap();
        logger
            .log(&manager, "FileSystem", LogLevel::Trace, "hidden", false)
            .unwrap();
        assert!(manager.lock().unwrap().scan().unwrap().0.is_empty());
        manager.lock().unwrap().flush().unwrap();
        let rows = manager.lock().unwrap().denormalized_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::varchar("CONNECTION"));
        assert_eq!(rows[0][9], Value::varchar("SELECT 1"));
        let err = manager.lock().unwrap().set_storage("quack").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid Input Error: Log storage 'quack' is not yet registered"
        );
        assert!(
            LogLevel::parse("bogus")
                .unwrap_err()
                .to_string()
                .contains("for enum \"LogLevel\"")
        );
        let err = logger
            .log(&manager, "", LogLevel::Warning, "careful", true)
            .unwrap_err();
        assert_eq!(err.to_string(), "Invalid Input Error: careful");
    }
}
