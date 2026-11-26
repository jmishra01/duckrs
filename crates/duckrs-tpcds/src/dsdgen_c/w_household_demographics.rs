// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_household_demographics.cpp @ rust-port-baseline

//! The `household_demographics` table: every combination of the household values, in order.

use super::Ctx;
use super::columns::*;
use super::tables::HOUSEHOLD_DEMOGRAPHICS;
use crate::append_info::AppendInfo;

/// `mk_w_household_demographics`
pub(crate) fn mk_w_household_demographics(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    ctx.null_set(HOUSEHOLD_DEMOGRAPHICS, HD_NULLS);
    let hd_demo_sk = index;
    let mut n_temp = hd_demo_sk;
    let income_bands = i64::from(ctx.distsize("income_band"));
    let hd_income_band_id = (n_temp % income_bands) + 1;
    n_temp /= income_bands;
    let hd_buy_potential = ctx.bitmap_to_dist_str("buy_potential", &mut n_temp, 1);
    let hd_dep_count = ctx.bitmap_to_dist_int("dependent_count", &mut n_temp, 1);
    let hd_vehicle_count = ctx.bitmap_to_dist_int("vehicle_count", &mut n_temp, 1);

    let info = info_arr.row_start(HOUSEHOLD_DEMOGRAPHICS, ctx.tdef(HOUSEHOLD_DEMOGRAPHICS).k_null_bit_map);
    info.append_key(hd_demo_sk, HD_DEMO_SK);
    info.append_key(hd_income_band_id, HD_INCOME_BAND_ID);
    info.append_varchar(hd_buy_potential, HD_BUY_POTENTIAL);
    info.append_integer(hd_dep_count, HD_DEP_COUNT);
    info.append_integer(hd_vehicle_count, HD_VEHICLE_COUNT);
    info.row_end();
}
