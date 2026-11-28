// Ported from duckdb/src/main/capi/v1/config_options-c.cpp @ rust-port-baseline

//! Configuration options a client registers (`duckdb_config_option`): what the setters set, registered on the
//! database of a connection as an extension option (`DBConfig::AddExtensionOption`), which SET, RESET,
//! `current_setting` and `duckdb_settings()` then know.

use crate::database::{ConnectionWrapper, context_of};
use crate::ffi::{self, Handle};
use crate::function_info::ClientContextWrapper;
use crate::scalar_function::logical_type;
use crate::types::*;
use pintail::settings::{ExtensionOption, SetScope, SettingLookupScope};
use pintail::{LogicalType, Value};
use std::ffi::c_char;

/// `CConfigOption`: what `duckdb_create_config_option` makes.
pub struct ConfigOptionHandle {
    name: String,
    /// `None` while no type is set (`LogicalTypeId::INVALID`)
    ty: Option<LogicalType>,
    default_value: Value,
    default_scope: SetScope,
    description: String,
}

impl Handle for _duckdb_config_option {
    type Target = ConfigOptionHandle;
}

/// The option behind a handle C passes, to change.
///
/// # Safety
///
/// `option` is null or a live config option.
unsafe fn option_mut<'a>(option: duckdb_config_option) -> Option<&'a mut ConfigOptionHandle> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get_mut(option) }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_config_option() -> duckdb_config_option {
    ffi::into_handle(ConfigOptionHandle {
        name: String::new(),
        ty: None,
        default_value: Value::sql_null(),
        default_scope: SetScope::Session,
        description: String::new(),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_config_option(option: *mut duckdb_config_option) {
    // SAFETY: C passes an option `duckdb_create_config_option` made, or null.
    drop(unsafe { ffi::take(option) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_config_option_set_name(option: duckdb_config_option, name: *const c_char) {
    // SAFETY: C passes a live option and a NUL-terminated name.
    if let (Some(option), Some(name)) = (unsafe { option_mut(option) }, unsafe { ffi::c_str(name) }) {
        option.name = String::from_utf8_lossy(name).into_owned();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_config_option_set_type(option: duckdb_config_option, ty: duckdb_logical_type) {
    // SAFETY: C passes a live option and a live logical type.
    if let (Some(option), Some(ty)) = (unsafe { option_mut(option) }, unsafe { logical_type(ty) }) {
        option.ty = Some(ty.clone());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_config_option_set_default_value(option: duckdb_config_option, value: duckdb_value) {
    // SAFETY: C passes a live option and a live value.
    let (Some(option), Some(value)) = (unsafe { option_mut(option) }, unsafe {
        ffi::get::<_duckdb_value>(value)
    }) else {
        return;
    };
    // without a type, the option takes the value's; else the value is cast to it (`DefaultCastAs`, which throws)
    match &option.ty {
        None => {
            option.ty = Some(value.logical_type().clone());
            option.default_value = value.clone();
        }
        Some(ty) if ty != value.logical_type() => {
            if let Ok(cast) = ffi::guard(Err(pintail::Error::new(pintail::ErrorKind::Internal, "panic")), || {
                pintail::cast_value(value, ty)
            }) {
                option.default_value = cast;
            }
        }
        Some(_) => option.default_value = value.clone(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_config_option_set_default_scope(
    option: duckdb_config_option,
    scope: duckdb_config_option_scope,
) {
    // SAFETY: C passes a live option.
    let Some(option) = (unsafe { option_mut(option) }) else {
        return;
    };
    option.default_scope = match scope {
        DUCKDB_CONFIG_OPTION_SCOPE_LOCAL => SetScope::Local,
        DUCKDB_CONFIG_OPTION_SCOPE_SESSION => SetScope::Session,
        DUCKDB_CONFIG_OPTION_SCOPE_GLOBAL => SetScope::Global,
        _ => return,
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_config_option_set_description(
    option: duckdb_config_option,
    description: *const c_char,
) {
    // SAFETY: C passes a live option and a NUL-terminated description.
    if let (Some(option), Some(description)) = (unsafe { option_mut(option) }, unsafe { ffi::c_str(description) }) {
        option.description = String::from_utf8_lossy(description).into_owned();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_register_config_option(
    connection: duckdb_connection,
    option: duckdb_config_option,
) -> duckdb_state {
    // SAFETY: C passes a live connection and a live option.
    let (Some(connection), Some(option)) = (unsafe { ffi::get::<_duckdb_connection>(connection) }, unsafe {
        ffi::get(option)
    }) else {
        return DuckDBError;
    };
    let ConfigOptionHandle {
        name,
        ty: Some(ty),
        default_value,
        default_scope,
        description,
    } = option
    else {
        return DuckDBError;
    };
    if name.is_empty() {
        return DuckDBError;
    }
    let ConnectionWrapper { connection, .. } = connection;
    let option = ExtensionOption {
        name: name.clone(),
        description: description.clone(),
        ty: ty.clone(),
        default_value: default_value.clone(),
        default_scope: *default_scope,
    };
    let registered = ffi::guard(false, || connection.lock().register_config_option(option).is_ok());
    if registered { DuckDBSuccess } else { DuckDBError }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_client_context_get_config_option(
    context: duckdb_client_context,
    name: *const c_char,
    out_scope: *mut duckdb_config_option_scope,
) -> duckdb_value {
    // SAFETY: C passes a live client context and a NUL-terminated name.
    let (Some(ClientContextWrapper { connection_id }), Some(name)) =
        (unsafe { ffi::get::<_duckdb_client_context>(context) }, unsafe {
            ffi::c_str(name)
        })
    else {
        return std::ptr::null_mut();
    };
    let name = String::from_utf8_lossy(name);
    // a context reads the settings of its connection, unless a query runs on it (whose callback asks): then those
    // of its database
    let found = context_of(*connection_id).and_then(|(database, connection)| {
        ffi::guard(None, || {
            if connection.is_busy() {
                let database = database.upgrade()?;
                database.lookup_setting(&name)
            } else {
                connection.lock().lookup_setting(&name)
            }
        })
    });
    // "session" is LOCAL on the settings side (`TryGetCurrentSetting`)
    let (value, scope) = match found {
        Some((value, SettingLookupScope::Local)) => (ffi::into_handle(value), DUCKDB_CONFIG_OPTION_SCOPE_SESSION),
        Some((value, SettingLookupScope::Global)) => (ffi::into_handle(value), DUCKDB_CONFIG_OPTION_SCOPE_GLOBAL),
        None => (std::ptr::null_mut(), DUCKDB_CONFIG_OPTION_SCOPE_INVALID),
    };
    // SAFETY: C passes where to write the scope, or null.
    unsafe { ffi::write(out_scope, scope) };
    value
}
