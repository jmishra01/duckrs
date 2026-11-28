// Ported from duckdb/tools/shell/shell_metadata_command.cpp @ rust-port-baseline
//
//! The dot commands (`MetadataCommand`): `.mode`, `.headers`, `.read`, `.import`, ...

use crate::renderers::{escape_c_string, quoted_string};
use crate::table_metadata::{ColumnInfo, TableInfo};
use crate::{Bail, CommandResult, InputMode, MetaOutcome, Output, Shell, string_to_int};
use pintail_common::value::ValueData;
use std::fs::File;

/// `MetadataCommand`.
struct Command {
    name: &'static str,
    /// the arguments including the command itself; 0 is any number
    argument_count: usize,
    run: Option<fn(&mut Shell, &[String]) -> CommandResult>,
    usage: &'static str,
    description: &'static str,
    /// the shortest prefix that names the command; 0 is any
    match_size: usize,
}

const fn cmd(
    name: &'static str,
    argument_count: usize,
    run: Option<fn(&mut Shell, &[String]) -> CommandResult>,
    usage: &'static str,
    description: &'static str,
    match_size: usize,
) -> Command {
    Command {
        name,
        argument_count,
        run,
        usage,
        description,
        match_size,
    }
}

fn unsupported(_: &mut Shell, _: &[String]) -> CommandResult {
    CommandResult::Success
}

/// The commands in the order the C++ table lists them (the first that matches a prefix wins).
const COMMANDS: &[Command] = &[
    cmd("about", 0, Some(show_about), "", "Show the about message", 0),
    cmd(
        "auto_format",
        2,
        Some(unsupported),
        "on|off",
        "Toggle automatic formatting of SQL statements",
        3,
    ),
    cmd(
        "bail",
        2,
        Some(toggle_bail),
        "on|off|auto",
        "Stop after hitting an error.  Default AUTO",
        3,
    ),
    cmd(
        "binary",
        2,
        Some(unsupported),
        "on|off",
        "Turn binary output on or off.  Default OFF",
        3,
    ),
    cmd(
        "cd",
        2,
        Some(change_directory),
        "DIRECTORY",
        "Change the working directory to DIRECTORY",
        0,
    ),
    cmd(
        "changes",
        2,
        Some(toggle_changes),
        "on|off",
        "Show number of rows changed by SQL",
        3,
    ),
    cmd(
        "columns",
        1,
        Some(set_columns),
        "",
        "Column-wise rendering of query results",
        0,
    ),
    cmd(
        "comment",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for comment values",
        0,
    ),
    cmd(
        "constant",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for constant values",
        0,
    ),
    cmd(
        "cont",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for continuation markers",
        0,
    ),
    cmd(
        "cont_sel",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for continuation markers",
        0,
    ),
    cmd(
        "decimal_sep",
        0,
        Some(set_decimal_sep),
        "SEP",
        "Sets the decimal separator used when rendering numbers. Only for duckbox mode.",
        3,
    ),
    cmd(
        "databases",
        1,
        Some(show_databases),
        "",
        "List names and files of attached databases",
        2,
    ),
    cmd(
        "dump",
        0,
        Some(dump_tables),
        "?TABLE?",
        "Render database content as SQL",
        0,
    ),
    cmd(
        "display_colors",
        0,
        Some(display_colors),
        "[bold|underline]",
        "Display all terminal colors and their names",
        0,
    ),
    cmd("echo", 2, Some(toggle_echo), "on|off", "Turn command echo on or off", 3),
    cmd("edit", 0, None, "", "Opens an external text editor to edit a query.", 0),
    cmd(
        "error",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for errors",
        0,
    ),
    cmd(
        "excel",
        0,
        Some(set_output_excel),
        "",
        "Display the output of next command in spreadsheet",
        0,
    ),
    cmd(
        "exit",
        0,
        Some(exit_process),
        "?CODE?",
        "Exit this program with return-code CODE",
        0,
    ),
    cmd(
        "headers",
        2,
        Some(toggle_headers),
        "on|off",
        "Turn display of headers on or off",
        0,
    ),
    cmd(
        "help",
        0,
        Some(show_help),
        "?-all? ?PATTERN?",
        "Show help text for PATTERN",
        0,
    ),
    cmd(
        "highlight",
        2,
        Some(toggle_highlight),
        "on|off",
        "Toggle syntax highlighting in the shell on/off",
        0,
    ),
    cmd(
        "highlight_colors",
        0,
        Some(highlight_colors),
        "OPTIONS",
        "Configure highlighting colors",
        0,
    ),
    cmd(
        "highlight_errors",
        2,
        Some(toggle_highlight_errors),
        "on|off",
        "Turn highlighting of errors on or off",
        0,
    ),
    cmd(
        "highlight_mode",
        2,
        Some(set_highlight_mode),
        "mixed|dark|light",
        "Toggle the highlighting mode",
        0,
    ),
    cmd(
        "highlight_results",
        2,
        Some(toggle_highlight_results),
        "on|off",
        "Turn highlighting of results on or off",
        0,
    ),
    cmd("history", 0, Some(unsupported), "", "Print the command history", 0),
    cmd(
        "import",
        0,
        Some(import_data),
        "FILE TABLE",
        "Import data from FILE into TABLE",
        0,
    ),
    cmd("indexes", 0, Some(show_indexes), "?TABLE?", "Show names of indexes", 0),
    cmd("indices", 0, Some(show_indexes), "?TABLE?", "Show names of indexes", 0),
    cmd(
        "keyword",
        2,
        Some(deprecated_color),
        "?COLOR?",
        "DEPRECATED: Sets the syntax highlighting color used for keywords",
        0,
    ),
    cmd("last", 1, Some(render_last), "", "Render the last result in full", 0),
    cmd(
        "large_number_rendering",
        2,
        Some(set_large_number_rendering),
        "MODE",
        "Toggle readable rendering of large numbers (duckbox only)",
        0,
    ),
    cmd(
        "log",
        2,
        Some(toggle_log),
        "FILE|off",
        "Turn logging on or off.  FILE can be stderr/stdout",
        0,
    ),
    cmd(
        "manual",
        2,
        None,
        "FUNCTION",
        "Show the manual page for a SQL function",
        0,
    ),
    cmd(
        "maxbytes",
        0,
        Some(unsupported),
        "COUNT",
        "Sets the maximum number of bytes of rows for display (0 = all). Only for markdown mode in -agent mode.",
        0,
    ),
    cmd(
        "maxcellwidth",
        0,
        Some(unsupported),
        "COUNT",
        "Sets the maximum number of characters per cell (0 = all). Only for markdown mode in -agent mode.",
        0,
    ),
    cmd(
        "maxrows",
        0,
        Some(set_max_rows),
        "COUNT",
        "Sets the maximum number of rows for display (default: 40, -1 = all). Only for duckbox mode and for markdown mode in -agent mode.",
        0,
    ),
    cmd(
        "maxwidth",
        0,
        Some(set_max_width),
        "COUNT",
        "Sets the maximum width in characters. 0 defaults to terminal width. Only for duckbox mode.",
        0,
    ),
    cmd("mode", 0, Some(set_output_mode), "MODE ?TABLE?", "Set output mode", 0),
    cmd(
        "nullvalue",
        2,
        Some(set_null_value),
        "STRING",
        "Use STRING in place of NULL values",
        0,
    ),
    cmd(
        "open",
        0,
        Some(open_database),
        "?OPTIONS? ?FILE?",
        "Close existing database and reopen FILE",
        2,
    ),
    cmd(
        "once",
        0,
        Some(set_output_once),
        "?OPTIONS? ?FILE?",
        "Output for the next SQL command only to FILE",
        0,
    ),
    cmd(
        "output",
        0,
        Some(set_output),
        "?FILE?",
        "Send output to FILE or stdout if FILE is omitted",
        0,
    ),
    cmd(
        "pager",
        0,
        Some(set_pager),
        "on|off|auto|COMMAND",
        "Control pager usage for output",
        0,
    ),
    cmd(
        "print",
        0,
        Some(print_arguments),
        "STRING...",
        "Print literal STRING",
        3,
    ),
    cmd(
        "progress_bar",
        0,
        Some(unsupported),
        "OPTIONS",
        "Configure the progress bar display",
        0,
    ),
    cmd(
        "prompt",
        0,
        Some(unsupported),
        "MAIN CONTINUE",
        "Replace the standard prompts",
        0,
    ),
    cmd("quit", 0, Some(quit_process), "", "Exit this program", 0),
    cmd("read", 2, Some(read_from_file), "FILE", "Read input from FILE", 3),
    cmd(
        "read_line_version",
        2,
        Some(unsupported),
        "linenoise|fallback",
        "Sets the library used for processing interactive input",
        0,
    ),
    cmd(
        "render_completion",
        2,
        Some(unsupported),
        "on|off",
        "Toggle displaying of completion prompts in the shell on/off",
        0,
    ),
    cmd(
        "render_errors",
        2,
        Some(unsupported),
        "on|off",
        "Toggle rendering of errors in the shell on/off",
        0,
    ),
    cmd(
        "rows",
        1,
        Some(set_rows),
        "",
        "Row-wise rendering of query results (default)",
        0,
    ),
    cmd("safe_mode", 0, Some(enable_safe_mode), "", "Enable safe-mode", 0),
    cmd(
        "separator",
        0,
        Some(set_separator),
        "COL ?ROW?",
        "Change the column and row separators",
        0,
    ),
    cmd("serve_command", 0, None, "COMMAND", "Set the command used by -serve", 0),
    cmd(
        "schema",
        0,
        Some(display_schemas),
        "?PATTERN?",
        "Show the CREATE statements matching PATTERN",
        0,
    ),
    cmd(
        "shell",
        0,
        Some(run_shell_command),
        "CMD ARGS...",
        "Run CMD ARGS... in a system shell",
        0,
    ),
    cmd(
        "show",
        1,
        Some(show_configuration),
        "",
        "Show the current values for various settings",
        0,
    ),
    cmd(
        "startup_text",
        2,
        Some(unsupported),
        "none|version|all",
        "Set the startup text",
        0,
    ),
    cmd(
        "system",
        0,
        Some(run_shell_command),
        "CMD ARGS...",
        "Run CMD ARGS... in a system shell",
        0,
    ),
    cmd(
        "tables",
        0,
        Some(show_tables),
        "?TABLE?",
        "List names of tables matching LIKE pattern TABLE",
        2,
    ),
    cmd(
        "thousand_sep",
        0,
        Some(set_thousand_sep),
        "SEP",
        "Sets the thousand separator used when rendering numbers. Only for duckbox mode.",
        4,
    ),
    cmd(
        "timer",
        0,
        Some(toggle_timer),
        "on|off [DIGITS]",
        "Turn SQL timer on or off, DIGITS decimals for the real time (default 3)",
        0,
    ),
    cmd("version", 1, Some(show_version), "", "Show the version", 0),
    cmd(
        "width",
        0,
        Some(set_widths),
        "NUM1 NUM2 ...",
        "Set minimum column widths for columnar output",
        0,
    ),
];

/// `resolve_backslashes`: C escapes in a dot-command argument.
pub fn resolve_backslashes(z: &str) -> String {
    let bytes = z.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    while pos < bytes.len() {
        let mut c = bytes[pos];
        if c == b'\\' && pos + 1 < bytes.len() {
            pos += 1;
            c = bytes[pos];
            c = match c {
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                b'"' | b'\'' | b'\\' => c,
                b'0'..=b'7' => {
                    let mut v = c - b'0';
                    for _ in 0..2 {
                        match bytes.get(pos + 1) {
                            Some(d @ b'0'..=b'7') => {
                                v = (v << 3).wrapping_add(d - b'0');
                                pos += 1;
                            }
                            _ => break,
                        }
                    }
                    v
                }
                // an unknown escape keeps the character after the backslash
                other => other,
            };
        }
        result.push(c);
        pos += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// `DoMetaCommand`'s argument splitting.
fn split_arguments(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut args = Vec::new();
    let mut pos = 1;
    while pos < chars.len() {
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        let mut arg = String::new();
        if chars[pos] == '\'' || chars[pos] == '"' {
            let quote = chars[pos];
            pos += 1;
            while pos < chars.len() && chars[pos] != quote {
                if chars[pos] == '\\' && quote == '"' && pos + 1 < chars.len() {
                    arg.push(chars[pos]);
                    pos += 1;
                }
                arg.push(chars[pos]);
                pos += 1;
            }
            if pos < chars.len() {
                pos += 1;
            }
            if quote == '"' {
                arg = resolve_backslashes(&arg);
            }
        } else {
            while pos < chars.len() && !chars[pos].is_whitespace() {
                arg.push(chars[pos]);
                pos += 1;
            }
            arg = resolve_backslashes(&arg);
        }
        args.push(arg);
    }
    args
}

/// `ShellState::FindMetadataCommand`.
fn find_command(name: &str) -> Result<&'static Command, String> {
    let n = name.len();
    for command in COMMANDS {
        let match_size = if command.match_size > 0 { command.match_size } else { n };
        if n < match_size || n == 0 || !command.name.starts_with(name) {
            continue;
        }
        return Ok(command);
    }
    let names: Vec<String> = COMMANDS.iter().map(|c| format!(".{}", c.name)).collect();
    let candidates = pintail_common::string_util::candidates_error_message(&names, name, "Did you mean");
    Err(format!(
        "Unknown Command Error: Unrecognized command '{name}'\n{candidates}\nRun '.help' for more information."
    ))
}

/// `ShellState::DoMetaCommand`.
pub fn do_meta_command(shell: &mut Shell, line: &str) -> MetaOutcome {
    let args = split_arguments(line);
    if args.is_empty() {
        return MetaOutcome::Ok;
    }
    let outcome = match find_command(&args[0]) {
        Err(message) => {
            shell.print_error(message.trim_end_matches('\n'));
            MetaOutcome::Error
        }
        Ok(command) => {
            let mut result = CommandResult::PrintUsage;
            match command.run {
                None => {
                    shell.eprint(&format!(
                        "Command \"{}\" is unsupported in the current version of the CLI\n",
                        command.name
                    ));
                    result = CommandResult::Fail;
                }
                Some(run) if command.argument_count == 0 || command.argument_count == args.len() => {
                    result = run(shell, &args);
                }
                Some(_) => {}
            }
            if result == CommandResult::PrintUsage {
                shell.print_error(&format!(
                    "Invalid Command Error: Invalid usage of command '.{}'\n\nUsage: '.{} {}'",
                    args[0], command.name, command.usage
                ));
                result = CommandResult::Fail;
            }
            match result {
                CommandResult::Success => MetaOutcome::Ok,
                CommandResult::Exit => MetaOutcome::Exit,
                _ => MetaOutcome::Error,
            }
        }
    };
    if shell.out_count > 0 {
        shell.out_count -= 1;
        if shell.out_count == 0 {
            shell.reset_output();
        }
    }
    outcome
}

fn show_about(shell: &mut Shell, _: &[String]) -> CommandResult {
    let text = format!(
        "DuckDB is an in-process analytical database management system designed for fast execution of complex SQL \
         queries. It runs embedded within its host process with no external dependencies, and is optimized for \
         OLAP workloads using a columnar, vectorized execution engine.\n\npintail v{} is a port of DuckDB to Rust. \
         DuckDB is developed and maintained by the DuckDB Foundation, available under the MIT License.\nFor more \
         information, visit https://duckdb.org or https://github.com/duckdb/duckdb.\n\n",
        env!("CARGO_PKG_VERSION")
    );
    shell.print(&text);
    CommandResult::Success
}

fn toggle_bail(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.bail = if args[1] == "auto" {
        Bail::Automatic
    } else if shell.string_to_bool(&args[1]) {
        Bail::Bail
    } else {
        Bail::DontBail
    };
    CommandResult::Success
}

fn change_directory(shell: &mut Shell, args: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".cd cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    if std::env::set_current_dir(&args[1]).is_err() {
        shell.eprint(&format!("Cannot change to directory \"{}\"\n", args[1]));
        return CommandResult::Fail;
    }
    CommandResult::Success
}

fn toggle_changes(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.count_changes = shell.string_to_bool(&args[1]);
    CommandResult::Success
}

fn set_columns(shell: &mut Shell, _: &[String]) -> CommandResult {
    shell.columns = true;
    CommandResult::Success
}

fn set_rows(shell: &mut Shell, _: &[String]) -> CommandResult {
    shell.columns = false;
    CommandResult::Success
}

/// `SetSeparator` (the decimal / thousand separators).
fn set_number_separator(shell: &mut Shell, args: &[String], name: &str, decimal: bool) -> CommandResult {
    let current = if decimal {
        shell.decimal_separator
    } else {
        shell.thousand_separator
    };
    if args.len() == 1 {
        let shown = current.map(String::from).unwrap_or_else(|| "\0".to_string());
        shell.print(&format!("current {name} separator: {shown}\n"));
        return CommandResult::Success;
    }
    if args.len() != 2 {
        return CommandResult::PrintUsage;
    }
    let value = if args[1] == "space" {
        Some(' ')
    } else if args[1] == "none" {
        None
    } else if args[1].len() != 1 {
        shell.eprint(&format!(".{name}_sep SEP must be one byte, \"space\" or \"none\"\n"));
        return CommandResult::Fail;
    } else {
        args[1].chars().next()
    };
    if decimal {
        shell.decimal_separator = value;
    } else {
        shell.thousand_separator = value;
    }
    CommandResult::Success
}

fn set_decimal_sep(shell: &mut Shell, args: &[String]) -> CommandResult {
    set_number_separator(shell, args, "decimal", true)
}

fn set_thousand_sep(shell: &mut Shell, args: &[String]) -> CommandResult {
    set_number_separator(shell, args, "thousand", false)
}

fn show_databases(shell: &mut Shell, _: &[String]) -> CommandResult {
    match shell.conn().query("SELECT name, file FROM pragma_database_list") {
        Ok(result) => {
            let mut table = TableInfo {
                table_name: "databases".to_string(),
                ..Default::default()
            };
            for row in 0..result.row_count() {
                let file = result.value(1, row);
                table.columns.push(ColumnInfo {
                    name: result.value(0, row).try_to_string().unwrap_or_default(),
                    ty: if file.is_null() {
                        "(memory)".to_string()
                    } else {
                        file.try_to_string().unwrap_or_default()
                    },
                    ..Default::default()
                });
            }
            shell.render_table_metadata(&[table]);
            CommandResult::Success
        }
        Err(error) => {
            shell.print_error(&error.to_string());
            CommandResult::Fail
        }
    }
}

/// `DumpTable`: the schema and the rows of the tables matching the patterns, as SQL.
fn dump_tables(shell: &mut Shell, args: &[String]) -> CommandResult {
    let saved_header = shell.show_header;
    let saved_echo = shell.echo;
    let saved_newlines = shell.newlines;
    shell.newlines = false;
    shell.echo = false;
    let mut like = String::new();
    for arg in &args[1..] {
        if let Some(option) = arg.strip_prefix('-') {
            if option.trim_start_matches('-') == "newlines" {
                shell.newlines = true;
            } else {
                shell.eprint(&format!("Unknown option \"{arg}\" on \".dump\"\n"));
                shell.newlines = saved_newlines;
                shell.echo = saved_echo;
                return CommandResult::Fail;
            }
        } else if like.is_empty() {
            like = format!("name LIKE {} ESCAPE '\\'", quoted_string(arg));
        } else {
            like = format!("{like} OR name LIKE {} ESCAPE '\\'", quoted_string(arg));
        }
    }
    shell.print("BEGIN TRANSACTION;\n");
    shell.show_header = false;
    let mut errors = 0;
    if like.is_empty() {
        like = "true".to_string();
    }
    // CREATE SCHEMA for the schemas other than main first
    let schemas = format!(
        "SELECT DISTINCT table_schema FROM information_schema.tables WHERE table_schema != 'main' AND table_schema \
         NOT LIKE 'pg_%' AND table_schema != 'information_schema' AND table_name IN (SELECT name FROM sqlite_schema \
         WHERE ({like}) AND type=='table') ORDER BY table_schema"
    );
    if let Ok(result) = shell.conn().query(&schemas) {
        for row in 0..result.row_count() {
            let schema = result.value(0, row).try_to_string().unwrap_or_default();
            shell.print(&format!(
                "CREATE SCHEMA IF NOT EXISTS {};;\n",
                crate::renderers::quoted_identifier(&schema)
            ));
        }
    }
    // `RunSchemaDumpQuery`: each table's CREATE and its rows as INSERTs
    let tables = format!(
        "SELECT name, type, sql FROM sqlite_schema WHERE ({like}) AND type=='table' AND sql NOT NULL ORDER BY \
         tbl_name='sqlite_sequence'"
    );
    let rows = shell.conn().query(&tables).ok();
    for row in 0..rows.as_ref().map_or(0, |r| r.row_count()) {
        let rows = rows.as_ref().expect("checked above");
        let get = |column: usize| rows.value(column, row).try_to_string().unwrap_or_default();
        let (table, kind, sql) = (get(0), get(1), get(2));
        shell.print(&format!("{sql};\n"));
        if kind != "table" {
            continue;
        }
        let schema = shell
            .conn()
            .query(&format!(
                "SELECT table_schema FROM information_schema.tables WHERE table_name = {} AND \
                 table_type='BASE TABLE' ORDER BY (table_schema='main') DESC LIMIT 1",
                quoted_string(&table)
            ))
            .ok()
            .filter(|r| r.row_count() > 0)
            .map(|r| r.value(0, 0).try_to_string().unwrap_or_default())
            .unwrap_or_default();
        let qualified = format!(
            "{}.{}",
            crate::renderers::quoted_identifier(&schema),
            crate::renderers::quoted_identifier(&table)
        );
        let columns: Vec<String> = match shell
            .conn()
            .query(&format!("PRAGMA table_info={}", quoted_string(&qualified)))
        {
            Ok(info) => (0..info.row_count())
                .map(|r| info.value(1, r).try_to_string().unwrap_or_default())
                .collect(),
            Err(_) => Vec::new(),
        };
        if columns.is_empty() {
            errors += 1;
            break;
        }
        let select = format!(
            "SELECT {} FROM {qualified}",
            columns
                .iter()
                .map(|c| crate::renderers::quoted_identifier(c))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let dest = if schema.is_empty() {
            crate::renderers::quoted_identifier(&table)
        } else {
            qualified.clone()
        };
        let saved_dest = std::mem::replace(&mut shell.dest_table, dest);
        let saved_mode = std::mem::replace(&mut shell.mode, crate::RenderMode::Insert);
        if !shell.execute_sql(&select) {
            errors += 1;
        }
        shell.dest_table = saved_dest;
        shell.mode = saved_mode;
    }
    // `RunTableDumpQuery`: the indexes, triggers and views
    let others =
        format!("SELECT sql FROM sqlite_schema WHERE ({like}) AND sql NOT NULL AND type IN ('index','trigger','view')");
    match shell.conn().query(&others) {
        Ok(result) => {
            for row in 0..result.row_count() {
                let sql = result.value(0, row).try_to_string().unwrap_or_default();
                // a `--` comment at the end would swallow the semicolon
                let end = if sql.contains("--") { "\n;\n" } else { ";\n" };
                shell.print(&format!("{sql}{end}"));
            }
        }
        Err(error) => {
            shell.print(&format!("/**** ERROR: {error} *****/\n"));
            errors += 1;
        }
    }
    shell.print(if errors > 0 {
        "ROLLBACK; -- due to errors\n"
    } else {
        "COMMIT;\n"
    });
    shell.show_header = saved_header;
    shell.echo = saved_echo;
    shell.newlines = saved_newlines;
    CommandResult::Success
}

fn toggle_highlight(shell: &mut Shell, args: &[String]) -> CommandResult {
    let on = shell.string_to_bool(&args[1]);
    shell.highlighter.set_enabled(on);
    CommandResult::Success
}

fn toggle_highlight_errors(shell: &mut Shell, args: &[String]) -> CommandResult {
    let on = shell.string_to_bool(&args[1]);
    shell.highlighter.set_highlight_errors(on);
    CommandResult::Success
}

fn toggle_highlight_results(shell: &mut Shell, args: &[String]) -> CommandResult {
    let on = shell.string_to_bool(&args[1]);
    shell.highlighter.set_highlight_results(on);
    CommandResult::Success
}

fn highlight_outcome(
    shell: &mut Shell,
    args: &[String],
    usage: &str,
    outcome: Result<(), crate::highlight::HighlightError>,
) -> CommandResult {
    match outcome {
        Ok(()) => CommandResult::Success,
        Err(crate::highlight::HighlightError::Usage) => CommandResult::PrintUsage,
        Err(error) => {
            shell.print_highlight_error(error, &args[0], usage);
            CommandResult::Fail
        }
    }
}

fn set_highlight_mode(shell: &mut Shell, args: &[String]) -> CommandResult {
    let outcome = shell.highlighter.toggle_mode(&args[1]);
    highlight_outcome(shell, args, "mixed|dark|light", outcome)
}

fn highlight_colors(shell: &mut Shell, args: &[String]) -> CommandResult {
    let words: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    let outcome = shell.highlighter.highlight_colors_command(&words);
    highlight_outcome(shell, args, "OPTIONS", outcome)
}

/// `.keyword COLOR` and the like: the old way to set a color.
fn deprecated_color(shell: &mut Shell, args: &[String]) -> CommandResult {
    let Some((warning, outcome)) = shell.highlighter.deprecated_set_color(&args[0], &args[1]) else {
        return CommandResult::PrintUsage;
    };
    shell.eprint(&warning);
    highlight_outcome(shell, args, "?COLOR?", outcome)
}

fn display_colors(shell: &mut Shell, args: &[String]) -> CommandResult {
    let words: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    match shell.highlighter.display_colors(&words) {
        Ok(text) => {
            shell.print(&text);
            CommandResult::Success
        }
        Err(crate::highlight::HighlightError::Usage) => CommandResult::PrintUsage,
        Err(error) => {
            shell.print_highlight_error(error, &args[0], "[bold|underline]");
            CommandResult::Fail
        }
    }
}

fn toggle_echo(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.echo = shell.string_to_bool(&args[1]);
    CommandResult::Success
}

fn exit_process(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 2 {
        return CommandResult::PrintUsage;
    }
    if args.len() > 1 {
        let rc = string_to_int(&args[1]) as i32;
        if rc != 0 {
            shell.flush();
            std::process::exit(rc);
        }
    }
    CommandResult::Exit
}

fn quit_process(_: &mut Shell, _: &[String]) -> CommandResult {
    CommandResult::Exit
}

fn toggle_headers(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.show_header = shell.string_to_bool(&args[1]);
    shell.header_set = true;
    CommandResult::Success
}

fn show_help(shell: &mut Shell, args: &[String]) -> CommandResult {
    let pattern = args.get(1).filter(|a| *a != "-all");
    let mut text = String::new();
    for command in COMMANDS {
        if command.run.is_none() {
            continue;
        }
        if let Some(pattern) = pattern
            && !command.name.contains(pattern.as_str())
        {
            continue;
        }
        let head = if command.usage.is_empty() {
            format!(".{}", command.name)
        } else {
            format!(".{} {}", command.name, command.usage)
        };
        text += &format!("{head:<29}{}\n", command.description);
    }
    if text.is_empty() {
        if let Some(pattern) = pattern {
            shell.print(&format!("Nothing matches '{pattern}'\n"));
        }
        return CommandResult::Success;
    }
    shell.print(&text);
    CommandResult::Success
}

fn sql_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `ShellState::ImportData`.
fn import_data(shell: &mut Shell, args: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".import cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    let mut table_name = String::new();
    let mut file_name = String::new();
    let mut parameters: Vec<(String, String)> = Vec::new();
    let mut function = String::new();
    let mut i = 1;
    while i < args.len() {
        let mut z = args[i].as_str();
        if z.starts_with("--") {
            z = &z[1..];
        }
        if !z.starts_with('-') {
            if file_name.is_empty() {
                file_name = z.to_string();
            } else if table_name.is_empty() {
                table_name = z.to_string();
            } else {
                shell.print(&format!("ERROR: extra argument: \"{z}\".  Usage:\n"));
                return CommandResult::Fail;
            }
        } else if z == "-v" {
        } else if z == "-ascii" {
            shell.eprint("-ascii mode is no longer supported for .import");
            return CommandResult::Fail;
        } else if z == "-csv" {
            function = "read_csv".into();
        } else if z == "-parquet" {
            function = "read_parquet".into();
        } else if z == "-json" {
            function = "read_json".into();
        } else {
            let key = &z[1..];
            if i + 1 >= args.len() {
                shell.print(&format!(
                    "ERROR: expected an argument for generic parameter: \"{key}\".  Usage:\n"
                ));
                return CommandResult::Fail;
            }
            i += 1;
            parameters.push((key.to_string(), args[i].clone()));
        }
        i += 1;
    }
    if table_name.is_empty() {
        let missing = if file_name.is_empty() { "FILE" } else { "TABLE" };
        shell.print(&format!("ERROR: missing {missing} argument. Usage:\n"));
        return CommandResult::Fail;
    }
    if function.is_empty() {
        let map = [
            (".parquet", "read_parquet"),
            (".csv", "read_csv"),
            (".tsv", "read_csv"),
            (".tbl", "read_csv"),
            (".json", "read_json"),
            (".jsonl", "read_json"),
            (".ndjson", "read_json"),
            (".avro", "read_avro"),
            (".xlsx", "read_xlsx"),
        ];
        'found: for (suffix, name) in map {
            for compression in ["", ".gz", ".zst"] {
                if file_name.ends_with(&format!("{suffix}{compression}")) {
                    function = name.to_string();
                    break 'found;
                }
            }
        }
        if function.is_empty() {
            function = "read_csv".into();
        }
    }
    if function == "read_csv" && !parameters.iter().any(|(k, _)| k == "ignore_errors") {
        parameters.push(("ignore_errors".into(), "true".into()));
    }
    let exists = shell
        .conn()
        .query(&format!(
            "SELECT 1 FROM duckdb_tables() WHERE table_name = {} AND database_name = current_database() AND \
             schema_name = current_schema()",
            quoted_string(&table_name)
        ))
        .map(|r| r.row_count() > 0)
        .unwrap_or(false);
    let mut query = if exists {
        format!("INSERT INTO {} ", sql_identifier(&table_name))
    } else {
        format!("CREATE TABLE {} AS ", sql_identifier(&table_name))
    };
    query += &format!("SELECT * FROM {function}({}", quoted_string(&file_name));
    for (key, value) in &parameters {
        query += &format!(", {}={}", sql_identifier(key), quoted_string(value));
    }
    query.push(')');
    if let Err(error) = shell.conn().query(&query) {
        shell.print_error(&format!(
            "Failed To Import Error: Failed to import from file '{file_name}'\n"
        ));
        shell.print_error(&error.to_string());
        return CommandResult::Fail;
    }
    CommandResult::Success
}

fn show_indexes(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 2 {
        return CommandResult::PrintUsage;
    }
    let mut query = "SELECT index_name FROM duckdb_indexes()".to_string();
    if let Some(table) = args.get(1) {
        query += &format!(" WHERE table_name LIKE {}", quoted_string(table));
    }
    query += " ORDER BY 1";
    run_list_query(shell, &query)
}

/// Prints the first column of each row of `query`.
fn run_list_query(shell: &mut Shell, query: &str) -> CommandResult {
    match shell.conn().query(query) {
        Ok(result) => {
            let mut text = String::new();
            for row in 0..result.row_count() {
                text += &result.value(0, row).try_to_string().unwrap_or_default();
                text.push('\n');
            }
            shell.print(&text);
            CommandResult::Success
        }
        Err(error) => {
            shell.print_error(&error.to_string());
            CommandResult::Fail
        }
    }
}

fn render_last(shell: &mut Shell, _: &[String]) -> CommandResult {
    let Some(result) = shell.last_result.take() else {
        shell.eprint("No result to render\n");
        return CommandResult::Fail;
    };
    let mode = shell.mode;
    shell.render_result(mode, &result, true);
    shell.last_result = Some(result);
    CommandResult::Success
}

fn set_large_number_rendering(shell: &mut Shell, args: &[String]) -> CommandResult {
    let mode = args[1].to_lowercase();
    shell.large_number_rendering = Some(match mode.as_str() {
        "all" => "all".to_string(),
        "footer" => "footer".to_string(),
        "off" => "off".to_string(),
        _ => {
            shell.eprint("Expected usage: .large_number_rendering [all|footer|off]\n");
            return CommandResult::Fail;
        }
    });
    CommandResult::Success
}

fn set_max_rows(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 3 {
        return CommandResult::PrintUsage;
    }
    if args.len() == 1 {
        let text = format!("current max rows: {}\n", shell.max_rows);
        shell.print(&text);
        return CommandResult::Success;
    }
    shell.max_rows = string_to_int(&args[1]) as usize;
    if args.len() > 2 {
        shell.max_analyze_rows = Some(string_to_int(&args[2]) as usize);
    }
    CommandResult::Success
}

fn set_max_width(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 2 {
        return CommandResult::PrintUsage;
    }
    if args.len() == 1 {
        let text = format!("current max rows: {}\n", shell.max_width);
        shell.print(&text);
    } else {
        shell.max_width = string_to_int(&args[1]) as usize;
    }
    CommandResult::Success
}

fn set_output_mode(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 3 {
        return CommandResult::PrintUsage;
    }
    if args.len() == 1 {
        let text = format!("current output mode: {}\n", shell.mode.name());
        shell.print(&text);
        return CommandResult::Success;
    }
    if shell.set_output_mode(&args[1], args.get(2).map(String::as_str)) {
        CommandResult::Success
    } else {
        CommandResult::Fail
    }
}

fn set_null_value(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.null_value = args[1].clone();
    CommandResult::Success
}

/// `ShellState::OpenDatabase`.
fn open_database(shell: &mut Shell, args: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".open cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    let mut new_flag = false;
    let mut read_only = false;
    let mut file: Option<String> = None;
    let mut has_sql = false;
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') {
        match args[i].trim_start_matches('-') {
            "new" => new_flag = true,
            "readonly" => read_only = true,
            "nofollow" => {}
            "sql" => {
                if has_sql {
                    shell.eprint("Error: --sql provided multiple times\n");
                    return CommandResult::Fail;
                }
                if i + 1 >= args.len() {
                    shell.eprint("Error: missing SQL query after --sql\n");
                    return CommandResult::Fail;
                }
                i += 1;
                match single_value(shell, &args[i]) {
                    Ok(value) => file = Some(value),
                    Err(message) => {
                        shell.eprint(&message);
                        return CommandResult::Fail;
                    }
                }
                has_sql = true;
            }
            _ => {
                shell.eprint(&format!("unknown option: {}\n", args[i]));
                return CommandResult::Fail;
            }
        }
        i += 1;
    }
    if has_sql && i < args.len() {
        shell.eprint("Error: cannot use both --sql and a FILE argument\n");
        return CommandResult::Fail;
    }
    if !has_sql && i < args.len() {
        file = Some(args[i].clone());
    }
    let file = file.unwrap_or_else(|| ":memory:".to_string());
    if new_flag && file != ":memory:" {
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(format!("{file}.wal"));
    }
    // close the current database first, so the file is free to open again
    shell.conn = None;
    shell.db = None;
    shell.last_result = None;
    shell.db_filename = file.clone();
    shell.read_only = read_only;
    if let Err(error) = shell.open_db() {
        shell.print_error(&error.to_string());
        shell.eprint(&format!("Error: cannot open '{file}'\n"));
        // fall back to an in-memory database
        shell.db_filename = ":memory:".to_string();
        shell.read_only = false;
        let _ = shell.open_db();
    }
    CommandResult::Success
}

/// `ExecuteSQLSingleValue`: the one value `sql` returns, or why there is not one.
fn single_value(shell: &mut Shell, sql: &str) -> Result<String, String> {
    let result = shell
        .conn()
        .query(sql)
        .map_err(|e| format!("Error: failed to evaluate --sql query '{sql}': {e}\n"))?;
    let statements = pintail_parser::parse_ast(sql).unwrap_or_default();
    let is_query = matches!(statements.last(), Some(pintail_parser::ast::Statement::Select(_)));
    if !is_query || result.row_count() == 0 {
        return Err("Error: --sql query returned no rows, expected single value\n".into());
    }
    if result.row_count() > 1 {
        return Err("Error: --sql query returned multiple rows, expected single value\n".into());
    }
    if result.column_count() != 1 {
        return Err("Error: --sql query returned multiple columns, expected single value\n".into());
    }
    let value = result.value(0, 0);
    if value.is_null() {
        return Err("Error: --sql query returned a null value\n".into());
    }
    Ok(value.try_to_string().unwrap_or_default())
}

/// `ShellState::SetOutputFile`.
fn set_output_file(shell: &mut Shell, args: &[String], once: bool) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".output/.once/.excel cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    let mut file = String::new();
    let mut bom = false;
    for arg in &args[1..] {
        if arg.starts_with('-') {
            let option = arg.trim_start_matches('-');
            if option == "bom" {
                bom = true;
            } else {
                shell.print(&format!("ERROR: unknown option: \"{arg}\".  Usage:\n"));
                return CommandResult::Fail;
            }
        } else if file.is_empty() {
            file = arg.clone();
        } else {
            shell.print(&format!("ERROR: extra parameter: \"{arg}\".  Usage:\n"));
            return CommandResult::Fail;
        }
    }
    if file.is_empty() {
        file = "stdout".to_string();
    }
    shell.out_count = if once { 2 } else { 0 };
    shell.reset_output();
    if file == "stdout" {
        return CommandResult::Success;
    }
    if file == "off" {
        shell.out = Output::Off;
        return CommandResult::Success;
    }
    match File::create(&file) {
        Ok(mut f) => {
            if bom {
                use std::io::Write;
                let _ = f.write_all(b"\xEF\xBB\xBF");
            }
            shell.out = Output::File(f, file);
            CommandResult::Success
        }
        Err(_) => {
            shell.eprint(&format!("Error: cannot write to \"{file}\"\n"));
            CommandResult::Fail
        }
    }
}

fn set_output_excel(shell: &mut Shell, _: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".output/.once/.excel cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    shell.eprint("Command \"excel\" is unsupported in the current version of the CLI\n");
    CommandResult::Fail
}

/// `ToggleLog`: the log goes nowhere yet; only the checks are ported.
fn toggle_log(shell: &mut Shell, _: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".log cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    CommandResult::Success
}

/// `SetPager`: there is no pager yet; the modes are accepted and a command is checked.
fn set_pager(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() == 3 {
        return CommandResult::Success;
    }
    if args.len() != 2 {
        return CommandResult::PrintUsage;
    }
    if !matches!(args[1].as_str(), "on" | "off" | "automatic") && shell.safe_mode {
        shell.eprint(".pager cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    CommandResult::Success
}

fn set_output(shell: &mut Shell, args: &[String]) -> CommandResult {
    set_output_file(shell, args, false)
}

fn set_output_once(shell: &mut Shell, args: &[String]) -> CommandResult {
    set_output_file(shell, args, true)
}

fn print_arguments(shell: &mut Shell, args: &[String]) -> CommandResult {
    let text = format!("{}\n", args[1..].join(" "));
    shell.print(&text);
    CommandResult::Success
}

fn read_from_file(shell: &mut Shell, args: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".read cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    // `active_read_files`: a file that reads itself would never end
    let canonical = std::fs::canonicalize(&args[1]).map_or_else(|_| args[1].clone(), |p| p.display().to_string());
    if shell.active_read_files.contains(&canonical) {
        shell.eprint(&format!("Error: recursive .read of \"{}\"\n", args[1]));
        return CommandResult::Fail;
    }
    shell.active_read_files.push(canonical);
    let result = read_file_contents(shell, args);
    shell.active_read_files.pop();
    result
}

fn read_file_contents(shell: &mut Shell, args: &[String]) -> CommandResult {
    let outcome = shell.process_file(&args[1], InputMode::File);
    if outcome.exit {
        return CommandResult::Exit;
    }
    if outcome.errors > 0 {
        CommandResult::Fail
    } else {
        CommandResult::Success
    }
}

fn enable_safe_mode(shell: &mut Shell, _: &[String]) -> CommandResult {
    shell.safe_mode = true;
    let conn = shell.conn();
    let _ = conn.query("SET enable_external_access=false");
    let _ = conn.query("SET lock_configuration=true");
    CommandResult::Success
}

fn set_separator(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() < 2 || args.len() > 3 {
        return CommandResult::PrintUsage;
    }
    shell.col_separator = args[1].clone();
    if args.len() >= 3 {
        shell.row_separator = args[2].clone();
    }
    CommandResult::Success
}

/// `.schema`: the CREATE statements, pretty-printed unless `--no-indent`.
fn display_schemas(shell: &mut Shell, args: &[String]) -> CommandResult {
    let mut name: Option<&str> = None;
    let mut format = true;
    for arg in &args[1..] {
        let option = arg.trim_start_matches('-');
        if arg.starts_with('-') && (option == "indent" || option == "format") {
            format = true;
        } else if arg.starts_with('-') && (option == "no-indent" || option == "no-format") {
            format = false;
        } else if arg.starts_with('-') {
            shell.eprint(&format!(
                "Error: unknown option \"{arg}\"\nUsage: .schema ?--indent|--no-indent? ?LIKE-PATTERN?\n"
            ));
            return CommandResult::Fail;
        } else if name.is_none() {
            name = Some(arg);
        } else {
            shell.eprint("Usage: .schema ?--indent|--no-indent? ?LIKE-PATTERN?\n");
            return CommandResult::Fail;
        }
    }
    let mut query = "SELECT sql FROM sqlite_master WHERE ".to_string();
    if let Some(name) = name {
        let glob = name.contains(['*', '?', '[']);
        query += if name.contains('.') {
            "lower(printf('%s.%s',sname,tbl_name))"
        } else {
            "lower(tbl_name)"
        };
        query += if glob { " GLOB " } else { " LIKE " };
        query += &quoted_string(name);
        if !glob {
            query += " ESCAPE '\\' ";
        }
        query += " AND ";
    }
    query += "type!='meta' AND sql IS NOT NULL ORDER BY name";
    match shell.conn().query(&query) {
        Ok(result) => {
            let mut text = String::new();
            for row in 0..result.row_count() {
                let sql = result.value(0, row).try_to_string().unwrap_or_default();
                text += &if format { format_create(&sql) } else { sql };
                text.push('\n');
            }
            shell.print(&text);
            CommandResult::Success
        }
        Err(error) => {
            shell.print_error(&error.to_string());
            shell.eprint("Error: querying schema information\n");
            CommandResult::Fail
        }
    }
}

/// `duckdb_format_sql` for the statements `.schema` shows: a CREATE TABLE's columns one per line.
fn format_create(sql: &str) -> String {
    let Some(open) = sql.find('(') else {
        return sql.to_string();
    };
    if !sql[..open].to_uppercase().contains("CREATE TABLE") || sql[..open].to_uppercase().contains(" AS ") {
        return sql.to_string();
    }
    // the matching parenthesis
    let bytes = sql.as_bytes();
    let mut depth = 0;
    let mut close = None;
    let mut quote: Option<u8> = None;
    for (i, &c) in bytes.iter().enumerate().skip(open) {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return sql.to_string();
    };
    let inner = &sql[open + 1..close];
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut quote: Option<u8> = None;
    for (i, &c) in inner.as_bytes().iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(inner[start..].trim());
    let body: Vec<String> = parts.iter().map(|p| format!("    {p}")).collect();
    format!(
        "{}(\n{}\n){}",
        sql[..open].trim_end(),
        body.join(",\n"),
        &sql[close + 1..]
    )
}

fn run_shell_command(shell: &mut Shell, args: &[String]) -> CommandResult {
    if shell.safe_mode {
        shell.eprint(".sh/.system cannot be used in -safe mode\n");
        return CommandResult::Fail;
    }
    if args.len() < 2 {
        return CommandResult::PrintUsage;
    }
    let quote = |a: &String| if a.contains(' ') { a.clone() } else { format!("\"{a}\"") };
    let command: Vec<String> = args[1..].iter().map(quote).collect();
    shell.flush();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command.join(" "))
        .status();
    match status {
        Ok(status) if !status.success() => {
            let code = status.code().unwrap_or(1);
            shell.eprint(&format!("System command returns {}\n", code << 8));
        }
        Err(_) => shell.eprint("System command returns -1\n"),
        _ => {}
    }
    CommandResult::Success
}

/// `ShellState::ShowConfiguration`.
fn show_configuration(shell: &mut Shell, _: &[String]) -> CommandResult {
    let on = |b: bool| if b { "on" } else { "off" };
    let mut text = String::new();
    text += &format!("{:>12}: {}\n", "echo", on(shell.echo));
    text += &format!("{:>12}: {}\n", "headers", on(shell.show_header));
    text += &format!("{:>12}: {}\n", "mode", shell.mode.name());
    text += &format!("{:>12}: {}\n", "nullvalue", escape_c_string(&shell.null_value));
    let output = match &shell.out {
        Output::File(_, name) => name.clone(),
        _ => "stdout".to_string(),
    };
    text += &format!("{:>12}: {}\n", "output", output);
    text += &format!("{:>12}: {}\n", "colseparator", escape_c_string(&shell.col_separator));
    text += &format!("{:>12}: {}\n", "rowseparator", escape_c_string(&shell.row_separator));
    let widths: String = shell.col_width.iter().map(|w| format!("{w} ")).collect();
    text += &format!("{:>12}: {}\n", "width", widths);
    text += &format!("{:>12}: {}\n", "filename", shell.db_filename);
    text += &format!("{:>12}: {}\n", "agent", "off");
    shell.print(&text);
    CommandResult::Success
}

/// `ShellState::DisplayTables`: the tables and views matching a pattern, with their columns.
fn show_tables(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() > 2 {
        return CommandResult::PrintUsage;
    }
    let pattern = args.get(1).cloned().unwrap_or_default();
    let mut schema_filter = String::new();
    let mut table_filter = format!("%{pattern}%");
    if let Some((schema, table)) = pattern.split_once('.') {
        let unquote = |s: &str| s.trim_matches('"').to_string();
        schema_filter = format!("%{}%", unquote(schema));
        table_filter = format!("%{}%", unquote(table));
    }
    let mut filters = format!(" AND columns.table_name ILIKE {}", quoted_string(&table_filter));
    if !schema_filter.is_empty() {
        filters += &format!(" AND columns.schema_name ILIKE {}", quoted_string(&schema_filter));
    }
    let query = format!(
        "SELECT columns.database_name, columns.schema_name, columns.table_name, list(struct_pack(column_name, \
         data_type, is_primary_key := c.column_index IS NOT NULL) ORDER BY column_index), t.estimated_size AS \
         estimated_size, t.table_oid AS table_oid FROM duckdb_columns() columns LEFT JOIN duckdb_tables() t USING \
         (table_oid) LEFT JOIN (SELECT table_oid, UNNEST(constraint_column_indexes)+1 column_index FROM \
         duckdb_constraints() WHERE constraint_type='PRIMARY KEY') c USING (table_oid, column_index) WHERE NOT \
         columns.internal{filters} GROUP BY ALL ORDER BY ALL"
    );
    let result = match shell.conn().query(&query) {
        Ok(result) => result,
        Err(error) => {
            shell.print_error(&error.to_string());
            return CommandResult::Fail;
        }
    };
    let text = |value: pintail::Value| value.try_to_string().unwrap_or_default();
    let mut tables = Vec::new();
    for row in 0..result.row_count() {
        let mut table = TableInfo {
            database_name: text(result.value(0, row)),
            schema_name: text(result.value(1, row)),
            table_name: text(result.value(2, row)),
            ..Default::default()
        };
        if let ValueData::List(columns) = result.value(3, row).data() {
            for column in columns {
                let fields = match column.data() {
                    ValueData::Struct(fields) => fields.clone(),
                    _ => Vec::new(),
                };
                let field = |i: usize| fields.get(i).cloned().map(text).unwrap_or_default();
                table.columns.push(ColumnInfo {
                    name: field(0),
                    ty: field(1),
                    is_primary_key: field(2) == "true",
                    ..Default::default()
                });
            }
        }
        let size = result.value(4, row);
        if !size.is_null() {
            table.estimated_size = text(size).parse().ok();
        }
        table.is_view = result.value(5, row).is_null();
        tables.push(table);
    }
    shell.render_table_metadata(&tables);
    CommandResult::Success
}

/// `ShellState::ToggleTimer`.
fn toggle_timer(shell: &mut Shell, args: &[String]) -> CommandResult {
    if args.len() < 2 || args.len() > 3 {
        return CommandResult::PrintUsage;
    }
    if args.len() == 3 {
        let digits = string_to_int(&args[2]);
        if !(0..=9).contains(&digits) {
            shell.eprint(".timer DIGITS must be between 0 and 9\n");
            return CommandResult::Fail;
        }
        shell.timer_digits = digits as usize;
    }
    shell.timer = shell.string_to_bool(&args[1]);
    CommandResult::Success
}

fn show_version(shell: &mut Shell, _: &[String]) -> CommandResult {
    let text = format!("{}\n", Shell::version());
    shell.print(&text);
    CommandResult::Success
}

fn set_widths(shell: &mut Shell, args: &[String]) -> CommandResult {
    shell.col_width = args[1..].iter().map(|a| string_to_int(a)).collect();
    CommandResult::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_split_and_unescaped() {
        assert_eq!(split_arguments(".mode csv"), vec!["mode", "csv"]);
        assert_eq!(split_arguments(".sep \"a\\tb\" '|'"), vec!["sep", "a\tb", "|"]);
        assert_eq!(resolve_backslashes("\\101"), "A");
    }

    #[test]
    fn commands_match_by_prefix() {
        assert_eq!(find_command("mo").unwrap().name, "mode");
        assert_eq!(find_command("sep").unwrap().name, "separator");
        assert_eq!(find_command("h").unwrap().name, "headers");
        // `.tables` needs two letters and `.thousand_sep` four, so `.t` is `.timer`
        assert_eq!(find_command("ta").unwrap().name, "tables");
        assert_eq!(find_command("t").unwrap().name, "timer");
        assert!(find_command("th").is_err());
        assert_eq!(find_command("thou").unwrap().name, "thousand_sep");
        assert!(find_command("xyz").is_err());
    }

    #[test]
    fn create_table_is_indented() {
        assert_eq!(
            format_create("CREATE TABLE test(a INTEGER, b VARCHAR);"),
            "CREATE TABLE test(\n    a INTEGER,\n    b VARCHAR\n);"
        );
    }
}
