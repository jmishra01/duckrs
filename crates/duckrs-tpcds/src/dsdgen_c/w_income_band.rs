// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_income_band.cpp @ rust-port-baseline

//! The `income_band` table.

use super::Ctx;
use super::columns::*;
use super::tables::INCOME_BAND;
use crate::append_info::AppendInfo;

/// `mk_w_income_band`
pub(crate) fn mk_w_income_band(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    ctx.null_set(INCOME_BAND, IB_NULLS);
    let ib_income_band_id = index as i32;
    let ib_lower_bound = ctx.dist_member_int("income_band", index as i32, 1);
    let ib_upper_bound = ctx.dist_member_int("income_band", index as i32, 2);

    let info = info_arr.row_start(INCOME_BAND, ctx.tdef(INCOME_BAND).k_null_bit_map);
    info.append_integer(ib_income_band_id, IB_INCOME_BAND_ID);
    info.append_integer(ib_lower_bound, IB_LOWER_BOUND);
    info.append_integer(ib_upper_bound, IB_UPPER_BOUND);
    info.row_end();
}
