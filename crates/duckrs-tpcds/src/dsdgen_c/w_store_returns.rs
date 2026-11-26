// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_store_returns.cpp @ rust-port-baseline

//! The `store_returns` table: the return of a store sales line, which `mk_detail` of
//! `store_sales` makes.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::pricing::Pricing;
use super::tables::*;
use super::w_store_sales::StoreSalesTbl;

/// `struct W_STORE_RETURNS_TBL`
#[derive(Default)]
pub(crate) struct StoreReturnsTbl {
    pub(crate) sr_returned_date_sk: i64,
    pub(crate) sr_returned_time_sk: i64,
    pub(crate) sr_item_sk: i64,
    pub(crate) sr_customer_sk: i64,
    pub(crate) sr_cdemo_sk: i64,
    pub(crate) sr_hdemo_sk: i64,
    pub(crate) sr_addr_sk: i64,
    pub(crate) sr_store_sk: i64,
    pub(crate) sr_reason_sk: i64,
    pub(crate) sr_ticket_number: i64,
    pub(crate) sr_pricing: Pricing,
}

/// `mk_w_store_returns`: the return of `sale`, the line just made.
pub(crate) fn mk_w_store_returns(ctx: &mut Ctx, r: &mut StoreReturnsTbl, sale: &StoreSalesTbl) {
    ctx.null_set(STORE_RETURNS, SR_NULLS);
    // some of the information in the return is taken from the original sale
    r.sr_ticket_number = sale.ss_ticket_number;
    r.sr_item_sk = sale.ss_sold_item_sk;
    r.sr_pricing = sale.ss_pricing;

    // some of the fields are conditionally taken from the sale
    r.sr_customer_sk = ctx.mk_join(SR_CUSTOMER_SK, CUSTOMER, 1);
    if ctx.genrand_integer(DIST_UNIFORM, 1, 100, 0, SR_TICKET_NUMBER) < SR_SAME_CUSTOMER {
        r.sr_customer_sk = sale.ss_sold_customer_sk;
    }

    // the rest of the columns are generated for this specific return; the time of the return
    // is during store hours
    r.sr_returned_date_sk = ctx.mk_join(SR_RETURNED_DATE_SK, DATET, sale.ss_sold_date_sk);
    r.sr_returned_time_sk =
        i64::from(ctx.genrand_integer(DIST_UNIFORM, (8 * 3600) - 1, (17 * 3600) - 1, 0, SR_RETURNED_TIME_SK));
    r.sr_cdemo_sk = ctx.mk_join(SR_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 1);
    r.sr_hdemo_sk = ctx.mk_join(SR_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 1);
    r.sr_addr_sk = ctx.mk_join(SR_ADDR_SK, CUSTOMER_ADDRESS, 1);
    r.sr_store_sk = ctx.mk_join(SR_STORE_SK, STORE, 1);
    r.sr_reason_sk = ctx.mk_join(SR_REASON_SK, REASON, 1);
    r.sr_pricing.quantity = ctx.genrand_integer(DIST_UNIFORM, 1, sale.ss_pricing.quantity, 0, SR_PRICING);
    ctx.set_pricing(SR_PRICING, &mut r.sr_pricing);
}
