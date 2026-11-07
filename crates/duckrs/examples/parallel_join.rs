//! Times joins of a table with a smaller one and a much smaller one, on one thread and on all of
//! them, and compares their answers.
//!
//! ```text
//! cargo run --release -p duckrs --example parallel_join -- <rows>
//! ```

use duckrs::{Database, Result};
use std::time::Instant;

const QUERIES: &[&str] = &[
    "SELECT count(*), sum(d.v) FROM f JOIN d ON f.k = d.k",
    "SELECT d.g, count(*), sum(f.a) FROM f JOIN d ON f.k = d.k GROUP BY d.g",
    "SELECT count(*), sum(s.w) FROM f JOIN s ON f.c = s.c WHERE f.a % 2 = 0",
    "SELECT count(*), count(d.v) FROM f LEFT JOIN d ON f.a = d.k",
    "SELECT count(*) FROM d WHERE k NOT IN (SELECT a FROM f WHERE a % 3 = 0)",
    "SELECT count(*), sum(f.a) FROM f JOIN d ON f.k = d.k AND f.c < d.v % 1000",
    "SELECT f.a, d.v FROM f JOIN d ON f.k = d.k WHERE f.c < 5",
];

fn main() -> Result<()> {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|r| r.parse().ok())
        .unwrap_or(10_000_000);
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    let dimension = rows / 10;
    con.query(&format!(
        "CREATE TABLE f AS SELECT i AS a, (i * 7919) % {dimension} AS k, i % 1000 AS c FROM range({rows}) r(i)"
    ))?;
    con.query(&format!(
        "CREATE TABLE d AS SELECT i AS k, i * 3 AS v, i % 10 AS g FROM range({dimension}) r(i)"
    ))?;
    con.query("CREATE TABLE s AS SELECT i AS c, i * 2 AS w FROM range(1000) r(i)")?;
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    println!("{rows} rows, 1 vs {available} threads");
    for query in QUERIES {
        let mut seconds = Vec::new();
        let mut answers: Vec<String> = Vec::new();
        // the first run of a query pays for what later ones find cached
        con.query(query)?;
        for threads in [1, available] {
            con.query(&format!("SET threads={threads}"))?;
            let start = Instant::now();
            let result = con.query(query)?;
            seconds.push(start.elapsed().as_secs_f64());
            let mut rows: Vec<String> = (0..result.row_count())
                .map(|row| {
                    (0..result.column_count())
                        .map(|column| format!("{:?}", result.value(column, row)))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            rows.sort();
            answers.push(rows.join(";"));
        }
        let same = if answers[0] == answers[1] { "same" } else { "DIFFERENT" };
        println!(
            "{:7.3}s {:7.3}s {:5.2}x {same} {:8} {query}",
            seconds[0],
            seconds[1],
            seconds[0] / seconds[1],
            answers[0].len()
        );
        if answers[0] != answers[1] {
            println!(
                "  1: {}
  n: {}",
                &answers[0][..answers[0].len().min(300)],
                &answers[1][..answers[1].len().min(300)]
            );
        }
    }
    Ok(())
}
