//! Drives the runner end to end with a scripted engine, so runner logic is tested without C++.

use sqllogictest::engine::{ColumnType, Connection, Database, Engine, QueryOutput};
use sqllogictest::runner::{Outcome, RunConfig, run_file};
use std::path::{Path, PathBuf};

struct MockEngine;
struct MockDatabase;
struct MockConnection;

impl Engine for MockEngine {
    fn name(&self) -> &str {
        "mock"
    }
    fn open(&self, _path: Option<&Path>, _read_only: bool) -> Result<Box<dyn Database>, String> {
        Ok(Box::new(MockDatabase))
    }
}

impl Database for MockDatabase {
    fn connect(&self) -> Result<Box<dyn Connection>, String> {
        Ok(Box::new(MockConnection))
    }
}

fn ints(values: &[&[i64]]) -> QueryOutput {
    let columns = values.first().map_or(0, |r| r.len());
    QueryOutput {
        names: (0..columns).map(|i| format!("c{i}")).collect(),
        types: vec![ColumnType::Integer; columns],
        rows: values
            .iter()
            .map(|r| r.iter().map(|v| Some(v.to_string())).collect())
            .collect(),
    }
}

impl Connection for MockConnection {
    fn query(&mut self, sql: &str) -> Result<QueryOutput, String> {
        match sql {
            "SELECT 42" => Ok(ints(&[&[42]])),
            "SELECT 1, 2 UNION ALL SELECT 3, 4" => Ok(ints(&[&[3, 4], &[1, 2]])),
            "SELECT NULL" => Ok(QueryOutput {
                names: vec!["n".into()],
                types: vec![ColumnType::Other],
                rows: vec![vec![None]],
            }),
            "SELECT 0.1 + 0.2" => Ok(QueryOutput {
                names: vec!["d".into()],
                types: vec![ColumnType::Double],
                rows: vec![vec![Some("0.30000000000000004".into())]],
            }),
            s if s.starts_with("SELECT ") && s[7..].parse::<i64>().is_ok() => Ok(ints(&[&[s[7..].parse().unwrap()]])),
            s if s.starts_with("CREATE") => Ok(QueryOutput::default()),
            "SELECT missing" => Err("Binder Error: Referenced column \"missing\" not found".into()),
            other => Err(format!("Parser Error: mock cannot run {other}")),
        }
    }
    fn load_extension(&mut self, name: &str) -> bool {
        name == "parquet"
    }
}

fn run(name: &str, body: &str) -> Outcome {
    let dir = std::env::temp_dir().join(format!("duckrs-slt-mock-{}-{name}", std::process::id()));
    std::fs::create_dir_all(dir.join("test")).unwrap();
    let file = format!("test/{name}.test");
    std::fs::write(dir.join(&file), body).unwrap();
    let config = RunConfig {
        root: dir.clone(),
        scratch: dir.join("scratch"),
        continue_on_failure: false,
        vector_size: 2048,
    };
    let report = run_file(&config, &MockEngine, &file, 0);
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
    report.outcome
}

#[test]
fn passing_file() {
    let outcome = run(
        "pass",
        "# a comment\n\nstatement ok\nCREATE TABLE t(i INT)\n\nquery I\nSELECT 42\n----\n42\n\n\
         query II rowsort\nSELECT 1, 2 UNION ALL SELECT 3, 4\n----\n1\t2\n3\t4\n\n\
         query T\nSELECT NULL\n----\nNULL\n\nquery R\nSELECT 0.1 + 0.2\n----\n0.3\n\n\
         statement error\nSELECT missing\n----\n<REGEX>:Binder Error:.*not found.*\n\n\
         loop i 0 3\n\nquery I\nSELECT ${i}\n----\n<REGEX>:[0-2]\n\nendloop\n\nloop i 0 3\n\nonlyif i=1\nquery I\nSELECT {i}\n----\n1\n\nendloop\n",
    );
    assert_eq!(outcome, Outcome::Pass);
}

#[test]
fn wrong_result_fails_at_the_right_line() {
    let outcome = run(
        "wrong",
        "statement ok\nCREATE TABLE t(i INT)\n\nquery I\nSELECT 42\n----\n43\n",
    );
    let Outcome::Fail { location, message } = outcome else {
        panic!("expected failure, got {outcome:?}")
    };
    assert_eq!(location.line, 4);
    assert!(message.contains("42 <> 43"), "{message}");
}

#[test]
fn hash_threshold_compares_hashes() {
    let outcome = run(
        "hash",
        "hash-threshold 0\n\nquery I nosort mylabel\nSELECT 42\n----\n\n\
         query I nosort mylabel\nSELECT 42\n----\n\n\
         query I\nSELECT 42\n----\n1 values hashing to 50a2fabfdd276f573ff97ace8b11c5f4\n",
    );
    assert_eq!(outcome, Outcome::Pass);
}

#[test]
fn missing_extension_skips() {
    assert_eq!(
        run("ext", "require parquet\n\nquery I\nSELECT 42\n----\n42\n"),
        Outcome::Pass
    );
    let outcome = run("noext", "require spatial\n\nquery I\nSELECT 42\n----\n42\n");
    assert_eq!(outcome, Outcome::Skip("require spatial".into()));
}

#[test]
fn ignore_error_messages_stops_file_as_pass() {
    let outcome = run(
        "ignore",
        "set ignore_error_messages mock cannot run\n\nstatement ok\nSELECT something\n\nquery I\nSELECT 42\n----\n0\n",
    );
    assert_eq!(outcome, Outcome::Pass);
}

#[test]
fn unexpected_success_fails() {
    let outcome = run("unexpected", "statement error\nSELECT 42\n----\nsome error\n");
    assert!(matches!(outcome, Outcome::Fail { .. }), "{outcome:?}");
}

#[test]
fn concurrent_loop_runs_every_iteration() {
    let outcome = run(
        "concurrent",
        "concurrentloop i 0 8\n\nquery I\nSELECT ${i}\n----\n<REGEX>:[0-7]\n\nendloop\n",
    );
    assert_eq!(outcome, Outcome::Pass);
}
