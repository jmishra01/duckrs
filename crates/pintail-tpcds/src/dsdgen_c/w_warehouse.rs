// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_warehouse.cpp @ rust-port-baseline

//! The `warehouse` table.

use super::Ctx;
use super::address::DsAddr;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::*;
use super::tables::WAREHOUSE;
use crate::append_info::AppendInfo;

/// `g_w_warehouse`
#[derive(Default)]
pub(crate) struct State {
    w_warehouse_id: Vec<u8>,
    w_warehouse_name: Vec<u8>,
    w_address: DsAddr,
    sz_temp: Vec<u8>,
}

/// `mk_w_warehouse`
pub(crate) fn mk_w_warehouse(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.warehouse);
    ctx.null_set(WAREHOUSE, W_NULLS);
    let w_warehouse_sk = index;
    mk_bkey(&mut st.w_warehouse_id, index, W_WAREHOUSE_ID);
    ctx.gen_text(
        &mut st.w_warehouse_name,
        W_NAME_MIN,
        RS_W_WAREHOUSE_NAME,
        W_WAREHOUSE_NAME,
    );
    let w_warehouse_sq_ft = ctx.genrand_integer(DIST_UNIFORM, W_SQFT_MIN, W_SQFT_MAX, 0, W_WAREHOUSE_SQ_FT);
    ctx.mk_address(&mut st.w_address, W_WAREHOUSE_ADDRESS);

    let (a, sz_temp) = (&st.w_address, &mut st.sz_temp);
    let info = info_arr.row_start(WAREHOUSE, ctx.tdef(WAREHOUSE).k_null_bit_map);
    info.append_key(w_warehouse_sk, W_WAREHOUSE_SK);
    info.append_varchar(&st.w_warehouse_id, W_WAREHOUSE_ID);
    info.append_varchar(&st.w_warehouse_name, W_WAREHOUSE_NAME);
    info.append_integer(w_warehouse_sq_ft, W_WAREHOUSE_SQ_FT);
    info.append_integer(a.street_num, W_ADDRESS_STREET_NUM);
    a.street_name(sz_temp);
    info.append_varchar(sz_temp, W_ADDRESS_STREET_NAME1);
    info.append_varchar(a.street_type, W_ADDRESS_STREET_TYPE);
    info.append_varchar(&a.suite_num, W_ADDRESS_SUITE_NUM);
    info.append_varchar(a.city, W_ADDRESS_CITY);
    info.append_varchar(a.county, W_ADDRESS_COUNTY);
    info.append_varchar(a.state, W_ADDRESS_STATE);
    a.zip_string(sz_temp);
    info.append_varchar(sz_temp, W_ADDRESS_ZIP);
    info.append_varchar(a.country, W_ADDRESS_COUNTRY);
    info.append_integer_decimal(a.gmt_offset, W_ADDRESS_GMT_OFFSET);
    info.row_end();

    ctx.warehouse = st;
}
