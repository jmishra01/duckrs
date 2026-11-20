// Ported from duckdb/src/storage/statistics/distinct_statistics.cpp,
// duckdb/src/include/duckdb/common/types/hyperloglog.hpp, duckdb/src/common/types/hyperloglog.cpp,
// duckdb/src/common/random_engine.cpp and third_party/pcg @ rust-port-baseline
//
//! `DistinctStatistics`: the distinct values of a column as C++ keeps them with its statistics, a
//! HyperLogLog of 64 registers over a sample of the rows, and the Good-Turing estimate of the
//! distinct values of all rows from it. The join order optimizer's estimates start from these, so
//! duckrs works them out the same way, from the chunks of the table in order: C++ updates them
//! as rows are appended, and samples a chunk only when it is a full vector.

use crate::catalog::TableCatalogEntry;
use duckrs_common::hash::hash_vector;
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_common::STANDARD_VECTOR_SIZE;
use std::sync::OnceLock;

/// `pcg32` (`setseq_xsh_rr_64_32`) with its default stream.
struct Pcg32 {
    state: u64,
    increment: u64,
}

impl Pcg32 {
    const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
    const DEFAULT_INCREMENT: u64 = 1_442_695_040_888_963_407;

    /// `engine(state)`: `state_ = bump(state + increment)`.
    fn new(seed: u64) -> Self {
        let mut engine = Self {
            state: 0,
            increment: Self::DEFAULT_INCREMENT,
        };
        engine.state = engine.bump(seed.wrapping_add(engine.increment));
        engine
    }

    fn bump(&self, state: u64) -> u64 {
        state.wrapping_mul(Self::MULTIPLIER).wrapping_add(self.increment)
    }

    /// The output of the state before the step (`output_previous`), `xsh_rr`.
    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = self.bump(old);
        let rotation = (old >> 59) as u32;
        let shifted = (((old >> 18) ^ old) >> 27) as u32;
        shifted.rotate_right(rotation)
    }

    /// `RandomEngine::NextRandomInteger64`.
    fn next_u64(&mut self) -> u64 {
        let high = u64::from(self.next_u32());
        (high << 32) | u64::from(self.next_u32())
    }
}

/// `HyperLogLog` (`HyperLogLogP<6>`).
struct HyperLogLog {
    registers: [u8; Self::M],
}

impl HyperLogLog {
    const P: u32 = 6;
    const Q: usize = 64 - Self::P as usize;
    const M: usize = 1 << Self::P;
    const ALPHA: f64 = 0.721_347_520_444_481_703_680;

    fn new() -> Self {
        Self {
            registers: [0; Self::M],
        }
    }

    /// Algorithm 1.
    fn insert(&mut self, mut hash: u64) {
        let index = (hash & ((1 << Self::P) - 1)) as usize;
        hash >>= Self::P;
        hash |= 1 << Self::Q;
        let zeros = hash.trailing_zeros() as u8 + 1;
        self.registers[index] = self.registers[index].max(zeros);
    }

    /// `HLLSigma`, from redis.
    fn sigma(mut x: f64) -> f64 {
        if x == 1.0 {
            return f64::INFINITY;
        }
        let mut y = 1.0;
        let mut z = x;
        loop {
            x *= x;
            let previous = z;
            z += x * y;
            y += y;
            if previous == z {
                return z;
            }
        }
    }

    /// `HLLTau`, from redis.
    fn tau(mut x: f64) -> f64 {
        if x == 0.0 || x == 1.0 {
            return 0.0;
        }
        let mut y = 1.0;
        let mut z = 1.0 - x;
        loop {
            x = x.sqrt();
            let previous = z;
            y *= 0.5;
            z -= (1.0 - x).powi(2) * y;
            if previous == z {
                return z / 3.0;
            }
        }
    }

    /// `Count`: algorithms 4 and 6.
    fn count(&self) -> u64 {
        let mut counts = [0u32; Self::Q + 2];
        for &register in &self.registers {
            counts[register as usize] += 1;
        }
        let m = Self::M as f64;
        let mut z = m * Self::tau((m - f64::from(counts[Self::Q])) / m);
        for k in (1..=Self::Q).rev() {
            z += f64::from(counts[k]);
            z *= 0.5;
        }
        z += m * Self::sigma(f64::from(counts[0]) / m);
        // `llroundl`
        (Self::ALPHA * m * m / z).round() as u64
    }
}

/// `CreateSampleSelection`: `count` rows of a vector, one at random in each of as many equal parts.
fn sample_selection(count: usize) -> Vec<usize> {
    let mut random = Pcg32::new(0);
    (0..count)
        .map(|i| {
            let begin = i * STANDARD_VECTOR_SIZE / count;
            let end = (i + 1) * STANDARD_VECTOR_SIZE / count;
            begin + (random.next_u64() % (end - begin) as u64) as usize
        })
        .collect()
}

/// `INTEGRAL_SAMPLE_RATE` and `BASE_SAMPLE_RATE`.
const INTEGRAL_SAMPLE_RATE: f64 = 0.3;
const BASE_SAMPLE_RATE: f64 = 0.1;

fn sample_size(integral: bool) -> usize {
    let rate = if integral { INTEGRAL_SAMPLE_RATE } else { BASE_SAMPLE_RATE };
    ((rate * STANDARD_VECTOR_SIZE as f64) as usize).max(1)
}

fn samples(integral: bool) -> &'static [usize] {
    static INTEGRAL: OnceLock<Vec<usize>> = OnceLock::new();
    static OTHER: OnceLock<Vec<usize>> = OnceLock::new();
    let cell = if integral { &INTEGRAL } else { &OTHER };
    cell.get_or_init(|| sample_selection(sample_size(integral)))
}

/// `DistinctStatistics::TypeIsSupported`.
pub fn type_is_supported(ty: &LogicalType) -> bool {
    // a VARIANT is a STRUCT in C++
    ty.id() != duckrs_common::types::LogicalTypeId::Variant
        && !matches!(
            ty.physical_type(),
            PhysicalType::List | PhysicalType::Struct | PhysicalType::Array | PhysicalType::Bit | PhysicalType::Bool
        )
}

/// The distinct values of column `column` of `table` as C++'s statistics would count them, from
/// the chunks it appended (`UpdateSample` for each, then `GetCount`).
pub fn table_column_distinct(table: &TableCatalogEntry, column: usize) -> Option<u64> {
    let ty = &table.columns[column].ty;
    if !type_is_supported(ty) {
        return None;
    }
    let integral = ty.is_integral();
    let mut log = HyperLogLog::new();
    let (mut sample_count, mut total_count) = (0u64, 0u64);
    let mut failed = false;
    table
        .for_each_chunk(column, |_, vector, count| {
            if failed || count == 0 {
                return;
            }
            total_count += count as u64;
            let Ok(hashes) = hash_vector(vector, count) else {
                failed = true;
                return;
            };
            let unified = vector.to_unified(count);
            let hash_data = hashes.to_unified(count);
            let hash_values = hash_data.data::<u64>();
            let mut insert = |row: usize| {
                if unified.is_valid(row) {
                    log.insert(hash_values[hash_data.sel.get(row)]);
                }
            };
            // `UpdateSample`: a chunk that is no full vector has its first rows of the sample's size
            // counted, a full one the rows of the sample
            let wanted = sample_size(integral).min(count);
            if count != STANDARD_VECTOR_SIZE || wanted == count {
                (0..wanted).for_each(&mut insert);
                sample_count += wanted as u64;
            } else {
                samples(integral).iter().take(wanted).copied().for_each(&mut insert);
                sample_count += wanted as u64;
            }
        })
        .ok()?;
    if failed {
        return None;
    }
    Some(distinct_count(&log, sample_count, total_count))
}

/// `DistinctStatistics::GetCount`: the Good-Turing estimate of the distinct values of all rows from
/// those of the sample.
fn distinct_count(log: &HyperLogLog, sample_count: u64, total_count: u64) -> u64 {
    if sample_count == 0 || total_count == 0 {
        return 0;
    }
    let u = log.count().min(sample_count) as f64;
    let s = sample_count as f64;
    let n = total_count as f64;
    // the share of the sampled values taken to occur only once
    let u1 = (u / s).powi(2) * u;
    let estimate = (u + u1 / s * (n - s)) as u64;
    estimate.min(total_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcg32_matches_the_reference_stream() {
        // pcg32 of the C++ library seeded with 0 on its default stream; `random()` after
        // `setseed(-1)` (which seeds 0) gives 0.8060448097461073 in C++, from the same engine
        let mut random = Pcg32::new(0);
        let first: Vec<u32> = (0..3).map(|_| random.next_u32()).collect();
        assert_eq!(first, [3_894_649_422, 2_055_130_073, 2_315_086_854]);
        let seed = Pcg32::new(0).next_u64();
        let drawn = Pcg32::new(seed).next_u64() as f64 * 2f64.powi(-64);
        assert_eq!(drawn, 0.806_044_809_746_107_3);
    }

    #[test]
    fn small_counts_are_close() {
        let mut log = HyperLogLog::new();
        for value in 0..1000u64 {
            log.insert(duckrs_common::hash::murmur64(value));
        }
        let count = log.count();
        assert!((700..1300).contains(&count), "{count}");
    }
}
