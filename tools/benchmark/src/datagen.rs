//! TPC-H and TPC-DS data for the engines that have no `tpch` / `tpcds` extension: `CALL dbgen(sf=N)`
//! and `CALL dsdgen(sf=N)` in a load query are carried out with the generators of the DuckDB checkout
//! (built by `tools/tpch/build_dbgen.sh` and `tools/tpcds/build_dsdgen.sh`), whose `.tbl` files are
//! then copied into tables of the same columns as the extensions make.

use regex::Regex;
use sqllogictest::engine::Connection;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The tables of `CALL dbgen` (`extension/tpch/dbgen/dbgen.cpp`), with their columns.
const TPCH_TABLES: &[(&str, &str)] = &[
    (
        "part",
        "p_partkey BIGINT, p_name VARCHAR, p_mfgr VARCHAR, p_brand VARCHAR, p_type VARCHAR, p_size INTEGER, \
         p_container VARCHAR, p_retailprice DECIMAL(15,2), p_comment VARCHAR",
    ),
    (
        "supplier",
        "s_suppkey BIGINT, s_name VARCHAR, s_address VARCHAR, s_nationkey INTEGER, s_phone VARCHAR, \
         s_acctbal DECIMAL(15,2), s_comment VARCHAR",
    ),
    (
        "partsupp",
        "ps_partkey BIGINT, ps_suppkey BIGINT, ps_availqty BIGINT, ps_supplycost DECIMAL(15,2), ps_comment VARCHAR",
    ),
    (
        "customer",
        "c_custkey BIGINT, c_name VARCHAR, c_address VARCHAR, c_nationkey INTEGER, c_phone VARCHAR, \
         c_acctbal DECIMAL(15,2), c_mktsegment VARCHAR, c_comment VARCHAR",
    ),
    (
        "orders",
        "o_orderkey BIGINT, o_custkey BIGINT, o_orderstatus VARCHAR, o_totalprice DECIMAL(15,2), o_orderdate DATE, \
         o_orderpriority VARCHAR, o_clerk VARCHAR, o_shippriority INTEGER, o_comment VARCHAR",
    ),
    (
        "lineitem",
        "l_orderkey BIGINT, l_partkey BIGINT, l_suppkey BIGINT, l_linenumber BIGINT, l_quantity DECIMAL(15,2), \
         l_extendedprice DECIMAL(15,2), l_discount DECIMAL(15,2), l_tax DECIMAL(15,2), l_returnflag VARCHAR, \
         l_linestatus VARCHAR, l_shipdate DATE, l_commitdate DATE, l_receiptdate DATE, l_shipinstruct VARCHAR, \
         l_shipmode VARCHAR, l_comment VARCHAR",
    ),
    (
        "nation",
        "n_nationkey INTEGER, n_name VARCHAR, n_regionkey INTEGER, n_comment VARCHAR",
    ),
    ("region", "r_regionkey INTEGER, r_name VARCHAR, r_comment VARCHAR"),
];

/// Which generator a load statement calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    Tpch,
    Tpcds,
}

impl Generator {
    fn name(self) -> &'static str {
        match self {
            Generator::Tpch => "dbgen",
            Generator::Tpcds => "dsdgen",
        }
    }
}

/// The `CALL dbgen(sf=N)` or `CALL dsdgen(sf=N)` statements of a load query, and the rest of it.
pub fn find_generator_calls(load: &str) -> (Vec<(Generator, String)>, String) {
    let pattern = Regex::new(r"(?i)\bCALL\s+(dbgen|dsdgen)\s*\(\s*sf\s*=\s*([0-9.]+)\s*[^)]*\)\s*;?").expect("a regex");
    let mut calls = Vec::new();
    for capture in pattern.captures_iter(load) {
        let generator = if capture[1].eq_ignore_ascii_case("dbgen") {
            Generator::Tpch
        } else {
            Generator::Tpcds
        };
        calls.push((generator, capture[2].to_string()));
    }
    (calls, pattern.replace_all(load, "").into_owned())
}

/// Builds the generator if it is not there yet, and makes the `.tbl` files of scale factor `sf`
/// (in `data/<tpch|tpcds>_sf<sf>/`), unless they are there already.
fn generate(generator: Generator, sf: &str, data: &Path) -> Result<PathBuf, String> {
    let repo = std::env::var("PINTAIL_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    );
    let tool = data.join(format!("bin/{}", generator.name()));
    if !tool.exists() {
        std::fs::create_dir_all(data.join("bin")).map_err(|e| e.to_string())?;
        let script = match generator {
            Generator::Tpch => repo.join("tools/tpch/build_dbgen.sh"),
            Generator::Tpcds => repo.join("tools/tpcds/build_dsdgen.sh"),
        };
        let root = std::env::current_dir().map_err(|e| e.to_string())?;
        eprintln!("building {} with {}", generator.name(), script.display());
        let status = Command::new("sh")
            .arg(&script)
            .arg(&root)
            .arg(&tool)
            .status()
            .map_err(|e| format!("could not run {}: {e}", script.display()))?;
        if !status.success() {
            return Err(format!("{} failed", script.display()));
        }
    }
    let prefix = match generator {
        Generator::Tpch => "tpch",
        Generator::Tpcds => "tpcds",
    };
    let directory = data.join(format!("{prefix}_sf{sf}"));
    let done = directory.join(".complete");
    if !done.exists() {
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        eprintln!(
            "generating {prefix} data at scale factor {sf} in {}",
            directory.display()
        );
        let status = Command::new(&tool)
            .arg(sf)
            .arg(&directory)
            .status()
            .map_err(|e| format!("could not run {}: {e}", tool.display()))?;
        if !status.success() {
            return Err(format!("{} {sf} failed", tool.display()));
        }
        std::fs::write(&done, "").map_err(|e| e.to_string())?;
    }
    Ok(directory)
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// Does what the generator call would: the tables, filled with the generated rows.
pub fn load(connection: &mut dyn Connection, generator: Generator, sf: &str, data: &Path) -> Result<(), String> {
    let directory = generate(generator, sf, data)?;
    let copy_options = "(DELIMITER '|', HEADER false, QUOTE '', ESCAPE '', NULL '')";
    match generator {
        Generator::Tpch => {
            for (table, columns) in TPCH_TABLES {
                connection.query(&format!("CREATE TABLE {table} ({columns})"))?;
                let file = directory.join(format!("{table}.tbl"));
                connection.query(&format!("COPY {table} FROM {} {copy_options}", quoted(&file)))?;
            }
        }
        Generator::Tpcds => {
            let schemas = std::path::Path::new("extension/tpcds/dsdgen/schema");
            let mut files: Vec<PathBuf> = std::fs::read_dir(schemas)
                .map_err(|e| format!("{}: {e}", schemas.display()))?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "sql"))
                .collect();
            files.sort();
            for schema in files {
                let table = schema.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let sql = std::fs::read_to_string(&schema).map_err(|e| e.to_string())?;
                connection.query(&sql)?;
                let file = directory.join(format!("{table}.tbl"));
                connection.query(&format!("COPY {table} FROM {} {copy_options}", quoted(&file)))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_calls_are_found_and_taken_out() {
        let (calls, rest) = find_generator_calls("CALL dbgen(sf=1); CREATE TABLE x AS SELECT 1;");
        assert_eq!(calls, vec![(Generator::Tpch, "1".to_string())]);
        assert_eq!(rest.trim(), "CREATE TABLE x AS SELECT 1;");
        let (calls, _) = find_generator_calls("call dsdgen(sf=0.1)");
        assert_eq!(calls, vec![(Generator::Tpcds, "0.1".to_string())]);
    }
}
