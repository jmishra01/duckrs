// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/permute.cpp @ rust-port-baseline

//! Random permutations.

use super::Ctx;
use super::constants::DIST_UNIFORM;

impl Ctx {
    /// `makePermutation`: a random order of `0..n_size`.
    pub(crate) fn make_permutation(&mut self, n_size: i32, n_stream: i32) -> Vec<i32> {
        if n_size <= 0 {
            return Vec::new();
        }
        let mut set: Vec<i32> = (0..n_size).collect();
        for i in 0..n_size as usize {
            let n_index = self.genrand_integer(DIST_UNIFORM, 0, n_size - 1, 0, n_stream) as usize;
            set.swap(i, n_index);
        }
        set
    }
}

/// `getPermutationEntry`: entry `n_index` (counting from 1) of a permutation, counting from 1.
pub(crate) fn get_permutation_entry(permutation: &[i32], n_index: i32) -> i64 {
    i64::from(permutation[(n_index - 1) as usize] + 1)
}
