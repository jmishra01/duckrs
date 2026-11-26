//! Writes the TPC-H tables of a scale factor as pipe-delimited `.tbl` files, like the C++ dbgen
//! that `tools/tpch/build_dbgen.sh` builds:
//!
//!     cargo run --release -p duckrs-tpch --example dbgen_tbl -- <sf> <directory>
//!
//! It prints the time spent generating the chunks and the time spent rendering them.

#[path = "../tests/common/tbl.rs"]
mod tbl;

use duckrs_tpch::{DBGenGenerator, TpchTable};
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, sf, dir] = args.as_slice() else {
        eprintln!("usage: dbgen_tbl <sf> <directory>");
        std::process::exit(2);
    };
    let sf: f64 = sf.parse().expect("a scale factor");
    let start = Instant::now();
    let generator = DBGenGenerator::new(sf, 1, None).expect("a valid scale factor");
    let mut files: HashMap<TpchTable, Vec<u8>> = HashMap::new();
    let mut render = Duration::ZERO;
    let mut rows = 0;
    for (table, chunk) in generator {
        rows += chunk.len();
        let render_start = Instant::now();
        tbl::write_chunk(files.entry(table).or_default(), &chunk);
        render += render_start.elapsed();
    }
    let total = start.elapsed();
    for table in TpchTable::ALL {
        let path = std::path::Path::new(dir).join(format!("{}.tbl", table.name()));
        std::fs::write(path, files.remove(&table).unwrap_or_default()).expect("writing a .tbl file");
    }
    println!(
        "sf {sf}: {rows} rows; generated in {:.3}s, rendered in {:.3}s, {:.3}s with writing",
        (total - render).as_secs_f64(),
        render.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
}
