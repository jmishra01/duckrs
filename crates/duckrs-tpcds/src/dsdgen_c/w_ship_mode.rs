// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_ship_mode.cpp @ rust-port-baseline

//! The `ship_mode` table.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::{ALPHANUM, RS_SM_CONTRACT};
use super::tables::SHIP_MODE;
use crate::append_info::AppendInfo;

/// `mk_w_ship_mode`
pub(crate) fn mk_w_ship_mode(ctx: &mut Ctx, info_arr: &mut AppendInfo, k_index: i64) {
    ctx.null_set(SHIP_MODE, SM_NULLS);
    let sm_ship_mode_sk = k_index;
    let mut sm_ship_mode_id = Vec::with_capacity(16);
    mk_bkey(&mut sm_ship_mode_id, k_index, SM_SHIP_MODE_ID);
    let mut n_temp = k_index;
    let sm_type = ctx.bitmap_to_dist_str("ship_mode_type", &mut n_temp, 1);
    let sm_code = ctx.bitmap_to_dist_str("ship_mode_code", &mut n_temp, 1);
    let sm_carrier = ctx.dist_member_str("ship_mode_carrier", k_index as i32, 1);
    let mut sm_contract = Vec::with_capacity(RS_SM_CONTRACT as usize);
    ctx.gen_charset(&mut sm_contract, ALPHANUM, 1, RS_SM_CONTRACT, SM_CONTRACT);

    let info = info_arr.row_start(SHIP_MODE, ctx.tdef(SHIP_MODE).k_null_bit_map);
    info.append_key(sm_ship_mode_sk, SM_SHIP_MODE_SK);
    info.append_varchar(&sm_ship_mode_id, SM_SHIP_MODE_ID);
    info.append_varchar(sm_type, SM_TYPE);
    info.append_varchar(sm_code, SM_CODE);
    info.append_varchar(sm_carrier, SM_CARRIER);
    info.append_varchar(&sm_contract, SM_CONTRACT);
    info.row_end();
}
