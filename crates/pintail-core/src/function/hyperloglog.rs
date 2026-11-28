// Ported from duckdb/src/include/duckdb/common/types/hyperloglog.hpp, duckdb/src/common/types/hyperloglog.cpp and
// duckdb/extension/core_functions/aggregate/distributive/approx_count.cpp @ rust-port-baseline
//
//! `approx_count_distinct`'s sketch: `HyperLogLogP<10>`, with the estimator of Otmar Ertl's "New cardinality
//! estimation algorithms for HyperLogLog sketches" (arXiv:1702.01284).

/// The registers are indexed by the low `PRECISION` bits of a hash.
pub const PRECISION: usize = 10;
const Q: usize = 64 - PRECISION;
pub const M: usize = 1 << PRECISION;
const ALPHA: f64 = 0.721_347_520_444_481_7;

#[derive(Clone)]
pub struct HyperLogLog {
    pub registers: Vec<u8>,
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self { registers: vec![0; M] }
    }
}

impl HyperLogLog {
    /// Algorithm 1
    pub fn insert(&mut self, hash: u64) {
        let index = (hash & ((1 << PRECISION) - 1)) as usize;
        let rest = (hash >> PRECISION) | (1u64 << Q);
        let zeros = rest.trailing_zeros() as u8 + 1;
        self.registers[index] = self.registers[index].max(zeros);
    }

    /// Algorithm 2
    pub fn merge(&mut self, other: &HyperLogLog) {
        for (register, other) in self.registers.iter_mut().zip(&other.registers) {
            *register = (*register).max(*other);
        }
    }

    /// Algorithms 4 and 6
    pub fn count(&self) -> i64 {
        let mut c = [0u32; Q + 2];
        for &register in &self.registers {
            c[register as usize] += 1;
        }
        let m = M as f64;
        let mut z = m * tau((m - f64::from(c[Q])) / m);
        for k in (1..=Q).rev() {
            z += f64::from(c[k]);
            z *= 0.5;
        }
        z += m * sigma(f64::from(c[0]) / m);
        (ALPHA * m * m / z).round() as i64
    }

    /// The highest value a register can hold: the position of the first set bit of a hash.
    pub const MAX_REGISTER: u8 = (Q + 1) as u8;
}

/// `HyperLogLogBase::HLLSigma`, taken from Redis
fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
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

/// `HyperLogLogBase::HLLTau`, taken from Redis
fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
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
