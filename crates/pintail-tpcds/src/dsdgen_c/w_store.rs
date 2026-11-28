// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_store.cpp @ rust-port-baseline

//! The `store` table.

use super::Ctx;
use super::address::DsAddr;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, strtodt};
use super::decimal::{Decimal, strtodec};
use super::scd::{change_scd, change_scd_ptr};
use super::tables::*;
use super::w_call_center::full_name;
use crate::append_info::AppendInfo;

/// `struct W_STORE_TBL`
#[derive(Clone, Default)]
struct StoreTbl {
    store_sk: i64,
    store_id: Vec<u8>,
    rec_start_date_id: i64,
    rec_end_date_id: i64,
    closed_date_id: i64,
    store_name: Vec<u8>,
    employees: i32,
    floor_space: i32,
    hours: &'static [u8],
    store_manager: Vec<u8>,
    market_id: i32,
    d_tax_percentage: Decimal,
    geography_class: &'static [u8],
    market_desc: Vec<u8>,
    market_manager: Vec<u8>,
    division_id: i64,
    division_name: &'static [u8],
    company_id: i64,
    company_name: &'static [u8],
    address: DsAddr,
}

/// `g_w_store`, `g_OldValues` and the statics of `mk_w_store`.
#[derive(Default)]
pub(crate) struct State {
    r: StoreTbl,
    old: StoreTbl,
    t_date: DateT,
    d_min_tax_percentage: Decimal,
    d_max_tax_percentage: Decimal,
    sz_temp: Vec<u8>,
}

/// `mk_w_store`
pub(crate) fn mk_w_store(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.store);
    let mut b_first_record = false;
    if !ctx.init.mk_w_store_init {
        // the hierarchy total (divisions times companies), the revenue limits and the revenue
        // growth limits are not used
        st.t_date = strtodt(DATE_MINIMUM);
        st.d_min_tax_percentage = strtodec(STORE_MIN_TAX_PERCENTAGE.as_bytes());
        st.d_max_tax_percentage = strtodec(STORE_MAX_TAX_PERCENTAGE.as_bytes());
        st.r.rec_end_date_id = -1;
        ctx.init.mk_w_store_init = true;
    }
    let r = &mut st.r;
    let old = &mut st.old;
    ctx.null_set(STORE, W_STORE_NULLS);
    r.store_sk = index;

    // if we have generated the required history for this business key and generate a new one
    // then reset associated fields (e.g., rec_start_date minimums)
    if ctx.set_scd_keys(
        S_STORE_ID,
        index,
        &mut r.store_id,
        &mut r.rec_start_date_id,
        &mut r.rec_end_date_id,
    ) {
        b_first_record = true;
    }

    // select the random number that controls if a field changes from one record to the next
    let mut n_field_change_flags = ctx.next_random(W_STORE_SCD) as i32;

    let n_percentage = ctx.genrand_integer(DIST_UNIFORM, 1, 100, 0, W_STORE_CLOSED_DATE_ID);
    let n_days_open = ctx.genrand_integer(
        DIST_UNIFORM,
        STORE_MIN_DAYS_OPEN,
        STORE_MAX_DAYS_OPEN,
        0,
        W_STORE_CLOSED_DATE_ID,
    );
    if n_percentage < STORE_CLOSED_PCT {
        r.closed_date_id = i64::from(st.t_date.julian + n_days_open);
    } else {
        r.closed_date_id = -1;
    }
    change_scd(
        &mut r.closed_date_id,
        &mut old.closed_date_id,
        &mut n_field_change_flags,
        b_first_record,
    );
    if r.closed_date_id == 0 {
        r.closed_date_id = -1;
    }

    ctx.mk_word(&mut r.store_name, "syllables", index, 5, W_STORE_NAME);
    change_scd(
        &mut r.store_name,
        &mut old.store_name,
        &mut n_field_change_flags,
        b_first_record,
    );

    // use the store type to set the parameters for the rest of the attributes
    let n_store_type = ctx.pick_distribution_str("store_type", 1, 1, W_STORE_TYPE).1;
    let n_min = ctx.dist_member_int("store_type", n_store_type, 2);
    let n_max = ctx.dist_member_int("store_type", n_store_type, 3);
    r.employees = ctx.genrand_integer(DIST_UNIFORM, n_min, n_max, 0, W_STORE_EMPLOYEES);
    change_scd(
        &mut r.employees,
        &mut old.employees,
        &mut n_field_change_flags,
        b_first_record,
    );

    let n_min = ctx.dist_member_int("store_type", n_store_type, 4);
    let n_max = ctx.dist_member_int("store_type", n_store_type, 5);
    r.floor_space = ctx.genrand_integer(DIST_UNIFORM, n_min, n_max, 0, W_STORE_FLOOR_SPACE);
    change_scd(
        &mut r.floor_space,
        &mut old.floor_space,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.hours = ctx.pick_distribution_str("call_center_hours", 1, 1, W_STORE_HOURS).0;
    change_scd_ptr(&mut n_field_change_flags);

    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, W_STORE_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, W_STORE_MANAGER).0;
    full_name(&mut r.store_manager, s_name1, s_name2);
    change_scd(
        &mut r.store_manager,
        &mut old.store_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.market_id = ctx.genrand_integer(DIST_UNIFORM, 1, 10, 0, W_STORE_MARKET_ID);
    change_scd(
        &mut r.market_id,
        &mut old.market_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (min, max) = (st.d_min_tax_percentage, st.d_max_tax_percentage);
    ctx.genrand_decimal(
        &mut r.d_tax_percentage,
        DIST_UNIFORM,
        &min,
        &max,
        None,
        W_STORE_TAX_PERCENTAGE,
    );
    change_scd(
        &mut r.d_tax_percentage,
        &mut old.d_tax_percentage,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.geography_class = ctx
        .pick_distribution_str("geography_class", 1, 1, W_STORE_GEOGRAPHY_CLASS)
        .0;
    change_scd_ptr(&mut n_field_change_flags);

    ctx.gen_text(
        &mut r.market_desc,
        STORE_DESC_MIN,
        RS_S_MARKET_DESC,
        W_STORE_MARKET_DESC,
    );
    change_scd(
        &mut r.market_desc,
        &mut old.market_desc,
        &mut n_field_change_flags,
        b_first_record,
    );

    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, W_STORE_MARKET_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, W_STORE_MARKET_MANAGER).0;
    full_name(&mut r.market_manager, s_name1, s_name2);
    change_scd(
        &mut r.market_manager,
        &mut old.market_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (division_name, division_id) = ctx.pick_distribution_str("divisions", 1, 1, W_STORE_DIVISION_NAME);
    r.division_name = division_name;
    r.division_id = i64::from(division_id);
    change_scd(
        &mut r.division_id,
        &mut old.division_id,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd_ptr(&mut n_field_change_flags);

    let (company_name, company_id) = ctx.pick_distribution_str("stores", 1, 1, W_STORE_COMPANY_NAME);
    r.company_name = company_name;
    r.company_id = i64::from(company_id);
    change_scd(
        &mut r.company_id,
        &mut old.company_id,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd_ptr(&mut n_field_change_flags);

    ctx.mk_address(&mut r.address, W_STORE_ADDRESS);
    change_scd_ptr(&mut n_field_change_flags); // city
    change_scd_ptr(&mut n_field_change_flags); // county
    change_scd(
        &mut r.address.gmt_offset,
        &mut old.address.gmt_offset,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd_ptr(&mut n_field_change_flags); // state
    change_scd_ptr(&mut n_field_change_flags); // street_type
    change_scd_ptr(&mut n_field_change_flags); // street_name1
    change_scd_ptr(&mut n_field_change_flags); // street_name2
    change_scd(
        &mut r.address.street_num,
        &mut old.address.street_num,
        &mut n_field_change_flags,
        b_first_record,
    );
    change_scd(
        &mut r.address.zip,
        &mut old.address.zip,
        &mut n_field_change_flags,
        b_first_record,
    );

    let sz_temp = &mut st.sz_temp;
    let info = info_arr.row_start(STORE, ctx.tdef(STORE).k_null_bit_map);
    info.append_key(r.store_sk, W_STORE_SK);
    info.append_varchar(&r.store_id, W_STORE_ID);
    info.append_date(r.rec_start_date_id, W_STORE_REC_START_DATE_ID);
    info.append_date(r.rec_end_date_id, W_STORE_REC_END_DATE_ID);
    info.append_key(r.closed_date_id, W_STORE_CLOSED_DATE_ID);
    info.append_varchar(&r.store_name, W_STORE_NAME);
    info.append_integer(r.employees, W_STORE_EMPLOYEES);
    info.append_integer(r.floor_space, W_STORE_FLOOR_SPACE);
    info.append_varchar(r.hours, W_STORE_HOURS);
    info.append_varchar(&r.store_manager, W_STORE_MANAGER);
    info.append_integer(r.market_id, W_STORE_MARKET_ID);
    info.append_varchar(r.geography_class, W_STORE_GEOGRAPHY_CLASS);
    info.append_varchar(&r.market_desc, W_STORE_MARKET_DESC);
    info.append_varchar(&r.market_manager, W_STORE_MARKET_MANAGER);
    info.append_key(r.division_id, W_STORE_DIVISION_ID);
    info.append_varchar(r.division_name, W_STORE_DIVISION_NAME);
    info.append_key(r.company_id, W_STORE_COMPANY_ID);
    info.append_varchar(r.company_name, W_STORE_COMPANY_NAME);
    info.append_integer(r.address.street_num, W_STORE_ADDRESS_STREET_NUM);
    r.address.street_name(sz_temp);
    info.append_varchar(sz_temp, W_STORE_ADDRESS_STREET_NAME1);
    info.append_varchar(r.address.street_type, W_STORE_ADDRESS_STREET_TYPE);
    info.append_varchar(&r.address.suite_num, W_STORE_ADDRESS_SUITE_NUM);
    info.append_varchar(r.address.city, W_STORE_ADDRESS_CITY);
    info.append_varchar(r.address.county, W_STORE_ADDRESS_COUNTY);
    info.append_varchar(r.address.state, W_STORE_ADDRESS_STATE);
    r.address.zip_string(sz_temp);
    info.append_varchar(sz_temp, W_STORE_ADDRESS_ZIP);
    info.append_varchar(r.address.country, W_STORE_ADDRESS_COUNTRY);
    info.append_integer_decimal(r.address.gmt_offset, W_STORE_ADDRESS_GMT_OFFSET);
    info.append_decimal(&r.d_tax_percentage, W_STORE_TAX_PERCENTAGE);
    info.row_end();

    ctx.store = st;
}
