//! Measures parser throughput over a statement corpus.
//!
//! `cargo run --release -p duckrs-parser --example corpus -- ../duckdb --dump /tmp/stmts`
//! `cargo run --release -p duckrs-parser --example bench -- /tmp/stmts`
//!
//! The input is NUL-separated SQL. Reports the time per statement to parse (on this thread),
//! parse through the public API, and parse and transform.

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench STATEMENTS_FILE");
    let text = std::fs::read_to_string(&path).expect("statements file is readable");
    let statements: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let n = statements.len() as f64;
    let report = |label: &str, start: Instant, failures: usize| {
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{label:<24} {:>8.3} s  {:>7.1} us/statement  ({failures} errors)",
            secs,
            secs / n * 1e6
        );
    };
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn({
            let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
            move || {
                let start = Instant::now();
                let failures = statements
                    .iter()
                    .filter(|s| duckrs_parser::parse_on_this_thread(s).is_err())
                    .count();
                (start, failures)
            }
        })
        .unwrap();
    let (start, failures) = handle.join().unwrap();
    report("parse_on_this_thread", start, failures);

    let start = Instant::now();
    let failures = statements.iter().filter(|s| duckrs_parser::parse(s).is_err()).count();
    report("parse", start, failures);

    let start = Instant::now();
    let failures = statements
        .iter()
        .filter(|s| duckrs_parser::parse_ast(s).is_err())
        .count();
    report("parse_ast", start, failures);
}
