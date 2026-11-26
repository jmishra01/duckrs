// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_web_returns.cpp @ rust-port-baseline

//! The `web_returns` table: the return of a web sales line, which `mk_detail` of `web_sales`
//! makes.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::pricing::Pricing;
use super::tables::*;
use super::w_web_sales::WebSalesTbl;

/// `struct W_WEB_RETURNS_TBL`
#[derive(Default)]
pub(crate) struct WebReturnsTbl {
    pub(crate) wr_returned_date_sk: i64,
    pub(crate) wr_returned_time_sk: i64,
    pub(crate) wr_item_sk: i64,
    pub(crate) wr_refunded_customer_sk: i64,
    pub(crate) wr_refunded_cdemo_sk: i64,
    pub(crate) wr_refunded_hdemo_sk: i64,
    pub(crate) wr_refunded_addr_sk: i64,
    pub(crate) wr_returning_customer_sk: i64,
    pub(crate) wr_returning_cdemo_sk: i64,
    pub(crate) wr_returning_hdemo_sk: i64,
    pub(crate) wr_returning_addr_sk: i64,
    pub(crate) wr_web_page_sk: i64,
    pub(crate) wr_reason_sk: i64,
    pub(crate) wr_order_number: i64,
    pub(crate) wr_pricing: Pricing,
}

/// `mk_w_web_returns`: the return of `sale`, the line just made.
pub(crate) fn mk_w_web_returns(ctx: &mut Ctx, r: &mut WebReturnsTbl, sale: &WebSalesTbl) {
    ctx.null_set(WEB_RETURNS, WR_NULLS);

    // fields taken from the original sale
    r.wr_item_sk = sale.ws_item_sk;
    r.wr_order_number = sale.ws_order_number;
    r.wr_pricing = sale.ws_pricing;
    r.wr_web_page_sk = sale.ws_web_page_sk;

    // remaining fields are specific to this return
    r.wr_returned_date_sk = ctx.mk_join(WR_RETURNED_DATE_SK, DATET, sale.ws_ship_date_sk);
    r.wr_returned_time_sk = ctx.mk_join(WR_RETURNED_TIME_SK, TIME, 1);

    // items are usually returned to the people they were shipped to, but sometimes not
    r.wr_refunded_customer_sk = ctx.mk_join(WR_REFUNDED_CUSTOMER_SK, CUSTOMER, 1);
    r.wr_refunded_cdemo_sk = ctx.mk_join(WR_REFUNDED_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.wr_refunded_hdemo_sk = ctx.mk_join(WR_REFUNDED_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.wr_refunded_addr_sk = ctx.mk_join(WR_REFUNDED_ADDR_SK, CUSTOMER_ADDRESS, 1);
    if ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, WR_RETURNING_CUSTOMER_SK) < WS_GIFT_PCT {
        r.wr_refunded_customer_sk = sale.ws_ship_customer_sk;
        r.wr_refunded_cdemo_sk = sale.ws_ship_cdemo_sk;
        r.wr_refunded_hdemo_sk = sale.ws_ship_hdemo_sk;
        r.wr_refunded_addr_sk = sale.ws_ship_addr_sk;
    }
    r.wr_returning_customer_sk = r.wr_refunded_customer_sk;
    r.wr_returning_cdemo_sk = r.wr_refunded_cdemo_sk;
    r.wr_returning_hdemo_sk = r.wr_refunded_hdemo_sk;
    r.wr_returning_addr_sk = r.wr_refunded_addr_sk;

    r.wr_reason_sk = ctx.mk_join(WR_REASON_SK, REASON, 1);
    r.wr_pricing.quantity = ctx.genrand_integer(DIST_UNIFORM, 1, sale.ws_pricing.quantity, 0, WR_PRICING);
    ctx.set_pricing(WR_PRICING, &mut r.wr_pricing);
}
