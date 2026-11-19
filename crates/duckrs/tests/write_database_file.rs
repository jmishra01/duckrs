//! Writes files and logs, reads them back, and kills what writes them. `tools/check_written.py` has the real DuckDB read one of them.

mod common;

use duckrs::{Connection, Database};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn path(name: &str) -> PathBuf {
    let directory = common::directory("duckrs-write-");
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
    "CREATE TABLE sorted AS SELECT i::INTEGER AS i, (i * 3)::BIGINT AS l, (i % 200)::SMALLINT AS s, \
     CASE WHEN i % 5 = 0 THEN NULL ELSE i END AS n FROM range(130000) r(i)",
    "CREATE TABLE runs AS SELECT (i // 1000)::INTEGER AS i, (i // 2000 % 2 = 0) AS b, (i // 500)::DOUBLE AS d, \
     7 AS same, CASE WHEN i // 1000 % 2 = 0 THEN NULL ELSE 1.5::DOUBLE END AS some_null FROM range(130000) r(i)",
    "CREATE TABLE decimals AS SELECT (i % 100000) / 100.0::DOUBLE AS d, (i % 1000) / 10.0::FLOAT AS f, \
     sin(i::DOUBLE) AS irregular, sqrt(i::FLOAT) AS root, \
     CASE WHEN i % 500 = 0 THEN 'NaN'::DOUBLE WHEN i % 501 = 0 THEN 'Infinity'::DOUBLE WHEN i % 502 = 0 THEN -0.0::DOUBLE \
     WHEN i % 7 = 0 THEN NULL ELSE i * 0.25 END AS odd FROM range(130000) r(i)",
    "CREATE TABLE words AS SELECT 'word ' || (i % 50) AS w, CASE WHEN i % 3 = 0 THEN NULL ELSE 'sometimes ' || (i % 7) END AS n, \
     CASE WHEN i % 2 = 0 THEN '' ELSE 'x' END AS e FROM range(130000) r(i)",
    "CREATE TABLE nulls AS SELECT i, CASE WHEN i % 1000 = 0 THEN NULL ELSE i END AS few, \
     CASE WHEN i % 1000 = 0 THEN i END AS rare, CASE WHEN i % 17 = 0 THEN NULL ELSE i END AS many, \
     CASE WHEN i % 10 < 8 THEN NULL ELSE i END AS most FROM range(9000) r(i)",
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
    "SELECT count(*), count(n), min(w), max(w), min(n), max(n), count(DISTINCT w), count(DISTINCT n), min(e), max(e) FROM words",
    "SELECT w, n, e FROM words LIMIT 4 OFFSET 122879",
    "SELECT count(*), sum(d), sum(f), sum(irregular), sum(root), count(odd), count(*) FILTER (WHERE isnan(odd)), \
     count(*) FILTER (WHERE isinf(odd)), count(*) FILTER (WHERE odd = 0 AND signbit(odd)) FROM decimals",
    "SELECT d, f, irregular, root, odd FROM decimals WHERE d = 1.23 ORDER BY root LIMIT 3",
    "SELECT count(few), count(rare), count(many), count(most), sum(few), sum(rare), sum(many), sum(most) FROM nulls",
    "SELECT count(*), sum(i), sum(l), sum(s), sum(n), count(n), min(n), max(n) FROM sorted",
    "SELECT * FROM sorted WHERE i BETWEEN 2046 AND 2050 ORDER BY i",
    "SELECT count(*), sum(i), count(*) FILTER (WHERE b), sum(d), sum(same), count(some_null) FROM runs",
    "SELECT * FROM runs WHERE i = 129 ORDER BY d, b LIMIT 2",
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

/// Keys of every kind, whose tables sort before what they reference.
const KEYS: &[&str] = &[
    "CREATE TABLE parent (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE, note VARCHAR NOT NULL, n INTEGER CHECK (n >= 0))",
    "CREATE TABLE a_child (id BIGINT PRIMARY KEY, parent INTEGER REFERENCES parent (id), boss BIGINT, \
     FOREIGN KEY (boss) REFERENCES a_child (id))",
    "CREATE TABLE pairs (a INTEGER, b VARCHAR, c DOUBLE, PRIMARY KEY (a, b))",
    "CREATE TABLE no_rows (i INTEGER PRIMARY KEY, j VARCHAR UNIQUE)",
    "INSERT INTO parent SELECT i, 'https://example.com/items/' || i, 'note ' || (i % 7), i % 3 FROM range(20000) r(i)",
    "INSERT INTO parent VALUES (-1, NULL, 'no name', 0)",
    "INSERT INTO a_child SELECT i, i % 100, NULL FROM range(2500) r(i)",
    "INSERT INTO a_child SELECT i + 5000, i % 100, i FROM range(2500) r(i)",
    "INSERT INTO pairs SELECT i % 50, 'pair ' || i, i / 2.0 FROM range(3000) r(i)",
    "CREATE INDEX by_note ON parent (note)",
    "CREATE UNIQUE INDEX by_sum ON parent ((id + 1))",
    "CREATE INDEX by_c ON pairs (c)",
];

fn refused(connection: &mut Connection, sql: &str, expected: &str) {
    let error = connection.query(sql).unwrap_err().to_string();
    assert!(error.contains(expected), "{sql}: {error}");
}

fn check_keys(connection: &mut Connection) {
    refused(
        connection,
        "INSERT INTO parent VALUES (5, 'x', 'n', 0)",
        "Duplicate key",
    );
    refused(
        connection,
        "INSERT INTO parent VALUES (500000, 'https://example.com/items/7', 'n', 0)",
        "Duplicate key",
    );
    refused(
        connection,
        "INSERT INTO a_child VALUES (9999, 123456, NULL)",
        "foreign key",
    );
    refused(
        connection,
        "INSERT INTO a_child VALUES (9999, 1, 123456)",
        "foreign key",
    );
    refused(connection, "INSERT INTO pairs VALUES (1, 'pair 1', 0)", "Duplicate key");
    refused(connection, "DELETE FROM parent WHERE id = 1", "still referenced");
    assert_eq!(
        rows(
            connection,
            "SELECT (SELECT count(*) FROM parent), (SELECT count(*) FROM a_child), (SELECT count(*) FROM pairs)"
        ),
        ["20001|5000|3000"]
    );
}

/// A file with keys takes writes like any other, and comes back with what it holds them to.
#[test]
fn keys_and_indexes_come_back() {
    let file = path("keys.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        for sql in KEYS {
            run(&mut connection, sql);
        }
        run(&mut connection, "CHECKPOINT");
        check_keys(&mut connection);
    }
    // for `tools/check_written.py`: DuckDB holds the ART of each of these to their rows
    if let Some(target) = std::env::var_os("DUCKRS_KEEP_KEYS") {
        std::fs::copy(&file, target).unwrap();
    }
    {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        check_keys(&mut connection);
        run(&mut connection, "INSERT INTO parent VALUES (30000, 'new', 'note', 1)");
        run(&mut connection, "DELETE FROM pairs WHERE a = 1");
        run(&mut connection, "UPDATE pairs SET c = 0 WHERE a = 2");
        run(&mut connection, "DROP INDEX by_c");
        run(&mut connection, "CREATE INDEX by_a ON pairs (a)");
    }
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    refused(
        &mut connection,
        "INSERT INTO parent VALUES (30000, 'y', 'note', 1)",
        "Duplicate key",
    );
    run(&mut connection, "DROP INDEX by_note");
    run(&mut connection, "DROP INDEX by_a");
    refused(&mut connection, "DROP INDEX by_c", "does not exist");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM pairs"), ["2940"]);
}

/// Without a checkpoint the indexes are in the log, in the ids its rows get when it is replayed.
#[test]
fn a_log_brings_back_keys_and_indexes() {
    let file = path("keys-logged.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, "PRAGMA disable_checkpoint_on_shutdown");
        for sql in KEYS {
            run(&mut connection, sql);
        }
        run(&mut connection, "CHECKPOINT");
        run(&mut connection, "INSERT INTO parent VALUES (30000, 'new', 'note', 1)");
        run(&mut connection, "DELETE FROM pairs WHERE a = 1");
        run(&mut connection, "UPDATE pairs SET c = 0 WHERE a = 2");
        // a row no child references: `note` has an index, so the update deletes and inserts the row, which
        // C++'s over-eager check refuses for a referenced one
        run(&mut connection, "UPDATE parent SET note = 'changed' WHERE id = 300");
        run(&mut connection, "DROP INDEX by_c");
        run(&mut connection, "CREATE INDEX by_a ON pairs (a)");
        run(&mut connection, "CREATE TABLE later (i INTEGER PRIMARY KEY)");
        run(&mut connection, "INSERT INTO later VALUES (1), (2)");
        run(&mut connection, "CREATE UNIQUE INDEX later_i ON later (i)");
    }
    let mut log = file.clone().into_os_string();
    log.push(".wal");
    assert!(std::fs::metadata(&log).unwrap().len() > 0);
    if let Some(target) = std::env::var_os("DUCKRS_KEEP_KEYS_LOGGED") {
        std::fs::copy(&file, &target).unwrap();
        let mut target_log = target;
        target_log.push(".wal");
        std::fs::copy(&log, target_log).unwrap();
    }
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    refused(
        &mut connection,
        "INSERT INTO parent VALUES (30000, 'y', 'n', 1)",
        "Duplicate key",
    );
    refused(&mut connection, "INSERT INTO later VALUES (2)", "Duplicate key");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM pairs"), ["2940"]);
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

/// Without a checkpoint on shutdown all of it comes back from the log alone.
#[test]
fn a_log_brings_back_what_no_checkpoint_saw() {
    let file = path("logged.db");
    let expected = {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, "PRAGMA disable_checkpoint_on_shutdown");
        for sql in SCHEMA {
            run(&mut connection, sql);
        }
        run(&mut connection, "UPDATE big SET s = 'updated' WHERE i % 5 = 0");
        run(&mut connection, "DELETE FROM big WHERE i % 7 = 0");
        run(&mut connection, "SELECT nextval('counter')");
        run(&mut connection, "CREATE TABLE gone (i INTEGER)");
        run(&mut connection, "DROP TABLE gone");
        run(&mut connection, "BEGIN");
        run(&mut connection, "INSERT INTO empty_table VALUES (1, 'rolled back')");
        run(&mut connection, "ROLLBACK");
        answers(&mut connection)
    };
    let mut log = file.clone().into_os_string();
    log.push(".wal");
    assert!(std::fs::metadata(&log).unwrap().len() > 0);
    // for `tools/check_written.py`: a file and a log that only replaying makes sense of
    if let Some(target) = std::env::var_os("DUCKRS_KEEP_LOGGED") {
        std::fs::copy(&file, &target).unwrap();
        let mut target_log = target;
        target_log.push(".wal");
        std::fs::copy(&log, target_log).unwrap();
    }
    let db = Database::open(&file, true).unwrap();
    let mut connection = db.connect().unwrap();
    assert_eq!(answers(&mut connection), expected);
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM empty_table"), ["0"]);
    drop(connection);
    drop(db);
    assert!(
        std::fs::metadata(&log).unwrap().len() > 0,
        "reading leaves the log alone"
    );
    // writing starts by putting the log in the file
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    assert!(std::fs::metadata(&log).is_err());
    assert_eq!(answers(&mut connection), expected);
    assert_eq!(rows(&mut connection, "SELECT nextval('counter')"), ["12"]);
}

/// The log is put in the file when it has grown enough.
#[test]
fn a_large_log_turns_into_a_checkpoint() {
    let file = path("auto.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(&mut connection, "SET checkpoint_threshold = '1MB'");
    run(
        &mut connection,
        "CREATE TABLE t AS SELECT i, 'row ' || i AS s FROM range(100000) r(i)",
    );
    let mut log = file.into_os_string();
    log.push(".wal");
    assert!(std::fs::metadata(&log).is_err());
}

/// A commit that can't be logged must not lose the commits around it.
#[test]
fn a_commit_that_fails_loses_nothing_else() {
    let file = path("failed_commit.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(&mut connection, "PRAGMA disable_checkpoint_on_shutdown");
    run(&mut connection, "CREATE TABLE kept (i INTEGER)");
    run(&mut connection, "BEGIN");
    run(&mut connection, "INSERT INTO kept VALUES (1)");
    run(&mut connection, "CREATE TABLE unstorable (v VARIANT)");
    assert!(connection.query("COMMIT").is_err());
    run(&mut connection, "DROP TABLE unstorable");
    run(&mut connection, "INSERT INTO kept VALUES (2)");
    drop(connection);
    drop(db);
    let db = Database::open(&file, true).unwrap();
    assert_eq!(
        rows(&mut db.connect().unwrap(), "SELECT count(*), sum(i) FROM kept"),
        ["2|3"]
    );
}

#[test]
fn attached_databases_keep_their_rows() {
    let first = path("first.db");
    let second = path("second.db");
    let db = Database::open(&first, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(&mut connection, "CREATE TABLE here (i INTEGER)");
    run(&mut connection, &format!("ATTACH '{}' AS other", second.display()));
    run(&mut connection, "CREATE TABLE other.there (i INTEGER)");
    run(&mut connection, "INSERT INTO other.there VALUES (1), (2)");
    assert!(connection.query("SELECT * FROM there").is_err());
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM other.there"), ["2"]);
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM other.main.there"), ["2"]);
    run(&mut connection, "USE other");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM there"), ["2"]);
    assert!(
        connection
            .query("DETACH other")
            .unwrap_err()
            .to_string()
            .contains("default database")
    );
    run(&mut connection, "USE first");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM here"), ["0"]);
    run(&mut connection, "DETACH other");
    assert!(connection.query("SELECT * FROM other.there").is_err());
    run(
        &mut connection,
        &format!("ATTACH '{}' AS back (READ_ONLY)", second.display()),
    );
    assert_eq!(rows(&mut connection, "SELECT sum(i) FROM back.there"), ["3"]);
    assert!(connection.query("INSERT INTO back.there VALUES (3)").is_err());
}

#[test]
fn storage_info_lists_what_is_in_the_file() {
    let file = path("segments.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(
        &mut connection,
        "CREATE TABLE t AS SELECT i, CASE WHEN i % 2 = 0 THEN 'x' END AS s FROM range(150000) r(i)",
    );
    run(&mut connection, "CHECKPOINT");
    let sql = "SELECT count(DISTINCT row_group_id), sum(count) FILTER (WHERE column_path = '[0]') \
               FROM pragma_storage_info('t')";
    assert_eq!(rows(&mut connection, sql), ["2|150000"]);
    let sql = "SELECT DISTINCT segment_type, compression FROM pragma_storage_info('t') ORDER BY 1, 2";
    let segments = rows(&mut connection, sql);
    assert!(segments.contains(&"BIGINT|BitPacking".to_string()), "{segments:?}");
    assert!(segments.contains(&"VALIDITY|Constant".to_string()), "{segments:?}");
}

/// What `force_compression` asks for ends up in the file, and comes back.
#[test]
fn codecs_can_be_forced() {
    for (force, column, expected) in [
        ("rle", "[0]", "RLE"),
        ("bitpacking", "[0]", "BitPacking"),
        ("uncompressed", "[0]", "Uncompressed"),
        ("roaring", "[1]", "Roaring"),
        ("alp", "[2]", "ALP"),
        ("alprd", "[2]", "ALPRD"),
    ] {
        let file = path(&format!("forced_{force}.db"));
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, &format!("SET force_compression = '{force}'"));
        run(
            &mut connection,
            "CREATE TABLE t AS SELECT i::BIGINT AS i, i % 3 = 0 AS b, sin(i::DOUBLE) AS f FROM range(5000) r(i)",
        );
        run(&mut connection, "CHECKPOINT");
        let sql = format!("SELECT DISTINCT compression FROM pragma_storage_info('t') WHERE column_path = '{column}'");
        assert_eq!(rows(&mut connection, &sql), [expected], "{force}");
        let sum = "SELECT sum(i), count(*) FILTER (WHERE b), count(f) FROM t";
        assert_eq!(rows(&mut connection, sum), ["12497500|1667|5000"], "{force}");
    }
}

#[test]
fn strings_can_go_to_a_dictionary() {
    let file = path("dictionary.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(&mut connection, "SET force_compression = 'dict_fsst'");
    run(
        &mut connection,
        "CREATE TABLE t AS SELECT 'row ' || (i % 9) AS s FROM range(5000) r(i)",
    );
    run(&mut connection, "CHECKPOINT");
    let sql = "SELECT DISTINCT compression FROM pragma_storage_info('t') WHERE column_path = '[0]'";
    assert_eq!(rows(&mut connection, sql), ["DICT_FSST"]);
    assert_eq!(
        rows(&mut connection, "SELECT count(DISTINCT s), min(s), max(s) FROM t"),
        ["9|row 0|row 8"]
    );
}

/// Without a setting the smallest one wins, and a value that doesn't change needs no block at all.
#[test]
fn the_smallest_codec_is_chosen() {
    let file = path("chosen.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    for sql in SCHEMA
        .iter()
        .filter(|sql| sql.contains("TABLE sorted") || sql.contains("TABLE runs"))
    {
        run(&mut connection, sql);
    }
    run(&mut connection, "CHECKPOINT");
    let compression = |connection: &mut Connection, table: &str, path: &str| {
        let sql = format!(
            "SELECT DISTINCT compression FROM pragma_storage_info('{table}') WHERE column_path = '{path}' ORDER BY 1"
        );
        rows(connection, &sql)
    };
    assert_eq!(compression(&mut connection, "sorted", "[0]"), ["BitPacking"]);
    assert_eq!(compression(&mut connection, "runs", "[0]"), ["RLE"]);
    assert_eq!(compression(&mut connection, "runs", "[3]"), ["Constant"]);
}

/// Floats come back bit for bit, in each codec, whatever they are: as text, -0.0 isn't 0.0.
#[test]
fn floats_keep_their_bits() {
    let values = "CASE WHEN i % 5 = 0 THEN i / 100.0::DOUBLE WHEN i % 5 = 1 THEN sin(i::DOUBLE) \
                  WHEN i % 5 = 2 THEN -0.0::DOUBLE WHEN i = 3 THEN 'NaN'::DOUBLE WHEN i = 8 THEN '-Infinity'::DOUBLE \
                  WHEN i % 5 = 3 THEN 0.0::DOUBLE ELSE 1e30 * i END";
    let check = "SELECT d::VARCHAR, f::VARCHAR FROM t ORDER BY i";
    let mut expected = None;
    for force in ["uncompressed", "alp", "alprd"] {
        let file = path(&format!("float_bits_{force}.db"));
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, &format!("SET force_compression = '{force}'"));
        let sql = format!(
            "CREATE TABLE t AS SELECT i, d, d::FLOAT AS f FROM (SELECT i, {values} AS d FROM range(6000) r(i))"
        );
        run(&mut connection, &sql);
        run(&mut connection, "CHECKPOINT");
        let answer = rows(&mut connection, check);
        assert_eq!(answer.iter().filter(|row| row.starts_with("-0.0|")).count(), 1200);
        assert_eq!(*expected.get_or_insert(answer.clone()), answer, "{force}");
        // for `tools/check_written.py`
        if let Some(target) = std::env::var_os("DUCKRS_KEEP_FLOATS") {
            drop(connection);
            drop(db);
            std::fs::copy(&file, std::path::Path::new(&target).join(format!("{force}.db"))).unwrap();
        }
    }
}

/// Strings go through FSST where that pays: on their own if none repeats, or else in a dictionary.
#[test]
fn strings_can_go_through_fsst() {
    const UNIQUE: &str = "SELECT i, 'https://example.com/items/' || i AS s FROM range(50000) r(i)";
    const REPEATED: &str = "SELECT i, CASE WHEN i % 7 = 0 THEN NULL WHEN i % 11 = 0 THEN '' \
                            ELSE 'customer-segment-' || (i % 3000) || '-ünï' END AS s FROM range(50000) r(i)";
    let mut blocks = Vec::new();
    for force in ["uncompressed", "auto"] {
        let file = path(&format!("fsst_{force}.db"));
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, &format!("SET force_compression = '{force}'"));
        run(&mut connection, &format!("CREATE TABLE u AS {UNIQUE}"));
        run(&mut connection, &format!("CREATE TABLE r AS {REPEATED}"));
        let answers = |connection: &mut Connection| {
            ["u", "r"].map(|table| {
                let sql = format!("SELECT i, s FROM {table} ORDER BY i");
                rows(connection, &sql)
            })
        };
        let expected = answers(&mut connection);
        run(&mut connection, "CHECKPOINT");
        assert_eq!(answers(&mut connection), expected, "{force}");
        let sql = "SELECT count(DISTINCT block_id) FROM pragma_storage_info('u') WHERE segment_type = 'VARCHAR'";
        blocks.push(rows(&mut connection, sql)[0].parse::<usize>().unwrap());
        if force == "auto" {
            let sql = "SELECT DISTINCT compression FROM pragma_storage_info('u') WHERE segment_type = 'VARCHAR'";
            assert_eq!(rows(&mut connection, sql), ["DICT_FSST"]);
        }
        // for `tools/check_written.py`
        if let Some(target) = std::env::var_os("DUCKRS_KEEP_FLOATS").filter(|_| force == "auto") {
            drop(connection);
            drop(db);
            std::fs::copy(&file, std::path::Path::new(&target).join("strings.db")).unwrap();
        }
    }
    assert!(blocks[1] * 2 <= blocks[0], "{blocks:?}");
}

/// Decimals go to ALP and the rest of the floats to ALPRD, as neither is forced here.
#[test]
fn floats_take_the_codec_that_suits_them() {
    let file = path("float_codecs.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    let sql = "CREATE TABLE t AS SELECT (hash(i) % 100000)::BIGINT / 100.0::DOUBLE AS price, sin(i::DOUBLE) AS wave \
               FROM range(100000) r(i)";
    run(&mut connection, sql);
    run(&mut connection, "CHECKPOINT");
    let compression = |connection: &mut Connection, path: &str| {
        let sql = format!("SELECT DISTINCT compression FROM pragma_storage_info('t') WHERE column_path = '{path}'");
        rows(connection, &sql)
    };
    assert_eq!(compression(&mut connection, "[0]"), ["ALP"]);
    assert_eq!(compression(&mut connection, "[1]"), ["ALPRD"]);
}

/// Zeros of both signs are one value to the statistics, and two to the file.
#[test]
fn a_column_of_both_zeros_is_not_constant() {
    let file = path("both_zeros.db");
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    let sql = "CREATE TABLE t AS SELECT i, CASE WHEN i % 2 = 0 THEN -0.0::DOUBLE ELSE 0.0::DOUBLE END AS d, \
               CASE WHEN i % 2 = 0 THEN 'NaN'::DOUBLE ELSE 'NaN'::DOUBLE END AS n FROM range(3000) r(i)";
    run(&mut connection, sql);
    run(&mut connection, "CHECKPOINT");
    let sql = "SELECT d::VARCHAR, count(*), count(*) FILTER (WHERE isnan(n)) FROM t GROUP BY 1 ORDER BY 1";
    assert_eq!(rows(&mut connection, sql), ["-0.0|1500|1500", "0.0|1500|1500"]);
}

const CRASH_FILE: &str = "DUCKRS_CRASH_FILE";

/// Commits for as long as it lives, and says so after each. Run by `commits_survive_kill_9`.
#[test]
#[ignore = "it never ends: commits_survive_kill_9 runs it and kills it"]
fn crash_child() {
    let Some(file) = std::env::var_os(CRASH_FILE) else {
        return;
    };
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    run(&mut connection, "CREATE TABLE IF NOT EXISTS log (n INTEGER, s VARCHAR)");
    run(&mut connection, "CREATE TABLE IF NOT EXISTS state (n INTEGER)");
    run(&mut connection, "CREATE SEQUENCE IF NOT EXISTS ticks");
    if rows(&mut connection, "SELECT count(*) FROM state") == ["0"] {
        run(&mut connection, "INSERT INTO state VALUES (0)");
    }
    let mut n: u64 = rows(&mut connection, "SELECT n FROM state")[0].parse().unwrap();
    loop {
        n += 1;
        run(&mut connection, "BEGIN");
        for part in 0..3 {
            run(
                &mut connection,
                &format!("INSERT INTO log VALUES ({n}, 'part {part} of commit {n}')"),
            );
        }
        run(&mut connection, &format!("UPDATE state SET n = {n}"));
        run(&mut connection, "SELECT nextval('ticks')");
        run(&mut connection, "COMMIT");
        println!("committed {n}");
        if n.is_multiple_of(7) {
            run(&mut connection, "CHECKPOINT");
        }
        if n.is_multiple_of(11) {
            run(&mut connection, &format!("CREATE TABLE extra_{n} AS SELECT {n} AS n"));
        }
    }
}

/// What was acknowledged is there, what wasn't is there in full or not at all, and the file opens,
/// whenever the process dies, the checkpoints it does included.
#[test]
fn commits_survive_kill_9() {
    let file = path("crash.db");
    let mut delay = 37u64;
    let mut acknowledged = 0u64;
    for round in 0..30 {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--ignored", "--nocapture"])
            .env(CRASH_FILE, &file)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let reading = std::thread::spawn(move || {
            let mut last = 0u64;
            for line in reader.lines().map_while(std::result::Result::ok) {
                // libtest can print in front of what the test prints
                if let Some((_, n)) = line.split_once("committed ") {
                    last = last.max(n.trim().parse().unwrap_or(0));
                }
            }
            last
        });
        delay = (delay * 31 + 17) % 251 + 40;
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(delay));
        child.kill().unwrap();
        child.wait().unwrap();
        acknowledged = acknowledged.max(reading.join().unwrap());
        let elapsed = start.elapsed();

        let db = Database::open(&file, true).unwrap_or_else(|error| panic!("round {round}: {error}"));
        let mut connection = db.connect().unwrap();
        if connection.query("SELECT 1 FROM log LIMIT 0").is_err() {
            // killed before the first table
            assert_eq!(acknowledged, 0, "round {round}");
            continue;
        }
        let Some(state) = rows(&mut connection, "SELECT n FROM state")
            .first()
            .map(|n| n.parse::<u64>().unwrap())
        else {
            // killed between the tables and the first row of `state`
            assert_eq!(
                acknowledged, 0,
                "round {round}: {acknowledged} were acknowledged, and `state` is empty"
            );
            continue;
        };
        assert!(
            state >= acknowledged,
            "round {round} after {elapsed:?}: {acknowledged} were acknowledged, {state} are there"
        );
        assert!(
            state <= acknowledged + 1,
            "round {round}: {acknowledged} acknowledged, {state} there"
        );
        let total = format!("{}|{}", state * 3, state);
        let sql = "SELECT count(*), count(DISTINCT n) FROM log";
        assert_eq!(rows(&mut connection, sql), [total], "round {round}");
        acknowledged = state;
    }
    assert!(acknowledged > 30, "only {acknowledged} commits were tested");
}

/// A table that is referenced comes back as a new entry, with what references it in the way.
#[test]
fn a_referenced_table_can_change_in_the_log() {
    let file = path("keys-altered.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut connection = db.connect().unwrap();
        run(&mut connection, "PRAGMA disable_checkpoint_on_shutdown");
        run(&mut connection, "CREATE TABLE p (i INTEGER PRIMARY KEY)");
        run(&mut connection, "CREATE TABLE c (i INTEGER REFERENCES p (i))");
        run(&mut connection, "INSERT INTO p VALUES (1), (2)");
        run(&mut connection, "INSERT INTO c VALUES (1)");
        run(&mut connection, "CREATE INDEX c_i ON c (i)");
        run(&mut connection, "CHECKPOINT");
        run(&mut connection, "ALTER TABLE p ADD COLUMN j INTEGER");
        run(&mut connection, "INSERT INTO p VALUES (3, 3)");
    }
    if let Some(target) = std::env::var_os("DUCKRS_KEEP_KEYS_ALTERED") {
        let mut log = file.clone().into_os_string();
        log.push(".wal");
        std::fs::copy(&file, &target).unwrap();
        let mut target_log = target;
        target_log.push(".wal");
        std::fs::copy(&log, target_log).unwrap();
    }
    let db = Database::open(&file, false).unwrap();
    let mut connection = db.connect().unwrap();
    assert_eq!(rows(&mut connection, "SELECT count(*), count(j) FROM p"), ["3|1"]);
    refused(&mut connection, "INSERT INTO c VALUES (9)", "foreign key");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM c"), ["1"]);
    run(&mut connection, "DROP INDEX c_i");
}

/// A checkpoint shreds VARIANT columns whose values have a common shape (`variant_minimum_shredding_size`
/// rows or more): the shredded part is in the file next to the unshredded one, and the values come
/// back as they were, those of another shape too.
#[test]
fn shredded_variants_come_back() {
    let file = path("shredded_variants.db");
    let tables = [
        ("ints", "SELECT (i % 7)::INTEGER::VARIANT v FROM range(50) t(i)"),
        (
            "mixed",
            "SELECT CASE WHEN i % 5 = 0 THEN ('s' || i)::VARIANT WHEN i % 7 = 0 THEN NULL ELSE i::VARIANT END v \
             FROM range(50) t(i)",
        ),
        (
            "objects",
            "SELECT CASE WHEN i % 9 = 0 THEN {'a': 'text', 'z': [1, 2]}::VARIANT \
             WHEN i % 11 = 0 THEN [i]::VARIANT ELSE {'a': i, 'b': 'x' || i}::VARIANT END v FROM range(50) t(i)",
        ),
        ("lists", "SELECT [i, NULL, i + 1]::VARIANT v FROM range(50) t(i)"),
    ];
    let read = |c: &mut Connection, table: &str| rows(c, &format!("SELECT v::VARCHAR FROM {table} ORDER BY rowid"));
    let mut expected = Vec::new();
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "SET variant_minimum_shredding_size = 0");
        for (table, query) in tables {
            run(&mut c, &format!("CREATE TABLE {table} AS {query}"));
            expected.push(read(&mut c, table));
        }
        run(&mut c, "CHECKPOINT");
        for (table, _) in tables {
            let shredded = rows(
                &mut c,
                &format!("SELECT count(*) FROM pragma_storage_info('{table}') WHERE column_path LIKE '[0, 2%'"),
            );
            assert_ne!(shredded, ["0"], "{table} is shredded");
        }
    }
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    for ((table, _), expected) in tables.iter().zip(&expected) {
        assert_eq!(&read(&mut c, table), expected, "{table}");
    }
}

/// `VariantStats::ToStruct` of a shredded column read back from its file: the typed parts that every value
/// is in (`fully_shredded`), by field and element, with their statistics, and an all-NULL INTEGER column
/// next to it, whose empty statistics hold no bounds.
#[test]
fn shredded_variant_statistics_come_back() {
    let file = path("shredded_variant_statistics.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "SET variant_minimum_shredding_size = 0");
        run(
            &mut c,
            "CREATE TABLE t AS SELECT {'a': i::INTEGER, 'l': [i::INTEGER]}::VARIANT v, NULL::INTEGER n FROM range(100) r(i)",
        );
        run(&mut c, "CHECKPOINT");
    }
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    let stats = |c: &mut Connection, query: &str| rows(c, &format!("SELECT {query} FROM t LIMIT 1"));
    assert_eq!(stats(&mut c, "stats(v).shredding_state"), ["SHREDDED"]);
    assert_eq!(
        stats(
            &mut c,
            "stats(v).fully_shredded.a.type || ' ' || stats(v).fully_shredded.a.stats.max"
        ),
        ["INTEGER 99"]
    );
    assert_eq!(stats(&mut c, "stats(v).fully_shredded.l.child_stats.stats.min"), ["0"]);
    assert_eq!(stats(&mut c, "stats(v.a).fully_shredded.stats.min"), ["0"]);
    assert_eq!(
        stats(&mut c, "stats(n)::VARCHAR"),
        ["{'has_no_null': false, 'has_null': true}"]
    );
    assert_eq!(rows(&mut c, "SELECT count(n), sum((v.a)::INTEGER) FROM t"), ["0|4950"]);
}

/// Before v1.5.0 a GEOMETRY is the spatial extension's BLOB, in the file and in the log, and the keys of an
/// index of one are too (`ART::ConvertKeyInput`): they come back as they went in, and the index finds them.
#[test]
fn geometries_of_old_storage_come_back() {
    let dir = path("geometry_v140");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("g.db");
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(dir.join("g.db.wal"));
    let shapes = "SELECT i, CASE WHEN i % 7 = 0 THEN NULL WHEN i % 5 = 0 THEN 'POLYGON ((0 0, 1 0, 1 1, 0 0))'::GEOMETRY \
                  WHEN i % 3 = 0 THEN ('LINESTRING Z (0 0 1, ' || i || ' 1 2)')::GEOMETRY \
                  ELSE ('POINT (' || i || ' ' || i || ')')::GEOMETRY END g FROM range(300) r(i)";
    let read = |c: &mut Connection| {
        rows(
            c,
            "SELECT md5(string_agg(g::VARCHAR, ',' ORDER BY i)), count(g) FROM db.t",
        )
    };
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    let attach = format!("ATTACH '{}' AS db (STORAGE_VERSION 'v1.4.0')", file.display());
    run(&mut c, &attach);
    run(&mut c, &format!("CREATE TABLE db.t AS {shapes}"));
    run(&mut c, "CREATE INDEX gi ON db.t(g)");
    run(&mut c, "CHECKPOINT db");
    run(&mut c, "INSERT INTO db.t VALUES (1000, 'POINT (7 8)'::GEOMETRY)");
    let expected = read(&mut c);
    run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
    run(&mut c, "USE memory");
    run(&mut c, "DETACH db");
    run(&mut c, &format!("ATTACH '{}' AS db", file.display()));
    assert_eq!(read(&mut c), expected);
    assert_eq!(
        rows(&mut c, "SELECT i FROM db.t WHERE g = 'POINT (151 151)'::GEOMETRY"),
        ["151"]
    );
}

/// A crash between the end of a checkpoint and the removal of its log leaves a log that its marker says is in
/// the file: opening the file again is done with it (`IsCheckpointClean`), and a checkpoint of a log that was
/// replayed marks it before the new header goes in, so that the same crash again leaves a log that says so too.
#[test]
fn a_log_that_is_in_the_file_goes() {
    let file = path("checkpointed_log.db");
    let mut log = file.clone().into_os_string();
    log.push(".wal");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        run(&mut c, "CREATE TABLE t AS SELECT range i FROM range(100)");
        run(&mut c, "SET debug_checkpoint_abort = 'before_truncate'");
        assert!(c.query("CHECKPOINT").is_err());
    }
    assert!(std::fs::metadata(&log).is_ok(), "the log stays behind the checkpoint");
    for _ in 0..2 {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        assert_eq!(rows(&mut c, "SELECT count(*), sum(i) FROM t"), ["100|4950"]);
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
    }
    assert!(std::fs::metadata(&log).is_err(), "the log is in the file, so it went");
}

/// A path of a shredded VARIANT that a scan gives is read from the shredded part where all its values are there,
/// and from the whole column where they aren't: either way it is what extracting from the whole column gives.
#[test]
fn shredded_paths_read_alone_are_the_same() {
    let file = path("shredded_paths.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "SET variant_minimum_shredding_size = 0");
        run(
            &mut c,
            "CREATE TABLE typed AS SELECT i, CASE WHEN i % 5 = 0 THEN NULL WHEN i % 3 = 0 THEN {'b': 'x'}::VARIANT \
             ELSE {'a': i, 'n': {'m': i * 2}}::VARIANT END v FROM range(3000) r(i)",
        );
        run(
            &mut c,
            "CREATE TABLE untyped AS SELECT i, CASE WHEN i % 4 = 0 THEN {'a': 'text'}::VARIANT \
             ELSE {'a': i}::VARIANT END v FROM range(3000) r(i)",
        );
        run(&mut c, "CHECKPOINT");
    }
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    for query in [
        "SELECT md5(string_agg(coalesce(v.a::VARCHAR, '-'), ',' ORDER BY i)), sum(v.n.m::BIGINT) FROM typed",
        "SELECT md5(string_agg(coalesce(v.a::VARCHAR, '-'), ',' ORDER BY i)), count(v.a) FROM untyped",
    ] {
        let pushed = rows(&mut c, query);
        run(&mut c, "SET disabled_optimizers = 'unused_columns'");
        let whole = rows(&mut c, query);
        run(&mut c, "SET disabled_optimizers = ''");
        assert_eq!(pushed, whole, "{query}");
    }
}

#[test]
fn columns_load_when_they_are_read() {
    let file = path("lazy_columns.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(
            &mut c,
            "CREATE TABLE t AS SELECT i a, i * 2 b, i::VARCHAR c FROM range(300000) r(i)",
        );
        run(&mut c, "DELETE FROM t WHERE a % 7 = 0");
    }
    let loaded = "SELECT DISTINCT column_name FROM pragma_storage_info('t', loaded_segments_only = true) ORDER BY 1";
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    assert!(rows(&mut c, loaded).is_empty());
    let sum = rows(&mut c, "SELECT sum(b) FROM t");
    assert_eq!(rows(&mut c, loaded), ["b"]);
    // a checkpoint keeps the row groups as they were, unread, and what of them was loaded
    run(&mut c, "CHECKPOINT");
    assert_eq!(rows(&mut c, loaded), ["b"]);
    assert_eq!(rows(&mut c, "SELECT sum(b) FROM t"), sum);
    drop(c);
    drop(db);
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    assert!(rows(&mut c, loaded).is_empty());
    assert_eq!(rows(&mut c, "SELECT count(*), sum(b) FROM t"), ["257142|77142342858"]);
}

#[test]
fn columns_that_stay_keep_their_blocks() {
    let file = path("per_column_reuse.db");
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(
        &mut c,
        &format!("ATTACH '{}' AS db (STORAGE_VERSION 'v2.0.0')", file.display()),
    );
    run(&mut c, "USE db");
    run(
        &mut c,
        "CREATE TABLE t AS SELECT i c0, i * 2 c1, i::VARCHAR c2, i % 10 c3 FROM range(300000) r(i)",
    );
    run(&mut c, "CHECKPOINT");
    // where each column's segments are, by column
    let blocks = |c: &mut Connection| -> std::collections::BTreeMap<String, Vec<String>> {
        let mut by_column = std::collections::BTreeMap::new();
        let query = "SELECT column_name, row_group_id, column_path, segment_id, block_id, block_offset \
                     FROM pragma_storage_info('t') ORDER BY ALL";
        for row in rows(c, query) {
            let name = row.split('|').next().unwrap().to_string();
            by_column.entry(name).or_insert_with(Vec::new).push(row);
        }
        by_column
    };
    let sums = "SELECT sum(c0), sum(c1), max(c2), sum(c3), count(*) FROM t";
    let before = blocks(&mut c);
    run(&mut c, "UPDATE t SET c1 = c1 + 1 WHERE c0 < 100");
    let expected = rows(&mut c, sums);
    run(&mut c, "CHECKPOINT");
    let after = blocks(&mut c);
    for column in ["c0", "c2", "c3"] {
        assert_eq!(before[column], after[column], "{column} stays where it was");
    }
    assert_ne!(before["c1"], after["c1"]);
    assert_eq!(rows(&mut c, sums), expected);
    // ALTER keeps the columns it doesn't change
    run(&mut c, "ALTER TABLE t ADD COLUMN c4 INTEGER DEFAULT 42");
    run(&mut c, "ALTER TABLE t DROP COLUMN c3");
    run(&mut c, "ALTER TABLE t ALTER c0 TYPE BIGINT");
    run(&mut c, "CHECKPOINT");
    let altered = blocks(&mut c);
    for column in ["c1", "c2"] {
        assert_eq!(after[column], altered[column], "{column} stays where it was");
    }
    assert_ne!(after["c0"], altered["c0"]);
    let sums = "SELECT sum(c0), sum(c1), max(c2), sum(c4), count(*) FROM t";
    let expected = rows(&mut c, sums);
    run(&mut c, "USE memory");
    run(&mut c, "DETACH db");
    run(&mut c, &format!("ATTACH '{}' AS db", file.display()));
    assert_eq!(rows(&mut c, &sums.replace("FROM t", "FROM db.t")), expected);
}

#[test]
fn alters_in_the_log_come_back() {
    let file = path("logged_alters.db");
    let steps = [
        "UPDATE t SET b = -b WHERE a % 3 = 0",
        "ALTER TABLE t ADD COLUMN d INTEGER DEFAULT 42",
        "UPDATE t SET d = a WHERE a < 100",
        "ALTER TABLE t DROP COLUMN b",
        // with gaps in the row ids this would be the table made again
        "ALTER TABLE t ALTER a TYPE VARCHAR",
        "DELETE FROM t WHERE a::INT % 7 = 0",
        "ALTER TABLE t RENAME COLUMN c TO cc",
        "ALTER TABLE t ALTER d SET DEFAULT 7",
        "INSERT INTO t (a, cc) VALUES ('new', 'y')",
        "ALTER TABLE t ALTER cc SET NOT NULL",
    ];
    let answer = "SELECT count(*), md5(string_agg(t::VARCHAR, ',' ORDER BY t::VARCHAR)) FROM t";
    let shape = "SELECT string_agg(column_name || ':' || data_type || ':' || coalesce(column_default, '-') || ':' || \
                 is_nullable, ' ') FROM information_schema.columns WHERE table_name = 't'";
    let expected = {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(
            &mut c,
            "CREATE TABLE t AS SELECT i a, i * 2 b, i::VARCHAR c FROM range(5000) r(i)",
        );
        run(&mut c, "CHECKPOINT");
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        for step in steps {
            run(&mut c, step);
        }
        (rows(&mut c, answer), rows(&mut c, shape))
    };
    // the log says the ALTERs as such, not the table made again
    let log = std::fs::metadata(format!("{}.wal", file.display())).unwrap().len();
    assert!(log < 100_000, "a log of {log} bytes");
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!((rows(&mut c, answer), rows(&mut c, shape)), expected);
}

#[test]
fn update_and_alter_read_only_their_columns() {
    let file = path("lazy_changes.db");
    {
        let db = Database::open_in_memory().unwrap();
        let mut c = db.connect().unwrap();
        run(
            &mut c,
            &format!("ATTACH '{}' AS db (STORAGE_VERSION 'v2.0.0')", file.display()),
        );
        run(
            &mut c,
            "CREATE TABLE db.t AS SELECT i c0, i * 2 c1, i::VARCHAR c2, i % 7 c3 FROM range(300000) r(i)",
        );
    }
    let loaded = "SELECT DISTINCT column_name FROM pragma_storage_info('t', loaded_segments_only = true) ORDER BY 1";
    let sums = "SELECT count(*), sum(c0), sum(c1), max(c2), sum(c4) FROM t";
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    run(&mut c, "UPDATE t SET c1 = -c1 WHERE c0 % 3 = 0");
    assert_eq!(rows(&mut c, loaded), ["c0", "c1"]);
    run(&mut c, "ALTER TABLE t ADD COLUMN c4 INTEGER DEFAULT 4");
    run(&mut c, "ALTER TABLE t DROP COLUMN c3");
    assert_eq!(rows(&mut c, loaded), ["c0", "c1", "c4"]);
    // a checkpoint writes the columns in memory and keeps the others as they were
    run(&mut c, "CHECKPOINT");
    assert_eq!(rows(&mut c, loaded), ["c0", "c1", "c4"]);
    let expected = rows(&mut c, sums);
    assert_eq!(expected, ["300000|44999850000|30000300000|99999|1200000"]);
    drop(c);
    drop(db);
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    assert!(rows(&mut c, loaded).is_empty());
    assert_eq!(rows(&mut c, sums), expected);
}

#[test]
fn filters_on_shredded_paths_are_the_same() {
    let file = path("path_filters.db");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "SET variant_minimum_shredding_size = 0");
        run(
            &mut c,
            "CREATE TABLE v AS SELECT {'x': i, 'y': i * 100, 's': 'k' || i}::VARIANT v, i FROM range(400000) r(i)",
        );
        run(
            &mut c,
            "CREATE TABLE m AS SELECT CASE WHEN i % 3 = 0 THEN {'x': i}::VARIANT WHEN i % 3 = 1 THEN {'y': i}::VARIANT \
             ELSE NULL END v, i FROM range(400000) r(i)",
        );
    }
    let db = Database::open(&file, true).unwrap();
    let mut c = db.connect().unwrap();
    for query in [
        "SELECT count(*), sum(i) FROM v WHERE v.x::BIGINT = 5",
        "SELECT count(*), sum(i) FROM v WHERE v.x::BIGINT BETWEEN 300000 AND 300010",
        "SELECT count(*) FROM v WHERE v.y::BIGINT > 39999000 AND v.x::BIGINT < 399995",
        "SELECT count(*) FROM v WHERE v.x::DOUBLE < 3.5",
        "SELECT count(*) FROM v WHERE v.s::VARCHAR = 'k7'",
        "SELECT i, v.y::BIGINT FROM v WHERE v.x::BIGINT IN (3, 399998) ORDER BY i",
        "SELECT count(*), sum(i) FROM m WHERE v.x::BIGINT > 399000",
        "SELECT count(*) FROM m WHERE v.y::BIGINT = 7",
    ] {
        let pushed = rows(&mut c, query);
        run(&mut c, "SET disabled_optimizers = 'unused_columns'");
        let whole = rows(&mut c, query);
        run(&mut c, "SET disabled_optimizers = ''");
        assert_eq!(pushed, whole, "{query}");
    }
}

/// `USING COMPRESSION`: a checkpoint stores a column only with its method, through a log, an ADD COLUMN and a
/// reopening.
#[test]
fn columns_keep_their_compression() {
    let file = path("using_compression.db");
    let methods = "SELECT column_name, compression FROM pragma_storage_info('t') WHERE segment_type NOT IN \
                   ('VALIDITY', 'INTEGER[]') GROUP BY ALL ORDER BY ALL";
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        run(
            &mut c,
            "CREATE TABLE t (a INTEGER USING COMPRESSION UNCOMPRESSED, b BIGINT USING COMPRESSION RLE, \
             s VARCHAR USING COMPRESSION ZSTD, l INTEGER[] USING COMPRESSION BITPACKING)",
        );
        run(
            &mut c,
            "INSERT INTO t SELECT i // 1000, i, 'v' || (i % 4), [i % 4] FROM range(50000) r(i)",
        );
        run(
            &mut c,
            "ALTER TABLE t ADD COLUMN d INTEGER USING COMPRESSION UNCOMPRESSED DEFAULT 42",
        );
    }
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    run(&mut c, "CHECKPOINT");
    assert_eq!(
        rows(&mut c, methods),
        // a column of one value is a constant, whatever its method, as in C++
        ["a|Uncompressed", "b|RLE", "d|Constant", "l|BitPacking", "s|ZSTD"]
    );
    assert_eq!(
        rows(&mut c, "SELECT sql FROM duckdb_tables() WHERE table_name = 't'"),
        [
            "CREATE TABLE t(a INTEGER USING COMPRESSION Uncompressed, b BIGINT USING COMPRESSION RLE, s VARCHAR USING \
          COMPRESSION ZSTD, l INTEGER[] USING COMPRESSION BitPacking, d INTEGER DEFAULT(42) USING COMPRESSION \
          Uncompressed);"
        ]
    );
    refused(
        &mut c,
        "CREATE TABLE u (x VARCHAR USING COMPRESSION RLE)",
        "Can't compress column \"x\" with type 'VARCHAR' (physical: VARCHAR) using compression type 'RLE'",
    );
    refused(
        &mut c,
        "CREATE TABLE u (x VARCHAR USING COMPRESSION Dictionary)",
        "that type is deprecated and only has decompress support",
    );
}

/// `DataTable::Delete` of a table in a file: the rows go from its version info, through a checkpoint and a log,
/// and only the columns of the WHERE are read.
#[test]
fn delete_reads_only_its_filter() {
    let file = path("lazy_delete.db");
    let create = "CREATE TABLE t AS SELECT i c0, i * 2 c1, i::VARCHAR c2 FROM range(300000) r(i)";
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, create);
    }
    let loaded = "SELECT DISTINCT column_name FROM pragma_storage_info('t', loaded_segments_only = true) ORDER BY 1";
    let sums = "SELECT count(*), sum(c0), sum(c1), max(c2) FROM t";
    let expected = {
        let db = Database::open_in_memory().unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, create);
        run(&mut c, "DELETE FROM t WHERE c0 % 3 = 0");
        run(&mut c, "DELETE FROM t WHERE c0 > 290000");
        rows(&mut c, sums)
    };
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "DELETE FROM t WHERE c0 % 3 = 0");
        assert_eq!(rows(&mut c, loaded), ["c0"]);
        // a checkpoint keeps the columns and writes which rows are left
        run(&mut c, "CHECKPOINT");
        assert_eq!(rows(&mut c, loaded), ["c0"]);
        // in the log only
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        run(&mut c, "DELETE FROM t WHERE c0 > 290000");
    }
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    assert!(rows(&mut c, loaded).is_empty());
    run(&mut c, "CHECKPOINT");
    assert!(rows(&mut c, loaded).is_empty());
    assert_eq!(rows(&mut c, sums), expected);
}
