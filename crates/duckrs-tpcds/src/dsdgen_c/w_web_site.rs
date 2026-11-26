// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_web_site.cpp @ rust-port-baseline

//! The `web_site` table.

use super::Ctx;
use super::address::DsAddr;
use super::columns::*;
use super::constants::*;
use super::decimal::{Decimal, strtodec};
use super::scd::{change_scd, change_scd_ptr};
use super::tables::*;
use super::w_call_center::full_name;
use crate::append_info::AppendInfo;

/// `struct W_WEB_SITE_TBL`
#[derive(Clone, Default)]
struct WebSiteTbl {
    web_site_sk: i64,
    web_site_id: Vec<u8>,
    web_rec_start_date_id: i64,
    web_rec_end_date_id: i64,
    web_name: Vec<u8>,
    web_open_date: i64,
    web_close_date: i64,
    web_class: Vec<u8>,
    web_manager: Vec<u8>,
    web_market_id: i32,
    web_market_class: Vec<u8>,
    web_market_desc: Vec<u8>,
    web_market_manager: Vec<u8>,
    web_company_id: i32,
    web_company_name: Vec<u8>,
    web_address: DsAddr,
    web_tax_percentage: Decimal,
}

/// `g_w_web_site`, `g_OldValues` and the statics of `mk_w_web_site`.
#[derive(Default)]
pub(crate) struct State {
    r: WebSiteTbl,
    old: WebSiteTbl,
    d_min_tax_percentage: Decimal,
    d_max_tax_percentage: Decimal,
    sz_street_name: Vec<u8>,
}

/// `mk_w_web_site`
pub(crate) fn mk_w_web_site(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.web_site);
    let mut b_first_record = false;
    if !ctx.init.mk_w_web_site_init {
        st.r.web_class = b"Unknown".to_vec();
        st.d_min_tax_percentage = strtodec(WEB_MIN_TAX_PERCENTAGE.as_bytes());
        st.d_max_tax_percentage = strtodec(WEB_MAX_TAX_PERCENTAGE.as_bytes());
        ctx.init.mk_w_web_site_init = true;
    }
    let r = &mut st.r;
    let old = &mut st.old;
    ctx.null_set(WEB_SITE, WEB_NULLS);
    r.web_site_sk = index;

    // if we have generated the required history for this business key and generate a new one
    // then reset associated fields (e.g., rec_start_date minimums)
    if ctx.set_scd_keys(
        WEB_SITE_ID,
        index,
        &mut r.web_site_id,
        &mut r.web_rec_start_date_id,
        &mut r.web_rec_end_date_id,
    ) {
        r.web_open_date = ctx.mk_join(WEB_OPEN_DATE, DATET, index);
        r.web_close_date = ctx.mk_join(WEB_CLOSE_DATE, DATET, index);
        if r.web_close_date > r.web_rec_end_date_id {
            r.web_close_date = -1;
        }
        r.web_name.clear();
        r.web_name
            .extend_from_slice(format!("site_{}", (index / 6) as i32).as_bytes());
        b_first_record = true;
    }

    // select the random number that controls if a field changes from one record to the next
    let mut n_field_change_flags = ctx.next_random(WEB_SCD) as i32;

    // the rest of the record in a history-keeping dimension can either be a new data value or
    // not; use a random number and its bit pattern to determine which fields to replace and
    // which to retain
    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, WEB_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, WEB_MANAGER).0;
    full_name(&mut r.web_manager, s_name1, s_name2);
    change_scd(
        &mut r.web_manager,
        &mut old.web_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.web_market_id = ctx.genrand_integer(DIST_UNIFORM, 1, 6, 0, WEB_MARKET_ID);
    change_scd(
        &mut r.web_market_id,
        &mut old.web_market_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.gen_text(&mut r.web_market_class, 20, RS_WEB_MARKET_CLASS, WEB_MARKET_CLASS);
    change_scd(
        &mut r.web_market_class,
        &mut old.web_market_class,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.gen_text(&mut r.web_market_desc, 20, RS_WEB_MARKET_DESC, WEB_MARKET_DESC);
    change_scd(
        &mut r.web_market_desc,
        &mut old.web_market_desc,
        &mut n_field_change_flags,
        b_first_record,
    );

    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, WEB_MARKET_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, WEB_MARKET_MANAGER).0;
    full_name(&mut r.web_market_manager, s_name1, s_name2);
    change_scd(
        &mut r.web_market_manager,
        &mut old.web_market_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.web_company_id = ctx.genrand_integer(DIST_UNIFORM, 1, 6, 0, WEB_COMPANY_ID);
    change_scd(
        &mut r.web_company_id,
        &mut old.web_company_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.mk_word(
        &mut r.web_company_name,
        "Syllables",
        i64::from(r.web_company_id),
        RS_WEB_COMPANY_NAME,
        WEB_COMPANY_NAME,
    );
    change_scd(
        &mut r.web_company_name,
        &mut old.web_company_name,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.mk_address(&mut r.web_address, WEB_ADDRESS);
    change_scd_ptr(&mut n_field_change_flags); // city
    change_scd_ptr(&mut n_field_change_flags); // county
    change_scd(
        &mut r.web_address.gmt_offset,
        &mut old.web_address.gmt_offset,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd_ptr(&mut n_field_change_flags); // state
    change_scd_ptr(&mut n_field_change_flags); // street_type
    change_scd_ptr(&mut n_field_change_flags); // street_name1
    change_scd_ptr(&mut n_field_change_flags); // street_name2
    change_scd(
        &mut r.web_address.street_num,
        &mut old.web_address.street_num,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd(
        &mut r.web_address.zip,
        &mut old.web_address.zip,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (min, max) = (st.d_min_tax_percentage, st.d_max_tax_percentage);
    ctx.genrand_decimal(
        &mut r.web_tax_percentage,
        DIST_UNIFORM,
        &min,
        &max,
        None,
        WEB_TAX_PERCENTAGE,
    );
    change_scd(
        &mut r.web_tax_percentage,
        &mut old.web_tax_percentage,
        &mut n_field_change_flags,
        b_first_record,
    );

    let sz_street_name = &mut st.sz_street_name;
    let info = info_arr.row_start(WEB_SITE, ctx.tdef(WEB_SITE).k_null_bit_map);
    info.append_key(r.web_site_sk, WEB_SITE_SK);
    info.append_varchar(&r.web_site_id, WEB_SITE_ID);
    info.append_date(i64::from(r.web_rec_start_date_id as i32), WEB_REC_START_DATE_ID);
    info.append_date(i64::from(r.web_rec_end_date_id as i32), WEB_REC_END_DATE_ID);
    info.append_varchar(&r.web_name, WEB_NAME);
    info.append_key(r.web_open_date, WEB_OPEN_DATE);
    info.append_key(r.web_close_date, WEB_CLOSE_DATE);
    info.append_varchar(&r.web_class, WEB_CLASS);
    info.append_varchar(&r.web_manager, WEB_MANAGER);
    info.append_integer(r.web_market_id, WEB_MARKET_ID);
    info.append_varchar(&r.web_market_class, WEB_MARKET_CLASS);
    info.append_varchar(&r.web_market_desc, WEB_MARKET_DESC);
    info.append_varchar(&r.web_market_manager, WEB_MARKET_MANAGER);
    info.append_integer(r.web_company_id, WEB_COMPANY_ID);
    info.append_varchar(&r.web_company_name, WEB_COMPANY_NAME);
    info.append_integer(r.web_address.street_num, WEB_ADDRESS_STREET_NUM);
    r.web_address.street_name(sz_street_name);
    info.append_varchar(sz_street_name, WEB_ADDRESS_STREET_NAME1);
    info.append_varchar(r.web_address.street_type, WEB_ADDRESS_STREET_TYPE);
    info.append_varchar(&r.web_address.suite_num, WEB_ADDRESS_SUITE_NUM);
    info.append_varchar(r.web_address.city, WEB_ADDRESS_CITY);
    info.append_varchar(r.web_address.county, WEB_ADDRESS_COUNTY);
    info.append_varchar(r.web_address.state, WEB_ADDRESS_STATE);
    r.web_address.zip_string(sz_street_name);
    info.append_varchar(sz_street_name, WEB_ADDRESS_ZIP);
    info.append_varchar(r.web_address.country, WEB_ADDRESS_COUNTRY);
    info.append_integer_decimal(r.web_address.gmt_offset, WEB_ADDRESS_GMT_OFFSET);
    info.append_decimal(&r.web_tax_percentage, WEB_TAX_PERCENTAGE);
    info.row_end();

    ctx.web_site = st;
}
