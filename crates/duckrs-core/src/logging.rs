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
fn enum_from<T: Copy>(values: &[(T, &str)], enum_name: &str, text: &str) -> Result<T> {
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
    let mut fields: Vec<String> = context_values(context)
        .iter()
        .chain(&entry.values()[1..])
        .map(|value| {
            if value.is_null() {
                String::new()
            } else {
                value.try_to_string().unwrap_or_default()
            }
        })
        .collect();
    for field in &mut fields {
        if field.contains(['\t', '"', '\n', '\r']) {
            *field = format!("\"{}\"", field.replace('"', "\"\""));
        }
    }
    println!("{}", fields.join("\t"));
}

#[derive(Debug)]
enum Storage {
    Memory(MemoryStorage),
    Stdout,
}

impl Storage {
    fn create(name: &str) -> Result<Self> {
        match name {
            MEMORY_STORAGE => Ok(Storage::Memory(MemoryStorage::new())),
            STDOUT_STORAGE => Ok(Storage::Stdout),
            FILE_STORAGE => Err(crate::unported("the 'file' log storage")),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Log storage '{name}' is not yet registered"),
            )),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Storage::Memory(_) => "InMemoryLogStorage",
            Storage::Stdout => "StdOutLogStorage",
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
}

impl Default for LogManager {
    fn default() -> Self {
        let mut manager = Self {
            config: LogConfig::default(),
            storage: Storage::Memory(MemoryStorage::new()),
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
            Storage::Stdout => {
                print_entry(&entry, context);
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

    pub fn flush(&mut self) {
        if let Storage::Memory(storage) = &mut self.storage {
            storage.flush_all();
        }
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
        let storage = Storage::create(&lower).map_err(|error| {
            if error.kind == ErrorKind::InvalidInput {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Log storage '{name}' is not yet registered"),
                )
            } else {
                error
            }
        })?;
        self.flush();
        self.storage = storage;
        self.config.storage = lower;
        Ok(())
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
            let storage = Storage::create(&name).map_err(|error| {
                if error.kind == ErrorKind::InvalidInput {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!("Log storage '{}' is not yet registered", config.storage),
                    )
                } else {
                    error
                }
            })?;
            self.flush();
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
            other if !config.is_empty() => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Log storage '{}' does not support passing configuration", other.name()),
            )),
            _ => Ok(()),
        }
    }

    /// `LogManager::TruncateLogStorage`.
    pub fn truncate(&mut self) {
        if let Storage::Memory(storage) = &mut self.storage {
            storage.reset();
        }
    }

    /// `DuckDBLogInit` and `DuckDBLogContextInit`: what a scan sees, if the storage can be scanned.
    pub fn scan(&self) -> Result<(&[LogEntry], &[RegisteredContext])> {
        match &self.storage {
            Storage::Memory(storage) => Ok((&storage.flushed_entries, &storage.flushed_contexts)),
            Storage::Stdout => Err(Error::new(
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

#[cfg(test)]
mod tests {
    use super::*;

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
        manager.lock().unwrap().flush();
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
