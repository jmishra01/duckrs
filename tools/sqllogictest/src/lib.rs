//! A Rust port of DuckDB's sqllogictest runner (`duckdb/test/sqlite`).
//!
//! It runs the `.test` corpus from a DuckDB checkout against any [`engine::Engine`]: pintail
//! itself, or (with the `reference` feature) the C++ DuckDB library, which validates the
//! harness and serves as the oracle for differential testing.

pub mod compare;
pub mod diff;
pub mod engine;
pub mod parser;
pub mod report;
pub mod runner;

use std::path::Path;
use walkdir::WalkDir;

/// Finds test files under `root` matching any of `patterns` (path prefixes or substrings,
/// relative to `root`). With no patterns, finds every `.test` file under `test/`.
pub fn discover(root: &Path, patterns: &[String], include_slow: bool) -> Vec<String> {
    let is_test = |p: &str| p.ends_with(".test") || (include_slow && p.ends_with(".test_slow"));
    let search = |dir: &Path| -> Vec<String> {
        WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                e.path()
                    .strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
            })
            .filter(|rel| is_test(rel))
            .collect()
    };
    if patterns.is_empty() {
        return search(&root.join("test"));
    }
    let mut corpus: Option<Vec<String>> = None;
    let mut files = Vec::new();
    for pattern in patterns {
        let path = root.join(pattern);
        if path.is_file() {
            files.push(pattern.clone());
        } else if path.is_dir() {
            files.extend(search(&path));
        } else {
            // Treat as a substring filter over the whole corpus.
            let corpus = corpus.get_or_insert_with(|| search(&root.join("test")));
            files.extend(corpus.iter().filter(|f| f.contains(pattern.as_str())).cloned());
        }
    }
    files.sort();
    files.dedup();
    files
}
