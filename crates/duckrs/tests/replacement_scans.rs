//! Replacement scans of the database (`Database::add_replacement_scan`) and table descriptions
//! (`Connection::table_info`).

use duckrs::{Database, Error, ErrorKind, LogicalType, Replacement, Value};

fn column(result: &duckrs::QueryResult, column: usize) -> Vec<Value> {
    (0..result.row_count()).map(|row| result.value(column, row)).collect()
}

#[test]
fn database_replacement_scans_call_table_functions() {
    let db = Database::open_in_memory().unwrap();
    let mut early = db.connect().unwrap();
    db.add_replacement_scan(|name| {
        let count = name.strip_prefix("numbers_")?.parse::<i64>().ok()?;
        Some(Ok(Replacement::TableFunction {
            name: "range".into(),
            parameters: vec![Value::bigint(count)],
        }))
    });
    db.add_replacement_scan(|name| {
        (name == "broken").then(|| Err(Error::new(ErrorKind::Binder, "Error in replacement scan: broken\n")))
    });
    let mut late = db.connect().unwrap();
    // every connection sees the scan, the one made before it too
    let result = early.query("SELECT * FROM numbers_3").unwrap();
    assert_eq!(
        column(&result, 0),
        vec![Value::bigint(0), Value::bigint(1), Value::bigint(2)]
    );
    let result = late.query("SELECT numbers_2.range FROM numbers_2").unwrap();
    assert_eq!(column(&result, 0), vec![Value::bigint(0), Value::bigint(1)]);
    let result = late.query("SELECT n.x FROM numbers_1 AS n(x)").unwrap();
    assert_eq!(column(&result, 0), vec![Value::bigint(0)]);
    let error = late.query("SELECT * FROM broken").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Binder);
    assert!(late.query("SELECT * FROM numbers_x").is_err());
    // the connection's own scans come before the database's
    late.add_replacement_scan(|name| {
        (name == "numbers_3").then(|| Ok((vec!["c".into()], vec![LogicalType::INTEGER], Default::default())))
    });
    assert_eq!(late.query("SELECT * FROM numbers_3").unwrap().row_count(), 0);
    assert_eq!(early.query("SELECT * FROM numbers_3").unwrap().row_count(), 3);
}

#[test]
fn table_info_reads_the_columns() {
    let db = Database::open_in_memory().unwrap();
    let mut con = db.connect().unwrap();
    con.query("CREATE TABLE t (i INTEGER, j VARCHAR DEFAULT 'x', k AS (i * 2), l DATE)")
        .unwrap();
    con.query("CREATE VIEW v AS SELECT 1").unwrap();
    let table = con.table_info(None, None, "t").unwrap().unwrap();
    assert_eq!(
        (table.catalog.as_str(), table.schema.as_str(), table.table.as_str()),
        ("memory", "main", "t")
    );
    let columns: Vec<_> = table.columns.iter().map(|c| (c.name.as_str(), c.has_default)).collect();
    assert_eq!(columns, vec![("i", false), ("j", true), ("k", false), ("l", false)]);
    assert_eq!(table.columns[3].ty, LogicalType::DATE);
    assert!(con.table_info(None, None, "v").unwrap().is_none());
    assert!(con.table_info(None, Some("main"), "missing").unwrap().is_none());
    assert!(con.table_info(Some("no_such_catalog"), None, "t").is_err());
}
