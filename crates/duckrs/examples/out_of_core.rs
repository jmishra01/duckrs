//! Times sorts, aggregates and joins over a table without a memory limit and with a small one, which
//! makes them spill to temporary files, and compares their answers.
//!
//! ```text
//! cargo run --release -p duckrs --example out_of_core -- <rows> <memory_limit> [only]
//! ```
//!
//! With `only`, just the runs with the limit, to see what memory the process takes for them.
//! `DUCKRS_QUERY=<n>` runs only the n-th query, `DUCKRS_THREADS=<n>` sets `threads`, and
//! `DUCKRS_REPEAT=<n>` runs the queries n times, to tell memory that is kept from memory that the
//! allocator holds on to.

use duckrs::{Database, Result};
use std::time::Instant;

const QUERIES: &[&str] = &[
    "SELECT bit_xor(hash(k, a, s)), count(*) FROM (SELECT k, a, s FROM t ORDER BY s DESC, a)",
    "SELECT sum(k * r) FROM (SELECT k, row_number() OVER () AS r FROM (SELECT k FROM t ORDER BY k, a))",
    "SELECT count(*), sum(c), max(s) FROM (SELECT k, count(*) AS c, max(s) AS s FROM t GROUP BY k)",
    "SELECT count(*), sum(c) FROM (SELECT k, count(*) AS c FROM t GROUP BY k)",
    "SELECT count(*), sum(c) FROM (SELECT a % 1000 AS g, k, count(*) AS c FROM t GROUP BY ALL)",
    "SELECT k, a, s FROM t ORDER BY s DESC, a LIMIT 5 OFFSET 1000000",
    "SELECT count(*), sum(u.b), max(u.w) FROM t JOIN u ON t.k = u.k",
    "SELECT count(*), count(u.b), sum(t.a) FROM t LEFT JOIN u ON t.a = u.k",
];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rows: u64 = args.next().and_then(|r| r.parse().ok()).unwrap_or(10_000_000);
    let limit = args.next().unwrap_or_else(|| "100MB".to_string());
    let only = args.next().as_deref() == Some("only");
    let db = Database::open_in_memory()?;
    let mut con = db.connect()?;
    let directory = std::env::temp_dir().join(format!("duckrs_out_of_core_{}", std::process::id()));
    con.query(&format!("SET temp_directory='{}'", directory.display()))?;
    if let Ok(threads) = std::env::var("DUCKRS_THREADS") {
        con.query(&format!("SET threads={threads}"))?;
    }
    con.query(&format!(
        "CREATE TABLE t AS SELECT (i * 7919) % {rows} AS k, i AS a, 'value ' || (i % 100003) AS s FROM range({rows}) r(i)"
    ))?;
    con.query(&format!(
        "CREATE TABLE u AS SELECT (i * 31) % {rows} AS k, i AS b, 'w' || (i % 1009) AS w FROM range({}) r(i)",
        rows / 2
    ))?;
    println!("{rows} rows, no limit vs memory_limit='{limit}'");
    let pick: Option<usize> = std::env::var("DUCKRS_QUERY").ok().and_then(|n| n.parse().ok());
    let repeat: usize = std::env::var("DUCKRS_REPEAT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    let queries: Vec<(usize, &str)> = (0..repeat).flat_map(|_| QUERIES.iter().copied().enumerate()).collect();
    for (n, query) in queries {
        if pick.is_some_and(|pick| pick != n % 10) {
            continue;
        }
        let mut seconds = Vec::new();
        let mut answers = Vec::new();
        let settings = if only {
            vec![limit.as_str()]
        } else {
            vec!["-1", limit.as_str()]
        };
        for setting in settings {
            con.query(&format!("SET memory_limit='{setting}'"))?;
            let start = Instant::now();
            let result = con.query(query)?;
            seconds.push(start.elapsed().as_secs_f64());
            let rows: Vec<String> = (0..result.row_count())
                .map(|row| {
                    (0..result.column_count())
                        .map(|column| format!("{:?}", result.value(column, row)))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            answers.push(rows.join(";"));
        }
        if only {
            println!("{:7.3}s {query}", seconds[0]);
            continue;
        }
        let same = if answers[0] == answers[1] { "same" } else { "DIFFERENT" };
        println!(
            "{:7.3}s {:7.3}s {:5.2}x slower {same} {query}",
            seconds[0],
            seconds[1],
            seconds[1] / seconds[0]
        );
    }
    Ok(())
}
