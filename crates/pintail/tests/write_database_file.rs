//! Writes files and logs, reads them back, and kills what writes them. `tools/check_written.py` has the real DuckDB read one of them.

mod common;

use pintail::{Connection, Database};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn path(name: &str) -> PathBuf {
    let directory = common::directory("pintail-write-");
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
    // the sums of values that aren't round are only as exact as the order a scan adds them in, which depends on
    // whether the rows are in memory or in a file: the hash of each value says they all came back
    "SELECT count(*), sum(d), sum(f), round(sum(irregular), 12), round(sum(root), 6), sum(hash(irregular, root)), \
     count(odd), count(*) FILTER (WHERE isnan(odd)), \
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
    // for `tools/check_written.py`: the file, and what pintail says about it
    if let Some(target) = std::env::var_os("PINTAIL_KEEP_WRITTEN") {
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
    if let Some(target) = std::env::var_os("PINTAIL_KEEP_KEYS") {
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
    if let Some(target) = std::env::var_os("PINTAIL_KEEP_KEYS_LOGGED") {
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
    if let Some(target) = std::env::var_os("PINTAIL_KEEP_LOGGED") {
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
    // a view that pintail can't write to the log yet (SHOW and DESCRIBE in a stored query)
    run(
        &mut connection,
        "CREATE VIEW unstorable AS SELECT * FROM (DESCRIBE kept)",
    );
    assert!(connection.query("COMMIT").is_err());
    run(&mut connection, "DROP VIEW IF EXISTS unstorable");
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
        if let Some(target) = std::env::var_os("PINTAIL_KEEP_FLOATS") {
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
        if let Some(target) = std::env::var_os("PINTAIL_KEEP_FLOATS").filter(|_| force == "auto") {
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

const CRASH_FILE: &str = "PINTAIL_CRASH_FILE";

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
    if let Some(target) = std::env::var_os("PINTAIL_KEEP_KEYS_ALTERED") {
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

/// `OptimisticDataWriter`: a commit of at least a row group of new rows writes them to blocks of their own, which the
/// log points to rather than holding the rows, and which a checkpoint keeps.
#[test]
fn large_appends_are_written_ahead() {
    let file = path("written_ahead.db");
    let create = "CREATE TABLE t AS SELECT i, i::VARCHAR s FROM range(300000) r(i)";
    let sums = "SELECT count(*), sum(i), max(s), sum(hash(s)) FROM t";
    let blocks = "SELECT block_id, block_offset FROM pragma_storage_info('t') WHERE block_id >= 0 ORDER BY ALL";
    let expected = {
        let db = Database::open_in_memory().unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, create);
        rows(&mut c, sums)
    };
    let mut log = file.clone().into_os_string();
    log.push(".wal");
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        run(&mut c, create);
        assert_eq!(rows(&mut c, sums), expected);
    }
    // the rows are in blocks; the log only says where
    assert!(std::fs::metadata(&log).unwrap().len() < 16 * 1024);
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(rows(&mut c, sums), expected);
    run(&mut c, "CHECKPOINT");
    let kept = rows(&mut c, blocks);
    assert_eq!(rows(&mut c, sums), expected);
    // an append of fewer rows than a row group goes in the log as rows
    run(&mut c, "INSERT INTO t VALUES (-1, 'x')");
    run(&mut c, "CHECKPOINT");
    let after: Vec<String> = rows(&mut c, blocks);
    assert!(kept.iter().all(|block| after.contains(block)), "{kept:?} {after:?}");
}

/// `OptimisticDataWriter::WriteNewRowGroup`: an append writes its full row groups to blocks as it goes and keeps only
/// the rest in memory; a statement that fails after that leaves the table as it was.
#[test]
fn appends_write_row_groups_as_they_fill() {
    let file = path("written_as_they_fill.db");
    let sums = "SELECT count(*), sum(a), sum(hash(s)) FROM t";
    let expected = {
        let db = Database::open_in_memory().unwrap();
        let mut c = db.connect().unwrap();
        run(
            &mut c,
            "CREATE TABLE t AS SELECT i a, 'v' || (i % 977) s FROM range(1000000) r(i)",
        );
        run(&mut c, "INSERT INTO t SELECT i, 'w' || i FROM range(700000) r(i)");
        rows(&mut c, sums)
    };
    {
        let db = Database::open(&file, false).unwrap();
        let mut c = db.connect().unwrap();
        run(&mut c, "PRAGMA disable_checkpoint_on_shutdown");
        run(
            &mut c,
            "CREATE TABLE t AS SELECT i a, 'v' || (i % 977) s FROM range(1000000) r(i)",
        );
        // fails at its last rows, when most of them are in blocks
        let failed = c.query(
            "INSERT INTO t SELECT i, CASE WHEN i = 690000 THEN error('late') ELSE 'w' || i END FROM range(700000) r(i)",
        );
        assert!(failed.is_err());
        assert_eq!(rows(&mut c, "SELECT count(*) FROM t"), ["1000000"]);
        run(&mut c, "INSERT INTO t SELECT i, 'w' || i FROM range(700000) r(i)");
        assert_eq!(rows(&mut c, sums), expected);
    }
    let db = Database::open(&file, false).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(rows(&mut c, sums), expected);
    run(&mut c, "CHECKPOINT");
    assert_eq!(rows(&mut c, sums), expected);
}

/// A CSV file of `rows` rows after a header, of more than the 8MB a thread scans: a bad value at row `bad`, and
/// with `quoted`, values in quotes with newlines in them.
fn big_csv(name: &str, rows: usize, bad: Option<usize>, quoted: bool) -> PathBuf {
    use std::fmt::Write;
    let file = path(name);
    let mut text = String::from("id,price,note,day\n");
    for row in 0..rows {
        let price = if Some(row) == bad {
            "oops".to_string()
        } else {
            format!("{}.{:02}", row % 9973, row % 100)
        };
        let note = if quoted && row % 1000 == 7 {
            format!("\"line {row}\nand the next, of it\"")
        } else {
            format!("note number {row} of the file with some words after it")
        };
        writeln!(
            text,
            "{row},{price},{note},1995-{:02}-{:02}",
            row % 12 + 1,
            row % 28 + 1
        )
        .unwrap();
    }
    std::fs::write(&file, text).unwrap();
    file
}

/// `COPY` of `csv` into a new table of a new file with `threads`, in row groups of 16384 rows, so that the rows fill
/// several of `WRITE_BUFFER_ROW_GROUP_COUNT` that are written ahead: what comes of it, the rows in their order, and
/// where the segments of the row groups are, before and after a checkpoint.
fn copied(name: &str, csv: &std::path::Path, threads: usize) -> Result<Vec<String>, String> {
    loaded(name, &format!("COPY f.t FROM '{}' (HEADER)", csv.display()), threads)
}

/// [`copied`] with `load`, which fills `f.t`.
fn loaded(name: &str, load: &str, threads: usize) -> Result<Vec<String>, String> {
    let file = path(name);
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(&mut c, &format!("SET threads = {threads}"));
    run(
        &mut c,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 16384)", file.display()),
    );
    run(
        &mut c,
        "CREATE TABLE f.t (id BIGINT, price DECIMAL(15,2), note VARCHAR, day DATE)",
    );
    c.query(load).map_err(|error| error.to_string())?;
    let layout = "SELECT count(*), count(DISTINCT row_group_id), sum(hash(row_group_id, column_id, segment_id, \
                  block_id, block_offset, compression)) FROM pragma_storage_info('f.t')";
    let mut result = rows(
        &mut c,
        "SELECT count(*), sum(price), sum(hash(rowid, id, price, note, day)), max(rowid) FROM f.t",
    );
    result.extend(rows(&mut c, layout));
    run(&mut c, "CHECKPOINT f");
    result.extend(rows(&mut c, layout));
    Ok(result)
}

#[test]
fn csv_loads_on_threads_as_on_one() {
    let csv = big_csv("threads.csv", 400_000, None, false);
    assert!(std::fs::metadata(&csv).unwrap().len() > 16 << 20);
    let one = copied("threads_1.db", &csv, 1).unwrap();
    assert_eq!(copied("threads_8.db", &csv, 8).unwrap(), one);
    assert_eq!(one[0].split('|').next(), Some("400000"));
    // quotes in the file, which a part can start after
    let quoted = big_csv("quoted.csv", 400_000, None, true);
    assert_eq!(
        copied("quoted_8.db", &quoted, 8).unwrap(),
        copied("quoted_1.db", &quoted, 1).unwrap()
    );
    // an error in a part after the first is the one the scan on one thread finds, with its line
    let bad = big_csv("bad.csv", 400_000, Some(350_000), false);
    let error = copied("bad_1.db", &bad, 1).unwrap_err();
    assert!(error.contains("Line: 350002"), "{error}");
    assert_eq!(copied("bad_8.db", &bad, 8).unwrap_err(), error);
}

/// A file with quotes in it loads on threads as on one: its parts start where rows do, though a quoted value has
/// `\n`s, `\r\n`s, delimiters and escaped quotes in it, its first quote is far into the file, and its lines end in
/// `\r\n`. An error after the quotes is the one the scan on one thread finds, with its line.
#[test]
fn quoted_csv_loads_on_threads_as_on_one() {
    use std::fmt::Write;
    let csv = |name: &str, end: &str, bad: Option<usize>| {
        let file = path(name);
        let mut text = format!("id,price,note,day{end}");
        for row in 0..400_000usize {
            let price = if Some(row) == bad {
                "oops".to_string()
            } else {
                format!("{}.{:02}", row % 9973, row % 100)
            };
            let note = match row % 5 {
                _ if row < 150_000 => format!("note number {row} of the file"),
                0 => format!("\"said \"\"{row}\"\",{end}then\nleft\r\n\""),
                1 => format!("\"{row}\n\n\""),
                2 => "\"\"".to_string(),
                _ => format!("note {row}"),
            };
            write!(
                text,
                "{row},{price},{note},1995-{:02}-{:02}{end}",
                row % 12 + 1,
                row % 28 + 1
            )
            .unwrap();
        }
        std::fs::write(&file, text).unwrap();
        file
    };
    for end in ["\n", "\r\n"] {
        let name = if end == "\n" { "lf" } else { "crlf" };
        let quoted = csv(&format!("quotes_{name}.csv"), end, None);
        // the sniffer's rows have no quote in them
        let copy = |file: &PathBuf| format!("COPY f.t FROM '{}' (HEADER, QUOTE '\"')", file.display());
        let one = loaded(&format!("quotes_{name}_1.db"), &copy(&quoted), 1).unwrap();
        assert_eq!(
            loaded(&format!("quotes_{name}_8.db"), &copy(&quoted), 8).unwrap(),
            one,
            "{name}"
        );
        assert_eq!(one[0].split('|').next(), Some("400000"));
        // read in buffers of 160000 bytes, in parts of 40000: the first quote is in a buffer after the last part's
        // start, from which the state machine runs on
        let read = format!(
            "INSERT INTO f.t SELECT * FROM read_csv('{}', header = true, quote = '\"', buffer_size = 160000, \
             max_line_size = 10000, columns = {{'id': 'BIGINT', 'price': 'DECIMAL(15,2)', 'note': 'VARCHAR', 'day': 'DATE'}})",
            quoted.display()
        );
        for threads in [1, 8] {
            let streamed = loaded(&format!("quotes_{name}_parts_{threads}.db"), &read, threads).unwrap();
            assert_eq!(streamed[..3], one[..3], "{name} in buffers on {threads} threads");
        }
    }
    let bad = csv("quotes_bad.csv", "\n", Some(350_000));
    let copy = format!("COPY f.t FROM '{}' (HEADER, QUOTE '\"')", bad.display());
    let error = loaded("quotes_bad_1.db", &copy, 1).unwrap_err();
    assert!(error.contains("oops"), "{error}");
    assert_eq!(loaded("quotes_bad_8.db", &copy, 8).unwrap_err(), error);
}

/// The distinct values of a column, which the join order's estimates start from, are kept in the file as C++ keeps
/// them (`DistinctStatistics`): read back, they give the estimates the rows gave before the checkpoint, and those of
/// the same rows in memory, without counting them again. The rows of one table stay in memory until the checkpoint,
/// those of the other are written ahead in row groups as they are inserted.
#[test]
fn distinct_counts_are_kept_in_the_file() {
    let file = path("distinct.db");
    let query = "EXPLAIN SELECT * FROM a, b WHERE a.k = b.k AND b.v = 'v3'";
    let create = |c: &mut Connection| {
        run(c, "CREATE TABLE a (k BIGINT, v VARCHAR)");
        run(c, "CREATE TABLE b (k BIGINT, v VARCHAR)");
        run(c, "INSERT INTO a SELECT i % 997, 'v' || (i % 31) FROM range(3000) t(i)");
        run(
            c,
            "INSERT INTO b SELECT i % 50000, 'v' || (i % 7) FROM range(300000) t(i)",
        );
    };
    let estimates = |c: &mut Connection| -> Vec<String> {
        rows(c, query)
            .iter()
            .flat_map(|row| {
                row.split('~')
                    .skip(1)
                    .map(|part| part.split(' ').next().unwrap_or("").to_string())
            })
            .collect::<Vec<_>>()
    };
    let memory = Database::open_in_memory().unwrap();
    let mut m = memory.connect().unwrap();
    create(&mut m);
    let in_memory = estimates(&mut m);
    assert!(!in_memory.is_empty());
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(
        &mut c,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 16384)", file.display()),
    );
    run(&mut c, "USE f");
    create(&mut c);
    assert_eq!(estimates(&mut c), in_memory, "before the checkpoint");
    run(&mut c, "CHECKPOINT f");
    run(&mut c, "USE memory");
    run(&mut c, "DETACH f");
    run(&mut c, &format!("ATTACH '{}' AS f", file.display()));
    run(&mut c, "USE f");
    assert_eq!(estimates(&mut c), in_memory, "read from the file");
}

/// Comparisons of a column of integers with constants that a scan of a file evaluates as a range of the values
/// keep what the same comparisons keep evaluated as expressions (`+ 0` makes them those): of each width of
/// integer, DATE and DECIMAL, with NULLs, BETWEEN, a constant on the left, equality, a range that is empty and
/// bounds at the ends of BIGINT.
#[test]
fn ranges_of_constants_filter_as_comparisons_do() {
    let file = path("ranges.db");
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(
        &mut c,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 16384)", file.display()),
    );
    run(
        &mut c,
        "CREATE TABLE f.t AS SELECT i, (i % 200 - 100)::TINYINT AS a, (i % 30000)::SMALLINT AS b, \
         (i * 7 % 100000)::INTEGER AS c, CASE WHEN i % 13 = 0 THEN NULL ELSE i * 1000003 - 50000000000 END AS d, \
         DATE '1990-01-01' + (i % 4000)::INTEGER AS e, (i % 1000 / 100)::DECIMAL(15,2) AS g \
         FROM range(100000) t(i)",
    );
    run(&mut c, "CHECKPOINT f");
    for filter in [
        "a < -50",
        "a >= 90 AND a <= 99",
        "b BETWEEN 100 AND 2000",
        "1000 < b AND b < 1100",
        "c = 70007",
        "c > 99990 AND c < 5",
        "d >= 0 AND d < 10000000000",
        "d < -9223372036854775808",
        "d > 9223372036854775807",
        "d <= 9223372036854775807",
        "e >= DATE '1994-01-01' AND e < DATE '1995-01-01'",
        "g BETWEEN 0.05 AND 7.00",
        "g = 3.00 AND c < 50000",
    ] {
        let as_expressions = filter
            .replace("a ", "(a + 0) ")
            .replace("b ", "(b + 0) ")
            .replace("c ", "(c + 0) ")
            .replace("d ", "(d + 0) ")
            .replace("e ", "(e + 0) ")
            .replace("g ", "(g + 0) ");
        let mut count = |filter: &str| -> Vec<String> {
            let sql = format!("SELECT count(*), sum(i), min(d), max(d) FROM f.t WHERE {filter}");
            rows(&mut c, &sql)
        };
        assert_eq!(count(filter), count(&as_expressions), "{filter}");
    }
}

/// The strings of a few rows of a row group that a filtered scan of a file fetches are each decoded from their
/// entry of the dictionary: they are what a scan of all the rows reads, for few and for many distinct strings,
/// empty ones and NULLs, and for few and for many rows of each group.
#[test]
fn strings_of_some_rows_read_as_all_rows_do() {
    let file = path("strings.db");
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(
        &mut c,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 16384)", file.display()),
    );
    run(
        &mut c,
        "CREATE TABLE f.t AS SELECT i, CASE i % 7 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE ['R', 'A', 'N'][i % 3 + 1] \
         END AS low, CASE WHEN i % 11 = 0 THEN NULL ELSE 'customer ' || i || ' furiously pending deposits ' || \
         (i * 7919 % 1000) END AS high FROM range(100000) t(i)",
    );
    run(&mut c, "CHECKPOINT f");
    // a copy in memory, of a scan of all the rows
    run(&mut c, "CREATE TABLE m AS SELECT * FROM f.t");
    for filter in [
        "i % 97 = 3",
        "i BETWEEN 1000 AND 1200",
        "i % 3 = 0",
        "i % 5 < 2",
        "low = 'R'",
    ] {
        let sql = |table: &str| {
            format!(
                "SELECT count(*), count(low), count(high), sum(hash(i, low, high)), max(high) FROM {table} WHERE \
                 {filter}"
            )
        };
        assert_eq!(rows(&mut c, &sql("f.t")), rows(&mut c, &sql("m")), "{filter}");
    }
}

/// A file read a buffer at a time, on one thread and in parts, gives what it does read whole: in buffers of 160000
/// bytes and parts of 40000 (four lines of at most 10000), rows and quoted values with newlines in them go across
/// many of each. The sniffer reads its 2048 rows of the first buffer.
#[test]
fn csv_read_in_buffers_as_whole() {
    let read = |csv: &PathBuf, line: &str| {
        format!(
            "INSERT INTO f.t SELECT * FROM read_csv('{}', header = true, sample_size = 100{line}, columns = {{'id': \
             'BIGINT', 'price': 'DECIMAL(15,2)', 'note': 'VARCHAR', 'day': 'DATE'}})",
            csv.display()
        )
    };
    for (name, quoted, bad) in [
        ("plain", false, None),
        ("quotes", true, None),
        ("error", false, Some(90_000)),
    ] {
        let csv = big_csv(&format!("buffers_{name}.csv"), 100_000, bad, quoted);
        // smaller than a buffer of the usual size, so read whole
        let whole = loaded(&format!("buffers_{name}_whole.db"), &read(&csv, ""), 1);
        for threads in [1, 8] {
            let streamed = loaded(
                &format!("buffers_{name}_{threads}.db"),
                &read(&csv, ", buffer_size = 160000, max_line_size = 10000"),
                threads,
            );
            assert_eq!(streamed, whole, "{name} on {threads} threads");
        }
    }
}
