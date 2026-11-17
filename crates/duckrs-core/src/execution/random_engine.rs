// Ported from duckdb/src/common/random_engine.cpp and third_party/pcg/pcg_random.hpp
// @ rust-port-baseline
//
//! `RandomEngine`: the `pcg32` generator C++ uses (`setseq_xsh_rr_64_32` with its default stream),
//! so that a seeded sample picks the rows C++ picks.

const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const INCREMENT: u64 = 1_442_695_040_888_963_407;

#[derive(Debug, Clone)]
pub struct RandomEngine {
    state: u64,
}

impl RandomEngine {
    /// `RandomEngine(seed)`: a seed below 0 takes one from the clock.
    pub fn new(seed: i64) -> Self {
        let seed = if seed < 0 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
        } else {
            seed as u64
        };
        let mut engine = Self { state: 0 };
        engine.set_seed(seed);
        engine
    }

    /// `SetSeed`: `pcg.seed(seed)`, the state the engine starts in is `bump(seed + increment)`.
    pub fn set_seed(&mut self, seed: u64) {
        self.state = seed
            .wrapping_add(INCREMENT)
            .wrapping_mul(MULTIPLIER)
            .wrapping_add(INCREMENT);
    }

    /// `NextRandomInteger`: the output of the state before it advances (XSH RR).
    pub fn next_random_integer(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rotation = (old >> 59) as u32;
        xorshifted.rotate_right(rotation)
    }

    /// `NextRandomInteger64`.
    pub fn next_random_integer64(&mut self) -> u64 {
        let high = u64::from(self.next_random_integer());
        let low = u64::from(self.next_random_integer());
        (high << 32) | low
    }

    /// `NextRandom`: in [0, 1).
    pub fn next_random(&mut self) -> f64 {
        self.next_random_integer64() as f64 * (-64f64).exp2()
    }
}
