// Ported from duckdb/src/main/capi/v1/expression-c.cpp, the client context functions of duckdb-c.cpp, and the
// info structures of scalar_function-c.cpp, aggregate_function-c.cpp, table_function-c.cpp and
// duckdb/src/include/duckdb/main/capi/capi_internal_table.hpp @ rust-port-baseline

//! What the callbacks of user-defined functions are given: `duckdb_function_info`, `duckdb_bind_info` and
//! `duckdb_init_info`, which point to an info of the function kind that calls the callback, and
//! `duckdb_client_context` and `duckdb_expression`, which binds hand out.
//!
//! An info lives on the stack of the call of the callback, and holds what the callback may read and what it set,
//! which the caller reads back when the callback returns.

use crate::database::ConnectionWrapper;
use crate::error::{ErrorData, new_error_data};
use crate::ffi::{self, CPointer, Handle};
use crate::types::*;
use duckrs::udf::Expression;
use duckrs::{ErrorKind, LogicalType, Value};
use std::sync::Arc;

/// C data with the callback that frees it (extra info, bind data, init data, a local state), freed when its last user
/// lets go of it, as C++'s destructors of `CScalarFunctionInfo`, `CTableBindData`, ... free it.
#[derive(Debug)]
pub(crate) struct OwnedData {
    pub(crate) data: CPointer,
    delete: duckdb_delete_callback_t,
}

impl OwnedData {
    pub(crate) fn new(data: *mut std::ffi::c_void, delete: duckdb_delete_callback_t) -> Self {
        Self {
            data: CPointer(data),
            delete,
        }
    }

    pub(crate) fn ptr(this: &Option<impl AsRef<OwnedData>>) -> *mut std::ffi::c_void {
        this.as_ref()
            .map_or(std::ptr::null_mut(), |owned| owned.as_ref().data.0)
    }
}

impl AsRef<OwnedData> for OwnedData {
    fn as_ref(&self) -> &OwnedData {
        self
    }
}

impl Drop for OwnedData {
    fn drop(&mut self) {
        if let Some(delete) = self.delete
            && !self.data.0.is_null()
        {
            // SAFETY: C gave the data with this callback to free it, which runs once, when duckrs is done with it.
            unsafe { delete(self.data.0) };
        }
    }
}

/// [`OwnedData`] shared by the copies of a function (`shared_ptr<CScalarFunctionInfo>`'s `extra_info`).
pub(crate) type ExtraInfo = Option<Arc<OwnedData>>;

pub(crate) fn extra_ptr(extra: &ExtraInfo) -> *mut std::ffi::c_void {
    OwnedData::ptr(extra)
}

/// What a callback's `*_set_error` set: `None` while it succeeds.
pub(crate) type CallbackError = Option<String>;

/// `CScalarFunctionInternalBindInfo`.
pub(crate) struct ScalarBindInfo {
    pub(crate) extra: CPointer,
    pub(crate) connection_id: u64,
    pub(crate) arguments: Vec<Expression>,
    pub(crate) bind_data: Option<OwnedData>,
    pub(crate) copy: duckdb_copy_callback_t,
    pub(crate) error: CallbackError,
}

/// `CTableInternalBindInfo`.
pub(crate) struct TableBindInfo {
    pub(crate) extra: CPointer,
    pub(crate) connection_id: u64,
    pub(crate) parameters: Vec<Value>,
    pub(crate) named_parameters: Vec<(String, Value)>,
    pub(crate) names: Vec<String>,
    /// the names as C strings, made when C asks for them
    pub(crate) c_names: Vec<Option<std::ffi::CString>>,
    pub(crate) types: Vec<LogicalType>,
    pub(crate) bind_data: Option<OwnedData>,
    pub(crate) cardinality: Option<(u64, bool)>,
    pub(crate) error: CallbackError,
}

/// The `duckdb_bind_info` of a scalar or a table function.
pub(crate) enum BindInfo {
    Scalar(ScalarBindInfo),
    Table(TableBindInfo),
}

/// `CScalarFunctionInternalInitInfo`.
pub(crate) struct ScalarInitInfo {
    pub(crate) extra: CPointer,
    pub(crate) bind_data: CPointer,
    pub(crate) connection_id: u64,
    pub(crate) state: Option<OwnedData>,
    pub(crate) error: CallbackError,
}

/// `CTableInternalInitInfo`.
pub(crate) struct TableInitInfo {
    pub(crate) extra: CPointer,
    pub(crate) bind_data: CPointer,
    pub(crate) column_ids: Vec<idx_t>,
    pub(crate) init_data: Option<OwnedData>,
    pub(crate) max_threads: idx_t,
    pub(crate) error: CallbackError,
}

/// The `duckdb_init_info` of a scalar or a table function.
pub(crate) enum InitInfo {
    Scalar(ScalarInitInfo),
    Table(TableInitInfo),
}

/// `CScalarFunctionInternalFunctionInfo`.
pub(crate) struct ScalarFunctionInfo {
    pub(crate) extra: CPointer,
    pub(crate) bind_data: CPointer,
    pub(crate) state: CPointer,
    pub(crate) error: CallbackError,
}

/// `CAggregateExecuteInfo`.
pub(crate) struct AggregateFunctionInfo {
    pub(crate) extra: CPointer,
    pub(crate) error: CallbackError,
}

/// `CTableInternalFunctionInfo`.
pub(crate) struct TableFunctionInfo {
    pub(crate) extra: CPointer,
    pub(crate) bind_data: CPointer,
    pub(crate) init_data: CPointer,
    pub(crate) local_data: CPointer,
    pub(crate) error: CallbackError,
}

/// `CCastExecuteInfo`.
pub(crate) struct CastFunctionInfo {
    pub(crate) extra: CPointer,
    /// `parameters.error_message != nullptr`: TRY_CAST
    pub(crate) try_cast: bool,
    pub(crate) error: CallbackError,
}

/// The `duckdb_function_info` of a scalar, an aggregate or a table function, or of a cast.
pub(crate) enum FunctionInfo {
    Scalar(ScalarFunctionInfo),
    Aggregate(AggregateFunctionInfo),
    Table(TableFunctionInfo),
    Cast(CastFunctionInfo),
}

impl Handle for _duckdb_bind_info {
    type Target = BindInfo;
}

impl Handle for _duckdb_init_info {
    type Target = InitInfo;
}

impl Handle for _duckdb_function_info {
    type Target = FunctionInfo;
}

/// Sets the error of a callback, from the C string `error` (`*_set_error`); nothing for a null one.
///
/// # Safety
///
/// `error` is null or a NUL-terminated string.
pub(crate) unsafe fn set_error(slot: &mut CallbackError, error: *const std::ffi::c_char) {
    // SAFETY: by the contract of this function.
    if let Some(error) = unsafe { ffi::c_str(error) } {
        *slot = Some(String::from_utf8_lossy(error).into_owned());
    }
}

/// The error of a query a callback's error gives (`throw BinderException(error)`, `throw InvalidInputException`).
pub(crate) fn callback_error(kind: ErrorKind, error: CallbackError) -> duckrs::Result<()> {
    match error {
        None => Ok(()),
        Some(message) => Err(duckrs::Error::new(kind, message)),
    }
}

// --------------------------------------------------------------------------------------------------------------
// Client context
// --------------------------------------------------------------------------------------------------------------

/// `CClientContextWrapper`: what duckrs tells of the connection a callback runs for.
#[derive(Debug, Clone, Copy)]
pub struct ClientContextWrapper {
    pub(crate) connection_id: u64,
}

impl Handle for _duckdb_client_context {
    type Target = ClientContextWrapper;
}

/// Writes a new client context of `connection_id` to `out` (`new CClientContextWrapper(context)`).
///
/// # Safety
///
/// `out` is null or valid for a write of a handle.
pub(crate) unsafe fn write_client_context(out: *mut duckdb_client_context, connection_id: u64) {
    if out.is_null() {
        return;
    }
    let handle = ffi::into_handle(ClientContextWrapper { connection_id });
    // SAFETY: by the contract of this function, `out` is valid for a write.
    unsafe { ffi::write(out, handle) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_connection_get_client_context(
    connection: duckdb_connection,
    out_context: *mut duckdb_client_context,
) {
    // SAFETY: C passes a live connection.
    let Some(connection) = (unsafe { ffi::get::<_duckdb_connection>(connection) }) else {
        return;
    };
    // the id is the connection's own, which a callback of a query on the connection can ask for
    let ConnectionWrapper { connection_id, .. } = connection;
    // SAFETY: C passes where to write the context.
    unsafe { write_client_context(out_context, *connection_id) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_client_context_get_connection_id(context: duckdb_client_context) -> idx_t {
    // SAFETY: C passes a live client context.
    unsafe { ffi::get(context) }.map_or(0, |context| context.connection_id)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_client_context(context: *mut duckdb_client_context) {
    // SAFETY: C passes a context duckdb made.
    drop(unsafe { ffi::take(context) });
}

// --------------------------------------------------------------------------------------------------------------
// Expressions
// --------------------------------------------------------------------------------------------------------------

impl Handle for _duckdb_expression {
    type Target = Expression;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_expression(expr: *mut duckdb_expression) {
    // SAFETY: C passes an expression duckdb made.
    drop(unsafe { ffi::take(expr) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_expression_return_type(expr: duckdb_expression) -> duckdb_logical_type {
    // SAFETY: C passes a live expression.
    unsafe { ffi::get(expr) }.map_or(std::ptr::null_mut(), |expr| ffi::into_handle(expr.return_type.clone()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_expression_is_foldable(expr: duckdb_expression) -> bool {
    // SAFETY: C passes a live expression.
    unsafe { ffi::get(expr) }.is_some_and(|expr| ffi::guard(false, || expr.is_foldable()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_expression_fold(
    _context: duckdb_client_context,
    expr: duckdb_expression,
    out_value: *mut duckdb_value,
) -> duckdb_error_data {
    // SAFETY: C passes a live expression.
    let Some(expr) = (unsafe { ffi::get(expr) }) else {
        return std::ptr::null_mut();
    };
    if !ffi::guard(false, || expr.is_foldable()) {
        return std::ptr::null_mut();
    }
    let folded = ffi::guard(None, || Some(duckrs::udf::fold(expr)));
    match folded {
        Some(Ok(value)) => {
            // SAFETY: C passes where to write the value.
            unsafe { ffi::write(out_value, ffi::into_handle::<_duckdb_value>(value)) };
            std::ptr::null_mut()
        }
        Some(Err(error)) => new_error_data(ErrorKind::InvalidInput, &error.to_string()),
        None => ffi::into_handle(ErrorData::new(
            ErrorKind::InvalidInput,
            "unknown error occurred during folding",
        )),
    }
}
