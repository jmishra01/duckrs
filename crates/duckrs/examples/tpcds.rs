//! Runs the 99 TPC-DS queries and compares them with DuckDB's answers.
//!
//! ```text
//! cargo run --release -p duckrs --example tpcds -- <dir with .tbl files> <extension/tpcds/dsdgen> <sf> [query numbers]
//! ```
//!
//! The `.tbl` files come from `tools/tpcds/build_dsdgen.sh`. duckrs has no COPY yet, so they go in
//! through INSERT. With `TPCDS_EXPLAIN` set it prints the plan of each query first.

use duckrs::{Connection, Database, Result};
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Instant;

const ROWS_PER_INSERT: usize = 2048;

fn load(connection: &mut Connection, directory: &Path, dsdgen: &Path) -> Result<()> {
    let mut schemas: Vec<_> = fs::read_dir(dsdgen.join("schema"))
        .expect("the schema directory")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    schemas.sort();
    for schema in schemas {
        let started = Instant::now();
        let table = schema.file_stem().expect("a name").to_string_lossy().into_owned();
        connection.query(&fs::read_to_string(&schema).expect("the schema"))?;
        let path = directory.join(format!("{table}.tbl"));
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        for rows in lines.chunks(ROWS_PER_INSERT) {
            let mut sql = format!("INSERT INTO {table} VALUES ");
            for (at, row) in rows.iter().enumerate() {
                sql.push_str(if at == 0 { "(" } else { ",(" });
                for (column, value) in row.split('|').enumerate() {
                    if column > 0 {
                        sql.push(',');
                    }
                    if value.is_empty() {
                        sql.push_str("NULL");
                    } else {
                        sql.push('\'');
                        sql.push_str(&value.replace('\'', "''"));
                        sql.push('\'');
                    }
                }
                sql.push(')');
            }
            connection.query(&sql)?;
        }
        println!("{table}: {} rows in {:.1?}", lines.len(), started.elapsed());
    }
    Ok(())
}

fn same_cell(actual: &str, expected: &str) -> bool {
    if actual == expected || (actual.is_empty() && expected == "NULL") {
        return true;
    }
    match (actual.parse::<f64>(), expected.parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() <= 1e-9 * b.abs().max(1.0),
        _ => false,
    }
}

fn same_rows(actual: &[Vec<String>], expected: &[Vec<String>]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(a, e)| a.len() == e.len() && a.iter().zip(e).all(|(a, e)| same_cell(a, e)))
}

fn answer(path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines = text.lines().skip(1);
    lines.map(|l| l.split('|').map(str::to_string).collect()).collect()
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

/// `ScaleQ9Thresholds`: the thresholds in the stored Q9 are for SF 1.
fn scale_q9(sql: &str, scale: f64) -> String {
    let mut result = String::new();
    let mut rest = sql;
    while let Some(at) = rest.find(") > ") {
        let (before, after) = rest.split_at(at + 4);
        result.push_str(before);
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(" THEN") {
            let threshold: f64 = after[..digits].parse().expect("digits");
            result.push_str(&((threshold * scale).round() as i64).max(1).to_string());
            rest = &after[digits..];
        } else {
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, tables, dsdgen, sf, wanted @ ..] = args.as_slice() else {
        panic!("usage: tpcds <dir with .tbl files> <extension/tpcds/dsdgen> <sf> [query numbers]");
    };
    let dsdgen = Path::new(dsdgen);
    let database = Database::open_in_memory()?;
    let mut connection = database.connect()?;
    load(&mut connection, Path::new(tables), dsdgen)?;
    let queries: Vec<usize> = if wanted.is_empty() {
        (1..=99).collect()
    } else {
        wanted.iter().map(|q| q.parse().expect("a query number")).collect()
    };
    let scale: f64 = sf.parse().expect("a scale factor");
    let (mut correct, mut total_time) = (0, 0.0);
    for q in &queries {
        let mut sql = fs::read_to_string(dsdgen.join(format!("queries/{q:02}.sql"))).expect("the query");
        if *q == 9 {
            sql = scale_q9(&sql, scale);
        }
        if std::env::var_os("TPCDS_EXPLAIN").is_some() {
            connection.query("SET explain_output = 'ALL'")?;
            let plan = run(&mut connection, &format!("EXPLAIN {sql}"))?;
            println!("{}", plan.last().map(|row| row[1].as_str()).unwrap_or(""));
        }
        let expected = answer(&dsdgen.join(format!("answers/sf{sf}/{q:02}.csv")));
        print!("q{q:02}: ");
        std::io::stdout().flush().ok();
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&mut connection, &sql)));
        let time = started.elapsed().as_secs_f64();
        total_time += time;
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
