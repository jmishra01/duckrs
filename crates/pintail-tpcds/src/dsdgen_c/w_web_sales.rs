// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_web_sales.cpp @ rust-port-baseline

//! The `web_sales` table, which also makes the `web_returns` rows.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::permute::get_permutation_entry;
use super::pricing::Pricing;
use super::tables::*;
use super::w_web_returns::{WebReturnsTbl, mk_w_web_returns};
use crate::append_info::AppendInfo;

/// `struct W_WEB_SALES_TBL`
#[derive(Default)]
pub(crate) struct WebSalesTbl {
    pub(crate) ws_sold_date_sk: i64,
    pub(crate) ws_sold_time_sk: i64,
    pub(crate) ws_ship_date_sk: i64,
    pub(crate) ws_item_sk: i64,
    pub(crate) ws_bill_customer_sk: i64,
    pub(crate) ws_bill_cdemo_sk: i64,
    pub(crate) ws_bill_hdemo_sk: i64,
    pub(crate) ws_bill_addr_sk: i64,
    pub(crate) ws_ship_customer_sk: i64,
    pub(crate) ws_ship_cdemo_sk: i64,
    pub(crate) ws_ship_hdemo_sk: i64,
    pub(crate) ws_ship_addr_sk: i64,
    pub(crate) ws_web_page_sk: i64,
    pub(crate) ws_web_site_sk: i64,
    pub(crate) ws_ship_mode_sk: i64,
    pub(crate) ws_warehouse_sk: i64,
    pub(crate) ws_promo_sk: i64,
    pub(crate) ws_order_number: i64,
    pub(crate) ws_pricing: Pricing,
}

/// `g_w_web_sales` and the statics of `w_web_sales.cpp`.
#[derive(Default)]
pub(crate) struct State {
    r: WebSalesTbl,
    k_new_date_index: i64,
    j_date: i64,
    n_item_index: i32,
    /// `mk_master`'s `nItemCount`
    master_item_count: i32,
    /// `mk_detail`'s `pItemPermutation` and `nItemCount`
    item_permutation: Vec<i32>,
    detail_item_count: i32,
}

/// `mk_master`: the fields of an order, which its lines share.
fn mk_master(ctx: &mut Ctx, st: &mut State, index: i64) {
    if !ctx.init.mk_master_init {
        st.j_date = ctx.skip_days(WEB_SALES, &mut st.k_new_date_index);
        st.master_item_count = ctx.get_id_count(ITEM) as i32;
        ctx.init.mk_master_init = true;
    }
    // the sale date is drawn at random, so the day count only follows along
    while index > st.k_new_date_index {
        st.j_date += 1;
        st.k_new_date_index += ctx.date_scaling(WEB_SALES, st.j_date);
    }
    let r = &mut st.r;
    r.ws_sold_date_sk = ctx.mk_join(WS_SOLD_DATE_SK, DATET, 1);
    r.ws_sold_time_sk = ctx.mk_join(WS_SOLD_TIME_SK, TIME, 1);
    r.ws_bill_customer_sk = ctx.mk_join(WS_BILL_CUSTOMER_SK, CUSTOMER, 1);
    r.ws_bill_cdemo_sk = ctx.mk_join(WS_BILL_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.ws_bill_hdemo_sk = ctx.mk_join(WS_BILL_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.ws_bill_addr_sk = ctx.mk_join(WS_BILL_ADDR_SK, CUSTOMER_ADDRESS, 1);

    // most orders are for the ordering customers, some are not
    let n_gift_pct = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, WS_SHIP_CUSTOMER_SK);
    if n_gift_pct > WS_GIFT_PCT {
        r.ws_ship_customer_sk = ctx.mk_join(WS_SHIP_CUSTOMER_SK, CUSTOMER, 2);
        r.ws_ship_cdemo_sk = ctx.mk_join(WS_SHIP_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 2);
        r.ws_ship_hdemo_sk = ctx.mk_join(WS_SHIP_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 2);
        r.ws_ship_addr_sk = ctx.mk_join(WS_SHIP_ADDR_SK, CUSTOMER_ADDRESS, 2);
    } else {
        r.ws_ship_customer_sk = r.ws_bill_customer_sk;
        r.ws_ship_cdemo_sk = r.ws_bill_cdemo_sk;
        r.ws_ship_hdemo_sk = r.ws_bill_hdemo_sk;
        r.ws_ship_addr_sk = r.ws_bill_addr_sk;
    }

    r.ws_order_number = index;
    st.n_item_index = ctx.genrand_integer(DIST_UNIFORM, 1, st.master_item_count, 0, WS_ITEM_SK);
}

/// `mk_detail`: one line of the order, and its return, if it is returned.
fn mk_detail(ctx: &mut Ctx, st: &mut State, info_arr: &mut AppendInfo) {
    if !ctx.init.mk_detail_init {
        st.j_date = ctx.skip_days(WEB_SALES, &mut st.k_new_date_index);
        st.detail_item_count = ctx.get_id_count(ITEM) as i32;
        st.item_permutation = ctx.make_permutation(st.detail_item_count, WS_PERMUTATION);
        ctx.init.mk_detail_init = true;
    }
    let r = &mut st.r;
    ctx.null_set(WEB_SALES, WS_NULLS);

    // orders are shipped some number of days after they are ordered, and not all lineitems ship
    // at the same time
    let n_ship_lag = ctx.genrand_integer(DIST_UNIFORM, WS_MIN_SHIP_DELAY, WS_MAX_SHIP_DELAY, 0, WS_SHIP_DATE_SK);
    r.ws_ship_date_sk = r.ws_sold_date_sk + i64::from(n_ship_lag);

    st.n_item_index += 1;
    if st.n_item_index > st.detail_item_count {
        st.n_item_index = 1;
    }
    let item = get_permutation_entry(&st.item_permutation, st.n_item_index);
    r.ws_item_sk = ctx.match_scd_sk(item, r.ws_sold_date_sk, ITEM);

    // the web page needs to be valid for the sale date
    r.ws_web_page_sk = ctx.mk_join(WS_WEB_PAGE_SK, WEB_PAGE, r.ws_sold_date_sk);
    r.ws_web_site_sk = ctx.mk_join(WS_WEB_SITE_SK, WEB_SITE, r.ws_sold_date_sk);

    r.ws_ship_mode_sk = ctx.mk_join(WS_SHIP_MODE_SK, SHIP_MODE, 1);
    r.ws_warehouse_sk = ctx.mk_join(WS_WAREHOUSE_SK, WAREHOUSE, 1);
    r.ws_promo_sk = ctx.mk_join(WS_PROMO_SK, PROMOTION, 1);
    ctx.set_pricing(WS_PRICING, &mut r.ws_pricing);

    // a fraction of all sales are returned
    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, WR_IS_RETURNED);
    if n_temp < WR_RETURN_PCT {
        let mut rr = WebReturnsTbl::default();
        mk_w_web_returns(ctx, &mut rr, r);
        let info = info_arr.row_start(WEB_RETURNS, ctx.tdef(WEB_RETURNS).k_null_bit_map);
        info.append_key(rr.wr_returned_date_sk, WR_RETURNED_DATE_SK);
        info.append_key(rr.wr_returned_time_sk, WR_RETURNED_TIME_SK);
        info.append_key(rr.wr_item_sk, WR_ITEM_SK);
        info.append_key(rr.wr_refunded_customer_sk, WR_REFUNDED_CUSTOMER_SK);
        info.append_key(rr.wr_refunded_cdemo_sk, WR_REFUNDED_CDEMO_SK);
        info.append_key(rr.wr_refunded_hdemo_sk, WR_REFUNDED_HDEMO_SK);
        info.append_key(rr.wr_refunded_addr_sk, WR_REFUNDED_ADDR_SK);
        info.append_key(rr.wr_returning_customer_sk, WR_RETURNING_CUSTOMER_SK);
        info.append_key(rr.wr_returning_cdemo_sk, WR_RETURNING_CDEMO_SK);
        info.append_key(rr.wr_returning_hdemo_sk, WR_RETURNING_HDEMO_SK);
        info.append_key(rr.wr_returning_addr_sk, WR_RETURNING_ADDR_SK);
        info.append_key(rr.wr_web_page_sk, WR_WEB_PAGE_SK);
        info.append_key(rr.wr_reason_sk, WR_REASON_SK);
        info.append_key(rr.wr_order_number, WR_ORDER_NUMBER);
        let p = &rr.wr_pricing;
        info.append_integer(p.quantity, WR_PRICING_QUANTITY);
        info.append_decimal(&p.net_paid, WR_PRICING_NET_PAID);
        info.append_decimal(&p.ext_tax, WR_PRICING_EXT_TAX);
        info.append_decimal(&p.net_paid_inc_tax, WR_PRICING_NET_PAID_INC_TAX);
        info.append_decimal(&p.fee, WR_PRICING_FEE);
        info.append_decimal(&p.ext_ship_cost, WR_PRICING_EXT_SHIP_COST);
        info.append_decimal(&p.refunded_cash, WR_PRICING_REFUNDED_CASH);
        info.append_decimal(&p.reversed_charge, WR_PRICING_REVERSED_CHARGE);
        info.append_decimal(&p.store_credit, WR_PRICING_STORE_CREDIT);
        info.append_decimal(&p.net_loss, WR_PRICING_NET_LOSS);
        info.row_end();
    }

    let info = info_arr.row_start(WEB_SALES, ctx.tdef(WEB_SALES).k_null_bit_map);
    info.append_key(r.ws_sold_date_sk, WS_SOLD_DATE_SK);
    info.append_key(r.ws_sold_time_sk, WS_SOLD_TIME_SK);
    info.append_key(r.ws_ship_date_sk, WS_SHIP_DATE_SK);
    info.append_key(r.ws_item_sk, WS_ITEM_SK);
    info.append_key(r.ws_bill_customer_sk, WS_BILL_CUSTOMER_SK);
    info.append_key(r.ws_bill_cdemo_sk, WS_BILL_CDEMO_SK);
    info.append_key(r.ws_bill_hdemo_sk, WS_BILL_HDEMO_SK);
    info.append_key(r.ws_bill_addr_sk, WS_BILL_ADDR_SK);
    info.append_key(r.ws_ship_customer_sk, WS_SHIP_CUSTOMER_SK);
    info.append_key(r.ws_ship_cdemo_sk, WS_SHIP_CDEMO_SK);
    info.append_key(r.ws_ship_hdemo_sk, WS_SHIP_HDEMO_SK);
    info.append_key(r.ws_ship_addr_sk, WS_SHIP_ADDR_SK);
    info.append_key(r.ws_web_page_sk, WS_WEB_PAGE_SK);
    info.append_key(r.ws_web_site_sk, WS_WEB_SITE_SK);
    info.append_key(r.ws_ship_mode_sk, WS_SHIP_MODE_SK);
    info.append_key(r.ws_warehouse_sk, WS_WAREHOUSE_SK);
    info.append_key(r.ws_promo_sk, WS_PROMO_SK);
    info.append_key(r.ws_order_number, WS_ORDER_NUMBER);
    let p = &r.ws_pricing;
    info.append_integer(p.quantity, WS_PRICING_QUANTITY);
    info.append_decimal(&p.wholesale_cost, WS_PRICING_WHOLESALE_COST);
    info.append_decimal(&p.list_price, WS_PRICING_LIST_PRICE);
    info.append_decimal(&p.sales_price, WS_PRICING_SALES_PRICE);
    info.append_decimal(&p.ext_discount_amt, WS_PRICING_EXT_DISCOUNT_AMT);
    info.append_decimal(&p.ext_sales_price, WS_PRICING_EXT_SALES_PRICE);
    info.append_decimal(&p.ext_wholesale_cost, WS_PRICING_EXT_WHOLESALE_COST);
    info.append_decimal(&p.ext_list_price, WS_PRICING_EXT_LIST_PRICE);
    info.append_decimal(&p.ext_tax, WS_PRICING_EXT_TAX);
    info.append_decimal(&p.coupon_amt, WS_PRICING_COUPON_AMT);
    info.append_decimal(&p.ext_ship_cost, WS_PRICING_EXT_SHIP_COST);
    info.append_decimal(&p.net_paid, WS_PRICING_NET_PAID);
    info.append_decimal(&p.net_paid_inc_tax, WS_PRICING_NET_PAID_INC_TAX);
    info.append_decimal(&p.net_paid_inc_ship, WS_PRICING_NET_PAID_INC_SHIP);
    info.append_decimal(&p.net_paid_inc_ship_tax, WS_PRICING_NET_PAID_INC_SHIP_TAX);
    info.append_decimal(&p.net_profit, WS_PRICING_NET_PROFIT);
    info.row_end();
}

/// `mk_w_web_sales`: order `index`, with 8 to 16 lines.
pub(crate) fn mk_w_web_sales(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.web_sales);
    mk_master(ctx, &mut st, index);
    let n_lineitems = ctx.genrand_integer(DIST_UNIFORM, 8, 16, 9, WS_ORDER_NUMBER);
    for _ in 1..=n_lineitems {
        mk_detail(ctx, &mut st, info_arr);
    }
    ctx.web_sales = st;
}
