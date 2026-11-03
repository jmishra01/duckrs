//! Measures how much of DuckDB's sqllogictest corpus the parser accepts.
//!
//! `cargo run --release -p duckrs-parser --example corpus -- [../duckdb] [--show N] [--min-parse PCT]
//! [--min-transform PCT]`
//!
//! With `--min-parse`, exits with an error if fewer statements than PCT percent parse (CI uses it).
//!
//! Statements that parse are also transformed into the AST. Those the transformer covers are
//! rendered back to SQL, and the rendering must parse and transform again (round trip). Like
//! DuckDB's own `ToString`, the second rendering is not always identical: VALUES lists nest one
//! level deeper, GROUPING SETS renumber their expressions and `UNNEST` is lowercased, so identical
//! renderings are reported separately. With `--min-transform PCT`, exits with an error if fewer
//! covered statements than PCT percent transform and round trip.
//!
//! Statements DuckDB must accept (`statement ok`, `query`) should parse; statements whose
//! expected error is a parser/syntax error should not. Loop bodies use their first iteration.

use duckrs_common::ErrorKind;
use sqllogictest::parser::{ExpectedResult, LoopKind, Record, parse_file};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default)]
struct Stats {
    must_parse: usize,
    skipped: usize,
    parsed: usize,
    must_fail: usize,
    rejected: usize,
    failures: Vec<(String, String, String)>,
    wrongly_accepted: Vec<(String, String)>,
    transformed: usize,
    not_ported: usize,
    not_ported_by_kind: BTreeMap<String, usize>,
    transform_failures: Vec<(String, String, String)>,
    round_trips: usize,
    identical: usize,
    round_trip_failures: Vec<(String, String, String)>,
}

fn substitute(sql: &str, vars: &[(String, String)]) -> String {
    let mut out = sql.to_string();
    for (name, value) in vars {
        // Like the C++ runner, only iterator names with commas split their values.
        let pairs: Vec<(&str, &str)> = if name.contains(',') {
            name.split(',').zip(value.split(',')).collect()
        } else {
            vec![(name.as_str(), value.as_str())]
        };
        for (n, v) in pairs {
            out = out.replace(&format!("${{{n}}}"), v).replace(&format!("{{{n}}}"), v);
        }
    }
    out
}

/// SQL that still holds test-environment placeholders (`{sf}`, `<variable:x>`) the runner
/// would fill in at run time; parsing it says nothing about the parser.
fn has_placeholders(sql: &str) -> bool {
    let b = sql.as_bytes();
    sql.contains("<variable:")
        || b.windows(2).enumerate().any(|(i, w)| {
            w[0] == b'{' && w[1].is_ascii_alphabetic() && {
                let rest = &sql[i + 1..];
                rest.find('}')
                    .is_some_and(|end| rest[..end].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
            }
        })
}

fn visit(records: &[Record], file: &str, vars: &mut Vec<(String, String)>, stats: &mut Stats) {
    for record in records {
        match record {
            Record::Statement { expected, sql, .. } => {
                let sql = substitute(sql, vars);
                if has_placeholders(&sql) {
                    stats.skipped += 1;
                    continue;
                }
                match expected {
                    ExpectedResult::Success => check_parses(&sql, file, stats),
                    ExpectedResult::Error(msg) if msg.contains("Parser Error") || msg.contains("syntax error") => {
                        stats.must_fail += 1;
                        if duckrs_parser::parse(&sql).is_err() {
                            stats.rejected += 1;
                        } else {
                            stats.wrongly_accepted.push((file.to_string(), sql));
                        }
                    }
                    _ => {}
                }
            }
            Record::Query { sql, .. } => {
                let sql = substitute(sql, vars);
                if has_placeholders(&sql) {
                    stats.skipped += 1;
                } else {
                    check_parses(&sql, file, stats);
                }
            }
            Record::Loop {
                iterator, kind, body, ..
            } => {
                let first = match kind {
                    LoopKind::Range { start, .. } => start.to_string(),
                    LoopKind::Foreach(items) => sqllogictest::runner::expand_foreach(items)
                        .first()
                        .cloned()
                        .unwrap_or_default(),
                };
                vars.push((iterator.clone(), first));
                visit(body, file, vars, stats);
                vars.pop();
            }
            _ => {}
        }
    }
}

fn check_parses(sql: &str, file: &str, stats: &mut Stats) {
    stats.must_parse += 1;
    match duckrs_parser::parse(sql) {
        Ok(_) => stats.parsed += 1,
        Err(e) => {
            stats.failures.push((file.to_string(), sql.to_string(), e.to_string()));
            return;
        }
    }
    let rendered = match duckrs_parser::parse_ast(sql) {
        Ok(statements) => statements.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(";\n"),
        Err(e) if e.kind == ErrorKind::NotImplemented => {
            stats.not_ported += 1;
            *stats.not_ported_by_kind.entry(e.message.clone()).or_default() += 1;
            return;
        }
        Err(e) => {
            stats
                .transform_failures
                .push((file.to_string(), sql.to_string(), e.to_string()));
            return;
        }
    };
    stats.transformed += 1;
    let again = duckrs_parser::parse_ast(&rendered)
        .map(|statements| statements.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(";\n"));
    match again {
        Ok(again) => {
            stats.round_trips += 1;
            if again == rendered {
                stats.identical += 1;
            }
        }
        Err(e) => stats
            .round_trip_failures
            .push((file.to_string(), rendered, e.to_string())),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "../duckdb".into());
    let show: usize = args
        .iter()
        .position(|a| a == "--show")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let root = Path::new(&root).canonicalize().expect("DuckDB checkout");
    let files = sqllogictest::discover(&root, &[], true);
    let mut stats = Stats::default();
    // Files under test/extension exercise grammar that loadable extensions add at run time.
    for file in files.iter().filter(|f| !f.starts_with("test/extension/")) {
        if let Ok(test) = parse_file(file, &root) {
            visit(&test.records, file, &mut Vec::new(), &mut stats);
        }
    }
    let pct = |a: usize, b: usize| 100.0 * a as f64 / b.max(1) as f64;
    println!("skipped (unresolved test placeholders): {}", stats.skipped);
    println!(
        "must parse: {}/{} ({:.2}%)\nmust be rejected (parser errors): {}/{} ({:.2}%)",
        stats.parsed,
        stats.must_parse,
        pct(stats.parsed, stats.must_parse),
        stats.rejected,
        stats.must_fail,
        pct(stats.rejected, stats.must_fail)
    );
    println!(
        "transformed: {} (not yet ported: {}, failed: {})\nround trips: {}/{} ({:.2}%), identical renderings: {:.2}%",
        stats.transformed,
        stats.not_ported,
        stats.transform_failures.len(),
        stats.round_trips,
        stats.transformed,
        pct(stats.round_trips, stats.transformed),
        pct(stats.identical, stats.transformed)
    );
    let mut missing: Vec<_> = stats.not_ported_by_kind.iter().collect();
    missing.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    println!("not yet ported:");
    for (message, count) in missing.iter().take(12) {
        println!("  {count:>6}  {message}");
    }
    // Group failures by their first word to show which statement kinds are missing.
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for (_, sql, _) in &stats.failures {
        let kind = sql.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        *by_kind.entry(kind).or_default() += 1;
    }
    let mut kinds: Vec<_> = by_kind.into_iter().collect();
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    println!("failures by first word: {:?}", &kinds[..kinds.len().min(15)]);
    for (file, sql, err) in stats.failures.iter().take(show) {
        println!("--- {file}\n{sql}\n=> {err}");
    }
    for (file, sql, err) in stats.transform_failures.iter().take(show) {
        println!("--- TRANSFORM {file}\n{sql}\n=> {err}");
    }
    for (file, sql, err) in stats.round_trip_failures.iter().take(show) {
        println!("--- ROUND TRIP {file}\n{sql}\n=> {err}");
    }
    if show > 0 {
        for (file, sql) in stats.wrongly_accepted.iter().take(show / 2) {
            println!("--- ACCEPTED (should fail) {file}\n{sql}");
        }
    }
    let min: Option<f64> = args
        .iter()
        .position(|a| a == "--min-parse")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
    if let Some(min) = min
        && pct(stats.parsed, stats.must_parse) < min
    {
        eprintln!("error: parse rate below {min}%");
        std::process::exit(1);
    }
    let min_transform: Option<f64> = args
        .iter()
        .position(|a| a == "--min-transform")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
    let covered = stats.transformed + stats.transform_failures.len();
    if let Some(min) = min_transform
        && pct(stats.round_trips, covered) < min
    {
        eprintln!("error: transform round-trip rate below {min}%");
        std::process::exit(1);
    }
}
