// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_catalog_sales.cpp @ rust-port-baseline

//! The `catalog_sales` table, which also makes the `catalog_returns` rows.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::permute::get_permutation_entry;
use super::pricing::Pricing;
use super::tables::*;
use super::w_catalog_returns::{CatalogReturnsTbl, mk_w_catalog_returns};
use crate::append_info::AppendInfo;

/// `struct W_CATALOG_SALES_TBL`
#[derive(Clone, Default)]
pub(crate) struct CatalogSalesTbl {
    pub(crate) cs_sold_date_sk: i64,
    pub(crate) cs_sold_time_sk: i64,
    pub(crate) cs_ship_date_sk: i64,
    pub(crate) cs_bill_customer_sk: i64,
    pub(crate) cs_bill_cdemo_sk: i64,
    pub(crate) cs_bill_hdemo_sk: i64,
    pub(crate) cs_bill_addr_sk: i64,
    pub(crate) cs_ship_customer_sk: i64,
    pub(crate) cs_ship_cdemo_sk: i64,
    pub(crate) cs_ship_hdemo_sk: i64,
    pub(crate) cs_ship_addr_sk: i64,
    pub(crate) cs_call_center_sk: i64,
    pub(crate) cs_catalog_page_sk: i64,
    pub(crate) cs_ship_mode_sk: i64,
    pub(crate) cs_warehouse_sk: i64,
    pub(crate) cs_sold_item_sk: i64,
    pub(crate) cs_promo_sk: i64,
    pub(crate) cs_order_number: i64,
    pub(crate) cs_pricing: Pricing,
}

/// `g_w_catalog_sales` and the statics of `w_catalog_sales.cpp`.
pub(crate) struct State {
    r: CatalogSalesTbl,
    k_new_date_index: i64,
    j_date: i64,
    n_ticket_item_base: i32,
    item_permutation: Vec<i32>,
    n_item_count: i32,
    /// `mk_detail`'s own `kNewDateIndex`, which `skipDays` sets but nothing reads.
    detail_k_new_date_index: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            r: CatalogSalesTbl::default(),
            k_new_date_index: 0,
            j_date: 0,
            n_ticket_item_base: 1,
            item_permutation: Vec::new(),
            n_item_count: 0,
            detail_k_new_date_index: 0,
        }
    }
}

/// `mk_master`: the fields of an order, which its lines share.
fn mk_master(ctx: &mut Ctx, st: &mut State, index: i64) {
    if !ctx.init.mk_master_catalog_sales_init {
        st.j_date = ctx.skip_days(CATALOG_SALES, &mut st.k_new_date_index);
        st.n_item_count = ctx.get_id_count(ITEM) as i32;
        st.item_permutation = ctx.make_permutation(st.n_item_count, CS_PERMUTE);
        ctx.init.mk_master_catalog_sales_init = true;
    }
    while index > st.k_new_date_index {
        st.j_date += 1;
        st.k_new_date_index += ctx.date_scaling(CATALOG_SALES, st.j_date);
    }
    let r = &mut st.r;
    r.cs_sold_date_sk = st.j_date;
    r.cs_sold_time_sk = ctx.mk_join(CS_SOLD_TIME_SK, TIME, r.cs_call_center_sk);
    r.cs_call_center_sk = if r.cs_sold_date_sk == -1 {
        -1
    } else {
        ctx.mk_join(CS_CALL_CENTER_SK, CALL_CENTER, r.cs_sold_date_sk)
    };

    r.cs_bill_customer_sk = ctx.mk_join(CS_BILL_CUSTOMER_SK, CUSTOMER, 1);
    r.cs_bill_cdemo_sk = ctx.mk_join(CS_BILL_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.cs_bill_hdemo_sk = ctx.mk_join(CS_BILL_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.cs_bill_addr_sk = ctx.mk_join(CS_BILL_ADDR_SK, CUSTOMER_ADDRESS, 1);

    // most orders are for the ordering customer, some are not
    let n_gift_pct = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, CS_SHIP_CUSTOMER_SK);
    if n_gift_pct <= CS_GIFT_PCT {
        r.cs_ship_customer_sk = ctx.mk_join(CS_SHIP_CUSTOMER_SK, CUSTOMER, 2);
        r.cs_ship_cdemo_sk = ctx.mk_join(CS_SHIP_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 2);
        r.cs_ship_hdemo_sk = ctx.mk_join(CS_SHIP_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 2);
        r.cs_ship_addr_sk = ctx.mk_join(CS_SHIP_ADDR_SK, CUSTOMER_ADDRESS, 2);
    } else {
        r.cs_ship_customer_sk = r.cs_bill_customer_sk;
        r.cs_ship_cdemo_sk = r.cs_bill_cdemo_sk;
        r.cs_ship_hdemo_sk = r.cs_bill_hdemo_sk;
        r.cs_ship_addr_sk = r.cs_bill_addr_sk;
    }

    r.cs_order_number = index;
    st.n_ticket_item_base = ctx.genrand_integer(DIST_UNIFORM, 1, st.n_item_count, 0, CS_SOLD_ITEM_SK);
}

/// `mk_detail`: one line of the order, and its return, if it is returned.
fn mk_detail(ctx: &mut Ctx, st: &mut State, info_arr: &mut AppendInfo) {
    if !ctx.init.mk_detail_catalog_sales_init {
        ctx.skip_days(CATALOG_SALES, &mut st.detail_k_new_date_index);
        ctx.init.mk_detail_catalog_sales_init = true;
    }
    ctx.null_set(CATALOG_SALES, CS_NULLS);
    let r = &mut st.r;

    // orders are shipped some number of days after they are ordered
    let n_ship_lag = ctx.genrand_integer(DIST_UNIFORM, CS_MIN_SHIP_DELAY, CS_MAX_SHIP_DELAY, 0, CS_SHIP_DATE_SK);
    r.cs_ship_date_sk = if r.cs_sold_date_sk == -1 {
        -1
    } else {
        r.cs_sold_date_sk + i64::from(n_ship_lag)
    };

    // items need to be unique within an order: use a sequence within the permutation
    st.n_ticket_item_base += 1;
    if st.n_ticket_item_base > st.n_item_count {
        st.n_ticket_item_base = 1;
    }
    let k_item = get_permutation_entry(&st.item_permutation, st.n_ticket_item_base);
    r.cs_sold_item_sk = ctx.match_scd_sk(k_item, r.cs_sold_date_sk, ITEM);

    // the catalog page needs to be from a catalog active at the time of the sale
    r.cs_catalog_page_sk = if r.cs_sold_date_sk == -1 {
        -1
    } else {
        ctx.mk_join(CS_CATALOG_PAGE_SK, CATALOG_PAGE, r.cs_sold_date_sk)
    };

    r.cs_ship_mode_sk = ctx.mk_join(CS_SHIP_MODE_SK, SHIP_MODE, 1);
    r.cs_warehouse_sk = ctx.mk_join(CS_WAREHOUSE_SK, WAREHOUSE, 1);
    r.cs_promo_sk = ctx.mk_join(CS_PROMO_SK, PROMOTION, 1);
    ctx.set_pricing(CS_PRICING, &mut r.cs_pricing);

    // a fraction of all orders are returned
    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, CR_IS_RETURNED);
    if n_temp < CR_RETURN_PCT {
        let mut rr = CatalogReturnsTbl::default();
        mk_w_catalog_returns(ctx, &mut rr, r);
        let info = info_arr.row_start(CATALOG_RETURNS, ctx.tdef(CATALOG_RETURNS).k_null_bit_map);
        info.append_key(rr.cr_returned_date_sk, CR_RETURNED_DATE_SK);
        info.append_key(rr.cr_returned_time_sk, CR_RETURNED_TIME_SK);
        info.append_key(rr.cr_item_sk, CR_ITEM_SK);
        info.append_key(rr.cr_refunded_customer_sk, CR_REFUNDED_CUSTOMER_SK);
        info.append_key(rr.cr_refunded_cdemo_sk, CR_REFUNDED_CDEMO_SK);
        info.append_key(rr.cr_refunded_hdemo_sk, CR_REFUNDED_HDEMO_SK);
        info.append_key(rr.cr_refunded_addr_sk, CR_REFUNDED_ADDR_SK);
        info.append_key(rr.cr_returning_customer_sk, CR_RETURNING_CUSTOMER_SK);
        info.append_key(rr.cr_returning_cdemo_sk, CR_RETURNING_CDEMO_SK);
        info.append_key(rr.cr_returning_hdemo_sk, CR_RETURNING_HDEMO_SK);
        info.append_key(rr.cr_returning_addr_sk, CR_RETURNING_ADDR_SK);
        info.append_key(rr.cr_call_center_sk, CR_CALL_CENTER_SK);
        info.append_key(rr.cr_catalog_page_sk, CR_CATALOG_PAGE_SK);
        info.append_key(rr.cr_ship_mode_sk, CR_SHIP_MODE_SK);
        info.append_key(rr.cr_warehouse_sk, CR_WAREHOUSE_SK);
        info.append_key(rr.cr_reason_sk, CR_REASON_SK);
        info.append_key(rr.cr_order_number, CR_ORDER_NUMBER);
        info.append_integer(rr.cr_pricing.quantity, CR_PRICING_QUANTITY);
        info.append_decimal(&rr.cr_pricing.net_paid, CR_PRICING_NET_PAID);
        info.append_decimal(&rr.cr_pricing.ext_tax, CR_PRICING_EXT_TAX);
        info.append_decimal(&rr.cr_pricing.net_paid_inc_tax, CR_PRICING_NET_PAID_INC_TAX);
        info.append_decimal(&rr.cr_pricing.fee, CR_PRICING_FEE);
        info.append_decimal(&rr.cr_pricing.ext_ship_cost, CR_PRICING_EXT_SHIP_COST);
        info.append_decimal(&rr.cr_pricing.refunded_cash, CR_PRICING_REFUNDED_CASH);
        info.append_decimal(&rr.cr_pricing.reversed_charge, CR_PRICING_REVERSED_CHARGE);
        info.append_decimal(&rr.cr_pricing.store_credit, CR_PRICING_STORE_CREDIT);
        info.append_decimal(&rr.cr_pricing.net_loss, CR_PRICING_NET_LOSS);
        info.row_end();
    }

    let info = info_arr.row_start(CATALOG_SALES, ctx.tdef(CATALOG_SALES).k_null_bit_map);
    info.append_key(r.cs_sold_date_sk, CS_SOLD_DATE_SK);
    info.append_key(r.cs_sold_time_sk, CS_SOLD_TIME_SK);
    info.append_key(r.cs_ship_date_sk, CS_SHIP_DATE_SK);
    info.append_key(r.cs_bill_customer_sk, CS_BILL_CUSTOMER_SK);
    info.append_key(r.cs_bill_cdemo_sk, CS_BILL_CDEMO_SK);
    info.append_key(r.cs_bill_hdemo_sk, CS_BILL_HDEMO_SK);
    info.append_key(r.cs_bill_addr_sk, CS_BILL_ADDR_SK);
    info.append_key(r.cs_ship_customer_sk, CS_SHIP_CUSTOMER_SK);
    info.append_key(r.cs_ship_cdemo_sk, CS_SHIP_CDEMO_SK);
    info.append_key(r.cs_ship_hdemo_sk, CS_SHIP_HDEMO_SK);
    info.append_key(r.cs_ship_addr_sk, CS_SHIP_ADDR_SK);
    info.append_key(r.cs_call_center_sk, CS_CALL_CENTER_SK);
    info.append_key(r.cs_catalog_page_sk, CS_CATALOG_PAGE_SK);
    info.append_key(r.cs_ship_mode_sk, CS_SHIP_MODE_SK);
    info.append_key(r.cs_warehouse_sk, CS_WAREHOUSE_SK);
    info.append_key(r.cs_sold_item_sk, CS_SOLD_ITEM_SK);
    info.append_key(r.cs_promo_sk, CS_PROMO_SK);
    info.append_key(r.cs_order_number, CS_ORDER_NUMBER);
    let p = &r.cs_pricing;
    info.append_integer(p.quantity, CS_PRICING_QUANTITY);
    info.append_decimal(&p.wholesale_cost, CS_PRICING_WHOLESALE_COST);
    info.append_decimal(&p.list_price, CS_PRICING_LIST_PRICE);
    info.append_decimal(&p.sales_price, CS_PRICING_SALES_PRICE);
    info.append_decimal(&p.ext_discount_amt, CS_PRICING_EXT_DISCOUNT_AMOUNT);
    info.append_decimal(&p.ext_sales_price, CS_PRICING_EXT_SALES_PRICE);
    info.append_decimal(&p.ext_wholesale_cost, CS_PRICING_EXT_WHOLESALE_COST);
    info.append_decimal(&p.ext_list_price, CS_PRICING_EXT_LIST_PRICE);
    info.append_decimal(&p.ext_tax, CS_PRICING_EXT_TAX);
    info.append_decimal(&p.coupon_amt, CS_PRICING_COUPON_AMT);
    info.append_decimal(&p.ext_ship_cost, CS_PRICING_EXT_SHIP_COST);
    info.append_decimal(&p.net_paid, CS_PRICING_NET_PAID);
    info.append_decimal(&p.net_paid_inc_tax, CS_PRICING_NET_PAID_INC_TAX);
    info.append_decimal(&p.net_paid_inc_ship, CS_PRICING_NET_PAID_INC_SHIP);
    info.append_decimal(&p.net_paid_inc_ship_tax, CS_PRICING_NET_PAID_INC_SHIP_TAX);
    info.append_decimal(&p.net_profit, CS_PRICING_NET_PROFIT);
    info.row_end();
}

/// `mk_w_catalog_sales`: order `index`, with 4 to 14 lines.
pub(crate) fn mk_w_catalog_sales(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.catalog_sales);
    mk_master(ctx, &mut st, index);
    let n_lineitems = ctx.genrand_integer(DIST_UNIFORM, 4, 14, 0, CS_ORDER_NUMBER);
    for _ in 1..=n_lineitems {
        mk_detail(ctx, &mut st, info_arr);
    }
    ctx.catalog_sales = st;
}
