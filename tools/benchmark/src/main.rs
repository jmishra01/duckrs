// Ported from duckdb/benchmark/benchmark_runner.cpp @ rust-port-baseline
//
//! `benchmark_runner`: times the `.benchmark` files of a DuckDB checkout on duckrs, and with the
//! `reference` feature on the C++ library too, as C++'s `benchmark_runner` does.
//!
//! ```text
//! cargo run --release -p benchmark -- [options] [name_pattern]
//! cargo run --release -p benchmark --features reference -- --engine=both 'benchmark/tpch/sf1/.*'
//! ```

use benchmark::{Config, Outcome, discover, geometric_mean, run_benchmark};
use sqllogictest::engine::{DuckrsEngine, Engine};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// The v1 gate (`docs/REMAINING_WORK.md`, item 48): the geometric mean of the duckrs / C++ ratios,
/// and the largest ratio of any one benchmark.
const GATE_GEOMETRIC_MEAN: f64 = 1.5;
const GATE_WORST: f64 = 3.0;

fn print_help() {
    eprintln!("Usage: benchmark_runner [options] [name_pattern]");
    eprintln!("              --list                 Show a list of all benchmarks");
    eprintln!("              --info                 Prints info about the benchmark");
    eprintln!("              --query                Prints query of the benchmark");
    eprintln!("              --threads=n            Sets the amount of threads to use during execution");
    eprintln!("              --timed-runs n         Sets the amount of timed runs per benchmark (default: 5)");
    eprintln!("              --memory_limit=n       Sets the memory limit to use during execution");
    eprintln!("              --out=[file]           Write the timings to file");
    eprintln!("              --root-dir DIR         The DuckDB checkout to run in (default: ../duckdb)");
    eprintln!("              --disable-timeout      Never stop a benchmark for taking long (30 seconds by default)");
    eprintln!("              --engine=E             duckrs (default), reference or both (needs --features reference)");
    eprintln!("              --no-summary           Do not print the summary at the end");
    eprintln!("              --NAME VALUE           Sets the benchmark argument NAME (e.g. --sf 10)");
    eprintln!(
        "              [name_pattern]         Run the benchmarks whose path or group matches, e.g. 'benchmark/tpch/sf1/.*'"
    );
}

fn engines(which: &str) -> Result<Vec<Box<dyn Engine>>, String> {
    let reference = || -> Result<Box<dyn Engine>, String> {
        #[cfg(feature = "reference")]
        {
            Ok(Box::new(sqllogictest::engine::ReferenceEngine))
        }
        #[cfg(not(feature = "reference"))]
        {
            Err("the reference engine needs a build with --features reference".to_string())
        }
    };
    Ok(match which {
        "duckrs" => vec![Box::new(DuckrsEngine)],
        "reference" => vec![reference()?],
        "both" => vec![Box::new(DuckrsEngine), reference()?],
        other => return Err(format!("unknown engine {other}: duckrs, reference or both")),
    })
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = Config::default();
    let mut pattern: Option<String> = None;
    let mut list = false;
    let mut info = false;
    let mut print_query = false;
    let mut summary = true;
    let mut out: Option<File> = None;
    let mut engine_name = "duckrs".to_string();
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../duckdb");
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let value = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        if arg == "--list" {
            list = true;
        } else if arg == "--info" {
            info = true;
        } else if arg == "--query" {
            print_query = true;
        } else if arg == "--no-summary" {
            summary = false;
        } else if arg == "--disable-timeout" {
            config.timeout = None;
        } else if arg == "-h" || arg == "--help" {
            print_help();
            return 0;
        } else if let Some(n) = arg.strip_prefix("--threads=") {
            match n.parse() {
                Ok(n) => config.threads = Some(n),
                Err(_) => {
                    eprintln!("Invalid value for --threads: {n}");
                    return 1;
                }
            }
        } else if let Some(limit) = arg.strip_prefix("--memory_limit=") {
            config.memory_limit = Some(limit.to_string());
        } else if let Some(file) = arg.strip_prefix("--out=") {
            match File::create(file) {
                Ok(file) => out = Some(file),
                Err(_) => {
                    eprintln!("Could not open file {file} for writing");
                    return 1;
                }
            }
        } else if let Some(engine) = arg.strip_prefix("--engine=") {
            engine_name = engine.to_string();
        } else if arg == "--root-dir" {
            let Some(dir) = value(&mut i) else {
                eprintln!("Missing argument for --root-dir");
                print_help();
                return 1;
            };
            root = PathBuf::from(dir);
        } else if let Some(name) = arg.strip_prefix("--") {
            let Some(value) = value(&mut i) else {
                eprintln!("Benchmark argument {name} requires an argument");
                print_help();
                return 1;
            };
            if name == "timed-runs" {
                match value.parse::<usize>() {
                    Ok(0) => {
                        eprintln!("Benchmark argument timed-runs must be greater than 0");
                        return 1;
                    }
                    Ok(n) => config.timed_runs = n,
                    Err(_) => {
                        eprintln!("Invalid value for benchmark argument timed-runs: {value}");
                        return 1;
                    }
                }
            } else {
                config.arguments.insert(name.to_string(), value);
            }
        } else if pattern.is_some() {
            eprintln!("Only one benchmark can be specified.");
            print_help();
            return 1;
        } else {
            pattern = Some(arg.clone());
        }
        i += 1;
    }
    let engines = match engines(&engine_name) {
        Ok(engines) => engines,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    // the benchmark files name their queries and answers relative to the checkout
    if let Err(error) = std::env::set_current_dir(&root) {
        eprintln!("Could not use {} as the root directory: {error}", root.display());
        return 1;
    }
    let _ = std::fs::create_dir_all(benchmark::BENCHMARK_DIRECTORY);
    let benchmarks = match discover(pattern.as_deref(), &config.arguments) {
        Ok(benchmarks) => benchmarks,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    if list {
        for benchmark in &benchmarks {
            println!("{}", benchmark.path);
        }
        return 0;
    }
    if benchmarks.is_empty() {
        eprintln!("No benchmark matches {}", pattern.unwrap_or_default());
        return 1;
    }
    if info || print_query {
        for benchmark in &benchmarks {
            if info {
                println!(
                    "display_name:{}\ngroup:{}\nsubgroup:{}",
                    benchmark.name(),
                    benchmark.group(),
                    benchmark.subgroup
                );
            } else {
                println!("{}", benchmark.run_query());
            }
        }
        return 0;
    }
    eprintln!("name\trun\ttiming");
    let mut results: Vec<(String, Vec<Outcome>)> = Vec::new();
    let mut failures = Vec::new();
    for benchmark in &benchmarks {
        let mut outcomes = Vec::new();
        for engine in &engines {
            let label = if engines.len() > 1 {
                format!("{} [{}]", benchmark.path, engine.name())
            } else {
                benchmark.path.clone()
            };
            let outcome = run_benchmark(benchmark, engine.as_ref(), &config);
            let mut lines = Vec::new();
            match &outcome {
                Outcome::Timings(times) => {
                    for (run, time) in times.iter().enumerate() {
                        lines.push(format!("{label}\t{}\t{:.6}", run + 1, time.as_secs_f64()));
                    }
                }
                Outcome::Error(error) => {
                    lines.push(format!("{label}\t1\tERROR"));
                    failures.push(format!("{label}\n{error}"));
                }
                Outcome::Incorrect(error) => {
                    lines.push(format!("{label}\t1\tINCORRECT"));
                    failures.push(format!("{label}\nINCORRECT RESULT: {error}"));
                }
                Outcome::Timeout => lines.push(format!("{label}\t1\tTIMEOUT")),
            }
            for line in lines {
                eprintln!("{line}");
                if let Some(out) = &mut out {
                    let _ = writeln!(out, "{line}");
                }
            }
            outcomes.push(outcome);
        }
        results.push((benchmark.name().to_string(), outcomes));
    }
    if summary && !failures.is_empty() {
        eprintln!("\n====================================================");
        eprintln!("================  FAILURES SUMMARY  ================");
        eprintln!("====================================================\n");
        for (i, failure) in failures.iter().enumerate() {
            eprintln!("{}: {failure}\n", i + 1);
        }
    }
    if engines.len() == 2 {
        print_comparison(&results);
    }
    i32::from(!failures.is_empty())
}

/// The median of each benchmark on both engines, their ratio, and the gate.
fn print_comparison(results: &[(String, Vec<Outcome>)]) {
    let seconds = |d: Option<Duration>| d.map_or_else(|| "-".to_string(), |d| format!("{:.4}", d.as_secs_f64()));
    println!(
        "\n{:<40} {:>12} {:>12} {:>8}",
        "benchmark", "duckrs (s)", "C++ (s)", "ratio"
    );
    let mut ratios = Vec::new();
    let mut by_name: HashMap<&str, f64> = HashMap::new();
    for (name, outcomes) in results {
        let ours = outcomes[0].median();
        let theirs = outcomes[1].median();
        let ratio = match (ours, theirs) {
            (Some(a), Some(b)) if b.as_secs_f64() > 0.0 => Some(a.as_secs_f64() / b.as_secs_f64()),
            _ => None,
        };
        if let Some(ratio) = ratio {
            ratios.push(ratio);
            by_name.insert(name, ratio);
        }
        println!(
            "{name:<40} {:>12} {:>12} {:>8}",
            seconds(ours),
            seconds(theirs),
            ratio.map_or_else(|| "-".to_string(), |r| format!("{r:.2}x"))
        );
    }
    let Some(mean) = geometric_mean(&ratios) else { return };
    let worst = by_name.iter().max_by(|a, b| a.1.total_cmp(b.1));
    println!(
        "\ngeometric mean of the ratios: {mean:.2}x over {} benchmarks",
        ratios.len()
    );
    if let Some((name, ratio)) = worst {
        println!("slowest relative to C++: {name} at {ratio:.2}x");
        let passes = mean <= GATE_GEOMETRIC_MEAN && *ratio <= GATE_WORST;
        println!(
            "v1 gate (geometric mean <= {GATE_GEOMETRIC_MEAN}x, none slower than {GATE_WORST}x): {}",
            if passes { "PASS" } else { "FAIL" }
        );
    }
}
