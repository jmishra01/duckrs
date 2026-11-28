// Ported from duckdb/src/storage/statistics/distinct_statistics.cpp,
// duckdb/src/include/duckdb/common/types/hyperloglog.hpp, duckdb/src/common/types/hyperloglog.cpp,
// duckdb/src/common/random_engine.cpp and third_party/pcg @ rust-port-baseline
//
//! `DistinctStatistics`: the distinct values of a column as C++ keeps them with its statistics, a
//! HyperLogLog of 64 registers over a sample of the rows, and the Good-Turing estimate of the
//! distinct values of all rows from it. The join order optimizer's estimates start from these, so
//! pintail works them out the same way, from the chunks of the table in order: C++ updates them
//! as rows are appended, and samples a chunk only when it is a full vector.

use crate::catalog::TableCatalogEntry;
use pintail_common::STANDARD_VECTOR_SIZE;
use pintail_common::column_data::ColumnDataCollection;
use pintail_common::hash::hash_vector;
use pintail_common::serializer::BinarySerializer;
use pintail_common::types::{LogicalType, PhysicalType};
use pintail_common::vector::Vector;
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
#[derive(Debug, Clone)]
struct HyperLogLog {
    registers: [u8; Self::M],
}

impl HyperLogLog {
    const P: u32 = 6;
    const Q: usize = 64 - Self::P as usize;
    const M: usize = 1 << Self::P;
    const ALPHA: f64 = 0.721_347_520_444_481_7;

    fn new() -> Self {
        Self {
            registers: [0; Self::M],
        }
    }

    /// `Merge`: each register the larger of the two.
    fn merge(&mut self, other: &Self) {
        for (register, other) in self.registers.iter_mut().zip(other.registers) {
            *register = (*register).max(other);
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
    let rate = if integral {
        INTEGRAL_SAMPLE_RATE
    } else {
        BASE_SAMPLE_RATE
    };
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
    ty.id() != pintail_common::types::LogicalTypeId::Variant
        && !matches!(
            ty.physical_type(),
            PhysicalType::List | PhysicalType::Struct | PhysicalType::Array | PhysicalType::Bit | PhysicalType::Bool
        )
}

/// `HLLStorageType::HLL_V2`, the registers as they are.
pub const HLL_V2: u8 = 2;
/// `HLLStorageType::HLL_V1`, the dense HyperLogLog of redis that older files have.
pub const HLL_V1: u8 = 1;

/// `HLLV1::ToNew`: the registers of the dense redis HyperLogLog `data` (its `hllhdr`, then 4096 registers of 6 bits
/// from the byte after `registers`), each new one the largest of as many old ones in turn. `None` if `data` is not
/// one.
pub fn registers_of_v1(data: &[u8]) -> Option<Vec<u8>> {
    const HEADER: usize = 16;
    const REGISTERS: usize = 1 << 12;
    const BITS: usize = 6;
    const HLL_DENSE: u8 = 0;
    let registers = data.get(HEADER + 1..)?;
    if !data.starts_with(b"HYLL") || data[4] != HLL_DENSE || registers.len() * 8 < REGISTERS * BITS {
        return None;
    }
    // `HLL_DENSE_GET_REGISTER`
    let register = |index: usize| -> u8 {
        let (byte, bit) = (index * BITS / 8, index * BITS % 8);
        let low = u16::from(registers[byte]);
        let high = u16::from(registers.get(byte + 1).copied().unwrap_or(0));
        (((low >> bit) | (high << (8 - bit))) & ((1 << BITS) - 1)) as u8
    };
    let per = REGISTERS / HyperLogLog::M;
    Some(
        (0..HyperLogLog::M)
            .map(|i| (0..per).map(|j| register(i * per + j)).max().unwrap_or(0))
            .collect(),
    )
}

/// `DistinctStatistics`: a HyperLogLog of a sample of a column's rows, and how many rows it saw and sampled. A
/// database file keeps one per column, which C++ updates as rows are appended.
#[derive(Debug, Clone)]
pub struct DistinctState {
    log: HyperLogLog,
    sample_count: u64,
    total_count: u64,
}

impl Default for DistinctState {
    fn default() -> Self {
        Self {
            log: HyperLogLog::new(),
            sample_count: 0,
            total_count: 0,
        }
    }
}

impl DistinctState {
    /// The state a file has (`HLL_V2`): `None` if a register can't be one (`Invalid HyperLogLog register value`).
    pub fn stored(registers: &[u8], sample_count: u64, total_count: u64) -> Option<Self> {
        let registers: [u8; HyperLogLog::M] = registers.try_into().ok()?;
        if registers.iter().any(|&register| register as usize > HyperLogLog::Q + 1) {
            return None;
        }
        Some(Self {
            log: HyperLogLog { registers },
            sample_count,
            total_count,
        })
    }

    /// `UpdateSample` of `count` rows of `vector` of type `ty`: false if they can't be hashed.
    pub fn update(&mut self, ty: &LogicalType, vector: &Vector, count: usize) -> bool {
        if count == 0 {
            return true;
        }
        let integral = ty.is_integral();
        self.total_count += count as u64;
        let Ok(hashes) = hash_vector(vector, count) else {
            return false;
        };
        let unified = vector.to_unified(count);
        let hash_data = hashes.to_unified(count);
        let hash_values = hash_data.data::<u64>();
        let log = &mut self.log;
        let mut insert = |row: usize| {
            if unified.is_valid(row) {
                log.insert(hash_values[hash_data.sel.get(row)]);
            }
        };
        // a chunk that is no full vector has its first rows of the sample's size counted, a full one the rows of
        // the sample
        let wanted = sample_size(integral).min(count);
        if count != STANDARD_VECTOR_SIZE || wanted == count {
            (0..wanted).for_each(&mut insert);
        } else {
            samples(integral).iter().take(wanted).copied().for_each(&mut insert);
        }
        self.sample_count += wanted as u64;
        true
    }

    /// `Merge`.
    pub fn merge(&mut self, other: &Self) {
        self.log.merge(&other.log);
        self.sample_count += other.sample_count;
        self.total_count += other.total_count;
    }

    /// `DistinctStatistics::Serialize`, with its log as `HLL_V2`.
    pub fn write(&self, s: &mut BinarySerializer) {
        s.write_property_with_default(100, "sample_count", &self.sample_count);
        s.write_property_with_default(101, "total_count", &self.total_count);
        s.write_nullable_object(102, "log", |s| {
            s.write_property(100, "type", &HLL_V2);
            s.write_property(101, "data", &self.log.registers[..]);
        });
    }

    /// `GetCount`.
    pub fn count(&self) -> u64 {
        distinct_count(&self.log, self.sample_count, self.total_count)
    }
}

/// The distinct values of column `column` of `table` as C++'s statistics would count them, from
/// the chunks it appended (`UpdateSample` for each, then `GetCount`).
pub fn table_column_distinct(table: &TableCatalogEntry, column: usize) -> Option<u64> {
    let ty = &table.columns[column].ty;
    if !type_is_supported(ty) {
        return None;
    }
    let mut state = DistinctState::default();
    let mut failed = false;
    table
        .for_each_chunk(column, |_, vector, count| {
            failed = failed || !state.update(ty, vector, count);
        })
        .ok()?;
    if failed {
        return None;
    }
    Some(state.count())
}

/// [`DistinctState`] of column `column` of `rows`, from their chunks, if its type has one.
pub fn rows_distinct(rows: &ColumnDataCollection, column: usize, ty: &LogicalType) -> Option<DistinctState> {
    if !type_is_supported(ty) {
        return None;
    }
    let mut state = DistinctState::default();
    for chunk in rows.chunks() {
        if !state.update(ty, chunk.column(column), chunk.len()) {
            return None;
        }
    }
    Some(state)
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

    /// A dense redis HyperLogLog (`HLLV1`) with `set` registers of the given values, the others zero.
    fn redis_log(set: &[(usize, u8)]) -> Vec<u8> {
        let mut data = vec![0u8; 17 + 4096 * 6 / 8];
        data[..4].copy_from_slice(b"HYLL");
        for &(index, value) in set {
            // `HLL_DENSE_SET_REGISTER`, from the byte after `registers`
            let (byte, bit) = (17 + index * 6 / 8, index * 6 % 8);
            data[byte] |= value << bit;
            if bit > 2 {
                data[byte + 1] |= value >> (8 - bit);
            }
        }
        data
    }

    #[test]
    fn v1_registers_are_the_largest_of_each_run() {
        // 64 old registers make a new one: 0..64 the first, 64..128 the second
        let data = redis_log(&[(3, 5), (40, 9), (64, 2), (127, 17), (4095, 63)]);
        let registers = registers_of_v1(&data).unwrap();
        assert_eq!(registers.len(), 64);
        assert_eq!(&registers[..3], &[9, 17, 0]);
        assert_eq!(registers[63], 63);
        assert!(registers[2..63].iter().all(|&register| register == 0));
        // what is not a dense redis log is not read
        assert_eq!(registers_of_v1(&data[..100]), None);
        let mut sparse = data.clone();
        sparse[4] = 1;
        assert_eq!(registers_of_v1(&sparse), None);
    }

    #[test]
    fn stored_states_count_and_merge_as_counted_ones() {
        let mut counted = DistinctState::default();
        let mut log = HyperLogLog::new();
        for value in 0..5000u64 {
            log.insert(pintail_common::hash::murmur64(value));
        }
        counted.log = log.clone();
        (counted.sample_count, counted.total_count) = (5000, 20000);
        let stored = DistinctState::stored(&log.registers, 5000, 20000).unwrap();
        assert_eq!(stored.count(), counted.count());
        // a register past `Q + 1` can't be one
        let mut bad = log.registers;
        bad[0] = HyperLogLog::Q as u8 + 2;
        assert!(DistinctState::stored(&bad, 1, 1).is_none());
        assert!(DistinctState::stored(&bad[..10], 1, 1).is_none());
        // `Merge` takes the larger register and adds the counts
        let mut merged = DistinctState::default();
        merged.merge(&stored);
        merged.merge(&stored);
        assert_eq!(merged.log.registers, log.registers);
        assert_eq!((merged.sample_count, merged.total_count), (10000, 40000));
    }

    #[test]
    fn small_counts_are_close() {
        let mut log = HyperLogLog::new();
        for value in 0..1000u64 {
            log.insert(pintail_common::hash::murmur64(value));
        }
        let count = log.count();
        assert!((700..1300).contains(&count), "{count}");
    }
}
