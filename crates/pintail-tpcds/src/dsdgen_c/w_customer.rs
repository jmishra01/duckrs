// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_customer.cpp @ rust-port-baseline

//! The `customer` table.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, jtodt, strtodt};
use super::tables::*;
use crate::append_info::AppendInfo;

/// `struct W_CUSTOMER_TBL`
#[derive(Default)]
struct CustomerTbl {
    c_customer_sk: i64,
    c_customer_id: Vec<u8>,
    c_current_cdemo_sk: i64,
    c_current_hdemo_sk: i64,
    c_current_addr_sk: i64,
    c_first_shipto_date_id: i32,
    c_first_sales_date_id: i32,
    c_salutation: &'static [u8],
    c_first_name: &'static [u8],
    c_last_name: &'static [u8],
    c_preferred_cust_flag: bool,
    c_birth_day: i32,
    c_birth_month: i32,
    c_birth_year: i32,
    c_birth_country: &'static [u8],
    /// never set, so always NULL
    c_login: Vec<u8>,
    c_email_address: Vec<u8>,
    c_last_review_date: i32,
}

/// `g_w_customer` and the statics of `mk_w_customer`.
#[derive(Default)]
pub(crate) struct State {
    r: CustomerTbl,
    dt_birth_min: DateT,
    dt_birth_max: DateT,
    dt_today: DateT,
    dt1_year_ago: DateT,
    dt10_years_ago: DateT,
}

/// `mk_w_customer`
pub(crate) fn mk_w_customer(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.customer);
    if !ctx.init.mk_w_customer_init {
        st.dt_birth_max = strtodt("1992-12-31");
        st.dt_birth_min = strtodt("1924-01-01");
        st.dt_today = strtodt(TODAYS_DATE);
        st.dt1_year_ago = jtodt(st.dt_today.julian - 365);
        st.dt10_years_ago = jtodt(st.dt_today.julian - 3650);
        ctx.init.mk_w_customer_init = true;
    }
    let r = &mut st.r;
    ctx.null_set(CUSTOMER, C_NULLS);
    r.c_customer_sk = index;
    mk_bkey(&mut r.c_customer_id, index, C_CUSTOMER_ID);
    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 1, 100, 0, C_PREFERRED_CUST_FLAG);
    r.c_preferred_cust_flag = n_temp < C_PREFERRED_PCT;

    // demographic keys are a composite of values; rebuild them a la bitmap_to_dist
    r.c_current_hdemo_sk = ctx.mk_join(C_CURRENT_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.c_current_cdemo_sk = ctx.mk_join(C_CURRENT_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.c_current_addr_sk = ctx.mk_join(C_CURRENT_ADDR_SK, CUSTOMER_ADDRESS, r.c_customer_sk);
    let (first_name, n_name_index) = ctx.pick_distribution_str("first_names", 1, 3, C_FIRST_NAME);
    r.c_first_name = first_name;
    r.c_last_name = ctx.pick_distribution_str("last_names", 1, 1, C_LAST_NAME).0;
    let n_gender = ctx.dist_weight("first_names", n_name_index, 2);
    r.c_salutation = ctx
        .pick_distribution_str("salutations", 1, if n_gender == 0 { 2 } else { 3 }, C_SALUTATION)
        .0;

    let (min, max) = (st.dt_birth_min, st.dt_birth_max);
    let dt_temp = ctx.genrand_date(DIST_UNIFORM, &min, &max, None, C_BIRTH_DAY);
    r.c_birth_day = dt_temp.day;
    r.c_birth_month = dt_temp.month;
    r.c_birth_year = dt_temp.year;
    ctx.genrand_email(&mut r.c_email_address, r.c_first_name, r.c_last_name, C_EMAIL_ADDRESS);
    let (min, max) = (st.dt1_year_ago, st.dt_today);
    r.c_last_review_date = ctx
        .genrand_date(DIST_UNIFORM, &min, &max, None, C_LAST_REVIEW_DATE)
        .julian;
    let (min, max) = (st.dt10_years_ago, st.dt_today);
    r.c_first_sales_date_id = ctx
        .genrand_date(DIST_UNIFORM, &min, &max, None, C_FIRST_SALES_DATE_ID)
        .julian;
    r.c_first_shipto_date_id = r.c_first_sales_date_id + 30;

    r.c_birth_country = ctx.pick_distribution_str("countries", 1, 1, C_BIRTH_COUNTRY).0;

    let info = info_arr.row_start(CUSTOMER, ctx.tdef(CUSTOMER).k_null_bit_map);
    info.append_key(r.c_customer_sk, C_CUSTOMER_SK);
    info.append_varchar(&r.c_customer_id, C_CUSTOMER_ID);
    info.append_key(r.c_current_cdemo_sk, C_CURRENT_CDEMO_SK);
    info.append_key(r.c_current_hdemo_sk, C_CURRENT_HDEMO_SK);
    info.append_key(r.c_current_addr_sk, C_CURRENT_ADDR_SK);
    info.append_integer(r.c_first_shipto_date_id, C_FIRST_SHIPTO_DATE_ID);
    info.append_integer(r.c_first_sales_date_id, C_FIRST_SALES_DATE_ID);
    info.append_varchar(r.c_salutation, C_SALUTATION);
    info.append_varchar(r.c_first_name, C_FIRST_NAME);
    info.append_varchar(r.c_last_name, C_LAST_NAME);
    info.append_varchar(if r.c_preferred_cust_flag { b"Y" } else { b"N" }, C_PREFERRED_CUST_FLAG);
    info.append_integer(r.c_birth_day, C_BIRTH_DAY);
    info.append_integer(r.c_birth_month, C_BIRTH_MONTH);
    info.append_integer(r.c_birth_year, C_BIRTH_YEAR);
    info.append_varchar(r.c_birth_country, C_BIRTH_COUNTRY);
    info.append_varchar(&r.c_login, C_LOGIN);
    info.append_varchar(&r.c_email_address, C_EMAIL_ADDRESS);
    info.append_integer(r.c_last_review_date, C_LAST_REVIEW_DATE);
    info.row_end();

    ctx.customer = st;
}
