// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_catalog_returns.cpp @ rust-port-baseline

//! The `catalog_returns` table: the return of a catalog sales line, which `mk_detail` of
//! `catalog_sales` makes.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::pricing::Pricing;
use super::tables::*;
use super::w_catalog_sales::CatalogSalesTbl;

/// `struct W_CATALOG_RETURNS_TBL`
#[derive(Default)]
pub(crate) struct CatalogReturnsTbl {
    pub(crate) cr_returned_date_sk: i64,
    pub(crate) cr_returned_time_sk: i64,
    pub(crate) cr_item_sk: i64,
    pub(crate) cr_refunded_customer_sk: i64,
    pub(crate) cr_refunded_cdemo_sk: i64,
    pub(crate) cr_refunded_hdemo_sk: i64,
    pub(crate) cr_refunded_addr_sk: i64,
    pub(crate) cr_returning_customer_sk: i64,
    pub(crate) cr_returning_cdemo_sk: i64,
    pub(crate) cr_returning_hdemo_sk: i64,
    pub(crate) cr_returning_addr_sk: i64,
    pub(crate) cr_call_center_sk: i64,
    pub(crate) cr_catalog_page_sk: i64,
    pub(crate) cr_ship_mode_sk: i64,
    pub(crate) cr_warehouse_sk: i64,
    pub(crate) cr_reason_sk: i64,
    pub(crate) cr_order_number: i64,
    pub(crate) cr_pricing: Pricing,
}

/// `mk_w_catalog_returns`: the return of `sale`, the line just made. (The stand-alone mode of the
/// C code, which makes the sale itself, is not used.)
pub(crate) fn mk_w_catalog_returns(ctx: &mut Ctx, r: &mut CatalogReturnsTbl, sale: &CatalogSalesTbl) {
    ctx.null_set(CATALOG_RETURNS, CR_NULLS);

    // some of the information in the return is taken from the original sale
    r.cr_item_sk = sale.cs_sold_item_sk;
    r.cr_catalog_page_sk = sale.cs_catalog_page_sk;
    r.cr_order_number = sale.cs_order_number;
    r.cr_pricing = sale.cs_pricing;
    r.cr_refunded_customer_sk = sale.cs_bill_customer_sk;
    r.cr_refunded_cdemo_sk = sale.cs_bill_cdemo_sk;
    r.cr_refunded_hdemo_sk = sale.cs_bill_hdemo_sk;
    r.cr_refunded_addr_sk = sale.cs_bill_addr_sk;
    r.cr_call_center_sk = sale.cs_call_center_sk;

    // some of the fields are conditionally taken from the sale
    r.cr_returning_customer_sk = ctx.mk_join(CR_RETURNING_CUSTOMER_SK, CUSTOMER, 2);
    r.cr_returning_cdemo_sk = ctx.mk_join(CR_RETURNING_CDEMO_SK, CUSTOMER_DEMOGRAPHICS, 2);
    r.cr_returning_hdemo_sk = ctx.mk_join(CR_RETURNING_HDEMO_SK, HOUSEHOLD_DEMOGRAPHICS, 2);
    r.cr_returning_addr_sk = ctx.mk_join(CR_RETURNING_ADDR_SK, CUSTOMER_ADDRESS, 2);
    if ctx.genrand_integer(DIST_UNIFORM, 0, 99, 0, CR_RETURNING_CUSTOMER_SK) < CS_GIFT_PCT {
        r.cr_returning_customer_sk = sale.cs_ship_customer_sk;
        r.cr_returning_cdemo_sk = sale.cs_ship_cdemo_sk;
        // cr_returning_hdemo_sk keeps its own key, as in C
        r.cr_returning_addr_sk = sale.cs_ship_addr_sk;
    }

    // the rest of the columns are generated for this specific return
    r.cr_returned_date_sk = ctx.mk_join(CR_RETURNED_DATE_SK, DATET, sale.cs_ship_date_sk);
    r.cr_returned_time_sk = ctx.mk_join(CR_RETURNED_TIME_SK, TIME, 1);
    r.cr_ship_mode_sk = ctx.mk_join(CR_SHIP_MODE_SK, SHIP_MODE, 1);
    r.cr_warehouse_sk = ctx.mk_join(CR_WAREHOUSE_SK, WAREHOUSE, 1);
    r.cr_reason_sk = ctx.mk_join(CR_REASON_SK, REASON, 1);
    if sale.cs_pricing.quantity != -1 {
        r.cr_pricing.quantity = ctx.genrand_integer(DIST_UNIFORM, 1, sale.cs_pricing.quantity, 0, CR_PRICING);
    } else {
        r.cr_pricing.quantity = -1;
    }
    ctx.set_pricing(CR_PRICING, &mut r.cr_pricing);
}
