// Ported from duckdb/src/main/capi/v1/prepared-c.cpp @ rust-port-baseline

//! Prepared statements (`PreparedStatementWrapper`) and extracted statements (`ExtractStatementsWrapper`).

use crate::database::SharedConnection;
use crate::error::c_string;
use crate::ffi::{self, Handle};
use crate::helper::statement_type_to_c;
use crate::result::translate;
use crate::types::*;
use pintail::{LogicalType, LogicalTypeId, PreparedStatement, Value};
use pintail_common::types::{Date, Interval, Time, Timestamp};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, c_char};

/// `PreparedStatementWrapper`: a statement prepared on a connection, and the values bound to its parameters.
pub struct PreparedWrapper {
    connection: SharedConnection,
    /// the statement, or the message of the error that preparing it gave
    statement: Result<PreparedStatement, CString>,
    /// the values bound so far, by parameter name
    values: HashMap<String, Value>,
    /// `param_index_to_name`: the name of each parameter by its index
    names: BTreeMap<idx_t, String>,
    /// `error_data` of a bind to a parameter that isn't there (`success = false`), which stays
    bind_error: Option<CString>,
}

/// `ExtractStatementsWrapper`.
pub struct ExtractedStatements {
    statements: Vec<String>,
    error: Option<CString>,
}

impl Handle for _duckdb_prepared_statement {
    type Target = PreparedWrapper;
}

impl Handle for _duckdb_extracted_statements {
    type Target = ExtractedStatements;
}

impl PreparedWrapper {
    fn new(connection: SharedConnection, statement: pintail::Result<PreparedStatement>) -> Self {
        let names = match &statement {
            Ok(statement) => parameter_order(statement.parameter_names()),
            Err(_) => BTreeMap::new(),
        };
        Self {
            connection,
            statement: statement.map_err(|error| c_string(&error.to_string())),
            values: HashMap::new(),
            names,
            bind_error: None,
        }
    }

    /// The statement, unless preparing it failed.
    fn prepared(&self) -> Option<&PreparedStatement> {
        self.statement.as_ref().ok()
    }

    /// The name of parameter `index`.
    fn name(&self, index: idx_t) -> Option<&str> {
        self.names.get(&index).map(String::as_str)
    }

    /// `duckdb_execute_prepared`, or `_streaming`.
    fn execute(&self, out_result: Option<&mut duckdb_result>, streaming: bool) -> duckdb_state {
        let Some((result, properties)) = self.run() else {
            return DuckDBError;
        };
        translate(result, Some(properties), out_result, streaming)
    }
}

/// `GetNamedParameterMap`: `?` and `$1` are parameters by their number; named parameters are numbered in the order
/// they first appear.
fn parameter_order(names: &[String]) -> BTreeMap<idx_t, String> {
    let numbers: Option<Vec<idx_t>> = names.iter().map(|name| name.parse().ok()).collect();
    match numbers {
        Some(numbers) => numbers.into_iter().zip(names.iter().cloned()).collect(),
        None => names
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, name)| (i as idx_t + 1, name))
            .collect(),
    }
}

/// Prepares `query` on `connection`, as `duckdb_prepare` and `duckdb_prepare_extracted_statement` do.
fn prepare(connection: &SharedConnection, query: &str) -> PreparedWrapper {
    let statement = ffi::guard(
        Err(pintail::Error::new(
            pintail::ErrorKind::Internal,
            "preparing the statement panicked",
        )),
        || connection.lock().prepare(query),
    );
    PreparedWrapper::new(connection.clone(), statement)
}

/// Writes the prepared statement to `out`: also one that failed, whose error `duckdb_prepare_error` gives.
///
/// # Safety
///
/// `out` is valid for a write of a handle.
unsafe fn write_prepared(wrapper: PreparedWrapper, out: *mut duckdb_prepared_statement) -> duckdb_state {
    let state = if wrapper.statement.is_ok() {
        DuckDBSuccess
    } else {
        DuckDBError
    };
    // SAFETY: by the contract of this function.
    unsafe { ffi::write(out, ffi::into_handle(wrapper)) };
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepare(
    connection: duckdb_connection,
    query: *const c_char,
    out_prepared_statement: *mut duckdb_prepared_statement,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a NUL-terminated query.
    let (Some(connection), Some(query)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(query) }) else {
        return DuckDBError;
    };
    if out_prepared_statement.is_null() {
        return DuckDBError;
    }
    let wrapper = prepare(&connection.connection, &String::from_utf8_lossy(query));
    // SAFETY: C passes where to write the statement; it is not null.
    unsafe { write_prepared(wrapper, out_prepared_statement) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_prepare(prepared_statement: *mut duckdb_prepared_statement) {
    // SAFETY: C passes a statement `duckdb_prepare` made.
    drop(unsafe { ffi::take(prepared_statement) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepare_error(prepared_statement: duckdb_prepared_statement) -> *const c_char {
    // SAFETY: C passes a live prepared statement.
    let Some(wrapper) = (unsafe { ffi::get(prepared_statement) }) else {
        return std::ptr::null();
    };
    if let Some(error) = &wrapper.bind_error {
        return error.as_ptr();
    }
    match &wrapper.statement {
        Err(error) => error.as_ptr(),
        Ok(_) => std::ptr::null(),
    }
}

/// The live statement behind a handle, prepared without an error.
///
/// # Safety
///
/// `prepared_statement` is null or a live prepared statement that nothing else uses now.
unsafe fn prepared<'a>(prepared_statement: duckdb_prepared_statement) -> Option<&'a mut PreparedWrapper> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(prepared_statement) }.filter(|wrapper| wrapper.statement.is_ok())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_nparams(prepared_statement: duckdb_prepared_statement) -> idx_t {
    // SAFETY: C passes a live prepared statement.
    unsafe { prepared(prepared_statement) }.map_or(0, |wrapper| wrapper.names.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_parameter_name(
    prepared_statement: duckdb_prepared_statement,
    index: idx_t,
) -> *const c_char {
    // SAFETY: C passes a live prepared statement.
    match unsafe { prepared(prepared_statement) }.and_then(|wrapper| wrapper.name(index)) {
        Some(name) => ffi::strdup(name),
        None => std::ptr::null(),
    }
}

/// `duckdb_param_logical_type`: the type preparing found for the parameter (`TryGetParameterType`), else that of the
/// value bound to it (`BoundParameterData`, where a VARCHAR is a STRING_LITERAL), if one is.
///
/// # Safety
///
/// As for [`prepared`].
unsafe fn param_type(prepared_statement: duckdb_prepared_statement, param_idx: idx_t) -> Option<LogicalType> {
    // SAFETY: by the contract of this function.
    let wrapper = unsafe { prepared(prepared_statement) }?;
    let name = wrapper.name(param_idx)?;
    if let Some(ty) = wrapper.prepared().and_then(|statement| statement.parameter_type(name)) {
        return Some(ty.clone());
    }
    let ty = wrapper.values.get(name)?.logical_type();
    // `BoundParameterData::GetDefaultType`
    if ty.id() == LogicalTypeId::Varchar && ty.collation().is_none_or(str::is_empty) {
        return Some(LogicalType::new(LogicalTypeId::StringLiteral));
    }
    Some(ty.clone())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_param_logical_type(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes a live prepared statement.
    unsafe { param_type(prepared_statement, param_idx) }.map_or(std::ptr::null_mut(), ffi::into_handle)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_param_type(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
) -> duckdb_type {
    // SAFETY: C passes a live prepared statement.
    unsafe { param_type(prepared_statement, param_idx) }
        .map_or(DUCKDB_TYPE_INVALID, |ty| crate::helper::type_id_to_c(ty.id()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_clear_bindings(prepared_statement: duckdb_prepared_statement) -> duckdb_state {
    // SAFETY: C passes a live prepared statement.
    match unsafe { prepared(prepared_statement) } {
        Some(wrapper) => {
            wrapper.values.clear();
            DuckDBSuccess
        }
        None => DuckDBError,
    }
}

/// The names and types of the result of a statement, as far as pintail knows them when it is prepared.
///
/// # Safety
///
/// As for [`prepared`].
unsafe fn result_columns<'a>(
    prepared_statement: duckdb_prepared_statement,
) -> Option<(&'a [String], &'a [LogicalType])> {
    // SAFETY: by the contract of this function.
    let statement = unsafe { prepared(prepared_statement) }?.statement.as_ref().ok()?;
    // a statement whose parameters' types could not be resolved has one column `unknown` of type UNKNOWN
    statement.result_columns()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_statement_column_count(
    prepared_statement: duckdb_prepared_statement,
) -> idx_t {
    // SAFETY: C passes a live prepared statement.
    unsafe { result_columns(prepared_statement) }.map_or(0, |(names, _)| names.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_statement_column_name(
    prepared_statement: duckdb_prepared_statement,
    col_idx: idx_t,
) -> *const c_char {
    // SAFETY: C passes a live prepared statement.
    match unsafe { result_columns(prepared_statement) }.and_then(|(names, _)| names.get(col_idx as usize)) {
        Some(name) => ffi::strdup(name),
        None => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_statement_column_logical_type(
    prepared_statement: duckdb_prepared_statement,
    col_idx: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes a live prepared statement.
    match unsafe { result_columns(prepared_statement) }.and_then(|(_, types)| types.get(col_idx as usize)) {
        Some(ty) => ffi::into_handle(ty.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_statement_column_type(
    prepared_statement: duckdb_prepared_statement,
    col_idx: idx_t,
) -> duckdb_type {
    // SAFETY: C passes a live prepared statement.
    match unsafe { result_columns(prepared_statement) }.and_then(|(_, types)| types.get(col_idx as usize)) {
        Some(ty) => crate::helper::type_id_to_c(ty.id()),
        None => DUCKDB_TYPE_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepared_statement_type(statement: duckdb_prepared_statement) -> duckdb_statement_type {
    // SAFETY: C passes a live prepared statement.
    match unsafe { ffi::get(statement) }.and_then(PreparedWrapper::prepared) {
        Some(statement) => statement_type_to_c(statement.properties().statement_type),
        None => DUCKDB_STATEMENT_TYPE_INVALID,
    }
}

/// `duckdb_bind_value` of `value`.
///
/// # Safety
///
/// As for [`prepared`].
unsafe fn bind(prepared_statement: duckdb_prepared_statement, param_idx: idx_t, value: Value) -> duckdb_state {
    // SAFETY: by the contract of this function.
    let Some(wrapper) = (unsafe { prepared(prepared_statement) }) else {
        return DuckDBError;
    };
    let Some(name) = wrapper.name(param_idx).map(str::to_string) else {
        wrapper.bind_error = Some(c_string(&format!(
            "Invalid Input Error: Can not bind to parameter number {param_idx}, statement only has {} parameter(s)",
            wrapper.names.len()
        )));
        return DuckDBError;
    };
    wrapper.values.insert(name, value);
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_value(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    val: duckdb_value,
) -> duckdb_state {
    // SAFETY: C passes a live value.
    let Some(value) = (unsafe { ffi::get(val) }).cloned() else {
        return DuckDBError;
    };
    // SAFETY: C passes a live prepared statement.
    unsafe { bind(prepared_statement, param_idx, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_parameter_index(
    prepared_statement: duckdb_prepared_statement,
    param_idx_out: *mut idx_t,
    name: *const c_char,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement and a NUL-terminated name.
    let (Some(wrapper), Some(name)) = (unsafe { prepared(prepared_statement) }, unsafe { ffi::c_str(name) }) else {
        return DuckDBError;
    };
    let name = String::from_utf8_lossy(name);
    let Some((&index, _)) = wrapper.names.iter().find(|(_, n)| n.eq_ignore_ascii_case(&name)) else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the index.
    if unsafe { ffi::write(param_idx_out, index) } {
        DuckDBSuccess
    } else {
        DuckDBError
    }
}

macro_rules! bind_scalar {
    ($name:ident, $t:ty, $make:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            prepared_statement: duckdb_prepared_statement,
            param_idx: idx_t,
            val: $t,
        ) -> duckdb_state {
            // SAFETY: C passes a live prepared statement.
            unsafe { bind(prepared_statement, param_idx, $make(val)) }
        }
    };
}

bind_scalar!(duckdb_bind_boolean, bool, Value::boolean);
bind_scalar!(duckdb_bind_int8, i8, Value::tinyint);
bind_scalar!(duckdb_bind_int16, i16, Value::smallint);
bind_scalar!(duckdb_bind_int32, i32, Value::integer);
bind_scalar!(duckdb_bind_int64, i64, Value::bigint);
bind_scalar!(duckdb_bind_uint8, u8, Value::utinyint);
bind_scalar!(duckdb_bind_uint16, u16, Value::usmallint);
bind_scalar!(duckdb_bind_uint32, u32, Value::uinteger);
bind_scalar!(duckdb_bind_uint64, u64, Value::ubigint);
bind_scalar!(duckdb_bind_float, f32, Value::float);
bind_scalar!(duckdb_bind_double, f64, Value::double);
bind_scalar!(duckdb_bind_hugeint, duckdb_hugeint, |v: duckdb_hugeint| Value::hugeint(
    v.to_i128()
));
bind_scalar!(duckdb_bind_uhugeint, duckdb_uhugeint, |v: duckdb_uhugeint| {
    Value::uhugeint(v.to_u128())
});
bind_scalar!(duckdb_bind_date, duckdb_date, |v: duckdb_date| Value::date(Date(
    v.days
)));
bind_scalar!(duckdb_bind_time, duckdb_time, |v: duckdb_time| Value::time(Time(
    v.micros
)));
bind_scalar!(duckdb_bind_timestamp, duckdb_timestamp, |v: duckdb_timestamp| {
    Value::timestamp(Timestamp(v.micros))
});
bind_scalar!(duckdb_bind_timestamp_tz, duckdb_timestamp, |v: duckdb_timestamp| {
    Value::timestamp_tz(Timestamp(v.micros))
});
bind_scalar!(duckdb_bind_interval, duckdb_interval, |v: duckdb_interval| {
    Value::interval(Interval {
        months: v.months,
        days: v.days,
        micros: v.micros,
    })
});

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_timestamp_tz_ns(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    val: duckdb_timestamp_ns,
) -> duckdb_state {
    let Ok(value) = Value::new(
        LogicalType::new(LogicalTypeId::TimestampTzNs),
        pintail_common::value::ValueData::Int64(val.nanos),
    ) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live prepared statement.
    unsafe { bind(prepared_statement, param_idx, value) }
}

/// A VARCHAR of `bytes`, which must be UTF-8 (`Value(string)` throws otherwise).
///
/// # Safety
///
/// As for [`prepared`].
unsafe fn bind_text(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    bytes: Option<&[u8]>,
) -> duckdb_state {
    match bytes.map(std::str::from_utf8) {
        // SAFETY: by the contract of this function.
        Some(Ok(text)) => unsafe { bind(prepared_statement, param_idx, Value::varchar(text)) },
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_varchar(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    val: *const c_char,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement and a NUL-terminated string.
    unsafe { bind_text(prepared_statement, param_idx, ffi::c_str(val)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_varchar_length(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    val: *const c_char,
    length: idx_t,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement and `length` bytes.
    unsafe { bind_text(prepared_statement, param_idx, ffi::bytes(val.cast(), length)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_decimal(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    val: duckdb_decimal,
) -> duckdb_state {
    // a DECIMAL that fits 64 bits is its `lower` half
    let unscaled = if val.width > 18 {
        val.value.to_i128()
    } else {
        i128::from(val.value.lower as i64)
    };
    let Ok(value) = Value::decimal(unscaled, val.width, val.scale) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live prepared statement.
    unsafe { bind(prepared_statement, param_idx, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_blob(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
    data: *const std::ffi::c_void,
    length: idx_t,
) -> duckdb_state {
    // SAFETY: C passes `length` bytes.
    let Some(bytes) = (unsafe { ffi::bytes(data, length) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live prepared statement.
    unsafe { bind(prepared_statement, param_idx, Value::blob(bytes)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_bind_null(
    prepared_statement: duckdb_prepared_statement,
    param_idx: idx_t,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement.
    unsafe { bind(prepared_statement, param_idx, Value::sql_null()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_prepared(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_result,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement.
    let Some(wrapper) = (unsafe { prepared(prepared_statement) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the result, or no place.
    wrapper.execute(unsafe { ffi::reference_mut(out_result) }, false)
}

/// pintail runs a statement to its end, so a stream is a result with all of its rows that C reads as a stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_execute_prepared_streaming(
    prepared_statement: duckdb_prepared_statement,
    out_result: *mut duckdb_result,
) -> duckdb_state {
    // SAFETY: C passes a live prepared statement.
    let Some(wrapper) = (unsafe { prepared(prepared_statement) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes where to write the result, or no place.
    let out = unsafe { ffi::reference_mut(out_result) };
    let streaming = out.is_some();
    wrapper.execute(out, streaming)
}

/// The statement behind a handle, for `pending.rs`.
///
/// # Safety
///
/// As for [`prepared`].
pub(crate) unsafe fn prepared_wrapper<'a>(
    prepared_statement: duckdb_prepared_statement,
) -> Option<&'a PreparedWrapper> {
    // SAFETY: by the contract of this function.
    unsafe { prepared(prepared_statement) }.map(|w| &*w)
}

impl PreparedWrapper {
    /// The connection the statement was prepared on (for the Arrow functions, which read its options).
    pub(crate) fn connection(&self) -> &SharedConnection {
        &self.connection
    }

    /// `GetParameterCount`.
    pub(crate) fn parameter_count(&self) -> usize {
        self.names.len()
    }

    /// Runs the statement with the values bound now, for a pending result: the result and the statement's
    /// properties.
    pub(crate) fn run(&self) -> Option<(pintail::Result<pintail::QueryResult>, pintail::StatementProperties)> {
        let statement = self.prepared()?;
        let result = ffi::guard(
            Err(pintail::Error::new(
                pintail::ErrorKind::Internal,
                "executing the statement panicked",
            )),
            || self.connection.lock().execute_named(statement, self.values.clone()),
        );
        Some((result, statement.properties()))
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_extract_statements(
    connection: duckdb_connection,
    query: *const c_char,
    out_extracted_statements: *mut duckdb_extracted_statements,
) -> idx_t {
    // SAFETY: C passes a live connection and a NUL-terminated query.
    let (Some(connection), Some(query)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(query) }) else {
        return 0;
    };
    if out_extracted_statements.is_null() {
        return 0;
    }
    let query = String::from_utf8_lossy(query);
    let extracted = ffi::guard(
        Err(pintail::Error::new(
            pintail::ErrorKind::Internal,
            "extracting the statements panicked",
        )),
        || connection.connection.lock().extract_statements(&query),
    );
    let wrapper = match extracted {
        Ok(statements) => ExtractedStatements {
            statements,
            error: None,
        },
        Err(error) => ExtractedStatements {
            statements: Vec::new(),
            error: Some(c_string(&error.to_string())),
        },
    };
    let count = wrapper.statements.len() as idx_t;
    // SAFETY: C passes where to write the statements; it is not null.
    unsafe { ffi::write(out_extracted_statements, ffi::into_handle(wrapper)) };
    count
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_prepare_extracted_statement(
    connection: duckdb_connection,
    extracted_statements: duckdb_extracted_statements,
    index: idx_t,
    out_prepared_statement: *mut duckdb_prepared_statement,
) -> duckdb_state {
    // SAFETY: C passes a live connection and extracted statements.
    let (Some(connection), Some(extracted)) = (unsafe { ffi::get(connection) }, unsafe {
        ffi::get(extracted_statements)
    }) else {
        return DuckDBError;
    };
    let Some(statement) = extracted.statements.get(index as usize) else {
        return DuckDBError;
    };
    if out_prepared_statement.is_null() {
        return DuckDBError;
    }
    let wrapper = prepare(&connection.connection, statement);
    // SAFETY: C passes where to write the statement; it is not null.
    unsafe { write_prepared(wrapper, out_prepared_statement) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_extract_statements_error(
    extracted_statements: duckdb_extracted_statements,
) -> *const c_char {
    // SAFETY: C passes live extracted statements.
    match unsafe { ffi::get(extracted_statements) }.and_then(|e| e.error.as_ref()) {
        Some(error) => error.as_ptr(),
        None => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_extracted(extracted_statements: *mut duckdb_extracted_statements) {
    // SAFETY: C passes extracted statements `duckdb_extract_statements` made.
    drop(unsafe { ffi::take(extracted_statements) });
}
