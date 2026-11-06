//! A row keeps its id until it goes, through whatever happens to the rows around it, and in the
//! file and the log too. A wrong id is a right answer to any query that doesn't ask for it, so this
//! keeps its own list of the rows and asks after each step.

use duckrs::{Connection, Database};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn path(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("duckrs_row_ids_{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join(name);
    for path in [file.clone(), PathBuf::from(format!("{}.wal", file.display()))] {
        let _ = std::fs::remove_file(path);
    }
    file
}

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
}

fn rows(connection: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.row_count())
        .map(|row| {
            let values = result.collection.row(row);
            values
                .iter()
                .map(|value| value.try_to_string().unwrap_or_default())
                .collect()
        })
        .collect()
}

/// Rows by id: their key, which has an index, and what changes in place.
type Model = BTreeMap<i64, (i64, i64)>;

fn check(connection: &mut Connection, model: &Model, step: &str) {
    let found = rows(connection, "SELECT rowid, k, note FROM f.t ORDER BY rowid");
    let expected: Vec<Vec<String>> = model
        .iter()
        .map(|(id, (key, note))| vec![id.to_string(), key.to_string(), note.to_string()])
        .collect();
    assert_eq!(found.len(), expected.len(), "{step}");
    if let Some(row) = (0..found.len()).find(|row| found[*row] != expected[*row]) {
        panic!("{step}: row {row} is {:?}, not {:?}", found[row], expected[row]);
    }
    // an index has to find them under the same ids
    for (id, (key, _)) in model.iter().step_by(model.len() / 50 + 1) {
        let found = rows(connection, &format!("SELECT rowid FROM f.t WHERE k = {key}"));
        assert_eq!(found, [[id.to_string()]], "{step}: key {key}");
    }
}

/// `rows` new ones, whose ids come after all that were given out. Their keys say which they are.
fn added(connection: &mut Connection, model: &mut Model, next_key: &mut i64, rows_added: i64, least: &mut i64) {
    let first = *next_key;
    *next_key += rows_added;
    run(
        connection,
        &format!("INSERT INTO f.t SELECT i, 0 FROM range({first}, {next_key}) r(i)"),
    );
    adopt(connection, model, first, *next_key, least);
}

/// The ids of the rows with keys from `first` up to `end`, which are new.
fn adopt(connection: &mut Connection, model: &mut Model, first: i64, end: i64, least: &mut i64) {
    let sql = format!("SELECT rowid, k, note FROM f.t WHERE k >= {first} AND k < {end} ORDER BY rowid");
    for row in rows(connection, &sql) {
        let id: i64 = row[0].parse().unwrap();
        assert!(id >= *least, "id {id} was given out before");
        model.insert(id, (row[1].parse().unwrap(), row[2].parse().unwrap()));
        *least = id + 1;
    }
}

/// A simple generator, so that a failure comes back the same.
struct Random(u64);

impl Random {
    fn next(&mut self, below: i64) -> i64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % below as u64) as i64
    }
}

/// What the file and the log say is what the database had.
fn reopen(connection: &mut Connection, file: &PathBuf, crash: bool) {
    let copy = PathBuf::from(format!("{}.copy", file.display()));
    let copy_log = format!("{}.wal", copy.display());
    let _ = std::fs::remove_file(&copy_log);
    if crash {
        std::fs::copy(file, &copy).unwrap();
        let log = format!("{}.wal", file.display());
        if std::fs::metadata(&log).is_ok() {
            std::fs::copy(&log, &copy_log).unwrap();
        }
    }
    run(connection, "DETACH f");
    if crash {
        std::fs::rename(&copy, file).unwrap();
        let log = format!("{}.wal", file.display());
        let _ = std::fs::remove_file(&log);
        if std::fs::metadata(&copy_log).is_ok() {
            std::fs::rename(&copy_log, log).unwrap();
        }
    }
    run(
        connection,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 2048)", file.display()),
    );
}

#[test]
fn ids_stay_through_everything() {
    for seed in 1..=4u64 {
        let file = path(&format!("stay{seed}.db"));
        let db = Database::open(path(&format!("home{seed}.db")), false).unwrap();
        let mut connection = db.connect().unwrap();
        let connection = &mut connection;
        run(connection, "SET max_vacuum_tasks = 0");
        run(connection, "SET index_scan_percentage = 1.0");
        run(connection, "SET index_scan_max_count = 1000000000");
        run(
            connection,
            &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 2048)", file.display()),
        );
        run(connection, "CREATE TABLE f.t (k BIGINT, note BIGINT)");
        run(connection, "CREATE INDEX t_k ON f.t (k)");
        let (mut model, mut next_key, mut least) = (Model::new(), 0, 0);
        let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        added(connection, &mut model, &mut next_key, 5000, &mut least);
        for step in 0..60 {
            let (modulus, rest) = (2 + random.next(40), random.next(2));
            let filter = format!("k % {modulus} = {rest}");
            let what = random.next(10);
            let name = format!("seed {seed} step {step} did {what} on {filter}");
            match what {
                0 | 1 => {
                    let rows_added = 1 + random.next(3000);
                    added(connection, &mut model, &mut next_key, rows_added, &mut least);
                }
                2 => {
                    run(connection, &format!("DELETE FROM f.t WHERE {filter}"));
                    model.retain(|_, (key, _)| *key % modulus != rest);
                }
                3 => {
                    run(connection, &format!("UPDATE f.t SET note = note + 1 WHERE {filter}"));
                    for (key, note) in model.values_mut() {
                        *note += i64::from(*key % modulus == rest);
                    }
                }
                4 => {
                    // what has an index changes by being deleted and inserted
                    let moved: Vec<i64> = model
                        .iter()
                        .filter(|(_, (key, _))| *key % modulus == rest)
                        .map(|(id, _)| *id)
                        .collect();
                    let first = next_key;
                    run(
                        connection,
                        &format!("UPDATE f.t SET k = k + {} WHERE {filter}", first * 2),
                    );
                    let notes: Vec<(i64, i64)> = moved
                        .iter()
                        .map(|id| {
                            let (key, note) = model.remove(id).unwrap();
                            (key + first * 2, note)
                        })
                        .collect();
                    next_key = next_key.max(notes.iter().map(|(key, _)| key + 1).max().unwrap_or(0));
                    let end = next_key.max(first * 2 + 1);
                    adopt(connection, &mut model, first * 2, end, &mut least);
                    for (key, note) in notes {
                        assert!(model.values().any(|row| *row == (key, note)), "{name}");
                    }
                }
                5 => {
                    run(connection, "BEGIN");
                    run(connection, &format!("DELETE FROM f.t WHERE {filter}"));
                    run(connection, "INSERT INTO f.t VALUES (-1, -1)");
                    run(connection, "UPDATE f.t SET note = -1");
                    assert!(
                        rows(connection, "SELECT rowid FROM f.t WHERE k = -1")[0][0]
                            .parse::<i64>()
                            .unwrap()
                            >= 1 << 55
                    );
                    run(connection, "ROLLBACK");
                }
                6 => {
                    run(connection, "BEGIN");
                    run(connection, &format!("UPDATE f.t SET note = note + 2 WHERE {filter}"));
                    run(
                        connection,
                        &format!("DELETE FROM f.t WHERE k % {modulus} = {}", 1 - rest),
                    );
                    let first = next_key;
                    next_key += 100;
                    run(
                        connection,
                        &format!("INSERT INTO f.t SELECT i, 0 FROM range({first}, {next_key}) r(i)"),
                    );
                    run(connection, "COMMIT");
                    model.retain(|_, (key, _)| *key % modulus != 1 - rest);
                    for (key, note) in model.values_mut() {
                        *note += 2 * i64::from(*key % modulus == rest);
                    }
                    adopt(connection, &mut model, first, next_key, &mut least);
                }
                7 => {
                    run(connection, "CHECKPOINT f");
                    least = rows(connection, "SELECT coalesce(max(rowid) + 1, 0) FROM f.t")[0][0]
                        .parse()
                        .unwrap();
                }
                8 => reopen(connection, &file, false),
                _ => reopen(connection, &file, true),
            }
            check(connection, &model, &name);
            if what >= 8 {
                least = rows(connection, "SELECT coalesce(max(rowid) + 1, 0) FROM f.t")[0][0]
                    .parse()
                    .unwrap();
            }
        }
    }
}

/// Coming together changes ids, and nothing else: rows stay in order and indexes follow them.
#[test]
fn a_vacuum_only_renumbers() {
    let file = path("vacuum.db");
    let db = Database::open(path("home_vacuum.db"), false).unwrap();
    let mut connection = db.connect().unwrap();
    let connection = &mut connection;
    run(
        connection,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 2048)", file.display()),
    );
    run(
        connection,
        "CREATE TABLE f.t AS SELECT i AS k, i * 2 AS note FROM range(20480) r(i)",
    );
    run(connection, "CREATE UNIQUE INDEX t_k ON f.t (k)");
    run(connection, "SET index_scan_percentage = 1.0");
    run(connection, "SET index_scan_max_count = 1000000000");
    run(connection, "CHECKPOINT f");
    run(connection, "DELETE FROM f.t WHERE k % 2 = 0 OR k BETWEEN 6000 AND 9000");
    run(connection, "CHECKPOINT f");
    let groups = rows(
        connection,
        "SELECT count(DISTINCT row_group_id) FROM pragma_storage_info('f.t')",
    );
    assert!(groups[0][0].parse::<i64>().unwrap() < 10, "{groups:?}");
    let expected: Vec<i64> = (0..20480)
        .filter(|k| k % 2 == 1 && !(6000..=9000).contains(k))
        .collect();
    let found = rows(connection, "SELECT rowid, k, note FROM f.t ORDER BY rowid");
    let keys: Vec<i64> = found.iter().map(|row| row[1].parse().unwrap()).collect();
    assert_eq!(keys, expected);
    for row in found.iter().step_by(97) {
        assert_eq!(row[2].parse::<i64>().unwrap(), row[1].parse::<i64>().unwrap() * 2);
        let looked_up = rows(connection, &format!("SELECT rowid FROM f.t WHERE k = {}", row[1]));
        assert_eq!(looked_up, [[row[0].clone()]]);
    }
    run(connection, "INSERT INTO f.t VALUES (-7, 0)");
    assert_eq!(rows(connection, "SELECT count(*) FROM f.t WHERE k = -7"), [["1"]]);
}

/// For `tools/check_written.py`: rows with gaps of each kind that DuckDB has to read, by id.
#[test]
fn gaps_for_duckdb() {
    let Some(target) = std::env::var_os("DUCKRS_KEEP_GAPS") else {
        return;
    };
    let target = PathBuf::from(target);
    let db = Database::open(path("home_gaps.db"), false).unwrap();
    let mut connection = db.connect().unwrap();
    let connection = &mut connection;
    run(connection, "SET max_vacuum_tasks = 0");
    run(connection, "PRAGMA disable_checkpoint_on_shutdown");
    run(
        connection,
        &format!("ATTACH '{}' AS f (ROW_GROUP_SIZE 2048)", target.display()),
    );
    run(
        connection,
        "CREATE TABLE f.gaps AS SELECT i, 'row ' || i AS s, [i] AS l FROM range(12288) r(i)",
    );
    run(connection, "CREATE UNIQUE INDEX gaps_i ON f.gaps (i)");
    run(connection, "CHECKPOINT f");
    // a row group that goes, one with gaps in it, and vectors with no row left
    run(
        connection,
        "DELETE FROM f.gaps WHERE i BETWEEN 2048 AND 4095 OR (i BETWEEN 4096 AND 6143 AND i % 3 = 0) OR i BETWEEN 8192 AND 10239 OR i >= 12280",
    );
    run(connection, "CHECKPOINT f");
    let mut report = String::new();
    for row in rows(
        connection,
        "SELECT count(*), min(rowid), max(rowid), sum(rowid), sum(i) FROM f.gaps",
    ) {
        report += &row.join("\t");
    }
    report += "\n";
    run(connection, "UPDATE f.gaps SET s = 'changed' WHERE i % 7 = 1");
    run(connection, "DELETE FROM f.gaps WHERE i % 7 = 2");
    run(
        connection,
        "INSERT INTO f.gaps SELECT i, 'added', [] FROM range(100000, 100010) r(i)",
    );
    run(connection, "UPDATE f.gaps SET i = i + 1000000 WHERE i % 11 = 3");
    for row in rows(
        connection,
        "SELECT count(*), min(rowid), max(rowid), sum(rowid), sum(i), count(*) FILTER (s = 'changed') FROM f.gaps",
    ) {
        report += &row.join("\t");
    }
    report += "\n";
    std::fs::write(format!("{}.expected", target.display()), report).unwrap();
    // the log stays for DuckDB to replay
    std::mem::forget(db);
}
