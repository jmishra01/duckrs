//! Writes files and reads them back. `tools/check_written.py` has the real DuckDB read one of them.

use duckrs::{Connection, Database};
use std::path::PathBuf;

fn path(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("duckrs-write-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
}

fn rows(connection: &mut Connection, sql: &str) -> Vec<String> {
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.row_count())
        .map(|row| {
            let fields: Vec<String> = result
                .collection
                .row(row)
                .iter()
                .map(|v| {
                    if v.is_null() {
                        "NULL".to_string()
                    } else {
                        v.try_to_string().unwrap()
                    }
                })
                .collect();
            fields.join("|")
        })
        .collect()
}

const SCHEMA: &[&str] = &[
    "CREATE SCHEMA other",
    "CREATE TYPE mood AS ENUM ('a', 'b', 'c')",
    "CREATE SEQUENCE counter START 10 INCREMENT BY 2",
    "CREATE TABLE types (b BOOLEAN, t TINYINT, s SMALLINT, i INTEGER, l BIGINT, h HUGEINT, u UBIGINT, f FLOAT, \
     d DOUBLE, dec DECIMAL(18,4), da DATE, ts TIMESTAMP, ti TIME, iv INTERVAL, v VARCHAR, bl BLOB, m mood, \
     li INTEGER[], st STRUCT(a INTEGER, b VARCHAR), mp MAP(VARCHAR, INTEGER), ar INTEGER[2])",
    "INSERT INTO types VALUES (true, 1, 2, 3, 4, 170141183460469231731687303715884105727, 18446744073709551615, 1.5, \
     2.5, 1.25, DATE '2020-01-02', TIMESTAMP '2020-01-02 03:04:05', TIME '01:02:03', INTERVAL 1 DAY, 'text', \
     'blob'::BLOB, 'b', [1, NULL, 3], {'a': 1, 'b': 'x'}, MAP {'k': 1}, [1, 2])",
    "INSERT INTO types VALUES (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
     NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    "INSERT INTO types VALUES (false, -1, -2, -3, -4, -5, 0, -1.5, -2.5, -1.25, DATE '1999-12-31', \
     TIMESTAMP '1999-12-31 23:59:59', TIME '23:59:59', INTERVAL 2 MONTH, '', ''::BLOB, 'c', [], {'a': NULL, 'b': NULL}, \
     MAP {}, [NULL, 4])",
    "CREATE TABLE big AS SELECT i::BIGINT AS i, 'value ' || (i % 7) AS s, CASE WHEN i % 3 = 0 THEN NULL ELSE i END AS n \
     FROM range(250000) r(i)",
    "CREATE TABLE long_strings AS SELECT i, repeat('x', i * 3000) AS s FROM range(5) r(i)",
    "CREATE TABLE empty_table (i INTEGER, s VARCHAR)",
    "CREATE TABLE other.t (x INTEGER NOT NULL, y INTEGER DEFAULT 5, z INTEGER GENERATED ALWAYS AS (x + y), CHECK (x > 0))",
    "INSERT INTO other.t (x) VALUES (1), (2)",
    "CREATE VIEW v AS SELECT i, count(*) AS c FROM big WHERE i < 10 GROUP BY i ORDER BY i",
    "CREATE MACRO twice(x) AS x * 2",
    "CREATE MACRO add_default(a, b := 1) AS a + b",
    "CREATE MACRO evens() AS TABLE SELECT i FROM big WHERE i % 2 = 0 LIMIT 3",
];

const QUERIES: &[&str] = &[
    "SELECT * FROM types ORDER BY i NULLS LAST",
    "SELECT count(*), count(n), sum(i), min(s), max(s) FROM big",
    "SELECT * FROM big WHERE i = 249999",
    "SELECT count(*) FROM big WHERE n IS NULL",
    "SELECT i, length(s) FROM long_strings ORDER BY i",
    "SELECT count(*) FROM empty_table",
    "SELECT * FROM other.t ORDER BY x",
    "SELECT * FROM v",
    "SELECT twice(4), add_default(1), add_default(1, b := 5)",
    "SELECT * FROM evens()",
    "SELECT 'c'::mood",
];

fn answers(connection: &mut Connection) -> Vec<Vec<String>> {
    QUERIES.iter().map(|sql| rows(connection, sql)).collect()
}

#[test]
fn a_database_comes_back_as_it_was() {
    let file = path("round_trip.db");
    let expected = {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        for sql in SCHEMA {
            run(&mut connection, sql);
        }
        answers(&mut connection)
    };
    for read_only in [true, false] {
        let db = Database::open(&file, read_only).unwrap();
        let mut connection = db.connect().unwrap();
        assert_eq!(answers(&mut connection), expected, "read_only: {read_only}");
    }
    // for `tools/check_written.py`: the file, and what duckrs says about it
    if let Some(target) = std::env::var_os("DUCKRS_KEEP_WRITTEN") {
        std::fs::copy(&file, &target).unwrap();
        let mut report = String::new();
        for (sql, rows) in QUERIES.iter().zip(&expected) {
            report += &format!("{sql}\n{}\n\n", rows.join("\n"));
        }
        let mut path = target;
        path.push(".expected");
        std::fs::write(path, report).unwrap();
    }
}

/// A checkpoint must not write over what the last one needs, and later ones reuse what is free.
#[test]
fn checkpoints_replace_each_other() {
    let file = path("again.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(
        &mut connection,
        "CREATE TABLE t AS SELECT i, 'row ' || i AS s FROM range(50000) r(i)",
    );
    run(&mut connection, "CHECKPOINT");
    let first = std::fs::metadata(&file).unwrap().len();
    assert_eq!(
        rows(&mut connection, "SELECT count(*), sum(i) FROM t"),
        ["50000|1249975000"]
    );
    for round in 0..6 {
        run(
            &mut connection,
            &format!("UPDATE t SET i = i + 1 WHERE i % 1000 = {round}"),
        );
        run(&mut connection, "DELETE FROM t WHERE i % 1000 = 500");
        run(&mut connection, "CHECKPOINT");
        assert!(
            rows(&mut connection, "SELECT count(*) FROM t")[0]
                .parse::<u64>()
                .unwrap()
                > 0
        );
    }
    let last = std::fs::metadata(&file).unwrap().len();
    assert!(last < first * 4, "the file went from {first} to {last} bytes");
    let expected = rows(&mut connection, "SELECT count(*), sum(i), min(s) FROM t");
    drop(connection);
    drop(db);
    let db = Database::open(&file, true).unwrap();
    assert_eq!(
        rows(&mut db.connect().unwrap(), "SELECT count(*), sum(i), min(s) FROM t"),
        expected
    );
}

/// What has no place in a file yet says so when it is made, and not when the file is written.
#[test]
fn keys_are_refused_up_front() {
    let file = path("keys.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    for sql in [
        "CREATE TABLE a (i INTEGER PRIMARY KEY)",
        "CREATE TABLE b (i INTEGER UNIQUE)",
    ] {
        let error = connection.query(sql).unwrap_err().to_string();
        assert!(error.contains("not yet ported"), "{sql}: {error}");
    }
    run(&mut connection, "CREATE TEMP TABLE c (i INTEGER PRIMARY KEY)");
    run(&mut connection, "CREATE TABLE d (i INTEGER)");
    assert!(connection.query("CREATE INDEX e ON d (i)").is_err());
}

#[test]
fn nothing_changed_writes_nothing() {
    let file = path("unchanged.db");
    drop(Database::open(&file, false).unwrap());
    let before = std::fs::read(&file).unwrap();
    {
        let db = Database::open(&file, false).unwrap();
        run(&mut db.connect().unwrap(), "SELECT 1");
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
}
