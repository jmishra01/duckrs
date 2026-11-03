//! Executor throughput on one 2048-row vector.

use criterion::{Criterion, criterion_group, criterion_main};
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::executor::{binary, binary_nullable, select};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use std::hint::black_box;

const N: usize = STANDARD_VECTOR_SIZE;

fn int_vector(nulls_every: Option<usize>) -> Vector {
    let mut v = Vector::new(LogicalType::INTEGER, N);
    for (i, x) in v.data_mut::<i32>().iter_mut().enumerate() {
        *x = i as i32;
    }
    if let Some(step) = nulls_every {
        for i in (0..N).step_by(step) {
            v.set_null(i, true);
        }
    }
    v
}

fn bench(c: &mut Criterion) {
    let left = int_vector(None);
    let right = int_vector(None);
    let with_nulls = int_vector(Some(10));
    let constant = Vector::constant(&Value::integer(3));
    let mut result = Vector::new(LogicalType::INTEGER, N);

    c.bench_function("binary flat+flat: a + b", |b| {
        b.iter(|| binary(&left, &right, &mut result, N, |a: i32, b: i32| a.wrapping_add(b)))
    });
    c.bench_function("binary flat+constant: a + 3", |b| {
        b.iter(|| binary(&left, &constant, &mut result, N, |a: i32, b: i32| a.wrapping_add(b)))
    });
    c.bench_function("binary flat+flat, 10% NULL", |b| {
        b.iter(|| binary(&with_nulls, &right, &mut result, N, |a: i32, b: i32| a.wrapping_add(b)))
    });
    c.bench_function("binary_nullable checked add", |b| {
        b.iter(|| binary_nullable(&left, &right, &mut result, N, |a: i32, b: i32| a.checked_add(b)))
    });
    c.bench_function("select a > 1000", |b| {
        b.iter(|| black_box(select(&left, N, |a: i32| a > 1000)))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
