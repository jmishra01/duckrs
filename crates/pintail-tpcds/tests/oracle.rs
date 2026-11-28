//! Compares the generator with C++'s dsdgen, byte for byte, at scale factor 0.01.
//!
//! It builds a C++ dsdgen from a DuckDB checkout with `tools/tpcds/build_dsdgen.sh`, so it needs a
//! C++ compiler and the checkout, and is ignored by default. Run it with
//!
//! ```text
//! DUCKDB_CHECKOUT=/path/to/duckdb cargo test --release -p pintail-tpcds --test oracle -- --ignored --nocapture
//! ```
//!
//! `DUCKDB_CHECKOUT` defaults to `../duckdb` next to the workspace. `TPCDS_ORACLE_SF` picks
//! another scale factor.

mod common;

use pintail_tpcds::Table;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace exists")
}

fn duckdb_checkout() -> PathBuf {
    std::env::var_os("DUCKDB_CHECKOUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("../duckdb"))
}

/// The first line where `expected` and `actual` differ, with both lines.
fn first_difference(expected: &[u8], actual: &[u8]) -> String {
    let mut expected_lines = expected.split(|&b| b == b'\n');
    let mut actual_lines = actual.split(|&b| b == b'\n');
    for line in 1.. {
        match (expected_lines.next(), actual_lines.next()) {
            (None, None) => break,
            (e, a) if e != a => {
                let show = |l: Option<&[u8]>| l.map_or("<none>".into(), |l| String::from_utf8_lossy(l).into_owned());
                return format!("line {line}:\n  C++:  {}\n  Rust: {}", show(e), show(a));
            }
            _ => {}
        }
    }
    "no difference".into()
}

#[test]
#[ignore = "needs a C++ compiler and a DuckDB checkout"]
fn matches_cpp_dsdgen() {
    let sf: f64 = std::env::var("TPCDS_ORACLE_SF").map_or(0.01, |s| s.parse().expect("a scale factor"));
    let checkout = duckdb_checkout();
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tpcds-oracle");
    let output = work.join(format!("sf{sf}"));
    let _ = std::fs::remove_dir_all(&output);
    std::fs::create_dir_all(&output).expect("create the output directory");
    let binary = work.join("dsdgen");
    let status = Command::new("sh")
        .arg(workspace().join("tools/tpcds/build_dsdgen.sh"))
        .arg(&checkout)
        .arg(&binary)
        .status()
        .expect("run build_dsdgen.sh");
    assert!(
        status.success(),
        "building the C++ dsdgen from {} failed",
        checkout.display()
    );

    let start = Instant::now();
    let status = Command::new(&binary)
        .arg(sf.to_string())
        .arg(&output)
        .status()
        .expect("run the C++ dsdgen");
    assert!(status.success());
    let cpp_time = start.elapsed();

    let start = Instant::now();
    let rows: usize = pintail_tpcds::dsdgen(sf)
        .expect("a valid scale factor")
        .map(|(_, chunk)| chunk.len())
        .sum();
    println!("sf {sf}: Rust dsdgen generates {rows} rows in {:?}", start.elapsed());

    let start = Instant::now();
    let rust = common::render_all(sf);
    let rust_time = start.elapsed();
    println!("sf {sf}: C++ dsdgen {cpp_time:?}, Rust dsdgen (with rendering) {rust_time:?}");

    let mut mismatches = Vec::new();
    for table in Table::ALL {
        let expected = std::fs::read(output.join(format!("{}.tbl", table.name()))).expect("read a C++ table");
        let actual = &rust[table as usize];
        let rows = expected.iter().filter(|&&b| b == b'\n').count();
        if expected == *actual {
            println!("{:<24} {rows:>8} rows  match", table.name());
        } else {
            println!(
                "{:<24} {rows:>8} rows  DIFFER at {}",
                table.name(),
                first_difference(&expected, actual)
            );
            mismatches.push(table.name());
        }
    }
    assert!(mismatches.is_empty(), "tables that differ from C++: {mismatches:?}");
}
