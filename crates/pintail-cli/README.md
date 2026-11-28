# pintail-cli

`pintail-cli` builds the `pintail` binary, the command-line shell of pintail. It is a port of DuckDB's
`tools/shell` (`shell.cpp`, `shell_metadata_command.cpp`, `shell_renderer.cpp`, `shell_highlight.cpp`,
`shell_render_table_metadata.cpp`) and of `src/common/box_renderer.cpp`, which draws the default
"duckbox" output. It sits at the top of the stack: it opens a database and runs SQL through the public
Rust API in [`pintail`](../pintail), parses statements with `pintail-parser` to decide how to show their
results, and uses a few helpers from `pintail-common` and `pintail-core`. No other crate depends on it.

## At a glance

| | |
|---|---|
| Lines of Rust | 10,473 (`find src -name '*.rs' \| xargs cat \| wc -l`), of which 1,962 are the test modules |
| Binary | `pintail` (`[[bin]]` in [`Cargo.toml`](Cargo.toml), from [`src/main.rs`](src/main.rs)) |
| Entry points | `run()` in `main.rs` (options, init file, input loop), `Shell` (the `ShellState` of C++), `commands::do_meta_command` (dot commands) |
| C++ ported | `duckdb/tools/shell/*.cpp`, `duckdb/src/common/box_renderer.cpp` |

## Building and running

```
cargo build --release -p pintail-cli
./target/release/pintail                 # in-memory database, interactive if stdin is a terminal
./target/release/pintail my.db           # open or create a database file
./target/release/pintail my.db "SELECT 42"   # run SQL (or a dot command) and exit
echo "SELECT 1;" | ./target/release/pintail  # batch input from a pipe
```

The usage line is `pintail [OPTIONS] FILENAME [SQL]`. The first argument that does not start with `-`
is the database file (`:memory:` if none is given). Every later such argument is a SQL statement or a
dot command that runs after the options, and stdin is then not read. A file with a `.parquet`, `.csv`
or `.json` name opens as an in-memory database with views over the file (`OpenFileStorageExtension`,
see [ARCHITECTURE.md](../../docs/ARCHITECTURE.md#shell-pintail-cli)).

At startup in interactive mode the shell prints `v<crate version> (pintail)` and
`Enter ".help" for usage hints.`. The prompt is `D `, and `‣ ` while a statement continues on the next
line.

## Dependencies

| Crate | Used for |
|---|---|
| `pintail` | `Database`, `Connection`, `QueryResult`, `Error`; `Connection::add_replacement_scan` for `_` (the last result) |
| `pintail-parser` | `parse_ast_with_text` splits input into statements with their text; the AST says whether a result is rows to show or a DESCRIBE; the tokenizer finds `?` parameters |
| `pintail-common` | `string_util::candidates_error_message` ("Did you mean ..." for unknown options and commands), `storage_version` for `-storage-version`, logical types |
| `pintail-core` | `function::json::value_to_json`, the text of nested values in the `json` and `jsonlines` modes |
| `rustyline` | line editing and history for interactive input (linenoise in C++) |
| `libc` | `getrusage` for the user and system times of `.timer` |

## Layout

| File | What it does | Ported from |
|---|---|---|
| [`src/main.rs`](src/main.rs) | `Shell` (settings and state), `RenderMode`, the command-line options (`OPTIONS`), `run()`, `process_input` (statement gathering), `execute_sql` / `execute_statement`, `format_error` (the `LINE n:` pointer), the init file, the `Interactive` (rustyline) and `ReaderInput` line sources | `tools/shell/shell.cpp` |
| [`src/commands.rs`](src/commands.rs) | the dot-command table (`COMMANDS`), argument splitting and backslash escapes, prefix lookup, every command's handler | `tools/shell/shell_metadata_command.cpp` |
| [`src/renderers.rs`](src/renderers.rs) | every output mode except duckbox: column-style (`render_columns`) and row-style (`render_rows`) renderers, escaping for CSV, HTML, JSON, SQL | `tools/shell/shell_renderer.cpp` |
| [`src/box_renderer.rs`](src/box_renderer.rs) | `BoxRendererConfig`, `render`, the duckbox table: column widths, truncation, hidden rows and columns, the footer, large-number rendering, terminal width | `src/common/box_renderer.cpp` |
| [`src/box_renderer/json.rs`](src/box_renderer/json.rs) | `JSONParser` / `JSONFormatter`: nested or JSON values spread over several lines when their column is narrow | `src/common/box_renderer.cpp` |
| [`src/highlight.rs`](src/highlight.rs) | `Highlighter`: colours, the dark/light/mixed modes, error and SQL highlighting, the highlighting dot commands, `.display_colors` | `tools/shell/shell_highlight.cpp`, `linenoise/highlighting.cpp`, the error tokenizer of `src/parser/parser.cpp` |
| [`src/table_metadata.rs`](src/table_metadata.rs) | `TableInfo`, `ColumnInfo`, `render_table_metadata`: boxes per table for `.tables`, `.databases` and DESCRIBE | `tools/shell/shell_render_table_metadata.cpp` |
| `src/*/tests.rs`, `src/*/*.txt` | unit tests and the expected outputs they compare with | |

## Command-line options

From `OPTIONS` in `main.rs`. An option may be written with one or two dashes, and `_` in its name may
be written as `-`. An unknown option fails with a "Did you mean" hint.

| Option | Argument | What it does |
|---|---|---|
| `-ascii` | | output mode `ascii` (column separator 0x1F, row separator 0x1E) |
| `-bail` | | stop after hitting an error |
| `-batch` | | force batch I/O (stdin is not treated as interactive) |
| `-box` | | output mode `box` |
| `-column` | | output mode `column` |
| `-cmd` | COMMAND | run COMMAND before reading stdin |
| `-csv` | | output mode `csv` |
| `-c` | COMMAND | run COMMAND and exit |
| `-echo` | | print commands before running them |
| `-f` | FILENAME | read and run the file, then exit |
| `-init` | FILENAME | read and run the file instead of `~/.duckdbrc` |
| `-header` | | turn headers on |
| `-h`, `-help` | | print the usage and exit |
| `-html` | | output mode `html` |
| `-interactive` | | force interactive I/O |
| `-json` | | output mode `json` |
| `-jsonlines` | | output mode `jsonlines` |
| `-line` | | output mode `line` |
| `-list` | | output mode `list` |
| `-markdown` | | output mode `markdown` |
| `-newline` | SEP | row separator (default `\n`; backslash escapes are resolved) |
| `-no-init` | | skip the init file |
| `-no-stdin` | | exit after the options instead of reading stdin |
| `-noheader` | | turn headers off |
| `-nullvalue` | TEXT | text for NULL values (default `NULL`) |
| `-quote` | | output mode `quote` |
| `-readonly` | | open the database read-only |
| `-s` | COMMAND | run COMMAND and exit (same as `-c`) |
| `-safe` | | safe mode: sets `enable_external_access=false` and `lock_configuration=true`, and refuses `.read`, `.open`, `.import`, `.output`, `.once`, `.log` and pager commands |
| `-separator` | SEP | column separator (default `\|`) |
| `-storage-version` | VER | storage version for a new database file; an unknown version prints the valid ones |
| `-table` | | output mode `table` |
| `-unredacted` | | accepted; does nothing yet |
| `-unsigned` | | accepted; does nothing yet |
| `-version` | | print the version and exit |

As in C++, the options are read in two passes. The first pass takes the database name and the options
needed before the database opens (`-readonly`, `-safe`, `-storage-version`, `-init`, `-no-init`, and
the ones that make stdin non-interactive). Then the database opens, the init file runs, and the second
pass applies the rest, so the command line overrides the init file. `-c`, `-s` and `-cmd` stop at an
error unless `.bail off` was set.

## Init file and history

- **Init file.** Unless `-no-init` is given, the shell runs `-init FILE`, or else `$HOME/.duckdbrc`, before
  the second pass of options. A missing `~/.duckdbrc` is ignored; a missing `-init` file is an
  `IO Error`. Errors in the init file stop the shell with
  `Encountered errors while executing init file "..."` unless `.bail off` was set.
- **History.** Interactive input uses rustyline. The history is loaded from `$DUCKDB_HISTORY`, or else
  `$HOME/.duckdb_history`, and saved there on exit. Each dot command and each complete statement is
  added. Ctrl-C drops the statement typed so far.
- **Highlighting as you type.** The rustyline helper (`EditorHelper`) colours the line with
  `Highlighter::highlight_sql`. `NO_COLOR` in the environment turns colours off.

## Output modes

Set with `.mode NAME` or the options above. `.mode` takes a prefix of the name (`lines` is `line`).
The default is `duckbox`.

| Mode | Drawn by | Output |
|---|---|---|
| `duckbox` | `box_renderer.rs` | the default: a box table with column types, truncated to the width and to `.maxrows` rows (40) with a footer of hidden rows |
| `box` | `renderers.rs` (columns) | a box table of all rows, newlines shown as `\n` |
| `column` | `renderers.rs` (columns) | aligned columns with a dashed line under the header; widths from `.width` |
| `table` | `renderers.rs` (columns) | an ASCII `+---+` table |
| `markdown` | `renderers.rs` (columns) | a Markdown table, `\|` escaped |
| `latex` | `renderers.rs` (columns) | a LaTeX `tabular` |
| `line` | `renderers.rs` (rows) | one `name = value` line per value |
| `list` | `renderers.rs` (rows) | values joined by the separator (`\|`) |
| `tabs` | `renderers.rs` (rows) | `list` with a tab separator |
| `csv` | `renderers.rs` (rows) | comma-separated, quoted where needed, CRLF row ends |
| `html` | `renderers.rs` (rows) | `<tr><td>` rows |
| `tcl` | `renderers.rs` (rows) | Tcl list elements |
| `ascii` | `renderers.rs` (rows) | unit and record separators (0x1F, 0x1E) |
| `quote` | `renderers.rs` (rows) | SQL literals, comma-separated |
| `json` | `renderers.rs` (rows) | a JSON array of objects |
| `jsonlines` | `renderers.rs` (rows) | one JSON object per line |
| `insert` | `renderers.rs` (rows) | `INSERT INTO table VALUES(...)`; `.mode insert TABLE` names the table |
| `trash` | | nothing is printed |

In duckbox mode two statements are drawn another way. An `EXPLAIN` prints the plan text of each row
(the internal `explain` mode). `DESCRIBE name`, `DESCRIBE (query)` and `SHOW name` are drawn as table
metadata with `table_metadata.rs`. `.columns` turns the duckbox table on its side (one row per column)
and `.rows` turns it back.

## Dot commands

From `COMMANDS` in `commands.rs`, in its order. A command may be shortened to any prefix of at least
the length in the "Min" column (any length when empty); the first command in the table that matches
wins, so `.d` is `.dump` and `.sep` is `.separator`. A wrong number of arguments prints
`Invalid usage of command`. "No-op" commands succeed and do nothing; "unsupported" ones print
`Command "X" is unsupported in the current version of the CLI` and fail.

| Command | Arguments | Min | What it does |
|---|---|---|---|
| `.about` | | | print the about text |
| `.auto_format` | on\|off | 3 | no-op |
| `.bail` | on\|off\|auto | 3 | stop after an error (auto: in files and the init file only) |
| `.binary` | on\|off | 3 | no-op |
| `.cd` | DIRECTORY | | change the working directory |
| `.changes` | on\|off | 3 | after each statement print `changes: N   total_changes: M` |
| `.columns` | | | column-wise duckbox output |
| `.comment` `.constant` `.cont` `.cont_sel` `.error` `.keyword` | ?COLOR? | | deprecated: set one highlighting colour |
| `.decimal_sep` | SEP | 3 | decimal separator for duckbox numbers |
| `.databases` | | 2 | list attached databases and their files, as table metadata |
| `.dump` | ?TABLE? | | write the database (or matching tables) as SQL |
| `.display_colors` | [bold\|underline] | | show the terminal colours and their names |
| `.echo` | on\|off | 3 | print each command before it runs |
| `.edit` | | | unsupported |
| `.excel` | | | unsupported (fails in safe mode first) |
| `.exit` | ?CODE? | | exit with return code CODE |
| `.headers` | on\|off | | headers on or off |
| `.help` | ?-all? ?PATTERN? | | help for the matching commands |
| `.highlight` | on\|off | | syntax highlighting on or off |
| `.highlight_colors` | OPTIONS | | set a highlighting colour (`COMPONENT COLOR [INTENSITY]`) |
| `.highlight_errors` | on\|off | | highlighting of errors |
| `.highlight_mode` | mixed\|dark\|light | | the colour mode |
| `.highlight_results` | on\|off | | highlighting of results (the setting only; see gaps) |
| `.history` | | | no-op |
| `.import` | FILE TABLE | | load a file into a table with `read_csv`, `read_parquet` or `read_json` (chosen by `--csv`, `--parquet`, `--json` or the file extension) |
| `.indexes`, `.indices` | ?TABLE? | | list index names |
| `.last` | | | draw the last result in full, without the row and width limits |
| `.large_number_rendering` | MODE | | readable large numbers in duckbox (`all`, `footer`, or off) |
| `.log` | FILE\|off | | accepted; logs nowhere yet (fails in safe mode) |
| `.manual` | FUNCTION | | unsupported |
| `.maxbytes` | COUNT | | no-op (agent mode only in C++) |
| `.maxcellwidth` | COUNT | | no-op (agent mode only in C++) |
| `.maxrows` | COUNT ?ANALYZE? | | rows shown in duckbox (default 40), and rows looked at for widths |
| `.maxwidth` | COUNT | | duckbox width; 0 is the terminal width |
| `.mode` | MODE ?TABLE? | | set the output mode |
| `.nullvalue` | STRING | | text for NULL |
| `.open` | ?OPTIONS? ?FILE? | 2 | close the database and open FILE (`--new`, `--readonly`, `--nofollow`, `--sql`) |
| `.once` | ?OPTIONS? ?FILE? | | send the next command's output to FILE (`--bom`) |
| `.output` | ?FILE? | | send output to FILE, or stdout |
| `.pager` | on\|off\|auto\|COMMAND | | accepted; there is no pager yet |
| `.print` | STRING... | 3 | print the arguments |
| `.progress_bar` | OPTIONS | | no-op |
| `.prompt` | MAIN CONTINUE | | no-op |
| `.quit` | | | exit |
| `.read` | FILE | 3 | run the commands in FILE (a file that reads itself is an error) |
| `.read_line_version` | linenoise\|fallback | | no-op |
| `.render_completion` | on\|off | | no-op |
| `.render_errors` | on\|off | | no-op |
| `.rows` | | | row-wise duckbox output (the default) |
| `.safe_mode` | | | turn on safe mode |
| `.separator` | COL ?ROW? | | column and row separators |
| `.serve_command` | COMMAND | | unsupported |
| `.schema` | ?PATTERN? | | the CREATE statements, indented unless `--no-indent` |
| `.shell`, `.system` | CMD ARGS... | | run a command in the system shell |
| `.show` | | | print the current settings |
| `.startup_text` | none\|version\|all | | no-op |
| `.tables` | ?TABLE? | 2 | tables matching a LIKE pattern, as table metadata |
| `.thousand_sep` | SEP | 4 | thousand separator for duckbox numbers |
| `.timer` | on\|off [DIGITS] | | print `Run Time (s): real ... user ... sys ...` after each statement |
| `.version` | | | print the version |
| `.width` | NUM1 NUM2 ... | | minimum column widths for `column` mode |

## Key types and APIs

The crate is a binary; its items are `pub` only so the modules can share them.

- `Shell` (`main.rs`): the shell state (`ShellState`): the database and connection, the mode,
  separators, `.maxrows`/`.maxwidth`, the output target (`Output`), bail, echo, timer, safe mode, the
  last result, and the `Highlighter`. `execute_sql`, `process_input`, `process_file`, `set_output_mode`,
  `box_config` and `render_result` are its main methods.
- `LineSource` (`main.rs`): where lines come from. `ReaderInput` reads a file or pipe; `Interactive`
  wraps a rustyline editor.
- `commands::do_meta_command(shell, line) -> MetaOutcome`: runs one dot command.
- `box_renderer::render(config, names, types, result) -> String` with `BoxRendererConfig`; the
  `ResultRenderer` trait receives the pieces (layout, names, types, values, NULLs, footer), and
  `StringResultRenderer` collects them into one string.
- `renderers::render(shell, mode, result) -> String`: the other modes.
- `table_metadata::render_table_metadata(&MetadataConfig, &[TableInfo]) -> String`.
- `highlight::Highlighter`: returns coloured text and never writes; `error`, `highlight_sql`,
  `display_colors`, and setters for the highlighting commands.

## How it works

**Reading input.** `process_input` reads lines from a `LineSource`. A line that starts with `.` when no
statement is pending is a dot command; a line that starts with `#` is skipped. Other lines are joined
until the text ends in a `;` outside quotes, comments and dollar-quoted strings (`sql_is_complete`).
After an error the loop stops if bail applies: `.bail on`, or the default `auto` when reading a file
(`-f`, `.read`) or the init file.

**Running a statement.** `execute_sql` parses the text with `pintail_parser::parse_ast_with_text`, which
gives each statement with its own text. A parse error is run through the engine once more so that it
aborts an open transaction, then printed by `format_error`, which adds a `LINE n:` line with a `^` under
the error position. Each statement runs on its own with `Connection::query`. A statement with a `?`
parameter outside PREPARE is refused, as in C++. INSERT, UPDATE, DELETE, MERGE and COPY answer with a
`Count` column; the shell keeps it for `.changes` and prints nothing. DDL and `SET` answer with an empty
`Success` column and print nothing. Other results go to the renderer for the current mode, and the
rows are saved so that `.last` and the table `_` can use them. `_` works through a replacement scan
(a callback for table names the catalog does not know) installed when the database opens.

**Duckbox.** `Shell::box_config` turns the shell settings into a `BoxRendererConfig`: `max_width` 0
means the terminal width, or no limit when stdout is not a terminal; output to a file shows all rows;
large numbers get a footer only on a console. The renderer measures each column over up to
`max_analyze_rows` rows, shrinks or hides columns that do not fit, keeps the first and last rows when
there are more than `max_rows`, and writes a footer with the row and column counts. Narrow nested or
JSON values are reflowed by `json.rs`. The terminal width comes from `ioctl(TIOCGWINSZ)` on stdin
(120 when stdin is not a terminal, as C++).

**Table metadata.** `.tables` queries `duckdb_columns()`, `duckdb_tables()` and `duckdb_constraints()`
into `TableInfo`s; `.databases` uses `pragma_database_list`; DESCRIBE uses its own result. The renderer
draws one box per table under database and schema headings and packs boxes side by side within the
render width.

## Status and gaps

From [`docs/REMAINING_WORK.md`](../../docs/REMAINING_WORK.md) item 42 and the code. Against
`duckdb/tools/shell/tests` the binary passes 150 tests of `test_shell_basics` (the C++ 1.5.6 shell passes
141), and all of `test_dump`, `test_errors`, `test_import`, `test_table_metadata_rendering` and
`test_shell_rendering`.

Done: batch and interactive input with history and highlighting as you type; the options, `-c`, `-s`,
`-cmd`, `-f`, `-init` and `~/.duckdbrc`; bail, echo and safe mode; every output mode; the dot commands
except those below; table metadata for `.tables`, `.databases` and DESCRIBE; error and SQL
highlighting; `_` as the last result; opening or attaching Parquet, CSV and JSON files; COPY to and from
stdin, stdout and stderr.

Not done:
- EXPLAIN and `query_tree` profiling output need the engine's text tree renderer
  (`text_tree_renderer.cpp`); the engine prints an indented plan where C++ prints boxes.
- The functions the shell extension registers, `getenv` and `shell_history` (needs item 49).
- `-serve` / `-connect` (the quack protocol) and agent mode; `MetadataConfig::compact` exists but is
  never set.
- `.manual`, `.edit`, `.excel` and `.serve_command` are unsupported. `.log` and `.pager` only check
  their arguments. `.progress_bar`, `.prompt`, `.history`, `.auto_format`, `.binary`,
  `.read_line_version`, `.render_completion`, `.render_errors`, `.startup_text`, `.maxbytes` and
  `.maxcellwidth` are no-ops.
- Autocomplete (the rustyline `Completer` and `Hinter` are empty), highlighting of duckbox results and
  of table metadata, and the layout of `.help`.
- `pragma_version` / `version()`: the shell prints the crate version (`Shell::version`) instead of a
  DuckDB version.
- `-unsigned` and `-unredacted` are accepted and ignored.
- Engine gaps the shell tests reach: `SET TimeZone` (ICU, item 50), `integer_division`,
  `preserve_identifier_case`, `lock_configuration`, CSV cardinality estimates for pipes.

## Testing

Unit tests (86 in all) run with:

```
cargo test -p pintail-cli
```

- `src/box_renderer/tests.rs` (44): duckbox output compared byte for byte with `duckdb --batch` 1.5.6
  with the settings the shell passes in batch mode.
- `src/table_metadata/tests.rs` (18): `.tables`, `.databases` and DESCRIBE layouts against the
  expected outputs in `src/table_metadata/*.txt`, taken from DuckDB 1.5.6 with colours stripped.
- `src/highlight/tests.rs` (16): error highlighting and `.display_colors` against the 1.5.6 CLI
  (`src/highlight/display_colors*.txt`), except where the baseline source differs.
- `main.rs` (5) and `commands.rs` (3): statement completion, size suffixes, error positions, DESCRIBE
  names, argument splitting, prefix lookup, `.schema` indentation.

The main check is DuckDB's own pytest suite for the shell, `duckdb/tools/shell/tests`. Its
`conftest.py` takes the binary to test with `--shell-binary`. From the repository root:

```
cargo build --release -p pintail-cli
cd ../duckdb && PYTHONPATH=. pytest tools/shell/tests --shell-binary ../duckrs/target/release/pintail
```

Change `../duckrs` to the checkout or worktree that holds your build. A single file can be run with,
for example, `pytest tools/shell/tests/test_shell_basics.py --shell-binary ...`.

## Notes for contributors

- Each file starts with a `// Ported from duckdb/...` header naming its C++ source; doc comments name
  the C++ function a Rust function ports (`ShellState::ExecuteStatement`, `SetOutputMode`, ...).
- Messages are copied from C++ word for word, including its typos (for example `force batch I/O'`),
  since the shell tests compare output text. The dot commands stay in the C++ table order, because
  prefix lookup takes the first match.
- Expected outputs in tests come from the reference DuckDB CLI, not from the port.
- The renderers and the highlighter are pure: they return strings, and only `Shell::print` and
  `Shell::eprint` write.
- The crate uses the engine only through `pintail` and the parser, plus the two helpers listed under
  Dependencies. Engine changes the shell needs (like `add_replacement_scan`) go in the engine crates
  as general hooks.
- `unsafe`: the crate has two existing uses, `getrusage` in `main.rs` and `ioctl` in
  `box_renderer.rs`. Per `../CLAUDE.md` they will be audited later; don't add more. Write a safe
  wrapper in `pintail-common` if you need another system call.
- `cargo clippy --workspace --all-targets -- -D warnings` is the CI gate.
