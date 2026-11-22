//! Runs the 22 TPC-H queries and compares them with DuckDB's answers.
//!
//! ```text
//! cargo run --release -p duckrs --example tpch -- <dir with .tbl files> <extension/tpch/dbgen> <sf> [query numbers]
//! ```
//!
//! The `.tbl` files are pipe-delimited rows, as dbgen writes them, which go in through INSERT. With
//! `TPCH_DATABASE` set to a database file that has the tables, it reads them there instead. With
//! `TPCH_EXPLAIN` set it prints the plan of each query first.

use duckrs::{Connection, Database, Result};
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Instant;

const TABLES: &[(&str, &str)] = &[
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

const ROWS_PER_INSERT: usize = 2048;

fn load(connection: &mut Connection, directory: &Path) -> Result<()> {
    for (table, columns) in TABLES {
        let started = Instant::now();
        connection.query(&format!("CREATE TABLE {table} ({columns})"))?;
        let path = directory.join(format!("{table}.tbl"));
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        for rows in lines.chunks(ROWS_PER_INSERT) {
            let mut sql = format!("INSERT INTO {table} VALUES ");
            for (at, row) in rows.iter().enumerate() {
                sql.push_str(if at == 0 { "('" } else { ",('" });
                sql.push_str(&row.trim_end_matches('|').replace('\'', "''").replace('|', "','"));
                sql.push_str("')");
            }
            connection.query(&sql)?;
        }
        println!("{table}: {} rows in {:.1?}", lines.len(), started.elapsed());
    }
    Ok(())
}

fn same_cell(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    match (actual.parse::<f64>(), expected.parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() <= 1e-9 * b.abs().max(1.0),
        _ => false,
    }
}

fn same_row(a: &[String], e: &[String]) -> bool {
    a.len() == e.len() && a.iter().zip(e).all(|(a, e)| same_cell(a, e))
}

/// Row for row, except that rows tied in the order may come in another order among themselves: each stretch where
/// the rows differ has to hold the same rows on both sides. (Q11 orders by value alone, and SF10 has ties there.)
fn same_rows(actual: &[Vec<String>], expected: &[Vec<String>]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let mut pending: Vec<&[String]> = Vec::new();
    for (a, e) in actual.iter().zip(expected) {
        if pending.is_empty() && same_row(a, e) {
            continue;
        }
        for row in [a.as_slice(), e.as_slice()] {
            match pending.iter().position(|p| same_row(p, row)) {
                Some(at) => {
                    pending.swap_remove(at);
                }
                None => pending.push(row),
            }
        }
    }
    pending.is_empty()
}

fn answer(path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines = text.lines().skip(1);
    // the SF10 answers quote some strings (`"Brand#44"`)
    let unquote = |cell: &str| {
        let inner = cell.strip_prefix('"').and_then(|c| c.strip_suffix('"'));
        inner.unwrap_or(cell).to_string()
    };
    lines.map(|l| l.split('|').map(unquote).collect()).collect()
}

fn run(connection: &mut Connection, sql: &str) -> Result<Vec<Vec<String>>> {
    let result = connection.query(sql)?;
    let (rows, columns) = (result.collection.len(), result.names.len());
    let text = |row, column| {
        let value = result.collection.get_value(column, row);
        if value.is_null() {
            String::new()
        } else {
            value.try_to_string().unwrap_or_default()
        }
    };
    Ok((0..rows)
        .map(|row| (0..columns).map(|c| text(row, c)).collect())
        .collect())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, tables, dbgen, sf, wanted @ ..] = args.as_slice() else {
        panic!("usage: tpch <dir with .tbl files> <extension/tpch/dbgen> <sf> [query numbers]");
    };
    let dbgen = Path::new(dbgen);
    // `TPCH_DATABASE`: the tables are in that file already, which is read as it is
    let database = match std::env::var_os("TPCH_DATABASE") {
        Some(file) => Database::open(Path::new(&file), true)?,
        None => Database::open_in_memory()?,
    };
    let mut connection = database.connect()?;
    if std::env::var_os("TPCH_DATABASE").is_none() {
        load(&mut connection, Path::new(tables))?;
    }
    let queries: Vec<usize> = if wanted.is_empty() {
        (1..=22).collect()
    } else {
        wanted.iter().map(|q| q.parse().expect("a query number")).collect()
    };
    let scale: f64 = sf.parse().expect("a scale factor");
    let (mut correct, mut total_time) = (0, 0.0);
    for q in &queries {
        let mut sql = fs::read_to_string(dbgen.join(format!("queries/q{q:02}.sql"))).expect("the query");
        if std::env::var_os("TPCH_EXPLAIN").is_some() {
            connection.query("SET explain_output = 'ALL'")?;
            let plan = run(&mut connection, &format!("EXPLAIN {sql}"))?;
            println!("{}", plan.last().map(|row| row[1].as_str()).unwrap_or(""));
        }
        // Q11's FRACTION is 0.0001 / SF, and the stored query has it for SF 1
        sql = sql.replace("0.0001000000", &format!("{:.10}", 0.0001 / scale));
        let expected = answer(&dbgen.join(format!("answers/sf{sf}/q{q:02}.csv")));
        print!("q{q:02}: ");
        std::io::stdout().flush().ok();
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&mut connection, &sql)));
        let cold = started.elapsed().as_secs_f64();
        // the first run also pays for the statistics that a write threw away
        let started = Instant::now();
        let warm = catch_unwind(AssertUnwindSafe(|| run(&mut connection, &sql))).is_ok();
        let time = if warm { started.elapsed().as_secs_f64() } else { cold };
        total_time += time;
        print!("cold {cold:8.3}s  ");
        match outcome {
            Ok(Ok(rows)) if same_rows(&rows, &expected) => {
                correct += 1;
                println!("ok    {time:8.3}s  {} rows", rows.len());
            }
            Ok(Ok(rows)) => {
                let first = rows
                    .iter()
                    .zip(&expected)
                    .find(|(a, e)| !same_rows(&[(*a).clone()], &[(*e).clone()]));
                println!(
                    "WRONG {time:8.3}s  {} rows, expected {}: {first:?}",
                    rows.len(),
                    expected.len()
                );
            }
            Ok(Err(error)) => println!("ERROR {time:8.3}s  {error}"),
            Err(_) => println!("PANIC {time:8.3}s"),
        }
    }
    println!("{correct} of {} correct in {total_time:.3}s", queries.len());
    Ok(())
}
