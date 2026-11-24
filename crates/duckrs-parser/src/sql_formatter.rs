// Ported from duckdb/src/parser/peg/sql_formatter.cpp, the HighlightTokenizerBehavior of
// duckdb/src/parser/peg/tokenizer/{base_tokenizer,highlight_tokenizer}.cpp @ rust-port-baseline

//! The SQL pretty-printer behind `duckdb_format_sql` and `EXPLAIN (SQL)` output (`FormatSQL`).
//!
//! Phase 1 lays the token stream out one clause per line; the later phases work on the lines:
//! they inline short struct literals and clauses, lift the first WHERE/HAVING predicate, expand
//! CREATE TABLE column lists, long CASE expressions and long parenthesized lists.

use crate::keywords;
use duckrs_common::render_width::render_width;

/// How SQL keywords are cased in the formatted output (`KeywordCase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeywordCase {
    /// Uppercase structural keywords; keep the case of other keywords used as names.
    #[default]
    Upper,
    Lower,
    Preserve,
}

/// Settings of the pretty-printer (`FormatterConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatterConfig {
    /// Spaces per indentation level.
    pub indent_size: usize,
    /// Widest line a clause is inlined into, in render columns; 0 always emits multiline output.
    pub inline_threshold: usize,
    pub keyword_case: KeywordCase,
}

impl Default for FormatterConfig {
    fn default() -> Self {
        Self {
            indent_size: 4,
            inline_threshold: 100,
            keyword_case: KeywordCase::Upper,
        }
    }
}

/// `FormatSQL`: pretty-prints `sql`.
pub fn format_sql(sql: &str, config: &FormatterConfig) -> String {
    SqlFormatter { config }.format(sql)
}

// ─── Tokenizer (Tokenizer + HighlightTokenizerBehavior) ──────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenType {
    Keyword,
    Identifier,
    StringLiteral,
    NumberLiteral,
    Operator,
    Comment,
    Terminator,
}

#[derive(Debug, Clone)]
struct Token {
    ty: TokenType,
    text: String,
    offset: usize,
}

impl Token {
    fn len(&self) -> usize {
        self.text.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Standard,
    SingleLineComment,
    MultiLineComment,
    QuotedIdentifier,
    StringLiteral,
    Keyword,
    Numeric,
    Operator,
    DollarQuotedString,
}

fn is_single_byte_operator(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' | b':' | b'?' | b'$' | b'#'
    )
}

fn is_initial_number(c: u8) -> bool {
    c.is_ascii_digit() || c == b'.'
}

fn is_special_string_char(c: u8) -> bool {
    matches!(c, b'N' | b'n' | b'X' | b'x' | b'E' | b'e' | b'B' | b'b')
}

fn is_scientific(c: u8) -> bool {
    c == b'e' || c == b'E'
}

fn is_control_flow(c: u8) -> bool {
    matches!(c, b'\'' | b';' | b'"' | b'.')
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `StringUtil::CharacterIsOperator`.
fn is_punctuation(c: u8) -> bool {
    c != b'_' && matches!(c, b'!'..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~')
}

fn is_keyword_char(c: u8) -> bool {
    !is_single_byte_operator(c) && !is_punctuation(c) && !is_space(c) && !is_control_flow(c)
}

/// `Tokenizer::CharacterIsOperator`.
fn is_operator_char(c: u8) -> bool {
    matches!(
        c,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>' | b'=' | b'~' | b'!' | b'@' | b'&' | b'|'
    )
}

fn is_dollar_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// `SkipTokenizerBytes`: the next `first`/`second` byte from `pos`, or the last byte.
fn skip_bytes(b: &[u8], mut pos: usize, first: u8, second: u8) -> usize {
    let end = b.len() - 1;
    while pos < end && b[pos] != first && b[pos] != second {
        pos += 1;
    }
    pos
}

struct Tokens<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
}

impl Tokens<'_> {
    fn emplace(&mut self, text: &str, offset: usize, ty: TokenType) {
        self.tokens.push(Token {
            ty,
            text: text.to_string(),
            offset,
        });
    }

    /// `HighlightTokenizerBehavior::PushToken`.
    fn push(&mut self, start: usize, end: usize, ty: TokenType) {
        if start < end {
            let text = &self.sql[start..end];
            self.emplace(text, start, ty);
        }
    }

    /// `Tokenizer::PushOperatorToken`: an operator cannot end in `+`/`-` unless it has a special
    /// character; trimmed signs become tokens of their own.
    fn push_operator(&mut self, start: usize, end: usize) {
        let b = self.sql.as_bytes();
        let has_special = b[start..end]
            .iter()
            .any(|c| matches!(c, b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?'));
        let mut end_pos = end;
        if !has_special {
            while end_pos > start + 1 && (b[end_pos - 1] == b'+' || b[end_pos - 1] == b'-') {
                end_pos -= 1;
            }
        }
        self.push(start, end_pos, TokenType::Operator);
        for pos in end_pos..end {
            let sign = &self.sql[pos..pos + 1];
            self.emplace(sign, pos, TokenType::Operator);
        }
    }
}

fn word_type(word: &str) -> TokenType {
    if keywords::is_keyword(word) {
        TokenType::Keyword
    } else {
        TokenType::Identifier
    }
}

/// `SQLFormatter::Tokenize`: the highlight tokens of `sql` (comments kept, no end token).
fn tokenize(sql: &str) -> Vec<Token> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut t = Tokens {
        sql,
        tokens: Vec::new(),
    };
    let mut state = State::Standard;
    let mut last_pos = 0;
    let mut escape_string = false;
    let mut quote = b'"';
    let mut dollar_marker: &str = "";
    let mut comment_depth = 0usize;
    let mut i = 0;
    while i < n {
        let mut c = b[i];
        match state {
            State::Standard => 'standard: {
                if c == b'\'' {
                    state = State::StringLiteral;
                    last_pos = i;
                    escape_string = false;
                    break 'standard;
                }
                if c == b'"' || c == b'`' {
                    state = State::QuotedIdentifier;
                    quote = c;
                    last_pos = i;
                    break 'standard;
                }
                if c == b';' {
                    t.emplace(";", i, TokenType::Terminator);
                    last_pos = i + 1;
                    break 'standard;
                }
                if c == b'$' {
                    if i + 1 >= n {
                        break 'standard;
                    }
                    if b[i + 1].is_ascii_digit() {
                        // `$1` is a parameter.
                        t.emplace("$", i, TokenType::Operator);
                        break 'standard;
                    }
                    last_pos = i;
                    let mut next_dollar = None;
                    for (idx, &ch) in b.iter().enumerate().skip(i + 1) {
                        if ch == b'$' {
                            next_dollar = Some(idx);
                            break;
                        }
                        if !is_dollar_tag_char(ch) {
                            break;
                        }
                    }
                    let Some(next_dollar) = next_dollar else {
                        // `$name` parameter.
                        t.emplace("$", i, TokenType::Operator);
                        break 'standard;
                    };
                    state = State::DollarQuotedString;
                    dollar_marker = &sql[last_pos + 1..next_dollar];
                    i = next_dollar;
                    break 'standard;
                }
                if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
                    i += 1;
                    state = State::SingleLineComment;
                    break 'standard;
                }
                if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
                    i += 1;
                    state = State::MultiLineComment;
                    comment_depth = 1;
                    break 'standard;
                }
                if is_space(c) {
                    last_pos = i + 1;
                    break 'standard;
                }
                if c == b':' && i + 1 < n && (b[i + 1] == b':' || b[i + 1] == b'=') {
                    if i + 2 < n && is_operator_char(b[i + 2]) {
                        state = State::Operator;
                        last_pos = i;
                        break 'standard;
                    }
                    let text = &sql[i..i + 2];
                    t.emplace(text, last_pos, TokenType::Operator);
                    i += 1;
                    last_pos = i + 1;
                    break 'standard;
                }
                if is_single_byte_operator(c) {
                    let text = &sql[i..i + 1];
                    t.emplace(text, last_pos, TokenType::Operator);
                    last_pos = i + 1;
                    break 'standard;
                }
                if is_initial_number(c) {
                    state = State::Numeric;
                    last_pos = i;
                    break 'standard;
                }
                if is_special_string_char(c) && i + 1 < n && b[i + 1] == b'\'' {
                    state = State::StringLiteral;
                    last_pos = i;
                    if c == b'E' || c == b'e' {
                        escape_string = true;
                    }
                    i += 1;
                    break 'standard;
                }
                state = if is_punctuation(c) {
                    State::Operator
                } else {
                    State::Keyword
                };
                last_pos = i;
            }
            State::Numeric => 'numeric: {
                if is_initial_number(c) {
                    break 'numeric;
                }
                if c == b'_' && b[i - 1].is_ascii_digit() && i + 1 < n && b[i + 1].is_ascii_digit() {
                    break 'numeric;
                }
                if is_scientific(c)
                    && !is_scientific(b[i - 1])
                    && (b[last_pos].is_ascii_digit() || b[i - 1].is_ascii_digit())
                {
                    break 'numeric;
                }
                if (c == b'+' || c == b'-') && is_scientific(b[i - 1]) {
                    break 'numeric;
                }
                while !is_initial_number(b[i - 1]) {
                    i -= 1;
                }
                t.push(last_pos, i, TokenType::NumberLiteral);
                state = State::Standard;
                last_pos = i;
                i -= 1;
            }
            State::Operator => {
                let comment_start =
                    (c == b'-' && i + 1 < n && b[i + 1] == b'-') || (c == b'/' && i + 1 < n && b[i + 1] == b'*');
                if comment_start || !is_operator_char(c) {
                    t.push_operator(last_pos, i);
                    state = State::Standard;
                    last_pos = i;
                    i -= 1;
                }
            }
            State::Keyword => {
                if c != b'$' && !is_keyword_char(c) {
                    t.push(last_pos, i, word_type(&sql[last_pos..i]));
                    state = State::Standard;
                    last_pos = i;
                    i -= 1;
                }
            }
            State::StringLiteral => {
                i = skip_bytes(b, i, b'\'', b'\\');
                c = b[i];
                if c == b'\\' && i + 1 < n && escape_string {
                    i += 1;
                } else if c == b'\'' {
                    if i + 1 < n && b[i + 1] == b'\'' {
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, TokenType::StringLiteral);
                        last_pos = i + 1;
                        escape_string = false;
                        state = State::Standard;
                    }
                }
            }
            State::QuotedIdentifier => {
                i = skip_bytes(b, i, quote, quote);
                if b[i] == quote {
                    if i + 1 < n && b[i + 1] == quote {
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, TokenType::Identifier);
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::SingleLineComment => {
                i = skip_bytes(b, i, b'\n', b'\r');
                if b[i] == b'\n' || b[i] == b'\r' {
                    t.push(last_pos, i + 1, TokenType::Comment);
                    last_pos = i + 1;
                    state = State::Standard;
                }
            }
            State::MultiLineComment => {
                i = skip_bytes(b, i, b'/', b'*');
                c = b[i];
                if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
                    i += 1;
                    comment_depth += 1;
                } else if c == b'*' && i + 1 < n && b[i + 1] == b'/' {
                    i += 1;
                    comment_depth -= 1;
                    if comment_depth == 0 {
                        t.push(last_pos, i + 1, TokenType::Comment);
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::DollarQuotedString => 'dollar: {
                i = skip_bytes(b, i, b'$', b'$');
                if b[i] != b'$' || i + 1 >= n {
                    break 'dollar;
                }
                let start = i + 1;
                let Some(len) = b[start..].iter().position(|&ch| ch == b'$') else {
                    break 'dollar;
                };
                let end = start + len;
                if &sql[start..end] != dollar_marker {
                    break 'dollar;
                }
                t.push(last_pos, end + 1, TokenType::StringLiteral);
                dollar_marker = "";
                state = State::Standard;
                i = end;
                last_pos = i + 1;
            }
        }
        i += 1;
    }
    match state {
        State::SingleLineComment | State::MultiLineComment => t.push(last_pos, n, TokenType::Comment),
        State::Operator => t.push_operator(last_pos, n),
        State::DollarQuotedString => t.push(last_pos, n, TokenType::StringLiteral),
        _ if last_pos < n => {
            // `TokenizerBehavior::OnLastToken`
            let word = &sql[last_pos..];
            let ty = match state {
                State::Keyword => word_type(word),
                State::StringLiteral => TokenType::StringLiteral,
                State::Numeric => TokenType::NumberLiteral,
                _ => TokenType::Identifier,
            };
            t.emplace(word, last_pos, ty);
        }
        _ => {}
    }
    t.tokens
}

fn is_parameter_start(tokens: &[Token], i: usize) -> bool {
    tokens[i].ty == TokenType::Operator
        && tokens[i].text == "$"
        && i + 1 < tokens.len()
        && matches!(
            tokens[i + 1].ty,
            TokenType::Identifier | TokenType::Keyword | TokenType::NumberLiteral
        )
}

/// `SkipNonCodeToken`: when `pos` is in a string, comment, quoted identifier or parameter, moves it
/// to that token's last byte and returns true, so text passes leave its contents alone.
fn skip_non_code(tokens: &[Token], token_idx: &mut usize, pos: &mut usize) -> bool {
    while *token_idx < tokens.len() && tokens[*token_idx].offset + tokens[*token_idx].len() <= *pos {
        *token_idx += 1;
    }
    if *token_idx == tokens.len() {
        return false;
    }
    let token = &tokens[*token_idx];
    if token.offset > *pos {
        return false;
    }
    if is_parameter_start(tokens, *token_idx) {
        *token_idx += 1;
        let name = &tokens[*token_idx];
        *pos = name.offset + name.len() - 1;
        return true;
    }
    let quoted_identifier =
        token.ty == TokenType::Identifier && (token.text.starts_with('"') || token.text.starts_with('`'));
    if token.ty != TokenType::StringLiteral && token.ty != TokenType::Comment && !quoted_identifier {
        return false;
    }
    let mut length = token.len();
    // The newline ending a line comment stays a line boundary.
    if token.ty == TokenType::Comment && token.text.starts_with("--") {
        let text = token.text.as_bytes();
        while length > 0 && (text[length - 1] == b'\n' || text[length - 1] == b'\r') {
            length -= 1;
        }
    }
    if token.offset + length <= *pos {
        return false;
    }
    *pos = token.offset + length - 1;
    true
}

// ─── Keyword classification ──────────────────────────────────────────────────

const CLAUSE_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "JOIN",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "WITH",
    "INSERT",
    "UPDATE",
    "DELETE",
    "SET",
    "RETURNING",
    "VALUES",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "QUALIFY",
    "PIVOT",
    "UNPIVOT",
    "REFRESH",
    "INSTALL",
    "LOAD",
    "ATTACH",
    "DETACH",
    "CHECKPOINT",
    "FORCE",
    "COPY",
];

const COMPOUND_CLAUSE_LINES: &[&str] = &[
    "GROUP BY",
    "ORDER BY",
    "PARTITION BY",
    "UNION ALL",
    "UNION DISTINCT",
    "INTERSECT ALL",
    "INTERSECT DISTINCT",
    "EXCEPT ALL",
    "EXCEPT DISTINCT",
    "INNER JOIN",
    "CROSS JOIN",
    "NATURAL JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
    "INSERT INTO",
    "DELETE FROM",
    "ON CONFLICT",
    "WITH RECURSIVE",
    "FORCE CHECKPOINT",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "CREATE UNIQUE INDEX",
    "CREATE SCHEMA",
    "CREATE SEQUENCE",
    "CREATE MACRO",
    "CREATE FUNCTION",
    "CREATE TYPE",
    "CREATE TEMP TABLE",
    "CREATE TEMP VIEW",
    "CREATE TEMPORARY TABLE",
    "CREATE TEMPORARY VIEW",
    "CREATE OR REPLACE TABLE",
    "CREATE OR REPLACE VIEW",
    "CREATE OR REPLACE INDEX",
    "CREATE OR REPLACE MACRO",
    "CREATE OR REPLACE FUNCTION",
    "CREATE OR REPLACE TYPE",
    "CREATE OR REPLACE TEMP TABLE",
    "CREATE OR REPLACE TEMP VIEW",
    "CREATE OR REPLACE TEMPORARY TABLE",
    "CREATE OR REPLACE TEMPORARY VIEW",
    "ALTER TABLE",
];

/// Structural keywords uppercased in `KeywordCase::Upper` (`ShouldUppercase`).
const UPPERCASE_KEYWORDS: &[&str] = &[
    "SELECT",
    "DISTINCT",
    "FROM",
    "WHERE",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "GROUP",
    "BY",
    "ORDER",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "ALL",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "NATURAL",
    "ON",
    "USING",
    "WITH",
    "RECURSIVE",
    "INSERT",
    "INTO",
    "UPDATE",
    "DELETE",
    "MERGE",
    "MATCHED",
    "SET",
    "VALUES",
    "RETURNING",
    "OVERRIDING",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "MATERIALIZED",
    "TEMP",
    "TEMPORARY",
    "IF",
    "NOT",
    "EXISTS",
    "OR",
    "REPLACE",
    "UNIQUE",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "CONSTRAINT",
    "CHECK",
    "GENERATED",
    "ALWAYS",
    "IDENTITY",
    "STORED",
    "VIRTUAL",
    "COLUMN",
    "ADD",
    "RENAME",
    "COPY",
    "AND",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "IN",
    "LIKE",
    "ILIKE",
    "GLOB",
    "BETWEEN",
    "SIMILAR",
    "SOME",
    "ANY",
    "OVERLAPS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "AS",
    "ASC",
    "DESC",
    "NULLS",
    "FIRST",
    "LAST",
    "OVER",
    "PARTITION",
    "ROWS",
    "RANGE",
    "GROUPS",
    "UNBOUNDED",
    "PRECEDING",
    "FOLLOWING",
    "CURRENT",
    "ROW",
    "EXCLUDE",
    "TIES",
    "OTHERS",
    "CAST",
    "TRY_CAST",
    "QUALIFY",
    "PIVOT",
    "UNPIVOT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "TRANSACTION",
    "SAVEPOINT",
    "RELEASE",
    "EXPLAIN",
    "ANALYZE",
    "DESCRIBE",
    "SHOW",
    "PRAGMA",
    "ATTACH",
    "DETACH",
    "CHECKPOINT",
    "VACUUM",
    "LOAD",
    "INSTALL",
    "FORCE",
    "POSITIONAL",
    "ASOF",
    "LATERAL",
    "TABLESAMPLE",
    "REPEATABLE",
    "SYSTEM",
    "BERNOULLI",
    "RESERVOIR",
    "NEAREST",
    "APPROX",
    "EXACT",
    "DISTANCE",
    "SIMILARITY",
];

/// Keywords written without a space before `(` (functions and type names).
const NO_SPACE_BEFORE_PAREN: &[&str] = &[
    "COALESCE",
    "NULLIF",
    "CAST",
    "TRY_CAST",
    "EXTRACT",
    "OVERLAY",
    "POSITION",
    "SUBSTRING",
    "TRIM",
    "GROUPING",
    "GROUPING_ID",
    "TREAT",
    "XMLATTRIBUTES",
    "XMLCONCAT",
    "XMLELEMENT",
    "XMLFOREST",
    "XMLNAMESPACES",
    "XMLPARSE",
    "XMLPI",
    "XMLROOT",
    "XMLSERIALIZE",
    "XMLTABLE",
    "XMLEXISTS",
    "BIGINT",
    "BIT",
    "BOOLEAN",
    "CHAR",
    "CHARACTER",
    "DEC",
    "DECIMAL",
    "FLOAT",
    "INT",
    "INTEGER",
    "INTERVAL",
    "MAP",
    "NATIONAL",
    "NCHAR",
    "NUMERIC",
    "PRECISION",
    "REAL",
    "SMALLINT",
    "STRUCT",
    "TIME",
    "TIMESTAMP",
    "VARCHAR",
];

/// Fixed multi-word clause keywords (`kFixedCompounds`).
const FIXED_COMPOUNDS: &[&[&str]] = &[
    &["GROUP", "BY"],
    &["ORDER", "BY"],
    &["PARTITION", "BY"],
    &["ALTER", "TABLE"],
    &["INSERT", "INTO"],
    &["DELETE", "FROM"],
    &["ON", "CONFLICT"],
    &["FORCE", "CHECKPOINT"],
    &["UNION", "ALL"],
    &["UNION", "DISTINCT"],
    &["INTERSECT", "ALL"],
    &["INTERSECT", "DISTINCT"],
    &["EXCEPT", "ALL"],
    &["EXCEPT", "DISTINCT"],
    &["INNER", "JOIN"],
    &["CROSS", "JOIN"],
    &["NATURAL", "JOIN"],
    &["LEFT", "JOIN"],
    &["LEFT", "OUTER", "JOIN"],
    &["RIGHT", "JOIN"],
    &["RIGHT", "OUTER", "JOIN"],
    &["FULL", "JOIN"],
    &["FULL", "OUTER", "JOIN"],
    &["WITH", "RECURSIVE"],
    &["CREATE", "UNIQUE", "INDEX"],
];

/// `CREATE [OR REPLACE] [TEMP | TEMPORARY] <object>`.
const CREATE_PREFIXES: &[&[&str]] = &[
    &["CREATE"],
    &["CREATE", "TEMP"],
    &["CREATE", "TEMPORARY"],
    &["CREATE", "OR", "REPLACE"],
    &["CREATE", "OR", "REPLACE", "TEMP"],
    &["CREATE", "OR", "REPLACE", "TEMPORARY"],
];

const CREATE_OBJECTS: &[&str] = &[
    "TABLE", "VIEW", "INDEX", "SCHEMA", "SEQUENCE", "MACRO", "FUNCTION", "TYPE",
];

fn is_clause_keyword(kw: &str) -> bool {
    CLAUSE_KEYWORDS.contains(&kw)
}

fn is_join_modifier(kw: &str) -> bool {
    matches!(kw, "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "NATURAL")
}

/// `IsClauseKeywordLine`: `trimmed` is exactly a clause keyword, single or compound.
fn is_clause_keyword_line(trimmed: &str) -> bool {
    let upper = trimmed.to_ascii_uppercase();
    is_clause_keyword(&upper) || COMPOUND_CLAUSE_LINES.contains(&upper.as_str())
}

fn should_uppercase(kw: &str) -> bool {
    UPPERCASE_KEYWORDS.contains(&kw)
}

fn peek_keyword(tokens: &[Token], i: usize, offset: usize) -> String {
    match tokens.get(i + offset) {
        Some(t) if t.ty == TokenType::Keyword => t.text.to_ascii_uppercase(),
        _ => String::new(),
    }
}

/// A clause keyword at `tokens[i]`: the extra tokens it spans, its uppercased text (`GROUP BY`)
/// and its original text joined by spaces (`group by`). `DetectCompoundClause`.
fn detect_compound_clause(tokens: &[Token], i: usize) -> Option<(usize, String, String)> {
    let kw = tokens[i].text.to_ascii_uppercase();
    let creates = CREATE_PREFIXES.iter().flat_map(|prefix| {
        CREATE_OBJECTS.iter().map(move |object| {
            let mut entry = prefix.to_vec();
            entry.push(object);
            entry
        })
    });
    let mut best: Option<Vec<&str>> = None;
    for entry in FIXED_COMPOUNDS.iter().map(|e| e.to_vec()).chain(creates) {
        if kw != entry[0] {
            continue;
        }
        let matched = (1..entry.len()).all(|k| peek_keyword(tokens, i, k) == entry[k]);
        if matched && best.as_ref().is_none_or(|b| entry.len() > b.len()) {
            best = Some(entry);
        }
    }
    if let Some(entry) = best {
        let mut compound = kw;
        let mut original = tokens[i].text.clone();
        for (k, word) in entry.iter().enumerate().skip(1) {
            compound.push(' ');
            compound.push_str(word);
            original.push(' ');
            original.push_str(&tokens[i + k].text);
        }
        return Some((entry.len() - 1, compound, original));
    }
    if is_clause_keyword(&kw) || is_join_modifier(&kw) {
        let original = tokens[i].text.clone();
        return Some((0, kw, original));
    }
    None
}

// ─── Line helpers ────────────────────────────────────────────────────────────

fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|&c| c == b' ').count()
}

fn trim_left(line: &str) -> &str {
    &line[leading_spaces(line)..]
}

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

/// Splits at newlines that are not inside strings, comments or quoted identifiers.
fn split_lines(formatted: &str) -> Vec<String> {
    let tokens = tokenize(formatted);
    let b = formatted.as_bytes();
    let mut token_idx = 0;
    let mut lines = Vec::new();
    let mut start = 0;
    let mut k = 0;
    while k <= b.len() {
        if !skip_non_code(&tokens, &mut token_idx, &mut k) && (k == b.len() || b[k] == b'\n') {
            lines.push(formatted[start..k].to_string());
            start = k + 1;
        }
        k += 1;
    }
    lines
}

fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

fn trim_spaces(s: &str) -> &str {
    s.trim_matches(' ')
}

/// `SplitTopLevelCommas`: the trimmed non-empty parts of `s` between commas outside parentheses.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let tokens = tokenize(s);
    let mut token_idx = 0;
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut k = 0;
    while k < b.len() {
        let c = b[k];
        if !skip_non_code(&tokens, &mut token_idx, &mut k) {
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
            } else if c == b',' && depth == 0 {
                let part = trim_spaces(&s[start..k]);
                if !part.is_empty() {
                    parts.push(part.to_string());
                }
                start = k + 1;
            }
        }
        k += 1;
    }
    let part = trim_spaces(&s[start..]);
    if !part.is_empty() {
        parts.push(part.to_string());
    }
    parts
}

/// The `)` matching the `(` at `open_pos`.
fn find_matching_close(s: &str, open_pos: usize) -> Option<usize> {
    let b = s.as_bytes();
    let tokens = tokenize(s);
    let mut token_idx = 0;
    let mut depth = 0i32;
    let mut k = open_pos;
    while k < b.len() {
        let c = b[k];
        if !skip_non_code(&tokens, &mut token_idx, &mut k) {
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
        }
        k += 1;
    }
    None
}

/// The first `(` of `line` at or after `from` that is code.
fn find_open_paren(line: &str, from: usize) -> Option<usize> {
    let b = line.as_bytes();
    let tokens = tokenize(line);
    let mut token_idx = 0;
    let mut k = from;
    while k < b.len() {
        if !skip_non_code(&tokens, &mut token_idx, &mut k) && b[k] == b'(' {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// The length of a `CREATE [OR REPLACE] [TEMP|TEMPORARY] TABLE` prefix, or 0.
fn match_create_table_prefix(upper_trimmed: &str) -> usize {
    const PREFIXES: [&str; 6] = [
        "CREATE OR REPLACE TEMPORARY TABLE",
        "CREATE OR REPLACE TEMP TABLE",
        "CREATE OR REPLACE TABLE",
        "CREATE TEMPORARY TABLE",
        "CREATE TEMP TABLE",
        "CREATE TABLE",
    ];
    let b = upper_trimmed.as_bytes();
    for prefix in PREFIXES {
        let len = prefix.len();
        if b.len() >= len && b.starts_with(prefix.as_bytes()) && matches!(b.get(len), None | Some(b' ' | b'(')) {
            return len;
        }
    }
    0
}

/// `s[pos..]` starts with the uppercase `keyword` (any case), not inside a longer word.
fn match_keyword_at(s: &[u8], pos: usize, keyword: &str) -> bool {
    let kw = keyword.as_bytes();
    if pos + kw.len() > s.len() {
        return false;
    }
    if !s[pos..pos + kw.len()]
        .iter()
        .zip(kw)
        .all(|(a, b)| a.to_ascii_uppercase() == *b)
    {
        return false;
    }
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if s.get(pos + kw.len()).is_some_and(|&c| is_word(c)) {
        return false;
    }
    !(pos > 0 && is_word(s[pos - 1]))
}

/// The first `keyword` in code from `from`, inside parentheses too.
fn find_keyword_any(s: &str, from: usize, keyword: &str) -> Option<usize> {
    let tokens = tokenize(s);
    let b = s.as_bytes();
    let mut token_idx = 0;
    let mut k = from;
    while k < b.len() {
        if !skip_non_code(&tokens, &mut token_idx, &mut k) && match_keyword_at(b, k, keyword) {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// The END closing the CASE at `case_pos`, past nested CASE ... END pairs.
fn find_case_end(s: &str, case_pos: usize) -> Option<usize> {
    let tokens = tokenize(s);
    let b = s.as_bytes();
    let mut token_idx = 0;
    let mut depth = 0i32;
    let mut k = case_pos;
    while k < b.len() {
        if skip_non_code(&tokens, &mut token_idx, &mut k) {
            k += 1;
            continue;
        }
        if match_keyword_at(b, k, "CASE") {
            depth += 1;
            k += 4;
            continue;
        }
        if match_keyword_at(b, k, "END") {
            depth -= 1;
            if depth == 0 {
                return Some(k);
            }
            k += 3;
            continue;
        }
        k += 1;
    }
    None
}

/// Splits the text between CASE and END at its top-level WHEN/ELSE keywords.
fn split_case_branches(content: &str) -> Vec<String> {
    let b = content.as_bytes();
    let tokens = tokenize(content);
    let mut token_idx = 0;
    let mut branches = Vec::new();
    let mut paren_depth = 0i32;
    let mut case_depth = 0i32;
    let mut start = leading_spaces(content);
    let mut k = start;
    let push_branch = |branches: &mut Vec<String>, from: usize, to: usize| {
        let mut e = to;
        while e > from && b[e - 1] == b' ' {
            e -= 1;
        }
        if e > from {
            branches.push(content[from..e].to_string());
        }
    };
    while k < b.len() {
        let c = b[k];
        if skip_non_code(&tokens, &mut token_idx, &mut k) {
            k += 1;
            continue;
        }
        if c == b'(' {
            paren_depth += 1;
            k += 1;
            continue;
        }
        if c == b')' {
            paren_depth -= 1;
            k += 1;
            continue;
        }
        if paren_depth == 0 {
            if match_keyword_at(b, k, "CASE") {
                case_depth += 1;
                k += 4;
                continue;
            }
            if match_keyword_at(b, k, "END") {
                case_depth -= 1;
                k += 3;
                continue;
            }
            if case_depth == 0 && (match_keyword_at(b, k, "WHEN") || match_keyword_at(b, k, "ELSE")) {
                if k > start {
                    push_branch(&mut branches, start, k);
                }
                start = k;
            }
        }
        k += 1;
    }
    push_branch(&mut branches, start, b.len());
    branches
}

/// The top-level `(` whose comma list spans the most bytes of `line`.
fn find_longest_paren_list(line: &str, from: usize) -> Option<usize> {
    let b = line.as_bytes();
    let tokens = tokenize(line);
    let mut token_idx = 0;
    let mut best = None;
    let mut best_len = 0;
    let mut depth = 0i32;
    let mut k = from;
    while k < b.len() {
        let c = b[k];
        if !skip_non_code(&tokens, &mut token_idx, &mut k) {
            if c == b'(' && depth == 0 {
                if let Some(close) = find_matching_close(line, k) {
                    let span = close - k;
                    if span > best_len && split_top_level_commas(&line[k + 1..close]).len() > 1 {
                        best_len = span;
                        best = Some(k);
                    }
                    k = close;
                }
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
            }
        }
        k += 1;
    }
    best
}

// ─── The formatter ───────────────────────────────────────────────────────────

struct SqlFormatter<'a> {
    config: &'a FormatterConfig,
}

/// Indentation state of one open `(`, `[` or `{`.
struct ParenInfo {
    has_clauses: bool,
    close_indent: usize,
    inner_clause_indent: usize,
}

/// Phase 1 output under construction.
struct Writer {
    result: String,
    at_line_start: bool,
}

impl Writer {
    fn last(&self) -> Option<u8> {
        self.result.as_bytes().last().copied()
    }

    fn indent(&mut self, n: usize) {
        self.result.push_str(&spaces(n));
        self.at_line_start = false;
    }

    fn newline(&mut self) {
        self.result.push('\n');
        self.at_line_start = true;
    }

    fn space(&mut self) {
        if !self.at_line_start && !matches!(self.last(), None | Some(b' ' | b'(' | b'.' | b'[' | b':')) {
            self.result.push(' ');
        }
    }

    fn pop_space(&mut self) {
        if self.last() == Some(b' ') {
            self.result.pop();
        }
    }

    fn write(&mut self, text: &str) {
        self.result.push_str(text);
        self.at_line_start = false;
    }
}

impl SqlFormatter<'_> {
    fn format(&self, sql: &str) -> String {
        let tokens = tokenize(sql);
        if tokens.is_empty() {
            return sql.to_string();
        }
        let multiline = self.format_multiline(sql, &tokens);
        let struct_inlined = self.inline_struct_literals(&multiline);
        let merged = self.merge_short_clauses(&struct_inlined);
        let collapsed = self.collapse_first_condition(&merged);
        let expanded = self.expand_table_definition(&collapsed);
        let case_expanded = self.expand_case_expressions(&expanded);
        let mut result = self.expand_long_paren_lists(&case_expanded);
        let trimmed = result.trim_end_matches(['\n', '\r', ' ']).len();
        result.truncate(trimmed);
        result
    }

    fn threshold(&self) -> usize {
        self.config.inline_threshold
    }

    /// `ApplyCase`: `structural` keywords are uppercased in `Upper` mode whatever their kind.
    fn apply_case(&self, upper_kw: &str, orig_kw: &str, structural: bool) -> String {
        match self.config.keyword_case {
            KeywordCase::Upper if structural || should_uppercase(upper_kw) => upper_kw.to_string(),
            KeywordCase::Upper | KeywordCase::Preserve => orig_kw.to_string(),
            KeywordCase::Lower => upper_kw.to_ascii_lowercase(),
        }
    }

    /// Phase 1: lays the tokens out with one clause keyword per line.
    fn format_multiline(&self, sql: &str, tokens: &[Token]) -> String {
        let mut w = Writer {
            result: String::with_capacity(tokens.len() * 8),
            at_line_start: true,
        };
        let mut after_clause = false;
        let mut prev_was_keyword = false;
        let mut in_between = false;
        let mut last_clause = String::new();
        // The last keyword, to decide on a space before `(`.
        let mut prev_keyword = String::new();
        let mut paren_stack: Vec<ParenInfo> = Vec::new();
        let indent_size = self.config.indent_size;
        let clause_indent = |stack: &[ParenInfo]| stack.last().map_or(0, |p| p.inner_clause_indent);
        let content_indent = |stack: &[ParenInfo]| clause_indent(stack) + indent_size;

        let mut i = 0;
        while i < tokens.len() {
            let tok = &tokens[i];
            let text = tok.text.as_str();
            let is_op = tok.ty == TokenType::Operator;

            if tok.ty == TokenType::Terminator {
                after_clause = false;
                prev_was_keyword = false;
                in_between = false;
                last_clause.clear();
                w.write(";");
                if i + 1 < tokens.len() {
                    w.newline();
                    let end_of_semi = tok.offset + tok.len();
                    let next_start = tokens[i + 1].offset.min(sql.len());
                    let gap = sql.as_bytes().get(end_of_semi..next_start).unwrap_or(&[]);
                    // A blank line between statements is kept.
                    if gap.iter().filter(|&&c| c == b'\n').count() >= 2 {
                        w.newline();
                    }
                }
                i += 1;
                continue;
            }

            if tok.ty == TokenType::Comment {
                let comment = text.trim_end_matches(['\n', '\r']);
                let is_block = comment.starts_with("/*");
                let is_line = comment.starts_with("--");
                if is_block && w.result.is_empty() {
                    // A block comment before the first statement starts at column 0.
                } else if is_block && w.at_line_start {
                    w.indent(clause_indent(&paren_stack));
                } else {
                    w.space();
                }
                w.result.push_str(comment);
                if is_line || is_block {
                    w.newline();
                }
                after_clause = false;
                prev_was_keyword = false;
                i += 1;
                continue;
            }

            if is_op && text == "(" {
                if prev_was_keyword && !NO_SPACE_BEFORE_PAREN.contains(&prev_keyword.as_str()) && !w.at_line_start {
                    w.space();
                }
                after_clause = false;
                prev_was_keyword = false;
                w.write("(");
                let ci = content_indent(&paren_stack);
                paren_stack.push(ParenInfo {
                    has_clauses: false,
                    close_indent: ci,
                    inner_clause_indent: ci + indent_size,
                });
                i += 1;
                continue;
            }

            if is_op && text == ")" {
                after_clause = false;
                prev_was_keyword = false;
                let had_clauses = paren_stack.last().is_some_and(|p| p.has_clauses);
                let close_indent = paren_stack.pop().map_or(0, |p| p.close_indent);
                if had_clauses && !w.at_line_start {
                    w.newline();
                    w.indent(close_indent);
                }
                w.write(")");
                i += 1;
                continue;
            }

            // Struct literals are always multiline here; phase 2 inlines short ones.
            if is_op && text == "{" {
                prev_was_keyword = false;
                if after_clause {
                    w.newline();
                    w.indent(content_indent(&paren_stack));
                } else {
                    w.space();
                }
                w.write("{");
                let ci = content_indent(&paren_stack);
                paren_stack.push(ParenInfo {
                    has_clauses: true,
                    close_indent: ci,
                    inner_clause_indent: ci + indent_size,
                });
                after_clause = true;
                i += 1;
                continue;
            }

            if is_op && text == "}" {
                after_clause = false;
                prev_was_keyword = false;
                let close_indent = paren_stack.pop().map_or(0, |p| p.close_indent);
                if !w.at_line_start {
                    w.newline();
                    w.indent(close_indent);
                }
                w.write("}");
                i += 1;
                continue;
            }

            if is_op && text == "[" {
                after_clause = false;
                prev_was_keyword = false;
                if !w.at_line_start && !matches!(w.last(), None | Some(b' ' | b'(' | b'[' | b'.')) {
                    w.result.push(' ');
                }
                w.write("[");
                // Array literals keep their commas inline.
                let ci = content_indent(&paren_stack);
                paren_stack.push(ParenInfo {
                    has_clauses: false,
                    close_indent: ci,
                    inner_clause_indent: ci + indent_size,
                });
                i += 1;
                continue;
            }

            if is_op && text == "]" {
                after_clause = false;
                prev_was_keyword = false;
                paren_stack.pop();
                w.pop_space();
                w.write("]");
                i += 1;
                continue;
            }

            if is_op && text == "," {
                after_clause = false;
                prev_was_keyword = false;
                w.write(",");
                if paren_stack.last().is_none_or(|p| p.has_clauses) {
                    w.newline();
                    w.indent(content_indent(&paren_stack));
                } else {
                    w.result.push(' ');
                }
                i += 1;
                continue;
            }

            if text == "." && (is_op || tok.ty == TokenType::NumberLiteral) {
                after_clause = false;
                prev_was_keyword = false;
                w.pop_space();
                w.write(".");
                i += 1;
                continue;
            }

            // `:` separates struct keys from values; `::` casts. Neither has a space before it.
            if is_op && (text == ":" || text == "::") {
                after_clause = false;
                prev_was_keyword = false;
                w.pop_space();
                w.write(if text == ":" { ": " } else { "::" });
                i += 1;
                continue;
            }

            // A parameter is one unit: a keyword name is not a clause and keeps its case.
            if is_parameter_start(tokens, i) {
                if after_clause {
                    w.newline();
                    w.indent(content_indent(&paren_stack));
                    after_clause = false;
                } else {
                    w.space();
                }
                w.write("$");
                w.write(&tokens[i + 1].text);
                prev_was_keyword = false;
                prev_keyword.clear();
                i += 2;
                continue;
            }

            if tok.ty == TokenType::Keyword {
                if let Some((extra, compound, original)) = detect_compound_clause(tokens, i) {
                    // In `COPY t FROM`, FROM does not start a clause.
                    if !(compound == "FROM" && last_clause == "COPY") {
                        let last_word = compound.rsplit(' ').next().unwrap_or(&compound).to_string();
                        let kw_text = self.apply_case(&compound, &original, true);
                        let ci = clause_indent(&paren_stack);
                        if !w.result.is_empty() && !w.at_line_start {
                            w.newline();
                            w.indent(ci);
                        } else if w.at_line_start && ci > 0 {
                            w.indent(ci);
                        }
                        w.write(&kw_text);
                        after_clause = true;
                        prev_was_keyword = true;
                        prev_keyword = last_word;
                        if let Some(p) = paren_stack.last_mut() {
                            p.has_clauses = true;
                        }
                        // WITH keeps the CTE name on its line.
                        if compound == "WITH" || compound == "WITH RECURSIVE" {
                            after_clause = false;
                        }
                        last_clause = compound;
                        i += 1 + extra;
                        continue;
                    }
                }

                let upper = text.to_ascii_uppercase();
                if upper == "BETWEEN" {
                    in_between = true;
                }
                if upper == "AND" || upper == "OR" {
                    // The AND of BETWEEN, and AND/OR in expression parentheses, stay inline.
                    let between_and = upper == "AND" && in_between;
                    if between_and || paren_stack.last().is_some_and(|p| !p.has_clauses) {
                        if between_and {
                            in_between = false;
                        }
                        w.space();
                    } else {
                        if !w.at_line_start {
                            w.newline();
                        }
                        w.indent(content_indent(&paren_stack));
                        after_clause = false;
                    }
                    w.write(&self.apply_case(&upper, text, true));
                    prev_was_keyword = true;
                    prev_keyword = upper;
                    i += 1;
                    continue;
                }

                if after_clause {
                    w.newline();
                    w.indent(content_indent(&paren_stack));
                    after_clause = false;
                } else if !w.result.is_empty() && w.last() != Some(b'.') {
                    w.space();
                }
                w.write(&self.apply_case(&upper, text, false));
                prev_was_keyword = true;
                prev_keyword = upper;
                i += 1;
                continue;
            }

            if after_clause {
                w.newline();
                w.indent(content_indent(&paren_stack));
                after_clause = false;
            } else if !w.result.is_empty() && w.last() != Some(b'.') {
                w.space();
            }
            w.write(text);
            prev_was_keyword = false;
            prev_keyword.clear();
            i += 1;
        }
        w.result
    }

    /// Phase 2: collapses a `{ ... }` struct literal onto one line when it fits.
    fn inline_struct_literals(&self, formatted: &str) -> String {
        if self.threshold() == 0 {
            return formatted.to_string();
        }
        let lines = split_lines(formatted);
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            if trim_left(line).ends_with('{')
                && let Some(close) = (i + 1..lines.len()).find(|&k| trim_left(&lines[k]) == "}")
                && close > i + 1
            {
                let flat: Vec<&str> = lines[i + 1..close].iter().map(|l| trim_left(l)).collect();
                let merged = format!("{line} {} }}", flat.join(" "));
                if render_width(&merged) <= self.threshold() {
                    out.push(merged);
                    i = close + 1;
                    continue;
                }
            }
            out.push(line.clone());
            i += 1;
        }
        join_lines(&out)
    }

    /// Phase 3: merges a clause keyword line with its content when the result fits, or when
    /// the content is a single line.
    fn merge_short_clauses(&self, formatted: &str) -> String {
        if self.threshold() == 0 {
            return formatted.to_string();
        }
        let lines = split_lines(formatted);
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let clause_ind = leading_spaces(line);
            let trimmed = trim_left(line);
            if !trimmed.is_empty() && is_clause_keyword_line(trimmed) {
                let content_ind = clause_ind + self.config.indent_size;
                let mut is_simple = true;
                let mut content_end = i + 1;
                for (j, cline) in lines.iter().enumerate().skip(i + 1) {
                    let ctrimmed = trim_left(cline);
                    let cind = leading_spaces(cline);
                    if ctrimmed.is_empty() || cind <= clause_ind {
                        break;
                    }
                    if cind > content_ind || is_clause_keyword_line(ctrimmed) {
                        is_simple = false;
                        break;
                    }
                    content_end = j + 1;
                    if cline.contains('\n') {
                        break;
                    }
                }

                // A `--` comment can only be on the last merged line.
                if is_simple && content_end > i + 1 && !lines[i + 1..content_end - 1].iter().any(|l| l.contains("--")) {
                    let flat: Vec<&str> = lines[i + 1..content_end].iter().map(|l| trim_left(l)).collect();
                    let merged = format!("{}{trimmed} {}", spaces(clause_ind), flat.join(" "));
                    let single_content = content_end == i + 2;
                    if single_content || render_width(&merged) <= self.threshold() {
                        out.push(merged);
                        i = content_end;
                        continue;
                    }
                }

                // CREATE and COPY keep the name on the keyword line.
                if i + 1 < lines.len() {
                    let upper = trimmed.to_ascii_uppercase();
                    if upper.starts_with("CREATE") || upper == "COPY" {
                        let next = &lines[i + 1];
                        if leading_spaces(next) > clause_ind {
                            out.push(format!("{}{trimmed} {}", spaces(clause_ind), trim_left(next)));
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            out.push(line.clone());
            i += 1;
        }
        join_lines(&out)
    }

    /// Phase 4: lifts the first predicate of a multiline WHERE/HAVING onto the keyword line and
    /// aligns the following AND/OR at the clause indent + 2.
    fn collapse_first_condition(&self, formatted: &str) -> String {
        if self.threshold() == 0 {
            return formatted.to_string();
        }
        let lines = split_lines(formatted);
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let clause_ind = leading_spaces(line);
            let trimmed = trim_left(line);
            let upper = trimmed.to_ascii_uppercase();
            if upper != "WHERE" && upper != "HAVING" {
                out.push(line.clone());
                i += 1;
                continue;
            }
            let content_ind = clause_ind + self.config.indent_size;
            let mut is_simple = true;
            let mut j = i + 1;
            while j < lines.len() {
                let cind = leading_spaces(&lines[j]);
                if trim_left(&lines[j]).is_empty() || cind <= clause_ind {
                    break;
                }
                if cind > content_ind {
                    is_simple = false;
                    break;
                }
                j += 1;
            }
            if !is_simple || j == i + 1 {
                out.push(line.clone());
                i += 1;
                continue;
            }
            let merged_first = format!("{}{trimmed} {}", spaces(clause_ind), trim_left(&lines[i + 1]));
            if render_width(&merged_first) > self.threshold() {
                out.push(line.clone());
                i += 1;
                continue;
            }
            out.push(merged_first);
            for cline in &lines[i + 2..j] {
                let ctrimmed = trim_left(cline);
                let cupper = ctrimmed.to_ascii_uppercase();
                if cupper.starts_with("AND ") || cupper.starts_with("OR ") {
                    out.push(format!("{}{ctrimmed}", spaces(clause_ind + 2)));
                } else {
                    out.push(cline.clone());
                }
            }
            i = j;
        }
        join_lines(&out)
    }

    /// Phase 5: puts each column of a CREATE TABLE column list on its own line, from both
    /// `CREATE TABLE t(a, b)` and the two-line `CREATE TABLE` / `t(a, b)`.
    fn expand_table_definition(&self, formatted: &str) -> String {
        let lines = split_lines(formatted);
        let mut out = Vec::with_capacity(lines.len() + 16);
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let indent = leading_spaces(line);
            let trimmed = trim_left(line);
            let upper_trimmed = trimmed.to_ascii_uppercase();
            let prefix_len = match_create_table_prefix(&upper_trimmed);
            if prefix_len == 0 {
                out.push(line.clone());
                i += 1;
                continue;
            }
            // CREATE TABLE ... AS has no column list.
            let after_prefix = &upper_trimmed[prefix_len..];
            if let Some(as_pos) = after_prefix.find(" AS")
                && matches!(after_prefix.as_bytes().get(as_pos + 3), None | Some(b' '))
            {
                out.push(line.clone());
                i += 1;
                continue;
            }

            let (prefix_part, col_list, suffix, consumed_next) =
                if let Some(paren_pos) = find_open_paren(line, indent + prefix_len) {
                    let Some(close_pos) = find_matching_close(line, paren_pos) else {
                        out.push(line.clone());
                        i += 1;
                        continue;
                    };
                    (
                        line[..paren_pos].to_string(),
                        &line[paren_pos + 1..close_pos],
                        &line[close_pos + 1..],
                        false,
                    )
                } else if i + 1 < lines.len() {
                    let next_line = &lines[i + 1];
                    let found = find_open_paren(next_line, 0)
                        .and_then(|open| find_matching_close(next_line, open).map(|close| (open, close)));
                    let Some((next_paren, close_pos)) = found else {
                        out.push(line.clone());
                        i += 1;
                        continue;
                    };
                    let table_spec = trim_left(&next_line[..next_paren]);
                    (
                        format!("{}{trimmed} {table_spec}", spaces(indent)),
                        &next_line[next_paren + 1..close_pos],
                        &next_line[close_pos + 1..],
                        true,
                    )
                } else {
                    out.push(line.clone());
                    i += 1;
                    continue;
                };

            let cols = split_top_level_commas(col_list);
            if cols.len() <= 1 {
                out.push(format!("{prefix_part}({col_list}){suffix}"));
            } else {
                let col_indent = spaces(indent + self.config.indent_size);
                out.push(format!("{prefix_part}("));
                for (c, col) in cols.iter().enumerate() {
                    let comma = if c + 1 < cols.len() { "," } else { "" };
                    out.push(format!("{col_indent}{col}{comma}"));
                }
                out.push(format!("{}){suffix}", spaces(indent)));
            }
            i += if consumed_next { 2 } else { 1 };
        }
        join_lines(&out)
    }

    /// Phase 6: puts each WHEN/ELSE of a CASE on a too-long line on its own line, until no line
    /// changes.
    fn expand_case_expressions(&self, formatted: &str) -> String {
        if self.threshold() == 0 {
            return formatted.to_string();
        }
        let mut lines = split_lines(formatted);
        let mut changed = true;
        while changed {
            changed = false;
            let mut out = Vec::with_capacity(lines.len());
            for line in &lines {
                if render_width(line) <= self.threshold() {
                    out.push(line.clone());
                    continue;
                }
                let line_indent = leading_spaces(line);
                let Some(case_pos) = find_keyword_any(line, 0, "CASE") else {
                    out.push(line.clone());
                    continue;
                };
                let Some(end_pos) = find_case_end(line, case_pos) else {
                    out.push(line.clone());
                    continue;
                };
                let branches = split_case_branches(&line[case_pos + 4..end_pos]);
                if branches.len() < 2 {
                    out.push(line.clone());
                    continue;
                }
                changed = true;
                let prefix = &line[..case_pos];
                let prefix_end = prefix.trim_end_matches(' ').len();
                let end_and_suffix = &line[end_pos..];
                if prefix[..prefix_end].ends_with('(') {
                    // CASE right after `(`: break after the `(` and indent the CASE.
                    let case_indent = line_indent + self.config.indent_size;
                    let branch_indent = spaces(case_indent + self.config.indent_size);
                    out.push(prefix[..prefix_end].to_string());
                    out.push(format!("{}CASE", spaces(case_indent)));
                    out.extend(branches.iter().map(|b| format!("{branch_indent}{b}")));
                    out.push(format!("{}{end_and_suffix}", spaces(case_indent)));
                } else {
                    let branch_indent = spaces(case_pos + self.config.indent_size);
                    out.push(line[..case_pos + 4].to_string());
                    out.extend(branches.iter().map(|b| format!("{branch_indent}{b}")));
                    out.push(format!("{}{end_and_suffix}", spaces(case_pos)));
                }
            }
            lines = out;
        }
        join_lines(&lines)
    }

    /// Phase 7: puts each value of the longest parenthesized list on a too-long line on its own
    /// line, the `)` after the last one, until no line changes.
    fn expand_long_paren_lists(&self, formatted: &str) -> String {
        if self.threshold() == 0 {
            return formatted.to_string();
        }
        let mut lines = split_lines(formatted);
        let mut changed = true;
        while changed {
            changed = false;
            let mut out = Vec::with_capacity(lines.len());
            for line in &lines {
                if render_width(line) <= self.threshold() {
                    out.push(line.clone());
                    continue;
                }
                let found = find_longest_paren_list(line, 0)
                    .and_then(|open| find_matching_close(line, open).map(|close| (open, close)));
                let Some((open_paren, close_paren)) = found else {
                    out.push(line.clone());
                    continue;
                };
                let values = split_top_level_commas(&line[open_paren + 1..close_paren]);
                if values.len() <= 1 {
                    out.push(line.clone());
                    continue;
                }
                changed = true;
                let suffix = &line[close_paren..];
                let val_indent = spaces(leading_spaces(line) + self.config.indent_size);
                out.push(line[..=open_paren].to_string());
                for (v, value) in values.iter().enumerate() {
                    let end = if v + 1 < values.len() { "," } else { suffix };
                    out.push(format!("{val_indent}{value}{end}"));
                }
            }
            lines = out;
        }
        join_lines(&lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sql: &str) -> String {
        format_sql(sql, &FormatterConfig::default())
    }

    fn with_threshold(inline_threshold: usize) -> FormatterConfig {
        FormatterConfig {
            inline_threshold,
            ..FormatterConfig::default()
        }
    }

    fn check_cases(cases: &[(&str, FormatterConfig, &str)]) {
        let failures: Vec<String> = cases
            .iter()
            .filter_map(|(sql, config, expected)| {
                let got = format_sql(sql, config);
                (got != *expected).then(|| format!("input: {sql}\n--- expected:\n{expected}\n--- got:\n{got}\n"))
            })
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }

    /// The `duckdb_format_sql(...) = $$...$$` cases of test/sql/function/autocomplete/sql_format.test.
    #[test]
    fn sql_format_test_cases() {
        let cases: &[(&str, FormatterConfig, &str)] = &[
            (
                r####"select $customer, $MiXeD, $select, $year, $1"####,
                FormatterConfig::default(),
                r####"SELECT $customer, $MiXeD, $select, $year, $1"####,
            ),
            (
                r####"SELECT $customer, $MiXeD, $SELECT, $YEAR, $1"####,
                FormatterConfig {
                    keyword_case: KeywordCase::Lower,
                    ..FormatterConfig::default()
                },
                r####"select $customer, $MiXeD, $SELECT, $YEAR, $1"####,
            ),
            (
                r####"select $select from t where id=$1"####,
                FormatterConfig {
                    inline_threshold: 0,
                    ..FormatterConfig::default()
                },
                r####"SELECT
    $select
FROM
    t
WHERE
    id = $1"####,
            ),
            (
                r####"select '$customer, $select', $1"####,
                FormatterConfig::default(),
                r####"SELECT '$customer, $select', $1"####,
            ),
            (
                r####"Select $customer, $MiXeD, $SELECT, $YEAR, $1"####,
                FormatterConfig {
                    keyword_case: KeywordCase::Preserve,
                    ..FormatterConfig::default()
                },
                r####"Select $customer, $MiXeD, $SELECT, $YEAR, $1"####,
            ),
            (
                r####"select a,b,c from t where x=1 and y=2 group by a,b order by a desc limit 10"####,
                FormatterConfig::default(),
                r####"SELECT a, b, c
FROM t
WHERE x = 1 AND y = 2
GROUP BY a, b
ORDER BY a DESC
LIMIT 10"####,
            ),
            (r####"select 1"####, FormatterConfig::default(), r####"SELECT 1"####),
            (
                r####"select a from t; select b from u"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t;
SELECT b
FROM u"####,
            ),
            (
                r####"select a from t1 inner join t2 on t1.id = t2.id where t1.active = true"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t1
INNER JOIN t2 ON t1.id = t2.id
WHERE t1.active = TRUE"####,
            ),
            (
                r####"select a from t1 left join t2 on t1.id = t2.id"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t1
LEFT JOIN t2 ON t1.id = t2.id"####,
            ),
            (
                r####"WITH cte AS (SELECT id FROM users WHERE active = 1) SELECT id FROM cte GROUP BY id HAVING count(*) > 5"####,
                FormatterConfig::default(),
                r####"WITH cte AS (
        SELECT id
        FROM users
        WHERE active = 1
    )
SELECT id
FROM cte
GROUP BY id
HAVING count(*) > 5"####,
            ),
            (
                r####"select id from orders where customer_id in (select id from customers where active = 1) and amount > 100"####,
                FormatterConfig::default(),
                r####"SELECT id
FROM orders
WHERE
    customer_id IN (
        SELECT id
        FROM customers
        WHERE active = 1
    )
    AND amount > 100"####,
            ),
            (
                r####"select a from t1 union all select a from t2"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t1
UNION ALL
SELECT a
FROM t2"####,
            ),
            (
                r####"select case when x > 0 then 'pos' else 'neg' end as sign from t"####,
                FormatterConfig::default(),
                r####"SELECT CASE WHEN x > 0 THEN 'pos' ELSE 'neg' END AS sign
FROM t"####,
            ),
            (
                r####"INSERT INTO t (a, b) VALUES (1, 2)"####,
                FormatterConfig::default(),
                r####"INSERT INTO t(a, b)
VALUES (1, 2)"####,
            ),
            (
                r####"update t set a = 1, b = 2 where id = 3"####,
                FormatterConfig::default(),
                r####"UPDATE t
SET a = 1, b = 2
WHERE id = 3"####,
            ),
            (
                r####"delete from t where id = 1"####,
                FormatterConfig::default(),
                r####"DELETE FROM t
WHERE id = 1"####,
            ),
            (
                r####"select 1 from t where a = 1 and b = 2 or c = 3"####,
                FormatterConfig::default(),
                r####"SELECT 1
FROM t
WHERE a = 1 AND b = 2 OR c = 3"####,
            ),
            (
                r####"select 1 from t where some_long_column_name = 1 and another_long_column = 2 or yet_another_column = 3"####,
                FormatterConfig::default(),
                r####"SELECT 1
FROM t
WHERE some_long_column_name = 1 AND another_long_column = 2 OR yet_another_column = 3"####,
            ),
            (
                r####"select a from t where x = 1"####,
                FormatterConfig {
                    inline_threshold: 0,
                    ..FormatterConfig::default()
                },
                r####"SELECT
    a
FROM
    t
WHERE
    x = 1"####,
            ),
            (
                r####"select a from t where x = 1"####,
                FormatterConfig {
                    indent_size: 2,
                    inline_threshold: 0,
                    ..FormatterConfig::default()
                },
                r####"SELECT
  a
FROM
  t
WHERE
  x = 1"####,
            ),
            (
                r####"select a, b, c from long_table_name"####,
                FormatterConfig {
                    inline_threshold: 10,
                    ..FormatterConfig::default()
                },
                r####"SELECT
    a,
    b,
    c
FROM long_table_name"####,
            ),
            (
                r####"SELECT a FROM t WHERE x = 1"####,
                FormatterConfig {
                    keyword_case: KeywordCase::Lower,
                    ..FormatterConfig::default()
                },
                r####"select a
from t
where x = 1"####,
            ),
            (
                r####"Select A From T Where X = 1"####,
                FormatterConfig {
                    keyword_case: KeywordCase::Preserve,
                    ..FormatterConfig::default()
                },
                r####"Select A
From T
Where X = 1"####,
            ),
            (
                r####"select a from t where x = 1"####,
                FormatterConfig {
                    keyword_case: KeywordCase::Upper,
                    ..FormatterConfig::default()
                },
                r####"SELECT a
FROM t
WHERE x = 1"####,
            ),
            (
                r####"CREATE TABLE t (id INTEGER PRIMARY KEY)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t(id INTEGER PRIMARY KEY)"####,
            ),
            (
                r####"CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id), amount DECIMAL(10,2) DEFAULT 0, status VARCHAR(20))"####,
                FormatterConfig::default(),
                r####"CREATE TABLE orders(
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    amount DECIMAL(10, 2) DEFAULT 0,
    status VARCHAR(20)
)"####,
            ),
            (
                r####"CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b), FOREIGN KEY (b) REFERENCES other(id))"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t(
    a INTEGER,
    b INTEGER,
    PRIMARY KEY (a, b),
    FOREIGN KEY (b) REFERENCES other(id)
)"####,
            ),
            (
                r####"CREATE TABLE t (id INTEGER, name VARCHAR(100) NOT NULL UNIQUE, created_at TIMESTAMP DEFAULT current_timestamp, PRIMARY KEY (id))"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t(
    id INTEGER,
    name VARCHAR(100) NOT NULL UNIQUE,
    created_at TIMESTAMP DEFAULT current_timestamp,
    PRIMARY KEY (id)
)"####,
            ),
            (
                r####"CREATE TABLE IF NOT EXISTS t (id INTEGER)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE IF NOT EXISTS t(id INTEGER)"####,
            ),
            (
                r####"CREATE TEMP TABLE t (id INTEGER, name VARCHAR)"####,
                FormatterConfig::default(),
                r####"CREATE TEMP TABLE t(
    id INTEGER,
    name VARCHAR
)"####,
            ),
            (
                r####"CREATE OR REPLACE TABLE t (id INTEGER)"####,
                FormatterConfig::default(),
                r####"CREATE OR REPLACE TABLE t(id INTEGER)"####,
            ),
            (
                r####"CREATE VIEW active_users AS SELECT id, name FROM users WHERE active = true"####,
                FormatterConfig::default(),
                r####"CREATE VIEW active_users AS
SELECT id, name
FROM users
WHERE active = TRUE"####,
            ),
            (
                r####"CREATE VIEW long_schema_name.long_view_name AS SELECT id, name FROM users WHERE active = true"####,
                FormatterConfig::default(),
                r####"CREATE VIEW long_schema_name.long_view_name AS
SELECT id, name
FROM users
WHERE active = TRUE"####,
            ),
            (
                r####"CREATE VIEW a_very_long_schema_name.a_very_long_view_name_here AS SELECT id FROM t"####,
                FormatterConfig::default(),
                r####"CREATE VIEW a_very_long_schema_name.a_very_long_view_name_here AS
SELECT id
FROM t"####,
            ),
            (
                r####"CREATE OR REPLACE VIEW analytics.monthly_summary AS SELECT region, sum(amount) AS total FROM orders GROUP BY region"####,
                FormatterConfig::default(),
                r####"CREATE OR REPLACE VIEW analytics.monthly_summary AS
SELECT region, sum(amount) AS total
FROM orders
GROUP BY region"####,
            ),
            (
                r####"CREATE TEMP VIEW v AS SELECT id FROM t"####,
                FormatterConfig::default(),
                r####"CREATE TEMP VIEW v AS
SELECT id
FROM t"####,
            ),
            (
                r####"CREATE INDEX idx_name ON orders (customer_id)"####,
                FormatterConfig::default(),
                r####"CREATE INDEX idx_name ON orders(customer_id)"####,
            ),
            (
                r####"CREATE UNIQUE INDEX idx_email ON users (email)"####,
                FormatterConfig::default(),
                r####"CREATE UNIQUE INDEX idx_email ON users(email)"####,
            ),
            (
                r####"CREATE SCHEMA analytics"####,
                FormatterConfig::default(),
                r####"CREATE SCHEMA analytics"####,
            ),
            (
                r####"CREATE SEQUENCE seq_id START 1 INCREMENT 1"####,
                FormatterConfig::default(),
                r####"CREATE SEQUENCE seq_id START 1 INCREMENT 1"####,
            ),
            (
                r####"CREATE MACRO increment(x) AS x + 1"####,
                FormatterConfig::default(),
                r####"CREATE MACRO increment (x) AS x + 1"####,
            ),
            (
                r####"CREATE OR REPLACE MACRO clamp(x, lo, hi) AS CASE WHEN x < lo THEN lo WHEN x > hi THEN hi ELSE x END"####,
                FormatterConfig::default(),
                r####"CREATE OR REPLACE MACRO clamp(x, lo, hi) AS CASE WHEN x < lo THEN lo WHEN x > hi THEN hi ELSE x END"####,
            ),
            (
                r####"CREATE MACRO range_rows(lo, hi) AS TABLE SELECT * FROM t WHERE id BETWEEN lo AND hi"####,
                FormatterConfig::default(),
                r####"CREATE MACRO range_rows(lo, hi) AS TABLE
SELECT *
FROM t
WHERE id BETWEEN lo AND hi"####,
            ),
            (
                r####"CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral')"####,
                FormatterConfig::default(),
                r####"CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral')"####,
            ),
            (
                r####"ALTER TABLE t ADD COLUMN email VARCHAR(100) NOT NULL"####,
                FormatterConfig::default(),
                r####"ALTER TABLE t ADD COLUMN email VARCHAR(100) NOT NULL"####,
            ),
            (
                r####"ALTER TABLE orders ADD CONSTRAINT pk PRIMARY KEY (id)"####,
                FormatterConfig::default(),
                r####"ALTER TABLE orders ADD CONSTRAINT pk PRIMARY KEY (id)"####,
            ),
            (
                r####"ALTER TABLE t DROP COLUMN old_col"####,
                FormatterConfig::default(),
                r####"ALTER TABLE t
DROP COLUMN old_col"####,
            ),
            (
                r####"select '🦆', '🏴‍☠️' from t"####,
                FormatterConfig::default(),
                r####"SELECT '🦆', '🏴‍☠️'
FROM t"####,
            ),
            (
                r####"select '🏴‍☠️' from t"####,
                FormatterConfig {
                    inline_threshold: 15,
                    ..FormatterConfig::default()
                },
                r####"SELECT '🏴‍☠️'
FROM t"####,
            ),
            (
                r####"WITH cte1 AS (SELECT id, name FROM users WHERE active = 1), cte2 AS (SELECT user_id, sum(amount) as total FROM orders GROUP BY user_id) SELECT cte1.name, cte2.total FROM cte1 JOIN cte2 ON cte1.id = cte2.user_id"####,
                FormatterConfig::default(),
                r####"WITH cte1 AS (
        SELECT id, name
        FROM users
        WHERE active = 1
    ),
    cte2 AS (
        SELECT user_id, sum(amount) AS total
        FROM orders
        GROUP BY user_id
    )
SELECT cte1.name, cte2.total
FROM cte1
JOIN cte2 ON cte1.id = cte2.user_id"####,
            ),
            (
                r####"WITH a AS (SELECT 1), b AS (SELECT 2), c AS (SELECT * FROM a UNION ALL SELECT * FROM b) SELECT * FROM c"####,
                FormatterConfig::default(),
                r####"WITH a AS (
        SELECT 1
    ),
    b AS (
        SELECT 2
    ),
    c AS (
        SELECT *
        FROM a
        UNION ALL
        SELECT *
        FROM b
    )
SELECT *
FROM c"####,
            ),
            (
                r####"WITH RECURSIVE tree AS (SELECT id, parent_id, name, 0 as depth FROM nodes WHERE parent_id IS NULL UNION ALL SELECT n.id, n.parent_id, n.name, t.depth + 1 FROM nodes n INNER JOIN tree t ON n.parent_id = t.id) SELECT * FROM tree ORDER BY depth, name"####,
                FormatterConfig::default(),
                r####"WITH RECURSIVE tree AS (
        SELECT id, parent_id, name, 0 AS depth
        FROM nodes
        WHERE parent_id IS NULL
        UNION ALL
        SELECT n.id, n.parent_id, n.name, t.depth + 1
        FROM nodes n
        INNER JOIN tree t ON n.parent_id = t.id
    )
SELECT *
FROM tree
ORDER BY depth, name"####,
            ),
            (
                r####"SELECT 'hello
world' FROM t"####,
                FormatterConfig::default(),
                r####"SELECT 'hello
world'
FROM t"####,
            ),
            (
                r####"SELECT 'line1
line2
line3', 42 FROM t"####,
                FormatterConfig::default(),
                r####"SELECT 'line1
line2
line3',
    42
FROM t"####,
            ),
            (
                r####"SELECT 'it''s a test' FROM t"####,
                FormatterConfig::default(),
                r####"SELECT 'it''s a test'
FROM t"####,
            ),
            (
                r####"SELECT a, b, c, d, e, f, g, h, i, j, k, l, m, n, coalesce(x, y, z) AS val, case when a > 1 then 'yes' when a > 0 then 'maybe' else 'no' end as result FROM very_long_table_name_here WHERE some_column_name = 1 AND another_column > 2 AND yet_another_column < 100 ORDER BY a, b, c, d"####,
                FormatterConfig::default(),
                r####"SELECT
    a,
    b,
    c,
    d,
    e,
    f,
    g,
    h,
    i,
    j,
    k,
    l,
    m,
    n,
    coalesce(x, y, z) AS val,
    CASE WHEN a > 1 THEN 'yes' WHEN a > 0 THEN 'maybe' ELSE 'no' END AS result
FROM very_long_table_name_here
WHERE some_column_name = 1 AND another_column > 2 AND yet_another_column < 100
ORDER BY a, b, c, d"####,
            ),
            (
                r####"SELECT a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q FROM very_long_table_name WHERE x = 1"####,
                FormatterConfig::default(),
                r####"SELECT a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q
FROM very_long_table_name
WHERE x = 1"####,
            ),
            (
                r####"SELECT a + b * c - d / e + f % g + h * (i + j) - k * l + m * n * o + p - q + r * s FROM t"####,
                FormatterConfig::default(),
                r####"SELECT a + b * c - d / e + f % g + h *(i + j) - k * l + m * n * o + p - q + r * s
FROM t"####,
            ),
            (
                r####"SELECT a, b, very_long_column_name_1, very_long_column_name_2, very_long_column_name_3, very_long_column_name_4 FROM t WHERE very_long_column_name_1 > 100 AND very_long_column_name_2 < 200 AND very_long_column_name_3 = 'test' AND very_long_column_name_4 IS NOT NULL GROUP BY ALL ORDER BY ALL"####,
                FormatterConfig::default(),
                r####"SELECT
    a,
    b,
    very_long_column_name_1,
    very_long_column_name_2,
    very_long_column_name_3,
    very_long_column_name_4
FROM t
WHERE very_long_column_name_1 > 100
  AND very_long_column_name_2 < 200
  AND very_long_column_name_3 = 'test'
  AND very_long_column_name_4 IS NOT NULL
GROUP BY ALL
ORDER BY ALL"####,
            ),
            (
                r####"SELECT a FROM mydb.myschema.mytable WHERE mydb.myschema.mytable.id = 1"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM mydb.myschema.mytable
WHERE mydb.myschema.mytable.id = 1"####,
            ),
            (
                r####"SELECT a FROM catalog1.schema1.table1 t1 INNER JOIN catalog2.schema2.table2 t2 ON t1.id = t2.id WHERE catalog1.schema1.table1.status = 'active'"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM catalog1.schema1.table1 t1
INNER JOIN catalog2.schema2.table2 t2 ON t1.id = t2.id
WHERE catalog1.schema1.table1.status = 'active'"####,
            ),
            (
                r####"SELECT s.id, s.name FROM mydb.information_schema.tables s WHERE s.table_schema = 'main'"####,
                FormatterConfig::default(),
                r####"SELECT s.id, s.name
FROM mydb.information_schema.tables s
WHERE s.table_schema = 'main'"####,
            ),
            (
                r####"CREATE TABLE mydb.myschema.mytable (id INTEGER PRIMARY KEY, name VARCHAR)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE mydb.myschema.mytable(
    id INTEGER PRIMARY KEY,
    name VARCHAR
)"####,
            ),
            (
                r####"SELECT name, count(*) FROM users GROUP BY ALL ORDER BY ALL"####,
                FormatterConfig::default(),
                r####"SELECT name, count(*)
FROM users
GROUP BY ALL
ORDER BY ALL"####,
            ),
            (
                r####"SELECT region, product, sum(amount), count(*) FROM sales GROUP BY ALL ORDER BY ALL LIMIT 100"####,
                FormatterConfig::default(),
                r####"SELECT region, product, sum(amount), count(*)
FROM sales
GROUP BY ALL
ORDER BY ALL
LIMIT 100"####,
            ),
            (
                r####"SELECT list_transform([1, 2, 3], x -> x + 1)"####,
                FormatterConfig::default(),
                r####"SELECT list_transform([1, 2, 3], x -> x + 1)"####,
            ),
            (
                r####"SELECT list_apply(list_filter([1,2,3,4,5], x -> x > 2), y -> y * 10)"####,
                FormatterConfig::default(),
                r####"SELECT list_apply(list_filter([1, 2, 3, 4, 5], x -> x > 2), y -> y * 10)"####,
            ),
            (
                r####"SELECT [1, 2, 3].list_transform(x -> x + 1)"####,
                FormatterConfig::default(),
                r####"SELECT [1, 2, 3].list_transform(x -> x + 1)"####,
            ),
            (
                r####"SELECT 'hello
world'.upper()"####,
                FormatterConfig::default(),
                r####"SELECT 'hello
world'.upper()"####,
            ),
            (
                r####"SELECT struct_pack(a := 1, b := 'hello').a"####,
                FormatterConfig::default(),
                r####"SELECT struct_pack(a := 1, b := 'hello').a"####,
            ),
            (
                r####"SELECT id6, v3, row_number() OVER (PARTITION BY id6 ORDER BY v3 DESC) AS order_v3 FROM x_group WHERE v3 IS NOT NULL"####,
                FormatterConfig::default(),
                r####"SELECT
    id6,
    v3,
    row_number() OVER (
        PARTITION BY id6
        ORDER BY v3 DESC
    ) AS order_v3
FROM x_group
WHERE v3 IS NOT NULL"####,
            ),
            (
                r####"SELECT sum(amount) OVER (PARTITION BY region, year ORDER BY month) FROM sales"####,
                FormatterConfig::default(),
                r####"SELECT
    sum(amount) OVER (
        PARTITION BY region, year
        ORDER BY month
    )
FROM sales"####,
            ),
            (
                r####"SELECT avg(salary) OVER (PARTITION BY department) FROM employees"####,
                FormatterConfig::default(),
                r####"SELECT
    avg(salary) OVER (
        PARTITION BY department
    )
FROM employees"####,
            ),
            (
                r####"CREATE TEMP TABLE ans AS SELECT id2, id4, pow(corr(v1, v2), 2) AS r2 FROM x_group GROUP BY id2, id4"####,
                FormatterConfig::default(),
                r####"CREATE TEMP TABLE ans AS
SELECT id2, id4, pow(corr(v1, v2), 2) AS r2
FROM x_group
GROUP BY id2, id4"####,
            ),
            (
                r####"CREATE TABLE t AS SELECT a, b FROM s"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t AS
SELECT a, b
FROM s"####,
            ),
            (
                r####"CREATE OR REPLACE TABLE t AS SELECT a, b FROM s"####,
                FormatterConfig::default(),
                r####"CREATE OR REPLACE TABLE t AS
SELECT a, b
FROM s"####,
            ),
            (
                r####"SELECT a FROM t WHERE k.keyword IN ('superhero', 'marvel-comics', 'based-on-comic', 'tv-special', 'fight', 'violence', 'magnet', 'web', 'claw', 'laser') AND kt.kind = 'movie'"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t
WHERE
    k.keyword IN (
        'superhero',
        'marvel-comics',
        'based-on-comic',
        'tv-special',
        'fight',
        'violence',
        'magnet',
        'web',
        'claw',
        'laser')
    AND kt.kind = 'movie'"####,
            ),
            (
                r####"SELECT a FROM t WHERE x IN (1, 2, 3)"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t
WHERE x IN (1, 2, 3)"####,
            ),
            (
                r####"SELECT a FROM t WHERE cn1.country_code = '[us]' AND it1.info = 'rating' AND t2.production_year BETWEEN 2005 AND 2008 AND lt.id = ml.link_type_id"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t
WHERE cn1.country_code = '[us]'
  AND it1.info = 'rating'
  AND t2.production_year BETWEEN 2005 AND 2008
  AND lt.id = ml.link_type_id"####,
            ),
            (
                r####"SELECT a FROM t WHERE x BETWEEN 1 AND 10"####,
                FormatterConfig::default(),
                r####"SELECT a
FROM t
WHERE x BETWEEN 1 AND 10"####,
            ),
            (
                r####"CREATE FUNCTION V29(x) AS LAST_VALUE(x IGNORE NULLS) OVER (ORDER BY x NULLS LAST); FORCE CHECKPOINT"####,
                FormatterConfig::default(),
                r####"CREATE FUNCTION V29(x) AS LAST_VALUE(x IGNORE NULLS) OVER (
        ORDER BY x NULLS LAST
    );
FORCE CHECKPOINT"####,
            ),
            (
                r####"SELECT bt.k, (k = 3 AND k + 1 = 4) OR (k = 2 AND k + 1 = 3) FROM base_table bt(k)"####,
                FormatterConfig::default(),
                r####"SELECT bt.k, (k = 3 AND k + 1 = 4) OR (k = 2 AND k + 1 = 3)
FROM base_table bt(k)"####,
            ),
            (
                r####"SELECT [1, 2, 3] FROM base_table bt(k)"####,
                FormatterConfig::default(),
                r####"SELECT [1, 2, 3]
FROM base_table bt(k)"####,
            ),
            (
                r####"SELECT list_transform([1, 2, 3], x -> x + 1)"####,
                FormatterConfig::default(),
                r####"SELECT list_transform([1, 2, 3], x -> x + 1)"####,
            ),
            (
                r####"SELECT [1, 2, 3].list_transform(x -> x + 1)"####,
                FormatterConfig::default(),
                r####"SELECT [1, 2, 3].list_transform(x -> x + 1)"####,
            ),
            (
                r####"CREATE TABLE strings AS SELECT ((i * 9582398353) % 1000)::VARCHAR AS s1 FROM RANGE(0, 10000000) tbl(i)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE strings AS
SELECT ((i * 9582398353) % 1000)::VARCHAR AS s1
FROM RANGE (0, 10000000) tbl(i)"####,
            ),
            (
                r####"SELECT a::INTEGER, b::VARCHAR(100) FROM t"####,
                FormatterConfig::default(),
                r####"SELECT a::INTEGER, b::VARCHAR(100)
FROM t"####,
            ),
            (
                r####"SELECT * REPLACE (make_date(EventDate) AS EventDate, epoch_ms(EventTime * 1000) AS EventTime, epoch_ms(ClientEventTime * 1000) AS ClientEventTime, epoch_ms(LocalEventTime * 1000) AS LocalEventTime) FROM t"####,
                FormatterConfig::default(),
                r####"SELECT * REPLACE (
    make_date(EventDate) AS EventDate,
    epoch_ms(EventTime * 1000) AS EventTime,
    epoch_ms(ClientEventTime * 1000) AS ClientEventTime,
    epoch_ms(LocalEventTime * 1000) AS LocalEventTime)
FROM t"####,
            ),
            (
                r####"SELECT pow(corr(v1, v2), 2) FROM t"####,
                FormatterConfig::default(),
                r####"SELECT pow(corr(v1, v2), 2)
FROM t"####,
            ),
            (
                r####"SELECT 100.00 * sum(CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) / sum(l_extendedprice * (1 - l_discount)) AS promo_revenue FROM t"####,
                FormatterConfig::default(),
                r####"SELECT 100.00 * sum(
    CASE
        WHEN p_type LIKE 'PROMO%' THEN l_extendedprice *(1 - l_discount)
        ELSE 0
    END) / sum(l_extendedprice *(1 - l_discount)) AS promo_revenue
FROM t"####,
            ),
            (
                r####"SELECT * FROM read_csv_auto('https://github.com/duckdb/duckdb-data/releases/download/v1.0/J1_1e7_NA_0_0.csv.gz')"####,
                FormatterConfig::default(),
                r####"SELECT *
FROM read_csv_auto('https://github.com/duckdb/duckdb-data/releases/download/v1.0/J1_1e7_NA_0_0.csv.gz')"####,
            ),
            (
                r####"/* hello */ SELECT 42"####,
                FormatterConfig::default(),
                r####"/* hello */
SELECT 42"####,
            ),
            (
                r####"/* Q6. Most authoritative users
\set tag 'Arnold_Schwarzenegger'
 */
SELECT 42"####,
                FormatterConfig::default(),
                r####"/* Q6. Most authoritative users
\set tag 'Arnold_Schwarzenegger'
 */
SELECT 42"####,
            ),
            (
                r####"CREATE FUNCTION DeltaPhi(p1, p2) AS (CASE WHEN FMod2Pi(p1['phi'] - p2['phi']) < -Pi() THEN FMod2Pi(p1['phi'] - p2['phi']) + 2 * Pi() WHEN FMod2Pi(p1['phi'] - p2['phi']) > Pi() THEN FMod2Pi(p1['phi'] - p2['phi']) - 2 * Pi() ELSE FMod2Pi(p1['phi'] - p2['phi']) END)"####,
                FormatterConfig::default(),
                r####"CREATE FUNCTION DeltaPhi(p1, p2) AS (
    CASE
        WHEN FMod2Pi(p1 ['phi'] - p2 ['phi']) < - Pi() THEN FMod2Pi(p1 ['phi'] - p2 ['phi']) + 2 * Pi()
        WHEN FMod2Pi(p1 ['phi'] - p2 ['phi']) > Pi() THEN FMod2Pi(p1 ['phi'] - p2 ['phi']) - 2 * Pi()
        ELSE FMod2Pi(p1 ['phi'] - p2 ['phi'])
    END)"####,
            ),
            (
                r####"CREATE OR REPLACE TABLE lineitem_sf100_random AS (SELECT * FROM lineitem ORDER BY hash(rowid + 42))"####,
                FormatterConfig::default(),
                r####"CREATE OR REPLACE TABLE lineitem_sf100_random AS (
        SELECT *
        FROM lineitem
        ORDER BY hash(rowid + 42)
    )"####,
            ),
            (
                r####"SELECT {'key': 'value', 'key2': 'value2', 'key3': 'value3'}"####,
                FormatterConfig::default(),
                r####"SELECT { 'key': 'value', 'key2': 'value2', 'key3': 'value3' }"####,
            ),
            (
                r####"SELECT {'l_orderkey': 1, 'l_partkey': 155190, 'l_suppkey': 7706, 'l_linenumber': 1, 'l_quantity': 17.00, 'l_extendedprice': 21168.23, 'l_discount': 0.04, 'l_tax': 0.02, 'l_returnflag': 'N', 'l_linestatus': 'O', 'l_shipdate': '1996-03-13', 'l_commitdate': '1996-02-12', 'l_receiptdate': '1996-03-22', 'l_shipinstruct': 'DELIVER IN PERSON', 'l_shipmode': 'TRUCK', 'l_comment': 'to beans x-ray carefull'}"####,
                FormatterConfig::default(),
                r####"SELECT
    {
            'l_orderkey': 1,
            'l_partkey': 155190,
            'l_suppkey': 7706,
            'l_linenumber': 1,
            'l_quantity': 17.00,
            'l_extendedprice': 21168.23,
            'l_discount': 0.04,
            'l_tax': 0.02,
            'l_returnflag': 'N',
            'l_linestatus': 'O',
            'l_shipdate': '1996-03-13',
            'l_commitdate': '1996-02-12',
            'l_receiptdate': '1996-03-22',
            'l_shipinstruct': 'DELIVER IN PERSON',
            'l_shipmode': 'TRUCK',
            'l_comment': 'to beans x-ray carefull'
    }"####,
            ),
            (
                r####"SELECT MAP {'l_orderkey': '1', 'l_partkey': '155190', 'l_suppkey': '7706', 'l_linenumber': '1', 'l_quantity': '17.00', 'l_extendedprice': '21168.23', 'l_discount': '0.04', 'l_tax': '0.02', 'l_returnflag': 'N', 'l_linestatus': 'O', 'l_shipdate': '1996-03-13', 'l_commitdate': '1996-02-12', 'l_receiptdate': '1996-03-22', 'l_shipinstruct': 'DELIVER IN PERSON', 'l_shipmode': 'TRUCK', 'l_comment': 'to beans x-ray carefull'}"####,
                FormatterConfig::default(),
                r####"SELECT
    MAP {
            'l_orderkey': '1',
            'l_partkey': '155190',
            'l_suppkey': '7706',
            'l_linenumber': '1',
            'l_quantity': '17.00',
            'l_extendedprice': '21168.23',
            'l_discount': '0.04',
            'l_tax': '0.02',
            'l_returnflag': 'N',
            'l_linestatus': 'O',
            'l_shipdate': '1996-03-13',
            'l_commitdate': '1996-02-12',
            'l_receiptdate': '1996-03-22',
            'l_shipinstruct': 'DELIVER IN PERSON',
            'l_shipmode': 'TRUCK',
            'l_comment': 'to beans x-ray carefull'
    }"####,
            ),
        ];
        check_cases(cases);
    }

    /// The exact-output cases of test/sql/function/autocomplete/sql_format_literals.test.
    #[test]
    fn sql_format_literals_test_cases() {
        let cases: &[(&str, FormatterConfig, &str)] = &[
            (
                r####"CREATE TABLE "foo(bar,baz)"(a INTEGER, b INTEGER)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE "foo(bar,baz)"(
    a INTEGER,
    b INTEGER
)"####,
            ),
            (
                r####"CREATE TABLE "foo(bar,baz"(a INTEGER, b INTEGER)"####,
                FormatterConfig {
                    inline_threshold: 0,
                    ..FormatterConfig::default()
                },
                r####"CREATE TABLE "foo(bar,baz"(
    a INTEGER,
    b INTEGER
)"####,
            ),
            (
                r####"CREATE TABLE t(`a,b` INTEGER, c INTEGER)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t(
    `a,b` INTEGER,
    c INTEGER
)"####,
            ),
            (
                r####"CREATE TABLE t(`a``b,c` INTEGER, c INTEGER)"####,
                FormatterConfig::default(),
                r####"CREATE TABLE t(
    `a``b,c` INTEGER,
    c INTEGER
)"####,
            ),
            (
                r####"CREATE TABLE `foo(bar,baz`(a INTEGER, b INTEGER)"####,
                FormatterConfig {
                    inline_threshold: 0,
                    ..FormatterConfig::default()
                },
                r####"CREATE TABLE `foo(bar,baz`(
    a INTEGER,
    b INTEGER
)"####,
            ),
        ];
        check_cases(cases);
    }

    #[test]
    fn empty_input() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("   "), "   ");
    }

    #[test]
    fn multiline_literals_are_kept() {
        for sql in [
            "SELECT 'a\n    b\n    c'",
            "SELECT 42 AS \"a\n    b\n    c\"",
            "SELECT 42 AS `a\n    b\n    c`",
            "SELECT $$a\n    b\n    c$$",
        ] {
            assert_eq!(fmt(sql), sql);
        }
    }

    #[test]
    fn literals_are_not_reformatted() {
        let config = with_threshold(20);
        let sql = "SELECT $value$CASE WHEN x THEN x ELSE y END$value$ AS s";
        assert!(format_sql(sql, &config).contains("$value$CASE WHEN x THEN x ELSE y END$value$"));
        let sql = "SELECT coalesce($value$x, (y,z), CASE WHEN x THEN y END$value$, 'fallback') AS s";
        assert!(format_sql(sql, &config).contains("$value$x, (y,z), CASE WHEN x THEN y END$value$"));
        let sql = "SELECT 42 AS `CASE WHEN x THEN y ELSE z END`";
        assert!(format_sql(sql, &config).contains("`CASE WHEN x THEN y ELSE z END`"));
    }

    #[test]
    fn line_comment_ends_before_next_clause() {
        // `range` is a keyword in the uppercase list.
        assert_eq!(
            fmt("SELECT 1 -- first\nFROM range(2)"),
            "SELECT 1 -- first\nFROM RANGE (2)"
        );
    }

    #[test]
    fn blank_lines_between_statements() {
        assert_eq!(
            fmt("SELECT 1;\n\nSELECT 2;\nSELECT 3"),
            "SELECT 1;\n\nSELECT 2;\nSELECT 3"
        );
    }

    #[test]
    fn parameters_stay_opaque_in_case_expansion() {
        for threshold in [100, 20] {
            for parameter in ["when", "else", "end", "case"] {
                let sql = format!(
                    "SELECT CASE WHEN ${parameter} THEN repeat('x', 120) ELSE repeat('y', 120) END AS \
                     very_long_result_column_alias_0123456789"
                );
                let got = format_sql(&sql, &with_threshold(threshold));
                assert!(got.contains(&format!("${parameter}")), "{got}");
                assert!(got.starts_with("SELECT CASE\n"), "{got}");
            }
        }
    }

    #[test]
    fn parameters_keep_their_spelling() {
        for case in [KeywordCase::Upper, KeywordCase::Lower, KeywordCase::Preserve] {
            for parameter in ["customer", "MiXeD", "select", "year", "1"] {
                let config = FormatterConfig {
                    keyword_case: case,
                    ..FormatterConfig::default()
                };
                let got = format_sql(&format!("SELECT ${parameter}::INTEGER AS value"), &config);
                assert!(got.contains(&format!("${parameter}::")), "{got}");
                assert!(got.to_ascii_uppercase().contains("::INTEGER"), "{got}");
            }
        }
    }

    // Layouts EXPLAIN (SQL) produces, from test/sql/sql_export/dml_statements.test.
    const DML_LAYOUTS: &[(&str, &str)] = &[
        (
            "INSERT INTO main.dml_target(a, b) SELECT r0.c0 AS c0, CAST('x' AS VARCHAR) AS c1 FROM (VALUES (CAST(10 AS INTEGER))) AS r0(c0)",
            "INSERT INTO main.dml_target(a, b)\nSELECT r0.c0 AS c0, CAST('x' AS VARCHAR) AS c1\nFROM (\n        VALUES (CAST(10 AS INTEGER))\n    ) AS r0(c0)",
        ),
        (
            "UPDATE main.dml_target SET b = r2.c0 FROM (SELECT CAST('z' AS VARCHAR) AS c0, r0.rowid AS c1 FROM main.dml_target AS r0(column0, column1) WHERE (r0.column0 = CAST(2 AS INTEGER))) AS r2(c0, c1) WHERE (dml_target.rowid = r2.c1)",
            "UPDATE main.dml_target\nSET b = r2.c0\nFROM (\n        SELECT CAST('z' AS VARCHAR) AS c0, r0.rowid AS c1\n        FROM main.dml_target AS r0(column0, column1)\n        WHERE (r0.column0 = CAST(2 AS INTEGER))\n    ) AS r2(c0, c1)\nWHERE (dml_target.rowid = r2.c1)",
        ),
        (
            "DELETE FROM main.dml_target WHERE (rowid = ANY (SELECT r0.rowid AS c1 FROM main.dml_target AS r0(column0, column1) WHERE (r0.column0 = CAST(3 AS INTEGER))))",
            "DELETE FROM main.dml_target\nWHERE (rowid = ANY (\n                SELECT r0.rowid AS c1\n                FROM main.dml_target AS r0(column0, column1)\n                WHERE (r0.column0 = CAST(3 AS INTEGER))\n            ))",
        ),
        (
            "INSERT INTO main.dml_target(a, b) SELECT r0.c0 AS c0, CAST('x' AS VARCHAR) AS c1 FROM (VALUES (CAST(20 AS INTEGER))) AS r0(c0) RETURNING \"system\".main.\"+\"(a, CAST(1 AS INTEGER)) AS next_a, b AS b",
            "INSERT INTO main.dml_target(a, b)\nSELECT r0.c0 AS c0, CAST('x' AS VARCHAR) AS c1\nFROM (\n        VALUES (CAST(20 AS INTEGER))\n    ) AS r0(c0)\nRETURNING \"system\".main.\"+\"(a, CAST(1 AS INTEGER)) AS next_a, b AS b",
        ),
        (
            "DELETE FROM main.dml_target WHERE (rowid = ANY (SELECT r0.rowid AS c1 FROM main.dml_target AS r0(column0, column1) WHERE (r0.column0 = CAST(20 AS INTEGER)))) RETURNING a AS a, b AS b, (rowid >= CAST(0 AS BIGINT)) AS has_row",
            "DELETE FROM main.dml_target\nWHERE (rowid = ANY (\n                SELECT r0.rowid AS c1\n                FROM main.dml_target AS r0(column0, column1)\n                WHERE (r0.column0 = CAST(20 AS INTEGER))\n            ))\nRETURNING a AS a, b AS b, (rowid >= CAST(0 AS BIGINT)) AS has_row",
        ),
    ];

    #[test]
    fn sql_export_layouts() {
        for (sql, expected) in DML_LAYOUTS {
            assert_eq!(fmt(sql), *expected, "input: {sql}");
            // Formatting is stable on its own output.
            assert_eq!(fmt(expected), *expected, "input: {expected}");
        }
    }

    #[test]
    fn tokenizer_token_types() {
        let tokens = tokenize("SELECT \"q\"\"x\", E'a\\'b', $$d$$, $t$e$t$, 1.5e-3, t.c, $1, $abc /* c */ -- l\n;");
        let got: Vec<(TokenType, &str)> = tokens.iter().map(|t| (t.ty, t.text.as_str())).collect();
        use TokenType::*;
        assert_eq!(
            got,
            vec![
                (Keyword, "SELECT"),
                (Identifier, "\"q\"\"x\""),
                (Operator, ","),
                (StringLiteral, "E'a\\'b'"),
                (Operator, ","),
                (StringLiteral, "$$d$$"),
                (Operator, ","),
                (StringLiteral, "$t$e$t$"),
                (Operator, ","),
                (NumberLiteral, "1.5e-3"),
                (Operator, ","),
                (Identifier, "t"),
                (NumberLiteral, "."),
                (Identifier, "c"),
                (Operator, ","),
                (Operator, "$"),
                (NumberLiteral, "1"),
                (Operator, ","),
                (Operator, "$"),
                (Identifier, "abc"),
                (Comment, "/* c */"),
                (Comment, "-- l\n"),
                (Terminator, ";"),
            ]
        );
    }

    #[test]
    fn operators_split_trailing_signs() {
        let tokens = tokenize("a<-1 b::c");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "<", "-", "1", "b", "::", "c"]);
    }

    #[test]
    fn phase_struct_inlining() {
        assert_eq!(fmt("SELECT {'a': 1}"), "SELECT { 'a': 1 }");
        assert_eq!(
            format_sql("SELECT {'a': 1, 'b': 2}", &with_threshold(10)),
            "SELECT\n    {\n            'a': 1,\n            'b': 2\n    }"
        );
    }

    #[test]
    fn phase_case_expansion_in_place() {
        let sql = "SELECT CASE WHEN a_long_column_name > 1 THEN 'first_long_value' WHEN a_long_column_name > 0 \
                   THEN 'second_long_value' ELSE 'other' END AS r FROM t";
        assert_eq!(
            fmt(sql),
            "SELECT CASE\n           WHEN a_long_column_name > 1 THEN 'first_long_value'\n           WHEN a_long_column_name > 0 \
             THEN 'second_long_value'\n           ELSE 'other'\n       END AS r\nFROM t"
        );
    }

    #[test]
    fn keyword_case_lower_of_compound() {
        let config = FormatterConfig {
            keyword_case: KeywordCase::Lower,
            ..FormatterConfig::default()
        };
        assert_eq!(
            format_sql("SELECT a FROM t GROUP  BY a ORDER BY a", &config),
            "select a\nfrom t\ngroup by a\norder by a"
        );
        let config = FormatterConfig {
            keyword_case: KeywordCase::Preserve,
            ..FormatterConfig::default()
        };
        assert_eq!(
            format_sql("select a from t Group By a", &config),
            "select a\nfrom t\nGroup By a"
        );
    }

    #[test]
    fn copy_from_is_not_a_clause() {
        assert_eq!(fmt("COPY t FROM 'x.csv'"), "COPY t FROM 'x.csv'");
    }
}
