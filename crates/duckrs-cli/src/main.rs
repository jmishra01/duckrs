// Ported from duckdb/tools/shell/shell.cpp @ rust-port-baseline
//
//! The duckrs shell: DuckDB's command-line interface over the duckrs engine.

mod box_renderer;
mod commands;
mod highlight;
mod renderers;
mod table_metadata;

use duckrs::{Connection, Database, Error, QueryResult};
use duckrs_parser::ast::Statement;
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The names, types and chunks of a result.
pub type LastRows = (Vec<String>, Vec<duckrs::LogicalType>, Arc<Vec<duckrs::DataChunk>>);

/// `RenderMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Line,
    Column,
    List,
    Semi,
    Html,
    Tcl,
    Csv,
    Explain,
    Ascii,
    Pretty,
    Box,
    Latex,
    Trash,
    Json,
    Markdown,
    Table,
    DuckBox,
    JsonLines,
    Insert,
    Quote,
}

impl RenderMode {
    /// `ModeToString`.
    pub fn name(self) -> &'static str {
        match self {
            RenderMode::Line => "line",
            RenderMode::Column => "column",
            RenderMode::List => "list",
            RenderMode::Semi => "semi",
            RenderMode::Html => "html",
            RenderMode::Tcl => "tcl",
            RenderMode::Csv => "csv",
            RenderMode::Explain => "explain",
            RenderMode::Ascii => "ascii",
            RenderMode::Pretty => "prettyprint",
            RenderMode::Box => "box",
            RenderMode::Latex => "latex",
            RenderMode::Trash => "trash",
            RenderMode::Json => "json",
            RenderMode::Markdown => "markdown",
            RenderMode::Table => "table",
            RenderMode::DuckBox => "duckbox",
            RenderMode::JsonLines => "jsonlines",
            RenderMode::Insert => "insert",
            RenderMode::Quote => "quote",
        }
    }
}

/// `BailOnError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bail {
    Automatic,
    Bail,
    DontBail,
}

/// `InputMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Standard,
    File,
    DuckDbRc,
}

/// What a dot command returns (`MetadataResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResult {
    Success,
    Fail,
    Exit,
    PrintUsage,
}

/// Where the shell's output goes.
pub enum Output {
    Stdout,
    File(File, String),
    Off,
}

/// `ShellState`.
pub struct Shell {
    pub db: Option<Database>,
    pub conn: Option<Connection>,
    pub db_filename: String,
    pub read_only: bool,
    /// `-storage-version`: the storage version a new database file is written in
    pub storage_version: Option<String>,
    pub mode: RenderMode,
    pub col_separator: String,
    pub row_separator: String,
    pub show_header: bool,
    pub header_set: bool,
    pub null_value: String,
    pub max_rows: usize,
    pub max_analyze_rows: Option<usize>,
    pub max_width: usize,
    pub col_width: Vec<i64>,
    pub columns: bool,
    pub large_number_rendering: Option<String>,
    pub decimal_separator: Option<char>,
    pub thousand_separator: Option<char>,
    pub dest_table: String,
    pub out: Output,
    /// `.once`: the output goes back to stdout after the next command
    pub out_count: usize,
    pub bail: Bail,
    pub echo: bool,
    /// `SHFLG_Newlines`: `.mode insert` writes newlines as they are
    pub newlines: bool,
    pub timer: bool,
    /// `timerDigits`: the decimals of the real time
    pub timer_digits: usize,
    /// the files `.read` is reading, innermost last
    pub active_read_files: Vec<String>,
    pub count_changes: bool,
    pub last_changes: i64,
    pub total_changes: i64,
    pub safe_mode: bool,
    pub stdin_is_interactive: bool,
    pub stdout_is_console: bool,
    pub last_result: Option<QueryResult>,
    /// the last result's rows, for the `_` replacement scan (`ShellScanLastResult`)
    pub last_rows: Arc<Mutex<Option<LastRows>>>,
    pub program_name: String,
    /// `ShellHighlight`'s settings
    pub highlighter: highlight::Highlighter,
    pub stderr_is_console: bool,
}

/// `SEP_*`.
pub const SEP_COLUMN: &str = "|";
pub const SEP_ROW: &str = "\n";
pub const SEP_TAB: &str = "\t";
pub const SEP_SPACE: &str = " ";
pub const SEP_COMMA: &str = ",";
pub const SEP_CRLF: &str = "\r\n";
pub const SEP_UNIT: &str = "\x1F";
pub const SEP_RECORD: &str = "\x1E";

impl Shell {
    fn new(program_name: String) -> Self {
        Self {
            db: None,
            conn: None,
            db_filename: String::new(),
            read_only: false,
            storage_version: None,
            mode: RenderMode::DuckBox,
            col_separator: SEP_COLUMN.to_string(),
            row_separator: SEP_ROW.to_string(),
            show_header: true,
            header_set: false,
            null_value: "NULL".to_string(),
            max_rows: 40,
            max_analyze_rows: None,
            max_width: 0,
            col_width: Vec::new(),
            columns: false,
            large_number_rendering: None,
            decimal_separator: None,
            thousand_separator: None,
            dest_table: "\"table\"".to_string(),
            out: Output::Stdout,
            out_count: 0,
            bail: Bail::Automatic,
            echo: false,
            newlines: false,
            timer: false,
            timer_digits: 3,
            active_read_files: Vec::new(),
            count_changes: false,
            last_changes: 0,
            total_changes: 0,
            safe_mode: false,
            stdin_is_interactive: std::io::stdin().is_terminal(),
            stdout_is_console: std::io::stdout().is_terminal(),
            last_result: None,
            last_rows: Arc::new(Mutex::new(None)),
            program_name,
            highlighter: {
                let mut highlighter = highlight::Highlighter::new();
                highlighter.apply_no_color(std::env::var_os("NO_COLOR").as_deref());
                highlighter
            },
            stderr_is_console: std::io::stderr().is_terminal(),
        }
    }

    /// Writes to the current output.
    pub fn print(&mut self, text: &str) {
        let result = match &mut self.out {
            Output::Stdout => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(text.as_bytes())
            }
            Output::File(file, _) => file.write_all(text.as_bytes()),
            Output::Off => Ok(()),
        };
        if result.is_err() {
            // a closed pipe ends the shell (`SIGPIPE`)
            std::process::exit(1);
        }
    }

    pub fn flush(&mut self) {
        match &mut self.out {
            Output::Stdout => {
                let _ = std::io::stdout().flush();
            }
            Output::File(file, _) => {
                let _ = file.flush();
            }
            Output::Off => {}
        }
    }

    /// `PrintDatabaseError`: to stderr, on a line of its own.
    pub fn print_error(&mut self, message: &str) {
        self.flush();
        let highlight = self.highlighter.highlight_errors(self.stderr_is_console);
        let text = self.highlighter.database_error(message, highlight);
        let _ = std::io::stderr().lock().write_all(text.as_bytes());
    }

    /// What a highlighting command failed with, printed as C++ prints it.
    pub fn print_highlight_error(&mut self, error: highlight::HighlightError, typed: &str, usage: &str) {
        match error {
            highlight::HighlightError::Stderr(message) => self.eprint(&message),
            highlight::HighlightError::DatabaseError(message) => self.print_error(message.trim_end_matches('\n')),
            highlight::HighlightError::Usage => {
                let message = highlight::usage_error(typed, typed, usage);
                self.print_error(&message);
            }
            highlight::HighlightError::Failed => {}
        }
    }

    pub fn eprint(&mut self, message: &str) {
        self.flush();
        let _ = std::io::stderr().lock().write_all(message.as_bytes());
    }

    /// `ShellState::StringToBool`.
    pub fn string_to_bool(&mut self, arg: &str) -> bool {
        let digits = if let Some(hex) = arg.strip_prefix("0x") {
            !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
        } else {
            !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit())
        };
        if digits {
            return string_to_int(arg) & 0xffff_ffff != 0;
        }
        if arg.eq_ignore_ascii_case("on") || arg.eq_ignore_ascii_case("yes") {
            return true;
        }
        if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("no") {
            return false;
        }
        self.eprint(&format!("ERROR: Not a boolean value: \"{arg}\". Assuming \"no\".\n"));
        false
    }

    /// `ShellState::OpenDB`.
    pub fn open_db(&mut self) -> Result<(), Error> {
        let db = if self.db_filename.is_empty() || self.db_filename == ":memory:" {
            Database::open_in_memory()?
        } else if let Some(version) = &self.storage_version {
            Database::open_with_storage_version(&self.db_filename, self.read_only, version)?
        } else {
            Database::open(&self.db_filename, self.read_only)?
        };
        let mut conn = db.connect()?;
        // `ShellScanLastResult`: `_` is the last result
        let last_rows = self.last_rows.clone();
        conn.add_replacement_scan(move |name| {
            if name != "_" {
                return None;
            }
            let rows = last_rows.lock().ok()?.clone();
            Some(rows.ok_or_else(|| {
                Error::new(
                    duckrs::ErrorKind::Binder,
                    "Failed to query last result \"_\": no result available",
                )
            }))
        });
        if self.safe_mode {
            let _ = conn.query("SET enable_external_access=false");
            let _ = conn.query("SET lock_configuration=true");
        }
        self.conn = Some(conn);
        self.db = Some(db);
        Ok(())
    }

    pub fn conn(&mut self) -> &mut Connection {
        if self.conn.is_none()
            && let Err(error) = self.open_db()
        {
            self.print_error(&error.to_string());
            std::process::exit(1);
        }
        self.conn.as_mut().expect("opened above")
    }

    /// `ShellState::SetOutputMode`.
    pub fn set_output_mode(&mut self, mode_name: &str, table: Option<&str>) -> bool {
        let n2 = mode_name.len();
        let prefix_of = |full: &str| n2 > 0 && full.starts_with(mode_name);
        let c2 = mode_name.chars().next().unwrap_or('\0');
        if table.is_some() && !(c2 == 'i' && prefix_of("insert")) {
            self.eprint("TABLE argument can only be used with .mode insert");
            return false;
        }
        if c2 == 'l' && n2 > 2 && prefix_of("lines") {
            self.mode = RenderMode::Line;
            self.row_separator = SEP_ROW.into();
        } else if c2 == 'c' && prefix_of("columns") {
            self.mode = RenderMode::Column;
            if self.header_set {
                self.show_header = true;
            }
            self.row_separator = SEP_ROW.into();
        } else if c2 == 'l' && n2 > 2 && prefix_of("list") {
            self.mode = RenderMode::List;
            self.col_separator = SEP_COLUMN.into();
            self.row_separator = SEP_ROW.into();
        } else if c2 == 'h' && prefix_of("html") {
            self.mode = RenderMode::Html;
        } else if c2 == 't' && prefix_of("tcl") {
            self.mode = RenderMode::Tcl;
            self.col_separator = SEP_SPACE.into();
            self.row_separator = SEP_ROW.into();
        } else if c2 == 'c' && prefix_of("csv") {
            self.mode = RenderMode::Csv;
            self.col_separator = SEP_COMMA.into();
            self.row_separator = SEP_CRLF.into();
        } else if c2 == 't' && prefix_of("tabs") {
            self.mode = RenderMode::List;
            self.col_separator = SEP_TAB.into();
        } else if c2 == 'i' && prefix_of("insert") {
            self.mode = RenderMode::Insert;
            self.dest_table = renderers::quoted_identifier(table.unwrap_or("table"));
        } else if c2 == 'q' && prefix_of("quote") {
            self.mode = RenderMode::Quote;
            self.col_separator = SEP_COMMA.into();
            self.row_separator = SEP_ROW.into();
        } else if c2 == 'a' && prefix_of("ascii") {
            self.mode = RenderMode::Ascii;
            self.col_separator = SEP_UNIT.into();
            self.row_separator = SEP_RECORD.into();
        } else if c2 == 'm' && prefix_of("markdown") {
            self.mode = RenderMode::Markdown;
        } else if c2 == 't' && prefix_of("table") {
            self.mode = RenderMode::Table;
        } else if c2 == 'b' && prefix_of("box") {
            self.mode = RenderMode::Box;
        } else if c2 == 'd' && prefix_of("duckbox") {
            self.mode = RenderMode::DuckBox;
        } else if c2 == 'j' && prefix_of("json") {
            self.mode = RenderMode::Json;
        } else if c2 == 'l' && prefix_of("latex") {
            self.mode = RenderMode::Latex;
        } else if c2 == 't' && prefix_of("trash") {
            self.mode = RenderMode::Trash;
        } else if c2 == 'j' && prefix_of("jsonlines") {
            self.mode = RenderMode::JsonLines;
        } else {
            self.eprint(
                "Error: mode should be one of: ascii box column csv duckbox html insert json jsonlines latex line \
                 list markdown quote table tabs tcl trash \n",
            );
            return false;
        }
        true
    }

    /// The duckbox settings (`ModeDuckBoxRenderer`).
    pub fn box_config(&self, remove_limits: bool) -> box_renderer::BoxRendererConfig {
        use box_renderer::{BoxRendererConfig, LargeNumberRendering};
        let mut config = BoxRendererConfig {
            max_rows: self.max_rows,
            max_width: self.max_width,
            null_value: self.null_value.clone(),
            decimal_separator: self.decimal_separator,
            thousand_separator: self.thousand_separator,
            ..BoxRendererConfig::default()
        };
        if self.columns {
            config.render_mode = box_renderer::RenderMode::Columns;
        }
        let to_console = matches!(self.out, Output::Stdout) && self.stdout_is_console;
        if config.max_width == 0 && !to_console {
            config.max_width = usize::MAX;
        }
        if matches!(self.out, Output::File(..)) {
            config.max_rows = usize::MAX;
        }
        config.max_analyze_rows = self
            .max_analyze_rows
            .unwrap_or(if to_console { 100_000 } else { usize::MAX });
        config.large_number_rendering = match self.large_number_rendering.as_deref() {
            Some("all") => LargeNumberRendering::All,
            Some("footer") => LargeNumberRendering::Footer,
            Some(_) => LargeNumberRendering::None,
            None if to_console => LargeNumberRendering::Footer,
            None => LargeNumberRendering::None,
        };
        config.hidden_rows_hint = "use .last to show entire result".to_string();
        if remove_limits {
            config.max_rows = usize::MAX;
            config.max_width = usize::MAX;
        }
        config
    }

    /// Draws a result in `mode`.
    pub fn render_result(&mut self, mode: RenderMode, result: &QueryResult, remove_limits: bool) {
        let text = match mode {
            RenderMode::Trash => return,
            RenderMode::DuckBox | RenderMode::Explain | RenderMode::Pretty => {
                let config = self.box_config(remove_limits);
                box_renderer::render(&config, &result.names, &result.types, result)
            }
            _ => renderers::render(self, mode, result),
        };
        self.print(&text);
    }

    /// `ShellState::ExecuteSQL`: runs each statement of `sql`, stopping at the first that fails.
    pub fn execute_sql(&mut self, sql: &str) -> bool {
        let statements = match duckrs_parser::parse_ast_with_text(sql, &Default::default()) {
            Ok(statements) => statements,
            Err(error) => {
                // the engine parses it again, so that the error aborts a transaction that is open
                let error = self.conn().query(sql).err().unwrap_or(error);
                let message = format_error(&error, sql);
                self.print_error(&message);
                return false;
            }
        };
        for (statement, text) in statements {
            let text = text.trim_start().to_string();
            if self.echo {
                let shown = if text.is_empty() { sql.to_string() } else { text.clone() };
                self.print(&format!("{shown}\n"));
            }
            if !self.execute_statement(&statement, &text) {
                return false;
            }
        }
        true
    }

    /// `ShellState::ExecuteStatement`.
    fn execute_statement(&mut self, statement: &Statement, text: &str) -> bool {
        if !matches!(statement, Statement::Prepare { .. }) && has_anonymous_parameters(text) {
            self.print_error(
                "Prepared statement parameters cannot be used directly\nTo use prepared statement parameters, use \
                 PREPARE to prepare a statement, followed by EXECUTE",
            );
            return false;
        }
        let result = match self.conn().query(text) {
            Ok(result) => result,
            Err(error) => {
                let message = format_error(&error, text);
                self.print_error(&message);
                return false;
            }
        };
        let changed_rows = matches!(
            statement,
            Statement::Insert(_)
                | Statement::Update(_)
                | Statement::Delete(_)
                | Statement::Merge(_)
                | Statement::Copy(_)
        ) && result.names.len() == 1
            && result.names[0] == "Count";
        if changed_rows {
            if result.row_count() == 1 {
                let value = result.value(0, 0);
                if let Ok(count) = value.try_to_string().unwrap_or_default().parse::<i64>() {
                    self.last_changes = count;
                    self.total_changes += count;
                }
            }
            return true;
        }
        if result.column_count() == 0 || !returns_rows(statement, &result) {
            return true;
        }
        if self.mode == RenderMode::DuckBox
            && let Some(table_name) = describe_table_name(statement)
            && result.names.len() == 6
            && result.names[0] == "column_name"
            && result.names[1] == "column_type"
        {
            self.render_describe(&table_name, &result);
            self.set_last_result(result);
            return true;
        }
        let rendered = match statement {
            Statement::Multi(statements) => statements.last().unwrap_or(statement),
            other => other,
        };
        let mode = if matches!(rendered, Statement::Explain { .. }) && self.mode == RenderMode::DuckBox {
            RenderMode::Explain
        } else {
            self.mode
        };
        if mode == RenderMode::Explain {
            self.render_explain(&result);
        } else {
            self.render_result(mode, &result, false);
        }
        self.set_last_result(result);
        true
    }

    fn set_last_result(&mut self, result: QueryResult) {
        let chunks: Vec<duckrs::DataChunk> = result.collection.chunks().cloned().collect();
        if let Ok(mut rows) = self.last_rows.lock() {
            *rows = Some((result.names.clone(), result.types.clone(), Arc::new(chunks)));
        }
        self.last_result = Some(result);
    }

    /// `ModeDescribeRenderer`: the columns DESCRIBE lists, drawn as a table's metadata.
    fn render_describe(&mut self, table_name: &str, result: &QueryResult) {
        let text = |column: usize, row: usize| -> Option<String> {
            let value = result.value(column, row);
            (!value.is_null()).then(|| value.try_to_string().unwrap_or_default())
        };
        let mut table = table_metadata::TableInfo {
            table_name: table_name.to_string(),
            ..Default::default()
        };
        for row in 0..result.row_count() {
            let key = text(3, row);
            table.columns.push(table_metadata::ColumnInfo {
                name: text(0, row).unwrap_or_default(),
                ty: text(1, row).unwrap_or_default(),
                is_not_null: text(2, row).as_deref() == Some("NO"),
                is_primary_key: key.as_deref() == Some("PRI"),
                is_unique: key.as_deref() == Some("UNI"),
                default_value: text(4, row).unwrap_or_default(),
            });
        }
        self.render_table_metadata(&[table]);
    }

    /// `ShellState::RenderTableMetadata`.
    pub fn render_table_metadata(&mut self, tables: &[table_metadata::TableInfo]) {
        let config = table_metadata::MetadataConfig {
            max_width: table_metadata::max_render_width(self.max_width),
            compact: false,
        };
        let text = table_metadata::render_table_metadata(&config, tables);
        self.print(&text);
    }

    /// `ModeExplainRenderer`: the plan text of each row.
    fn render_explain(&mut self, result: &QueryResult) {
        if result.column_count() == 2 {
            let mut text = String::new();
            for row in 0..result.row_count() {
                let value = result.value(1, row);
                text.push_str(&value.try_to_string().unwrap_or_default());
                text.push('\n');
            }
            self.print(&text);
        } else {
            self.render_result(RenderMode::DuckBox, result, false);
        }
    }

    /// `ShellState::RunOneSqlLine`.
    fn run_one_sql_line(&mut self, sql: &str) -> usize {
        let start = (Instant::now(), cpu_times());
        let success = self.execute_sql(sql);
        self.end_timer(start);
        if !success {
            return 1;
        }
        if self.count_changes {
            let text = format!(
                "changes: {:3}   total_changes: {}\n",
                self.last_changes, self.total_changes
            );
            self.print(&text);
        }
        0
    }

    fn end_timer(&mut self, start: (Instant, (f64, f64))) {
        if self.timer {
            let elapsed = start.0.elapsed().as_secs_f64();
            let (user, sys) = cpu_times();
            let (user, sys) = (user - start.1.0, sys - start.1.1);
            let digits = self.timer_digits;
            self.print(&format!(
                "Run Time (s): real {elapsed:.digits$} user {user:.6} sys {sys:.6}\n"
            ));
        }
    }

    pub fn bail_on_error(&self, mode: InputMode) -> bool {
        match self.bail {
            Bail::Bail => true,
            Bail::DontBail => false,
            Bail::Automatic => matches!(mode, InputMode::File | InputMode::DuckDbRc),
        }
    }

    /// `ShellState::ProcessInput`: the number of errors is returned as 0 or 1; `exit` is set by `.exit`.
    pub fn process_input(&mut self, input: &mut dyn LineSource, mode: InputMode) -> InputOutcome {
        let interactive = input.is_interactive();
        let mut sql = String::new();
        let mut errors = 0usize;
        loop {
            if errors > 0 && self.bail_on_error(mode) && !interactive {
                break;
            }
            self.flush();
            let Some(line) = input.next_line(!sql.is_empty()) else {
                if interactive {
                    println!();
                }
                break;
            };
            if interactive && line == "\u{3}" {
                // Ctrl-C: the statement typed so far is dropped
                sql.clear();
                continue;
            }
            if sql.is_empty() && all_whitespace(&line) {
                if self.echo {
                    self.print(&format!("{line}\n"));
                }
                continue;
            }
            if sql.is_empty() && (line.starts_with('.') || line.starts_with('#')) {
                if self.echo {
                    self.print(&format!("{line}\n"));
                }
                if line.starts_with('.') {
                    input.add_history(&line);
                    match commands::do_meta_command(self, &line) {
                        MetaOutcome::Exit => return InputOutcome { errors, exit: true },
                        MetaOutcome::Error => errors += 1,
                        MetaOutcome::Ok => {}
                    }
                }
                continue;
            }
            let prior = sql.len();
            if sql.is_empty() {
                sql.push_str(line.trim_start());
            } else {
                sql.push('\n');
                sql.push_str(&line);
            }
            if sql[prior..].contains(';') && sql_is_complete(&sql) {
                input.add_history(&sql);
                errors += self.run_one_sql_line(&sql);
                sql.clear();
                self.after_command();
            } else if all_whitespace(&sql) {
                if self.echo {
                    self.print(&format!("{sql}\n"));
                }
                sql.clear();
            }
        }
        if !sql.is_empty() && !all_whitespace(&sql) {
            errors += self.run_one_sql_line(&sql);
        }
        InputOutcome { errors, exit: false }
    }

    /// `.once`: the output returns to stdout after one command.
    pub fn after_command(&mut self) {
        if self.out_count > 0 {
            self.out_count = 0;
            self.reset_output();
        }
    }

    pub fn reset_output(&mut self) {
        self.flush();
        self.out = Output::Stdout;
    }

    /// `ShellState::ProcessFile`.
    pub fn process_file(&mut self, file: &str, mode: InputMode) -> InputOutcome {
        match File::open(file) {
            Ok(f) => {
                let mut reader = ReaderInput(BufReader::new(f));
                self.process_input(&mut reader, mode)
            }
            Err(_) => {
                if mode != InputMode::DuckDbRc {
                    self.print_error(&format!("IO Error: Failed to open file \"{file}\""));
                    return InputOutcome { errors: 1, exit: false };
                }
                InputOutcome { errors: 0, exit: false }
            }
        }
    }

    /// `ShellState::RunInitialCommand`: `-c`, `-s` and `-cmd`.
    fn run_initial_command(&mut self, sql: &str, bail: bool) -> i32 {
        if sql.starts_with('.') {
            match commands::do_meta_command(self, sql) {
                MetaOutcome::Error if bail => return 1,
                MetaOutcome::Exit if bail => return 0,
                _ => {}
            }
        } else {
            let start = (Instant::now(), cpu_times());
            let ok = self.execute_sql(sql);
            self.end_timer(start);
            if !ok && bail {
                return 1;
            }
        }
        0
    }

    /// `DuckDB::LibraryVersion` and `ReleaseCodename`: the engine has no version functions yet, so
    /// this is the crate's version.
    pub fn version() -> String {
        format!("v{} (duckrs)", env!("CARGO_PKG_VERSION"))
    }
}

/// `StatementReturnType::QUERY_RESULT`: whether the statement's result is rows to show (the
/// others answer with a `Count` or `Success` column, which the shell does not show).
fn returns_rows(statement: &Statement, result: &QueryResult) -> bool {
    let status = |name: &str| result.names.len() == 1 && result.names[0] == name;
    // the result of a statement that returns nothing (`StatementReturnType::NOTHING`)
    if status("Success") && result.row_count() == 0 && result.types[0].id() == duckrs::LogicalTypeId::Boolean {
        return false;
    }
    match statement {
        Statement::Select(_) | Statement::Explain { .. } | Statement::Call(_) => true,
        Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Merge(_)
        | Statement::Copy(_) => !status("Count"),
        Statement::Pragma(_) | Statement::Execute { .. } => {
            !(status("Count") || (status("Success") && result.row_count() == 0))
        }
        Statement::Multi(statements) => statements.last().is_some_and(|last| returns_rows(last, result)),
        _ => false,
    }
}

/// `ShellState::UseDescribeRenderMode`: for `DESCRIBE name`, `DESCRIBE (query)` and `SHOW name`, the
/// name the table metadata is drawn under.
fn describe_table_name(statement: &Statement) -> Option<String> {
    use duckrs_parser::ast::{ExprKind, QueryNodeKind, ShowType, TableRefKind};
    // `RenderedStatement`: of the statements one expands into, the last is shown
    let statement = match statement {
        Statement::Multi(statements) => statements.last()?,
        other => other,
    };
    let Statement::Select(select) = statement else {
        return None;
    };
    let QueryNodeKind::Select(node) = &select.node.kind else {
        return None;
    };
    if node.select_list.len() != 1 || !matches!(node.select_list[0].kind, ExprKind::Star(_)) {
        return None;
    }
    let TableRefKind::Show(show) = &node.from_table.kind else {
        return None;
    };
    let name = show.name.as_ref().map(|n| n.name.clone()).filter(|n| !n.is_empty());
    let is_show = show.show_type == ShowType::Show && (name.is_some() || show.query.is_some());
    if show.show_type != ShowType::Describe && !is_show {
        return None;
    }
    if let Some(name) = name {
        return Some(name);
    }
    if let Some(query) = &show.query
        && let QueryNodeKind::Select(inner) = &query.kind
        && let TableRefKind::BaseTable { name, .. } = &inner.from_table.kind
    {
        return Some(name.name.clone());
    }
    Some("Describe".to_string())
}

/// `SQLStatement::has_anonymous_parameters`: a `?` parameter in the statement.
fn has_anonymous_parameters(text: &str) -> bool {
    duckrs_parser::tokenizer::tokenize(text).is_ok_and(|tokens| {
        tokens
            .iter()
            .any(|t| t.token_type == duckrs_parser::tokenizer::TokenType::Operator && t.text == "?")
    })
}

/// Where the shell reads its input from (`OneInputLine`).
pub trait LineSource {
    /// The next line without its line ending, or `None` at the end of the input.
    fn next_line(&mut self, continuation: bool) -> Option<String>;
    fn is_interactive(&self) -> bool {
        false
    }
    fn add_history(&mut self, _entry: &str) {}
}

/// Input from a file or a pipe.
pub struct ReaderInput<R: BufRead>(pub R);

impl<R: BufRead> LineSource for ReaderInput<R> {
    fn next_line(&mut self, _continuation: bool) -> Option<String> {
        let mut line = Vec::new();
        match self.0.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                if line.ends_with(b"\n") {
                    line.pop();
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                }
                Some(String::from_utf8_lossy(&line).into_owned())
            }
        }
    }
}

/// The line editor's helper: the input is highlighted as it is typed (`linenoise_format`).
struct EditorHelper {
    highlighter: highlight::Highlighter,
}

impl rustyline::completion::Completer for EditorHelper {
    type Candidate = String;
}

impl rustyline::hint::Hinter for EditorHelper {
    type Hint = String;
}

impl rustyline::validate::Validator for EditorHelper {}

impl rustyline::highlight::Highlighter for EditorHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        std::borrow::Cow::Owned(self.highlighter.highlight_sql(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: rustyline::highlight::CmdKind) -> bool {
        self.highlighter.is_enabled()
    }
}

impl rustyline::Helper for EditorHelper {}

/// Input typed at a terminal, with line editing and the history in `~/.duckdb_history` (linenoise in C++).
struct Interactive {
    editor: rustyline::Editor<EditorHelper, rustyline::history::FileHistory>,
    history: Option<String>,
}

impl Interactive {
    fn new(highlighter: highlight::Highlighter) -> Option<Self> {
        let mut editor = rustyline::Editor::new().ok()?;
        editor.set_helper(Some(EditorHelper { highlighter }));
        let history = std::env::var("DUCKDB_HISTORY")
            .ok()
            .or_else(|| std::env::var("HOME").ok().map(|home| format!("{home}/.duckdb_history")));
        if let Some(path) = &history {
            let _ = editor.load_history(path);
        }
        Some(Self { editor, history })
    }

    fn save_history(&mut self) {
        if let Some(path) = &self.history {
            let _ = self.editor.save_history(path);
        }
    }
}

impl LineSource for Interactive {
    fn next_line(&mut self, continuation: bool) -> Option<String> {
        let prompt = if continuation { "‣ " } else { "D " };
        match self.editor.readline(prompt) {
            Ok(line) => Some(line),
            Err(rustyline::error::ReadlineError::Interrupted) => Some("\u{3}".to_string()),
            Err(_) => None,
        }
    }

    fn is_interactive(&self) -> bool {
        true
    }

    fn add_history(&mut self, entry: &str) {
        let _ = self.editor.add_history_entry(entry);
    }
}

/// What processing input ended with.
pub struct InputOutcome {
    pub errors: usize,
    pub exit: bool,
}

/// What a dot command ended with (as `DoMetaCommand`'s return code).
pub enum MetaOutcome {
    Ok,
    Error,
    Exit,
}

/// `ShellState::StringToInt`.
pub fn string_to_int(arg: &str) -> i64 {
    let (negative, digits) = match arg.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, arg.strip_prefix('+').unwrap_or(arg)),
    };
    let mut value: i64 = 0;
    if let Some(hex) = digits.strip_prefix("0x") {
        for c in hex.chars() {
            let Some(d) = c.to_digit(16) else { break };
            value = value.wrapping_mul(16).wrapping_add(d as i64);
        }
    } else {
        let mut rest = digits;
        while let Some(c) = rest.chars().next() {
            let Some(d) = c.to_digit(10) else { break };
            value = value.wrapping_mul(10).wrapping_add(d as i64);
            rest = &rest[1..];
        }
        // the size suffixes of `integerValue`
        let multiplier = match rest.to_ascii_lowercase().as_str() {
            "kib" => 1024,
            "mib" => 1024 * 1024,
            "gib" => 1024 * 1024 * 1024,
            "kb" => 1000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "k" => 1000,
            "m" => 1_000_000,
            "g" => 1_000_000_000,
            _ => 1,
        };
        value = value.wrapping_mul(multiplier);
    }
    if negative { -value } else { value }
}

/// `_all_whitespace`: only spaces and comments.
fn all_whitespace(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            if i >= bytes.len() {
                return false;
            }
            i += 2;
            continue;
        }
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        return false;
    }
    true
}

/// `Parser::IsQueryComplete`, approximated: the text ends in a `;` outside quotes and comments.
fn sql_is_complete(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut terminated = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                let quote = c;
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return false;
                    }
                    if bytes[i] == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                terminated = false;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                let mut depth = 1;
                while depth > 0 {
                    if i + 1 >= bytes.len() {
                        return false;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'$' => {
                // a dollar-quoted string: `$tag$ ... $tag$`
                let rest = &sql[i + 1..];
                let tag_end = rest.find(|ch: char| !(ch.is_alphanumeric() || ch == '_'));
                match tag_end {
                    Some(end)
                        if rest.as_bytes()[end] == b'$' && !rest[..end].starts_with(|ch: char| ch.is_ascii_digit()) =>
                    {
                        let marker = &sql[i..i + end + 2];
                        let body_start = i + end + 2;
                        match sql[body_start..].find(marker) {
                            Some(close) => {
                                i = body_start + close + marker.len();
                                terminated = false;
                                continue;
                            }
                            None => return false,
                        }
                    }
                    _ => terminated = false,
                }
            }
            b';' => terminated = true,
            c if c.is_ascii_whitespace() => {}
            _ => terminated = false,
        }
        i += 1;
    }
    terminated
}

/// `ErrorData::Message`: the client added the line the error is at already (`ClientContext::ProcessError`).
pub fn format_error(error: &Error, query: &str) -> String {
    duckrs::with_error_location(error, query).to_string()
}

/// The user and system CPU seconds this process has used (`getrusage`).
fn cpu_times() -> (f64, f64) {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `getrusage` writes a `rusage` into the pointer, which points at enough space for one.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc != 0 {
            return (0.0, 0.0);
        }
        // SAFETY: `getrusage` succeeded, so it filled the struct in (it was zeroed before).
        let usage = unsafe { usage.assume_init() };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        (seconds(usage.ru_utime), seconds(usage.ru_stime))
    }
    #[cfg(not(unix))]
    {
        (0.0, 0.0)
    }
}

/// A command-line option (`CommandLineOption`).
struct CliOption {
    name: &'static str,
    argument_count: usize,
    arguments: &'static str,
    description: &'static str,
}

const fn opt(
    name: &'static str,
    argument_count: usize,
    arguments: &'static str,
    description: &'static str,
) -> CliOption {
    CliOption {
        name,
        argument_count,
        arguments,
        description,
    }
}

const OPTIONS: &[CliOption] = &[
    opt("ascii", 0, "", "set output mode to 'ascii'"),
    opt("bail", 0, "", "stop after hitting an error"),
    opt("batch", 0, "", "force batch I/O'"),
    opt("box", 0, "", "set output mode to 'box'"),
    opt("column", 0, "", "set output mode to 'column'"),
    opt("cmd", 1, "COMMAND", "run \"COMMAND\" before reading stdin"),
    opt("csv", 0, "", "set output mode to 'csv'"),
    opt("c", 1, "COMMAND", "run \"COMMAND\" and exit"),
    opt("echo", 0, "", "print commands before execution"),
    opt("f", 1, "FILENAME", "read/process named file and exit"),
    opt("init", 1, "FILENAME", "read/process named file"),
    opt("header", 0, "", "turn headers on"),
    opt("h", 0, "", "show help message"),
    opt("help", 0, "", "show help message"),
    opt("html", 0, "", "set output mode to HTML"),
    opt("interactive", 0, "", "force interactive I/O"),
    opt("json", 0, "", "set output mode to 'json'"),
    opt("jsonlines", 0, "", "set output mode to 'jsonlines'"),
    opt("line", 0, "", "set output mode to 'line'"),
    opt("list", 0, "", "set output mode to 'list'"),
    opt("markdown", 0, "", "set output mode to 'markdown'"),
    opt("newline", 1, "SEP", "set output row separator. Default: '\\n'"),
    opt("no-init", 0, "", "skip processing the init file"),
    opt(
        "no-stdin",
        0,
        "",
        "exit after processing options instead of reading stdin",
    ),
    opt("noheader", 0, "", "turn headers off"),
    opt(
        "nullvalue",
        1,
        "TEXT",
        "set text string for NULL values. Default 'NULL'",
    ),
    opt("quote", 0, "", "set output mode to 'quote'"),
    opt("readonly", 0, "", "open the database read-only"),
    opt("s", 1, "COMMAND", "run \"COMMAND\" and exit"),
    opt("safe", 0, "", "enable safe-mode"),
    opt("separator", 1, "SEP", "set output column separator. Default: '|'"),
    opt(
        "storage-version",
        1,
        "VER",
        "database storage compatibility version to use. Default: 'v0.10.0'",
    ),
    opt("table", 0, "", "set output mode to 'table'"),
    opt("unredacted", 0, "", "allow printing unredacted secrets"),
    opt("unsigned", 0, "", "allow loading of unsigned extensions"),
    opt("version", 0, "", "show DuckDB version"),
];

fn find_option(name: &str) -> Option<&'static CliOption> {
    OPTIONS.iter().find(|o| o.name == name).or_else(|| {
        let dashed = name.replace('_', "-");
        OPTIONS.iter().find(|o| o.name == dashed)
    })
}

/// `ShellState::PrintUsage`.
fn usage(program: &str) -> String {
    let mut text = format!(
        "Usage: {program} [OPTIONS] FILENAME [SQL]\nFILENAME is the name of an DuckDB database. A new database is \
         created\nif the file does not previously exist. Defaults to :memory:.\nOPTIONS include:\n"
    );
    for option in OPTIONS {
        let head = if option.arguments.is_empty() {
            format!("-{}", option.name)
        } else {
            format!("-{} {}", option.name, option.arguments)
        };
        text += &format!("   {head:<22}{}\n", option.description);
    }
    text
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let program = args.first().cloned().unwrap_or_else(|| "duckrs".into());
    let mut shell = Shell::new(program.clone());
    let mut read_stdin = true;
    let mut run_init = true;
    let mut init_file: Option<String> = None;
    let mut extra_commands = Vec::new();
    let mut calls: Vec<(&'static CliOption, Vec<String>)> = Vec::new();
    let mut i = 1;
    // the first pass: the database, and the options needed before it is opened
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') {
            if shell.db_filename.is_empty() {
                shell.db_filename = arg.clone();
            } else {
                read_stdin = false;
                shell.stdin_is_interactive = false;
                extra_commands.push(arg.clone());
            }
            i += 1;
            continue;
        }
        let mut name = &arg[1..];
        if let Some(rest) = name.strip_prefix('-') {
            name = rest;
        }
        let Some(option) = find_option(name) else {
            let names: Vec<String> = OPTIONS.iter().map(|o| format!("-{}", o.name)).collect();
            let candidates = duckrs_common::string_util::candidates_error_message(&names, arg, "Did you mean");
            shell.print_error(&format!(
                "Invalid Input Error: Unrecognized option '{arg}'{candidates}\nRun '{program} -help' for a list of \
                 options.\n"
            ));
            return 1;
        };
        let mut arguments = vec![option.name.to_string()];
        for index in 0..option.argument_count {
            if i + 1 >= args.len() {
                shell.print_error(&format!(
                    "Missing Argument Error: Argument '-{}' needs {} arguments, but got {index}\nOPTION:\n  -{} {}    \
                     {}\n\nRun '{program} -help' for a list of options.\n",
                    option.name, option.argument_count, option.name, option.arguments, option.description
                ));
                return 1;
            }
            i += 1;
            arguments.push(args[i].clone());
        }
        // the pre-init callbacks
        match option.name {
            "c" | "s" | "f" | "h" | "help" | "batch" => shell.stdin_is_interactive = false,
            "init" => init_file = Some(arguments[1].clone()),
            "no-init" => run_init = false,
            "readonly" => shell.read_only = true,
            "safe" => shell.safe_mode = true,
            // `SetStorageVersion`
            "storage-version" => {
                let version = &arguments[1];
                if duckrs_common::storage_version::StorageVersion::from_name(version).is_none() {
                    let candidates = duckrs_common::storage_version::StorageVersion::candidates().join(", ");
                    shell.eprint(&format!(
                        "{program}: Error: unknown argument ({version}) for '-storage-version': Invalid Input Error: \
                         The version string '{version}' is not a known DuckDB version, valid options are: \
                         {candidates}\n"
                    ));
                    // `MetadataResult::EXIT` from a pre-init option ends the shell successfully
                    return 0;
                }
                shell.storage_version = Some(version.clone());
            }
            _ => {}
        }
        if matches!(option.name, "h" | "help") {
            print!("{}", usage(&program));
            return 0;
        }
        calls.push((option, arguments));
        i += 1;
    }
    if shell.db_filename.is_empty() {
        shell.db_filename = ":memory:".to_string();
    }
    if let Err(error) = shell.open_db() {
        shell.print_error(&error.to_string());
        return 1;
    }
    // the init file
    if run_init {
        let (path, explicit) = match &init_file {
            Some(file) => (file.clone(), true),
            None => match std::env::var("HOME") {
                Ok(home) => (format!("{home}/.duckdbrc"), false),
                Err(_) => (String::new(), false),
            },
        };
        if !path.is_empty() {
            let mode = InputMode::DuckDbRc;
            let outcome = if explicit && File::open(&path).is_err() {
                shell.print_error(&format!("IO Error: Failed to open file \"{path}\""));
                InputOutcome { errors: 1, exit: false }
            } else {
                shell.process_file(&path, mode)
            };
            if outcome.errors > 0 && shell.bail != Bail::DontBail {
                if explicit || File::open(&path).is_ok() {
                    shell.print_error(&format!(
                        "Encountered errors while executing init file \"{path}\". Exiting."
                    ));
                }
                return 1;
            }
        }
    }
    // the second pass: the options that override the init file
    for (option, arguments) in &calls {
        let arg = arguments.get(1).map(String::as_str).unwrap_or("");
        match option.name {
            "ascii" => {
                shell.mode = RenderMode::Ascii;
                shell.col_separator = SEP_UNIT.into();
                shell.row_separator = SEP_RECORD.into();
            }
            "csv" => {
                shell.mode = RenderMode::Csv;
                shell.col_separator = SEP_COMMA.into();
            }
            "box" => shell.mode = RenderMode::Box,
            "column" => shell.mode = RenderMode::Column,
            "html" => shell.mode = RenderMode::Html,
            "json" => shell.mode = RenderMode::Json,
            "jsonlines" => shell.mode = RenderMode::JsonLines,
            "line" => shell.mode = RenderMode::Line,
            "list" => shell.mode = RenderMode::List,
            "markdown" => shell.mode = RenderMode::Markdown,
            "quote" => shell.mode = RenderMode::Quote,
            "table" => shell.mode = RenderMode::Table,
            "bail" => shell.bail = Bail::Bail,
            "batch" => shell.stdin_is_interactive = false,
            "interactive" => shell.stdin_is_interactive = true,
            "echo" => shell.echo = true,
            "header" => shell.show_header = true,
            "noheader" => shell.show_header = false,
            "nullvalue" => shell.null_value = arg.to_string(),
            "separator" => shell.col_separator = commands::resolve_backslashes(arg),
            "newline" => shell.row_separator = commands::resolve_backslashes(arg),
            "no-stdin" => read_stdin = false,
            "version" => {
                println!("{}", Shell::version());
                return 0;
            }
            "cmd" | "c" | "s" => {
                if option.name != "cmd" {
                    read_stdin = false;
                }
                let bail = match shell.bail {
                    Bail::Automatic => true,
                    other => other == Bail::Bail,
                };
                if shell.run_initial_command(arg, bail) != 0 {
                    return 1;
                }
            }
            "f" => {
                read_stdin = false;
                let outcome = shell.process_file(arg, InputMode::File);
                if outcome.errors > 0 {
                    return 1;
                }
                if outcome.exit {
                    return 0;
                }
            }
            _ => {}
        }
    }
    if !read_stdin {
        for command in &extra_commands {
            if command.starts_with('.') {
                match commands::do_meta_command(&mut shell, command) {
                    MetaOutcome::Exit => return 0,
                    MetaOutcome::Error => return 1,
                    MetaOutcome::Ok => {}
                }
            } else if !shell.execute_sql(command) {
                return 1;
            }
        }
        shell.flush();
        return 0;
    }
    let outcome = if shell.stdin_is_interactive {
        println!("{}\nEnter \".help\" for usage hints.", Shell::version());
        match Interactive::new(shell.highlighter.clone()) {
            Some(mut editor) => {
                let outcome = shell.process_input(&mut editor, InputMode::Standard);
                editor.save_history();
                outcome
            }
            None => shell.process_input(&mut ReaderInput(std::io::stdin().lock()), InputMode::Standard),
        }
    } else {
        shell.process_input(&mut ReaderInput(std::io::stdin().lock()), InputMode::Standard)
    };
    shell.flush();
    i32::from(outcome.errors > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_end_at_a_semicolon_outside_quotes_and_comments() {
        assert!(sql_is_complete("select 1;"));
        assert!(sql_is_complete("select 1; -- done"));
        assert!(!sql_is_complete("select ';"));
        assert!(!sql_is_complete("select 1 /* ; */"));
        assert!(!sql_is_complete("select $$ ; "));
        assert!(sql_is_complete("select $x$ ; $x$;"));
        assert!(!sql_is_complete("select 1"));
    }

    #[test]
    fn whitespace_and_comments_are_blank() {
        assert!(all_whitespace("  -- a comment"));
        assert!(all_whitespace("/* x */ "));
        assert!(!all_whitespace("/* open"));
        assert!(!all_whitespace("select"));
    }

    #[test]
    fn integers_take_size_suffixes() {
        assert_eq!(string_to_int("42"), 42);
        assert_eq!(string_to_int("-7"), -7);
        assert_eq!(string_to_int("0x10"), 16);
        assert_eq!(string_to_int("2k"), 2000);
        assert_eq!(string_to_int("1KiB"), 1024);
    }

    #[test]
    fn errors_point_at_their_position() {
        let error = Error::new(duckrs::ErrorKind::Parser, "syntax error at or near \"inser\"").with_position(0);
        assert_eq!(
            format_error(&error, "inser into t;"),
            "Parser Error: syntax error at or near \"inser\"\n\nLINE 1: inser into t;\n        ^"
        );
        let error = Error::new(duckrs::ErrorKind::Parser, "x").with_position(9);
        assert_eq!(
            format_error(&error, "select 1\nfrom t"),
            "Parser Error: x\n\nLINE 2: from t\n        ^"
        );
    }

    #[test]
    fn a_describe_names_its_table() {
        let statement = |sql: &str| duckrs_parser::parse_ast(sql).unwrap().remove(0);
        assert_eq!(describe_table_name(&statement("DESCRIBE t")), Some("t".to_string()));
        assert_eq!(
            describe_table_name(&statement("DESCRIBE SELECT * FROM tbl")),
            Some("tbl".to_string())
        );
        assert_eq!(describe_table_name(&statement("SELECT 1")), None);
    }
}
