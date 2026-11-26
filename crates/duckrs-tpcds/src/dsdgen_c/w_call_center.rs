// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_call_center.cpp @ rust-port-baseline

//! The `call_center` table.

use super::Ctx;
use super::address::DsAddr;
use super::columns::*;
use super::constants::*;
use super::date::{dttoj, strtodt};
use super::decimal::{Decimal, strtodec};
use super::scd::{change_scd, change_scd_ptr};
use super::tables::CALL_CENTER;
use crate::append_info::AppendInfo;

/// `struct CALL_CENTER_TBL`
#[derive(Clone, Default)]
pub(crate) struct CallCenterTbl {
    cc_call_center_sk: i64,
    cc_call_center_id: Vec<u8>,
    cc_rec_start_date_id: i64,
    cc_rec_end_date_id: i64,
    cc_closed_date_id: i64,
    cc_open_date_id: i64,
    cc_name: Vec<u8>,
    cc_class: &'static [u8],
    cc_employees: i32,
    cc_sq_ft: i32,
    cc_hours: &'static [u8],
    cc_manager: Vec<u8>,
    cc_market_id: i32,
    cc_market_class: Vec<u8>,
    cc_market_desc: Vec<u8>,
    cc_market_manager: Vec<u8>,
    cc_division_id: i32,
    cc_division_name: Vec<u8>,
    cc_company: i32,
    cc_company_name: Vec<u8>,
    cc_address: DsAddr,
    cc_tax_percentage: Decimal,
}

/// `g_w_call_center`, `g_OldValues` and the statics of `mk_w_call_center`.
#[derive(Default)]
pub(crate) struct State {
    r: CallCenterTbl,
    old: CallCenterTbl,
    j_date_start: i32,
    n_scale: f64,
    d_min_tax_percentage: Decimal,
    d_max_tax_percentage: Decimal,
    sz_temp: Vec<u8>,
}

/// `"first last"`
pub(crate) fn full_name(dest: &mut Vec<u8>, first: &[u8], last: &[u8]) {
    dest.clear();
    dest.extend_from_slice(first);
    dest.push(b' ');
    dest.extend_from_slice(last);
}

/// `mk_w_call_center`
pub(crate) fn mk_w_call_center(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.call_center);
    let r = &mut st.r;
    let old = &mut st.old;
    let mut b_first_record = false;
    if !ctx.init.mk_w_call_center_init {
        st.j_date_start = dttoj(&strtodt(DATA_START_DATE)) - super::tables::WEB_SITE;
        // nDaysPerRevision, from the date range and nParam, is not used
        st.n_scale = ctx.params.scale;
        r.cc_division_id = -1;
        r.cc_closed_date_id = -1;
        r.cc_division_name = b"No Name".to_vec();
        st.d_min_tax_percentage = strtodec(MIN_CC_TAX_PERCENTAGE.as_bytes());
        st.d_max_tax_percentage = strtodec(MAX_CC_TAX_PERCENTAGE.as_bytes());
        ctx.init.mk_w_call_center_init = true;
    }
    ctx.null_set(CALL_CENTER, CC_NULLS);
    r.cc_call_center_sk = index;

    if ctx.set_scd_keys(
        CC_CALL_CENTER_ID,
        index,
        &mut r.cc_call_center_id,
        &mut r.cc_rec_start_date_id,
        &mut r.cc_rec_end_date_id,
    ) {
        r.cc_open_date_id = i64::from(st.j_date_start - ctx.genrand_integer(DIST_UNIFORM, -365, 0, 0, CC_OPEN_DATE_ID));
        let n_suffix = index as i32 / ctx.distsize("call_centers");
        let cp = ctx.dist_member_str(
            "call_centers",
            (index % i64::from(ctx.distsize("call_centers"))) as i32 + 1,
            1,
        );
        r.cc_name.clear();
        r.cc_name.extend_from_slice(cp);
        if n_suffix > 0 {
            r.cc_name.extend_from_slice(format!("_{n_suffix}").as_bytes());
        }
        ctx.mk_address(&mut r.cc_address, CC_ADDRESS);
        b_first_record = true;
    }

    let mut n_field_change_flags = ctx.next_random(CC_SCD) as i32;

    r.cc_class = ctx.pick_distribution_str("call_center_class", 1, 1, CC_CLASS).0;
    change_scd_ptr(&mut n_field_change_flags);

    let employee_max = if st.n_scale >= 1.0 {
        (f64::from(CC_EMPLOYEE_MAX) * st.n_scale * st.n_scale) as i32
    } else {
        CC_EMPLOYEE_MAX
    };
    r.cc_employees = ctx.genrand_integer(DIST_UNIFORM, 1, employee_max, 0, CC_EMPLOYEES);
    change_scd(
        &mut r.cc_employees,
        &mut old.cc_employees,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.cc_sq_ft = ctx.genrand_integer(DIST_UNIFORM, 100, 700, 0, CC_SQ_FT);
    r.cc_sq_ft = r.cc_sq_ft.wrapping_mul(r.cc_employees);
    change_scd(
        &mut r.cc_sq_ft,
        &mut old.cc_sq_ft,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.cc_hours = ctx.pick_distribution_str("call_center_hours", 1, 1, CC_HOURS).0;
    change_scd_ptr(&mut n_field_change_flags);

    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, CC_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, CC_MANAGER).0;
    full_name(&mut r.cc_manager, s_name1, s_name2);
    change_scd(
        &mut r.cc_manager,
        &mut old.cc_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.cc_market_id = ctx.genrand_integer(DIST_UNIFORM, 1, 6, 0, CC_MARKET_ID);
    change_scd(
        &mut r.cc_market_id,
        &mut old.cc_market_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.gen_text(&mut r.cc_market_class, 20, RS_CC_MARKET_CLASS, CC_MARKET_CLASS);
    change_scd(
        &mut r.cc_market_class,
        &mut old.cc_market_class,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.gen_text(&mut r.cc_market_desc, 20, RS_CC_MARKET_DESC, CC_MARKET_DESC);
    change_scd(
        &mut r.cc_market_desc,
        &mut old.cc_market_desc,
        &mut n_field_change_flags,
        b_first_record,
    );

    let s_name1 = ctx.pick_distribution_str("first_names", 1, 1, CC_MARKET_MANAGER).0;
    let s_name2 = ctx.pick_distribution_str("last_names", 1, 1, CC_MARKET_MANAGER).0;
    full_name(&mut r.cc_market_manager, s_name1, s_name2);
    change_scd(
        &mut r.cc_market_manager,
        &mut old.cc_market_manager,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.cc_company = ctx.genrand_integer(DIST_UNIFORM, 1, 6, 0, CC_COMPANY);
    change_scd(
        &mut r.cc_company,
        &mut old.cc_company,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.cc_division_id = ctx.genrand_integer(DIST_UNIFORM, 1, 6, 0, CC_COMPANY);
    change_scd(
        &mut r.cc_division_id,
        &mut old.cc_division_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.mk_word(
        &mut r.cc_division_name,
        "syllables",
        i64::from(r.cc_division_id),
        RS_CC_DIVISION_NAME,
        CC_DIVISION_NAME,
    );
    change_scd(
        &mut r.cc_division_name,
        &mut old.cc_division_name,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.mk_companyname(&mut r.cc_company_name, CC_COMPANY_NAME, r.cc_company);
    change_scd(
        &mut r.cc_company_name,
        &mut old.cc_company_name,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (min, max) = (st.d_min_tax_percentage, st.d_max_tax_percentage);
    ctx.genrand_decimal(
        &mut r.cc_tax_percentage,
        DIST_UNIFORM,
        &min,
        &max,
        None,
        CC_TAX_PERCENTAGE,
    );
    change_scd(
        &mut r.cc_tax_percentage,
        &mut old.cc_tax_percentage,
        &mut n_field_change_flags,
        b_first_record,
    );

    let sz_temp = &mut st.sz_temp;
    let info = info_arr.row_start(CALL_CENTER, ctx.tdef(CALL_CENTER).k_null_bit_map);
    info.append_key(r.cc_call_center_sk, CC_CALL_CENTER_SK);
    info.append_varchar(&r.cc_call_center_id, CC_CALL_CENTER_ID);
    info.append_date(r.cc_rec_start_date_id, CC_REC_START_DATE_ID);
    info.append_date(r.cc_rec_end_date_id, CC_REC_END_DATE_ID);
    info.append_key(r.cc_closed_date_id, CC_CLOSED_DATE_ID);
    info.append_key(r.cc_open_date_id, CC_OPEN_DATE_ID);
    info.append_varchar(&r.cc_name, CC_NAME);
    info.append_varchar(r.cc_class, CC_CLASS);
    info.append_integer(r.cc_employees, CC_EMPLOYEES);
    info.append_integer(r.cc_sq_ft, CC_SQ_FT);
    info.append_varchar(r.cc_hours, CC_HOURS);
    info.append_varchar(&r.cc_manager, CC_MANAGER);
    info.append_integer(r.cc_market_id, CC_MARKET_ID);
    info.append_varchar(&r.cc_market_class, CC_MARKET_CLASS);
    info.append_varchar(&r.cc_market_desc, CC_MARKET_DESC);
    info.append_varchar(&r.cc_market_manager, CC_MARKET_MANAGER);
    info.append_integer(r.cc_division_id, CC_DIVISION);
    info.append_varchar(&r.cc_division_name, CC_DIVISION_NAME);
    info.append_integer(r.cc_company, CC_COMPANY);
    info.append_varchar(&r.cc_company_name, CC_COMPANY_NAME);
    info.append_integer(r.cc_address.street_num, CC_ADDRESS);
    r.cc_address.street_name(sz_temp);
    info.append_varchar(sz_temp, CC_ADDRESS);
    info.append_varchar(r.cc_address.street_type, CC_ADDRESS);
    info.append_varchar(&r.cc_address.suite_num, CC_ADDRESS);
    info.append_varchar(r.cc_address.city, CC_ADDRESS);
    info.append_varchar(r.cc_address.county, CC_ADDRESS);
    info.append_varchar(r.cc_address.state, CC_ADDRESS);
    r.cc_address.zip_string(sz_temp);
    info.append_varchar(sz_temp, CC_ADDRESS);
    info.append_varchar(r.cc_address.country, CC_ADDRESS);
    info.append_integer_decimal(r.cc_address.gmt_offset, CC_ADDRESS);
    info.append_decimal(&r.cc_tax_percentage, CC_TAX_PERCENTAGE);
    info.row_end();

    ctx.call_center = st;
}
