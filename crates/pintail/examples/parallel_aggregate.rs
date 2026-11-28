//! Times aggregates over a table, on one thread and on all of them, and compares their answers.
//! Floating point sums can differ in their last digits, as they add up in another order.
//!
//! ```text
//! cargo run --release -p pintail --example parallel_aggregate -- <rows>
//! ```

use pintail::{Database, Result};
use std::time::Instant;

const QUERIES: &[&str] = &[
    "SELECT sum(a), avg(b), min(c), max(c), count(*) FROM t",
    "SELECT sum(a * 2 + b), count(*) FROM t WHERE c % 3 = 0",
    "SELECT g, sum(a), avg(b), count(*) FROM t GROUP BY g",
    "SELECT h, sum(a), max(b) FROM t GROUP BY h",
    "SELECT median(b), stddev_samp(a) FROM t",
];

fn main() -> Result<()> {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|r| r.parse().ok())
        .unwrap_or(10_000_000);
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    con.query(&format!(
        "CREATE TABLE t AS SELECT i AS a, i::DOUBLE / 7 AS b, i % 1000 AS c, i % 10 AS g, i % 100000 AS h FROM \
         range({rows}) r(i)"
    ))?;
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
