// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c @ rust-port-baseline

//! The TPC-DS data generator (dsdgen), ported file by file from `dsdgen-c`.
//!
//! The C code keeps its state in (thread-local) globals and function statics. Here it all lives in
//! one [`Ctx`], which every ported function takes, so generators are independent and safe to run
//! on any thread. Each `w_*` module keeps its table's row struct and statics in a `State` held by
//! [`Ctx`]; a builder takes that state out of the context while it runs, so it can call the support
//! functions on the context at the same time.

pub(crate) mod address;
pub(crate) mod build_support;
pub(crate) mod columns;
pub(crate) mod constants;
pub(crate) mod date;
pub(crate) mod decimal;
pub(crate) mod dist;
pub(crate) mod genrand;
pub(crate) mod init;
pub(crate) mod join;
pub(crate) mod nulls;
pub(crate) mod parallel;
pub(crate) mod permute;
pub(crate) mod pricing;
pub(crate) mod r_params;
pub(crate) mod scaling;
pub(crate) mod scd;
pub(crate) mod skip_days;
pub(crate) mod streams;
pub(crate) mod tables;
pub(crate) mod tdef_data;
pub(crate) mod tdef_functions;
pub(crate) mod tdefs;
pub(crate) mod text;
pub(crate) mod w_call_center;
pub(crate) mod w_catalog_page;
pub(crate) mod w_catalog_returns;
pub(crate) mod w_catalog_sales;
pub(crate) mod w_customer;
pub(crate) mod w_customer_address;
pub(crate) mod w_customer_demographics;
pub(crate) mod w_datetbl;
pub(crate) mod w_household_demographics;
pub(crate) mod w_income_band;
pub(crate) mod w_inventory;
pub(crate) mod w_item;
pub(crate) mod w_promotion;
pub(crate) mod w_reason;
pub(crate) mod w_ship_mode;
pub(crate) mod w_store;
pub(crate) mod w_store_returns;
pub(crate) mod w_store_sales;
pub(crate) mod w_timetbl;
pub(crate) mod w_warehouse;
pub(crate) mod w_web_page;
pub(crate) mod w_web_returns;
pub(crate) mod w_web_sales;
pub(crate) mod w_web_site;

use genrand::Rng;
use init::InitConstants;
use r_params::Params;
use tdefs::Tdef;

/// All of dsdgen's state: what the C code keeps in globals and function statics.
#[derive(Default)]
pub(crate) struct Ctx {
    pub(crate) params: Params,
    pub(crate) init: InitConstants,
    /// `Streams`: one random number stream per column.
    pub(crate) streams: Vec<Rng>,
    /// `w_tdefs`: the warehouse table definitions; the null bitmaps change per row.
    pub(crate) w_tdefs: Vec<Tdef>,
    /// The table of each column, as `getTableFromColumn` finds it.
    pub(crate) column_tables: Vec<i32>,
    pub(crate) scaling: scaling::State,
    pub(crate) scd: scd::State,
    pub(crate) address: address::State,
    pub(crate) join: join::State,
    pub(crate) hierarchy: build_support::HierarchyState,
    pub(crate) pricing: pricing::State,
    pub(crate) text: text::State,
    pub(crate) call_center: w_call_center::State,
    pub(crate) catalog_page: w_catalog_page::State,
    pub(crate) catalog_sales: w_catalog_sales::State,
    pub(crate) customer: w_customer::State,
    pub(crate) customer_address: w_customer_address::State,
    pub(crate) date: w_datetbl::State,
    pub(crate) inventory: w_inventory::State,
    pub(crate) item: w_item::State,
    pub(crate) promotion: w_promotion::State,
    pub(crate) store: w_store::State,
    pub(crate) store_sales: w_store_sales::State,
    pub(crate) warehouse: w_warehouse::State,
    pub(crate) web_page: w_web_page::State,
    pub(crate) web_sales: w_web_sales::State,
    pub(crate) web_site: w_web_site::State,
}
