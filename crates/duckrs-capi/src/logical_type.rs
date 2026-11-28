// Ported from duckdb/src/main/capi/v1/logical_types-c.cpp @ rust-port-baseline

//! `duckdb_logical_type`: a handle to a `LogicalType`.

use crate::ffi::{self, Handle};
use crate::helper::{type_id_from_c, type_id_to_c};
use crate::types::*;
use crate::vector::contains_invalid_or_any;
use duckrs::{LogicalType, LogicalTypeId};
use duckrs_common::types::PhysicalType;
use std::ffi::c_char;

impl Handle for _duckdb_logical_type {
    type Target = LogicalType;
}

fn new_type(ty: LogicalType) -> duckdb_logical_type {
    ffi::into_handle(ty)
}

/// The type behind `ty` when its id is `id` (`AssertLogicalTypeId`).
///
/// # Safety
///
/// `ty` is null or a live logical type.
unsafe fn type_with_id<'a>(ty: duckdb_logical_type, id: LogicalTypeId) -> Option<&'a LogicalType> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get(ty) }.filter(|ty| ty.id() == id)
}

/// The type behind `ty` when it is stored as a STRUCT (`AssertInternalType(STRUCT)`): a STRUCT or a UNION.
///
/// # Safety
///
/// `ty` is null or a live logical type.
unsafe fn struct_like<'a>(ty: duckdb_logical_type) -> Option<&'a LogicalType> {
    // SAFETY: by the contract of this function.
    unsafe { ffi::get(ty) }.filter(|ty| ty.physical_type() == PhysicalType::Struct)
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_logical_type(ty: duckdb_type) -> duckdb_logical_type {
    let id = match ty {
        // types with parameters have constructors of their own
        DUCKDB_TYPE_DECIMAL | DUCKDB_TYPE_ENUM | DUCKDB_TYPE_LIST | DUCKDB_TYPE_STRUCT | DUCKDB_TYPE_MAP
        | DUCKDB_TYPE_ARRAY | DUCKDB_TYPE_UNION => LogicalTypeId::Invalid,
        _ => type_id_from_c(ty).unwrap_or(LogicalTypeId::Invalid),
    };
    new_type(LogicalType::new(id))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_list_type(ty: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    unsafe { ffi::get(ty) }.map_or(std::ptr::null_mut(), |child| new_type(LogicalType::list(child.clone())))
}

/// `ArrayType::MAX_ARRAY_SIZE`.
const MAX_ARRAY_SIZE: idx_t = 100_000;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_array_type(ty: duckdb_logical_type, array_size: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    let Some(child) = (unsafe { ffi::get(ty) }) else {
        return std::ptr::null_mut();
    };
    if array_size >= MAX_ARRAY_SIZE {
        return std::ptr::null_mut();
    }
    new_type(LogicalType::array(child.clone(), array_size as u32))
}

/// The `count` names and types of the members C passes.
///
/// # Safety
///
/// `types` and `names` are null or arrays of `count` logical types and NUL-terminated names.
unsafe fn members(types: *mut duckdb_logical_type, names: *mut *const c_char, count: idx_t) -> Option<Vec<(String, LogicalType)>> {
    // SAFETY: by the contract of this function.
    let types = unsafe { ffi::slice(types.cast_const(), count) }?;
    // SAFETY: by the contract of this function.
    let names = unsafe { ffi::slice(names.cast_const(), count) }?;
    let mut members = Vec::with_capacity(types.len());
    for (ty, name) in types.iter().zip(names) {
        // SAFETY: each type is live and each name NUL-terminated, by the contract of this function.
        let (ty, name) = (unsafe { ffi::get(*ty) }?, unsafe { ffi::c_str(*name) }?);
        members.push((String::from_utf8_lossy(name).into_owned(), ty.clone()));
    }
    Some(members)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_union_type(
    member_types: *mut duckdb_logical_type,
    member_names: *mut *const c_char,
    member_count: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes `member_count` types and names.
    unsafe { members(member_types, member_names, member_count) }
        .map_or(std::ptr::null_mut(), |members| new_type(LogicalType::union_type(members)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_struct_type(
    member_types: *mut duckdb_logical_type,
    member_names: *mut *const c_char,
    member_count: idx_t,
) -> duckdb_logical_type {
    // SAFETY: C passes `member_count` types and names.
    let Some(members) = (unsafe { members(member_types, member_names, member_count) }) else {
        return std::ptr::null_mut();
    };
    // a struct whose members have no names is a TUPLE
    if !members.is_empty() && members.iter().all(|(name, _)| name.is_empty()) {
        return new_type(LogicalType::tuple(members.into_iter().map(|(_, ty)| ty).collect()));
    }
    new_type(LogicalType::struct_type(members))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_enum_type(member_names: *mut *const c_char, member_count: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes `member_count` names.
    let Some(names) = (unsafe { ffi::slice(member_names.cast_const(), member_count) }) else {
        return std::ptr::null_mut();
    };
    let mut values = Vec::with_capacity(names.len());
    for name in names {
        // SAFETY: each name is NUL-terminated.
        let Some(name) = (unsafe { ffi::c_str(*name) }) else {
            return std::ptr::null_mut();
        };
        values.push(String::from_utf8_lossy(name).into_owned());
    }
    new_type(LogicalType::enum_type(values))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_map_type(key_type: duckdb_logical_type, value_type: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes two live logical types.
    let (Some(key), Some(value)) = (unsafe { ffi::get(key_type) }, unsafe { ffi::get(value_type) }) else {
        return std::ptr::null_mut();
    };
    new_type(LogicalType::map(key.clone(), value.clone()))
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_decimal_type(width: u8, scale: u8) -> duckdb_logical_type {
    // `Decimal::IsValidWidthScale`
    if !(1..=38).contains(&width) || scale > width {
        return std::ptr::null_mut();
    }
    new_type(LogicalType::decimal(width, scale))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_get_type_id(ty: duckdb_logical_type) -> duckdb_type {
    // SAFETY: C passes a live logical type.
    unsafe { ffi::get(ty) }.map_or(DUCKDB_TYPE_INVALID, |ty| type_id_to_c(ty.id()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_logical_type(ty: *mut duckdb_logical_type) {
    // SAFETY: C passes a logical type it owns.
    drop(unsafe { ffi::take(ty) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_decimal_width(ty: duckdb_logical_type) -> u8 {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Decimal) }.and_then(LogicalType::decimal_properties).map_or(0, |(w, _)| w)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_decimal_scale(ty: duckdb_logical_type) -> u8 {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Decimal) }.and_then(LogicalType::decimal_properties).map_or(0, |(_, s)| s)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_decimal_internal_type(ty: duckdb_logical_type) -> duckdb_type {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Decimal) }.map(LogicalType::physical_type) {
        Some(PhysicalType::Int16) => DUCKDB_TYPE_SMALLINT,
        Some(PhysicalType::Int32) => DUCKDB_TYPE_INTEGER,
        Some(PhysicalType::Int64) => DUCKDB_TYPE_BIGINT,
        Some(PhysicalType::Int128) => DUCKDB_TYPE_HUGEINT,
        _ => DUCKDB_TYPE_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_enum_internal_type(ty: duckdb_logical_type) -> duckdb_type {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Enum) }.map(LogicalType::physical_type) {
        Some(PhysicalType::UInt8) => DUCKDB_TYPE_UTINYINT,
        Some(PhysicalType::UInt16) => DUCKDB_TYPE_USMALLINT,
        Some(PhysicalType::UInt32) => DUCKDB_TYPE_UINTEGER,
        _ => DUCKDB_TYPE_INVALID,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_enum_dictionary_size(ty: duckdb_logical_type) -> u32 {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Enum) }.map_or(0, |ty| ty.enum_values().len() as u32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_enum_dictionary_value(ty: duckdb_logical_type, index: idx_t) -> *mut c_char {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Enum) }.and_then(|ty| ty.enum_values().get(index as usize)) {
        Some(value) => ffi::strdup(value),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_list_type_child_type(ty: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    match unsafe { ffi::get(ty) } {
        Some(ty) if matches!(ty.id(), LogicalTypeId::List | LogicalTypeId::Map) => {
            ty.child_type().map_or(std::ptr::null_mut(), |child| new_type(child.clone()))
        }
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_array_type_child_type(ty: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Array) }
        .and_then(LogicalType::child_type)
        .map_or(std::ptr::null_mut(), |child| new_type(child.clone()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_array_type_array_size(ty: duckdb_logical_type) -> idx_t {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Array) }
        .and_then(LogicalType::array_size)
        .map_or(0, idx_t::from)
}

/// `MapType::KeyType` (`0`) or `ValueType` (`1`).
///
/// # Safety
///
/// `ty` is null or a live logical type.
unsafe fn map_member(ty: duckdb_logical_type, member: usize) -> duckdb_logical_type {
    // SAFETY: by the contract of this function.
    unsafe { type_with_id(ty, LogicalTypeId::Map) }
        .and_then(LogicalType::child_type)
        .and_then(|entry| entry.struct_children().get(member))
        .map_or(std::ptr::null_mut(), |(_, ty)| new_type(ty.clone()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_map_type_key_type(ty: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    unsafe { map_member(ty, 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_map_type_value_type(ty: duckdb_logical_type) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    unsafe { map_member(ty, 1) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_struct_type_child_count(ty: duckdb_logical_type) -> idx_t {
    // SAFETY: C passes a live logical type.
    unsafe { struct_like(ty) }.map_or(0, |ty| ty.struct_children().len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_struct_type_child_name(ty: duckdb_logical_type, index: idx_t) -> *mut c_char {
    // SAFETY: C passes a live logical type.
    match unsafe { struct_like(ty) }.and_then(|ty| ty.struct_children().get(index as usize)) {
        Some((name, _)) => ffi::strdup(name),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_struct_type_child_type(ty: duckdb_logical_type, index: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    match unsafe { struct_like(ty) }.and_then(|ty| ty.struct_children().get(index as usize)) {
        Some((_, child)) => new_type(child.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_union_type_member_count(ty: duckdb_logical_type) -> idx_t {
    // SAFETY: C passes a live logical type.
    unsafe { type_with_id(ty, LogicalTypeId::Union) }.map_or(0, |ty| ty.struct_children().len().saturating_sub(1) as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_union_type_member_name(ty: duckdb_logical_type, index: idx_t) -> *mut c_char {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Union) }.and_then(|ty| ty.struct_children().get(index as usize + 1)) {
        Some((name, _)) => ffi::strdup(name),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_union_type_member_type(ty: duckdb_logical_type, index: idx_t) -> duckdb_logical_type {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Union) }.and_then(|ty| ty.struct_children().get(index as usize + 1)) {
        Some((_, member)) => new_type(member.clone()),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_logical_type_get_alias(ty: duckdb_logical_type) -> *mut c_char {
    // SAFETY: C passes a live logical type.
    match unsafe { ffi::get(ty) }.and_then(LogicalType::alias) {
        Some(alias) => ffi::strdup(alias),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_logical_type_set_alias(ty: duckdb_logical_type, alias: *const c_char) {
    // SAFETY: C passes a live logical type it owns, and a NUL-terminated alias.
    let (Some(ty), Some(alias)) = (unsafe { ffi::get_mut(ty) }, unsafe { ffi::c_str(alias) }) else {
        return;
    };
    *ty = ty.with_alias(String::from_utf8_lossy(alias));
}

/// Whether `ty` holds a type no value can have; [`contains_invalid_or_any`] for the other modules.
pub(crate) fn is_unusable(ty: &LogicalType) -> bool {
    contains_invalid_or_any(ty)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_geometry_type_get_crs(ty: duckdb_logical_type) -> *mut c_char {
    // SAFETY: C passes a live logical type.
    match unsafe { type_with_id(ty, LogicalTypeId::Geometry) }.and_then(LogicalType::crs) {
        // `GeoType::GetCRS(type).GetDefinition()`
        Some(crs) => ffi::strdup(&crs.definition),
        None => std::ptr::null_mut(),
    }
}
