# pintail-parser

`pintail-parser` turns SQL text into an AST (abstract syntax tree: the statement as Rust data,
before any names are resolved). It is a port of DuckDB's PEG parser, `duckdb/src/parser/peg`, and
of the parsed expression, query-node, table-ref and statement classes in `duckdb/src/parser`. A
PEG (parsing expression grammar) is a grammar where alternatives are tried in order and the first
match wins. The parser does not have its own copy of the SQL grammar: it reads DuckDB's own
grammar file, vendored in [`grammar/`](grammar/). In the stack it sits right above
`pintail-common`, its only workspace dependency. `pintail-core`, `pintail-main` and `pintail-cli`
depend on it. See [`../../docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the whole stack.

## At a glance

| | |
|---|---|
| Lines of Rust (`src/`) | 14,025 |
| Main entry points | `parse_ast`, `parse_ast_with`, `parse_ast_with_text`, `parse`, `parse_on_this_thread`, `ParserOptions`, `ast::Statement`, `transformer::Transformer`, `Grammar::duckdb()` |
| C++ it ports | `src/parser/peg` (grammar loader, tokenizer, matcher, keyword maps, transformer), `src/parser/{expression,query_node,tableref,statement}`, `Parser::StripUnicodeSpaces` in `src/parser/parser.cpp` |

## Dependencies

| Crate | Used for |
|---|---|
| `pintail-common` | `Error` / `ErrorKind` / `Result`, and `LogicalType` in the AST |
| `sqllogictest` (dev, `tools/sqllogictest`) | reading the `.test` files in the `corpus` example |
| `proptest` (dev) | listed in `Cargo.toml`; no test uses it yet |

No other external crates.

## Layout

| Path | What it does | Ported from (`duckdb/...`) |
|---|---|---|
| [`src/lib.rs`](src/lib.rs) | Public entry points, `ParserOptions`, the statement loop, syntax errors | `src/parser/peg/peg_parser.cpp`, `PEGSyntaxError` |
| [`src/unicode.rs`](src/unicode.rs) | Replaces Unicode spaces outside quotes and comments with ASCII spaces | `src/parser/parser.cpp` (`StripUnicodeSpaces`) |
| [`src/tokenizer.rs`](src/tokenizer.rs) | Splits SQL into `Token`s (keyword, string, number, operator, identifier, `;`, end) | `src/parser/peg/tokenizer/{base_tokenizer,parser_tokenizer}.cpp` |
| [`src/keywords.rs`](src/keywords.rs) | The keyword map: category bits per word, read from the `.list` files | `src/parser/peg/keyword_helper/default_keyword_maps.cpp` |
| [`src/grammar.rs`](src/grammar.rs) | Parses the `.gram` text and compiles it into `Node`s; terminal overrides | `src/parser/peg/peg_parser.cpp`, `compiled_grammar.cpp`, `matcher_factory.cpp` |
| [`src/matcher.rs`](src/matcher.rs) | Packrat matcher: runs the grammar over the tokens, builds `ParseNode` trees | `src/parser/peg/{matcher,matcher_stack,parser_packrat}.cpp` |
| [`src/pool.rs`](src/pool.rs) | Pool of parser threads with large stacks | (Rust only) |
| [`src/transformer.rs`](src/transformer.rs) | Parse tree to AST: expressions, SELECT, common parts; AST visitors | `src/parser/peg/transformer/{transform_expression,transform_select,transform_common,transform_create_view}.cpp` |
| [`src/transformer/statements.rs`](src/transformer/statements.rs) | INSERT, UPDATE, DELETE, CREATE TABLE/INDEX, DROP, transactions, CHECKPOINT, CALL, SET, PRAGMA, EXPLAIN | `transform_{insert,update,delete,create_table,create_index,drop,transaction,checkpoint,call,set,pragma,explain}.cpp` |
| [`statements/alter.rs`](src/transformer/statements/alter.rs) | ALTER, COMMENT ON, SHOW / DESCRIBE / SUMMARIZE | `transform_{alter,comment,describe}.cpp` |
| [`statements/pivot.rs`](src/transformer/statements/pivot.rs) | PIVOT / UNPIVOT, MERGE INTO, NEAREST joins, `top_level_statement` | `transform_{pivot,merge_into}.cpp`, `CreatePivotStatement` |
| [`statements/match_recognize.rs`](src/transformer/statements/match_recognize.rs) | MATCH_RECOGNIZE | `transform_match_recognize.cpp` |
| [`statements/utility.rs`](src/transformer/statements/utility.rs) | COPY, EXPORT/IMPORT, ATTACH, VACUUM/ANALYZE, LOAD/INSTALL, generic option lists | `transform_{generic_copy_option,copy,export,attach,analyze,vacuum,load}.cpp` |
| [`src/ast.rs`](src/ast.rs) | The AST types and their `Display` (DuckDB's `ToString`) | `src/parser/{expression,query_node,tableref,statement}/*.cpp` |
| [`grammar/inlined_grammar.gram`](grammar/inlined_grammar.gram) | DuckDB's combined grammar (2,517 lines) | `src/include/duckdb/parser/peg/inlined_grammar.gram` |
| [`grammar/keywords/*.list`](grammar/keywords/) | The six keyword lists (reserved, unreserved, column name, function name, type name, bare label) | `src/parser/peg/grammar/keywords/*.list` |
| [`grammar/NOTICE`](grammar/NOTICE) | MIT license notice for the copied grammar | |
| [`sync_grammar.sh`](sync_grammar.sh) | Copies the grammar from a DuckDB checkout | |

## Key types and APIs

Functions in `lib.rs`:

| Function | Returns |
|---|---|
| `parse_ast(sql)` | `Result<Vec<ast::Statement>>`, one per statement, with default options |
| `parse_ast_with(sql, &ParserOptions)` | the same, with explicit options |
| `parse_ast_with_text(sql, &ParserOptions)` | each statement with its own text (`SQLStatement::query`) |
| `parse(sql)` | `Result<Vec<ParsedStatement>>`: parse trees and tokens, no AST |
| `parse_on_this_thread(sql)` / `_with` | tokens and parse trees, on the calling thread (which needs a large stack) |

All but the `_on_this_thread` functions run on a pooled parser thread (see below).

**`ParserOptions`** carries the settings that change parsing:

| Field | Setting | Default |
|---|---|---|
| `max_expression_depth` | `max_expression_depth` | 1000 (`transformer::DEFAULT_MAX_EXPRESSION_DEPTH`) |
| `integer_division` | `integer_division`: `/` on integers acts as `//` | `false` |
| `regex_full_match` | `regex_match_operator_semantics = full`: `~` and friends match the whole string | `false` |
| `identifier_case` | `preserve_identifier_case`: `IdentifierCase::{Preserve, Lower, Upper}` for unquoted names | `Preserve` |

Other public types:

- **`Grammar`** (`grammar.rs`): the compiled grammar. `Grammar::duckdb()` compiles the vendored
  grammar once and returns a `&'static Grammar`. `Grammar::compile(text)` compiles any grammar text.
- **`Token` / `TokenType`** (`tokenizer.rs`), from `tokenizer::tokenize(sql)`. Each token has its
  byte `offset` in the SQL.
- **`Matcher`** (`matcher.rs`): `Matcher::new(grammar, &tokens)` and `match_rule(rule, pos)`.
  After a failed match, `max_position` is the furthest token reached and `depth_exceeded` says
  whether the input nested too deep.
- **`ParseNode`** and **`TerminalValue`**: the parse tree. Nodes are `Terminal`, `List` (a
  sequence), `Choice` (which alternative matched), `Optional`, `Repeat` and `Rule` (a named rule).
- **`Transformer`** (`transformer.rs`): `Transformer::new(grammar)`, then `top_level_statement(tree)`
  for each parse tree. Its public fields `max_expression_depth`, `integer_division` and
  `regex_full_match` take the options.
- **`keywords`**: `categories(word)`, `is_keyword`, `is_reserved`, and `keyword_list()` (what
  `duckdb_keywords()` lists).

The main AST types in `ast.rs`:

- **`Statement`**: one enum variant per statement kind: `Select`, `Insert`, `Update`, `Delete`,
  `CreateTable`, `Create` (views, schemas, sequences, indexes, macros, types), `Drop`, `Alter`,
  `Transaction`, `Call`, `SetVariable`, `ResetVariable`, `Pragma`, `Explain`, `Prepare`, `Execute`,
  `Deallocate`, `Attach`, `Detach`, `Copy`, `CopyDatabase`, `Export`, `Vacuum`, `Load`, `Merge`, and
  more. `Multi(Vec<Statement>)` holds the ENUM-creating statements a PIVOT needs, followed by
  the PIVOT itself.
- **`SelectStatement`** wraps a **`QueryNode`**: a `QueryNodeKind` (`Select(SelectNode)`,
  `SetOperation`, `RecursiveCte`, and INSERT/UPDATE/DELETE/MERGE/COPY as CTE bodies), its
  `ResultModifier`s (ORDER BY, LIMIT, DISTINCT) and its `CteMap`.
- **`SelectNode`**: select list, FROM, WHERE, GROUP BY (`GroupByNode`, and `force_aggregates` for
  `GROUP BY ALL`), HAVING, QUALIFY and SAMPLE.
- **`TableRef`**: a `TableRefKind` (`BaseTable`, `Join`, `Subquery`, `TableFunction`,
  `ExpressionList`, `Show`, `Pivot`, `MatchRecognize`, `Empty`) plus alias, column aliases and sample.
- **`Expr`**: an `ExprKind` and an `alias`. `ExprKind` covers constants (`Literal`), column refs,
  `*`, functions (`FunctionExpr`), windows (`WindowExpr`), comparisons, AND/OR, operators, BETWEEN,
  CASE, casts, COLLATE, subqueries, lambdas, parameters, `DEFAULT`, types, and the MATCH_RECOGNIZE
  pattern parts.
- `transformer::visit_expr_mut` and `visit_query_node_exprs_mut` walk the expressions of a tree.

Every AST type implements `Display` with DuckDB's `ToString` output, so rendering a statement and
parsing it again gives the same tree.

Example (checked against the current API):

```rust
use pintail_parser::{ParserOptions, ast::Statement, parse_ast, parse_ast_with};

let statements = parse_ast("SELECT a + 1 AS b FROM t WHERE c > 2; DROP TABLE t")?;
assert_eq!(statements.len(), 2);
assert!(matches!(statements[0], Statement::Select(_)));
assert_eq!(statements[0].to_string(), "SELECT (a + 1) AS b FROM t WHERE (c > 2)");

let options = ParserOptions { integer_division: true, ..ParserOptions::default() };
assert_eq!(parse_ast_with("SELECT 7 / 2", &options)?[0].to_string(), "SELECT (7 // 2)");

let error = parse_ast("SELEC 1").unwrap_err();
assert_eq!(error.to_string(), "Parser Error: syntax error at or near \"SELEC\"");
```

## How it works

### Where the grammar comes from

In DuckDB the grammar is split into `.gram` files under `src/parser/peg/grammar/statements/`.
DuckDB's `scripts/parser/inline_grammar.py` combines them, with rules made from the keyword lists,
into one file, `src/include/duckdb/parser/peg/inlined_grammar.gram`. This crate vendors that
combined file and the keyword lists:

```sh
crates/pintail-parser/sync_grammar.sh [path/to/duckdb]   # default: ../../../duckdb from this crate
```

The script copies `inlined_grammar.gram` and `src/parser/peg/grammar/keywords/*.list` into
`grammar/` and prints the DuckDB commit it copied from. Run it after moving the
`rust-port-baseline` marker, with the DuckDB checkout at that commit (CI pins the baseline in
`DUCKDB_BASELINE` in `.github/workflows/ci.yml`). Both files are built into the crate with
`include_str!`, so a sync needs a rebuild and nothing else. A grammar change may need transformer
changes too: a new rule that reaches the transformer without a handler gives "the transformer for
... is not yet ported".

### Parsing, step by step

1. **Thread.** `parse_ast` and friends hand the work to a parser thread from `pool.rs`.
   Matching and transforming recurse deeply, so the threads have a 512 MiB stack. Starting a
   thread per call cost about as much as parsing a statement, so up to 8 idle threads are kept for
   reuse; each concurrent caller takes its own. A panic on a parser thread becomes an `Internal`
   error.
2. **Unicode spaces.** `unicode::strip_unicode_spaces` replaces spaces such as U+00A0 or U+3000
   outside quotes, dollar-quoted strings and comments with the same number of ASCII spaces, so byte
   offsets stay correct.
3. **Tokenize.** `tokenizer::tokenize` produces `Token`s with their byte offsets. It also records
   whether a newline or block comment came before each token (adjacent string literals are joined
   only across a newline).
4. **Compile the grammar (once).** `Grammar::duckdb()` parses the grammar text into rules, expands
   macros such as `List(D)` and `Parens(D)` inline, and turns choices made only of literals (the
   keyword lists) into one set lookup (`Node::LiteralSet`). Rules such as `Identifier`,
   `TableName`, `NumberLiteral` and `StringLiteral` are replaced by token-level terminals, as C++'s
   "rule overrides" do. Each identifier terminal accepts non-keywords plus the keyword categories
   allowed there (`IdentifierKind::mask`). The grammar's regex rules must all be overridden, and
   every rule reachable from `Program`/`Statement` must compile.
5. **Match.** `parse_statements` follows `Program <- TopLevelStatement*`: it matches the
   `Statement` rule at the current token, skips `;`s, and repeats until the end of input. The
   matcher is a recursive interpreter with PEG semantics: ordered choice, greedy repetition and
   case-insensitive literals. It is a packrat parser for the rules DuckDB memoizes (`Expression`,
   `TableRef`, `Identifier`, `ColId` and the qualification rules): the result at each
   (rule, position) is cached and shared through an `Arc`. Memoizing every rule cost more than the
   backtracking it saved. The C++ matcher is an explicit-stack machine with autocomplete hooks; this
   one has the same matching semantics.
6. **Errors.** If a statement stops before `;` or the end, the error points at the furthest
   token any terminal reached: `syntax error at or near "..."` or `syntax error at end of input`,
   with the byte offset as the error position. As in C++, the parser first tries inserting
   `,`, `)`, `]` or `}` to find where a missing token goes. If the matcher used up its stack budget
   (448 MiB), the error is `Max expression depth limit of N exceeded`.
7. **Transform.** `Transformer::top_level_statement` turns each parse tree into a `Statement`.
   The C++ transformer has one function per rule, with generated trampolines that unpack each
   rule's sequence by position. Here each rule is handled by name, and its parts are found by the
   names of the sub-rules it matched. The transformer also numbers parameters, keeps the WITH
   clauses in scope and the named WINDOW clauses, counts expression depth against
   `max_expression_depth`, and adds the ENUM statements a data-driven PIVOT needs.

Parse trees can be very deep, so `ParseNode` drops recursively only to depth 256 and iteratively
below that, so dropping a deep tree on a thread with a small stack cannot overflow it.

## Status and gaps

- The grammar is DuckDB's own, so the parser accepts what DuckDB accepts. The CI `corpus` step
  requires at least 99.9% of the statements in DuckDB's sqllogictest corpus to parse, 99.99% of
  those to transform and round trip, and 88% of statements that expect a parser error to be
  rejected.
- The transformer covers SELECT and most other statements (the list in Layout). Rules with no
  handler return `NotImplemented: the transformer for <rule> is not yet ported` (in
  `transformer.rs` and `transformer/statements.rs`). The `corpus` example counts these by kind.
  (The `lib.rs` doc comment still says "The transformer covers SELECT statements so far", which is
  out of date.)
- Many `NotImplemented` errors are DuckDB's own messages, for things DuckDB does not support
  either (for example "SET LOCAL is not implemented.", "Creating partial indexes is not supported
  currently").
- The C++ matcher's autocomplete hooks are not ported.
- `docs/REMAINING_WORK.md` item 29a (parser and errors, `sql/parser`: 23 failing files) lists what
  is open here: error locations (carets), `errors_as_json`, AT clauses and
  `preserve_identifier_case`.

## Testing

```sh
cargo test -p pintail-parser
cargo run -p pintail-parser --example render < queries.sql
cargo run --release -p pintail-parser --example corpus -- ../duckdb --min-parse 99.9
cargo run --release -p pintail-parser --example corpus -- ../duckdb --dump /tmp/stmts
cargo run --release -p pintail-parser --example bench -- /tmp/stmts
```

- Unit tests in `lib.rs` (statements parse, syntax errors match DuckDB, 2000-deep nesting does not
  crash), `grammar.rs`, `matcher.rs`, `tokenizer.rs`, `keywords.rs`, `unicode.rs`, `pool.rs` (thread
  reuse and surviving a panic) and `match_recognize.rs`.
- [`tests/transform.rs`](tests/transform.rs): 12 tests that the transformer and the `Display`
  rendering give DuckDB's `ToString` output and error messages.
- [`examples/render.rs`](examples/render.rs) prints the rendering of each SQL line from stdin.
- [`examples/corpus.rs`](examples/corpus.rs) reads every `.test` file of a DuckDB checkout and
  measures how many statements parse, transform and round trip (render, then parse and transform
  again), and how many statements that expect a parser error are rejected. `--min-parse`,
  `--min-transform` and `--min-reject` turn the rates into a pass/fail check; CI runs it with
  `--min-parse 99.9 --min-transform 99.99 --min-reject 88`. `--dump FILE` writes the statements
  that should parse, NUL-separated.
- [`examples/bench.rs`](examples/bench.rs) is the parse benchmark. It reads a `--dump` file and
  reports the time per statement for four paths: `parse_on_this_thread` and parse plus transform on
  one 512 MiB-stack thread (what C++'s `Parser::ParseQuery` measures), and `parse` and `parse_ast`
  through the public API, which uses the thread pool. Run it with `--release`.
- The full engine's sqllogictest run (`tools/sqllogictest`) is the end-to-end check.

## Notes for contributors

- **Do not edit the grammar by hand.** `grammar/` is a copy of DuckDB's files; re-sync with
  `sync_grammar.sh`. Grammar fixes belong upstream.
- **Layering.** This crate depends only on `pintail-common`. It produces unbound ASTs; name
  resolution and types belong to the binder in `pintail-core`.
- **Error messages verbatim.** Syntax errors and transformer errors use DuckDB's exact text,
  because sqllogictest `statement error` blocks match on it.
- **`Display` is `ToString`.** When adding an AST field, update its `Display` so the rendering
  stays what DuckDB prints and round trips; the `corpus` example checks this.
- **Stack use.** Anything that recurses over parse trees or ASTs runs on a parser thread. Use the
  public functions, or a thread with a large stack if you call `parse_on_this_thread`.
- **No `unsafe`.** By `../CLAUDE.md`, new `unsafe` is only allowed in `pintail-common` and storage
  code. This crate has none.
- **Port headers.** Each file starts with `// Ported from duckdb/<path> @ rust-port-baseline`, and
  doc comments name the C++ function or class in backticks.
