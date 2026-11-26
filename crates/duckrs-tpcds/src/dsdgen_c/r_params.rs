// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/r_params.cpp @ rust-port-baseline

//! The generator's parameters. The C code keeps them as strings in `params[]`, looked up by name
//! (`get_int("PARALLEL")`, ...); the port keeps the handful the generator reads as typed fields,
//! with the defaults of `params.h`.

use super::constants::RNG_SEED;

#[derive(Clone)]
pub(crate) struct Params {
    /// `get_dbl("SCALE")`: the scale factor as `atof` reads back the string it was set from.
    pub(crate) scale: f64,
    /// `get_int("PARALLEL")`, and whether it was set (`is_set("PARALLEL")`).
    pub(crate) parallel: i32,
    pub(crate) parallel_set: bool,
    /// `get_int("CHILD")`
    pub(crate) child: i32,
    /// `get_int("RNGSEED")`; `is_set("RNGSEED")` is always true, as it has a default.
    pub(crate) rng_seed: i64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            scale: 1.0,
            parallel: 0,
            parallel_set: false,
            child: 1,
            rng_seed: RNG_SEED,
        }
    }
}

/// C's `atoi`: optional leading white space and sign, then as many digits as there are.
pub(crate) fn atoi(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || (b'\t'..=b'\r').contains(&s[i])) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        negative = s[i] == b'-';
        i += 1;
    }
    let mut value: i32 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        value = value.wrapping_mul(10).wrapping_add(i32::from(s[i] - b'0'));
        i += 1;
    }
    if negative { value.wrapping_neg() } else { value }
}
