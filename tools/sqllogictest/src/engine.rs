//! The database interface the runner drives. One implementation wraps duckrs; the optional
//! `reference` feature adds one backed by the C++ DuckDB library.

use std::path::Path;

/// The result-column categories the comparison logic cares about (see `CompareValues` in
/// duckdb/test/sqlite/result_helper.cpp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Boolean,
    /// TINYINT .. UHUGEINT
    Integer,
    Float,
    Double,
    Decimal {
        scale: u8,
    },
    Other,
}

impl ColumnType {
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Float | Self::Double | Self::Decimal { .. })
    }
}

/// A materialized result. Each cell holds the value cast to VARCHAR, or `None` for NULL.
#[derive(Debug, Clone, Default)]
pub struct QueryOutput {
    pub names: Vec<String>,
    pub types: Vec<ColumnType>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl QueryOutput {
    pub fn column_count(&self) -> usize {
        self.types.len()
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

pub trait Engine: Send + Sync {
    fn name(&self) -> &str;

    /// Opens a database; `None` means in-memory.
    fn open(&self, path: Option<&Path>, read_only: bool) -> Result<Box<dyn Database>, String>;
}

pub trait Database: Send + Sync {
    fn connect(&self) -> Result<Box<dyn Connection>, String>;
}

pub trait Connection: Send {
    /// Runs one or more `;`-separated statements and returns the last result. The error string
    /// must be rendered like DuckDB's `ErrorData::Message()`, e.g. `Binder Error: ...`.
    fn query(&mut self, sql: &str) -> Result<QueryOutput, String>;

    /// Handles `require <extension>`; returns whether the extension is available.
    fn load_extension(&mut self, name: &str) -> bool {
        self.query(&format!("LOAD {name}")).is_ok()
    }
}

/// The engine under development.
pub struct DuckrsEngine;

impl Engine for DuckrsEngine {
    fn name(&self) -> &str {
        "duckrs"
    }

    fn open(&self, path: Option<&Path>, read_only: bool) -> Result<Box<dyn Database>, String> {
        let db = match path {
            Some(path) => duckrs::Database::open(path, read_only),
            None => duckrs::Database::open_in_memory(),
        };
        Ok(Box::new(DuckrsDatabase(db.map_err(|e| e.to_string())?)))
    }
}

struct DuckrsDatabase(duckrs::Database);

impl Database for DuckrsDatabase {
    fn connect(&self) -> Result<Box<dyn Connection>, String> {
        Ok(Box::new(DuckrsConnection(self.0.connect().map_err(|e| e.to_string())?)))
    }
}

struct DuckrsConnection(duckrs::Connection);

impl Connection for DuckrsConnection {
    fn query(&mut self, sql: &str) -> Result<QueryOutput, String> {
        let result = self.0.query(sql).map_err(|e| e.to_string())?;
        let types = result.types.iter().map(column_type).collect();
        let mut rows = Vec::with_capacity(result.row_count());
        for row in 0..result.row_count() {
            let mut cells = Vec::with_capacity(result.column_count());
            for column in 0..result.column_count() {
                let value = result.value(column, row);
                cells.push(if value.is_null() {
                    None
                } else {
                    Some(value.try_to_string().map_err(|e| e.to_string())?)
                });
            }
            rows.push(cells);
        }
        Ok(QueryOutput {
            names: result.names,
            types,
            rows,
        })
    }

    fn load_extension(&mut self, _name: &str) -> bool {
        false
    }
}

/// The comparison category of a result column (as the reference engine maps C API types).
fn column_type(ty: &duckrs::LogicalType) -> ColumnType {
    use duckrs::LogicalTypeId::*;
    match ty.id() {
        Boolean => ColumnType::Boolean,
        TinyInt | SmallInt | Integer | BigInt | HugeInt | UTinyInt | USmallInt | UInteger | UBigInt | UHugeInt => {
            ColumnType::Integer
        }
        Float => ColumnType::Float,
        Double => ColumnType::Double,
        Decimal => ColumnType::Decimal {
            scale: ty.decimal_properties().map_or(0, |(_, scale)| scale),
        },
        _ => ColumnType::Other,
    }
}

#[cfg(feature = "reference")]
pub use reference::ReferenceEngine;

#[cfg(feature = "reference")]
mod reference {
    //! The C++ DuckDB engine, linked through its C API.

    use super::{ColumnType, Connection, Database, Engine, QueryOutput};
    use libduckdb_sys as ffi;
    use std::ffi::{CStr, CString};
    use std::path::Path;
    use std::ptr;

    pub struct ReferenceEngine;

    impl ReferenceEngine {
        pub fn version() -> String {
            // SAFETY: duckdb_library_version returns a static NUL-terminated string.
            unsafe { CStr::from_ptr(ffi::duckdb_library_version()) }
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Engine for ReferenceEngine {
        fn name(&self) -> &str {
            "reference"
        }

        fn open(&self, path: Option<&Path>, read_only: bool) -> Result<Box<dyn Database>, String> {
            let path_c = match path {
                Some(p) => Some(CString::new(p.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?),
                None => None,
            };
            let mut config: ffi::duckdb_config = ptr::null_mut();
            let mut db: ffi::duckdb_database = ptr::null_mut();
            let mut error: *mut std::os::raw::c_char = ptr::null_mut();
            // SAFETY: every out-pointer is valid; config is destroyed before returning and the
            // error string is freed with duckdb_free as the C API requires.
            unsafe {
                if ffi::duckdb_create_config(&mut config) != ffi::DuckDBSuccess {
                    return Err("failed to create config".into());
                }
                let set = |name: &str, value: &str| {
                    let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
                    ffi::duckdb_set_config(config, n.as_ptr(), v.as_ptr());
                };
                set("access_mode", if read_only { "READ_ONLY" } else { "AUTOMATIC" });
                // Match the C++ unittest defaults: extensions are linked statically, never downloaded.
                set("autoinstall_known_extensions", "false");
                set("autoload_known_extensions", "true");
                let path_ptr = path_c.as_ref().map_or(ptr::null(), |p| p.as_ptr());
                let state = ffi::duckdb_open_ext(path_ptr, &mut db, config, &mut error);
                ffi::duckdb_destroy_config(&mut config);
                if state != ffi::DuckDBSuccess {
                    let message = if error.is_null() {
                        "failed to open database".to_string()
                    } else {
                        let m = CStr::from_ptr(error).to_string_lossy().into_owned();
                        ffi::duckdb_free(error.cast());
                        m
                    };
                    return Err(message);
                }
            }
            Ok(Box::new(ReferenceDatabase(db)))
        }
    }

    struct ReferenceDatabase(ffi::duckdb_database);

    // SAFETY: a DuckDB database handle may be used from any thread; connections are created
    // per thread and the database itself is internally synchronized.
    unsafe impl Send for ReferenceDatabase {}
    // SAFETY: see above.
    unsafe impl Sync for ReferenceDatabase {}

    impl Drop for ReferenceDatabase {
        fn drop(&mut self) {
            // SAFETY: the handle came from duckdb_open_ext and all connections borrow nothing
            // from it at the Rust level, but DuckDB keeps the instance alive until they close.
            unsafe { ffi::duckdb_close(&mut self.0) };
        }
    }

    impl Database for ReferenceDatabase {
        fn connect(&self) -> Result<Box<dyn Connection>, String> {
            let mut con: ffi::duckdb_connection = ptr::null_mut();
            // SAFETY: self.0 is an open database and con is a valid out-pointer.
            if unsafe { ffi::duckdb_connect(self.0, &mut con) } != ffi::DuckDBSuccess {
                return Err("failed to connect".into());
            }
            Ok(Box::new(ReferenceConnection(con)))
        }
    }

    struct ReferenceConnection(ffi::duckdb_connection);

    // SAFETY: a connection is only used by one thread at a time (it is owned, not shared).
    unsafe impl Send for ReferenceConnection {}

    impl Drop for ReferenceConnection {
        fn drop(&mut self) {
            // SAFETY: the handle came from duckdb_connect.
            unsafe { ffi::duckdb_disconnect(&mut self.0) };
        }
    }

    fn column_type(result: &mut ffi::duckdb_result, col: u64) -> ColumnType {
        // SAFETY: result is a successful materialized result and col is in range.
        unsafe {
            match ffi::duckdb_column_type(result, col) {
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => ColumnType::Boolean,
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT
                | ffi::DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT => ColumnType::Integer,
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT => ColumnType::Float,
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE => ColumnType::Double,
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => {
                    let mut logical = ffi::duckdb_column_logical_type(result, col);
                    let scale = ffi::duckdb_decimal_scale(logical);
                    ffi::duckdb_destroy_logical_type(&mut logical);
                    ColumnType::Decimal { scale }
                }
                _ => ColumnType::Other,
            }
        }
    }

    /// A raw result: the output, whether some non-NULL value had no C API string form (nested
    /// types such as STRUCT and LIST), and whether the statement was a SELECT.
    struct RawResult {
        output: QueryOutput,
        unrenderable: bool,
        is_select: bool,
    }

    impl ReferenceConnection {
        fn run(&mut self, sql: &str) -> Result<RawResult, String> {
            let sql = CString::new(sql).map_err(|e| e.to_string())?;
            // SAFETY: duckdb_result is a plain C struct that duckdb_query fully initializes.
            let mut result: ffi::duckdb_result = unsafe { std::mem::zeroed() };
            // SAFETY: the connection is open, sql is NUL-terminated, and result is destroyed on
            // every path below. Strings from duckdb_value_string are freed with duckdb_free.
            unsafe {
                let state = ffi::duckdb_query(self.0, sql.as_ptr(), &mut result);
                if state != ffi::DuckDBSuccess {
                    let error = ffi::duckdb_result_error(&mut result);
                    let message = if error.is_null() {
                        "unknown error".to_string()
                    } else {
                        CStr::from_ptr(error).to_string_lossy().into_owned()
                    };
                    ffi::duckdb_destroy_result(&mut result);
                    return Err(message);
                }
                let is_select = ffi::duckdb_result_statement_type(result)
                    == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT;
                let columns = ffi::duckdb_column_count(&mut result);
                let rows = ffi::duckdb_row_count(&mut result);
                let mut output = QueryOutput::default();
                let mut unrenderable = false;
                for col in 0..columns {
                    let name = ffi::duckdb_column_name(&mut result, col);
                    output.names.push(CStr::from_ptr(name).to_string_lossy().into_owned());
                    output.types.push(column_type(&mut result, col));
                }
                for row in 0..rows {
                    let mut cells = Vec::with_capacity(columns as usize);
                    for col in 0..columns {
                        if ffi::duckdb_value_is_null(&mut result, col, row) {
                            cells.push(None);
                            continue;
                        }
                        // duckdb_value_string keeps embedded NUL bytes, unlike duckdb_value_varchar.
                        let s = ffi::duckdb_value_string(&mut result, col, row);
                        if s.data.is_null() {
                            unrenderable = true;
                            cells.push(None);
                            continue;
                        }
                        let bytes = std::slice::from_raw_parts(s.data.cast::<u8>(), s.size as usize);
                        cells.push(Some(String::from_utf8_lossy(bytes).into_owned()));
                        ffi::duckdb_free(s.data.cast());
                    }
                    output.rows.push(cells);
                }
                ffi::duckdb_destroy_result(&mut result);
                Ok(RawResult {
                    output,
                    unrenderable,
                    is_select,
                })
            }
        }
    }

    impl Connection for ReferenceConnection {
        fn query(&mut self, sql: &str) -> Result<QueryOutput, String> {
            let raw = self.run(sql)?;
            let mut output = raw.output;
            if raw.unrenderable && raw.is_select {
                // The C++ runner renders values with a VARCHAR cast, so re-run the SELECT with
                // every column cast to VARCHAR and take the strings from that run.
                let inner = sql.trim().trim_end_matches(';');
                let wrapped = format!("SELECT COLUMNS(*)::VARCHAR FROM ({inner}\n)");
                if let Ok(cast) = self.run(&wrapped)
                    && cast.output.row_count() == output.row_count()
                    && cast.output.column_count() == output.column_count()
                {
                    output.rows = cast.output.rows;
                }
            }
            Ok(output)
        }
    }
}
