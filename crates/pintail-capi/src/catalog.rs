// Ported from duckdb/src/main/capi/v1/catalog-c.cpp @ rust-port-baseline

//! The catalogs a client context sees (`duckdb_catalog`) and their entries (`duckdb_catalog_entry`).

use crate::database::context_of;
use crate::ffi::{self, Handle};
use crate::function_info::ClientContextWrapper;
use crate::types::*;
use pintail::CatalogEntryKind;
use std::ffi::{CString, c_char};

/// `CCatalogWrapper`: a catalog by its name, of the database of the context it came from.
pub struct CatalogWrapper {
    name: String,
    database: pintail::WeakDatabase,
    /// `Catalog::GetCatalogType`: pintail's catalogs are DuckDB's own
    catalog_type: CString,
}

/// `CCatalogEntryWrapper`: what C reads of an entry.
pub struct CatalogEntryWrapper {
    kind: CatalogEntryKind,
    name: CString,
}

impl Handle for _duckdb_catalog {
    type Target = CatalogWrapper;
}

impl Handle for _duckdb_catalog_entry {
    type Target = CatalogEntryWrapper;
}

/// `CatalogTypeFromC`.
fn kind_from_c(ty: duckdb_catalog_entry_type) -> Option<CatalogEntryKind> {
    Some(match ty {
        DUCKDB_CATALOG_ENTRY_TYPE_TABLE => CatalogEntryKind::Table,
        DUCKDB_CATALOG_ENTRY_TYPE_SCHEMA => CatalogEntryKind::Schema,
        DUCKDB_CATALOG_ENTRY_TYPE_VIEW => CatalogEntryKind::View,
        DUCKDB_CATALOG_ENTRY_TYPE_INDEX => CatalogEntryKind::Index,
        DUCKDB_CATALOG_ENTRY_TYPE_PREPARED_STATEMENT => CatalogEntryKind::PreparedStatement,
        DUCKDB_CATALOG_ENTRY_TYPE_SEQUENCE => CatalogEntryKind::Sequence,
        DUCKDB_CATALOG_ENTRY_TYPE_COLLATION => CatalogEntryKind::Collation,
        DUCKDB_CATALOG_ENTRY_TYPE_TYPE => CatalogEntryKind::Type,
        DUCKDB_CATALOG_ENTRY_TYPE_DATABASE => CatalogEntryKind::Database,
        _ => return None,
    })
}

/// `CatalogTypeToC`.
fn kind_to_c(kind: CatalogEntryKind) -> duckdb_catalog_entry_type {
    match kind {
        CatalogEntryKind::Table => DUCKDB_CATALOG_ENTRY_TYPE_TABLE,
        CatalogEntryKind::Schema => DUCKDB_CATALOG_ENTRY_TYPE_SCHEMA,
        CatalogEntryKind::View => DUCKDB_CATALOG_ENTRY_TYPE_VIEW,
        CatalogEntryKind::Index => DUCKDB_CATALOG_ENTRY_TYPE_INDEX,
        CatalogEntryKind::PreparedStatement => DUCKDB_CATALOG_ENTRY_TYPE_PREPARED_STATEMENT,
        CatalogEntryKind::Sequence => DUCKDB_CATALOG_ENTRY_TYPE_SEQUENCE,
        CatalogEntryKind::Collation => DUCKDB_CATALOG_ENTRY_TYPE_COLLATION,
        CatalogEntryKind::Type => DUCKDB_CATALOG_ENTRY_TYPE_TYPE,
        CatalogEntryKind::Database => DUCKDB_CATALOG_ENTRY_TYPE_DATABASE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_client_context_get_catalog(
    context: duckdb_client_context,
    name: *const c_char,
) -> duckdb_catalog {
    // SAFETY: C passes a live client context and a NUL-terminated name.
    let (Some(ClientContextWrapper { connection_id }), Some(name)) =
        (unsafe { ffi::get::<_duckdb_client_context>(context) }, unsafe {
            ffi::c_str(name)
        })
    else {
        return std::ptr::null_mut();
    };
    if name.is_empty() {
        return std::ptr::null_mut();
    }
    let Some((database, connection)) = context_of(*connection_id) else {
        return std::ptr::null_mut();
    };
    // `HasActiveTransaction`: a context is in a transaction while a query runs on it, which is when its callbacks
    // ask
    if !connection.is_busy() {
        return std::ptr::null_mut();
    }
    let name = String::from_utf8_lossy(name).into_owned();
    let found = database
        .upgrade()
        .is_some_and(|database| ffi::guard(false, || database.has_catalog(&name)));
    if !found {
        return std::ptr::null_mut();
    }
    ffi::into_handle(CatalogWrapper {
        name,
        database,
        catalog_type: CString::from(c"duckdb"),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_catalog(catalog: *mut duckdb_catalog) {
    // SAFETY: C passes a catalog `duckdb_client_context_get_catalog` made.
    drop(unsafe { ffi::take(catalog) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_catalog_get_type_name(catalog: duckdb_catalog) -> *const c_char {
    // SAFETY: C passes a live catalog.
    unsafe { ffi::get(catalog) }.map_or(std::ptr::null(), |catalog| catalog.catalog_type.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_catalog_get_entry(
    catalog: duckdb_catalog,
    context: duckdb_client_context,
    entry_type: duckdb_catalog_entry_type,
    schema_name: *const c_char,
    entry_name: *const c_char,
) -> duckdb_catalog_entry {
    // SAFETY: C passes a live catalog, a live client context and NUL-terminated names.
    let (Some(catalog), Some(_), Some(schema), Some(name)) = (
        unsafe { ffi::get(catalog) },
        unsafe { ffi::get::<_duckdb_client_context>(context) },
        unsafe { ffi::c_str(schema_name) },
        unsafe { ffi::c_str(entry_name) },
    ) else {
        return std::ptr::null_mut();
    };
    let Some(kind) = kind_from_c(entry_type) else {
        return std::ptr::null_mut();
    };
    let (schema, name) = (String::from_utf8_lossy(schema), String::from_utf8_lossy(name));
    let Some(database) = catalog.database.upgrade() else {
        return std::ptr::null_mut();
    };
    match ffi::guard(None, || database.catalog_entry(&catalog.name, kind, &schema, &name)) {
        Some((kind, name)) => ffi::into_handle(CatalogEntryWrapper {
            kind,
            name: crate::error::c_string(&name),
        }),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_catalog_entry_get_type(entry: duckdb_catalog_entry) -> duckdb_catalog_entry_type {
    // SAFETY: C passes a live catalog entry.
    unsafe { ffi::get(entry) }.map_or(DUCKDB_CATALOG_ENTRY_TYPE_INVALID, |entry| kind_to_c(entry.kind))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_catalog_entry_get_name(entry: duckdb_catalog_entry) -> *const c_char {
    // SAFETY: C passes a live catalog entry.
    unsafe { ffi::get(entry) }.map_or(std::ptr::null(), |entry| entry.name.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_catalog_entry(entry: *mut duckdb_catalog_entry) {
    // SAFETY: C passes an entry `duckdb_catalog_get_entry` made.
    drop(unsafe { ffi::take(entry) });
}
