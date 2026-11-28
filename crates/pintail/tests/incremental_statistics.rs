//! Statistics follow writes without reading the table again. If they say too little, a filter
//! proves a row can't match and it is gone without an error, so every step here compares what
//! filters find with and without them, on more than one row group.

use pintail::{Connection, Database};

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
}

fn rows(connection: &mut Connection, sql: &str) -> Vec<String> {
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let mut rows: Vec<String> = (0..result.row_count())
        .map(|row| {
            let values = result.collection.row(row);
            let fields: Vec<String> = values.iter().map(|v| v.try_to_string().unwrap_or_default()).collect();
            fields.join("|")
        })
        .collect();
    rows.sort();
    rows
}

const FILTERS: [&str; 11] = [
    "a = 1000000",
    "a = -1000000",
    "a > 999000",
    "a < -5",
    "a IS NULL",
    "a IS NOT NULL AND a BETWEEN 200 AND 210",
    "s = 'zzz'",
    "s = 'aaa'",
    "s > 'row 8'",
    "b IS NULL",
    "b = 77777",
];

fn check(connection: &mut Connection, step: &str) {
    for filter in FILTERS {
        let sql = format!("SELECT count(*), sum(a), min(s), max(b) FROM t WHERE {filter}");
        run(connection, "RESET disabled_optimizers");
        let with = rows(connection, &sql);
        run(connection, "SET disabled_optimizers = 'statistics_propagation'");
        let without = rows(connection, &sql);
        run(connection, "RESET disabled_optimizers");
        assert_eq!(with, without, "{step}: {filter}");
    }
}

/// Makes the values a table has known to its statistics and zones, and then asks after each step.
fn every_kind_of_write(connection: &mut Connection) {
    run(
        connection,
        "CREATE TABLE t AS SELECT i AS a, 'row ' || (i % 9) AS s, (i % 5)::INTEGER AS b FROM range(400000) r(i)",
    );
    check(connection, "at first");
    for (step, sql) in [
        (
            "insert past the bounds",
            "INSERT INTO t VALUES (1000000, 'zzz', 77777), (NULL, NULL, NULL)",
        ),
        ("update of one row far back", "UPDATE t SET a = -1000000 WHERE a = 10"),
        ("update to NULL", "UPDATE t SET b = NULL WHERE a BETWEEN 5 AND 6"),
        ("update of a string", "UPDATE t SET s = 'aaa' WHERE a = 300000"),
        ("delete", "DELETE FROM t WHERE a % 1000 = 7"),
        ("update after a delete", "UPDATE t SET a = 999500 WHERE a = 123456"),
        ("delete of what was updated", "DELETE FROM t WHERE a = -1000000"),
        ("update of what moved up", "UPDATE t SET b = 77777 WHERE a = 350000"),
        ("insert again", "INSERT INTO t SELECT -i, 'x', 1 FROM range(1, 10) r(i)"),
        ("all rows", "UPDATE t SET b = b + 1"),
    ] {
        run(connection, sql);
        check(connection, step);
    }
    run(connection, "BEGIN");
    run(connection, "UPDATE t SET a = 2000000 WHERE a = 1");
    run(connection, "INSERT INTO t VALUES (3000000, 'zzzz', 5)");
    let sql = "SELECT count(*) FROM t WHERE a >= 2000000";
    assert_eq!(rows(connection, sql), ["2"]);
    run(connection, "ROLLBACK");
    assert_eq!(rows(connection, sql), ["0"]);
    run(connection, "BEGIN");
    run(connection, "UPDATE t SET a = 2000000 WHERE a = 1");
    run(connection, "INSERT INTO t VALUES (3000000, 'zzzz', 5)");
    run(connection, "COMMIT");
    assert_eq!(rows(connection, sql), ["2"]);
    check(connection, "after a commit");
}

#[test]
fn statistics_keep_up_with_writes() {
    let database = Database::open_in_memory().unwrap();
    every_kind_of_write(&mut database.connect().unwrap());
}

#[test]
fn statistics_keep_up_with_keys() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    run(&mut connection, "CREATE TABLE u (id INTEGER PRIMARY KEY, v INTEGER)");
    run(&mut connection, "INSERT INTO u SELECT i, i FROM range(200000) r(i)");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM u WHERE v = 5"), ["1"]);
    run(
        &mut connection,
        "INSERT INTO u VALUES (5, 9000000), (900000, 9000001) ON CONFLICT (id) DO UPDATE SET v = excluded.v",
    );
    for (value, count) in [("9000000", "1"), ("9000001", "1"), ("5", "0")] {
        let sql = format!("SELECT count(*) FROM u WHERE v = {value}");
        assert_eq!(rows(&mut connection, &sql), [count], "{sql}");
    }
    run(&mut connection, "UPDATE u SET id = id + 1000000 WHERE id = 7");
    assert_eq!(rows(&mut connection, "SELECT v FROM u WHERE id = 1000007"), ["7"]);
    run(
        &mut connection,
        "MERGE INTO u USING (SELECT 9 AS id) s ON u.id = s.id WHEN MATCHED THEN UPDATE SET v = 8000000",
    );
    assert_eq!(rows(&mut connection, "SELECT id FROM u WHERE v = 8000000"), ["9"]);
}

/// Each connection has temporary objects of its own, which a rollback takes back.
#[test]
fn temporary_tables_are_of_a_connection() {
    let database = Database::open_in_memory().unwrap();
    let mut first = database.connect().unwrap();
    let mut second = database.connect().unwrap();
    run(&mut first, "CREATE TEMPORARY TABLE t (i INTEGER)");
    run(&mut second, "CREATE TEMPORARY TABLE t (s VARCHAR)");
    run(&mut first, "INSERT INTO t VALUES (1)");
    run(&mut second, "INSERT INTO t VALUES ('a'), ('b')");
    assert_eq!(rows(&mut first, "SELECT * FROM t"), ["1"]);
    assert_eq!(rows(&mut second, "SELECT count(*) FROM t"), ["2"]);
    run(&mut first, "BEGIN");
    run(&mut first, "INSERT INTO t VALUES (2)");
    run(&mut first, "CREATE TEMPORARY TABLE u (i INTEGER)");
    assert_eq!(rows(&mut first, "SELECT count(*) FROM t"), ["2"]);
    run(&mut first, "ROLLBACK");
    assert_eq!(rows(&mut first, "SELECT count(*) FROM t"), ["1"]);
    assert!(first.query("SELECT * FROM u").is_err());
    run(&mut first, "BEGIN");
    run(&mut first, "INSERT INTO t VALUES (3)");
    run(&mut first, "CREATE TEMPORARY TABLE u (i INTEGER)");
    run(&mut first, "COMMIT");
    assert_eq!(rows(&mut first, "SELECT count(*) FROM t"), ["2"]);
    assert!(rows(&mut first, "SELECT * FROM u").is_empty());
    assert!(second.query("SELECT * FROM u").is_err());
    run(&mut second, "DROP TABLE t");
    assert_eq!(rows(&mut first, "SELECT count(*) FROM t"), ["2"]);
    let third = database.connect();
    assert!(third.unwrap().query("SELECT * FROM t").is_err());
}
