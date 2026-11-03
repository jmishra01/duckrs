//! Measures parser throughput over a statement corpus.
//!
//! `cargo run --release -p duckrs-parser --example corpus -- ../duckdb --dump /tmp/stmts`
//! `cargo run --release -p duckrs-parser --example bench -- /tmp/stmts`
//!
//! The input is NUL-separated SQL. Reports the time per statement to parse and to parse and
//! transform, both on one thread (what C++'s `Parser::ParseQuery` measures) and through the
//! public API, which hands each call to a pooled parser thread.

use duckrs_parser::ast::Statement;
use duckrs_parser::transformer::Transformer;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench STATEMENTS_FILE");
    let text = std::fs::read_to_string(&path).expect("statements file is readable");
    let statements: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let n = statements.len() as f64;
    let report = |label: &str, elapsed: Duration, failures: usize| {
        let secs = elapsed.as_secs_f64();
        println!(
            "{label:<24} {:>8.3} s  {:>7.1} us/statement  ({failures} errors)",
            secs,
            secs / n * 1e6
        );
    };
    // On one large-stack thread, like C++'s `Parser::ParseQuery`: matching only, then matching
    // and transforming.
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn({
            let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
            move || {
                let parse_ast_here = |sql: &str| -> duckrs_common::Result<Vec<Statement>> {
                    let (_, trees) = duckrs_parser::parse_on_this_thread(sql)?;
                    let mut transformer = Transformer::new(duckrs_parser::Grammar::duckdb());
                    trees.iter().map(|tree| transformer.top_level_statement(tree)).collect()
                };
                // warm up: the grammar loads on first use
                let _ = parse_ast_here("SELECT 1");
                let start = Instant::now();
                let failures = statements
                    .iter()
                    .filter(|s| duckrs_parser::parse_on_this_thread(s).is_err())
                    .count();
                let matched = (start.elapsed(), failures);
                let start = Instant::now();
                let failures = statements.iter().filter(|s| parse_ast_here(s).is_err()).count();
                (matched, (start.elapsed(), failures))
            }
        })
        .unwrap();
    let ((matched, match_failures), (transformed, transform_failures)) = handle.join().unwrap();
    report("parse_on_this_thread", matched, match_failures);
    report("parse_ast_on_this_thread", transformed, transform_failures);

    let start = Instant::now();
    let failures = statements.iter().filter(|s| duckrs_parser::parse(s).is_err()).count();
    report("parse", start.elapsed(), failures);

    let start = Instant::now();
    let failures = statements
        .iter()
        .filter(|s| duckrs_parser::parse_ast(s).is_err())
        .count();
    report("parse_ast", start.elapsed(), failures);
}
