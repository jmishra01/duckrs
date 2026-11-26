// Ported from duckdb/src/main/capi/v1/duckdb-c.cpp and config-c.cpp @ rust-port-baseline

//! Databases, connections, configurations and `duckdb_query`.

use crate::ffi::{self, Handle};
use crate::result::translate_result;
use crate::types::*;
use duckrs::{Connection, Database, LogicalType, Value};
use std::ffi::c_char;
use std::sync::{Arc, Mutex, MutexGuard};

/// `DatabaseWrapper`.
pub struct DatabaseWrapper {
    database: Database,
}

/// A connection (`Connection`). Prepared statements and appenders share it, as C++'s share its `ClientContext`.
pub struct ConnectionWrapper {
    pub(crate) connection: SharedConnection,
}

/// A connection that statements and appenders made on it hold on to.
#[derive(Clone)]
pub struct SharedConnection(Arc<Mutex<Connection>>);

impl SharedConnection {
    /// The connection, to run something on; a connection whose last user panicked is still usable.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `DBConfig`: the options set on it, by name, in order.
#[derive(Debug, Default)]
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
    let mut connection = database.connect()?;
    for (name, value) in options {
        if ["access_mode", "storage_compatibility_version", "duckdb_api"]
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
    let opened = ffi::guard(None, || Some(open(path.as_deref(), config)));
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
        Some(Err(error)) => error.to_string(),
        None => "Unknown error".to_string(),
    };
    // SAFETY: C passes where to write the error, or no place.
    unsafe { ffi::write(out_error, ffi::strdup(error)) };
    DuckDBError
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
    let handle = ffi::into_handle(ConnectionWrapper {
        connection: SharedConnection(Arc::new(Mutex::new(connection))),
    });
    // SAFETY: C passes where to write the connection; it is not null.
    unsafe { ffi::write(out_connection, handle) };
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_disconnect(connection: *mut duckdb_connection) {
    // SAFETY: C passes a connection `duckdb_connect` made.
    drop(unsafe { ffi::take(connection) });
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
