// Ported from duckdb/src/main/capi/v1/data_chunk-c.cpp and helper-c.cpp (PrepareCAPIVector) @ rust-port-baseline

//! `duckdb_data_chunk`, `duckdb_vector` and the validity masks C reads and writes through them.
//!
//! A chunk handle owns a `DataChunk`; a vector handle points to a `Vector`, owned by a chunk (or by the vector
//! it is a child of) or, from `duckdb_create_vector`, by the handle itself. C reads and writes the vector's
//! values through the pointer `duckdb_vector_get_data` gives, in the layout of the vector's physical type, which
//! duckrs keeps the same as DuckDB's.

use crate::ffi::{self, Handle};
use crate::types::*;
use duckrs::{DataChunk, LogicalType, LogicalTypeId};
use duckrs_common::types::PhysicalType;
use duckrs_common::vector::{SelectionVector, Vector, VectorType};
use std::ffi::c_char;

impl Handle for _duckdb_data_chunk {
    type Target = DataChunk;
}

impl Handle for _duckdb_vector {
    type Target = Vector;
}

impl Handle for _duckdb_selection_vector {
    type Target = Vec<sel_t>;
}

/// `PrepareCAPIVector`: makes `vector` and what is nested in it flat for its first `count` rows, as C reads every
/// vector as a flat one.
pub(crate) fn prepare_vector(vector: &mut Vector, count: usize) {
    vector.flatten(count);
    match vector.physical_type() {
        PhysicalType::Struct => {
            for child in vector.struct_children_mut() {
                prepare_vector(child, count);
            }
        }
        PhysicalType::List => {
            let size = vector.list_size();
            prepare_vector(vector.list_child_mut(), size);
        }
        PhysicalType::Array => {
            let size = vector.logical_type().array_size().unwrap_or(0) as usize;
            prepare_vector(vector.array_child_mut(), count * size);
        }
        _ => {}
    }
}

pub(crate) fn prepare_chunk(chunk: &mut DataChunk) {
    let count = chunk.len();
    for column in chunk.columns_mut() {
        prepare_vector(column, count);
    }
}

/// `TypeVisitor::Contains(type, INVALID) || Contains(type, ANY)`: a type no vector can hold.
pub(crate) fn contains_invalid_or_any(ty: &LogicalType) -> bool {
    if matches!(ty.id(), LogicalTypeId::Invalid | LogicalTypeId::Any) {
        return true;
    }
    match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Map | LogicalTypeId::Array => {
            ty.child_type().is_some_and(contains_invalid_or_any)
        }
        LogicalTypeId::Struct | LogicalTypeId::Union | LogicalTypeId::Tuple => {
            ty.struct_children().iter().any(|(_, child)| contains_invalid_or_any(child))
        }
        _ => false,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_data_chunk(column_types: *mut duckdb_logical_type, column_count: idx_t) -> duckdb_data_chunk {
    // SAFETY: C passes `column_count` logical types.
    let Some(handles) = (unsafe { ffi::slice(column_types.cast_const(), column_count) }) else {
        return std::ptr::null_mut();
    };
    let mut types = Vec::with_capacity(handles.len());
    for handle in handles {
        // SAFETY: each is a live logical type.
        let Some(ty) = (unsafe { ffi::get(*handle) }) else {
            return std::ptr::null_mut();
        };
        if contains_invalid_or_any(ty) {
            return std::ptr::null_mut();
        }
        types.push(ty.clone());
    }
    ffi::guard(std::ptr::null_mut(), || ffi::into_handle(DataChunk::new(&types)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_data_chunk(chunk: *mut duckdb_data_chunk) {
    // SAFETY: C passes a chunk it got from duckdb.
    drop(unsafe { ffi::take(chunk) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_reset(chunk: duckdb_data_chunk) {
    // SAFETY: C passes a live chunk.
    if let Some(chunk) = unsafe { ffi::get_mut(chunk) } {
        chunk.reset();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t {
    // SAFETY: C passes a live chunk.
    unsafe { ffi::get(chunk) }.map_or(0, |chunk| chunk.column_count() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_get_vector(chunk: duckdb_data_chunk, col_idx: idx_t) -> duckdb_vector {
    // SAFETY: C passes a live chunk.
    let Some(chunk) = (unsafe { ffi::get_mut(chunk) }) else {
        return std::ptr::null_mut();
    };
    if col_idx as usize >= chunk.column_count() {
        return std::ptr::null_mut();
    }
    ffi::borrowed_handle(chunk.column_mut(col_idx as usize))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t {
    // SAFETY: C passes a live chunk.
    unsafe { ffi::get(chunk) }.map_or(0, |chunk| chunk.len() as idx_t)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t) {
    // SAFETY: C passes a live chunk.
    let Some(chunk) = (unsafe { ffi::get_mut(chunk) }) else {
        return;
    };
    // `SetChildCardinality` of a size beyond the chunk's capacity throws, which the C API cannot report
    if size as usize <= chunk.capacity() {
        chunk.set_len(size as usize);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_create_vector(ty: duckdb_logical_type, capacity: idx_t) -> duckdb_vector {
    // SAFETY: C passes a live logical type.
    let Some(ty) = (unsafe { ffi::get(ty) }) else {
        return std::ptr::null_mut();
    };
    if contains_invalid_or_any(ty) {
        return std::ptr::null_mut();
    }
    ffi::guard(std::ptr::null_mut(), || ffi::into_handle(Vector::new(ty.clone(), capacity as usize)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_vector(vector: *mut duckdb_vector) {
    // SAFETY: C passes a vector `duckdb_create_vector` made.
    drop(unsafe { ffi::take(vector) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_get_column_type(vector: duckdb_vector) -> duckdb_logical_type {
    // SAFETY: C passes a live vector.
    unsafe { ffi::get(vector) }.map_or(std::ptr::null_mut(), |v| ffi::into_handle(v.logical_type().clone()))
}

/// A vector C reads as flat or constant: one of a dictionary or a sequence (which C does not make) becomes flat.
fn flat(vector: &mut Vector) -> &mut Vector {
    if matches!(vector.vector_type(), VectorType::Dictionary | VectorType::Sequence) {
        vector.flatten(duckrs_common::STANDARD_VECTOR_SIZE);
    }
    vector
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut std::ffi::c_void {
    // SAFETY: C passes a live vector.
    let Some(vector) = (unsafe { ffi::get_mut(vector) }) else {
        return std::ptr::null_mut();
    };
    ffi::guard(std::ptr::null_mut(), || flat(vector).data_ptr_mut().cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64 {
    // SAFETY: C passes a live vector.
    let Some(vector) = (unsafe { ffi::get_mut(vector) }) else {
        return std::ptr::null_mut();
    };
    match vector.vector_type() {
        VectorType::Flat | VectorType::Constant => vector
            .validity_mut()
            .allocated_entries_mut()
            .map_or(std::ptr::null_mut(), <[u64]>::as_mut_ptr),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_ensure_validity_writable(vector: duckdb_vector) {
    // SAFETY: C passes a live vector.
    if let Some(vector) = unsafe { ffi::get_mut(vector) } {
        ffi::guard((), || {
            flat(vector).validity_mut().entries_mut();
        });
    }
}

/// `StringVector::AddStringOrBlob` into row `index`, unless the vector is VARCHAR and `bytes` are not UTF-8: then the
/// row becomes NULL.
fn assign_string(vector: &mut Vector, index: usize, bytes: &[u8], check_utf8: bool) {
    ffi::guard((), || {
        let vector = flat(vector);
        if !matches!(vector.physical_type(), PhysicalType::Varchar) || index >= vector.capacity() {
            return;
        }
        if check_utf8 && std::str::from_utf8(bytes).is_err() {
            vector.set_null(index, true);
            return;
        }
        let valid = vector.validity().is_valid(index);
        vector.set_str(index, bytes);
        // `set_str` marks the row valid; C keeps the validity it set itself
        if !valid {
            vector.set_null(index, true);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_assign_string_element(vector: duckdb_vector, index: idx_t, text: *const c_char) {
    // SAFETY: C passes a live vector and a NUL-terminated string.
    let (Some(vector), Some(bytes)) = (unsafe { ffi::get_mut(vector) }, unsafe { ffi::c_str(text) }) else {
        return;
    };
    assign_string(vector, index as usize, bytes, true);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_assign_string_element_len(
    vector: duckdb_vector,
    index: idx_t,
    text: *const c_char,
    length: idx_t,
) {
    // SAFETY: C passes a live vector and `length` bytes.
    let (Some(vector), Some(bytes)) = (unsafe { ffi::get_mut(vector) }, unsafe { ffi::bytes(text.cast(), length) }) else {
        return;
    };
    let check = vector.logical_type().id() == LogicalTypeId::Varchar;
    assign_string(vector, index as usize, bytes, check);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_unsafe_vector_assign_string_element_len(
    vector: duckdb_vector,
    index: idx_t,
    text: *const c_char,
    length: idx_t,
) {
    // SAFETY: C passes a live vector and `length` bytes.
    let (Some(vector), Some(bytes)) = (unsafe { ffi::get_mut(vector) }, unsafe { ffi::bytes(text.cast(), length) }) else {
        return;
    };
    assign_string(vector, index as usize, bytes, false);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_list_vector_get_child(vector: duckdb_vector) -> duckdb_vector {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::List => ffi::borrowed_handle(flat(vector).list_child_mut()),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_list_vector_get_size(vector: duckdb_vector) -> idx_t {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::List => flat(vector).list_size() as idx_t,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_list_vector_set_size(vector: duckdb_vector, size: idx_t) -> duckdb_state {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::List => {
            ffi::guard(DuckDBError, || {
                flat(vector).set_list_size(size as usize);
                DuckDBSuccess
            })
        }
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_list_vector_reserve(vector: duckdb_vector, required_capacity: idx_t) -> duckdb_state {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::List => ffi::guard(DuckDBError, || {
            let child = flat(vector).list_child_mut();
            if child.capacity() < required_capacity as usize {
                child.resize((required_capacity as usize).next_power_of_two());
            }
            DuckDBSuccess
        }),
        _ => DuckDBError,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_struct_vector_get_child(vector: duckdb_vector, index: idx_t) -> duckdb_vector {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::Struct => {
            match flat(vector).struct_children_mut().get_mut(index as usize) {
                Some(child) => ffi::borrowed_handle(child),
                None => std::ptr::null_mut(),
            }
        }
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_array_vector_get_child(vector: duckdb_vector) -> duckdb_vector {
    // SAFETY: C passes a live vector.
    match unsafe { ffi::get_mut(vector) } {
        Some(vector) if vector.physical_type() == PhysicalType::Array => ffi::borrowed_handle(flat(vector).array_child_mut()),
        _ => std::ptr::null_mut(),
    }
}

/// The validity entry of `row` C passes a mask for, or `None` for a null mask (every row valid).
///
/// # Safety
///
/// `validity` is null or points to a mask (from `duckdb_vector_get_validity`) with an entry for `row`.
unsafe fn validity_entry<'a>(validity: *mut u64, row: idx_t) -> Option<&'a mut u64> {
    if validity.is_null() {
        return None;
    }
    // SAFETY: by the contract of this function, entry `row / 64` is in the mask C passes.
    unsafe { ffi::reference_mut(validity.add((row / 64) as usize)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool {
    // SAFETY: C passes a validity mask with an entry for `row`.
    unsafe { validity_entry(validity, row) }.is_none_or(|entry| *entry & (1 << (row % 64)) != 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_validity_set_row_validity(validity: *mut u64, row: idx_t, valid: bool) {
    // SAFETY: C passes a validity mask with an entry for `row`.
    if let Some(entry) = unsafe { validity_entry(validity, row) } {
        if valid {
            *entry |= 1 << (row % 64);
        } else {
            *entry &= !(1 << (row % 64));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t) {
    // SAFETY: C passes a validity mask with an entry for `row`.
    unsafe { duckdb_validity_set_row_validity(validity, row, false) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t) {
    // SAFETY: C passes a validity mask with an entry for `row`.
    unsafe { duckdb_validity_set_row_validity(validity, row, true) }
}

#[unsafe(no_mangle)]
pub extern "C" fn duckdb_create_selection_vector(size: idx_t) -> duckdb_selection_vector {
    ffi::into_handle(vec![0 as sel_t; size as usize])
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_selection_vector(sel: duckdb_selection_vector) {
    // SAFETY: C passes a selection vector `duckdb_create_selection_vector` made.
    drop(unsafe { ffi::take(&mut { sel }) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_selection_vector_get_data_ptr(sel: duckdb_selection_vector) -> *mut sel_t {
    // SAFETY: C passes a live selection vector.
    unsafe { ffi::get_mut(sel) }.map_or(std::ptr::null_mut(), |sel| sel.as_mut_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_slice_vector(vector: duckdb_vector, sel: duckdb_selection_vector, len: idx_t) {
    // SAFETY: C passes a live vector and selection vector.
    let (Some(vector), Some(sel)) = (unsafe { ffi::get_mut(vector) }, unsafe { ffi::get(sel) }) else {
        return;
    };
    ffi::guard((), || {
        let selection = SelectionVector::from_vec(sel[..(len as usize).min(sel.len())].to_vec());
        vector.slice(&selection, len as usize);
        vector.flatten(len as usize);
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_reference_vector(to_vector: duckdb_vector, from_vector: duckdb_vector) {
    if to_vector == from_vector {
        return;
    }
    // SAFETY: C passes two live, distinct vectors.
    let (Some(to), Some(from)) = (unsafe { ffi::get_mut(to_vector) }, unsafe { ffi::get(from_vector) }) else {
        return;
    };
    *to = from.clone();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_vector_reference_value(vector: duckdb_vector, value: duckdb_value) {
    // SAFETY: C passes a live vector and value.
    let (Some(vector), Some(value)) = (unsafe { ffi::get_mut(vector) }, unsafe { ffi::get(value) }) else {
        return;
    };
    ffi::guard((), || *vector = Vector::constant(value));
}
