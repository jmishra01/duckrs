// Ported from duckdb/src/main/capi/v1/duckdb-c.cpp and config-c.cpp, and the parts of
// duckdb/src/main/db_instance_cache.cpp the instance cache uses @ rust-port-baseline

//! Databases, the instance cache, connections, configurations and `duckdb_query`.

use crate::ffi::{self, Handle};
use crate::result::translate_result;
use crate::types::*;
use duckrs::{Connection, Database, LogicalType, Value};
use std::ffi::c_char;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// `DatabaseWrapper`.
pub struct DatabaseWrapper {
    database: Database,
}

impl DatabaseWrapper {
    /// The database (`DatabaseWrapper::database`).
    pub(crate) fn database(&self) -> &Database {
        &self.database
    }
}

/// A connection (`Connection`). Prepared statements and appenders share it, as C++'s share its `ClientContext`.
pub struct ConnectionWrapper {
    pub(crate) connection: SharedConnection,
    /// `ClientContext::GetConnectionId`, which a callback of a query on the connection asks for while the query has
    /// the connection
    pub(crate) connection_id: u64,
}

/// What a client context (`duckdb_client_context`, which holds a connection id) reaches of its connection: the
/// database and the connection, by connection id, for each connection `duckdb_connect` made until it is
/// disconnected.
static CONTEXTS: Mutex<Option<HashMap<u64, (duckrs::WeakDatabase, SharedConnection)>>> = Mutex::new(None);

/// The database and the connection of the connection `connection_id`, while it is connected.
pub(crate) fn context_of(connection_id: u64) -> Option<(duckrs::WeakDatabase, SharedConnection)> {
    let contexts = CONTEXTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    contexts.as_ref()?.get(&connection_id).cloned()
}

/// A connection that statements and appenders made on it hold on to.
#[derive(Clone)]
pub struct SharedConnection(Arc<Mutex<Connection>>);

impl SharedConnection {
    /// The connection, to run something on; a connection whose last user panicked is still usable.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether something runs on the connection now (a query, whose callbacks ask): it is then in a transaction.
    pub(crate) fn is_busy(&self) -> bool {
        matches!(self.0.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }
}

/// `DBConfig`: the options set on it, by name, in order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    options: Vec<(String, String)>,
}

impl Handle for _duckdb_database {
    type Target = DatabaseWrapper;
}

impl Handle for _duckdb_connection {
    type Target = ConnectionWrapper;
}

impl Handle for _duckdb_config {
    type Target = Config;
}

/// `DBInstanceCache`: the databases it opened, by `GetCacheKey` of their path, while they are open, with the options
/// each was opened with.
#[derive(Default)]
pub struct InstanceCache {
    instances: Mutex<HashMap<String, (duckrs::WeakDatabase, Config)>>,
}

impl Handle for _duckdb_instance_cache {
    type Target = InstanceCache;
}

const IN_MEMORY_PATH: &str = ":memory:";

/// `GetDBAbsolutePath`: `:memory:` for an empty path, a path of an in-memory database as it is, and any other path
/// made absolute and normalized (`CanonicalizePath`), after `~` is expanded (`ExpandPath`).
fn absolute_path(path: &str) -> String {
    let path = match path.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", std::env::var("HOME").unwrap_or_default()),
        None => path.to_string(),
    };
    if path.is_empty() {
        return IN_MEMORY_PATH.to_string();
    }
    if path.starts_with(IN_MEMORY_PATH) || path.contains("://") {
        return path;
    }
    let joined = match std::env::current_dir() {
        Ok(dir) if !std::path::Path::new(&path).is_absolute() => dir.join(&path),
        _ => std::path::PathBuf::from(&path),
    };
    let mut normalized = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized.to_string_lossy().into_owned()
}

/// `GetCacheKey`: the absolute path, in lower case where the file system ignores case.
fn cache_key(path: &str) -> String {
    let path = absolute_path(path);
    if cfg!(any(windows, target_os = "macos")) {
        path.to_lowercase()
    } else {
        path
    }
}

impl InstanceCache {
    /// `DBInstanceCache::GetOrCreateInstance` with `CacheBehavior::AUTOMATIC`: the open database of `path` if there
    /// is one (with the same options), else a new one, kept unless it is an unnamed in-memory one.
    fn get_or_create(&self, path: &str, config: Option<&Config>) -> Result<Database, String> {
        let default = Config::default();
        let config = config.unwrap_or(&default);
        let cached = !(path.is_empty() || path == IN_MEMORY_PATH);
        let key = cache_key(path);
        let mut instances = self.instances.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if cached && let Some((weak, options)) = instances.get(&key) {
            match weak.upgrade() {
                Some(database) if options != config => {
                    drop(database);
                    return Err("Connection Error: Can't open a connection to same database file with a different                                 configuration than existing connections"
                        .to_string());
                }
                Some(database) => return Ok(database),
                None => {
                    instances.remove(&key);
                }
            }
        }
        // `CreateInstanceInternal`: the database opens at its absolute path
        let mut instance_path = absolute_path(path);
        if instance_path.starts_with(IN_MEMORY_PATH) {
            instance_path = IN_MEMORY_PATH.to_string();
        }
        let database = open(Some(&instance_path), Some(config)).map_err(|error| error.to_string())?;
        if cached {
            instances.insert(key, (database.downgrade(), config.clone()));
        }
        Ok(database)
    }
}

/// `DBConfig::SetOptionByName`: `value` must name a setting and fit its type.
fn check_option(name: &str, value: &str) -> Result<(), String> {
    let Some((_, setting)) = duckrs::settings::find_setting(name) else {
        return Err(format!("Invalid Input Error: The following options were not recognized: {name}"));
    };
    let ty = match setting.input_type {
        "BOOLEAN" => LogicalType::BOOLEAN,
        "UBIGINT" => LogicalType::UBIGINT,
        "BIGINT" => LogicalType::BIGINT,
        "DOUBLE" => LogicalType::DOUBLE,
        _ => LogicalType::VARCHAR,
    };
    duckrs::cast_value(&Value::varchar(value), &ty).map_err(|error| error.to_string())?;
    if !setting.enum_values.is_empty() && !setting.enum_values.iter().any(|v| v.eq_ignore_ascii_case(value)) {
        return Err(format!(
            "Invalid Input Error: Unrecognized value \"{value}\" for setting \"{name}\""
        ));
    }
    Ok(())
}

impl Config {
    fn option(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Opens the database at `path` (in memory without one) with the options of `config`: `access_mode` and
/// `storage_compatibility_version` say how the file opens, and the others are set as `SET GLOBAL` sets them.
fn open(path: Option<&str>, config: Option<&Config>) -> duckrs::Result<Database> {
    let options = config.map(|c| c.options.as_slice()).unwrap_or_default();
    let read_only = config
        .and_then(|c| c.option("access_mode"))
        .is_some_and(|mode| mode.eq_ignore_ascii_case("read_only"));
    let in_memory = path.is_none_or(|p| p.is_empty() || p.starts_with(":memory:"));
    let database = match (path, config.and_then(|c| c.option("storage_compatibility_version"))) {
        _ if in_memory => Database::open_in_memory()?,
        (Some(path), Some(version)) => Database::open_with_storage_version(path, read_only, version)?,
        (Some(path), None) => Database::open(path, read_only)?,
        (None, _) => unreachable!("no path is in memory"),
    };
    // `duckdb_create_config` (or `duckdb_open_internal` without a config) sets `duckdb_api` to `capi`; it and
    // `custom_user_agent` are set when the database starts, not after
    let api = config.and_then(|c| c.option("duckdb_api")).unwrap_or("capi");
    database.set_startup_setting("duckdb_api", api);
    if let Some(agent) = config.and_then(|c| c.option("custom_user_agent")) {
        database.set_startup_setting("custom_user_agent", agent);
    }
    let mut connection = database.connect()?;
    for (name, value) in options {
        if ["access_mode", "storage_compatibility_version", "duckdb_api", "custom_user_agent"]
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let quoted_name = name.replace('"', "\"\"");
        let quoted_value = value.replace('\'', "''");
        connection.query(&format!("SET GLOBAL \"{quoted_name}\" = '{quoted_value}'"))?;
    }
    Ok(database)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_open_ext(
    path: *const c_char,
    out_database: *mut duckdb_database,
    config: duckdb_config,
    out_error: *mut *mut c_char,
) -> duckdb_state {
    // SAFETY: C passes a NUL-terminated path, or none.
    let path = unsafe { ffi::c_str(path) }.map(String::from_utf8_lossy);
    // SAFETY: C passes a configuration `duckdb_create_config` made, or none.
    let config = unsafe { ffi::get(config) };
    let opened = ffi::guard(None, || Some(open(path.as_deref(), config).map_err(|error| error.to_string())));
    // SAFETY: C passes where to write the database and the error.
    unsafe { finish_open(opened, out_database, out_error) }
}

/// The end of `duckdb_open_internal`: the database `opened` gave written to `out_database`, or its error to
/// `out_error`.
///
/// # Safety
///
/// `out_database` and `out_error` are null or valid for a write of a pointer.
unsafe fn finish_open(
    opened: Option<Result<Database, String>>,
    out_database: *mut duckdb_database,
    out_error: *mut *mut c_char,
) -> duckdb_state {
    let error = match opened {
        Some(Ok(database)) => {
            let handle = ffi::into_handle(DatabaseWrapper { database });
            // SAFETY: C passes where to write the database.
            if unsafe { ffi::write(out_database, handle) } {
                return DuckDBSuccess;
            }
            // SAFETY: the handle was just made, and C never saw it.
            drop(unsafe { ffi::take(&mut { handle }) });
            return DuckDBError;
        }
        Some(Err(error)) => error,
        None => "Unknown error".to_string(),
    };
    // SAFETY: C passes where to write the error, or no place.
    unsafe { ffi::write(out_error, ffi::strdup(error)) };
    DuckDBError
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_instance_cache() -> duckdb_instance_cache {
    ffi::into_handle(InstanceCache::default())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_instance_cache(instance_cache: *mut duckdb_instance_cache) {
    // SAFETY: C passes a cache `duckdb_create_instance_cache` made; the databases it opened stay open.
    drop(unsafe { ffi::take(instance_cache) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_or_create_from_cache(
    instance_cache: duckdb_instance_cache,
    path: *const c_char,
    out_database: *mut duckdb_database,
    config: duckdb_config,
    out_error: *mut *mut c_char,
) -> duckdb_state {
    // SAFETY: C passes a live instance cache, or none.
    let Some(cache) = (unsafe { ffi::get(instance_cache) }) else {
        // SAFETY: C passes where to write the error, or no place.
        unsafe { ffi::write(out_error, ffi::strdup("instance cache cannot be nullptr")) };
        return DuckDBError;
    };
    // SAFETY: C passes a NUL-terminated path, or none.
    let path = unsafe { ffi::c_str(path) }.map(String::from_utf8_lossy).unwrap_or_default();
    // SAFETY: C passes a configuration `duckdb_create_config` made, or none.
    let config = unsafe { ffi::get(config) };
    let opened = ffi::guard(None, || Some(cache.get_or_create(&path, config)));
    // SAFETY: C passes where to write the database and the error.
    unsafe { finish_open(opened, out_database, out_error) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_open(path: *const c_char, out_database: *mut duckdb_database) -> duckdb_state {
    // SAFETY: C passes what `duckdb_open_ext` takes.
    unsafe { duckdb_open_ext(path, out_database, std::ptr::null_mut(), std::ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_close(database: *mut duckdb_database) {
    // SAFETY: C passes a database `duckdb_open` made.
    drop(unsafe { ffi::take(database) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_connect(database: duckdb_database, out_connection: *mut duckdb_connection) -> duckdb_state {
    // SAFETY: C passes a live database.
    let Some(database) = (unsafe { ffi::get(database) }) else {
        return DuckDBError;
    };
    if out_connection.is_null() {
        return DuckDBError;
    }
    let Some(Ok(connection)) = ffi::guard(None, || Some(database.database.connect())) else {
        return DuckDBError;
    };
    let connection_id = connection.connection_id();
    let shared = SharedConnection(Arc::new(Mutex::new(connection)));
    CONTEXTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(connection_id, (database.database.downgrade(), shared.clone()));
    let handle = ffi::into_handle(ConnectionWrapper {
        connection: shared,
        connection_id,
    });
    // SAFETY: C passes where to write the connection; it is not null.
    unsafe { ffi::write(out_connection, handle) };
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_disconnect(connection: *mut duckdb_connection) {
    // SAFETY: C passes a connection `duckdb_connect` made.
    let Some(connection) = (unsafe { ffi::take(connection) }) else {
        return;
    };
    let mut contexts = CONTEXTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(contexts) = contexts.as_mut() {
        contexts.remove(&connection.connection_id);
    }
}

/// `Connection::Interrupt`. duckrs runs a query to its end, so there is nothing to interrupt yet.
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_interrupt(_connection: duckdb_connection) {}

/// `duckdb_query_progress`: duckrs does not track the progress of a query, which is reported as unknown (-1).
#[unsafe(no_mangle)]
pub extern "C" fn duckdb_query_progress(_connection: duckdb_connection) -> duckdb_query_progress_type {
    duckdb_query_progress_type {
        percentage: -1.0,
        rows_processed: 0,
        total_rows_to_process: 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query(
    connection: duckdb_connection,
    query: *const c_char,
    out_result: *mut duckdb_result,
) -> duckdb_state {
    // SAFETY: C passes a live connection.
    let Some(connection) = (unsafe { ffi::get(connection) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a NUL-terminated query.
    let Some(query) = (unsafe { ffi::c_str(query) }) else {
        return DuckDBError;
    };
    let query = String::from_utf8_lossy(query);
    let outcome = ffi::guard(None, || {
        let mut connection = connection.connection.lock();
        let result = connection.query(&query);
        let properties = connection.statement_properties(&query);
        Some((result, properties))
    });
    let Some((result, properties)) = outcome else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the result, or no place.
    let out = unsafe { ffi::reference_mut(out_result) };
    translate_result(result, properties, out)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_config(out_config: *mut duckdb_config) -> duckdb_state {
    // SAFETY: C passes where to write the configuration.
    if unsafe { ffi::write(out_config, ffi::into_handle(Config::default())) } {
        DuckDBSuccess
    } else {
        DuckDBError
    }
}

/// `DBConfig::GetOptionCount() + GetAliasCount()` and the settings of the extensions, which duckrs lists with the
/// others.
fn config_flags() -> Vec<(&'static str, &'static str)> {
    let settings: Vec<_> = duckrs::settings::all_settings().collect();
    let mut flags: Vec<(&str, &str)> = settings.iter().map(|s| (s.name, s.description)).collect();
    for setting in &settings {
        flags.extend(setting.aliases.iter().map(|alias| (*alias, setting.description)));
    }
    flags
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_config_count() -> usize {
    config_flags().len()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_config_flag(
    index: usize,
    out_name: *mut *const c_char,
    out_description: *mut *const c_char,
) -> duckdb_state {
    use std::sync::OnceLock;
    static FLAGS: OnceLock<Vec<(std::ffi::CString, std::ffi::CString)>> = OnceLock::new();
    let flags = FLAGS.get_or_init(|| {
        config_flags()
            .into_iter()
            .map(|(name, description)| (crate::error::c_string(name), crate::error::c_string(description)))
            .collect()
    });
    let Some((name, description)) = flags.get(index) else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the name and the description, or no place; they live as long as the library.
    unsafe {
        ffi::write(out_name, name.as_ptr());
        ffi::write(out_description, description.as_ptr());
    }
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_set_config(config: duckdb_config, name: *const c_char, option: *const c_char) -> duckdb_state {
    // SAFETY: C passes a live configuration.
    let Some(config) = (unsafe { ffi::get_mut(config) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes NUL-terminated strings.
    let (Some(name), Some(option)) = (unsafe { ffi::c_str(name) }, unsafe { ffi::c_str(option) }) else {
        return DuckDBError;
    };
    let (name, option) = (String::from_utf8_lossy(name), String::from_utf8_lossy(option));
    if ffi::guard(Err(String::new()), || check_option(&name, &option)).is_err() {
        return DuckDBError;
    }
    config.options.push((name.into_owned(), option.into_owned()));
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_config(config: *mut duckdb_config) {
    // SAFETY: C passes a configuration `duckdb_create_config` made.
    drop(unsafe { ffi::take(config) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_table_names(
    connection: duckdb_connection,
    query: *const c_char,
    qualified: bool,
) -> duckdb_value {
    // SAFETY: C passes a live connection and a NUL-terminated query.
    let (Some(connection), Some(query)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(query) }) else {
        return std::ptr::null_mut();
    };
    let query = String::from_utf8_lossy(query);
    let names = ffi::guard(None, || connection.connection.lock().get_table_names(&query, qualified).ok());
    let Some(names) = names else {
        return std::ptr::null_mut();
    };
    let names = names.into_iter().map(Value::varchar).collect();
    match Value::list(LogicalType::VARCHAR, names) {
        Ok(list) => ffi::into_handle::<_duckdb_value>(list),
        Err(_) => std::ptr::null_mut(),
    }
}
