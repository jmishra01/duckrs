// Ported from duckdb/src/main/capi/v1/helper-c.cpp @ rust-port-baseline

//! The small helpers of the library, and the conversions between pintail's enums and those of `duckdb.h`.

use crate::error::new_error_data;
use crate::ffi;
use crate::types::*;
use pintail::{ErrorKind, LogicalTypeId, StatementReturnType, StatementType};
use std::ffi::{c_char, c_void};

/// `LogicalTypeIdFromC`.
pub(crate) fn type_id_from_c(ty: duckdb_type) -> Option<LogicalTypeId> {
    use LogicalTypeId as L;
    Some(match ty {
        DUCKDB_TYPE_INVALID => L::Invalid,
        DUCKDB_TYPE_BOOLEAN => L::Boolean,
        DUCKDB_TYPE_TINYINT => L::TinyInt,
        DUCKDB_TYPE_SMALLINT => L::SmallInt,
        DUCKDB_TYPE_INTEGER => L::Integer,
        DUCKDB_TYPE_BIGINT => L::BigInt,
        DUCKDB_TYPE_UTINYINT => L::UTinyInt,
        DUCKDB_TYPE_USMALLINT => L::USmallInt,
        DUCKDB_TYPE_UINTEGER => L::UInteger,
        DUCKDB_TYPE_UBIGINT => L::UBigInt,
        DUCKDB_TYPE_FLOAT => L::Float,
        DUCKDB_TYPE_DOUBLE => L::Double,
        DUCKDB_TYPE_TIMESTAMP => L::Timestamp,
        DUCKDB_TYPE_DATE => L::Date,
        DUCKDB_TYPE_TIME => L::Time,
        DUCKDB_TYPE_INTERVAL => L::Interval,
        DUCKDB_TYPE_HUGEINT => L::HugeInt,
        DUCKDB_TYPE_UHUGEINT => L::UHugeInt,
        DUCKDB_TYPE_VARCHAR => L::Varchar,
        DUCKDB_TYPE_BLOB => L::Blob,
        DUCKDB_TYPE_DECIMAL => L::Decimal,
        DUCKDB_TYPE_TIMESTAMP_S => L::TimestampSec,
        DUCKDB_TYPE_TIMESTAMP_MS => L::TimestampMs,
        DUCKDB_TYPE_TIMESTAMP_NS => L::TimestampNs,
        DUCKDB_TYPE_ENUM => L::Enum,
        DUCKDB_TYPE_LIST => L::List,
        DUCKDB_TYPE_STRUCT => L::Struct,
        DUCKDB_TYPE_MAP => L::Map,
        DUCKDB_TYPE_ARRAY => L::Array,
        DUCKDB_TYPE_UUID => L::Uuid,
        DUCKDB_TYPE_UNION => L::Union,
        DUCKDB_TYPE_BIT => L::Bit,
        DUCKDB_TYPE_TIME_TZ => L::TimeTz,
        DUCKDB_TYPE_TIMESTAMP_TZ => L::TimestampTz,
        DUCKDB_TYPE_TIMESTAMP_TZ_NS => L::TimestampTzNs,
        DUCKDB_TYPE_ANY => L::Any,
        DUCKDB_TYPE_BIGNUM => L::Bignum,
        DUCKDB_TYPE_SQLNULL => L::SqlNull,
        DUCKDB_TYPE_STRING_LITERAL => L::StringLiteral,
        DUCKDB_TYPE_INTEGER_LITERAL => L::IntegerLiteral,
        DUCKDB_TYPE_TIME_NS => L::TimeNs,
        DUCKDB_TYPE_GEOMETRY => L::Geometry,
        DUCKDB_TYPE_VARIANT => L::Variant,
        _ => return None,
    })
}

/// `LogicalTypeIdToC`.
pub(crate) fn type_id_to_c(id: LogicalTypeId) -> duckdb_type {
    use LogicalTypeId as L;
    match id {
        L::Boolean => DUCKDB_TYPE_BOOLEAN,
        L::TinyInt => DUCKDB_TYPE_TINYINT,
        L::SmallInt => DUCKDB_TYPE_SMALLINT,
        L::Integer => DUCKDB_TYPE_INTEGER,
        L::BigInt => DUCKDB_TYPE_BIGINT,
        L::UTinyInt => DUCKDB_TYPE_UTINYINT,
        L::USmallInt => DUCKDB_TYPE_USMALLINT,
        L::UInteger => DUCKDB_TYPE_UINTEGER,
        L::UBigInt => DUCKDB_TYPE_UBIGINT,
        L::HugeInt => DUCKDB_TYPE_HUGEINT,
        L::UHugeInt => DUCKDB_TYPE_UHUGEINT,
        L::Float => DUCKDB_TYPE_FLOAT,
        L::Double => DUCKDB_TYPE_DOUBLE,
        L::Timestamp => DUCKDB_TYPE_TIMESTAMP,
        L::TimestampTz => DUCKDB_TYPE_TIMESTAMP_TZ,
        L::TimestampTzNs => DUCKDB_TYPE_TIMESTAMP_TZ_NS,
        L::TimestampSec => DUCKDB_TYPE_TIMESTAMP_S,
        L::TimestampMs => DUCKDB_TYPE_TIMESTAMP_MS,
        L::TimestampNs => DUCKDB_TYPE_TIMESTAMP_NS,
        L::Date => DUCKDB_TYPE_DATE,
        L::Time => DUCKDB_TYPE_TIME,
        L::TimeTz => DUCKDB_TYPE_TIME_TZ,
        L::Varchar | L::Char => DUCKDB_TYPE_VARCHAR,
        L::Blob => DUCKDB_TYPE_BLOB,
        L::Bit => DUCKDB_TYPE_BIT,
        L::Bignum => DUCKDB_TYPE_BIGNUM,
        L::Interval => DUCKDB_TYPE_INTERVAL,
        L::Decimal => DUCKDB_TYPE_DECIMAL,
        L::Enum => DUCKDB_TYPE_ENUM,
        L::List => DUCKDB_TYPE_LIST,
        // TUPLE is an unnamed struct, reported as a STRUCT whose children have empty names
        L::Struct | L::Tuple => DUCKDB_TYPE_STRUCT,
        L::Map => DUCKDB_TYPE_MAP,
        L::Union => DUCKDB_TYPE_UNION,
        L::Uuid => DUCKDB_TYPE_UUID,
        L::Array => DUCKDB_TYPE_ARRAY,
        L::Any => DUCKDB_TYPE_ANY,
        L::SqlNull => DUCKDB_TYPE_SQLNULL,
        L::StringLiteral => DUCKDB_TYPE_STRING_LITERAL,
        L::IntegerLiteral => DUCKDB_TYPE_INTEGER_LITERAL,
        L::TimeNs => DUCKDB_TYPE_TIME_NS,
        L::Geometry => DUCKDB_TYPE_GEOMETRY,
        L::Variant => DUCKDB_TYPE_VARIANT,
        _ => DUCKDB_TYPE_INVALID,
    }
}

/// `GetCTypeSize`: the size of a value of `ty` in the deprecated materialized result, 0 for the types it leaves out.
pub(crate) fn c_type_size(ty: duckdb_type) -> usize {
    match ty {
        DUCKDB_TYPE_BOOLEAN | DUCKDB_TYPE_TINYINT | DUCKDB_TYPE_UTINYINT => 1,
        DUCKDB_TYPE_SMALLINT | DUCKDB_TYPE_USMALLINT => 2,
        DUCKDB_TYPE_INTEGER | DUCKDB_TYPE_SQLNULL | DUCKDB_TYPE_UINTEGER | DUCKDB_TYPE_FLOAT | DUCKDB_TYPE_DATE => 4,
        DUCKDB_TYPE_BIGINT | DUCKDB_TYPE_UBIGINT | DUCKDB_TYPE_DOUBLE | DUCKDB_TYPE_TIME | DUCKDB_TYPE_TIME_NS => 8,
        DUCKDB_TYPE_TIMESTAMP
        | DUCKDB_TYPE_TIMESTAMP_TZ
        | DUCKDB_TYPE_TIMESTAMP_S
        | DUCKDB_TYPE_TIMESTAMP_MS
        | DUCKDB_TYPE_TIMESTAMP_NS
        | DUCKDB_TYPE_TIMESTAMP_TZ_NS => size_of::<duckdb_timestamp>(),
        DUCKDB_TYPE_UHUGEINT | DUCKDB_TYPE_HUGEINT | DUCKDB_TYPE_UUID | DUCKDB_TYPE_DECIMAL => {
            size_of::<duckdb_hugeint>()
        }
        DUCKDB_TYPE_VARCHAR => size_of::<*const c_char>(),
        DUCKDB_TYPE_BLOB => size_of::<duckdb_blob>(),
        DUCKDB_TYPE_INTERVAL => size_of::<duckdb_interval>(),
        // nested and other types: the deprecated result leaves them NULL; the vector interface reads them
        _ => 0,
    }
}

/// `StatementTypeToC`.
pub(crate) fn statement_type_to_c(ty: StatementType) -> duckdb_statement_type {
    use StatementType as S;
    match ty {
        S::Invalid => DUCKDB_STATEMENT_TYPE_INVALID,
        S::Select => DUCKDB_STATEMENT_TYPE_SELECT,
        S::Insert => DUCKDB_STATEMENT_TYPE_INSERT,
        S::Update => DUCKDB_STATEMENT_TYPE_UPDATE,
        S::Explain => DUCKDB_STATEMENT_TYPE_EXPLAIN,
        S::Delete => DUCKDB_STATEMENT_TYPE_DELETE,
        S::Prepare => DUCKDB_STATEMENT_TYPE_PREPARE,
        S::Create => DUCKDB_STATEMENT_TYPE_CREATE,
        S::Execute => DUCKDB_STATEMENT_TYPE_EXECUTE,
        S::Alter => DUCKDB_STATEMENT_TYPE_ALTER,
        S::Transaction => DUCKDB_STATEMENT_TYPE_TRANSACTION,
        S::Copy => DUCKDB_STATEMENT_TYPE_COPY,
        S::Analyze => DUCKDB_STATEMENT_TYPE_ANALYZE,
        S::VariableSet => DUCKDB_STATEMENT_TYPE_VARIABLE_SET,
        S::CreateFunc => DUCKDB_STATEMENT_TYPE_CREATE_FUNC,
        S::Drop => DUCKDB_STATEMENT_TYPE_DROP,
        S::Export => DUCKDB_STATEMENT_TYPE_EXPORT,
        S::Pragma => DUCKDB_STATEMENT_TYPE_PRAGMA,
        S::Vacuum => DUCKDB_STATEMENT_TYPE_VACUUM,
        S::Call => DUCKDB_STATEMENT_TYPE_CALL,
        S::Set => DUCKDB_STATEMENT_TYPE_SET,
        S::Load => DUCKDB_STATEMENT_TYPE_LOAD,
        S::Relation => DUCKDB_STATEMENT_TYPE_RELATION,
        S::Extension => DUCKDB_STATEMENT_TYPE_EXTENSION,
        S::LogicalPlan => DUCKDB_STATEMENT_TYPE_LOGICAL_PLAN,
        S::Attach => DUCKDB_STATEMENT_TYPE_ATTACH,
        S::Detach => DUCKDB_STATEMENT_TYPE_DETACH,
        S::Multi => DUCKDB_STATEMENT_TYPE_MULTI,
        S::CopyDatabase => DUCKDB_STATEMENT_TYPE_COPY_DATABASE,
        S::UpdateExtensions => DUCKDB_STATEMENT_TYPE_UPDATE_EXTENSIONS,
        S::MergeInto => DUCKDB_STATEMENT_TYPE_MERGE_INTO,
    }
}

pub(crate) fn return_type_to_c(ty: StatementReturnType) -> duckdb_result_type {
    match ty {
        StatementReturnType::ChangedRows => DUCKDB_RESULT_TYPE_CHANGED_ROWS,
        StatementReturnType::Nothing => DUCKDB_RESULT_TYPE_NOTHING,
        StatementReturnType::QueryResult => DUCKDB_RESULT_TYPE_QUERY_RESULT,
    }
}

const ERROR_KINDS: [(ErrorKind, duckdb_error_type); 44] = [
    (ErrorKind::Invalid, DUCKDB_ERROR_INVALID),
    (ErrorKind::OutOfRange, DUCKDB_ERROR_OUT_OF_RANGE),
    (ErrorKind::Conversion, DUCKDB_ERROR_CONVERSION),
    (ErrorKind::UnknownType, DUCKDB_ERROR_UNKNOWN_TYPE),
    (ErrorKind::Decimal, DUCKDB_ERROR_DECIMAL),
    (ErrorKind::MismatchType, DUCKDB_ERROR_MISMATCH_TYPE),
    (ErrorKind::DivideByZero, DUCKDB_ERROR_DIVIDE_BY_ZERO),
    (ErrorKind::ObjectSize, DUCKDB_ERROR_OBJECT_SIZE),
    (ErrorKind::InvalidType, DUCKDB_ERROR_INVALID_TYPE),
    (ErrorKind::Serialization, DUCKDB_ERROR_SERIALIZATION),
    (ErrorKind::Transaction, DUCKDB_ERROR_TRANSACTION),
    (ErrorKind::NotImplemented, DUCKDB_ERROR_NOT_IMPLEMENTED),
    (ErrorKind::Expression, DUCKDB_ERROR_EXPRESSION),
    (ErrorKind::Catalog, DUCKDB_ERROR_CATALOG),
    (ErrorKind::Parser, DUCKDB_ERROR_PARSER),
    (ErrorKind::Planner, DUCKDB_ERROR_PLANNER),
    (ErrorKind::Scheduler, DUCKDB_ERROR_SCHEDULER),
    (ErrorKind::Executor, DUCKDB_ERROR_EXECUTOR),
    (ErrorKind::Constraint, DUCKDB_ERROR_CONSTRAINT),
    (ErrorKind::Index, DUCKDB_ERROR_INDEX),
    (ErrorKind::Stat, DUCKDB_ERROR_STAT),
    (ErrorKind::Connection, DUCKDB_ERROR_CONNECTION),
    (ErrorKind::Syntax, DUCKDB_ERROR_SYNTAX),
    (ErrorKind::Settings, DUCKDB_ERROR_SETTINGS),
    (ErrorKind::Binder, DUCKDB_ERROR_BINDER),
    (ErrorKind::Optimizer, DUCKDB_ERROR_OPTIMIZER),
    (ErrorKind::NullPointer, DUCKDB_ERROR_NULL_POINTER),
    (ErrorKind::Io, DUCKDB_ERROR_IO),
    (ErrorKind::Interrupt, DUCKDB_ERROR_INTERRUPT),
    (ErrorKind::Fatal, DUCKDB_ERROR_FATAL),
    (ErrorKind::Internal, DUCKDB_ERROR_INTERNAL),
    (ErrorKind::InvalidInput, DUCKDB_ERROR_INVALID_INPUT),
    (ErrorKind::OutOfMemory, DUCKDB_ERROR_OUT_OF_MEMORY),
    (ErrorKind::Permission, DUCKDB_ERROR_PERMISSION),
    (ErrorKind::ParameterNotResolved, DUCKDB_ERROR_PARAMETER_NOT_RESOLVED),
    (ErrorKind::ParameterNotAllowed, DUCKDB_ERROR_PARAMETER_NOT_ALLOWED),
    (ErrorKind::Dependency, DUCKDB_ERROR_DEPENDENCY),
    (ErrorKind::Http, DUCKDB_ERROR_HTTP),
    (ErrorKind::MissingExtension, DUCKDB_ERROR_MISSING_EXTENSION),
    (ErrorKind::Autoload, DUCKDB_ERROR_AUTOLOAD),
    (ErrorKind::Sequence, DUCKDB_ERROR_SEQUENCE),
    (ErrorKind::DataCorruption, DUCKDB_ERROR_DATA_CORRUPTION),
    (ErrorKind::InvalidConfiguration, DUCKDB_INVALID_CONFIGURATION),
    (ErrorKind::ResourceInUse, DUCKDB_ERROR_RESOURCE_IN_USE),
];

/// `ErrorTypeToC`.
pub(crate) fn error_type_to_c(kind: ErrorKind) -> duckdb_error_type {
    ERROR_KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(DUCKDB_ERROR_INVALID, |(_, c)| *c)
}

/// `ErrorTypeFromC`.
pub(crate) fn error_type_from_c(ty: duckdb_error_type) -> ErrorKind {
    ERROR_KINDS
        .iter()
        .find(|(_, c)| *c == ty)
        .map_or(ErrorKind::Invalid, |(k, _)| *k)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_malloc(size: usize) -> *mut c_void {
    // SAFETY: `malloc` may be called with any size.
    unsafe { libc::malloc(size) }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_free(ptr: *mut c_void) {
    ffi::free(ptr);
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_vector_size() -> idx_t {
    pintail_common::STANDARD_VECTOR_SIZE as idx_t
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_library_version() -> *const c_char {
    concat!("v", env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The length of a `duckdb_string_t`, which both members of its union start with.
fn string_length(string: &duckdb_string_t) -> u32 {
    // SAFETY: both members of the union are `repr(C)` structs that start with the `u32` length, so reading it
    // through either is reading the same initialized bytes.
    unsafe { string.value.inlined.length }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_string_is_inlined(string: duckdb_string_t) -> bool {
    string_length(&string) as usize <= pintail_common::types::StringT::INLINE_LENGTH
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_string_t_length(string: duckdb_string_t) -> u32 {
    string_length(&string)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_string_t_data(string: *mut duckdb_string_t) -> *const c_char {
    // SAFETY: C passes a pointer to a string it holds (a vector's or its own).
    let Some(string) = (unsafe { ffi::reference_mut(string) }) else {
        return std::ptr::null();
    };
    if duckdb_string_is_inlined(*string) {
        // SAFETY: an inlined string's bytes are those of the `inlined` member; taking their address reads nothing.
        unsafe { std::ptr::addr_of!(string.value.inlined.inlined) }.cast()
    } else {
        // SAFETY: a string that is not inlined is the `pointer` member, whose pointer C made or a vector holds.
        unsafe { string.value.pointer.ptr }.cast_const()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_valid_utf8_check(text: *const c_char, length: idx_t) -> duckdb_error_data {
    // SAFETY: C passes `length` bytes at `text`.
    let bytes = unsafe { ffi::bytes(text.cast(), length) };
    match bytes.map(std::str::from_utf8) {
        Some(Ok(_)) => std::ptr::null_mut(),
        _ => new_error_data(
            ErrorKind::InvalidInput,
            "invalid Unicode detected, str must be valid UTF-8",
        ),
    }
}
