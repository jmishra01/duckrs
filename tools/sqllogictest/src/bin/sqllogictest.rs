use clap::{Parser, ValueEnum};
use sqllogictest::diff::{DiffEngine, DiffOptions};
use sqllogictest::engine::{Engine, PintailEngine};
use sqllogictest::report;
use sqllogictest::runner::{self, FileReport, Outcome, RunConfig};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EngineKind {
    /// The Rust engine under development.
    Pintail,
    /// The C++ DuckDB library (requires the `reference` cargo feature).
    Reference,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DiffKind {
    /// pintail with the optimizer off, like `PRAGMA enable_verification`.
    Unoptimized,
    /// The C++ DuckDB library (requires the `reference` cargo feature).
    Reference,
}

/// Runs DuckDB's sqllogictest corpus against pintail or the C++ reference engine.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// Test files, directories, or substrings to select (relative to the DuckDB checkout).
    /// Defaults to every `.test` file under `test/`.
    patterns: Vec<String>,

    #[arg(long, value_enum, default_value = "pintail")]
    engine: EngineKind,

    /// Path to the DuckDB checkout that holds the test corpus and its data files.
    #[arg(long, default_value = "../duckdb")]
    duckdb_root: PathBuf,

    /// Also run `.test_slow` files.
    #[arg(long)]
    slow: bool,

    /// Number of files to run in parallel.
    #[arg(long, short)]
    jobs: Option<usize>,

    /// Keep running a file after its first failure (gives better statement counts).
    #[arg(long)]
    continue_on_failure: bool,

    /// Print the failure message of every failing file.
    #[arg(long, short)]
    verbose: bool,

    /// Write a per-directory pass-rate table (markdown) to this path.
    #[arg(long)]
    status_out: Option<PathBuf>,

    /// Write the sorted list of passing files to this path.
    #[arg(long)]
    passing_out: Option<PathBuf>,

    /// Fail if any file listed here (one path per line) no longer passes.
    #[arg(long)]
    known_passing: Option<PathBuf>,

    /// Exit successfully even if tests fail (regressions against --known-passing still fail).
    #[arg(long)]
    allow_failures: bool,

    /// Run each file in a child process, so an engine crash or hang fails only that file.
    #[arg(long)]
    isolate: bool,

    /// Per-file time limit in seconds when isolating.
    #[arg(long, default_value_t = 300)]
    timeout: u64,

    /// Also run every statement on this engine and report where the results differ.
    #[arg(long, value_enum)]
    diff_against: Option<DiffKind>,

    /// Append the differences to this file, one per line, instead of printing them.
    #[arg(long)]
    diff_out: Option<PathBuf>,

    /// Internal: run exactly this file (index, path) and print its report for the parent.
    #[arg(long, hide = true, num_args = 2, value_names = ["INDEX", "FILE"])]
    worker: Option<Vec<String>>,
}

fn engine(kind: EngineKind) -> Result<(Box<dyn Engine>, String), String> {
    match kind {
        EngineKind::Pintail => Ok((
            Box::new(PintailEngine),
            format!("pintail {}", env!("CARGO_PKG_VERSION")),
        )),
        #[cfg(feature = "reference")]
        EngineKind::Reference => {
            use sqllogictest::engine::ReferenceEngine;
            Ok((
                Box::new(ReferenceEngine),
                format!("C++ DuckDB {}", ReferenceEngine::version()),
            ))
        }
        #[cfg(not(feature = "reference"))]
        EngineKind::Reference => {
            Err("the reference engine is not compiled in; rebuild with `--features reference`".into())
        }
    }
}

/// Wraps `primary` for `--diff-against`. Outside of a worker `label` is empty: a run without
/// `--isolate` reports differences without their file.
fn with_diff(
    args: &Args,
    primary: Box<dyn Engine>,
    out: Option<PathBuf>,
    label: &str,
) -> Result<Box<dyn Engine>, String> {
    let Some(kind) = args.diff_against else {
        return Ok(primary);
    };
    let (secondary, setup, allow_secondary_errors): (Box<dyn Engine>, _, _) = match kind {
        DiffKind::Unoptimized => (Box::new(PintailEngine), Some("PRAGMA disable_optimizer"), true),
        DiffKind::Reference => (engine(EngineKind::Reference)?.0, None, false),
    };
    let options = DiffOptions {
        setup,
        allow_secondary_errors,
        out,
        label: label.to_string(),
    };
    Ok(Box::new(DiffEngine::new(primary, secondary, options)))
}

fn baseline(root: &std::path::Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short=11", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn main() -> ExitCode {
    let mut args = Args::parse();
    let (engine, engine_version) = match engine(args.engine) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let root = match args.duckdb_root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("error: DuckDB checkout {} not found: {e}", args.duckdb_root.display());
            return ExitCode::from(2);
        }
    };
    // Output paths are relative to where the user ran us, not the DuckDB checkout.
    let cwd = std::env::current_dir().expect("current directory");
    let resolve = |p: &Option<PathBuf>| p.as_ref().map(|p| cwd.join(p));
    let diff_out = resolve(&args.diff_out);
    args.diff_out = diff_out.clone();
    let (status_out, passing_out, known_passing) = (
        resolve(&args.status_out),
        resolve(&args.passing_out),
        resolve(&args.known_passing),
    );

    if let Some(worker) = &args.worker {
        std::env::set_current_dir(&root).expect("enter DuckDB checkout");
        let scratch = std::env::temp_dir().join(format!("pintail-sqllogictest-{}", std::process::id()));
        let config = RunConfig {
            root: root.clone(),
            scratch: scratch.clone(),
            continue_on_failure: args.continue_on_failure,
            vector_size: 2048,
        };
        let index = worker[0].parse().unwrap_or(0);
        let engine = match with_diff(&args, engine, diff_out, &worker[1]) {
            Ok(engine) => engine,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        };
        let report = runner::run_file(&config, engine.as_ref(), &worker[1], index);
        let _ = std::fs::remove_dir_all(&scratch);
        println!("{RESULT_MARKER}{}", encode_report(&report));
        return ExitCode::SUCCESS;
    }

    let engine = match with_diff(&args, engine, diff_out, "") {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let files = sqllogictest::discover(&root, &args.patterns, args.slow);
    if files.is_empty() {
        eprintln!("error: no test files matched");
        return ExitCode::from(2);
    }
    // Test files reference data with paths relative to the DuckDB checkout.
    std::env::set_current_dir(&root).expect("enter DuckDB checkout");

    let config = RunConfig {
        root: root.clone(),
        scratch: std::env::temp_dir().join(format!("pintail-sqllogictest-{}", std::process::id())),
        continue_on_failure: args.continue_on_failure,
        vector_size: 2048,
    };
    let jobs = args
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    println!("Running {} files with {engine_version} ({jobs} jobs)", files.len());

    let start = Instant::now();
    let next = AtomicUsize::new(0);
    let reports: Mutex<Vec<FileReport>> = Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { break };
                    let r = if args.isolate {
                        run_isolated(&args, &root, file, i)
                    } else {
                        runner::run_file(&config, engine.as_ref(), file, i)
                    };
                    match &r.outcome {
                        Outcome::Fail { location, message } if args.verbose => {
                            println!("FAIL  {location}\n{}\n", indent(message));
                        }
                        Outcome::Error(message) if args.verbose => {
                            println!("ERROR {}\n{}\n", r.path, indent(message))
                        }
                        Outcome::Fail { .. } => println!("FAIL  {}", r.path),
                        Outcome::Error(_) => println!("ERROR {}", r.path),
                        _ => {}
                    }
                    reports.lock().unwrap().push(r);
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&config.scratch);
    // Workers killed for running too long, and runs killed before, leave theirs.
    sweep_scratch();
    let mut reports = reports.into_inner().unwrap();
    reports.sort_by(|a, b| a.path.cmp(&b.path));

    let totals = report::totals(&reports);
    println!(
        "\n{} files in {:.1}s: {} passed, {} failed, {} skipped, {} errors ({:.1}% of files that ran)",
        totals.files,
        start.elapsed().as_secs_f64(),
        totals.pass,
        totals.fail,
        totals.skip,
        totals.error,
        totals.pass_rate()
    );
    println!(
        "{} statements executed, {} passed",
        totals.executed, totals.passed_statements
    );

    let write = |path: &PathBuf, contents: String| {
        if let Err(e) = std::fs::write(path, contents) {
            eprintln!("error: cannot write {}: {e}", path.display());
        }
    };
    if let Some(path) = &status_out {
        write(
            path,
            report::status_markdown(&engine_version, &baseline(&root), &reports),
        );
    }
    if let Some(path) = &passing_out {
        write(path, report::passing_list(&reports));
    }
    if let Some(path) = &known_passing {
        let known = match std::fs::read_to_string(path) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", path.display());
                return ExitCode::from(2);
            }
        };
        let regressions = report::regressions(&known, &reports);
        if !regressions.is_empty() {
            println!(
                "\n{} regressions (listed in {} but not passing):",
                regressions.len(),
                path.display()
            );
            for r in regressions {
                println!("  {r}");
            }
            return ExitCode::FAILURE;
        }
    }
    if !args.allow_failures && totals.fail + totals.error > 0 {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

const RESULT_MARKER: &str = "@@sqllogictest-result\t";

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// One tab-separated line: outcome, counters, failure location, and message.
fn encode_report(r: &FileReport) -> String {
    let (file, line, message) = match &r.outcome {
        Outcome::Pass => (String::new(), 0, String::new()),
        Outcome::Fail { location, message } => (location.file.to_string(), location.line, message.clone()),
        Outcome::Skip(m) | Outcome::Error(m) => (String::new(), 0, m.clone()),
    };
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        r.outcome.label(),
        r.executed,
        r.passed,
        r.mode_skipped,
        escape(&file),
        line,
        escape(&message)
    )
}

fn decode_report(path: &str, line: &str, duration: Duration) -> Option<FileReport> {
    let f: Vec<&str> = line.splitn(7, '\t').collect();
    let [label, executed, passed, mode_skipped, file, line, message] = f.as_slice() else {
        return None;
    };
    let message = unescape(message);
    let outcome = match *label {
        "pass" => Outcome::Pass,
        "fail" => Outcome::Fail {
            location: sqllogictest::parser::Location {
                file: unescape(file).into(),
                line: line.parse().ok()?,
            },
            message,
        },
        "skip" => Outcome::Skip(message),
        _ => Outcome::Error(message),
    };
    Some(FileReport {
        path: path.to_string(),
        outcome,
        executed: executed.parse().ok()?,
        passed: passed.parse().ok()?,
        mode_skipped: mode_skipped.parse().ok()?,
        duration,
    })
}

/// Runs one file in a child process of this binary, killing it after `--timeout`.
fn run_isolated(args: &Args, root: &std::path::Path, file: &str, index: usize) -> FileReport {
    let start = Instant::now();
    let error = |message: String| FileReport {
        path: file.to_string(),
        outcome: Outcome::Error(message),
        executed: 0,
        passed: 0,
        mode_skipped: 0,
        duration: start.elapsed(),
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return error(format!("cannot locate own executable: {e}")),
    };
    let mut command = Command::new(exe);
    command
        .arg("--engine")
        .arg(args.engine.to_possible_value().expect("engine name").get_name())
        .arg("--duckdb-root")
        .arg(root)
        .arg("--worker")
        .arg(index.to_string())
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if args.continue_on_failure {
        command.arg("--continue-on-failure");
    }
    if let Some(kind) = args.diff_against {
        command
            .arg("--diff-against")
            .arg(kind.to_possible_value().expect("diff name").get_name());
    }
    if let Some(path) = &args.diff_out {
        command.arg("--diff-out").arg(path);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return error(format!("cannot start worker: {e}")),
    };
    // Drain the pipes on threads so a chatty child cannot block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = start + Duration::from_secs(args.timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return error(format!("waiting for worker failed: {e}")),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    let Some(status) = status else {
        return error(format!("timed out after {}s", args.timeout));
    };
    match out.lines().find_map(|l| l.strip_prefix(RESULT_MARKER)) {
        Some(line) => decode_report(file, line, start.elapsed())
            .unwrap_or_else(|| error(format!("malformed worker output: {line}"))),
        None => {
            let tail: Vec<&str> = err.lines().rev().take(15).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            error(format!("worker crashed ({status})\n{}", tail.join("\n")))
        }
    }
}

/// Removes the `pintail-sqllogictest-{pid}` directories whose process is gone.
fn sweep_scratch() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|name| name.strip_prefix("pintail-sqllogictest-"))
            .and_then(|pid| pid.parse::<libc::pid_t>().ok());
        let gone = pid.is_some_and(|pid| {
            // SAFETY: signal 0 only asks whether the process exists.
            let found = unsafe { libc::kill(pid, 0) } == 0;
            !found && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
        if gone {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_round_trips_through_worker_encoding() {
        let report = FileReport {
            path: "test/x.test".into(),
            outcome: Outcome::Fail {
                location: sqllogictest::parser::Location {
                    file: "test/x.test".into(),
                    line: 7,
                },
                message: "a\tb\nc\\d".into(),
            },
            executed: 3,
            passed: 2,
            mode_skipped: 1,
            duration: Duration::ZERO,
        };
        let decoded = decode_report("test/x.test", &encode_report(&report), Duration::ZERO).unwrap();
        assert_eq!(decoded.outcome, report.outcome);
        assert_eq!((decoded.executed, decoded.passed, decoded.mode_skipped), (3, 2, 1));
    }
}
