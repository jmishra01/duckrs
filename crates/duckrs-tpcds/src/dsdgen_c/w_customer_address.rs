// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_customer_address.cpp @ rust-port-baseline

//! The `customer_address` table.

use super::Ctx;
use super::address::DsAddr;
use super::build_support::mk_bkey;
use super::columns::*;
use super::tables::CUSTOMER_ADDRESS;
use crate::append_info::AppendInfo;

/// `struct W_CUSTOMER_ADDRESS_TBL`
#[derive(Default)]
struct CustomerAddressTbl {
    ca_addr_sk: i64,
    ca_addr_id: Vec<u8>,
    ca_address: DsAddr,
    ca_location_type: &'static [u8],
}

/// `g_w_customer_address`
#[derive(Default)]
pub(crate) struct State {
    r: CustomerAddressTbl,
    sz_temp: Vec<u8>,
}

/// `mk_w_customer_address`
pub(crate) fn mk_w_customer_address(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.customer_address);
    let r = &mut st.r;
    ctx.null_set(CUSTOMER_ADDRESS, CA_NULLS);
    r.ca_addr_sk = index;
    mk_bkey(&mut r.ca_addr_id, index, CA_ADDRESS_ID);
    r.ca_location_type = ctx.pick_distribution_str("location_type", 1, 1, CA_LOCATION_TYPE).0;
    ctx.mk_address(&mut r.ca_address, CA_ADDRESS);

    let sz_temp = &mut st.sz_temp;
    let info = info_arr.row_start(CUSTOMER_ADDRESS, ctx.tdef(CUSTOMER_ADDRESS).k_null_bit_map);
    info.append_key(r.ca_addr_sk, CA_ADDRESS_SK);
    info.append_varchar(&r.ca_addr_id, CA_ADDRESS_ID);
    info.append_integer(r.ca_address.street_num, CA_ADDRESS_STREET_NUM);
    r.ca_address.street_name(sz_temp);
    info.append_varchar(sz_temp, CA_ADDRESS_STREET_NAME1);
    info.append_varchar(r.ca_address.street_type, CA_ADDRESS_STREET_TYPE);
    info.append_varchar(&r.ca_address.suite_num, CA_ADDRESS_SUITE_NUM);
    info.append_varchar(r.ca_address.city, CA_ADDRESS_CITY);
    info.append_varchar(r.ca_address.county, CA_ADDRESS_COUNTY);
    info.append_varchar(r.ca_address.state, CA_ADDRESS_STATE);
    r.ca_address.zip_string(sz_temp);
    info.append_varchar(sz_temp, CA_ADDRESS_ZIP);
    info.append_varchar(r.ca_address.country, CA_ADDRESS_COUNTRY);
    info.append_integer_decimal(r.ca_address.gmt_offset, CA_ADDRESS_GMT_OFFSET);
    info.append_varchar(r.ca_location_type, CA_LOCATION_TYPE);
    info.row_end();

    ctx.customer_address = st;
}
