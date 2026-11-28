// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/tdef_functions.cpp @ rust-port-baseline

//! `w_tdef_funcs`: the builder of each warehouse table.

use super::Ctx;
use super::tables::*;
use super::{
    w_call_center, w_catalog_page, w_catalog_sales, w_customer, w_customer_address, w_customer_demographics, w_datetbl,
    w_household_demographics, w_income_band, w_inventory, w_item, w_promotion, w_reason, w_ship_mode, w_store,
    w_store_sales, w_timetbl, w_warehouse, w_web_page, w_web_sales, w_web_site,
};
use crate::append_info::AppendInfo;

/// A table builder: makes row `index` (counting from 1) of its table, and of its child table.
pub(crate) type BuilderFn = fn(&mut Ctx, &mut AppendInfo, i64);

/// `getTdefFunctionsByNumber(table)->builder`. The child tables (the returns) have none: their
/// parents build them.
pub(crate) fn builder(table: i32) -> Option<BuilderFn> {
    Some(match table {
        CALL_CENTER => w_call_center::mk_w_call_center,
        CATALOG_PAGE => w_catalog_page::mk_w_catalog_page,
        CATALOG_SALES => w_catalog_sales::mk_w_catalog_sales,
        CUSTOMER => w_customer::mk_w_customer,
        CUSTOMER_ADDRESS => w_customer_address::mk_w_customer_address,
        CUSTOMER_DEMOGRAPHICS => w_customer_demographics::mk_w_customer_demographics,
        DATET => w_datetbl::mk_w_date,
        HOUSEHOLD_DEMOGRAPHICS => w_household_demographics::mk_w_household_demographics,
        INCOME_BAND => w_income_band::mk_w_income_band,
        INVENTORY => w_inventory::mk_w_inventory,
        ITEM => w_item::mk_w_item,
        PROMOTION => w_promotion::mk_w_promotion,
        REASON => w_reason::mk_w_reason,
        SHIP_MODE => w_ship_mode::mk_w_ship_mode,
        STORE => w_store::mk_w_store,
        STORE_SALES => w_store_sales::mk_w_store_sales,
        TIME => w_timetbl::mk_w_time,
        WAREHOUSE => w_warehouse::mk_w_warehouse,
        WEB_PAGE => w_web_page::mk_w_web_page,
        WEB_SALES => w_web_sales::mk_w_web_sales,
        WEB_SITE => w_web_site::mk_w_web_site,
        _ => return None,
    })
}
