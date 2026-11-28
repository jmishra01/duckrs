// Ported from duckdb/src/main/client_context_execution.cpp (RefreshLoggerInternal, the logger of
// EndQueryInternal), the logging settings of duckdb/src/main/settings/custom_settings.cpp and
// duckdb/src/function/table/system/logging_utils.cpp @ rust-port-baseline
//
//! What a connection does for the log: a logger per statement, the query log, the logging
//! settings, and `enable_logging`, `disable_logging` and `truncate_duckdb_logs`.

use super::ClientContext;
use super::settings::{self, SettingValues};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_core::execution::expression_executor::evaluate_scalar;
use duckrs_core::logging::{
    FILE_STORAGE, LogConfig, LogLevel, LogManager, LogMode, LogScope, LogTarget, LoggingContext, lookup_log_type,
};
use duckrs_core::planner::binder::bind_constant_expression;
use duckrs_parser::ast::{Expr, ExprKind};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The settings whose value is the log manager's (`GetSetting` reads it from there).
const LOGGING_SETTINGS: [&str; 6] = [
    "enable_logging",
    "logging_level",
    "logging_mode",
    "logging_storage",
    "enabled_log_types",
    "disabled_log_types",
];

/// `EnabledLogTypes::GetSetting`: the types, joined by commas.
fn joined(types: &BTreeSet<String>) -> Value {
    Value::varchar(types.iter().cloned().collect::<Vec<_>>().join(","))
}

/// Stores the log manager's configuration as the values of the logging settings.
pub(super) fn sync_settings(manager: &LogManager, global: &mut SettingValues) {
    let config = manager.config();
    // `EnableLogging::GetSetting` gives the flag as a `Value` of a `bool`, which is an INTEGER
    settings::store_global("enable_logging", Value::integer(i32::from(config.enabled)), global);
    settings::store_global("logging_level", Value::varchar(config.level.name()), global);
    settings::store_global("logging_mode", Value::varchar(config.mode.name()), global);
    settings::store_global("logging_storage", Value::varchar(config.storage.clone()), global);
    settings::store_global("enabled_log_types", joined(&config.enabled_log_types), global);
    settings::store_global("disabled_log_types", joined(&config.disabled_log_types), global);
}

/// `StringUtil::Split(..., ",")` into a set.
fn split_types(value: &Value) -> Result<BTreeSet<String>> {
    if value.is_null() {
        return Ok(BTreeSet::new());
    }
    Ok(value
        .try_to_string()?
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect())
}

/// Whether `name` is `enable_logging` and the like, by its canonical name.
pub(super) fn is_logging_setting(name: &str) -> Option<&'static str> {
    let (_, setting) = settings::find_setting(name)?;
    LOGGING_SETTINGS.iter().copied().find(|s| *s == setting.name)
}

/// What `enable_logging(...)` was called with (`EnableLoggingBindData`).
struct EnableLogging {
    config: LogConfig,
    types: Vec<String>,
    storage_config: Vec<(String, Value)>,
}

/// The logging call `function` is, if it is one: `enable_logging`, `disable_logging` or
/// `truncate_duckdb_logs`.
pub(super) fn logging_call(function: &Expr) -> Option<&'static str> {
    let ExprKind::Function(call) = &function.kind else {
        return None;
    };
    if !call.name.qualification.is_empty() {
        return None;
    }
    ["enable_logging", "disable_logging", "truncate_duckdb_logs"]
        .into_iter()
        .find(|name| call.name.name.eq_ignore_ascii_case(name))
}

impl ClientContext {
    fn log_manager(&self) -> std::sync::MutexGuard<'_, LogManager> {
        self.home.log_manager.lock().expect("the log manager is not poisoned")
    }

    fn warnings_as_errors(&self) -> bool {
        self.setting("warnings_as_errors")
            .is_some_and(|v| matches!(v.data(), ValueData::Bool(true)))
    }

    /// Writes to the log with the connection's logger (`DUCKDB_LOG`).
    pub(super) fn log(&self, log_type: &str, level: LogLevel, message: &str) -> Result<()> {
        let logger = self.log_target.logger.lock().expect("not poisoned").clone();
        logger.log(
            &self.home.log_manager,
            log_type,
            level,
            message,
            self.warnings_as_errors(),
        )
    }

    /// The logger of a connection outside a statement: of the connection alone.
    pub(super) fn new_log_target(db: &super::DatabaseInstance, connection_id: u64) -> Arc<LogTarget> {
        let manager = db.log_manager.clone();
        let mut context = LoggingContext::new(LogScope::Connection);
        context.connection_id = Some(connection_id);
        let logger = manager.lock().expect("not poisoned").create_logger(context);
        Arc::new(LogTarget {
            manager,
            logger: std::sync::Mutex::new(logger),
            warnings_as_errors: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// `RefreshLoggerInternal`: a logger with the configuration of now for the statement `text`,
    /// which it logs.
    pub(super) fn begin_statement_logging(&mut self, text: &str) -> Result<()> {
        // an auto-commit statement is a transaction of its own
        let transaction_id = match &self.transaction {
            Some(transaction) => transaction.number,
            None => self.home.transaction_number.fetch_add(1, Ordering::Relaxed) + 1,
        };
        self.statement_transaction = transaction_id;
        let query_id = self.home.query_number.fetch_add(1, Ordering::Relaxed) + 1;
        self.query_id = query_id;
        let logger = {
            let mut manager = self.log_manager();
            manager.flush()?;
            let mut context = LoggingContext::new(LogScope::Connection);
            context.connection_id = Some(self.id);
            context.transaction_id = Some(transaction_id);
            context.query_id = Some(query_id);
            manager.create_logger(context)
        };
        *self.log_target.logger.lock().expect("not poisoned") = logger;
        self.log_target
            .warnings_as_errors
            .store(self.warnings_as_errors(), Ordering::Relaxed);
        self.log("QueryLog", LogLevel::Info, text)
    }

    /// The logger of `EndQueryInternal`, of the connection alone.
    pub(super) fn end_statement_logging(&mut self) {
        let logger = {
            let mut manager = self.log_manager();
            // C++ throws from the end of a query too; a statement's own error comes first here
            let _ = manager.flush();
            let mut context = LoggingContext::new(LogScope::Connection);
            context.connection_id = Some(self.id);
            manager.create_logger(context)
        };
        *self.log_target.logger.lock().expect("not poisoned") = logger;
    }

    /// The `SetGlobal` of the logging settings: `value` (already checked as a value of the
    /// setting) goes to the log manager.
    pub(super) fn set_logging_setting(&mut self, name: &str, value: &Value) -> Result<()> {
        let text = || value.try_to_string();
        {
            let mut manager = self.log_manager();
            match name {
                "enable_logging" => {
                    if value.is_null() {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "enable_logging setting cannot be NULL",
                        ));
                    }
                    manager.set_enabled(matches!(value.data(), ValueData::Bool(true)));
                }
                "logging_level" => manager.set_level(LogLevel::parse(&text()?)?),
                "logging_mode" => manager.set_mode(LogMode::parse(&text()?)?),
                "logging_storage" => manager.set_storage(&text()?)?,
                "enabled_log_types" => manager.set_enabled_log_types(split_types(value)?),
                "disabled_log_types" => manager.set_disabled_log_types(split_types(value)?),
                _ => unreachable!("a logging setting"),
            }
        }
        self.sync_logging_settings();
        Ok(())
    }

    /// The `ResetGlobal` of the logging settings.
    pub(super) fn reset_logging_setting(&mut self, name: &str) -> Result<()> {
        {
            let mut manager = self.log_manager();
            match name {
                "enable_logging" => manager.set_enabled(false),
                "logging_level" => manager.set_level(LogLevel::Info),
                "logging_mode" => manager.set_mode(LogMode::LevelOnly),
                "logging_storage" => manager.set_storage(duckrs_core::logging::MEMORY_STORAGE)?,
                "enabled_log_types" => manager.set_enabled_log_types(BTreeSet::new()),
                "disabled_log_types" => manager.set_disabled_log_types(BTreeSet::new()),
                _ => unreachable!("a logging setting"),
            }
        }
        self.sync_logging_settings();
        Ok(())
    }

    pub(super) fn sync_logging_settings(&self) {
        let manager = self.log_manager();
        sync_settings(&manager, &mut self.home.settings.write());
    }

    /// `enable_logging`, `disable_logging` and `truncate_duckdb_logs`, which give no rows.
    pub(super) fn run_logging_call(&mut self, function: &Expr) -> Result<super::MaterializedQueryResult> {
        let name = logging_call(function).expect("checked by the caller");
        match name {
            "enable_logging" => {
                let call = self.bind_enable_logging(function)?;
                self.log(
                    "",
                    LogLevel::Warning,
                    "The logging settings have been changed so you may lose warnings printed in the CLI.\nTo \
                     continue printing warnings to the console, set storage='shell_log_storage'.\nFor more info \
                     see https://duckdb.org/docs/current/operations_manual/logging/overview.",
                )?;
                self.log_manager()
                    .enable_logging(call.config, &call.types, &call.storage_config)?;
            }
            "disable_logging" => self.log_manager().set_enabled(false),
            _ => self.log_manager().truncate()?,
        }
        self.sync_logging_settings();
        Ok(Self::success())
    }

    /// `BindEnableLogging`.
    fn bind_enable_logging(&self, function: &Expr) -> Result<EnableLogging> {
        let ExprKind::Function(call) = &function.kind else {
            unreachable!("a function call");
        };
        let catalog = self.catalog();
        // `TableFunctionBinder::BindColumnReference`: a column name is the text of the name
        let constant = |expr: &Expr| match &expr.kind {
            ExprKind::ColumnRef(names) if names.len() == 1 => Ok(Value::varchar(names[0].clone())),
            _ => evaluate_scalar(&bind_constant_expression(&catalog, expr)?),
        };
        let arguments: Vec<_> = call
            .args
            .iter()
            .map(duckrs_core::planner::binder::named_argument)
            .collect();
        let positional: Vec<&Expr> = arguments
            .iter()
            .filter(|a| a.name.is_empty())
            .map(|a| &a.expr)
            .collect();
        if positional.len() > 1 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "EnableLogging: expected 0 or 1 parameter",
            ));
        }
        let current = self.log_manager().config().clone();
        // the configuration starts from the defaults, with the level and storage given
        let mut config = LogConfig::default();
        let mut storage = None;
        let mut storage_config: Vec<(String, Value)> = Vec::new();
        let mut set_option = |key: &str, value: Value| {
            storage_config.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
            storage_config.push((key.to_string(), value));
        };
        for argument in arguments.iter().filter(|a| !a.name.is_empty()) {
            let value = constant(&argument.expr)?;
            match argument.name.to_lowercase().as_str() {
                "level" => {
                    if value.is_null() {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "EnableLogging: level cannot be NULL",
                        ));
                    }
                    config.level = LogLevel::parse(&value.try_to_string()?)?;
                }
                "storage" => storage = Some(value.try_to_string()?),
                "storage_config" => {
                    let ValueData::Struct(children) = value.data() else {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "EnableLogging: storage_config must be a struct",
                        ));
                    };
                    for ((key, _), child) in value.logical_type().struct_children().iter().zip(children) {
                        set_option(key, child.clone());
                    }
                }
                "storage_path" => set_option("path", value),
                "storage_normalize" => set_option("normalize", value),
                "storage_buffer_size" => set_option("buffer_size", value),
                _ => {
                    return Err(Error::new(
                        ErrorKind::Binder,
                        format!(
                            "Invalid named parameter \"{}\" for function enable_logging",
                            argument.name
                        ),
                    ));
                }
            }
        }
        // without storage=, a path means a file, and otherwise the storage stays as it is
        config.storage = match storage {
            Some(storage) => storage,
            None if storage_config.iter().any(|(k, _)| k.eq_ignore_ascii_case("path")) => FILE_STORAGE.into(),
            None => current.storage.clone(),
        };
        // `SetLogStorageInternal`
        if config.storage.eq_ignore_ascii_case(FILE_STORAGE)
            && current.storage != FILE_STORAGE
            && self
                .setting_text("disabled_filesystems")
                .split(',')
                .any(|f| f.trim() == "LocalFileSystem")
        {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Can not enable file logging with the LocalFileSystem disabled",
            ));
        }
        // `VerifyFileStoragePath`
        if config.storage.eq_ignore_ascii_case(FILE_STORAGE) && current.storage != FILE_STORAGE {
            let usable = storage_config.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("path") && !v.is_null() && !v.try_to_string().unwrap_or_default().is_empty()
            });
            if !usable {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Cannot enable 'file' log storage without a valid path. Provide one via storage_path, e.g. CALL \
                     enable_logging(storage='file', storage_path='mylog.csv');",
                ));
            }
        }
        let mut types = Vec::new();
        if let Some(argument) = positional.first() {
            let value = constant(argument)?;
            match value.data() {
                ValueData::Bytes(_) if value.logical_type() == &LogicalType::VARCHAR => {
                    types.push(value.try_to_string()?);
                }
                ValueData::List(items) if value.logical_type() == &LogicalType::list(LogicalType::VARCHAR) => {
                    for item in items {
                        types.push(item.try_to_string()?);
                    }
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Binder,
                        "Unexpected type positional parameter to enable_logging",
                    ));
                }
            }
        }
        for log_type in &types {
            if lookup_log_type(log_type).is_none() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Unknown log type: '{log_type}'"),
                ));
            }
        }
        Ok(EnableLogging {
            config,
            types,
            storage_config,
        })
    }
}
