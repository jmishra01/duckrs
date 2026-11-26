//! The oracle: the generator's tables, rendered as `.tbl` files, must equal byte for byte the files
//! of the C++ dbgen that `tools/tpch/build_dbgen.sh` builds from a DuckDB checkout.
//!
//! It needs a C++ compiler and the checkout, so it is ignored by default:
//!
//!     DUCKDB_SOURCE=/path/to/duckdb cargo test --release -p duckrs-tpch --test oracle -- --ignored
//!
//! Without `DUCKDB_SOURCE` it looks for a `duckdb` directory next to one of the crate's ancestors
//! (the layout of the duckdbrust checkout).

mod common;

use duckrs_tpch::{DBGenGenerator, TpchTable};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn duckdb_source() -> PathBuf {
    if let Some(dir) = std::env::var_os("DUCKDB_SOURCE") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|dir| dir.join("duckdb"))
        .find(|dir| dir.join("extension/tpch/dbgen/dbgen.cpp").exists())
        .expect("no DuckDB checkout found: set DUCKDB_SOURCE")
}

fn build_cpp_dbgen(out_dir: &Path) -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/tpch/build_dbgen.sh");
    let binary = out_dir.join("cpp_dbgen");
    let status = Command::new("sh")
        .arg(&script)
        .arg(duckdb_source())
        .arg(&binary)
        .status()
        .expect("running build_dbgen.sh");
    assert!(status.success(), "build_dbgen.sh failed");
    binary
}

fn rust_tables(sf: f64) -> HashMap<TpchTable, Vec<u8>> {
    let mut files: HashMap<TpchTable, Vec<u8>> = TpchTable::ALL.iter().map(|&t| (t, Vec::new())).collect();
    for (table, chunk) in DBGenGenerator::new(sf, 1, None).unwrap() {
        assert!(chunk.len() <= duckrs_common::STANDARD_VECTOR_SIZE);
        common::tbl::write_chunk(files.get_mut(&table).unwrap(), &chunk);
    }
    files
}

/// The first line where `actual` and `expected` differ, for the failure message.
fn first_difference(actual: &[u8], expected: &[u8]) -> String {
    let actual_lines = actual.split(|&b| b == b'\n');
    let mut expected_lines = expected.split(|&b| b == b'\n');
    for (i, line) in actual_lines.enumerate() {
        let other = expected_lines.next().unwrap_or_default();
        if line != other {
            return format!(
                "line {}:\n  rust: {}\n  c++:  {}",
                i + 1,
                String::from_utf8_lossy(line),
                String::from_utf8_lossy(other)
            );
        }
    }
    "the C++ file has more lines".to_string()
}

fn compare(binary: &Path, out_dir: &Path, sf: f64) {
    let dir = out_dir.join(format!("sf{sf}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let start = Instant::now();
    let status = Command::new(binary)
        .arg(sf.to_string())
        .arg(&dir)
        .status()
        .expect("running the C++ dbgen");
    assert!(status.success(), "the C++ dbgen failed");
    let cpp_time = start.elapsed();
    let start = Instant::now();
    let files = rust_tables(sf);
    let rust_time = start.elapsed();
    eprintln!(
        "sf {sf}: C++ dbgen (with writing .tbl files) {:.3}s, Rust generator (with rendering) {:.3}s",
        cpp_time.as_secs_f64(),
        rust_time.as_secs_f64()
    );
    let mut failures = Vec::new();
    for table in TpchTable::ALL {
        let expected = std::fs::read(dir.join(format!("{}.tbl", table.name()))).unwrap();
        let actual = &files[&table];
        if *actual == expected {
            eprintln!("sf {sf}: {table}: {} bytes identical", actual.len());
        } else {
            failures.push(format!(
                "sf {sf}: {table} differs at {}",
                first_difference(actual, &expected)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "needs a C++ compiler and a DuckDB checkout (DUCKDB_SOURCE)"]
fn identical_to_cpp_dbgen() {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tpch_oracle");
    std::fs::create_dir_all(&out_dir).unwrap();
    let binary = build_cpp_dbgen(&out_dir);
    compare(&binary, &out_dir, 0.01);
    compare(&binary, &out_dir, 0.1);
}
