// Ported from duckdb/extension/tpcds @ rust-port-baseline

//! TPC-DS for pintail: a pure-Rust port of DuckDB's `tpcds` extension — the dsdgen data
//! generator, the schema it fills, and the 99 queries with the answers DuckDB ships.
//!
//! The crate has no SQL side; `CALL dsdgen(...)`, `PRAGMA tpcds(n)`, `tpcds_queries()` and
//! `tpcds_answers()` are built on it elsewhere.
//!
//! # Schema (`DSDGenWrapper::CreateTPCDSSchema`)
//!
//! - [`Table`]: the 24 tables, in C++'s order ([`Table::ALL`]); [`Table::info`] gives a table's
//!   name, columns with their [`ColumnType`]s, and primary key ([`TableInfo`]).
//! - [`tpcds_schema(suffix, keys)`](tpcds_schema): the `CREATE TABLE` C++ runs, as data: one
//!   [`TableDefinition`] per table, named `name + suffix`, with a primary key constraint when
//!   `keys` is set. (C++'s `overwrite` option only picks `CREATE OR REPLACE`; the caller applies
//!   it.)
//!
//! # Data (`DSDGenWrapper::DSDGen`)
//!
//! - [`dsdgen(sf)`](dsdgen) returns a [`DsdGen`], an iterator of `(Table, DataChunk)`: the rows
//!   of every table, in chunks of at most [`STANDARD_VECTOR_SIZE`] rows whose columns have the
//!   table's types. Each table's chunks come in row order, and the tables come in C++'s order,
//!   except that a returns table is generated along with its sales table (C++ does the same), so
//!   its chunks come between that sales table's. A scale factor of 0 or less yields no rows, as in
//!   C++; NaN and scale factors above 100000 are errors.
//!
//! # Queries and answers
//!
//! - [`QUERIES_COUNT`], [`query(n)`](query) (`DSDGenWrapper::GetQuery`), and
//!   [`query_for_scale(n, sf)`](query_for_scale) (`GetQuery(query, sf)`, which scales the
//!   thresholds of query 9 to the scale factor).
//! - [`answer(sf, n)`](answer) (`DSDGenWrapper::GetAnswer`): the expected result of query `n` as
//!   the pipe-separated text DuckDB embeds, for scale factors 0.01 and 1.
//!   [`TPCDS_ANSWERS_SCALE_FACTORS`] are the scale factors C++'s `tpcds_answers()` lists.
//!
//! [`STANDARD_VECTOR_SIZE`]: pintail_common::STANDARD_VECTOR_SIZE

mod append_info;
mod dsdgen;
mod dsdgen_c;
mod dsdgen_schema;
mod queries;

pub use dsdgen::{DsdGen, dsdgen};
pub use queries::{QUERIES_COUNT, TPCDS_ANSWERS_SCALE_FACTORS, answer, query, query_for_scale};

use pintail_common::types::LogicalType;

/// The type of a TPC-DS column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnType {
    Integer,
    BigInt,
    Varchar,
    Date,
    /// `DECIMAL(width, scale)`
    Decimal(u8, u8),
}

impl ColumnType {
    pub fn logical_type(self) -> LogicalType {
        match self {
            ColumnType::Integer => LogicalType::INTEGER,
            ColumnType::BigInt => LogicalType::BIGINT,
            ColumnType::Varchar => LogicalType::VARCHAR,
            ColumnType::Date => LogicalType::DATE,
            ColumnType::Decimal(width, scale) => LogicalType::decimal(width, scale),
        }
    }
}

/// A table as C++'s `dsdgen_schema.hpp` declares it.
#[derive(Debug)]
pub struct TableInfo {
    pub name: &'static str,
    pub columns: &'static [(&'static str, ColumnType)],
    pub primary_key: &'static [&'static str],
}

/// The 24 TPC-DS tables, numbered as in dsdgen (`tables.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Table {
    CallCenter = 0,
    CatalogPage,
    CatalogReturns,
    CatalogSales,
    Customer,
    CustomerAddress,
    CustomerDemographics,
    DateDim,
    HouseholdDemographics,
    IncomeBand,
    Inventory,
    Item,
    Promotion,
    Reason,
    ShipMode,
    Store,
    StoreReturns,
    StoreSales,
    TimeDim,
    Warehouse,
    WebPage,
    WebReturns,
    WebSales,
    WebSite,
}

impl Table {
    /// All tables, in the order C++ creates them.
    pub const ALL: [Table; 24] = [
        Table::CallCenter,
        Table::CatalogPage,
        Table::CatalogReturns,
        Table::CatalogSales,
        Table::Customer,
        Table::CustomerAddress,
        Table::CustomerDemographics,
        Table::DateDim,
        Table::HouseholdDemographics,
        Table::IncomeBand,
        Table::Inventory,
        Table::Item,
        Table::Promotion,
        Table::Reason,
        Table::ShipMode,
        Table::Store,
        Table::StoreReturns,
        Table::StoreSales,
        Table::TimeDim,
        Table::Warehouse,
        Table::WebPage,
        Table::WebReturns,
        Table::WebSales,
        Table::WebSite,
    ];

    /// The table's columns, types and primary key.
    pub fn info(self) -> &'static TableInfo {
        &dsdgen_schema::TABLES[self as usize]
    }

    /// The table's name, such as `store_sales`.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// The types of the table's columns, in order.
    pub fn types(self) -> Vec<LogicalType> {
        self.info().columns.iter().map(|&(_, ty)| ty.logical_type()).collect()
    }

    /// The table of this name (without a suffix).
    pub fn from_name(name: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The returns table generated along with this sales table.
    pub fn child(self) -> Option<Table> {
        match self {
            Table::CatalogSales => Some(Table::CatalogReturns),
            Table::StoreSales => Some(Table::StoreReturns),
            Table::WebSales => Some(Table::WebReturns),
            _ => None,
        }
    }

    /// Whether the table is a returns table, which its sales table generates.
    pub fn is_child(self) -> bool {
        matches!(self, Table::CatalogReturns | Table::StoreReturns | Table::WebReturns)
    }

    pub(crate) fn from_id(id: i32) -> Table {
        Table::ALL[id as usize]
    }
}

/// A `CREATE TABLE` of [`tpcds_schema`].
#[derive(Clone, Debug, PartialEq)]
pub struct TableDefinition {
    pub table: Table,
    /// The table's name with the suffix.
    pub name: String,
    pub columns: Vec<(String, LogicalType)>,
    /// The columns of the primary key (`UniqueConstraint(columns, is_primary_key = true)`), when
    /// keys were asked for.
    pub primary_key: Option<Vec<String>>,
}

/// `DSDGenWrapper::CreateTPCDSSchema`: the 24 tables C++ creates, in its order, named with
/// `suffix`, with primary keys when `keys` is set.
pub fn tpcds_schema(suffix: &str, keys: bool) -> Vec<TableDefinition> {
    Table::ALL
        .iter()
        .map(|&table| {
            let info = table.info();
            TableDefinition {
                table,
                name: format!("{}{suffix}", info.name),
                columns: info
                    .columns
                    .iter()
                    .map(|&(name, ty)| (name.to_string(), ty.logical_type()))
                    .collect(),
                primary_key: keys.then(|| info.primary_key.iter().map(|c| c.to_string()).collect()),
            }
        })
        .collect()
}
