//! The duckrs SQL parser: a port of DuckDB's PEG parser (`duckdb/src/parser/peg`).
//!
//! It tokenizes SQL, then matches DuckDB's own grammar (vendored from the baseline commit in
//! `grammar/`) to produce a parse tree, which the transformer turns into the AST ([`ast`]).
//! The transformer covers SELECT statements so far.

pub mod ast;
pub mod grammar;
pub mod keywords;
pub mod matcher;
mod pool;
pub mod tokenizer;
pub mod transformer;
pub mod unicode;

pub use grammar::Grammar;
pub use matcher::{ParseNode, TerminalValue};

use duckrs_common::{Error, ErrorKind, Result};
use matcher::Matcher;
use std::sync::Arc;
use tokenizer::{Token, TokenType};

/// One parsed statement: its parse tree and the tokens it refers to.
#[derive(Debug, Clone)]
pub struct ParsedStatement {
    pub tree: Arc<ParseNode>,
    pub tokens: Arc<Vec<Token>>,
}

/// Parses SQL text into one parse tree per statement.
pub fn parse(sql: &str) -> Result<Vec<ParsedStatement>> {
    let sql = sql.to_string();
    let (tokens, trees) = pool::run(move || parse_on_this_thread(&sql))??;
    let tokens = Arc::new(tokens);
    Ok(trees
        .into_iter()
        .map(|tree| ParsedStatement {
            tree,
            tokens: tokens.clone(),
        })
        .collect())
}

/// Parses SQL text into one AST statement per statement.
pub fn parse_ast(sql: &str) -> Result<Vec<ast::Statement>> {
    parse_ast_with(sql, &ParserOptions::default())
}

/// Settings that change how SQL is parsed (`ParserOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserOptions {
    /// `max_expression_depth`: how deep expression trees may nest.
    pub max_expression_depth: usize,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self {
            max_expression_depth: transformer::DEFAULT_MAX_EXPRESSION_DEPTH,
        }
    }
}

/// [`parse_ast`] with explicit options.
pub fn parse_ast_with(sql: &str, options: &ParserOptions) -> Result<Vec<ast::Statement>> {
    let sql = sql.to_string();
    let options = options.clone();
    pool::run(move || {
        let (_, trees) = parse_on_this_thread_with(&sql, &options)?;
        let mut transformer = transformer::Transformer::new(Grammar::duckdb());
        transformer.max_expression_depth = options.max_expression_depth;
        trees.iter().map(|tree| transformer.top_level_statement(tree)).collect()
    })?
}

/// Parses on the current thread (which must have a large enough stack).
pub fn parse_on_this_thread(sql: &str) -> Result<(Vec<Token>, Vec<Arc<ParseNode>>)> {
    parse_on_this_thread_with(sql, &ParserOptions::default())
}

/// [`parse_on_this_thread`] with explicit options.
pub fn parse_on_this_thread_with(sql: &str, options: &ParserOptions) -> Result<(Vec<Token>, Vec<Arc<ParseNode>>)> {
    let stripped = unicode::strip_unicode_spaces(sql);
    let tokens = tokenizer::tokenize(stripped.as_deref().unwrap_or(sql))?;
    let grammar = Grammar::duckdb();
    let statement = grammar.rule("Statement").expect("the grammar has a Statement rule");
    let mut matcher = Matcher::new(grammar, &tokens);
    let mut statements = Vec::new();
    let mut pos = 0;
    // Program <- TopLevelStatement*;  TopLevelStatement <- Statement? (';'+ / EndOfInput)
    loop {
        let end = match matcher.match_rule(statement, pos) {
            Some((end, tree)) => {
                statements.push(tree);
                end
            }
            None => pos,
        };
        match tokens[end].token_type {
            TokenType::Terminator => {
                pos = end;
                while tokens[pos].token_type == TokenType::Terminator {
                    pos += 1;
                }
                if tokens[pos].token_type == TokenType::EndOfInput {
                    break;
                }
            }
            TokenType::EndOfInput => break,
            _ if matcher.depth_exceeded => {
                return Err(Error::new(
                    ErrorKind::Parser,
                    format!(
                        "Max expression depth limit of {} exceeded. Use \"SET max_expression_depth TO x\" to \
                         increase the maximum expression depth.",
                        options.max_expression_depth
                    ),
                ));
            }
            _ => return Err(syntax_error(&tokens, matcher.max_position.max(end))),
        }
    }
    Ok((tokens, statements))
}

/// `syntax error at or near "..."` at the furthest token the matcher reached.
fn syntax_error(tokens: &[Token], position: usize) -> Error {
    let message = match tokens.get(position) {
        Some(t) if t.token_type != TokenType::EndOfInput => format!("syntax error at or near \"{}\"", t.text),
        _ => "syntax error at end of input".to_string(),
    };
    Error::new(ErrorKind::Parser, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statements() {
        assert_eq!(parse("SELECT 42").unwrap().len(), 1);
        assert_eq!(parse("SELECT 1; SELECT 2;;").unwrap().len(), 2);
        assert_eq!(parse("").unwrap().len(), 0);
        assert_eq!(parse(";").unwrap().len(), 0);
        let ok = [
            "SELECT a, b + 1 AS c FROM t WHERE x > 5 GROUP BY ALL ORDER BY 1 DESC LIMIT 10",
            "FROM tbl SELECT *",
            "CREATE TABLE t (i INTEGER PRIMARY KEY, s VARCHAR DEFAULT 'x', d DECIMAL(10, 2))",
            "INSERT INTO t VALUES (1, 'a'), (2, NULL)",
            "WITH RECURSIVE r AS (SELECT 1 AS x UNION ALL SELECT x + 1 FROM r WHERE x < 10) SELECT * FROM r",
            "SELECT [1, 2, 3][2], {'a': 1}.a, list_transform([1, 2], x -> x * 2)",
            "SELECT CAST('1' AS INTEGER), '2'::BIGINT, INTERVAL '1 day', DATE '1992-01-01'",
            "SELECT * EXCLUDE (a) REPLACE (b + 1 AS b) FROM t",
            "PIVOT t ON year USING sum(x)",
            "UPDATE t SET a = 1 WHERE b IN (SELECT c FROM u)",
            "COPY t TO 'out.csv' (HEADER, DELIMITER ',')",
            "SELECT \"Quoted\", `bt` FROM 'data/file.csv'",
            "SELECT 'a'\n'b'",
        ];
        for sql in ok {
            if let Err(e) = parse(sql) {
                panic!("{sql}: {e}");
            }
        }
    }

    #[test]
    fn syntax_errors_match_duckdb() {
        assert_eq!(
            parse("SELEC 1").unwrap_err().to_string(),
            "Parser Error: syntax error at or near \"SELEC\""
        );
        assert_eq!(
            parse("SELECT 1 +").unwrap_err().to_string(),
            "Parser Error: syntax error at end of input"
        );
        assert_eq!(
            parse("SELECT * FROM t WHERE").unwrap_err().to_string(),
            "Parser Error: syntax error at end of input"
        );
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let depth = 2000;
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        // Either outcome is fine; it must not crash.
        let _ = parse(&sql);
    }
}
