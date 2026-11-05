//! Reads files written by the real DuckDB and compares every row with what DuckDB read from them.
//! `tests/fixtures/generate.sh` makes both.

use duckrs::{Connection, Database, Error, ErrorKind, LogicalType};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn connect(file: &str) -> Connection {
    Database::open(fixture(file), true).unwrap().connect().unwrap()
}

/// `SELECT *` as text, with DuckDB's `-list` format.
fn rows(connection: &mut Connection, table: &str) -> Vec<String> {
    let columns = connection.query(&format!("SELECT * FROM {table} LIMIT 0")).unwrap();
    let casts: Vec<String> = columns
        .names
        .iter()
        .zip(&columns.types)
        .map(|(c, ty)| {
            let through = if *ty == LogicalType::FLOAT { "::DOUBLE" } else { "" };
            format!("\"{c}\"{through}::VARCHAR")
        })
        .collect();
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
        let types = connection
            .query(&format!("SELECT * FROM {table} LIMIT 0"))
            .unwrap()
            .types;
        for (row, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            let same = actual == expected || same_floats(actual, expected, &types);
            assert!(same, "row {row} of {table}: {actual} and {expected}");
        }
    }
}

/// Whether two rows differ at most in how floats are printed. A value can have two shortest forms,
/// and DuckDB and duckrs pick differently: `8.124038696289062` and `...063` are one double.
fn same_floats(actual: &str, expected: &str, types: &[LogicalType]) -> bool {
    let (actual, expected): (Vec<_>, Vec<_>) = (actual.split('|').collect(), expected.split('|').collect());
    actual.len() == types.len()
        && expected.len() == types.len()
        && (0..types.len()).all(|i| {
            let float = types[i] == LogicalType::FLOAT || types[i] == LogicalType::DOUBLE;
            let bits = |text: &str| text.parse::<f64>().map(f64::to_bits).ok();
            actual[i] == expected[i] || (float && bits(actual[i]).is_some() && bits(actual[i]) == bits(expected[i]))
        })
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
fn writes_to_a_file_fail_instead_of_getting_lost() {
    let db = Database::open(fixture("compatible.db"), false).unwrap();
    let mut connection = db.connect().unwrap();
    let error: Error = connection.query("DROP TABLE other.t").unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotImplemented);
    assert_eq!(connection.query("SELECT count(*) FROM other.t").unwrap().row_count(), 1);
}

#[test]
fn damaged_files_are_errors() {
    let directory = std::env::temp_dir().join(format!("duckrs-storage-{}", std::process::id()));
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
