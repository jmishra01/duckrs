//! Reads files written by the real DuckDB and compares every row with what DuckDB read from them.
//! `tests/fixtures/generate.sh` makes both.

mod common;

use pintail::{Connection, Database, Error, ErrorKind};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A copy of a file and its log, for what opens it to write: a checkpoint would change the fixture.
fn writable_copy(file: &str) -> PathBuf {
    let directory = common::directory("pintail-read-");
    let path = directory.join(file);
    std::fs::copy(fixture(file), &path).unwrap();
    let log = format!("{file}.wal");
    if fixture(&log).exists() {
        std::fs::copy(fixture(&log), directory.join(log)).unwrap();
    }
    path
}

fn connect(file: &str) -> Connection {
    Database::open(fixture(file), true).unwrap().connect().unwrap()
}

/// `SELECT *` as text, with DuckDB's `-list` format.
fn rows(connection: &mut Connection, table: &str) -> Vec<String> {
    let columns = connection.query(&format!("SELECT * FROM {table} LIMIT 0")).unwrap();
    let casts: Vec<String> = columns.names.iter().map(|c| format!("\"{c}\"::VARCHAR")).collect();
    let result = connection
        .query(&format!("SELECT {} FROM {table}", casts.join(", ")))
        .unwrap();
    (0..result.row_count())
        .map(|row| {
            let fields: Vec<String> = result
                .collection
                .row(row)
                .iter()
                .map(|value| {
                    if value.is_null() {
                        "NULL".into()
                    } else {
                        value.try_to_string().unwrap()
                    }
                })
                .collect();
            fields.join("|")
        })
        .collect()
}

fn compare(file: &str, tables: &[&str]) {
    let mut connection = connect(file);
    for table in tables {
        let name = table.replace('.', "_");
        let bytes = std::fs::read(fixture(&format!("{name}.expected"))).unwrap();
        let expected: Vec<&str> = std::str::from_utf8(&bytes).unwrap().lines().collect();
        let actual = rows(&mut connection, table);
        assert_eq!(actual.len(), expected.len(), "rows of {table}");
        for (row, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(actual, expected, "row {row} of {table}");
        }
    }
}

#[test]
fn uncompressed_types_schemas_and_overflow_strings() {
    compare(
        "compatible.db",
        &["other.t", "uncompressed", "long_strings", "empty_table"],
    );
}

#[test]
fn rle_dictionary_fsst_and_constant() {
    compare(
        "compressed.db",
        &["rle", "dictionary", "fsst", "fsst_empty", "constant"],
    );
}

#[test]
fn bitpacking() {
    compare("bitpacked.db", &["bitpacking", "groups"]);
}

#[test]
fn floats() {
    compare("floats.db", &["alp", "alprd"]);
}

#[test]
fn roaring_and_dict_fsst_from_newer_storage_versions() {
    compare("modern.db", &["validity", "dict_fsst"]);
}

#[test]
fn nested_columns() {
    compare("nested.db", &["lists", "empty_lists", "structs", "arrays", "maps"]);
}

#[test]
fn deleted_rows_stay_deleted() {
    compare("deletes.db", &["deletes"]);
}

#[test]
fn not_null_comes_along() {
    let db = Database::open(fixture("compatible.db"), true).unwrap();
    let mut connection = db.connect().unwrap();
    let result = connection.query("SELECT \"null\" FROM (DESCRIBE other.t)").unwrap();
    assert_eq!(result.collection.row(0)[0].try_to_string().unwrap(), "NO");
    assert_eq!(result.collection.row(1)[0].try_to_string().unwrap(), "YES");
}

const QUERIES: &str = include_str!("fixtures/stored_queries.txt");

/// Views run through the binder, so the query that a file stores has to come out as it went in.
#[test]
fn views_answer_as_they_do_in_duckdb() {
    let mut failures = Vec::new();
    for file in ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"] {
        let mut connection = connect(file);
        for (n, query) in QUERIES.lines().enumerate() {
            let view = format!("q{}", n + 1);
            let bytes = std::fs::read(fixture(&format!("{view}.expected"))).unwrap();
            let expected: Vec<&str> = std::str::from_utf8(&bytes).unwrap().lines().collect();
            if let Err(error) = connection.query(&format!("SELECT * FROM {view} LIMIT 0")) {
                failures.push(format!("{file} {view}: {error}\n  {query}"));
                continue;
            }
            let actual = rows(&mut connection, &view);
            if actual != expected {
                failures.push(format!("{file} {view}: {actual:?} != {expected:?}\n  {query}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn column(connection: &mut Connection, sql: &str) -> Vec<String> {
    let result = connection.query(sql).unwrap();
    (0..result.row_count())
        .map(|row| {
            let values = result.collection.row(row);
            let fields: Vec<String> = values
                .iter()
                .map(|v| {
                    if v.is_null() {
                        "NULL".into()
                    } else {
                        v.try_to_string().unwrap()
                    }
                })
                .collect();
            fields.join("|")
        })
        .collect()
}

/// What `duckdb_views()` says is the query rendered from what the file holds. DuckDB renders that
/// from its own reading of the file, so the two agree only if the trees do. Files for v1.0.0 don't
/// say all of what this compares, such as which joins are explicit or the syntax of a lambda.
#[test]
fn stored_queries_render_as_in_duckdb() {
    // the CLI that writes the fixtures is older than the baseline, whose `ToString` differs here
    const RENDERING: [(&str, &str); 10] = [
        ("\"DATE\"", "DATE"),
        ("\"mood\"", "mood"),
        (")SELECT", ") SELECT"),
        ("UNION  ALL", "UNION ALL"),
        (" -(i) AS e", " (-(i)) AS e"),
        (
            "FROM base AS a INNER JOIN base AS b ON ((a.i = (b.i + 1)))",
            "FROM (base AS a INNER JOIN base AS b ON ((a.i = (b.i + 1))))",
        ),
        (
            "FROM base AS a LEFT JOIN base AS b USING (i)",
            "FROM (base AS a LEFT JOIN base AS b USING (i))",
        ),
        (
            "FROM base AS a CROSS JOIN base AS b",
            "FROM (base AS a CROSS JOIN base AS b)",
        ),
        (
            "FROM base AS a NATURAL INNER JOIN other",
            "FROM (base AS a NATURAL INNER JOIN other)",
        ),
        // the literal of a DOUBLE constant reads back as a DECIMAL (`LiteralOrCast` of the baseline)
        (" 1000.0 AS e", " CAST(1000.0 AS DOUBLE) AS e"),
    ];
    let bytes = std::fs::read_to_string(fixture("views_sql.expected")).unwrap();
    let expected: Vec<String> = bytes
        .lines()
        .map(|line| {
            RENDERING
                .iter()
                .fold(line.to_string(), |l, (from, to)| l.replace(from, to))
        })
        .collect();
    let actual = column(
        &mut connect("parsed_v150.db"),
        "SELECT sql FROM duckdb_views() WHERE NOT internal ORDER BY view_name",
    );
    let differences: Vec<String> = actual
        .iter()
        .zip(&expected)
        .filter(|(a, e)| a != e)
        .map(|(a, e)| format!("pintail: {a}\nDuckDB: {e}"))
        .collect();
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    assert_eq!(actual.len(), expected.len());
}

#[test]
fn defaults_and_generated_columns_come_along() {
    let bytes = std::fs::read_to_string(fixture("with_everything_columns.expected")).unwrap();
    let expected: Vec<&str> = bytes.lines().collect();
    for file in ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"] {
        let actual = column(
            &mut connect(file),
            "SELECT column_name, column_type, \"null\", \"default\" FROM (DESCRIBE with_everything)",
        );
        assert_eq!(actual, expected, "{file}");
    }
}

/// A sequence goes on from where it was.
#[test]
fn sequences_keep_their_state() {
    for file in ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"] {
        let db = Database::open(writable_copy(file), false).unwrap();
        let mut connection = db.connect().unwrap();
        let sql = "SELECT nextval('counter'), nextval('other_schema.spent')";
        assert_eq!(column(&mut connection, sql), ["14|6"], "{file}");
    }
}

#[test]
fn a_check_constraint_comes_along() {
    let mut connection = connect("parsed_v150.db");
    let sql = "SELECT check_constraint_count FROM duckdb_tables() WHERE table_name = 'with_everything'";
    assert_eq!(column(&mut connection, sql), ["1"]);
}

#[test]
fn constraints_count_as_indexes() {
    let expected = ["child|3", "compound|1", "indexed|3", "keyed|2", "parent|3"];
    for file in ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"] {
        let sql = "SELECT table_name, index_count FROM duckdb_tables()
                   WHERE index_count > 0 ORDER BY table_name";
        assert_eq!(column(&mut connect(file), sql), expected, "{file}");
    }
}

const SCAN_QUERIES: &str = include_str!("fixtures/scan_queries.txt");

/// Scans read these tables in place, with what the file says about them: the row groups that a filter
/// skips, the statistics that decide comparisons, and rows that don't line up with chunks.
#[test]
fn scans_of_tables_in_place_answer_as_in_duckdb() {
    let mut failures = Vec::new();
    for (file, size) in [("scan_v122880.db", 122880), ("scan_v4096.db", 4096)] {
        let mut connection = connect(file);
        for (n, query) in SCAN_QUERIES.lines().enumerate() {
            let expected = expected_rows(&format!("scan_{size}_{}", n + 1));
            let actual = match connection.query(query) {
                Ok(_) => column(&mut connection, query),
                Err(error) => {
                    failures.push(format!("{file}: {query}: {error}"));
                    continue;
                }
            };
            if actual != expected {
                failures.push(format!(
                    "{file}: {query}\n  pintail: {actual:?}\n  DuckDB: {expected:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const LOGGED_FILES: [&str; 3] = ["wal_v100.db", "wal_v150.db", "wal_v200.db"];

fn expected_rows(name: &str) -> Vec<String> {
    let bytes = std::fs::read_to_string(fixture(&format!("{name}.expected"))).unwrap();
    bytes.lines().map(String::from).collect()
}

/// The tables as DuckDB has them after it replays the same log: inserts, deletes and updates, large
/// inserts that went to the file ahead of the commit, and every column type.
#[test]
fn write_ahead_logs_are_replayed() {
    for file in LOGGED_FILES {
        let mut connection = connect(file);
        for (table, name) in [
            ("kept", "wal_kept"),
            ("logged_schema.fresh", "wal_logged_schema_fresh"),
            ("constants", "wal_constants"),
            ("repeated", "wal_repeated"),
            ("bulk", "wal_bulk"),
            ("logged_view", "wal_logged_view"),
        ] {
            let expected = expected_rows(name);
            let actual = rows(&mut connection, table);
            assert_eq!(actual.len(), expected.len(), "{file}: rows of {table}");
            let first = actual.iter().zip(&expected).position(|(a, e)| a != e);
            assert_eq!(first, None, "{file}: {table}");
        }
    }
}

#[test]
fn logged_catalog_changes_are_replayed() {
    for file in LOGGED_FILES {
        let mut connection = connect(file);
        for missing in ["doomed", "doomed_view"] {
            assert!(connection.query(&format!("SELECT * FROM {missing}")).is_err(), "{file}");
        }
        assert!(connection.query("SELECT doomed_macro(1)").is_err(), "{file}");
        assert_eq!(column(&mut connection, "SELECT logged_macro(2)"), ["6"], "{file}");
        assert_eq!(
            column(&mut connection, "SELECT * FROM logged_table_macro()"),
            ["1"],
            "{file}"
        );
        assert_eq!(column(&mut connection, "SELECT 'x'::logged_type"), ["x"], "{file}");
        let sql = "SELECT table_name, index_count FROM duckdb_tables() WHERE index_count > 0";
        assert_eq!(column(&mut connection, sql), ["kept|1"], "{file}");
    }
}

/// The sequences go on from what the log says of them, and from what the file says of the others.
#[test]
fn logged_sequence_values_are_replayed() {
    for file in LOGGED_FILES {
        let db = Database::open(writable_copy(file), false).unwrap();
        let mut connection = db.connect().unwrap();
        assert_eq!(column(&mut connection, "SELECT nextval('counter')"), ["16"], "{file}");
        assert!(connection.query("SELECT nextval('doomed_sequence')").is_err(), "{file}");
    }
}

/// A copy of the file and its log, in a directory of its own, whose log `change` changes.
fn with_changed_log(name: &str, change: impl FnOnce(Vec<u8>) -> Vec<u8>) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("pintail-wal-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("wal_v150.db");
    std::fs::copy(fixture("wal_v150.db"), &path).unwrap();
    let log = std::fs::read(fixture("wal_v150.db.wal")).unwrap();
    std::fs::write(directory.join("wal_v150.db.wal"), change(log)).unwrap();
    path
}

/// A crash leaves part of the last transaction: what was committed before it is what counts.
#[test]
fn a_torn_log_keeps_what_was_committed() {
    let path = with_changed_log("torn", |mut log| {
        log.truncate(log.len() - 3);
        log
    });
    let db = Database::open(&path, true).unwrap();
    let mut connection = db.connect().unwrap();
    assert_eq!(column(&mut connection, "SELECT count(*) FROM bulk"), ["299995"]);
    assert_eq!(
        column(&mut connection, "SELECT nextval('counter') FROM range(0)"),
        Vec::<String>::new()
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn a_damaged_log_is_an_error() {
    let path = with_changed_log("damaged", |mut log| {
        let middle = log.len() / 2;
        log[middle] ^= 1;
        log
    });
    let error = Database::open(&path, true).unwrap_err();
    assert_eq!(error.kind, ErrorKind::DataCorruption, "{error}");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn a_log_of_another_database_is_an_error() {
    // logs for versions before v1.3.0 don't say which database they are of
    let path = with_changed_log("other", |_| std::fs::read(fixture("wal_v200.db.wal")).unwrap());
    let error = Database::open(&path, true).unwrap_err();
    assert_eq!(error.to_string(), "IO Error: WAL does not match database file.");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn replaying_leaves_the_log_alone() {
    let before = std::fs::read(fixture("wal_v150.db.wal")).unwrap();
    let _ = connect("wal_v150.db");
    assert_eq!(std::fs::read(fixture("wal_v150.db.wal")).unwrap(), before);
}

#[test]
fn a_read_only_file_takes_no_writes() {
    let mut connection = connect("compatible.db");
    let error = connection.query("INSERT INTO other.t VALUES (3, 'x')").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid Input Error: Cannot execute statement of type \"INSERT\" on database \"compatible\" which is \
         attached in read-only mode!"
    );
}

#[test]
fn writes_to_a_file_take_effect() {
    let db = Database::open(writable_copy("compatible.db"), false).unwrap();
    let mut connection = db.connect().unwrap();
    connection.query("DROP TABLE other.t").unwrap();
    let error: Error = connection.query("SELECT count(*) FROM other.t").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Catalog);
}

#[test]
fn damaged_files_are_errors() {
    let directory = std::env::temp_dir().join(format!("pintail-storage-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("damaged.db");
    let original = std::fs::read(fixture("compressed.db")).unwrap();
    let message = |bytes: &[u8]| {
        std::fs::write(&path, bytes).unwrap();
        Database::open(&path, true).unwrap_err().to_string()
    };

    assert!(message(b"this is not a database file").contains("is not a valid DuckDB database file"));
    for offset in [20, 4096 + 20, 2 * 4096 + 20] {
        let mut bytes = original.clone();
        bytes[offset] ^= 1;
        assert!(
            message(&bytes).contains("does not match stored checksum"),
            "offset {offset}"
        );
    }
    // the current header goes unnoticed if the other one is damaged, but not a block
    let mut bytes = original.clone();
    bytes[3 * 4096 + 100] ^= 1;
    assert!(message(&bytes).contains("does not match stored checksum"));
    assert!(message(&original[..3 * 4096 + 100]).contains("Corrupt database file"));
    std::fs::remove_dir_all(&directory).unwrap();
}
