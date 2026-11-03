// Ported from duckdb/src/include/duckdb/common/vector_operations/{unary,binary,scalar}_executor.hpp
// @ rust-port-baseline
//
// Executors apply a scalar function to every row of one or two vectors, handling the vector
// representations (constant inputs give a constant result; flat inputs take a tight loop over
// 64-row validity entries; anything else goes through the unified format) and NULLs (a NULL
// input gives a NULL output without calling the function).
//
// Each executor has three flavors, matching the C++ wrappers:
// - plain: `f(a) -> r`
// - nullable: `f(a) -> Option<r>`, where `None` makes the row NULL (`ADDS_NULLS`)
// - fallible: `f(a) -> Result<Option<r>>`, which stops at the first error (casts that throw)

use crate::Result;
use crate::types::NativeType;
use crate::vector::{SelectionVector, ValidityMask, Vector, VectorType};
use std::convert::Infallible;

const ENTRY: usize = 64;

/// Runs `op(i)` for every valid row `i < count` of `validity`, visiting 64-row entries at a
/// time so all-valid and all-NULL entries skip the per-row bit test (`ExecuteFlat`).
#[inline(always)]
fn for_each_valid<E>(
    validity: &ValidityMask,
    count: usize,
    mut op: impl FnMut(usize) -> Result<(), E>,
) -> Result<(), E> {
    let Some(entries) = validity.entries() else {
        for i in 0..count {
            op(i)?;
        }
        return Ok(());
    };
    for (entry_idx, &entry) in entries.iter().enumerate().take(count.div_ceil(ENTRY)) {
        let start = entry_idx * ENTRY;
        let end = (start + ENTRY).min(count);
        if entry == u64::MAX {
            for i in start..end {
                op(i)?;
            }
        } else if entry != 0 {
            for i in start..end {
                if entry & (1 << (i - start)) != 0 {
                    op(i)?;
                }
            }
        }
    }
    Ok(())
}

fn prepare_constant(result: &mut Vector) {
    result.prepare_flat(1);
    result.set_constant();
}

/// The core unary loop. `f` returns `Ok(None)` to produce NULL.
fn unary_core<A, R, E>(
    input: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A) -> Result<Option<R>, E>,
) -> Result<(), E>
where
    A: NativeType,
    R: NativeType,
{
    match input.vector_type() {
        VectorType::Constant => {
            prepare_constant(result);
            if input.is_constant_null() {
                result.set_null(0, true);
                return Ok(());
            }
            match f(input.data::<A>()[0])? {
                Some(r) => result.data_mut::<R>()[0] = r,
                None => result.set_null(0, true),
            }
            Ok(())
        }
        VectorType::Flat => {
            result.prepare_flat(count);
            *result.validity_mut() = input.validity().clone();
            let a = input.data::<A>();
            let mut nulls = Vec::new();
            {
                let out = result.data_mut::<R>();
                for_each_valid(input.validity(), count, |i| {
                    match f(a[i])? {
                        Some(r) => out[i] = r,
                        None => nulls.push(i),
                    }
                    Ok(())
                })?;
            }
            nulls.into_iter().for_each(|i| result.set_null(i, true));
            Ok(())
        }
        VectorType::Dictionary | VectorType::Sequence => {
            result.prepare_flat(count);
            let u = input.to_unified(count);
            let a = u.data::<A>();
            let mut nulls = Vec::new();
            {
                let out = result.data_mut::<R>();
                for (i, slot) in out.iter_mut().enumerate().take(count) {
                    let idx = u.sel.get(i);
                    if !u.validity.is_valid(idx) {
                        nulls.push(i);
                        continue;
                    }
                    match f(a[idx])? {
                        Some(r) => *slot = r,
                        None => nulls.push(i),
                    }
                }
            }
            nulls.into_iter().for_each(|i| result.set_null(i, true));
            Ok(())
        }
    }
}

/// `result[i] = f(input[i])` (`UnaryExecutor::Execute`).
pub fn unary<A: NativeType, R: NativeType>(
    input: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A) -> R,
) {
    let never: Result<(), Infallible> = unary_core(input, result, count, |a| Ok(Some(f(a))));
    let Ok(()) = never;
}

/// Like [`unary`], but `f` may return `None` to make a row NULL.
pub fn unary_nullable<A: NativeType, R: NativeType>(
    input: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A) -> Option<R>,
) {
    let never: Result<(), Infallible> = unary_core(input, result, count, |a| Ok(f(a)));
    let Ok(()) = never;
}

/// Like [`unary_nullable`], but `f` may fail; the first error stops execution.
pub fn unary_fallible<A: NativeType, R: NativeType>(
    input: &Vector,
    result: &mut Vector,
    count: usize,
    f: impl FnMut(A) -> Result<Option<R>>,
) -> Result<()> {
    unary_core(input, result, count, f)
}

/// Intersection of two validity masks over `count` rows.
fn and_validity(a: &ValidityMask, b: &ValidityMask, count: usize) -> ValidityMask {
    let mut m = a.slice(0, count);
    m.and_with(b);
    m
}

/// The core binary loop. `f` returns `Ok(None)` to produce NULL.
fn binary_core<A, B, R, E>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A, B) -> Result<Option<R>, E>,
) -> Result<(), E>
where
    A: NativeType,
    B: NativeType,
    R: NativeType,
{
    use VectorType::{Constant, Flat};
    let (lt, rt) = (left.vector_type(), right.vector_type());
    if lt == Constant && rt == Constant {
        prepare_constant(result);
        if left.is_constant_null() || right.is_constant_null() {
            result.set_null(0, true);
            return Ok(());
        }
        match f(left.data::<A>()[0], right.data::<B>()[0])? {
            Some(r) => result.data_mut::<R>()[0] = r,
            None => result.set_null(0, true),
        }
        return Ok(());
    }
    // A constant NULL on either side makes every row NULL (`ExecuteConstant`/`ExecuteFlat`).
    if (lt == Constant && left.is_constant_null()) || (rt == Constant && right.is_constant_null()) {
        prepare_constant(result);
        result.set_null(0, true);
        return Ok(());
    }
    result.prepare_flat(count);
    let mut nulls = Vec::new();
    if matches!((lt, rt), (Flat | Constant, Flat | Constant)) {
        let validity = match (lt, rt) {
            (Flat, Flat) => and_validity(left.validity(), right.validity(), count),
            (Flat, _) => left.validity().slice(0, count),
            _ => right.validity().slice(0, count),
        };
        let (a, b) = (left.data::<A>(), right.data::<B>());
        {
            let out = result.data_mut::<R>();
            // Specialize on which side is constant so the hot loop has no per-row branching.
            match (lt, rt) {
                (Flat, Flat) => for_each_valid(&validity, count, |i| {
                    match f(a[i], b[i])? {
                        Some(r) => out[i] = r,
                        None => nulls.push(i),
                    }
                    Ok(())
                })?,
                (Flat, _) => {
                    let b0 = b[0];
                    for_each_valid(&validity, count, |i| {
                        match f(a[i], b0)? {
                            Some(r) => out[i] = r,
                            None => nulls.push(i),
                        }
                        Ok(())
                    })?
                }
                _ => {
                    let a0 = a[0];
                    for_each_valid(&validity, count, |i| {
                        match f(a0, b[i])? {
                            Some(r) => out[i] = r,
                            None => nulls.push(i),
                        }
                        Ok(())
                    })?
                }
            }
        }
        *result.validity_mut() = validity;
    } else {
        let (lu, ru) = (left.to_unified(count), right.to_unified(count));
        let (a, b) = (lu.data::<A>(), ru.data::<B>());
        let out = result.data_mut::<R>();
        for (i, slot) in out.iter_mut().enumerate().take(count) {
            let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
            if !lu.validity.is_valid(li) || !ru.validity.is_valid(ri) {
                nulls.push(i);
                continue;
            }
            match f(a[li], b[ri])? {
                Some(r) => *slot = r,
                None => nulls.push(i),
            }
        }
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
    Ok(())
}

/// `result[i] = f(left[i], right[i])` (`BinaryExecutor::Execute`).
pub fn binary<A: NativeType, B: NativeType, R: NativeType>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A, B) -> R,
) {
    let never: Result<(), Infallible> = binary_core(left, right, result, count, |a, b| Ok(Some(f(a, b))));
    let Ok(()) = never;
}

/// Like [`binary`], but `f` may return `None` to make a row NULL.
pub fn binary_nullable<A: NativeType, B: NativeType, R: NativeType>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A, B) -> Option<R>,
) {
    let never: Result<(), Infallible> = binary_core(left, right, result, count, |a, b| Ok(f(a, b)));
    let Ok(()) = never;
}

/// Like [`binary_nullable`], but `f` may fail (e.g. overflow); the first error stops execution.
pub fn binary_fallible<A: NativeType, B: NativeType, R: NativeType>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    f: impl FnMut(A, B) -> Result<Option<R>>,
) -> Result<()> {
    binary_core(left, right, result, count, f)
}

/// Applies `f` to the string at every row, producing `R` (e.g. `length(s)`).
pub fn unary_str<R: NativeType>(input: &Vector, result: &mut Vector, count: usize, mut f: impl FnMut(&[u8]) -> R) {
    let u = input.to_unified(count);
    let constant = input.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    if constant {
        prepare_constant(result);
    } else {
        result.prepare_flat(count);
    }
    let mut nulls = Vec::new();
    {
        let out = result.data_mut::<R>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            if u.is_valid(i) {
                *slot = f(u.str_at(i));
            } else {
                nulls.push(i);
            }
        }
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
}

/// Applies `f` to every row, writing a string result. `f` appends the output bytes to the
/// buffer it is given (e.g. `upper(s)`, or casts to VARCHAR).
pub fn unary_to_str<A: NativeType>(
    input: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(A, &mut Vec<u8>),
) {
    let u = input.to_unified(count);
    let a = u.data::<A>();
    write_strings(input.vector_type() == VectorType::Constant, result, count, |i, buf| {
        if !u.is_valid(i) {
            return false;
        }
        f(a[u.sel.get(i)], buf);
        true
    });
}

/// Applies `f` to the string at every row, writing a string result.
pub fn unary_str_to_str(input: &Vector, result: &mut Vector, count: usize, mut f: impl FnMut(&[u8], &mut Vec<u8>)) {
    let u = input.to_unified(count);
    write_strings(input.vector_type() == VectorType::Constant, result, count, |i, buf| {
        if !u.is_valid(i) {
            return false;
        }
        f(u.str_at(i), buf);
        true
    });
}

/// Shared driver for string-producing executors: `produce(i, buf)` fills `buf` with row `i`'s
/// bytes and returns false for NULL.
fn write_strings(
    constant: bool,
    result: &mut Vector,
    count: usize,
    mut produce: impl FnMut(usize, &mut Vec<u8>) -> bool,
) {
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows);
    let mut buf = Vec::new();
    for i in 0..rows {
        buf.clear();
        if produce(i, &mut buf) {
            result.set_str(i, &buf);
        } else {
            result.set_null(i, true);
        }
    }
    if constant {
        result.set_constant();
    }
}

/// Applies `f` to the strings of both inputs, producing `R` (e.g. string comparisons).
pub fn binary_str<R: NativeType>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    mut f: impl FnMut(&[u8], &[u8]) -> R,
) {
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    let constant = left.vector_type() == VectorType::Constant && right.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    if constant {
        prepare_constant(result);
    } else {
        result.prepare_flat(count);
    }
    let mut nulls = Vec::new();
    {
        let out = result.data_mut::<R>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            if lu.is_valid(i) && ru.is_valid(i) {
                *slot = f(lu.str_at(i), ru.str_at(i));
            } else {
                nulls.push(i);
            }
        }
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
}

/// The rows of `input[0..count]` for which `pred` holds (NULLs never match), as a selection
/// and its length (`UnaryExecutor::Select`). Filters use this to slice their chunk.
pub fn select<A: NativeType>(
    input: &Vector,
    count: usize,
    mut pred: impl FnMut(A) -> bool,
) -> (SelectionVector, usize) {
    let mut sel = vec![0u32; count];
    let mut n = 0;
    if input.vector_type() == VectorType::Flat && input.validity().all_valid() {
        // Branchless: always write the index, advance only on a match.
        for (i, &a) in input.data::<A>()[..count].iter().enumerate() {
            sel[n] = i as u32;
            n += usize::from(pred(a));
        }
    } else {
        let u = input.to_unified(count);
        let a = u.data::<A>();
        for i in 0..count {
            let idx = u.sel.get(i);
            sel[n] = i as u32;
            n += usize::from(u.validity.is_valid(idx) && pred(a[idx]));
        }
    }
    sel.truncate(n);
    (SelectionVector::from_vec(sel), n)
}

/// Like [`select`], over two inputs (`BinaryExecutor::Select`).
pub fn select_binary<A: NativeType, B: NativeType>(
    left: &Vector,
    right: &Vector,
    count: usize,
    mut pred: impl FnMut(A, B) -> bool,
) -> (SelectionVector, usize) {
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    let (a, b) = (lu.data::<A>(), ru.data::<B>());
    let mut sel = Vec::with_capacity(count);
    for i in 0..count {
        let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
        if lu.validity.is_valid(li) && ru.validity.is_valid(ri) && pred(a[li], b[ri]) {
            sel.push(i as u32);
        }
    }
    let n = sel.len();
    (SelectionVector::from_vec(sel), n)
}

#[cfg(test)]
mod tests;
