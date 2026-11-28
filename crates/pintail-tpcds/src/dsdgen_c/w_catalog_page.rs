// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_catalog_page.cpp @ rust-port-baseline

//! The `catalog_page` table.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, strtodt};
use super::tables::CATALOG_PAGE;
use crate::append_info::AppendInfo;

/// `struct CATALOG_PAGE_TBL`
#[derive(Default)]
struct CatalogPageTbl {
    cp_catalog_page_sk: i64,
    cp_catalog_page_id: Vec<u8>,
    cp_start_date_id: i64,
    cp_end_date_id: i64,
    cp_department: Vec<u8>,
    cp_catalog_number: i32,
    cp_catalog_page_number: i32,
    cp_description: Vec<u8>,
    cp_type: &'static [u8],
}

/// `g_w_catalog_page` and the statics of `mk_w_catalog_page`.
#[derive(Default)]
pub(crate) struct State {
    r: CatalogPageTbl,
    d_start_date: DateT,
    n_catalog_page_max: i32,
}

/// `mk_w_catalog_page`
pub(crate) fn mk_w_catalog_page(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.catalog_page);
    if !ctx.init.mk_w_catalog_page_init {
        st.n_catalog_page_max =
            ((ctx.get_rowcount(CATALOG_PAGE) as i32) / CP_CATALOGS_PER_YEAR) / (YEAR_MAXIMUM - YEAR_MINIMUM + 2);
        st.d_start_date = strtodt(DATA_START_DATE);
        st.r.cp_department = b"DEPARTMENT".to_vec();
        ctx.init.mk_w_catalog_page_init = true;
    }
    let r = &mut st.r;
    ctx.null_set(CATALOG_PAGE, CP_NULLS);
    r.cp_catalog_page_sk = index;
    mk_bkey(&mut r.cp_catalog_page_id, index, CP_CATALOG_PAGE_ID);
    r.cp_catalog_number = ((index - 1) / i64::from(st.n_catalog_page_max) + 1) as i32;
    r.cp_catalog_page_number = ((index - 1) % i64::from(st.n_catalog_page_max) + 1) as i32;
    let n_catalog_interval = (r.cp_catalog_number - 1) % CP_CATALOGS_PER_YEAR;
    let (n_type, n_duration, n_offset) = match n_catalog_interval {
        0 | 1 => (1, 182, n_catalog_interval * 182),
        2..=5 => (2, 91, (n_catalog_interval - 2) * 91),
        _ => (3, 30, (n_catalog_interval - 6) * 30),
    };
    r.cp_start_date_id = i64::from(st.d_start_date.julian + n_offset);
    r.cp_start_date_id += i64::from(((r.cp_catalog_number - 1) / CP_CATALOGS_PER_YEAR) * 365);
    r.cp_end_date_id = r.cp_start_date_id + i64::from(n_duration) - 1;
    r.cp_type = ctx.dist_member_str("catalog_page_type", n_type, 1);
    ctx.gen_text(
        &mut r.cp_description,
        RS_CP_DESCRIPTION / 2,
        RS_CP_DESCRIPTION - 1,
        CP_DESCRIPTION,
    );

    let info = info_arr.row_start(CATALOG_PAGE, ctx.tdef(CATALOG_PAGE).k_null_bit_map);
    info.append_key(r.cp_catalog_page_sk, CP_CATALOG_PAGE_SK);
    info.append_varchar(&r.cp_catalog_page_id, CP_CATALOG_PAGE_ID);
    info.append_key(r.cp_start_date_id, CP_START_DATE_ID);
    info.append_key(r.cp_end_date_id, CP_END_DATE_ID);
    info.append_varchar(&r.cp_department, CP_DEPARTMENT);
    info.append_integer(r.cp_catalog_number, CP_CATALOG_NUMBER);
    info.append_integer(r.cp_catalog_page_number, CP_CATALOG_PAGE_NUMBER);
    info.append_varchar(&r.cp_description, CP_DESCRIPTION);
    info.append_varchar(r.cp_type, CP_TYPE);
    info.row_end();

    ctx.catalog_page = st;
}
