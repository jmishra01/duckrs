// Ported from duckdb/benchmark/interpreted_benchmark.cpp @ rust-port-baseline
//
//! The `.benchmark` file format (`InterpretedBenchmark::ProcessFile`): the queries a benchmark loads
//! and runs, the result it checks, and the templates, includes and arguments that build it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;

/// A result to check (`BenchmarkQuery`): the query whose rows are compared (`None` is the run's own
/// result, `select * from __answer` in C++), and the expected rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectedResult {
    pub query: Option<String>,
    pub column_count: usize,
    pub rows: Vec<Vec<String>>,
}

/// How the result of the run query is fetched (`BenchmarkResultMode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResultMode {
    #[default]
    Materialized,
    Streaming,
    Arrow,
}

/// A parsed benchmark (`InterpretedBenchmark`).
#[derive(Debug, Clone, Default)]
pub struct Benchmark {
    /// the path it was found at, relative to the root (`name` in C++)
    pub path: String,
    pub display_name: String,
    pub display_group: String,
    pub subgroup: String,
    /// `load`, `run`, `init`, `cleanup` and `reload`, by name
    pub queries: BTreeMap<String, String>,
    pub extensions: Vec<String>,
    pub load_only_extensions: Vec<String>,
    pub result: Option<ExpectedResult>,
    pub asserts: Vec<ExpectedResult>,
    pub cache_db: Option<String>,
    pub cache_file: Option<String>,
    pub cache_no_connect: bool,
    pub in_memory: bool,
    pub storage_version: Option<String>,
    pub require_reinit: bool,
    pub retry_load: usize,
    pub result_mode: ResultMode,
    /// `${NAME}` replacements: the template parameters and the arguments
    pub replacements: HashMap<String, String>,
}

impl Benchmark {
    /// The query that is timed.
    pub fn run_query(&self) -> &str {
        self.queries.get("run").map(String::as_str).unwrap_or_default()
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.queries.get(name).map(String::as_str)
    }

    /// `DisplayName`: the `name` given, or the file's path.
    pub fn name(&self) -> &str {
        if self.display_name.is_empty() {
            &self.path
        } else {
            &self.display_name
        }
    }

    /// `Group`: the `group` given, or the directory the file is in under `benchmark/`.
    pub fn group(&self) -> String {
        if !self.display_group.is_empty() {
            return self.display_group.clone();
        }
        // `ParseGroupFromPath`: the directory after `benchmark/`
        let mut parts = self.path.split('/');
        match (parts.next(), parts.next()) {
            (Some("benchmark"), Some(group)) if !group.ends_with(".benchmark") => group.to_string(),
            _ => String::new(),
        }
    }
}

/// `ProcessReplacements`.
fn replace(text: &str, replacements: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (name, value) in replacements {
        result = result.replace(&format!("${{{name}}}"), value);
    }
    result
}

/// `BenchmarkFileReader`: the lines of a file, with the replacements made and trimmed.
struct Reader {
    path: String,
    lines: Vec<String>,
    next: usize,
}

impl Reader {
    fn open(path: &str) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("Could not open benchmark file {path}: {e}"))?;
        Ok(Self {
            path: path.to_string(),
            lines: text.lines().map(str::to_string).collect(),
            next: 0,
        })
    }

    fn read_line(&mut self, replacements: &HashMap<String, String>) -> Option<String> {
        let line = self.lines.get(self.next)?;
        self.next += 1;
        Some(replace(line, replacements).trim().to_string())
    }

    fn error(&self, message: &str) -> String {
        format!("{}:{} - {message}", self.path, self.next)
    }
}

/// The parsing state of one benchmark (`InterpretedBenchmark` while it loads).
struct Parser<'a> {
    benchmark: Benchmark,
    arguments: &'a HashMap<String, String>,
    handled_arguments: HashSet<String>,
}

/// Reads the benchmark at `path` (relative to the current directory, the DuckDB root), with the
/// values of its `argument`s given in `arguments` (`--name value` on the command line).
pub fn load(path: &str, arguments: &HashMap<String, String>) -> Result<Benchmark, String> {
    let mut parser = Parser {
        benchmark: Benchmark {
            path: path.to_string(),
            in_memory: true,
            ..Benchmark::default()
        },
        arguments,
        handled_arguments: HashSet::new(),
    };
    parser
        .benchmark
        .replacements
        .insert("BENCHMARK_DIR".to_string(), crate::BENCHMARK_DIRECTORY.to_string());
    parser.process_file(path)?;
    // `LoadBenchmark`: an argument given that the benchmark does not have is an error
    for name in arguments.keys() {
        if !parser.handled_arguments.contains(name) {
            return Err(format!(
                "Invalid benchmark argument {name}: argument was not specified in benchmark {path}"
            ));
        }
    }
    if !parser.benchmark.queries.contains_key("run") {
        return Err("Invalid benchmark file: no \"run\" query specified".to_string());
    }
    Ok(parser.benchmark)
}

impl Parser<'_> {
    /// `InterpretedBenchmark::ProcessFile`.
    fn process_file(&mut self, path: &str) -> Result<(), String> {
        let mut reader = Reader::open(path)?;
        while let Some(line) = reader.read_line(&self.benchmark.replacements) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lower = line.to_lowercase();
            let splits: Vec<&str> = lower.split(' ').collect();
            let command = splits[0];
            match command {
                "load" | "run" | "init" | "cleanup" | "reload" => {
                    if self.benchmark.queries.contains_key(command) {
                        return Err(format!("Multiple calls to {command} in the same benchmark file"));
                    }
                    let mut query = String::new();
                    while let Some(line) = reader.read_line(&self.benchmark.replacements) {
                        if line.is_empty() {
                            break;
                        }
                        query += &line;
                        query.push(' ');
                    }
                    if let Some(file) = splits.get(1).filter(|f| !f.is_empty()) {
                        query = fs::read_to_string(file)
                            .map_err(|_| format!("Failed to read {command} from file {file}"))?;
                    }
                    let query = query.trim().to_string();
                    if query.is_empty() {
                        return Err(format!("Encountered an empty {command} node!"));
                    }
                    self.benchmark.queries.insert(command.to_string(), query);
                }
                "require" => {
                    if !(2..=3).contains(&splits.len()) {
                        return Err(reader.error("require requires a single parameter"));
                    }
                    let list = if splits.len() == 3 {
                        if splits[2] != "load_only" {
                            return Err(reader.error("require only supports load_only as a second parameter"));
                        }
                        &mut self.benchmark.load_only_extensions
                    } else {
                        &mut self.benchmark.extensions
                    };
                    if !list.iter().any(|e| e == splits[1]) {
                        list.push(splits[1].to_string());
                    }
                }
                "resultmode" => {
                    self.benchmark.result_mode = match splits.get(1).copied() {
                        Some("streaming") => ResultMode::Streaming,
                        Some("arrow" | "arrow_stream") => ResultMode::Arrow,
                        Some("materialized") => ResultMode::Materialized,
                        _ => {
                            return Err(reader.error(
                                "Invalid argument for resultmode, valid options are: streaming, arrow, \
                                 arrow_stream, materialized",
                            ));
                        }
                    };
                }
                "cache" => {
                    let file = match splits.as_slice() {
                        [_, file] => file.to_string(),
                        [_, file, "no_connect"] => {
                            self.benchmark.cache_no_connect = true;
                            file.to_string()
                        }
                        _ => return Err(reader.error("cache requires a db file, and optionally a no_connect")),
                    };
                    let file = replace(&file, &self.benchmark.replacements);
                    if file.ends_with(".csv") || file.ends_with(".parquet") || file.ends_with(".csv.gz") {
                        self.benchmark.cache_file = Some(file);
                        self.benchmark.cache_db = None;
                    } else {
                        self.benchmark.cache_db = Some(file);
                    }
                }
                "cache_file" => {
                    let [_, file] = splits.as_slice() else {
                        return Err(reader.error("cache_file requires a single file"));
                    };
                    self.benchmark.cache_file = Some(replace(file, &self.benchmark.replacements));
                }
                "storage" => {
                    match splits.get(1).copied() {
                        Some("transient") => self.benchmark.in_memory = true,
                        Some("persistent") => self.benchmark.in_memory = false,
                        Some(_) => return Err(reader.error("Invalid argument for storage")),
                        None => return Err(reader.error("storage requires at least one parameter")),
                    }
                    if let Some(version) = splits.get(2) {
                        self.benchmark.storage_version = Some(version.to_string());
                    }
                }
                "require_reinit" => {
                    if splits.len() != 1 {
                        return Err(reader.error("require_reinit does not take any parameters"));
                    }
                    self.benchmark.require_reinit = true;
                }
                "name" | "group" | "subgroup" => {
                    if splits.len() == 1 {
                        return Err(reader.error(&format!("{command} requires a parameter")));
                    }
                    // the original case of the rest of the line
                    let value = line[command.len() + 1..].trim().to_string();
                    match command {
                        "name" => self.benchmark.display_name = value,
                        "group" => self.benchmark.display_group = value,
                        _ => self.benchmark.subgroup = value,
                    }
                }
                "assert" => {
                    let columns = splits
                        .get(1)
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| reader.error("assert must be followed by a column count (e.g. result III)"))?;
                    let sql = self.read_query(&mut reader)?;
                    let rows = self.read_rows(&mut reader, columns.len())?;
                    self.benchmark.asserts.push(ExpectedResult {
                        query: Some(sql),
                        column_count: columns.len(),
                        rows,
                    });
                }
                "result_query" | "result" => self.process_result(&mut reader, &splits)?,
                "retry" => {
                    if splits.len() != 3 {
                        return Err(reader.error(&format!("{command} requires two parameters")));
                    }
                    if splits[1] != "load" {
                        return Err("Only retry load is supported".to_string());
                    }
                    self.benchmark.retry_load = splits[2].parse().map_err(|_| reader.error("retry count"))?;
                }
                "template" => {
                    let template = splits
                        .get(1)
                        .ok_or_else(|| reader.error("template requires a file"))?
                        .to_string();
                    while let Some(line) = reader.read_line(&self.benchmark.replacements) {
                        if line.is_empty() {
                            break;
                        }
                        let parameters: Vec<&str> = line.split('=').collect();
                        let [name, value] = parameters.as_slice() else {
                            return Err(reader.error("Expected a template parameter in the form of X=Y"));
                        };
                        self.benchmark.replacements.insert(name.to_string(), value.to_string());
                    }
                    // the rest comes from the template
                    return self.process_file(&template);
                }
                "argument" => {
                    let [_, name, default] = splits.as_slice() else {
                        return Err(reader.error(&format!("{command} requires two parameters (name and default)")));
                    };
                    if !self.handled_arguments.insert(name.to_string()) {
                        // the argument is defined already
                        continue;
                    }
                    let value = self
                        .arguments
                        .get(*name)
                        .cloned()
                        .unwrap_or_else(|| default.to_string());
                    self.benchmark.replacements.insert(name.to_string(), value);
                }
                "include" => {
                    let [_, file] = splits.as_slice() else {
                        return Err("include requires a single argument".to_string());
                    };
                    self.process_file(file)?;
                }
                other => return Err(reader.error(&format!("unrecognized command {other}"))),
            }
        }
        Ok(())
    }

    /// The query of a `result_query` or `assert`, up to the `----`.
    fn read_query(&self, reader: &mut Reader) -> Result<String, String> {
        let mut sql = String::new();
        while let Some(line) = reader.read_line(&self.benchmark.replacements) {
            if line == "----" {
                return Ok(sql);
            }
            sql.push('\n');
            sql += &line;
        }
        Err(reader.error("result_query must be followed by a query and a result (separated by ----)"))
    }

    /// `ReadQueryFromReader`: tab-separated rows up to a blank line.
    fn read_rows(&self, reader: &mut Reader, column_count: usize) -> Result<Vec<Vec<String>>, String> {
        let mut rows = Vec::new();
        while let Some(line) = reader.read_line(&self.benchmark.replacements) {
            if line.is_empty() {
                break;
            }
            let values: Vec<String> = line.split('\t').map(str::to_string).collect();
            if values.len() != column_count {
                return Err(reader.error(&format!("expected {} values but got {column_count}", values.len())));
            }
            rows.push(values);
        }
        Ok(rows)
    }

    /// `result` and `result_query`: rows given inline (`result III`) or in a file, maybe only when
    /// the arguments match (`result answers/sf1/q01.csv sf=1`).
    fn process_result(&mut self, reader: &mut Reader, splits: &[&str]) -> Result<(), String> {
        let spec = splits
            .get(1)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| reader.error("result must be followed by a column count (e.g. result III)"))?;
        let is_file = spec.chars().any(|c| c != 'i');
        let mut matches = true;
        for condition in &splits[2..] {
            let parts: Vec<&str> = condition.split('=').collect();
            let [name, value] = parts.as_slice() else {
                return Err("result with condition must have one equality".to_string());
            };
            match self.benchmark.replacements.get(*name) {
                None => return Err(format!("Condition argument {name} not found in benchmark")),
                Some(actual) if actual != value => {
                    matches = false;
                    break;
                }
                Some(_) => {}
            }
        }
        let query = if splits[0] == "result_query" {
            Some(self.read_query(reader)?)
        } else {
            None
        };
        let expected = if is_file {
            if !matches {
                return Ok(());
            }
            let file = replace(spec, &self.benchmark.replacements);
            let rows = read_answer_file(&file)?;
            ExpectedResult {
                query,
                column_count: rows.first().map_or(0, Vec::len),
                rows,
            }
        } else {
            ExpectedResult {
                query,
                column_count: spec.len(),
                rows: self.read_rows(reader, spec.len())?,
            }
        };
        if matches {
            if self.benchmark.result.is_some() {
                return Err(reader.error("multiple results found"));
            }
            self.benchmark.result = Some(expected);
        }
        Ok(())
    }
}

/// `ReadQueryFromFile`: the rows of an answer file, read as `read_csv(file, delim='|', header=1,
/// nullstr='NULL', all_varchar=1, quote='"', escape='"')` reads them.
pub fn read_answer_file(path: &str) -> Result<Vec<Vec<String>>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("Failed to read answer file {path}: {e}"))?;
    let mut rows = parse_csv(&text);
    if !rows.is_empty() {
        // the header
        rows.remove(0);
    }
    Ok(rows)
}

/// Rows of `|`-separated values; a value in double quotes may hold `|`, newlines and `""`.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut value = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        any = true;
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    value.push('"');
                } else {
                    quoted = false;
                }
            } else {
                value.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '|' => row.push(std::mem::take(&mut value)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut value));
                rows.push(std::mem::take(&mut row));
                any = false;
            }
            c => value.push(c),
        }
    }
    if any {
        row.push(value);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_files_are_pipe_separated_with_quotes() {
        assert_eq!(
            parse_csv("a|b\n1|\"x|y\"\n2|\"say \"\"hi\"\"\"\n"),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["1".to_string(), "x|y".to_string()],
                vec!["2".to_string(), "say \"hi\"".to_string()],
            ]
        );
    }

    #[test]
    fn replacements_fill_in_names() {
        let mut map = HashMap::new();
        map.insert("sf".to_string(), "10".to_string());
        assert_eq!(replace("CALL dbgen(sf=${sf})", &map), "CALL dbgen(sf=10)");
    }
}
