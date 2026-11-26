// Ported from duckdb/src/include/duckdb.h @ rust-port-baseline

//! The types of `duckdb.h`, laid out as the header lays them out. `tests/layout.rs` checks the sizes and field
//! offsets against the header itself.

#![allow(non_camel_case_types, non_upper_case_globals)]

use std::ffi::{c_char, c_void};

pub type idx_t = u64;
pub type sel_t = u32;

/// C enums are passed as `int`-sized values; a Rust enum would make any value C passes outside it undefined
/// behaviour, so each enum is a `u32` with its values as constants.
pub type duckdb_state = u32;
pub const DuckDBSuccess: duckdb_state = 0;
pub const DuckDBError: duckdb_state = 1;

pub type duckdb_type = u32;
pub const DUCKDB_TYPE_INVALID: duckdb_type = 0;
pub const DUCKDB_TYPE_BOOLEAN: duckdb_type = 1;
pub const DUCKDB_TYPE_TINYINT: duckdb_type = 2;
pub const DUCKDB_TYPE_SMALLINT: duckdb_type = 3;
pub const DUCKDB_TYPE_INTEGER: duckdb_type = 4;
pub const DUCKDB_TYPE_BIGINT: duckdb_type = 5;
pub const DUCKDB_TYPE_UTINYINT: duckdb_type = 6;
pub const DUCKDB_TYPE_USMALLINT: duckdb_type = 7;
pub const DUCKDB_TYPE_UINTEGER: duckdb_type = 8;
pub const DUCKDB_TYPE_UBIGINT: duckdb_type = 9;
pub const DUCKDB_TYPE_FLOAT: duckdb_type = 10;
pub const DUCKDB_TYPE_DOUBLE: duckdb_type = 11;
pub const DUCKDB_TYPE_TIMESTAMP: duckdb_type = 12;
pub const DUCKDB_TYPE_DATE: duckdb_type = 13;
pub const DUCKDB_TYPE_TIME: duckdb_type = 14;
pub const DUCKDB_TYPE_INTERVAL: duckdb_type = 15;
pub const DUCKDB_TYPE_HUGEINT: duckdb_type = 16;
pub const DUCKDB_TYPE_VARCHAR: duckdb_type = 17;
pub const DUCKDB_TYPE_BLOB: duckdb_type = 18;
pub const DUCKDB_TYPE_DECIMAL: duckdb_type = 19;
pub const DUCKDB_TYPE_TIMESTAMP_S: duckdb_type = 20;
pub const DUCKDB_TYPE_TIMESTAMP_MS: duckdb_type = 21;
pub const DUCKDB_TYPE_TIMESTAMP_NS: duckdb_type = 22;
pub const DUCKDB_TYPE_ENUM: duckdb_type = 23;
pub const DUCKDB_TYPE_LIST: duckdb_type = 24;
pub const DUCKDB_TYPE_STRUCT: duckdb_type = 25;
pub const DUCKDB_TYPE_MAP: duckdb_type = 26;
pub const DUCKDB_TYPE_UUID: duckdb_type = 27;
pub const DUCKDB_TYPE_UNION: duckdb_type = 28;
pub const DUCKDB_TYPE_BIT: duckdb_type = 29;
pub const DUCKDB_TYPE_TIME_TZ: duckdb_type = 30;
pub const DUCKDB_TYPE_TIMESTAMP_TZ: duckdb_type = 31;
pub const DUCKDB_TYPE_UHUGEINT: duckdb_type = 32;
pub const DUCKDB_TYPE_ARRAY: duckdb_type = 33;
pub const DUCKDB_TYPE_ANY: duckdb_type = 34;
pub const DUCKDB_TYPE_BIGNUM: duckdb_type = 35;
pub const DUCKDB_TYPE_SQLNULL: duckdb_type = 36;
pub const DUCKDB_TYPE_STRING_LITERAL: duckdb_type = 37;
pub const DUCKDB_TYPE_INTEGER_LITERAL: duckdb_type = 38;
pub const DUCKDB_TYPE_TIME_NS: duckdb_type = 39;
pub const DUCKDB_TYPE_GEOMETRY: duckdb_type = 40;
pub const DUCKDB_TYPE_VARIANT: duckdb_type = 41;
pub const DUCKDB_TYPE_TIMESTAMP_TZ_NS: duckdb_type = 42;

pub type duckdb_pending_state = u32;
pub const DUCKDB_PENDING_RESULT_READY: duckdb_pending_state = 0;
pub const DUCKDB_PENDING_RESULT_NOT_READY: duckdb_pending_state = 1;
pub const DUCKDB_PENDING_ERROR: duckdb_pending_state = 2;
pub const DUCKDB_PENDING_NO_TASKS_AVAILABLE: duckdb_pending_state = 3;

pub type duckdb_result_type = u32;
pub const DUCKDB_RESULT_TYPE_INVALID: duckdb_result_type = 0;
pub const DUCKDB_RESULT_TYPE_CHANGED_ROWS: duckdb_result_type = 1;
pub const DUCKDB_RESULT_TYPE_NOTHING: duckdb_result_type = 2;
pub const DUCKDB_RESULT_TYPE_QUERY_RESULT: duckdb_result_type = 3;

pub type duckdb_statement_type = u32;
pub const DUCKDB_STATEMENT_TYPE_INVALID: duckdb_statement_type = 0;
pub const DUCKDB_STATEMENT_TYPE_SELECT: duckdb_statement_type = 1;
pub const DUCKDB_STATEMENT_TYPE_INSERT: duckdb_statement_type = 2;
pub const DUCKDB_STATEMENT_TYPE_UPDATE: duckdb_statement_type = 3;
pub const DUCKDB_STATEMENT_TYPE_EXPLAIN: duckdb_statement_type = 4;
pub const DUCKDB_STATEMENT_TYPE_DELETE: duckdb_statement_type = 5;
pub const DUCKDB_STATEMENT_TYPE_PREPARE: duckdb_statement_type = 6;
pub const DUCKDB_STATEMENT_TYPE_CREATE: duckdb_statement_type = 7;
pub const DUCKDB_STATEMENT_TYPE_EXECUTE: duckdb_statement_type = 8;
pub const DUCKDB_STATEMENT_TYPE_ALTER: duckdb_statement_type = 9;
pub const DUCKDB_STATEMENT_TYPE_TRANSACTION: duckdb_statement_type = 10;
pub const DUCKDB_STATEMENT_TYPE_COPY: duckdb_statement_type = 11;
pub const DUCKDB_STATEMENT_TYPE_ANALYZE: duckdb_statement_type = 12;
pub const DUCKDB_STATEMENT_TYPE_VARIABLE_SET: duckdb_statement_type = 13;
pub const DUCKDB_STATEMENT_TYPE_CREATE_FUNC: duckdb_statement_type = 14;
pub const DUCKDB_STATEMENT_TYPE_DROP: duckdb_statement_type = 15;
pub const DUCKDB_STATEMENT_TYPE_EXPORT: duckdb_statement_type = 16;
pub const DUCKDB_STATEMENT_TYPE_PRAGMA: duckdb_statement_type = 17;
pub const DUCKDB_STATEMENT_TYPE_VACUUM: duckdb_statement_type = 18;
pub const DUCKDB_STATEMENT_TYPE_CALL: duckdb_statement_type = 19;
pub const DUCKDB_STATEMENT_TYPE_SET: duckdb_statement_type = 20;
pub const DUCKDB_STATEMENT_TYPE_LOAD: duckdb_statement_type = 21;
pub const DUCKDB_STATEMENT_TYPE_RELATION: duckdb_statement_type = 22;
pub const DUCKDB_STATEMENT_TYPE_EXTENSION: duckdb_statement_type = 23;
pub const DUCKDB_STATEMENT_TYPE_LOGICAL_PLAN: duckdb_statement_type = 24;
pub const DUCKDB_STATEMENT_TYPE_ATTACH: duckdb_statement_type = 25;
pub const DUCKDB_STATEMENT_TYPE_DETACH: duckdb_statement_type = 26;
pub const DUCKDB_STATEMENT_TYPE_MULTI: duckdb_statement_type = 27;
pub const DUCKDB_STATEMENT_TYPE_COPY_DATABASE: duckdb_statement_type = 28;
pub const DUCKDB_STATEMENT_TYPE_UPDATE_EXTENSIONS: duckdb_statement_type = 29;
pub const DUCKDB_STATEMENT_TYPE_MERGE_INTO: duckdb_statement_type = 30;

pub type duckdb_error_type = u32;
pub const DUCKDB_ERROR_INVALID: duckdb_error_type = 0;
pub const DUCKDB_ERROR_OUT_OF_RANGE: duckdb_error_type = 1;
pub const DUCKDB_ERROR_CONVERSION: duckdb_error_type = 2;
pub const DUCKDB_ERROR_UNKNOWN_TYPE: duckdb_error_type = 3;
pub const DUCKDB_ERROR_DECIMAL: duckdb_error_type = 4;
pub const DUCKDB_ERROR_MISMATCH_TYPE: duckdb_error_type = 5;
pub const DUCKDB_ERROR_DIVIDE_BY_ZERO: duckdb_error_type = 6;
pub const DUCKDB_ERROR_OBJECT_SIZE: duckdb_error_type = 7;
pub const DUCKDB_ERROR_INVALID_TYPE: duckdb_error_type = 8;
pub const DUCKDB_ERROR_SERIALIZATION: duckdb_error_type = 9;
pub const DUCKDB_ERROR_TRANSACTION: duckdb_error_type = 10;
pub const DUCKDB_ERROR_NOT_IMPLEMENTED: duckdb_error_type = 11;
pub const DUCKDB_ERROR_EXPRESSION: duckdb_error_type = 12;
pub const DUCKDB_ERROR_CATALOG: duckdb_error_type = 13;
pub const DUCKDB_ERROR_PARSER: duckdb_error_type = 14;
pub const DUCKDB_ERROR_PLANNER: duckdb_error_type = 15;
pub const DUCKDB_ERROR_SCHEDULER: duckdb_error_type = 16;
pub const DUCKDB_ERROR_EXECUTOR: duckdb_error_type = 17;
pub const DUCKDB_ERROR_CONSTRAINT: duckdb_error_type = 18;
pub const DUCKDB_ERROR_INDEX: duckdb_error_type = 19;
pub const DUCKDB_ERROR_STAT: duckdb_error_type = 20;
pub const DUCKDB_ERROR_CONNECTION: duckdb_error_type = 21;
pub const DUCKDB_ERROR_SYNTAX: duckdb_error_type = 22;
pub const DUCKDB_ERROR_SETTINGS: duckdb_error_type = 23;
pub const DUCKDB_ERROR_BINDER: duckdb_error_type = 24;
pub const DUCKDB_ERROR_NETWORK: duckdb_error_type = 25;
pub const DUCKDB_ERROR_OPTIMIZER: duckdb_error_type = 26;
pub const DUCKDB_ERROR_NULL_POINTER: duckdb_error_type = 27;
pub const DUCKDB_ERROR_IO: duckdb_error_type = 28;
pub const DUCKDB_ERROR_INTERRUPT: duckdb_error_type = 29;
pub const DUCKDB_ERROR_FATAL: duckdb_error_type = 30;
pub const DUCKDB_ERROR_INTERNAL: duckdb_error_type = 31;
pub const DUCKDB_ERROR_INVALID_INPUT: duckdb_error_type = 32;
pub const DUCKDB_ERROR_OUT_OF_MEMORY: duckdb_error_type = 33;
pub const DUCKDB_ERROR_PERMISSION: duckdb_error_type = 34;
pub const DUCKDB_ERROR_PARAMETER_NOT_RESOLVED: duckdb_error_type = 35;
pub const DUCKDB_ERROR_PARAMETER_NOT_ALLOWED: duckdb_error_type = 36;
pub const DUCKDB_ERROR_DEPENDENCY: duckdb_error_type = 37;
pub const DUCKDB_ERROR_HTTP: duckdb_error_type = 38;
pub const DUCKDB_ERROR_MISSING_EXTENSION: duckdb_error_type = 39;
pub const DUCKDB_ERROR_AUTOLOAD: duckdb_error_type = 40;
pub const DUCKDB_ERROR_SEQUENCE: duckdb_error_type = 41;
pub const DUCKDB_INVALID_CONFIGURATION: duckdb_error_type = 42;
pub const DUCKDB_ERROR_DATA_CORRUPTION: duckdb_error_type = 43;
pub const DUCKDB_ERROR_RESOURCE_IN_USE: duckdb_error_type = 44;

pub type duckdb_cast_mode = u32;
pub const DUCKDB_CAST_NORMAL: duckdb_cast_mode = 0;
pub const DUCKDB_CAST_TRY: duckdb_cast_mode = 1;

/// `duckdb_string_t`: DuckDB's 16-byte string, the layout of `duckrs_common::types::StringT`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_string_t {
    pub value: duckdb_string_t_value,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union duckdb_string_t_value {
    pub pointer: duckdb_string_t_pointer,
    pub inlined: duckdb_string_t_inlined,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_string_t_pointer {
    pub length: u32,
    pub prefix: [c_char; 4],
    pub ptr: *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_string_t_inlined {
    pub length: u32,
    pub inlined: [c_char; 12],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_date {
    pub days: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_date_struct {
    pub year: i32,
    pub month: i8,
    pub day: i8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_time {
    pub micros: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_time_struct {
    pub hour: i8,
    pub min: i8,
    pub sec: i8,
    pub micros: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_time_ns {
    pub nanos: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_time_tz {
    pub bits: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_time_tz_struct {
    pub time: duckdb_time_struct,
    pub offset: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_timestamp {
    pub micros: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_timestamp_struct {
    pub date: duckdb_date_struct,
    pub time: duckdb_time_struct,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_timestamp_s {
    pub seconds: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_timestamp_ms {
    pub millis: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_timestamp_ns {
    pub nanos: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_hugeint {
    pub lower: u64,
    pub upper: i64,
}

impl duckdb_hugeint {
    pub fn from_i128(value: i128) -> Self {
        Self {
            lower: value as u64,
            upper: (value >> 64) as i64,
        }
    }

    pub fn to_i128(self) -> i128 {
        (i128::from(self.upper) << 64) | i128::from(self.lower)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_uhugeint {
    pub lower: u64,
    pub upper: u64,
}

impl duckdb_uhugeint {
    pub fn from_u128(value: u128) -> Self {
        Self {
            lower: value as u64,
            upper: (value >> 64) as u64,
        }
    }

    pub fn to_u128(self) -> u128 {
        (u128::from(self.upper) << 64) | u128::from(self.lower)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_decimal {
    pub width: u8,
    pub scale: u8,
    pub value: duckdb_hugeint,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct duckdb_query_progress_type {
    pub percentage: f64,
    pub rows_processed: u64,
    pub total_rows_to_process: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct duckdb_list_entry {
    pub offset: u64,
    pub length: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_column {
    pub deprecated_data: *mut c_void,
    pub deprecated_nullmask: *mut bool,
    pub deprecated_type: duckdb_type,
    pub deprecated_name: *mut c_char,
    pub internal_data: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_string {
    pub data: *mut c_char,
    pub size: idx_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_blob {
    pub data: *mut c_void,
    pub size: idx_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_bit {
    pub data: *mut u8,
    pub size: idx_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_bignum {
    pub data: *mut u8,
    pub size: idx_t,
    pub is_negative: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct duckdb_result {
    pub deprecated_column_count: idx_t,
    pub deprecated_row_count: idx_t,
    pub deprecated_rows_changed: idx_t,
    pub deprecated_columns: *mut duckdb_column,
    pub deprecated_error_message: *mut c_char,
    pub internal_data: *mut c_void,
}

impl duckdb_result {
    /// The all-zero result (`memset(out, 0, sizeof(duckdb_result))`).
    pub const fn zeroed() -> Self {
        Self {
            deprecated_column_count: 0,
            deprecated_row_count: 0,
            deprecated_rows_changed: 0,
            deprecated_columns: std::ptr::null_mut(),
            deprecated_error_message: std::ptr::null_mut(),
            internal_data: std::ptr::null_mut(),
        }
    }
}

/// The opaque handles: each is a pointer to a struct of one pointer, which C never reads. The pointer C holds
/// is that of the Rust object behind the handle ([`crate::ffi::Handle::Target`]).
macro_rules! handles {
    ($($handle:ident = $object:ident;)*) => {$(
        #[repr(C)]
        pub struct $object {
            pub internal_ptr: *mut c_void,
        }
        pub type $handle = *mut $object;
    )*};
}

handles! {
    duckdb_instance_cache = _duckdb_instance_cache;
    duckdb_database = _duckdb_database;
    duckdb_connection = _duckdb_connection;
    duckdb_client_context = _duckdb_client_context;
    duckdb_prepared_statement = _duckdb_prepared_statement;
    duckdb_extracted_statements = _duckdb_extracted_statements;
    duckdb_pending_result = _duckdb_pending_result;
    duckdb_appender = _duckdb_appender;
    duckdb_table_description = _duckdb_table_description;
    duckdb_config = _duckdb_config;
    duckdb_logical_type = _duckdb_logical_type;
    duckdb_data_chunk = _duckdb_data_chunk;
    duckdb_value = _duckdb_value;
    duckdb_error_data = _duckdb_error_data;
    duckdb_vector = _duckdb_vector;
    duckdb_selection_vector = _duckdb_selection_vector;
    duckdb_expression = _duckdb_expression;
    duckdb_function_info = _duckdb_function_info;
    duckdb_bind_info = _duckdb_bind_info;
    duckdb_init_info = _duckdb_init_info;
    duckdb_scalar_function = _duckdb_scalar_function;
    duckdb_scalar_function_set = _duckdb_scalar_function_set;
    duckdb_aggregate_function = _duckdb_aggregate_function;
    duckdb_aggregate_function_set = _duckdb_aggregate_function_set;
    duckdb_aggregate_state = _duckdb_aggregate_state;
    duckdb_table_function = _duckdb_table_function;
}

/// The callbacks of the user-defined functions. A null function pointer is `None`.
pub type duckdb_delete_callback_t = Option<unsafe extern "C" fn(data: *mut c_void)>;
pub type duckdb_copy_callback_t = Option<unsafe extern "C" fn(data: *mut c_void) -> *mut c_void>;
pub type duckdb_scalar_function_bind_t = Option<unsafe extern "C" fn(info: duckdb_bind_info)>;
pub type duckdb_scalar_function_init_t = Option<unsafe extern "C" fn(info: duckdb_init_info)>;
pub type duckdb_scalar_function_t =
    Option<unsafe extern "C" fn(info: duckdb_function_info, input: duckdb_data_chunk, output: duckdb_vector)>;
pub type duckdb_aggregate_state_size = Option<unsafe extern "C" fn(info: duckdb_function_info) -> idx_t>;
pub type duckdb_aggregate_init_t = Option<unsafe extern "C" fn(info: duckdb_function_info, state: duckdb_aggregate_state)>;
pub type duckdb_aggregate_destroy_t = Option<unsafe extern "C" fn(states: *mut duckdb_aggregate_state, count: idx_t)>;
pub type duckdb_aggregate_update_t = Option<
    unsafe extern "C" fn(info: duckdb_function_info, input: duckdb_data_chunk, states: *mut duckdb_aggregate_state),
>;
pub type duckdb_aggregate_combine_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        source: *mut duckdb_aggregate_state,
        target: *mut duckdb_aggregate_state,
        count: idx_t,
    ),
>;
pub type duckdb_aggregate_finalize_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        source: *mut duckdb_aggregate_state,
        result: duckdb_vector,
        count: idx_t,
        offset: idx_t,
    ),
>;
pub type duckdb_table_function_bind_t = Option<unsafe extern "C" fn(info: duckdb_bind_info)>;
pub type duckdb_table_function_init_t = Option<unsafe extern "C" fn(info: duckdb_init_info)>;
pub type duckdb_table_function_t = Option<unsafe extern "C" fn(info: duckdb_function_info, output: duckdb_data_chunk)>;
