//! Writes files and logs, reads them back, and kills what writes them. `tools/check_written.py` has the real DuckDB read one of them.

use duckrs::{Connection, Database};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        let state: u64 = rows(&mut connection, "SELECT n FROM state")[0].parse().unwrap();
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
