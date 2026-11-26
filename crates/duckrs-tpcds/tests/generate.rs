//! The generator at scale factors 0 and 0.01: row counts, chunk shapes and some rows, as C++'s
//! dsdgen makes them (see `oracle.rs` for the full comparison).

mod common;

use duckrs_common::types::LogicalType;
use duckrs_tpcds::{Table, dsdgen, tpcds_schema};
use std::sync::OnceLock;

fn sf001() -> &'static [Vec<u8>] {
    static TABLES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    TABLES.get_or_init(|| common::render_all(0.01))
}

fn lines(table: Table) -> Vec<&'static str> {
    std::str::from_utf8(&sf001()[table as usize])
        .expect("the rows are UTF-8")
        .lines()
        .collect()
}

#[test]
fn row_counts_at_sf_0_01() {
    let expected = [
        (Table::CallCenter, 1),
        (Table::CatalogPage, 11718),
        (Table::CatalogReturns, 1377),
        (Table::CatalogSales, 14313),
        (Table::Customer, 1000),
        (Table::CustomerAddress, 500),
        (Table::CustomerDemographics, 19208),
        (Table::DateDim, 73049),
        (Table::HouseholdDemographics, 7200),
        (Table::IncomeBand, 20),
        (Table::Inventory, 23490),
        (Table::Item, 180),
        (Table::Promotion, 3),
        (Table::Reason, 1),
        (Table::ShipMode, 20),
        (Table::Store, 1),
        (Table::StoreReturns, 2829),
        (Table::StoreSales, 28810),
        (Table::TimeDim, 86400),
        (Table::Warehouse, 1),
        (Table::WebPage, 1),
        (Table::WebReturns, 685),
        (Table::WebSales, 7212),
        (Table::WebSite, 1),
    ];
    for (table, rows) in expected {
        assert_eq!(lines(table).len(), rows, "{}", table.name());
    }
}

#[test]
fn known_rows_at_sf_0_01() {
    let first = |t: Table| lines(t)[0];
    let last = |t: Table| *lines(t).last().unwrap();
    assert_eq!(
        first(Table::CallCenter),
        "1|AAAAAAAABAAAAAAA|1998-01-01|||2450952|NY Metro|large|2|1138|8AM-4PM|Bob Belcher|6|More than other \
         authori|Shared others could not count fully dollars. New members ca|Julius Tran|3|pri|6|cally|730|Ash \
         Hill|Boulevard|Suite 0|Midway|Williamson County|TN|31904|United States|-5.00|0.11"
    );
    assert_eq!(first(Table::Reason), "1|AAAAAAAABAAAAAAA|Package was damaged");
    assert_eq!(
        lines(Table::ShipMode)[1],
        "2|AAAAAAAACAAAAAAA|NEXT DAY|AIR|FEDEX|ldhM8IvpzHgdbBgDfI"
    );
    assert_eq!(lines(Table::IncomeBand)[1], "2|10001|20000");
    assert_eq!(
        first(Table::DateDim),
        "2415022|AAAAAAAAOKJNECAA|1900-01-02|0|1|1|1900|1|1|2|1|1900|1|1|Monday|1900Q1|N|N|Y|2415021|2415020|\
         2414657|2414930|N|N|N|N|N"
    );
    assert_eq!(
        last(Table::DateDim),
        "2488070|AAAAAAAAGAHPFCAA|2100-01-01|2400|10436|801|2100|4|1|1|1|2100|801|10436|Thursday|2100Q1|Y|N|N|\
         2488070|2488069|2487705|2487978|N|N|N|N|N"
    );
    assert_eq!(
        first(Table::Customer),
        "1|AAAAAAAABAAAAAAA|516|7135|446|2452238|2452208|Mr.|Javier|Lewis|Y|9|12|1936|CHILE||\
         Javier.Lewis@VFAxlnZEvOx.org|2452508"
    );
    assert_eq!(
        first(Table::StoreSales),
        "2451813|65495|128|6|15377|3428|339|1|2|1|79|11.41|18.71|2.80|99.54|221.20|901.39|1478.09|6.08|99.54|\
         121.66|127.74|-779.73"
    );
    assert_eq!(
        first(Table::StoreReturns),
        "2451984|46418|115|6|5494|4033|397|1|1|1|51|37.23|3.35|40.58|55.28|714.00|0.74|17.51|18.98|772.63"
    );
    assert_eq!(
        first(Table::CatalogReturns),
        "2450926|45816|49|601|10467|6189|83|601|10467|4703|83|1|106|2|1|1|2|47|3888.31|233.29|4121.60|91.23|\
         1348.90|3577.24|186.64|124.43|1673.42"
    );
    assert_eq!(
        last(Table::WebReturns),
        "2450946|53668|148|104|1778|5181|496|104|1778|5181|496|1|1|600|27|126.36|3.79|130.15|53.80|221.40|111.19|\
         8.49|6.68|278.99"
    );
    assert_eq!(first(Table::Inventory), "2450815|1|1|211");
    assert_eq!(last(Table::Inventory), "2452635|180|1|311");
}

#[test]
fn scale_factor_zero_generates_nothing() {
    assert_eq!(dsdgen(0.0).unwrap().count(), 0);
    assert_eq!(dsdgen(-1.0).unwrap().count(), 0);
    assert!(dsdgen(f64::NAN).is_err());
    assert!(dsdgen(100001.0).is_err());
}

#[test]
fn chunks_are_full_and_ordered() {
    // every table's chunks but its last are full, and a table's chunks are not interleaved with
    // other tables' except for a sales table and its returns
    let mut chunks: Vec<(Table, usize)> = Vec::new();
    for (table, chunk) in dsdgen(0.01).unwrap() {
        chunks.push((table, chunk.len()));
    }
    for table in Table::ALL {
        let sizes: Vec<usize> = chunks.iter().filter(|c| c.0 == table).map(|c| c.1).collect();
        if let Some((_, full)) = sizes.split_last() {
            assert!(
                full.iter().all(|&n| n == duckrs_common::STANDARD_VECTOR_SIZE),
                "{}",
                table.name()
            );
        }
    }
    let order: Vec<Table> = chunks.iter().map(|c| c.0).collect();
    let mut seen = Vec::new();
    for table in order {
        let parent = Table::ALL
            .into_iter()
            .find(|t| t.child() == Some(table))
            .unwrap_or(table);
        if seen.last() != Some(&parent) {
            assert!(!seen.contains(&parent), "{} comes back", parent.name());
            seen.push(parent);
        }
    }
}

#[test]
fn schema() {
    let tables = tpcds_schema("_1", true);
    assert_eq!(tables.len(), 24);
    assert_eq!(tables[0].name, "call_center_1");
    assert_eq!(tables[23].name, "web_site_1");
    let store_sales = &tables[Table::StoreSales as usize];
    assert_eq!(store_sales.columns.len(), 23);
    assert_eq!(
        store_sales.columns[0],
        ("ss_sold_date_sk".to_string(), LogicalType::BIGINT)
    );
    assert_eq!(
        store_sales.columns[22],
        ("ss_net_profit".to_string(), LogicalType::decimal(7, 2))
    );
    assert_eq!(
        store_sales.primary_key.as_deref(),
        Some(&["ss_item_sk".to_string(), "ss_ticket_number".to_string()][..])
    );
    assert!(tpcds_schema("", false).iter().all(|t| t.primary_key.is_none()));
    assert_eq!(Table::from_name("time_dim"), Some(Table::TimeDim));
}
