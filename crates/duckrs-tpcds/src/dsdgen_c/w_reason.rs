// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_reason.cpp @ rust-port-baseline

//! The `reason` table.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::tables::REASON;
use crate::append_info::AppendInfo;

/// `mk_w_reason`
pub(crate) fn mk_w_reason(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    ctx.null_set(REASON, R_NULLS);
    let r_reason_sk = index;
    let mut r_reason_id = Vec::with_capacity(16);
    mk_bkey(&mut r_reason_id, index, R_REASON_ID);
    let r_reason_description = ctx.dist_member_str("return_reasons", index as i32, 1);

    let info = info_arr.row_start(REASON, ctx.tdef(REASON).k_null_bit_map);
    info.append_key(r_reason_sk, R_REASON_SK);
    info.append_varchar(&r_reason_id, R_REASON_ID);
    info.append_varchar(r_reason_description, R_REASON_DESCRIPTION);
    info.row_end();
}
