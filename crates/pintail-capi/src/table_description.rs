// Ported from duckdb/src/main/capi/v1/table_description-c.cpp @ rust-port-baseline

//! `duckdb_table_description` (`TableDescriptionWrapper`): the columns of a table, as `Connection::TableInfo` reads
//! them from the catalog.

use crate::database::ConnectionWrapper;
use crate::error::c_string;
use crate::ffi::{self, Handle};
use crate::types::*;
use pintail::TableDescription;
use std::ffi::{CString, c_char};

/// `TableDescriptionWrapper`: the table, once found, and the last error.
#[derive(Debug, Default)]
pub struct TableDescriptionWrapper {
    description: Option<TableDescription>,
    error: Option<CString>,
}

impl Handle for _duckdb_table_description {
    type Target = TableDescriptionWrapper;
}

impl TableDescriptionWrapper {
    fn set_error(&mut self, message: &str) {
        self.error = Some(c_string(message));
    }

    /// `GetTableDescription`: the table, with `index` among its columns if given; otherwise the error says why not.
    fn table(&mut self, index: Option<idx_t>) -> Option<&TableDescription> {
        let columns = self.description.as_ref()?.columns.len();
        if let Some(index) = index
            && index >= columns as idx_t
        {
            self.set_error(&format!(
                "Column index {index} is out of range, table only has {columns} columns"
            ));
            return None;
        }
        self.description.as_ref()
    }
}

/// `DEFAULT_SCHEMA`: the schema of a table given without one.
const DEFAULT_SCHEMA: &str = "main";

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_create(
    connection: duckdb_connection,
    schema: *const c_char,
    table: *const c_char,
    out: *mut duckdb_table_description,
) -> duckdb_state {
    // SAFETY: as C passes the arguments, with no catalog.
    unsafe { duckdb_table_description_create_ext(connection, std::ptr::null(), schema, table, out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_create_ext(
    connection: duckdb_connection,
    catalog: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    out: *mut duckdb_table_description,
) -> duckdb_state {
    if out.is_null() {
        return DuckDBError;
    }
    let handle: duckdb_table_description = ffi::into_handle(TableDescriptionWrapper::default());
    // SAFETY: C passes where to write the description, which C destroys even on an error; it is not null.
    unsafe { ffi::write(out, handle) };
    // SAFETY: the handle was made just now, and nothing else references it.
    let Some(wrapper) = (unsafe { ffi::get_mut(handle) }) else {
        return DuckDBError;
    };
    // SAFETY: C passes a live connection, or none.
    let connection = unsafe { ffi::get(connection) };
    // SAFETY: C passes NUL-terminated names, or none.
    let (catalog, schema, table) = unsafe { (ffi::c_str(catalog), ffi::c_str(schema), ffi::c_str(table)) };
    let (Some(connection), Some(table)) = (connection, table) else {
        return DuckDBError;
    };
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let catalog = catalog.map(text);
    let schema = schema.map_or_else(|| DEFAULT_SCHEMA.to_string(), text);
    let table = text(table);
    ffi::guard(DuckDBError, || {
        describe(wrapper, connection, catalog.as_deref(), &schema, &table)
    })
}

/// `Connection::TableInfo` into the wrapper.
fn describe(
    wrapper: &mut TableDescriptionWrapper,
    connection: &ConnectionWrapper,
    catalog: Option<&str>,
    schema: &str,
    table: &str,
) -> duckdb_state {
    match connection.connection.lock().table_info(catalog, Some(schema), table) {
        Err(error) => {
            wrapper.set_error(&error.message);
            DuckDBError
        }
        Ok(None) => {
            wrapper.set_error("No table with that schema+name could be located");
            DuckDBError
        }
        Ok(Some(description)) => {
            wrapper.description = Some(description);
            DuckDBSuccess
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_destroy(table_description: *mut duckdb_table_description) {
    // SAFETY: C passes a description `duckdb_table_description_create` made.
    drop(unsafe { ffi::take(table_description) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_error(table_description: duckdb_table_description) -> *const c_char {
    // SAFETY: C passes a live description, or none.
    let Some(wrapper) = (unsafe { ffi::get(table_description) }) else {
        return std::ptr::null();
    };
    wrapper.error.as_ref().map_or(std::ptr::null(), |error| error.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_has_default(
    table_description: duckdb_table_description,
    index: idx_t,
    out: *mut bool,
) -> duckdb_state {
    // SAFETY: C passes a live description, or none.
    let Some(wrapper) = (unsafe { ffi::get_mut(table_description) }) else {
        return DuckDBError;
    };
    let Some(table) = wrapper.table(Some(index)) else {
        return DuckDBError;
    };
    let has_default = table.columns[index as usize].has_default;
    // SAFETY: C passes where to write the answer, or none.
    if !unsafe { ffi::write(out, has_default) } {
        wrapper.set_error("Please provide a valid (non-null) 'out' variable");
        return DuckDBError;
    }
    DuckDBSuccess
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_get_column_count(
    table_description: duckdb_table_description,
) -> idx_t {
    // SAFETY: C passes a live description, or none.
    let Some(wrapper) = (unsafe { ffi::get_mut(table_description) }) else {
        return 0;
    };
    wrapper.table(None).map_or(0, |table| table.columns.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_get_column_name(
    table_description: duckdb_table_description,
    index: idx_t,
) -> *mut c_char {
    // SAFETY: C passes a live description, or none.
    let Some(wrapper) = (unsafe { ffi::get_mut(table_description) }) else {
        return std::ptr::null_mut();
    };
    wrapper.table(Some(index)).map_or(std::ptr::null_mut(), |table| {
        ffi::strdup(&table.columns[index as usize].name)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_table_description_get_column_type(
    table_description: duckdb_table_description,
    index: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes a live description, or none.
    let Some(wrapper) = (unsafe { ffi::get_mut(table_description) }) else {
        return std::ptr::null_mut();
    };
    wrapper.table(Some(index)).map_or(std::ptr::null_mut(), |table| {
        ffi::into_handle(table.columns[index as usize].ty.clone())
    })
}
