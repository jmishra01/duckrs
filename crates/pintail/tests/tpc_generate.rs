//! `dbgen(...)` and `dsdgen(...)` in a FROM clause: the scan gives no rows, and its run creates the benchmark's
//! tables and fills them, as `CALL dbgen(...)` does.

use pintail::{Database, Value};

fn count(connection: &mut pintail::Connection, sql: &str) -> Value {
    connection.query(sql).unwrap().value(0, 0)
}

#[test]
fn dbgen_in_a_from_clause_fills_the_tables() {
    let db = Database::open_in_memory().unwrap();
    let mut connection = db.connect().unwrap();
    // the scan itself has one column and no rows
    let result = connection.query("SELECT * FROM dbgen(sf = 0.01)").unwrap();
    assert_eq!(result.names, vec!["Success"]);
    assert_eq!(result.row_count(), 0);
    // the same rows as CALL dbgen(sf = 0.01)
    assert_eq!(
        count(&mut connection, "SELECT count(*) FROM lineitem"),
        Value::bigint(60175)
    );
    assert_eq!(count(&mut connection, "SELECT count(*) FROM region"), Value::bigint(5));
    // committed: another connection sees them
    let mut other = db.connect().unwrap();
    assert_eq!(count(&mut other, "SELECT count(*) FROM orders"), Value::bigint(15000));
    // run again, it appends again (the tables are IF NOT EXISTS)
    connection.query("SELECT count(*) FROM dbgen(sf = 0.01)").unwrap();
    assert_eq!(count(&mut connection, "SELECT count(*) FROM region"), Value::bigint(10));
}

#[test]
fn dbgen_in_a_from_clause_takes_its_options() {
    let db = Database::open_in_memory().unwrap();
    let mut connection = db.connect().unwrap();
    connection.query("CREATE SCHEMA tpch").unwrap();
    connection
        .query("SELECT * FROM dbgen(sf = 0, schema = 'tpch', suffix = '_x')")
        .unwrap();
    assert_eq!(
        count(&mut connection, "SELECT count(*) FROM tpch.lineitem_x"),
        Value::bigint(0)
    );
    let error = connection
        .query("SELECT * FROM dbgen(sf = 0.01, children = 2)")
        .unwrap_err();
    assert!(
        error.message.contains("Step must be defined when children are defined"),
        "{error}"
    );
    let error = connection.query("SELECT * FROM dbgen(sf = NULL)").unwrap_err();
    assert!(
        error.message.contains("Cannot use NULL as function argument"),
        "{error}"
    );
}

#[test]
fn dsdgen_in_a_from_clause_fills_the_tables() {
    let db = Database::open_in_memory().unwrap();
    let mut connection = db.connect().unwrap();
    connection.query("SELECT * FROM dsdgen(sf = 0.01)").unwrap();
    assert_eq!(
        count(&mut connection, "SELECT count(*) FROM store_sales"),
        Value::bigint(28810)
    );
    // an existing table is an error, or replaced with overwrite
    assert!(connection.query("SELECT * FROM dsdgen(sf = 0)").is_err());
    connection
        .query("SELECT * FROM dsdgen(sf = 0, overwrite = true)")
        .unwrap();
    assert_eq!(
        count(&mut connection, "SELECT count(*) FROM store_sales"),
        Value::bigint(0)
    );
}

#[test]
fn dbgen_in_a_from_clause_refuses_a_read_only_database() {
    let dir = std::env::temp_dir().join(format!("pintail_tpc_generate_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("read_only.db");
    {
        let db = Database::open(&file, false).unwrap();
        db.connect().unwrap().query("CREATE TABLE t (i INTEGER)").unwrap();
    }
    let db = Database::open(&file, true).unwrap();
    let error = db.connect().unwrap().query("SELECT * FROM dbgen(sf = 0)").unwrap_err();
    assert!(error.message.contains("read-only"), "{error}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
