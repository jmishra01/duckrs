//! Fast checks of the generator, the schema and the queries (the byte-for-byte comparison with the
//! C++ dbgen is `tests/oracle.rs`).

mod common;

use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::types::LogicalType;
use duckrs_tpch::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Keeps one generator, and so the text pool, alive, so the tests share one pool.
fn keep_text_pool() {
    static KEEP: OnceLock<Mutex<DBGenGenerator>> = OnceLock::new();
    KEEP.get_or_init(|| Mutex::new(DBGenGenerator::new(0.01, 1, None).unwrap()));
}

/// Every table of a generation rendered as `.tbl` text, checking the chunks on the way.
fn generate(sf: f64, children: u32, step: Option<u32>) -> HashMap<TpchTable, String> {
    keep_text_pool();
    let mut files: HashMap<TpchTable, Vec<u8>> = TpchTable::ALL.iter().map(|&t| (t, Vec::new())).collect();
    let mut generator = DBGenGenerator::new(sf, children, step).unwrap();
    let mut last_progress = generator.progress();
    while let Some((table, chunk)) = generator.next_chunk() {
        assert!(!chunk.is_empty() && chunk.len() <= STANDARD_VECTOR_SIZE);
        assert_eq!(chunk.types(), table.types());
        let progress = generator.progress();
        assert!(progress >= last_progress && progress <= 100.0);
        last_progress = progress;
        common::tbl::write_chunk(files.get_mut(&table).unwrap(), &chunk);
    }
    assert!(generator.is_finished());
    assert_eq!(generator.progress(), 100.0);
    files
        .into_iter()
        .map(|(t, bytes)| (t, String::from_utf8(bytes).unwrap()))
        .collect()
}

fn sf001() -> &'static HashMap<TpchTable, String> {
    static TABLES: OnceLock<HashMap<TpchTable, String>> = OnceLock::new();
    TABLES.get_or_init(|| generate(0.01, 1, None))
}

#[test]
fn row_counts_at_sf_001() {
    let expected = [
        (TpchTable::Region, 5),
        (TpchTable::Nation, 25),
        (TpchTable::Supplier, 100),
        (TpchTable::Customer, 1500),
        (TpchTable::Part, 2000),
        (TpchTable::PartSupp, 8000),
        (TpchTable::Orders, 15000),
        (TpchTable::Lineitem, 60175),
    ];
    for (table, rows) in expected {
        assert_eq!(sf001()[&table].lines().count(), rows, "{table}");
    }
}

#[test]
fn known_rows_at_sf_001() {
    let first = |t: TpchTable| sf001()[&t].lines().next().unwrap();
    let last = |t: TpchTable| sf001()[&t].lines().last().unwrap();
    assert_eq!(
        first(TpchTable::Region),
        "0|AFRICA|lar deposits. blithely final packages cajole. regular waters are final requests. regular accounts are according to "
    );
    assert_eq!(
        first(TpchTable::Nation),
        "0|ALGERIA|0| haggle. carefully final deposits detect slyly agai"
    );
    assert_eq!(
        first(TpchTable::Supplier),
        "1|Supplier#000000001| N kD4on9OM Ipw3,gf0JBoQDd7tgrzrddZ|17|27-918-335-1736|5755.94|each slyly above the careful"
    );
    assert_eq!(
        first(TpchTable::Customer),
        "1|Customer#000000001|IVhzIApeRb ot,c,E|15|25-989-741-2988|711.56|BUILDING|to the even, regular platelets. regular, ironic epitaphs nag e"
    );
    assert_eq!(
        first(TpchTable::Part),
        "1|goldenrod lavender spring chocolate lace|Manufacturer#1|Brand#13|PROMO BURNISHED COPPER|7|JUMBO PKG|901.00|ly. slyly ironi"
    );
    assert_eq!(
        first(TpchTable::Orders),
        "1|370|O|172799.49|1996-01-02|5-LOW|Clerk#000000951|0|nstructions sleep furiously among "
    );
    assert_eq!(
        first(TpchTable::Lineitem),
        "1|1552|93|1|17.00|24710.35|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|DELIVER IN PERSON|TRUCK|egular courts above the"
    );
    assert_eq!(
        last(TpchTable::Lineitem),
        "60000|836|3|6|45.00|78157.35|0.04|0.08|N|O|1995-07-23|1995-07-17|1995-07-24|DELIVER IN PERSON|TRUCK|ke final packages. carefully final fo"
    );
    assert_eq!(
        last(TpchTable::Orders),
        "60000|1426|P|299401.61|1995-04-21|2-HIGH|Clerk#000000194|0|usual frets use alongside of the furiou"
    );
    let customer_11 = sf001()[&TpchTable::Customer].lines().nth(10).unwrap();
    assert!(
        customer_11.starts_with("11|Customer#000000011|PkWS 3HlXqwTuzrKg633BEi|23|33-464-151-3439|-272.60|BUILDING|")
    );
}

#[test]
fn partial_generation_adds_up_to_the_whole() {
    // explicit children and steps skip the seeds ahead; the parts must join to the full tables
    let mut joined: HashMap<TpchTable, String> = HashMap::new();
    for step in 0..3 {
        for (table, text) in generate(0.01, 3, Some(step)) {
            joined.entry(table).or_default().push_str(&text);
        }
    }
    for table in TpchTable::ALL {
        assert!(joined[&table] == sf001()[&table], "{table} differs");
    }
    // a step past the last generates nothing
    assert!(generate(0.01, 3, Some(3)).values().all(String::is_empty));
}

#[test]
fn scale_factor_zero_generates_nothing() {
    let mut generator = DBGenGenerator::new(0.0, 1, None).unwrap();
    assert_eq!(generator.progress(), 100.0);
    assert!(generator.next_chunk().is_none());
    assert!(generator.is_finished());
    assert!(DBGenGenerator::new(-1.0, 1, None).unwrap().next().is_none());
}

#[test]
fn invalid_arguments() {
    let message = |r: Result<DBGenGenerator, duckrs_common::Error>| r.err().unwrap().to_string();
    assert!(message(DBGenGenerator::new(0.01, 4, None)).contains("Step must be defined when children are defined"));
    assert!(message(DBGenGenerator::new(f64::NAN, 1, None)).contains("DBGen requires a valid scale factor."));
    assert!(
        message(create_dbgen_generator(100001.0, 1, None))
            .contains("DBGen does not support a scale factor exceeding 100000.")
    );
}

#[test]
fn schema() {
    let schema = tpch_schema();
    let names: Vec<_> = schema.iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [
            "region", "nation", "supplier", "customer", "part", "partsupp", "orders", "lineitem"
        ]
    );
    let lineitem = TpchTable::Lineitem.schema();
    assert_eq!(lineitem.columns.len(), 16);
    assert_eq!(lineitem.columns[4].name, "l_quantity");
    assert_eq!(lineitem.columns[4].ty, LogicalType::decimal(15, 2));
    assert_eq!(lineitem.columns[10].ty, LogicalType::DATE);
    assert!(schema.iter().flat_map(|t| &t.columns).all(|c| c.not_null));
    assert_eq!(
        TpchTable::Nation.column_names(),
        ["n_nationkey", "n_name", "n_regionkey", "n_comment"]
    );
    assert_eq!(
        TpchTable::Nation.types(),
        [
            LogicalType::INTEGER,
            LogicalType::VARCHAR,
            LogicalType::INTEGER,
            LogicalType::VARCHAR
        ]
    );

    let types = TpchTable::Region.types();
    let good: Vec<(&str, &LogicalType)> = vec![
        ("R_REGIONKEY", &types[0]),
        ("r_name", &types[1]),
        ("r_comment", &types[2]),
    ];
    assert!(validate_tpch_table_schema(TpchTable::Region, "region", &good).is_ok());
    let err = validate_tpch_table_schema(TpchTable::Region, "region_x", &good[..2])
        .unwrap_err()
        .to_string();
    assert!(err.contains("TPC-H table \"region_x\" has an incompatible schema: expected 3 columns but found 2"));
    let bigint = LogicalType::BIGINT;
    let wrong_type = vec![
        ("r_regionkey", &bigint),
        ("r_name", &types[1]),
        ("r_comment", &types[2]),
    ];
    let err = validate_tpch_table_schema(TpchTable::Region, "region", &wrong_type)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("column \"r_regionkey\" has type BIGINT but expected INTEGER"),
        "{err}"
    );
}

#[test]
fn queries() {
    assert_eq!(TPCH_QUERIES_COUNT, 22);
    for n in 1..=22 {
        assert!(get_query(n).unwrap().to_lowercase().contains("select"));
        assert_eq!(get_query_for_sf(n, 1.0).unwrap(), get_query(n).unwrap());
    }
    assert!(
        get_query(0)
            .unwrap_err()
            .to_string()
            .contains("Out of range TPC-H query number 0")
    );
    assert!(get_query(23).is_err());
    assert!(get_query(1).unwrap().starts_with("SELECT\n    l_returnflag,"));

    let q11 = get_query_for_sf(11, 0.1).unwrap();
    assert!(q11.contains("* 0.0010000000") && !q11.contains("0.0001000000"));
    assert!(get_query_for_sf(11, 100.0).unwrap().contains("* 0.0000010000"));
    assert_eq!(get_query_for_sf(1, 0.5).unwrap(), get_query(1).unwrap());
    for sf in [0.0, -1.0, f64::NAN, 100001.0] {
        let err = get_query_for_sf(1, sf).unwrap_err().to_string();
        assert!(
            err.contains("TPC-H queries require a scale factor between 0 and 100000"),
            "{err}"
        );
    }
}

#[test]
fn answers() {
    for sf in TPCH_ANSWER_SCALE_FACTORS {
        for n in 1..=22 {
            assert!(!get_answer(sf, n).unwrap().is_empty());
        }
    }
    assert!(
        get_answer(0.01, 1)
            .unwrap()
            .starts_with("l_returnflag|l_linestatus|sum_qty|")
    );
    assert_ne!(get_answer(0.01, 1).unwrap(), get_answer(1.0, 1).unwrap());
    assert!(get_answer(1.0, 23).is_err());
    let err = get_answer(0.5, 1).unwrap_err().to_string();
    assert!(err.contains("Don't have TPC-H answers for SF 0.500000!"), "{err}");
}
