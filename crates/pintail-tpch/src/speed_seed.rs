// Ported from duckdb/extension/tpch/dbgen/speed_seed.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// Skipping ahead in the random number streams, so a child of a parallel load starts where the
// rows before it would have left the seeds. `fake_tpch_a_rnd` and the 64-bit `ADVANCE_STREAM64`
// are not ported: the C++ generator never calls them.

use crate::dss::*;

/// `advanceStream(seed, num_calls, 0)` (the `ADVANCE_STREAM` macro)
fn advance_stream(seed: &mut Seed, n_calls: DssHuge) {
    nth_element(n_calls, &mut seed.value);
}

const MULTIPLIER: DssHuge = 16807;
const MODULUS: DssHuge = 2147483647;

/// `Multiplier^N % Modulus` for the small advances that happen on every row (`row_stop_h`).
const SMALL_POWERS: [DssHuge; 256] = {
    let mut powers = [0; 256];
    powers[0] = 1;
    let mut i = 1;
    while i < powers.len() {
        powers[i] = (powers[i - 1] * MULTIPLIER) % MODULUS;
        i += 1;
    }
    powers
};

/// `NthElement`: advances `start_seed` by `n` applications of the generator, using
/// `X_n = [(Mult ** n) X_0] mod Modulus`.
pub fn nth_element(mut n: DssHuge, start_seed: &mut DssHuge) {
    let mut z = *start_seed;
    if (0..SMALL_POWERS.len() as DssHuge).contains(&n) {
        *start_seed = (SMALL_POWERS[n as usize] * z) % MODULUS;
        return;
    }
    let mut mult = MULTIPLIER;
    while n > 0 {
        if n % 2 != 0 {
            z = (mult * z) % MODULUS;
        }
        n /= 2;
        mult = (mult * mult) % MODULUS;
    }
    *start_seed = z;
}

/// `sd_part`
pub fn sd_part(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    for i in P_MFG_SD..=P_CNTR_SD {
        advance_stream(&mut ctx.seed[i], skip_count);
    }
    advance_stream(&mut ctx.seed[P_CMNT_SD], skip_count * 2);
    advance_stream(&mut ctx.seed[P_NAME_SD], skip_count * 92);
}

/// `sd_line`
pub fn sd_line(child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    for _ in 0..O_LCNT_MAX {
        for i in L_QTY_SD..=L_RFLG_SD {
            advance_stream(&mut ctx.seed[i], skip_count);
        }
        advance_stream(&mut ctx.seed[L_CMNT_SD], skip_count * 2);
    }
    // need to special case this as the link between master and detail
    if child == 1 {
        advance_stream(&mut ctx.seed[O_ODATE_SD], skip_count);
        advance_stream(&mut ctx.seed[O_LCNT_SD], skip_count);
    }
}

/// `sd_order`
pub fn sd_order(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    advance_stream(&mut ctx.seed[O_LCNT_SD], skip_count);
    advance_stream(&mut ctx.seed[O_CKEY_SD], skip_count);
    advance_stream(&mut ctx.seed[O_CMNT_SD], skip_count * 2);
    advance_stream(&mut ctx.seed[O_SUPP_SD], skip_count);
    advance_stream(&mut ctx.seed[O_CLRK_SD], skip_count);
    advance_stream(&mut ctx.seed[O_PRIO_SD], skip_count);
    advance_stream(&mut ctx.seed[O_ODATE_SD], skip_count);
}

/// `sd_psupp`
pub fn sd_psupp(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    for _ in 0..SUPP_PER_PART {
        advance_stream(&mut ctx.seed[PS_QTY_SD], skip_count);
        advance_stream(&mut ctx.seed[PS_SCST_SD], skip_count);
        advance_stream(&mut ctx.seed[PS_CMNT_SD], skip_count * 2);
    }
}

/// `sd_cust`
pub fn sd_cust(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    advance_stream(&mut ctx.seed[C_ADDR_SD], skip_count * 9);
    advance_stream(&mut ctx.seed[C_CMNT_SD], skip_count * 2);
    advance_stream(&mut ctx.seed[C_NTRG_SD], skip_count);
    advance_stream(&mut ctx.seed[C_PHNE_SD], 3 * skip_count);
    advance_stream(&mut ctx.seed[C_ABAL_SD], skip_count);
    advance_stream(&mut ctx.seed[C_MSEG_SD], skip_count);
}

/// `sd_supp`
pub fn sd_supp(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    advance_stream(&mut ctx.seed[S_NTRG_SD], skip_count);
    advance_stream(&mut ctx.seed[S_PHNE_SD], 3 * skip_count);
    advance_stream(&mut ctx.seed[S_ABAL_SD], skip_count);
    advance_stream(&mut ctx.seed[S_ADDR_SD], skip_count * 9);
    advance_stream(&mut ctx.seed[S_CMNT_SD], skip_count * 2);
    advance_stream(&mut ctx.seed[BBB_CMNT_SD], skip_count);
    advance_stream(&mut ctx.seed[BBB_JNK_SD], skip_count);
    advance_stream(&mut ctx.seed[BBB_OFFSET_SD], skip_count);
    // avoid one trudge
    advance_stream(&mut ctx.seed[BBB_TYPE_SD], skip_count);
}

/// `sd_nation`
pub fn sd_nation(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    advance_stream(&mut ctx.seed[N_CMNT_SD], skip_count * 2);
}

/// `sd_region`
pub fn sd_region(_child: i32, skip_count: DssHuge, ctx: &mut DBGenContext) {
    advance_stream(&mut ctx.seed[R_CMNT_SD], skip_count * 2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rnd::next_rand;

    #[test]
    fn nth_element_matches_stepping() {
        for n in [0, 1, 5, 255, 256, 257, 1000] {
            let mut stepped = 12345;
            for _ in 0..n {
                stepped = next_rand(stepped);
            }
            let mut jumped = 12345;
            nth_element(n, &mut jumped);
            assert_eq!(stepped, jumped, "n = {n}");
        }
    }
}
