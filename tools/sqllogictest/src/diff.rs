//! Differential testing: an [`Engine`] that runs every statement on two engines and reports where
//! their results differ. Wrapping the engine keeps the whole runner (loops, `restart`,
//! `concurrentloop`, `require`) working unchanged, and the test's own expectations still apply to
//! the primary engine.
//!
//! The plan's two checks are both modes of this: duckrs against the C++ library, and duckrs against
//! itself with the optimizer off (DuckDB's `enable_verification` compares the same way).

use crate::engine::{ColumnType, Connection, Database, Engine, QueryOutput};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a comparison run needs besides the two engines.
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Runs on every connection of the second engine.
    pub setup: Option<&'static str>,
    /// The second engine may fail where the first one does not: the optimizer avoids errors.
    pub allow_secondary_errors: bool,
    /// Appends a line per difference here; otherwise differences go to standard error.
    pub out: Option<PathBuf>,
    /// The test file, for the report.
    pub label: String,
}

pub struct DiffEngine {
    primary: Box<dyn Engine>,
    secondary: Box<dyn Engine>,
    options: Arc<DiffOptions>,
    name: String,
}

impl DiffEngine {
    pub fn new(primary: Box<dyn Engine>, secondary: Box<dyn Engine>, options: DiffOptions) -> Self {
        let name = format!("{} vs {}", primary.name(), secondary.name());
        Self {
            primary,
            secondary,
            options: Arc::new(options),
            name,
        }
    }
}

impl Engine for DiffEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn open(&self, path: Option<&Path>, read_only: bool) -> Result<Box<dyn Database>, String> {
        // the engines cannot share a file
        let second_path = path.map(|p| {
            let mut name = p.as_os_str().to_owned();
            name.push(".difftest");
            PathBuf::from(name)
        });
        Ok(Box::new(DiffDatabase {
            primary: self.primary.open(path, read_only)?,
            secondary: self.secondary.open(second_path.as_deref(), read_only)?,
            options: self.options.clone(),
        }))
    }
}

struct DiffDatabase {
    primary: Box<dyn Database>,
    secondary: Box<dyn Database>,
    options: Arc<DiffOptions>,
}

impl Database for DiffDatabase {
    fn connect(&self) -> Result<Box<dyn Connection>, String> {
        let mut secondary = self.secondary.connect()?;
        if let Some(setup) = self.options.setup {
            secondary.query(setup)?;
        }
        Ok(Box::new(DiffConnection {
            primary: self.primary.connect()?,
            secondary,
            options: self.options.clone(),
        }))
    }
}

struct DiffConnection {
    primary: Box<dyn Connection>,
    secondary: Box<dyn Connection>,
    options: Arc<DiffOptions>,
}

impl Connection for DiffConnection {
    fn query(&mut self, sql: &str) -> Result<QueryOutput, String> {
        let first = self.primary.query(sql);
        let second = self.secondary.query(sql);
        if let Some(difference) = compare(sql, &first, &second, self.options.allow_secondary_errors) {
            report(&self.options, sql, &difference);
        }
        first
    }

    fn load_extension(&mut self, name: &str) -> bool {
        let loaded = self.primary.load_extension(name);
        self.secondary.load_extension(name);
        loaded
    }
}

/// Text that makes two runs of a statement differ for reasons other than a bug.
const UNCOMPARABLE: &[&str] = &[
    "random(", "uuid", "now(", "current_", "today(", "nextval", "currval", "setseed", "sample", "version(", "explain",
    "pragma", "duckdb_", "pg_", "getenv",
];

/// The difference between two outcomes of `sql`, if there is one.
pub fn compare(
    sql: &str,
    first: &Result<QueryOutput, String>,
    second: &Result<QueryOutput, String>,
    allow_secondary_errors: bool,
) -> Option<String> {
    let lower = sql.to_lowercase();
    if UNCOMPARABLE.iter().any(|text| lower.contains(text)) {
        return None;
    }
    match (first, second) {
        (Err(_), Err(_)) => None,
        (Ok(_), Err(_)) if allow_secondary_errors => None,
        (Ok(_), Err(error)) => Some(format!("only the second engine failed: {}", one_line(error))),
        (Err(error), Ok(_)) => Some(format!("only the first engine failed: {}", one_line(error))),
        (Ok(first), Ok(second)) => compare_rows(&lower, first, second),
    }
}

fn compare_rows(lower_sql: &str, first: &QueryOutput, second: &QueryOutput) -> Option<String> {
    if first.column_count() != second.column_count() {
        return Some(format!(
            "{} columns against {}",
            first.column_count(),
            second.column_count()
        ));
    }
    if first.row_count() != second.row_count() {
        return Some(format!("{} rows against {}", first.row_count(), second.row_count()));
    }
    // without ORDER BY any order is right, and a LIMIT may keep any of the rows
    let ordered = lower_sql.contains("order by");
    if lower_sql.contains("limit") && !ordered {
        return None;
    }
    let sorted = |output: &QueryOutput| {
        let mut rows = output.rows.clone();
        if !ordered {
            rows.sort();
        }
        rows
    };
    let (a, b) = (sorted(first), sorted(second));
    for (row, (a, b)) in a.iter().zip(&b).enumerate() {
        for (column, (a, b)) in a.iter().zip(b).enumerate() {
            if !cells_equal(a.as_deref(), b.as_deref(), first.types[column]) {
                return Some(format!("row {row} column {column}: {a:?} against {b:?}"));
            }
        }
    }
    None
}

fn cells_equal(a: Option<&str>, b: Option<&str>, ty: ColumnType) -> bool {
    if a == b {
        return true;
    }
    if !matches!(ty, ColumnType::Float | ColumnType::Double) {
        return false;
    }
    // the sum of floats depends on the order of the rows
    match (
        a.and_then(|a| a.parse::<f64>().ok()),
        b.and_then(|b| b.parse::<f64>().ok()),
    ) {
        (Some(a), Some(b)) => (a - b).abs() <= a.abs().max(b.abs()) * 1e-6,
        _ => false,
    }
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\t'], " ")
}

fn report(options: &DiffOptions, sql: &str, difference: &str) {
    let line = format!("{}\t{}\t{}\n", options.label, difference, one_line(sql));
    match &options.out {
        Some(path) => {
            // one write of one line, so that the processes of a run do not interleave
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = file.write_all(line.as_bytes());
            }
        }
        None => eprint!("DIFF {line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(rows: &[&str]) -> Result<QueryOutput, String> {
        Ok(QueryOutput {
            names: vec!["a".into()],
            types: vec![ColumnType::Double],
            rows: rows.iter().map(|r| vec![Some(r.to_string())]).collect(),
        })
    }

    #[test]
    fn order_only_counts_with_order_by() {
        let (a, b) = (output(&["1", "2"]), output(&["2", "1"]));
        assert_eq!(compare("SELECT a FROM t", &a, &b, false), None);
        assert!(compare("SELECT a FROM t ORDER BY a", &a, &b, false).is_some());
    }

    #[test]
    fn floats_get_a_tolerance_and_row_counts_do_not() {
        assert_eq!(
            compare("SELECT 1", &output(&["1.0"]), &output(&["1.00000001"]), false),
            None
        );
        assert!(compare("SELECT 1", &output(&["1"]), &output(&["1", "2"]), false).is_some());
    }

    #[test]
    fn errors_of_the_second_engine_can_be_allowed() {
        let (ok, error) = (output(&["1"]), Err("Conversion Error".to_string()));
        assert_eq!(compare("SELECT 1", &ok, &error, true), None);
        assert!(compare("SELECT 1", &ok, &error, false).is_some());
        assert!(compare("SELECT 1", &error, &ok, true).is_some());
    }

    #[test]
    fn volatile_statements_are_not_compared() {
        assert_eq!(
            compare("SELECT random()", &output(&["1"]), &output(&["2"]), false),
            None
        );
    }
}
