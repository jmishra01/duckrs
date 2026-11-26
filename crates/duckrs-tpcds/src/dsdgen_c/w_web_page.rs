// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_web_page.cpp @ rust-port-baseline

//! The `web_page` table.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, strtodt};
use super::scd::{change_scd, change_scd_ptr};
use super::tables::*;
use crate::append_info::AppendInfo;

/// `struct W_WEB_PAGE_TBL`
#[derive(Clone, Default)]
struct WebPageTbl {
    wp_page_sk: i64,
    wp_page_id: Vec<u8>,
    wp_rec_start_date_id: i64,
    wp_rec_end_date_id: i64,
    wp_creation_date_sk: i64,
    wp_access_date_sk: i64,
    wp_autogen_flag: i32,
    wp_customer_sk: i64,
    wp_url: Vec<u8>,
    wp_type: &'static [u8],
    wp_char_count: i32,
    wp_link_count: i32,
    wp_image_count: i32,
    wp_max_ad_count: i32,
}

/// `g_w_web_page`, `g_OldValues` and the statics of `mk_w_web_page`.
#[derive(Default)]
pub(crate) struct State {
    r: WebPageTbl,
    old: WebPageTbl,
    d_today: DateT,
}

/// `mk_w_web_page`
pub(crate) fn mk_w_web_page(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.web_page);
    let mut b_first_record = false;
    if !ctx.init.mk_w_web_page_init {
        // setup invariant values
        st.d_today = strtodt(&format!("{CURRENT_YEAR}-{CURRENT_MONTH}-{CURRENT_DAY}"));
        // the number of revisions (web pages per concurrent site) is not used
        ctx.init.mk_w_web_page_init = true;
    }
    let r = &mut st.r;
    let old = &mut st.old;
    ctx.null_set(WEB_PAGE, WP_NULLS);
    r.wp_page_sk = index;

    // if we have generated the required history for this business key and generate a new one
    // then reset associated fields (e.g., rec_start_date minimums)
    if ctx.set_scd_keys(
        WP_PAGE_ID,
        index,
        &mut r.wp_page_id,
        &mut r.wp_rec_start_date_id,
        &mut r.wp_rec_end_date_id,
    ) {
        // some fields are not changed, even when a new version of the row is written
        b_first_record = true;
    }

    // select the random number that controls if a field changes from one record to the next
    let mut n_field_change_flags = ctx.next_random(WP_SCD) as i32;

    r.wp_creation_date_sk = ctx.mk_join(WP_CREATION_DATE_SK, DATET, index);
    change_scd(
        &mut r.wp_creation_date_sk,
        &mut old.wp_creation_date_sk,
        &mut n_field_change_flags,
        b_first_record,
    );

    let n_access = ctx.genrand_integer(DIST_UNIFORM, 0, WP_IDLE_TIME_MAX, 0, WP_ACCESS_DATE_SK);
    r.wp_access_date_sk = i64::from(st.d_today.julian - n_access);
    change_scd(
        &mut r.wp_access_date_sk,
        &mut old.wp_access_date_sk,
        &mut n_field_change_flags,
        b_first_record,
    );
    if r.wp_access_date_sk == 0 {
        r.wp_access_date_sk = -1; // special case for dates
    }

    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, WP_AUTOGEN_FLAG);
    r.wp_autogen_flag = i32::from(n_temp < WP_AUTOGEN_PCT);
    change_scd(
        &mut r.wp_autogen_flag,
        &mut old.wp_autogen_flag,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.wp_customer_sk = ctx.mk_join(WP_CUSTOMER_SK, CUSTOMER, 1);
    change_scd(
        &mut r.wp_customer_sk,
        &mut old.wp_customer_sk,
        &mut n_field_change_flags,
        b_first_record,
    );

    if r.wp_autogen_flag == 0 {
        r.wp_customer_sk = -1;
    }

    ctx.genrand_url(&mut r.wp_url, WP_URL);
    change_scd(
        &mut r.wp_url,
        &mut old.wp_url,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.wp_type = ctx.pick_distribution_str("web_page_use", 1, 1, WP_TYPE).0;
    change_scd_ptr(&mut n_field_change_flags);

    r.wp_link_count = ctx.genrand_integer(DIST_UNIFORM, WP_LINK_MIN, WP_LINK_MAX, 0, WP_LINK_COUNT);
    change_scd(
        &mut r.wp_link_count,
        &mut old.wp_link_count,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.wp_image_count = ctx.genrand_integer(DIST_UNIFORM, WP_IMAGE_MIN, WP_IMAGE_MAX, 0, WP_IMAGE_COUNT);
    change_scd(
        &mut r.wp_image_count,
        &mut old.wp_image_count,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.wp_max_ad_count = ctx.genrand_integer(DIST_UNIFORM, WP_AD_MIN, WP_AD_MAX, 0, WP_MAX_AD_COUNT);
    change_scd(
        &mut r.wp_max_ad_count,
        &mut old.wp_max_ad_count,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.wp_char_count = ctx.genrand_integer(
        DIST_UNIFORM,
        r.wp_link_count * 125 + r.wp_image_count * 50,
        r.wp_link_count * 300 + r.wp_image_count * 150,
        0,
        WP_CHAR_COUNT,
    );
    change_scd(
        &mut r.wp_char_count,
        &mut old.wp_char_count,
        &mut n_field_change_flags,
        b_first_record,
    );

    let info = info_arr.row_start(WEB_PAGE, ctx.tdef(WEB_PAGE).k_null_bit_map);
    info.append_key(r.wp_page_sk, WP_PAGE_SK);
    info.append_varchar(&r.wp_page_id, WP_PAGE_ID);
    info.append_date(r.wp_rec_start_date_id, WP_REC_START_DATE_ID);
    info.append_date(r.wp_rec_end_date_id, WP_REC_END_DATE_ID);
    info.append_key(r.wp_creation_date_sk, WP_CREATION_DATE_SK);
    info.append_key(r.wp_access_date_sk, WP_ACCESS_DATE_SK);
    info.append_varchar(if r.wp_autogen_flag != 0 { b"Y" } else { b"N" }, WP_AUTOGEN_FLAG);
    info.append_key(r.wp_customer_sk, WP_CUSTOMER_SK);
    info.append_varchar(&r.wp_url, WP_URL);
    info.append_varchar(r.wp_type, WP_TYPE);
    info.append_integer(r.wp_char_count, WP_CHAR_COUNT);
    info.append_integer(r.wp_link_count, WP_LINK_COUNT);
    info.append_integer(r.wp_image_count, WP_IMAGE_COUNT);
    info.append_integer(r.wp_max_ad_count, WP_MAX_AD_COUNT);
    info.row_end();

    ctx.web_page = st;
}
