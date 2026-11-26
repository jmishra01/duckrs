// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/init.cpp @ rust-port-baseline

//! `InitConstants`: the "already initialized" flags of the generator's functions. The C code keeps
//! them in thread-locals and clears them with `InitConstants::Reset`; here a fresh [`Ctx`] starts
//! with them all clear.
//!
//! [`Ctx`]: super::Ctx

#[derive(Default, Clone)]
pub(crate) struct InitConstants {
    pub(crate) init_rand_init: bool,
    pub(crate) mk_address_init: bool,
    pub(crate) mk_w_call_center_init: bool,
    pub(crate) mk_w_catalog_page_init: bool,
    pub(crate) mk_master_catalog_sales_init: bool,
    pub(crate) mk_detail_catalog_sales_init: bool,
    pub(crate) skip_days_init: bool,
    pub(crate) mk_w_customer_init: bool,
    pub(crate) mk_w_date_init: bool,
    pub(crate) mk_w_inventory_init: bool,
    pub(crate) mk_w_item_init: bool,
    pub(crate) mk_w_promotion_init: bool,
    pub(crate) mk_master_store_sales_init: bool,
    pub(crate) mk_w_store_init: bool,
    pub(crate) mk_w_web_page_init: bool,
    pub(crate) mk_master_init: bool,
    pub(crate) mk_detail_init: bool,
    pub(crate) mk_w_web_site_init: bool,
    pub(crate) date_join_init: bool,
    pub(crate) cp_join_init: bool,
    pub(crate) web_join_init: bool,
    pub(crate) set_pricing_init: bool,
    pub(crate) get_rowcount_init: bool,
}
