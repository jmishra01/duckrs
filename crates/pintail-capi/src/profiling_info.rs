// Ported from duckdb/src/main/capi/v1/profiling_info-c.cpp @ rust-port-baseline

//! The profile of a connection's last statement (`duckdb_profiling_info`): a node of its `QueryProfileResult` tree.
//!
//! As C++'s points into the profiler of the connection, a handle points into the tree the connection keeps of its
//! last profiled statement, which lives until the next statement starts.

use crate::database::ConnectionWrapper;
use crate::ffi::{self, Handle};
use crate::types::*;
use pintail::profiling::{ProfileResult, ProfileResultKind};
use pintail::{LogicalType, Value};
use pintail_common::value::ValueData;
use std::ffi::c_char;

impl Handle for _duckdb_profiling_info {
    type Target = ProfileResult;
}

/// `CountChildren` and `GetChild`: the items of the node's lists (flattened, in order), then its objects.
fn navigable(node: &ProfileResult) -> impl Iterator<Item = &ProfileResult> {
    let items = node
        .children()
        .iter()
        .filter(|c| matches!(c.kind, ProfileResultKind::List(_)))
        .flat_map(|list| list.children().iter());
    let objects = node
        .children()
        .iter()
        .filter(|c| matches!(c.kind, ProfileResultKind::Object(_)));
    items.chain(objects)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_profiling_info(connection: duckdb_connection) -> duckdb_profiling_info {
    // SAFETY: C passes a live connection.
    let Some(ConnectionWrapper { connection, .. }) = (unsafe { ffi::get::<_duckdb_connection>(connection) }) else {
        return std::ptr::null_mut();
    };
    match ffi::guard(None, || connection.lock().profiling_info()) {
        // the connection keeps the tree until its next statement starts
        Some(profile) => ffi::borrowed_const_handle(&*profile),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_profiling_info_get_value(
    info: duckdb_profiling_info,
    key: *const c_char,
) -> duckdb_value {
    // SAFETY: C passes a live profiling info and a NUL-terminated key.
    let (Some(node), Some(key)) = (unsafe { ffi::get(info) }, unsafe { ffi::c_str(key) }) else {
        return std::ptr::null_mut();
    };
    let key = String::from_utf8_lossy(key);
    node.children()
        .iter()
        .find_map(|child| match &child.kind {
            ProfileResultKind::Value(value) if child.key.eq_ignore_ascii_case(&key) => Some(value.clone()),
            _ => None,
        })
        .map_or(std::ptr::null_mut(), ffi::into_handle::<_duckdb_value>)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_profiling_info_get_metrics(info: duckdb_profiling_info) -> duckdb_value {
    // SAFETY: C passes a live profiling info.
    let Some(node) = (unsafe { ffi::get(info) }) else {
        return std::ptr::null_mut();
    };
    // `Value::MAP` of an `InsertionOrderPreservingMap<string>`: each metric's value as text, a key once
    let mut entries: Vec<(String, String)> = Vec::new();
    for child in node.children() {
        if let ProfileResultKind::Value(value) = &child.kind {
            let text = value.try_to_string().unwrap_or_default();
            match entries.iter_mut().find(|(key, _)| *key == child.key) {
                Some(entry) => entry.1 = text,
                None => entries.push((child.key.clone(), text)),
            }
        }
    }
    let ty = LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR);
    let Some(entry_type) = ty.child_type().cloned() else {
        return std::ptr::null_mut();
    };
    let entries: Option<Vec<Value>> = entries
        .into_iter()
        .map(|(key, value)| {
            Value::new(
                entry_type.clone(),
                ValueData::Struct(vec![Value::varchar(key), Value::varchar(value)]),
            )
            .ok()
        })
        .collect();
    match entries.and_then(|entries| Value::new(ty, ValueData::List(entries)).ok()) {
        Some(map) => ffi::into_handle::<_duckdb_value>(map),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_profiling_info_get_child_count(info: duckdb_profiling_info) -> idx_t {
    // SAFETY: C passes a live profiling info.
    unsafe { ffi::get(info) }.map_or(0, |node| navigable(node).count() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_profiling_info_get_child(
    info: duckdb_profiling_info,
    index: idx_t,
) -> duckdb_profiling_info {
    // SAFETY: C passes a live profiling info.
    let Some(node) = (unsafe { ffi::get(info) }) else {
        return std::ptr::null_mut();
    };
    match navigable(node).nth(index as usize) {
        Some(child) => ffi::borrowed_const_handle(child),
        None => std::ptr::null_mut(),
    }
}
