//! Encrypted database files (`ENCRYPTION_KEY`): what goes into one comes back through its log and its checkpoints, under
//! both ciphers, with the errors of C++ for a wrong or missing key, a key for a file that isn't encrypted, a cipher
//! other than the file's, and writing without `force_mbedtls_unsafe`. Files DuckDB 1.4 encrypted open too.

use pintail::{Connection, Database};
use std::path::{Path, PathBuf};

fn rows(connection: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let result = connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..result.row_count())
        .map(|row| {
            (0..result.column_count())
                .map(|column| result.value(column, row).try_to_string().unwrap())
                .collect()
        })
        .collect()
}

fn error(connection: &mut Connection, sql: &str) -> String {
    match connection.query(sql) {
        Ok(_) => panic!("{sql} succeeded"),
        Err(error) => error.to_string(),
    }
}

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("pintail-encryption-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn connection() -> Connection {
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    c.query("SET force_mbedtls_unsafe = true").unwrap();
    c
}

#[test]
fn rows_come_back_through_the_log_and_checkpoints_under_both_ciphers() {
    let directory = directory("round_trip");
    for cipher in ["GCM", "CTR"] {
        let file = directory.join(format!("{cipher}.db"));
        let attach = format!(
            "ATTACH '{}' AS e (ENCRYPTION_KEY 'secret key', ENCRYPTION_CIPHER '{cipher}')",
            file.display()
        );
        let mut c = connection();
        c.query(&attach).unwrap();
        c.query("CREATE TABLE e.t AS SELECT range AS i, 'row ' || range AS s FROM range(50000)")
            .unwrap();
        c.query("CHECKPOINT e").unwrap();
        // this one only goes to the log
        c.query("PRAGMA disable_checkpoint_on_shutdown").unwrap();
        c.query("INSERT INTO e.t VALUES (-1, 'a row in the log')").unwrap();
        drop(c);
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !bytes.windows(9).any(|w| w == b"row 49999"),
            "{cipher}: plaintext in the file"
        );
        let log = std::fs::read(format!("{}.wal", file.display())).unwrap();
        assert!(
            !log.windows(12).any(|w| w == b"row in the l"),
            "{cipher}: plaintext in the log"
        );

        let mut c = connection();
        c.query(&attach).unwrap();
        assert_eq!(
            rows(&mut c, "SELECT count(*), sum(i), min(s) FROM e.t"),
            [["50001", "1249974999", "a row in the log"]]
        );
        assert_eq!(
            rows(
                &mut c,
                "SELECT encrypted, cipher FROM duckdb_databases() WHERE database_name = 'e'"
            ),
            [["true", cipher]]
        );
    }
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn keys_and_ciphers_are_checked_as_in_duckdb() {
    let directory = directory("errors");
    let file = directory.join("e.db");
    let plain = directory.join("plain.db");
    let ctr = directory.join("ctr.db");
    let mut c = connection();
    c.query(&format!("ATTACH '{}' AS e (ENCRYPTION_KEY 'right')", file.display()))
        .unwrap();
    c.query("CREATE TABLE e.t AS SELECT 1 AS i").unwrap();
    c.query(&format!("ATTACH '{}' AS p", plain.display())).unwrap();
    c.query(&format!(
        "ATTACH '{}' AS c (ENCRYPTION_KEY 'right', ENCRYPTION_CIPHER 'CTR')",
        ctr.display()
    ))
    .unwrap();
    c.query("DETACH e").unwrap();
    c.query("DETACH p").unwrap();
    c.query("DETACH c").unwrap();

    let attach = |path: &Path, options: &str| format!("ATTACH '{}' AS x ({options})", path.display());
    assert!(
        error(&mut c, &attach(&file, "ENCRYPTION_KEY 'wrong'"))
            .contains("Wrong encryption key used to open the database file")
    );
    assert!(error(&mut c, &attach(&file, "READ_ONLY")).contains("without a key"));
    assert!(error(&mut c, &attach(&plain, "ENCRYPTION_KEY 'right'")).contains("is not encrypted"));
    assert!(
        error(
            &mut c,
            &attach(&file, "ENCRYPTION_KEY 'right', ENCRYPTION_CIPHER 'CTR'")
        )
        .contains("with a different cipher (CTR) than the one used to create it (GCM)")
    );
    assert!(
        error(&mut c, &attach(&ctr, "ENCRYPTION_KEY 'right'"))
            .contains("without explicitly specifying the encryption cipher")
    );
    assert!(
        error(
            &mut c,
            &attach(&file, "ENCRYPTION_KEY 'right', ENCRYPTION_CIPHER 'CBC'")
        )
        .contains("CBC")
    );
    assert!(error(&mut c, &attach(&file, "ENCRYPTION_KEY ''")).contains("A key cannot be empty"));
    assert!(
        error(
            &mut c,
            &attach(
                &directory.join("old.db"),
                "ENCRYPTION_KEY 'k', STORAGE_VERSION 'v1.2.0'"
            )
        )
        .contains("are not compatible")
    );

    // without force_mbedtls_unsafe, encrypted files can only be read
    let db = Database::open_in_memory().unwrap();
    let mut reader = db.connect().unwrap();
    assert!(
        error(&mut reader, &attach(&file, "ENCRYPTION_KEY 'right'"))
            .contains("DuckDB currently has a read-only crypto module loaded")
    );
    reader
        .query(&attach(&file, "ENCRYPTION_KEY 'right', READ_ONLY"))
        .unwrap();
    assert_eq!(rows(&mut reader, "SELECT i FROM x.t"), [["1"]]);
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn files_duckdb_1_4_encrypted_open_and_take_writes() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../duckdb/data/attach_test");
    let directory = directory("duckdb_files");
    for (name, cipher) in [("gcm", "GCM"), ("ctr", "CTR")] {
        let source = data.join(format!("encrypted_{name}_key=abcde.db"));
        if !source.exists() {
            // (the files are in the DuckDB checkout next to this one)
            continue;
        }
        let file = directory.join(format!("{name}.db"));
        std::fs::copy(&source, &file).unwrap();
        let attach = format!(
            "ATTACH '{}' AS enc (ENCRYPTION_KEY 'abcde', ENCRYPTION_CIPHER '{cipher}')",
            file.display()
        );
        let mut c = connection();
        c.query(&attach).unwrap();
        let count = |c: &mut Connection| rows(c, "SELECT count(*) FROM enc.test")[0][0].parse::<u64>().unwrap();
        let before = count(&mut c);
        c.query("INSERT INTO enc.test VALUES (5)").unwrap();
        c.query("CHECKPOINT enc").unwrap();
        c.query("DETACH enc").unwrap();
        c.query(&attach).unwrap();
        assert_eq!(count(&mut c), before + 1, "{name}");
        assert_eq!(
            rows(
                &mut c,
                "SELECT tags['storage_version'] FROM duckdb_databases() WHERE database_name = 'enc'"
            ),
            [["v1.4.0+"]]
        );
    }
    std::fs::remove_dir_all(&directory).unwrap();
}
