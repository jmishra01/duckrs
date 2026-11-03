//! The duckrs SQL parser: a port of DuckDB's PEG parser (`duckdb/src/parser/peg`).
//!
//! It tokenizes SQL, then matches DuckDB's own grammar (vendored from the baseline commit in
//! `grammar/`) to produce a parse tree. The transformer that turns parse trees into an AST is
//! the next part of Phase 2.

pub mod grammar;
pub mod keywords;
pub mod matcher;
pub mod tokenizer;
pub mod unicode;

pub use grammar::Grammar;
pub use matcher::{ParseNode, TerminalValue};

use duckrs_common::{Error, ErrorKind, Result};
use matcher::Matcher;
use std::sync::Arc;
use tokenizer::{Token, TokenType};

/// Stack for the recursive matcher; deeply nested SQL needs far more than a default thread.
const PARSER_STACK_SIZE: usize = 512 * 1024 * 1024;

/// One parsed statement: its parse tree and the tokens it refers to.
#[derive(Debug, Clone)]
pub struct ParsedStatement {
    pub tree: Arc<ParseNode>,
    pub tokens: Arc<Vec<Token>>,
}

/// Parses SQL text into one parse tree per statement.
pub fn parse(sql: &str) -> Result<Vec<ParsedStatement>> {
    let sql = sql.to_string();
    let handle = std::thread::Builder::new()
        .stack_size(PARSER_STACK_SIZE)
        .spawn(move || parse_on_this_thread(&sql))
        .map_err(|e| Error::new(ErrorKind::Internal, format!("failed to start the parser thread: {e}")))?;
    let (tokens, trees) = handle
        .join()
        .map_err(|_| Error::new(ErrorKind::Internal, "the parser thread panicked"))??;
    let tokens = Arc::new(tokens);
    Ok(trees
        .into_iter()
        .map(|tree| ParsedStatement {
            tree,
            tokens: tokens.clone(),
        })
        .collect())
}

/// Parses on the current thread (which must have a large enough stack).
pub fn parse_on_this_thread(sql: &str) -> Result<(Vec<Token>, Vec<Arc<ParseNode>>)> {
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
