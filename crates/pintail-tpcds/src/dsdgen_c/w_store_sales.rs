// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_store_sales.cpp @ rust-port-baseline

//! The `store_sales` table, which also makes the `store_returns` rows.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::permute::get_permutation_entry;
use super::pricing::Pricing;
use super::tables::*;
use super::w_store_returns::{StoreReturnsTbl, mk_w_store_returns};
use crate::append_info::AppendInfo;

/// `struct W_STORE_SALES_TBL`
#[derive(Default)]
pub(crate) struct StoreSalesTbl {
    pub(crate) ss_sold_date_sk: i64,
    pub(crate) ss_sold_time_sk: i64,
    pub(crate) ss_sold_item_sk: i64,
    pub(crate) ss_sold_customer_sk: i64,
    pub(crate) ss_sold_cdemo_sk: i64,
    pub(crate) ss_sold_hdemo_sk: i64,
    pub(crate) ss_sold_addr_sk: i64,
    pub(crate) ss_sold_store_sk: i64,
    pub(crate) ss_sold_promo_sk: i64,
    pub(crate) ss_ticket_number: i64,
    pub(crate) ss_pricing: Pricing,
}

/// `g_w_store_sales` and the statics of `w_store_sales.cpp`.
#[derive(Default)]
pub(crate) struct State {
    r: StoreSalesTbl,
    item_permutation: Vec<i32>,
    n_item_count: i32,
    n_item_index: i32,
    j_date: i64,
    /// `mk_master`'s own `kNewDateIndex`
    k_new_date_index: i64,
}

/// `mk_master`: the fields of a ticket, which its lines share.
fn mk_master(ctx: &mut Ctx, st: &mut State, index: i64) {
    if !ctx.init.mk_master_store_sales_init {
        st.j_date = ctx.skip_days(STORE_SALES, &mut st.k_new_date_index);
        st.n_item_count = ctx.get_id_count(ITEM) as i32;
        st.item_permutation = ctx.make_permutation(st.n_item_count, SS_PERMUTATION);
        ctx.init.mk_master_store_sales_init = true;
    }
    // the sale date is drawn at random, so the day count only follows along
    while index > st.k_new_date_index {
        st.j_date += 1;
        st.k_new_date_index += ctx.date_scaling(STORE_SALES, st.j_date);
    }
    let r = &mut st.r;
    r.ss_sold_store_sk = ctx.mk_join(SS_SOLD_STORE_SK, STORE, 1);
    r.ss_sold_time_sk = ctx.mk_join(SS_SOLD_TIME_SK, TIME, 1);
    r.ss_sold_date_sk = ctx.mk_join(SS_SOLD_DATE_SK, DATET, 1);
    r.ss_sold_customer_sk = ctx.mk_join(SS_SOLD_CUSTOMER_SK, CUSTOMER, 1);
    r.ss_sold_cdemo_sk = ctx.mk_join(SS_SOLD_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.ss_sold_hdemo_sk = ctx.mk_join(SS_SOLD_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.ss_sold_addr_sk = ctx.mk_join(SS_SOLD_ADDR_SK, CUSTOMER_ADDRESS, 1);
    r.ss_ticket_number = index;
    st.n_item_index = ctx.genrand_integer(DIST_UNIFORM, 1, st.n_item_count, 0, SS_SOLD_ITEM_SK);
}

/// `mk_detail`: one line of the ticket, and its return, if it is returned.
fn mk_detail(ctx: &mut Ctx, st: &mut State, info_arr: &mut AppendInfo) {
    ctx.null_set(STORE_SALES, SS_NULLS);
    let r = &mut st.r;
    // items need to be unique within an order: use a sequence within the permutation
    st.n_item_index += 1;
    if st.n_item_index > st.n_item_count {
        st.n_item_index = 1;
    }
    let item = get_permutation_entry(&st.item_permutation, st.n_item_index);
    r.ss_sold_item_sk = ctx.match_scd_sk(item, r.ss_sold_date_sk, ITEM);
    r.ss_sold_promo_sk = ctx.mk_join(SS_SOLD_PROMO_SK, PROMOTION, 1);
    ctx.set_pricing(SS_PRICING, &mut r.ss_pricing);

    // a fraction of all sales are returned
    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, SR_IS_RETURNED);
    if n_temp < SR_RETURN_PCT {
        let mut rr = StoreReturnsTbl::default();
        mk_w_store_returns(ctx, &mut rr, r);
        let info = info_arr.row_start(STORE_RETURNS, ctx.tdef(STORE_RETURNS).k_null_bit_map);
        info.append_key(rr.sr_returned_date_sk, SR_RETURNED_DATE_SK);
        info.append_key(rr.sr_returned_time_sk, SR_RETURNED_TIME_SK);
        info.append_key(rr.sr_item_sk, SR_ITEM_SK);
        info.append_key(rr.sr_customer_sk, SR_CUSTOMER_SK);
        info.append_key(rr.sr_cdemo_sk, SR_CDEMO_SK);
        info.append_key(rr.sr_hdemo_sk, SR_HDEMO_SK);
        info.append_key(rr.sr_addr_sk, SR_ADDR_SK);
        info.append_key(rr.sr_store_sk, SR_STORE_SK);
        info.append_key(rr.sr_reason_sk, SR_REASON_SK);
        info.append_key(rr.sr_ticket_number, SR_TICKET_NUMBER);
        let p = &rr.sr_pricing;
        info.append_integer(p.quantity, SR_PRICING_QUANTITY);
        info.append_decimal(&p.net_paid, SR_PRICING_NET_PAID);
        info.append_decimal(&p.ext_tax, SR_PRICING_EXT_TAX);
        info.append_decimal(&p.net_paid_inc_tax, SR_PRICING_NET_PAID_INC_TAX);
        info.append_decimal(&p.fee, SR_PRICING_FEE);
        info.append_decimal(&p.ext_ship_cost, SR_PRICING_EXT_SHIP_COST);
        info.append_decimal(&p.refunded_cash, SR_PRICING_REFUNDED_CASH);
        info.append_decimal(&p.reversed_charge, SR_PRICING_REVERSED_CHARGE);
        info.append_decimal(&p.store_credit, SR_PRICING_STORE_CREDIT);
        info.append_decimal(&p.net_loss, SR_PRICING_NET_LOSS);
        info.row_end();
    }

    let info = info_arr.row_start(STORE_SALES, ctx.tdef(STORE_SALES).k_null_bit_map);
    info.append_key(r.ss_sold_date_sk, SS_SOLD_DATE_SK);
    info.append_key(r.ss_sold_time_sk, SS_SOLD_TIME_SK);
    info.append_key(r.ss_sold_item_sk, SS_SOLD_ITEM_SK);
    info.append_key(r.ss_sold_customer_sk, SS_SOLD_CUSTOMER_SK);
    info.append_key(r.ss_sold_cdemo_sk, SS_SOLD_CDEMO_SK);
    info.append_key(r.ss_sold_hdemo_sk, SS_SOLD_HDEMO_SK);
    info.append_key(r.ss_sold_addr_sk, SS_SOLD_ADDR_SK);
    info.append_key(r.ss_sold_store_sk, SS_SOLD_STORE_SK);
    info.append_key(r.ss_sold_promo_sk, SS_SOLD_PROMO_SK);
    info.append_key(r.ss_ticket_number, SS_TICKET_NUMBER);
    let p = &r.ss_pricing;
    info.append_integer(p.quantity, SS_PRICING_QUANTITY);
    info.append_decimal(&p.wholesale_cost, SS_PRICING_WHOLESALE_COST);
    info.append_decimal(&p.list_price, SS_PRICING_LIST_PRICE);
    info.append_decimal(&p.sales_price, SS_PRICING_SALES_PRICE);
    info.append_decimal(&p.coupon_amt, SS_PRICING_COUPON_AMT);
    info.append_decimal(&p.ext_sales_price, SS_PRICING_EXT_SALES_PRICE);
    info.append_decimal(&p.ext_wholesale_cost, SS_PRICING_EXT_WHOLESALE_COST);
    info.append_decimal(&p.ext_list_price, SS_PRICING_EXT_LIST_PRICE);
    info.append_decimal(&p.ext_tax, SS_PRICING_EXT_TAX);
    info.append_decimal(&p.coupon_amt, SS_PRICING_COUPON_AMT);
    info.append_decimal(&p.net_paid, SS_PRICING_NET_PAID);
    info.append_decimal(&p.net_paid_inc_tax, SS_PRICING_NET_PAID_INC_TAX);
    info.append_decimal(&p.net_profit, SS_PRICING_NET_PROFIT);
    info.row_end();
}

/// `mk_w_store_sales`: ticket `index`, with 8 to 16 lines.
pub(crate) fn mk_w_store_sales(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.store_sales);
    mk_master(ctx, &mut st, index);
    let n_lineitems = ctx.genrand_integer(DIST_UNIFORM, 8, 16, 0, SS_TICKET_NUMBER);
    for _ in 1..=n_lineitems {
        mk_detail(ctx, &mut st, info_arr);
    }
    ctx.store_sales = st;
}
