// Ported from duckdb/src/main/capi/v1/appender-c.cpp and duckdb/src/main/appender.cpp @ rust-port-baseline

//! `duckdb_appender` (`AppenderWrapper` around a table `Appender`): rows go into a chunk of the table's types a
//! value at a time, full chunks into a collection, and the collection into the table on a flush.

use crate::database::SharedConnection;
use crate::error::ErrorData;
use crate::ffi::{self, Handle};
use crate::types::*;
use duckrs::{DataChunk, Error, ErrorKind, LogicalType, LogicalTypeId, Value};
use duckrs_common::types::{Date, Interval, NativeType, Time, Timestamp};
use std::ffi::c_char;

/// `BaseAppender::flush_count`: the rows it collects before it flushes them.
const FLUSH_COUNT: usize = 204_800;

/// A table `Appender`.
struct Appender {
    connection: SharedConnection,
    schema: Option<String>,
    table: String,
    /// the columns of the table, and what `AppendDefault` gives for each
    columns: Vec<(String, LogicalType)>,
    defaults: Vec<Option<Value>>,
    /// `column_ids`: the columns `duckdb_appender_add_column` chose, all of them when empty
    active: Vec<usize>,
    /// the rows of the row being appended to, and of the rows before it that are not collected yet
    chunk: DataChunk,
    /// the column the next value goes to
    column: usize,
    collection: Vec<DataChunk>,
    collected_rows: usize,
}

/// `AppenderWrapper`: an appender, or none when making it failed, and the last error.
pub struct AppenderWrapper {
    appender: Option<Appender>,
    error: ErrorData,
    flush_failed: bool,
}

impl Handle for _duckdb_appender {
    type Target = AppenderWrapper;
}

fn invalid_input(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

impl Appender {
    fn new(connection: SharedConnection, schema: Option<String>, table: String) -> duckrs::Result<Self> {
        let (columns, defaults) = {
            let connection = connection.lock();
            let columns = connection.table_columns(schema.as_deref(), &table)?;
            let defaults = connection
                .table_column_defaults(schema.as_deref(), &table)
                .unwrap_or_else(|_| vec![None; columns.len()]);
            (columns, defaults)
        };
        let types: Vec<LogicalType> = columns.iter().map(|(_, ty)| ty.clone()).collect();
        Ok(Self {
            connection,
            schema,
            table,
            columns,
            defaults,
            active: Vec::new(),
            chunk: DataChunk::new(&types),
            column: 0,
            collection: Vec::new(),
            collected_rows: 0,
        })
    }

    /// `GetActiveTypes`.
    fn types(&self) -> Vec<LogicalType> {
        self.active_columns().into_iter().map(|i| self.columns[i].1.clone()).collect()
    }

    fn active_columns(&self) -> Vec<usize> {
        if self.active.is_empty() {
            (0..self.columns.len()).collect()
        } else {
            self.active.clone()
        }
    }

    /// `InitializeChunk`, with an empty collection.
    fn initialize_chunk(&mut self) {
        self.chunk = DataChunk::new(&self.types());
        self.collection.clear();
        self.collected_rows = 0;
    }

    fn check_column(&self) -> duckrs::Result<()> {
        if self.column >= self.chunk.column_count() {
            return Err(invalid_input("Too many appends for chunk!"));
        }
        Ok(())
    }

    /// `AppendValue`: the next value of the row, cast to its column's type (`Vector::SetValue`).
    fn append_value(&mut self, value: &Value) -> duckrs::Result<()> {
        self.check_column()?;
        let row = self.chunk.len();
        let vector = self.chunk.column_mut(self.column);
        if value.is_null() {
            vector.set_null(row, true);
        } else if value.logical_type() == vector.logical_type() {
            vector.set_value(row, value)?;
        } else {
            let cast = duckrs::cast_value(value, vector.logical_type())?;
            vector.set_value(row, &cast)?;
        }
        self.column += 1;
        Ok(())
    }

    /// `AppendValueInternal<T>`: a value of a C type, written as it is into a column of the type it is, and cast
    /// otherwise.
    fn append_native<T: NativeType>(&mut self, input: T, id: LogicalTypeId, value: impl FnOnce() -> Value) -> duckrs::Result<()> {
        self.check_column()?;
        let row = self.chunk.len();
        let vector = self.chunk.column_mut(self.column);
        if vector.logical_type().id() != id || vector.logical_type().has_params() {
            return self.append_value(&value());
        }
        vector.data_mut::<T>()[row] = input;
        vector.set_null(row, false);
        self.column += 1;
        Ok(())
    }

    fn append_null(&mut self) -> duckrs::Result<()> {
        self.check_column()?;
        let row = self.chunk.len();
        self.chunk.column_mut(self.column).set_null(row, true);
        self.column += 1;
        Ok(())
    }

    /// `GetDefaultValue`.
    fn default_value(&self, column: usize) -> duckrs::Result<Value> {
        let index = if self.active.is_empty() {
            column
        } else {
            *self
                .active
                .get(column)
                .ok_or_else(|| invalid_input("Column index out of bounds"))?
        };
        match self.defaults.get(index) {
            Some(Some(value)) => Ok(value.clone()),
            _ => Err(Error::new(
                ErrorKind::NotImplemented,
                format!(
                    "AppendDefault is not supported for column {}: not a foldable default expressions.",
                    self.columns.get(index).map_or("", |(name, _)| name.as_str())
                ),
            )),
        }
    }

    fn append_default(&mut self) -> duckrs::Result<()> {
        self.check_column()?;
        let value = self.default_value(self.column)?;
        self.append_value(&value)
    }

    /// `AppendDefault(chunk, col, row)`: the default of column `col` into `chunk`.
    fn append_default_to(&self, chunk: &mut DataChunk, col: usize, row: usize) -> duckrs::Result<()> {
        if col >= chunk.column_count() {
            return Err(invalid_input("Too many appends for chunk!"));
        }
        let value = self.default_value(col)?;
        let target = chunk.column(col).logical_type().clone();
        let value = if value.logical_type() == &target || value.is_null() {
            value
        } else {
            duckrs::cast_value(&value, &target).map_err(|_| {
                invalid_input(format!(
                    "type mismatch in Append, expected {target}, got {}, for column {col}",
                    value.logical_type()
                ))
            })?
        };
        if row >= chunk.column(col).capacity() {
            return Err(invalid_input("row index out of bounds"));
        }
        if value.is_null() {
            chunk.column_mut(col).set_null(row, true);
            Ok(())
        } else {
            chunk.column_mut(col).set_value(row, &value)
        }
    }

    fn end_row(&mut self) -> duckrs::Result<()> {
        if self.column != self.chunk.column_count() {
            return Err(invalid_input("Call to EndRow before all columns have been appended to!"));
        }
        self.column = 0;
        let count = self.chunk.len() + 1;
        self.chunk.set_len(count);
        if count >= duckrs_common::STANDARD_VECTOR_SIZE {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// `FlushChunk`: the rows of the chunk into the collection.
    fn flush_chunk(&mut self) -> duckrs::Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let fresh = DataChunk::new(&self.types());
        let chunk = std::mem::replace(&mut self.chunk, fresh);
        self.collected_rows += chunk.len();
        self.collection.push(chunk);
        if self.collected_rows >= FLUSH_COUNT {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> duckrs::Result<()> {
        if self.column != 0 {
            return Err(invalid_input("Failed to Flush appender: incomplete append to row!"));
        }
        self.flush_chunk()?;
        if self.collection.is_empty() {
            return Ok(());
        }
        let chunks = std::mem::take(&mut self.collection);
        self.collected_rows = 0;
        let columns: Vec<(String, LogicalType)> =
            self.active_columns().into_iter().map(|i| self.columns[i].clone()).collect();
        let mut connection = self.connection.lock();
        connection.append_chunks(self.schema.as_deref(), &self.table, columns, chunks)
    }

    fn close(&mut self) -> duckrs::Result<()> {
        if self.column == 0 || self.column == self.chunk.column_count() {
            self.flush()?;
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.chunk = DataChunk::new(&self.types());
        self.collection.clear();
        self.collected_rows = 0;
        self.column = 0;
    }

    /// `AppendDataChunk`: the rows of `chunk`, cast to the appender's types where they differ.
    fn append_data_chunk(&mut self, chunk: &DataChunk) -> duckrs::Result<()> {
        let types = self.types();
        let mut chunk = chunk.clone();
        crate::vector::prepare_chunk(&mut chunk);
        if chunk.types() != types {
            if chunk.column_count() != types.len() {
                return Err(invalid_input(format!(
                    "incorrect column count in AppendDataChunk, expected {}, got {}",
                    types.len(),
                    chunk.column_count()
                )));
            }
            let count = chunk.len();
            let mut columns = Vec::with_capacity(types.len());
            for (i, ty) in types.iter().enumerate() {
                let column = chunk.column(i);
                if column.logical_type() == ty {
                    columns.push(column.clone());
                    continue;
                }
                let cast = duckrs::cast_vector(column, ty, count).map_err(|_| {
                    invalid_input(format!(
                        "type mismatch in AppendDataChunk, expected {ty}, got {} for column {i}",
                        column.logical_type()
                    ))
                })?;
                columns.push(cast);
            }
            chunk = DataChunk::from_columns(columns, count);
        }
        if chunk.is_empty() {
            return Ok(());
        }
        self.collected_rows += chunk.len();
        self.collection.push(chunk);
        if self.collected_rows >= FLUSH_COUNT {
            self.flush()?;
        }
        Ok(())
    }

    /// `Appender::AddColumn`.
    fn add_column(&mut self, name: &str) -> duckrs::Result<()> {
        self.flush()?;
        let Some(index) = self.columns.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            return Err(invalid_input("the column must exist in the table"));
        };
        if self.active.contains(&index) {
            return Err(invalid_input("cannot add the same column twice"));
        }
        self.active.push(index);
        self.initialize_chunk();
        Ok(())
    }

    fn clear_columns(&mut self) -> duckrs::Result<()> {
        self.flush()?;
        self.active.clear();
        self.initialize_chunk();
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_create(
    connection: duckdb_connection,
    schema: *const c_char,
    table: *const c_char,
    out_appender: *mut duckdb_appender,
) -> duckdb_state {
    // SAFETY: C passes what `duckdb_appender_create_ext` takes.
    unsafe { duckdb_appender_create_ext(connection, std::ptr::null(), schema, table, out_appender) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_create_ext(
    connection: duckdb_connection,
    _catalog: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    out_appender: *mut duckdb_appender,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a NUL-terminated table name.
    let (Some(connection), Some(table)) = (unsafe { ffi::get(connection) }, unsafe { ffi::c_str(table) }) else {
        return DuckDBError;
    };
    if out_appender.is_null() {
        return DuckDBError;
    }
    // SAFETY: C passes a NUL-terminated schema name, or none for the default one.
    let schema = unsafe { ffi::c_str(schema) }.map(|s| String::from_utf8_lossy(s).into_owned());
    let table = String::from_utf8_lossy(table).into_owned();
    let created = ffi::guard(
        Err(Error::new(ErrorKind::Internal, "Unknown create appender error")),
        || Appender::new(connection.connection.clone(), schema, table),
    );
    let (wrapper, state) = match created {
        Ok(appender) => (
            AppenderWrapper {
                appender: Some(appender),
                error: ErrorData::default(),
                flush_failed: false,
            },
            DuckDBSuccess,
        ),
        Err(error) => (
            AppenderWrapper {
                appender: None,
                error: ErrorData::from_error(&error),
                flush_failed: false,
            },
            DuckDBError,
        ),
    };
    // SAFETY: C passes where to write the appender; it is not null.
    unsafe { ffi::write(out_appender, ffi::into_handle(wrapper)) };
    state
}

/// `duckdb_appender_run_function`: `function` on the appender, its error kept for `duckdb_appender_error`.
///
/// # Safety
///
/// `appender` is null or a live appender that nothing else uses now.
unsafe fn run(appender: duckdb_appender, function: impl FnOnce(&mut Appender) -> duckrs::Result<()>) -> duckdb_state {
    // SAFETY: by the contract of this function.
    let Some(wrapper) = (unsafe { ffi::get_mut(appender) }) else {
        return DuckDBError;
    };
    let Some(inner) = wrapper.appender.as_mut() else {
        wrapper.error = ErrorData::new(ErrorKind::Invalid, "not a valid appender");
        return DuckDBError;
    };
    let outcome = ffi::guard(Err(Error::new(ErrorKind::Internal, "Unknown appender error")), || function(inner));
    match outcome {
        Ok(()) => DuckDBSuccess,
        Err(error) => {
            wrapper.error = ErrorData::from_error(&error);
            DuckDBError
        }
    }
}

/// [`run`], marking the appender as failed to flush when it fails (`duckdb_appender_flush`, `_close`).
///
/// # Safety
///
/// As for [`run`].
unsafe fn run_flush(appender: duckdb_appender, function: impl FnOnce(&mut Appender) -> duckrs::Result<()>) -> duckdb_state {
    // SAFETY: by the contract of this function.
    let state = unsafe { run(appender, function) };
    if state == DuckDBError {
        // SAFETY: by the contract of this function.
        if let Some(wrapper) = unsafe { ffi::get_mut(appender) } {
            wrapper.flush_failed = true;
        }
    }
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_destroy(appender: *mut duckdb_appender) -> duckdb_state {
    // SAFETY: C passes where the appender is.
    let Some(handle) = (unsafe { ffi::reference_mut(appender) }).map(|h| *h) else {
        return DuckDBError;
    };
    if handle.is_null() {
        return DuckDBError;
    }
    // SAFETY: the handle is a live appender.
    let mut state = unsafe { duckdb_appender_close(handle) };
    // SAFETY: C passes the appender `duckdb_appender_create` made, which it gives up here.
    if let Some(wrapper) = unsafe { ffi::take(appender) }
        && wrapper.flush_failed
    {
        state = DuckDBError;
    }
    state
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_add_column(appender: duckdb_appender, name: *const c_char) -> duckdb_state {
    // SAFETY: C passes a NUL-terminated name.
    let name = unsafe { ffi::c_str(name) }.map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default();
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.add_column(&name)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_clear_columns(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run(appender, Appender::clear_columns) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_error(appender: duckdb_appender) -> *const c_char {
    // SAFETY: C passes a live appender.
    unsafe { ffi::get(appender) }.map_or(std::ptr::null(), |wrapper| wrapper.error.raw_message())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_error_data(appender: duckdb_appender) -> duckdb_error_data {
    // SAFETY: C passes a live appender.
    let error = unsafe { ffi::get(appender) }.map(|wrapper| wrapper.error.clone()).unwrap_or_default();
    ffi::into_handle(error)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_appender_begin_row(_appender: duckdb_appender) -> duckdb_state {
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_end_row(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run(appender, Appender::end_row) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_default(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run(appender, Appender::append_default) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_default_to_chunk(
    appender: duckdb_appender,
    chunk: duckdb_data_chunk,
    col: idx_t,
    row: idx_t,
) -> duckdb_state {
    // SAFETY: C passes a live chunk, which is not the appender.
    let Some(chunk) = (unsafe { ffi::get_mut(chunk) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_default_to(chunk, col as usize, row as usize)) }
}

macro_rules! append_native {
    ($name:ident, $t:ty, $id:ident, $make:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(appender: duckdb_appender, value: $t) -> duckdb_state {
            // SAFETY: C passes a live appender.
            unsafe { run(appender, |a| a.append_native(value, LogicalTypeId::$id, || $make(value))) }
        }
    };
}

append_native!(duckdb_append_bool, bool, Boolean, Value::boolean);
append_native!(duckdb_append_int8, i8, TinyInt, Value::tinyint);
append_native!(duckdb_append_int16, i16, SmallInt, Value::smallint);
append_native!(duckdb_append_int32, i32, Integer, Value::integer);
append_native!(duckdb_append_int64, i64, BigInt, Value::bigint);
append_native!(duckdb_append_uint8, u8, UTinyInt, Value::utinyint);
append_native!(duckdb_append_uint16, u16, USmallInt, Value::usmallint);
append_native!(duckdb_append_uint32, u32, UInteger, Value::uinteger);
append_native!(duckdb_append_uint64, u64, UBigInt, Value::ubigint);
append_native!(duckdb_append_float, f32, Float, Value::float);
append_native!(duckdb_append_double, f64, Double, Value::double);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_hugeint(appender: duckdb_appender, value: duckdb_hugeint) -> duckdb_state {
    let value = value.to_i128();
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_native(value, LogicalTypeId::HugeInt, || Value::hugeint(value))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_uhugeint(appender: duckdb_appender, value: duckdb_uhugeint) -> duckdb_state {
    let value = value.to_u128();
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_native(value, LogicalTypeId::UHugeInt, || Value::uhugeint(value))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_date(appender: duckdb_appender, value: duckdb_date) -> duckdb_state {
    let date = Date(value.days);
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_native(date, LogicalTypeId::Date, || Value::date(date))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_time(appender: duckdb_appender, value: duckdb_time) -> duckdb_state {
    let time = Time(value.micros);
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_native(time, LogicalTypeId::Time, || Value::time(time))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_timestamp(appender: duckdb_appender, value: duckdb_timestamp) -> duckdb_state {
    let timestamp = Timestamp(value.micros);
    // SAFETY: C passes a live appender.
    unsafe {
        run(appender, |a| {
            a.append_native(timestamp, LogicalTypeId::Timestamp, || Value::timestamp(timestamp))
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_interval(appender: duckdb_appender, value: duckdb_interval) -> duckdb_state {
    let interval = Interval {
        months: value.months,
        days: value.days,
        micros: value.micros,
    };
    // SAFETY: C passes a live appender.
    unsafe {
        run(appender, |a| {
            a.append_native(interval, LogicalTypeId::Interval, || Value::interval(interval))
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_null(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run(appender, Appender::append_null) }
}

/// A VARCHAR of `bytes`, appended as `Append(string_t)` does: cast to the column's type.
///
/// # Safety
///
/// As for [`run`].
unsafe fn append_text(appender: duckdb_appender, bytes: Option<&[u8]>) -> duckdb_state {
    let Some(bytes) = bytes else {
        return DuckDBError;
    };
    let text = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: by the contract of this function.
    unsafe { run(appender, |a| a.append_value(&Value::varchar(text))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_varchar(appender: duckdb_appender, value: *const c_char) -> duckdb_state {
    // SAFETY: C passes a live appender and a NUL-terminated string.
    unsafe { append_text(appender, ffi::c_str(value)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_varchar_length(
    appender: duckdb_appender,
    value: *const c_char,
    length: idx_t,
) -> duckdb_state {
    // SAFETY: C passes a live appender and `length` bytes.
    unsafe { append_text(appender, ffi::bytes(value.cast(), length)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_blob(
    appender: duckdb_appender,
    data: *const std::ffi::c_void,
    length: idx_t,
) -> duckdb_state {
    // SAFETY: C passes `length` bytes.
    let Some(bytes) = (unsafe { ffi::bytes(data, length) }) else {
        return DuckDBError;
    };
    let value = Value::blob(bytes);
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_value(&value)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_value(appender: duckdb_appender, value: duckdb_value) -> duckdb_state {
    // SAFETY: C passes a live value.
    let Some(value) = (unsafe { ffi::get(value) }).cloned() else {
        return DuckDBError;
    };
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_value(&value)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_flush(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run_flush(appender, Appender::flush) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_clear(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe {
        run(appender, |a| {
            a.clear();
            Ok(())
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_close(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: C passes a live appender.
    unsafe { run_flush(appender, Appender::close) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_column_count(appender: duckdb_appender) -> idx_t {
    // SAFETY: C passes a live appender.
    unsafe { ffi::get(appender) }
        .and_then(|wrapper| wrapper.appender.as_ref())
        .map_or(0, |a| a.chunk.column_count() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_column_type(appender: duckdb_appender, col_idx: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes a live appender.
    let Some(appender) = (unsafe { ffi::get(appender) }).and_then(|wrapper| wrapper.appender.as_ref()) else {
        return std::ptr::null_mut();
    };
    match appender.types().get(col_idx as usize) {
        Some(ty) => ffi::into_handle(ty.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_data_chunk(appender: duckdb_appender, chunk: duckdb_data_chunk) -> duckdb_state {
    // SAFETY: C passes a live chunk.
    let Some(chunk) = (unsafe { ffi::get(chunk) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live appender.
    unsafe { run(appender, |a| a.append_data_chunk(chunk)) }
}
