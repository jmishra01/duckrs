// Ported from duckdb/extension/tpch/dbgen/rng64.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// The 64-bit generator dbgen uses for o_custkey and l_partkey at scale factors of 30000 and up.
// `AdvanceRand64` is not ported: the C++ only reaches it through `ADVANCE_STREAM64`, whose uses in
// speed_seed.cpp are commented out.

use crate::dss::{DssHuge, RNG_A, RNG_C, Seed};

/// `dss_random64` (the `RANDOM64` macro)
pub fn dss_random64(mut n_low: DssHuge, mut n_high: DssHuge, seed: &mut Seed) -> DssHuge {
    if n_low > n_high {
        std::mem::swap(&mut n_low, &mut n_high);
    }
    seed.value = next_rand64(seed.value);
    let mut n_temp = seed.value;
    if n_temp < 0 {
        n_temp = n_temp.wrapping_neg();
    }
    n_temp %= n_high - n_low + 1;
    seed.usage += 1;
    n_low + n_temp
}

/// `NextRand64`
pub fn next_rand64(n_seed: DssHuge) -> DssHuge {
    // implicitly truncated to 64 bits
    (n_seed as u64).wrapping_mul(RNG_A).wrapping_add(RNG_C) as DssHuge
}
