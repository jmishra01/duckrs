// Ported from duckdb/benchmark/benchmark_runner.cpp and interpreted_benchmark.cpp @ rust-port-baseline
//
//! A Rust port of DuckDB's benchmark runner. It reads the `.benchmark` files of a DuckDB checkout
//! (`benchmark/micro`, `benchmark/tpch`, ...) and times them on any [`Engine`] of the sqllogictest
//! crate: duckrs, and with the `reference` feature the C++ DuckDB library, so that the two can be
//! compared on the same queries.

pub mod datagen;
pub mod file;

use file::{Benchmark, ExpectedResult};
use sqllogictest::engine::{Connection, Database, Engine, QueryOutput};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// `BenchmarkRunner::DUCKDB_BENCHMARK_DIRECTORY`: where caches and generated data go, under the root.
pub const BENCHMARK_DIRECTORY: &str = "duckdb_benchmark_data";

/// What the runner was asked for (`BenchmarkConfiguration` and `BenchmarkRunner`'s settings).
#[derive(Debug, Clone)]
pub struct Config {
    pub timed_runs: usize,
    /// `--threads`: `PRAGMA threads` of each database
    pub threads: Option<usize>,
    pub memory_limit: Option<String>,
    /// how long a run may take before the benchmark stops (it is not interrupted: the run ends first)
    pub timeout: Option<Duration>,
    /// `--name value`: the benchmarks' `argument`s
    pub arguments: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timed_runs: 5,
            threads: None,
            memory_limit: None,
            timeout: Some(Duration::from_secs(30)),
            arguments: HashMap::new(),
        }
    }
}

/// How one benchmark went on one engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// the time of each timed run
    Timings(Vec<Duration>),
    Error(String),
    Incorrect(String),
    Timeout,
}

impl Outcome {
    /// The median of the timed runs.
    pub fn median(&self) -> Option<Duration> {
        let Outcome::Timings(times) = self else { return None };
        if times.is_empty() {
            return None;
        }
        let mut sorted = times.clone();
        sorted.sort();
        Some(sorted[sorted.len() / 2])
    }
}

/// The `.benchmark` files under `benchmark/` whose path or group matches `pattern` entirely
/// (`RE2::FullMatch` of `run_benchmarks`), sorted by path.
pub fn discover(pattern: Option<&str>, arguments: &HashMap<String, String>) -> Result<Vec<Benchmark>, String> {
    let regex = match pattern {
        Some(p) => Some(regex::Regex::new(&format!("^(?:{p})$")).map_err(|e| e.to_string())?),
        None => None,
    };
    let mut paths: Vec<String> = WalkDir::new("benchmark")
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
        .filter(|p| p.ends_with(".benchmark"))
        .collect();
    paths.sort();
    let mut benchmarks = Vec::new();
    for path in paths {
        let group = path.split('/').nth(1).unwrap_or_default().to_string();
        if let Some(regex) = &regex {
            // the group of the path is enough to skip a file without parsing it
            if !regex.is_match(&path) && !regex.is_match(&group) {
                // the `group` line may say otherwise: parse it to be sure
                let Ok(benchmark) = file::load(&path, &HashMap::new()) else {
                    continue;
                };
                if !regex.is_match(&benchmark.group()) {
                    continue;
                }
            }
        }
        match file::load(&path, arguments) {
            Ok(benchmark) => benchmarks.push(benchmark),
            // arguments are given for the benchmarks that have them; skip the others
            Err(error) if error.starts_with("Invalid benchmark argument") => continue,
            Err(error) => {
                let mut broken = Benchmark {
                    path: path.clone(),
                    ..Benchmark::default()
                };
                broken.queries.insert("__error".to_string(), error);
                benchmarks.push(broken);
            }
        }
    }
    Ok(benchmarks)
}

/// An open benchmark database (`InterpretedBenchmarkState`).
struct State {
    connection: Box<dyn Connection>,
    // the database lives as long as the connection
    _database: Box<dyn Database>,
}

fn data_directory() -> PathBuf {
    PathBuf::from(BENCHMARK_DIRECTORY)
}

/// `InterpretedBenchmark::GetDatabasePath`: the cache, a fresh file for persistent storage, or memory.
fn database_path(benchmark: &Benchmark, engine: &dyn Engine) -> Option<PathBuf> {
    // each engine keeps its own caches: their files need not be readable by the other
    let directory = data_directory().join(engine.name());
    if let Some(cache) = &benchmark.cache_db {
        return Some(directory.join(cache));
    }
    if benchmark.in_memory {
        return None;
    }
    let path = directory.join("duckdb_benchmark_db.db");
    delete_database(&path);
    Some(path)
}

fn delete_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension(format!(
        "{}.wal",
        path.extension().unwrap_or_default().to_string_lossy()
    )));
}

/// Runs each statement of `sql` (several `;`-separated statements go in one call).
fn run_all(connection: &mut dyn Connection, sql: &str) -> Result<QueryOutput, String> {
    connection.query(sql)
}

/// `InterpretedBenchmark::OpenState`.
fn open_state(
    benchmark: &Benchmark,
    engine: &dyn Engine,
    config: &Config,
    path: Option<&Path>,
) -> Result<State, String> {
    if let Some(path) = path
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let database = match engine.open_with_version(path, false, benchmark.storage_version.as_deref()) {
        Ok(database) => database,
        Err(_) if path.is_some() => {
            // probably a file of another storage version: start over
            delete_database(path.expect("checked above"));
            engine.open_with_version(path, false, benchmark.storage_version.as_deref())?
        }
        Err(error) => return Err(error),
    };
    let mut connection = database.connect()?;
    if let Some(threads) = config.threads {
        connection.query(&format!("PRAGMA threads={threads}"))?;
    }
    if let Some(limit) = &config.memory_limit {
        connection.query(&format!("PRAGMA memory_limit='{limit}'"))?;
    }
    for extension in &benchmark.extensions {
        if !is_generator(extension) && !connection.load_extension(extension) {
            return Err(format!(
                "Extension {extension} is not available/was not compiled. Cannot run this benchmark."
            ));
        }
    }
    if let Some(init) = benchmark.query("init") {
        run_all(connection.as_mut(), init)?;
    }
    Ok(State {
        connection,
        _database: database,
    })
}

/// `tpch` and `tpcds` are carried out by the runner when the engine has no such extension.
fn is_generator(extension: &str) -> bool {
    matches!(extension, "tpch" | "tpcds")
}

/// Runs a load query, with `CALL dbgen` / `CALL dsdgen` done by the generators of the checkout.
fn run_load(connection: &mut dyn Connection, benchmark: &Benchmark, load: &str) -> Result<(), String> {
    for extension in &benchmark.load_only_extensions {
        if !is_generator(extension) && !connection.load_extension(extension) {
            return Err(format!(
                "Extension {extension} is not available. Cannot run this benchmark."
            ));
        }
    }
    let (calls, rest) = datagen::find_generator_calls(load);
    for (generator, sf) in calls {
        datagen::load(connection, generator, &sf, &data_directory())?;
    }
    if !rest.trim().is_empty() {
        run_all(connection, &rest)?;
    }
    Ok(())
}

/// `InterpretedBenchmark::Initialize`: opens the database and loads the data, or finds it cached.
fn initialize(benchmark: &Benchmark, engine: &dyn Engine, config: &Config) -> Result<State, String> {
    let path = database_path(benchmark, engine);
    let mut state = open_state(benchmark, engine, config, path.as_deref())?;
    let load = benchmark.query("load");
    let reload = benchmark.query("reload");
    let cached = if let Some(cache_file) = &benchmark.cache_file {
        data_directory().join(cache_file).exists()
    } else if benchmark.cache_db.is_some() {
        // tables in the cached database mean it was loaded
        state
            .connection
            .query("SHOW TABLES")
            .is_ok_and(|tables| tables.row_count() > 0)
    } else {
        false
    };
    if cached {
        if let Some(reload) = reload {
            run_load(state.connection.as_mut(), benchmark, reload)?;
        }
    } else if let Some(load) = load {
        let mut attempt = 0;
        loop {
            match run_load(state.connection.as_mut(), benchmark, load) {
                Ok(()) => break,
                Err(error) => {
                    // never leave a partly loaded cache behind
                    drop(state);
                    if let Some(path) = &path {
                        delete_database(path);
                    }
                    if attempt >= benchmark.retry_load {
                        return Err(error);
                    }
                    attempt += 1;
                    state = open_state(benchmark, engine, config, path.as_deref())?;
                }
            }
        }
    }
    if benchmark.cache_db.is_some() && benchmark.cache_no_connect {
        // the cache was made; the benchmark itself runs in memory
        let mut fresh = benchmark.clone();
        fresh.cache_db = None;
        fresh.cache_no_connect = false;
        fresh.in_memory = true;
        fresh.queries.remove("load");
        return initialize(&fresh, engine, config);
    }
    Ok(state)
}

/// `InterpretedBenchmark::VerifyInternal`: the rows against the expected ones.
pub fn verify(expected: &ExpectedResult, actual: &QueryOutput) -> Result<(), String> {
    if expected.column_count != actual.column_count() {
        return Err(format!(
            "Error in result: expected {} columns but got {}",
            expected.column_count,
            actual.column_count()
        ));
    }
    if expected.rows.len() != actual.row_count() {
        return Err(format!(
            "Error in result: expected {} rows but got {}",
            expected.rows.len(),
            actual.row_count()
        ));
    }
    for (r, (want, got)) in expected.rows.iter().zip(&actual.rows).enumerate() {
        for (c, (want, got)) in want.iter().zip(got).enumerate() {
            if !same_value(want, got.as_deref()) {
                return Err(format!(
                    "Error in result on row {} column {}: expected value \"{want}\" but got value \"{}\"",
                    r + 1,
                    c + 1,
                    got.as_deref().unwrap_or("NULL")
                ));
            }
        }
    }
    Ok(())
}

/// A value as `VerifyInternal` compares it: the same text, `NULL`, `(empty)`, or the same number
/// once the expected text is cast to the value's type (`ValuesAreEqual`, approximate for floats).
fn same_value(expected: &str, actual: Option<&str>) -> bool {
    let Some(actual) = actual else {
        return expected == "NULL" || expected == "(empty)";
    };
    if expected == actual || (expected == "(empty)" && actual.is_empty()) {
        return true;
    }
    match (expected.parse::<f64>(), actual.parse::<f64>()) {
        (Ok(e), Ok(a)) => {
            if e == a {
                return true;
            }
            // `ApproxEqual` of doubles
            let tolerance = 1e-9 * e.abs().max(a.abs()).max(1.0);
            (e - a).abs() <= tolerance
        }
        _ => {
            // booleans and the like: the casted spellings
            expected.eq_ignore_ascii_case(actual)
        }
    }
}

/// `InterpretedBenchmark::Verify`: the run's result (or the `result_query` over it) against the
/// expected rows.
fn verify_run(state: &mut State, benchmark: &Benchmark, output: &QueryOutput) -> Result<(), String> {
    let Some(expected) = &benchmark.result else {
        return Ok(());
    };
    match &expected.query {
        None => verify(expected, output),
        Some(query) => {
            // `__answer` holds the run's result; it is made by running the query again
            state.connection.query(&format!(
                "CREATE OR REPLACE TEMP TABLE __answer AS {}",
                benchmark.run_query().trim().trim_end_matches(';')
            ))?;
            let result = state.connection.query(query)?;
            verify(expected, &result)
        }
    }
}

/// `BenchmarkRunner::RunBenchmark`: a cold run, then `timed_runs` timed ones, each checked.
pub fn run_benchmark(benchmark: &Benchmark, engine: &dyn Engine, config: &Config) -> Outcome {
    if let Some(error) = benchmark.queries.get("__error") {
        return Outcome::Error(error.clone());
    }
    let mut state = match initialize(benchmark, engine, config) {
        Ok(state) => state,
        Err(error) => return Outcome::Error(error),
    };
    // `Assert`
    for assertion in &benchmark.asserts {
        let query = assertion.query.as_deref().unwrap_or_default();
        match state.connection.query(query) {
            Ok(result) => {
                if let Err(error) = verify(assertion, &result) {
                    return Outcome::Error(format!("Assertion query failed:\n{error}"));
                }
            }
            Err(error) => return Outcome::Error(error),
        }
    }
    let mut timings = Vec::with_capacity(config.timed_runs);
    for run in 0..=config.timed_runs {
        let hot = run > 0;
        if hot && benchmark.require_reinit {
            state = match initialize(benchmark, engine, config) {
                Ok(state) => state,
                Err(error) => return Outcome::Error(error),
            };
        }
        let (output, elapsed) = match state.connection.query_timed(benchmark.run_query()) {
            Ok(result) => result,
            Err(error) => return Outcome::Error(error),
        };
        if hot {
            if config.timeout.is_some_and(|limit| elapsed > limit) {
                return Outcome::Timeout;
            }
            if let Err(error) = verify_run(&mut state, benchmark, &output) {
                return Outcome::Incorrect(error);
            }
            timings.push(elapsed);
        }
        if let Some(cleanup) = benchmark.query("cleanup")
            && let Err(error) = run_all(state.connection.as_mut(), cleanup)
        {
            return Outcome::Error(error);
        }
    }
    Outcome::Timings(timings)
}

/// The geometric mean of `ratios` (the v1 gate compares the engines with it).
pub fn geometric_mean(ratios: &[f64]) -> Option<f64> {
    if ratios.is_empty() {
        return None;
    }
    let log_sum: f64 = ratios.iter().map(|r| r.ln()).sum();
    Some((log_sum / ratios.len() as f64).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_compare_as_the_cpp_runner_does() {
        assert!(same_value("NULL", None));
        assert!(same_value("(empty)", Some("")));
        assert!(same_value("42", Some("42")));
        assert!(same_value("1.5", Some("1.50")));
        assert!(same_value("0.1", Some("0.1000000000000001")));
        assert!(!same_value("3.33", Some("3.3333333")));
        assert!(same_value("true", Some("TRUE")));
        assert!(!same_value("41", Some("42")));
        assert!(!same_value("x", None));
    }

    #[test]
    fn the_geometric_mean_of_ratios() {
        let mean = geometric_mean(&[1.0, 4.0]).unwrap();
        assert!((mean - 2.0).abs() < 1e-12);
        assert_eq!(geometric_mean(&[]), None);
    }

    #[test]
    fn the_median_of_runs() {
        let outcome = Outcome::Timings(vec![
            Duration::from_millis(3),
            Duration::from_millis(1),
            Duration::from_millis(2),
        ]);
        assert_eq!(outcome.median(), Some(Duration::from_millis(2)));
    }
}
