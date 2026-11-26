// Ported from duckdb/extension/tpch/dbgen/permute.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.

use crate::bm_utils::Distribution;
use crate::dss::{DBGenContext, Seed};
use crate::rnd::dss_random;

/// `permute`: Fisher-Yates from the front. Position i is final after iteration i, so only the first
/// `needed` positions are permuted; the seed is advanced to its per-row boundary by `row_stop_h` as
/// if all draws were made.
pub fn permute(a: &mut [i64], needed: usize, seed: &mut Seed) {
    let c = a.len() as i64;
    for i in 0..needed {
        let source = dss_random(i as i64, c - 1, seed) as usize;
        a.swap(source, i);
    }
}

/// `permute_dist`: permutes the member indexes of `d` into `ctx.permute`, with stream `sd`.
pub fn permute_dist(d: &Distribution, needed: usize, sd: usize, ctx: &mut DBGenContext) {
    ctx.permute.clear();
    ctx.permute.extend(0..d.count as i64);
    permute(&mut ctx.permute, needed, &mut ctx.seed[sd]);
}
