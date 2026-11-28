//! Threads against one database, checked by what has to hold however they interleave.

use pintail::{Connection, Database};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
}

fn single(connection: &mut Connection, sql: &str) -> i64 {
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    result.collection.row(0)[0].try_to_string().unwrap().parse().unwrap()
}

const ACCOUNTS: usize = 20;
const OPENING_BALANCE: usize = 1000;

fn with_accounts(database: &Database) {
    let mut connection = database.connect().unwrap();
    run(
        &mut connection,
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance BIGINT NOT NULL)",
    );
    run(
        &mut connection,
        &format!("INSERT INTO accounts SELECT i, {OPENING_BALANCE} FROM range({ACCOUNTS}) r(i)"),
    );
}

/// Moves money around from many threads, with readers looking on: no snapshot may ever show money
/// that appeared or went missing, whichever transfers lost a conflict.
fn transfers_keep_the_total(database: &Arc<Database>) {
    with_accounts(database);
    let total = (ACCOUNTS * OPENING_BALANCE) as i64;
    let committed = Arc::new(AtomicUsize::new(0));
    let conflicts = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let database = database.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut connection = database.connect().unwrap();
                while !done.load(Ordering::SeqCst) {
                    run(&mut connection, "BEGIN");
                    let first = single(&mut connection, "SELECT sum(balance) FROM accounts");
                    assert_eq!(first, total, "a snapshot lost money");
                    assert_eq!(single(&mut connection, "SELECT sum(balance) FROM accounts"), first);
                    run(&mut connection, "COMMIT");
                    assert_eq!(
                        single(&mut connection, "SELECT count(*) FROM accounts"),
                        ACCOUNTS as i64
                    );
                }
            })
        })
        .collect();
    let writers: Vec<_> = (0..8)
        .map(|thread| {
            let database = database.clone();
            let committed = committed.clone();
            let conflicts = conflicts.clone();
            thread::spawn(move || {
                let mut connection = database.connect().unwrap();
                for round in 0..150 {
                    let from = (thread * 3 + round) % ACCOUNTS;
                    let to = (thread * 7 + round * 2 + 1) % ACCOUNTS;
                    if from == to {
                        continue;
                    }
                    run(&mut connection, "BEGIN");
                    let moved = [
                        format!("UPDATE accounts SET balance = balance - 5 WHERE id = {from}"),
                        format!("UPDATE accounts SET balance = balance + 5 WHERE id = {to}"),
                    ]
                    .iter()
                    .all(|sql| connection.query(sql).is_ok());
                    if moved && connection.query("COMMIT").is_ok() {
                        committed.fetch_add(1, Ordering::SeqCst);
                    } else {
                        conflicts.fetch_add(1, Ordering::SeqCst);
                        let _ = connection.query("ROLLBACK");
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    for reader in readers {
        reader.join().unwrap();
    }
    let mut connection = database.connect().unwrap();
    assert_eq!(single(&mut connection, "SELECT sum(balance) FROM accounts"), total);
    assert!(committed.load(Ordering::SeqCst) > 0, "nothing got through");
    assert!(conflicts.load(Ordering::SeqCst) > 0, "these threads never met");
}

#[test]
fn transfers_keep_the_total_in_memory() {
    transfers_keep_the_total(&Arc::new(Database::open_in_memory().unwrap()));
}

#[test]
fn transfers_keep_the_total_in_a_file() {
    let directory = std::env::temp_dir().join(format!("pintail-concurrent-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("transfers.db");
    let _ = std::fs::remove_file(&file);
    {
        let database = Arc::new(Database::open(&file, false).unwrap());
        transfers_keep_the_total(&database);
    }
    let database = Database::open(&file, false).unwrap();
    let mut connection = database.connect().unwrap();
    let expected = (ACCOUNTS * OPENING_BALANCE) as i64;
    assert_eq!(single(&mut connection, "SELECT sum(balance) FROM accounts"), expected);
    connection.query("INSERT INTO accounts VALUES (0, 0)").unwrap_err();
    drop(connection);
    drop(database);
    std::fs::remove_dir_all(directory).unwrap();
}

/// All threads try to add the same keys: each key gets in once, whoever wins it.
#[test]
fn each_key_is_won_once() {
    const KEYS: usize = 200;
    const THREADS: usize = 8;
    let database = Arc::new(Database::open_in_memory().unwrap());
    run(
        &mut database.connect().unwrap(),
        "CREATE TABLE winners (k INTEGER PRIMARY KEY, thread INTEGER)",
    );
    let barrier = Arc::new(Barrier::new(THREADS));
    let wins = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let database = database.clone();
            let barrier = barrier.clone();
            let wins = wins.clone();
            thread::spawn(move || {
                let mut connection = database.connect().unwrap();
                barrier.wait();
                for key in 0..KEYS {
                    run(&mut connection, "BEGIN");
                    let insert = format!("INSERT INTO winners VALUES ({key}, {thread})");
                    if connection.query(&insert).is_ok() && connection.query("COMMIT").is_ok() {
                        wins.fetch_add(1, Ordering::SeqCst);
                    } else {
                        let _ = connection.query("ROLLBACK");
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut connection = database.connect().unwrap();
    assert_eq!(wins.load(Ordering::SeqCst), KEYS);
    assert_eq!(single(&mut connection, "SELECT count(*) FROM winners"), KEYS as i64);
    assert_eq!(
        single(&mut connection, "SELECT count(DISTINCT k) FROM winners"),
        KEYS as i64
    );
}

/// Appends don't conflict, whatever else goes on, so none of them can get lost.
#[test]
fn concurrent_appends_all_arrive() {
    const THREADS: usize = 8;
    const ROWS: usize = 300;
    let database = Arc::new(Database::open_in_memory().unwrap());
    run(
        &mut database.connect().unwrap(),
        "CREATE TABLE log (thread INTEGER, n INTEGER)",
    );
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let database = database.clone();
            thread::spawn(move || {
                let mut connection = database.connect().unwrap();
                for n in 0..ROWS {
                    run(&mut connection, "BEGIN");
                    run(&mut connection, &format!("INSERT INTO log VALUES ({thread}, {n})"));
                    if n % 5 == 0 {
                        run(&mut connection, "ROLLBACK");
                        run(&mut connection, &format!("INSERT INTO log VALUES ({thread}, {n})"));
                    } else {
                        run(&mut connection, "COMMIT");
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut connection = database.connect().unwrap();
    assert_eq!(
        single(&mut connection, "SELECT count(*) FROM log"),
        (THREADS * ROWS) as i64
    );
    assert_eq!(
        single(&mut connection, "SELECT count(DISTINCT thread * 1000 + n) FROM log"),
        (THREADS * ROWS) as i64
    );
}

/// An INSERT that fails takes back its own rows, not the ones that others added in the meantime.
#[test]
fn a_failed_insert_leaves_the_rows_of_others() {
    const THREADS: usize = 8;
    const KEYS: usize = 1000;
    let database = Arc::new(Database::open_in_memory().unwrap());
    run(
        &mut database.connect().unwrap(),
        "CREATE TABLE t (i INTEGER PRIMARY KEY)",
    );
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let database = database.clone();
            thread::spawn(move || {
                let mut connection = database.connect().unwrap();
                for key in 0..KEYS {
                    // all but one of the threads fail
                    let _ = connection.query(&format!("INSERT INTO t VALUES ({key})"));
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut connection = database.connect().unwrap();
    assert_eq!(single(&mut connection, "SELECT count(*) FROM t"), KEYS as i64);
    assert_eq!(single(&mut connection, "SELECT count(DISTINCT i) FROM t"), KEYS as i64);
}

/// A commit only brings what its transaction did to the indexes of a table.
#[test]
fn a_commit_leaves_the_indexes_of_others() {
    let database = Database::open_in_memory().unwrap();
    let mut first = database.connect().unwrap();
    let mut second = database.connect().unwrap();
    run(&mut first, "CREATE TABLE t (i INTEGER, j INTEGER)");
    run(&mut first, "CREATE INDEX gone ON t (i)");
    run(&mut second, "BEGIN");
    run(&mut second, "CREATE INDEX added ON t (j)");
    run(&mut first, "DROP INDEX gone");
    run(&mut first, "CREATE INDEX other ON t (j)");
    run(&mut second, "COMMIT");
    let count = |connection: &mut Connection, name: &str| {
        single(
            connection,
            &format!("SELECT count(*) FROM duckdb_indexes() WHERE index_name = '{name}'"),
        )
    };
    assert_eq!(count(&mut first, "gone"), 0);
    assert_eq!(count(&mut first, "added"), 1);
    assert_eq!(count(&mut first, "other"), 1);
}

/// Rows that a transaction adds and deletes don't stop a checkpoint, and survive one.
#[test]
fn a_checkpoint_goes_past_inserts_and_deletes() {
    let file = std::env::temp_dir().join(format!("pintail_past_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let database = Database::open(&file, false).unwrap();
    let mut first = database.connect().unwrap();
    let mut second = database.connect().unwrap();
    run(&mut first, "CREATE TABLE t AS SELECT i FROM range(5000) r(i)");
    run(&mut first, "CREATE TABLE u (i INTEGER)");
    run(&mut second, "BEGIN");
    run(&mut second, "DELETE FROM t WHERE i < 100");
    run(&mut second, "INSERT INTO t VALUES (-1)");
    run(&mut first, "INSERT INTO t VALUES (-2)");
    run(&mut first, "CHECKPOINT");
    run(&mut second, "COMMIT");
    assert_eq!(single(&mut first, "SELECT count(*) FROM t"), 4902);
    assert_eq!(single(&mut first, "SELECT count(*) FROM t WHERE i < 0"), 2);
    run(&mut second, "BEGIN");
    run(&mut second, "UPDATE t SET i = i + 1 WHERE i = 200");
    assert!(first.query("CHECKPOINT").is_err());
    run(&mut second, "COMMIT");
    run(&mut first, "CHECKPOINT");
    drop((first, second, database));
    let database = Database::open(&file, false).unwrap();
    let mut connection = database.connect().unwrap();
    assert_eq!(single(&mut connection, "SELECT count(*) FROM t"), 4902);
    assert_eq!(single(&mut connection, "SELECT count(*) FROM t WHERE i = 201"), 2);
    drop((connection, database));
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(format!("{}.wal", file.display()));
}
