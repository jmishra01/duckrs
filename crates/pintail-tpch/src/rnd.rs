// Ported from duckdb/extension/tpch/dbgen/rnd.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// Park & Miller's "Minimum Standard" RNG (CACM, Oct 1988, pp 1192-1201), plus the per-row
// bookkeeping that keeps every stream on a known per-row boundary.

use crate::dss::{DBGenContext, DssHuge, MAX_LONG, MAX_STREAM, ORDER, ORDER_LINE, PART, PART_PSUPP, SET_SEEDS, Seed};
use crate::speed_seed::nth_element;
use std::sync::OnceLock;

/// `dss_random` (the `RANDOM` macro): a uniform integer in `[lower, upper]`.
#[inline]
pub fn dss_random(lower: DssHuge, upper: DssHuge, seed: &mut Seed) -> DssHuge {
    let result = unif_int(lower, upper, seed);
    seed.usage += 1;
    result
}

fn seed_table(t: usize) -> usize {
    match t {
        ORDER_LINE => ORDER,
        PART_PSUPP => PART,
        _ => t,
    }
}

const TABLE_COUNT: usize = 10;

/// The streams of each table (`TableStreams`); the seed table and the child relation are constants
/// of the generator.
struct TableStreams {
    count: usize,
    stream: [usize; MAX_STREAM + 1],
}

fn table_streams(t: usize, ctx: &DBGenContext) -> &'static TableStreams {
    static STREAMS: OnceLock<Vec<TableStreams>> = OnceLock::new();
    let streams = STREAMS.get_or_init(|| {
        (0..TABLE_COUNT)
            .map(|table| {
                let mut result = TableStreams {
                    count: 0,
                    stream: [0; MAX_STREAM + 1],
                };
                for (i, seed) in ctx.seed.iter().enumerate() {
                    // seed_matches_table
                    if seed.table == table as i64 || seed.table == ctx.tdefs[table].child {
                        result.stream[result.count] = i;
                        result.count += 1;
                    }
                }
                result
            })
            .collect()
    });
    &streams[t]
}

/// `row_start`
pub fn row_start(t: usize, ctx: &mut DBGenContext) {
    let streams = table_streams(seed_table(t), ctx);
    for &s in &streams.stream[..streams.count] {
        ctx.seed[s].usage = 0;
    }
}

/// `row_stop_h`: advances every stream of the table to its per-row boundary.
pub fn row_stop_h(t: usize, ctx: &mut DBGenContext) {
    let streams = table_streams(seed_table(t), ctx);
    for &s in &streams.stream[..streams.count] {
        let seed = &mut ctx.seed[s];
        if SET_SEEDS && seed.usage > seed.boundary {
            seed.boundary = seed.usage;
        } else {
            let advance_count = seed.boundary - seed.usage;
            if advance_count > 0 {
                nth_element(advance_count, &mut seed.value);
            }
        }
    }
}

/// `NextRand`: the next number of the sequence, in `1 .. 2^31 - 2`.
#[inline]
pub fn next_rand(n_seed: DssHuge) -> DssHuge {
    (n_seed * 16807) % 2147483647
}

/// `UnifInt`: an integer uniformly distributed between `n_low` and `n_high`, both included.
#[inline]
pub fn unif_int(n_low: DssHuge, n_high: DssHuge, seed: &mut Seed) -> DssHuge {
    let d_range = if n_high == MAX_LONG && n_low == 0 {
        // the reference dbgen overflows int32 here; replicate it to stay bit-compatible
        f64::from(i32::MIN)
    } else {
        (n_high - n_low + 1) as f64
    };
    seed.value = next_rand(seed.value);
    let n_temp = ((seed.value as f64 / DBGenContext::DM) * d_range) as DssHuge;
    n_low + n_temp
}
