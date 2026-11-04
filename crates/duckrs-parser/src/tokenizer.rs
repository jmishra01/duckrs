// Ported from duckdb/src/parser/peg/tokenizer/{base_tokenizer,parser_tokenizer}.cpp @ rust-port-baseline

use crate::keywords;
use duckrs_common::{Error, ErrorKind, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Keyword,
    StringLiteral,
    NumberLiteral,
    Operator,
    Identifier,
    /// `;`
    Terminator,
    EndOfInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub token_type: TokenType,
    pub text: String,
    /// Byte offset in the SQL text.
    pub offset: usize,
    /// A newline separates this token from the previous one (string literal continuation).
    pub preceded_by_newline: bool,
    /// A block comment separates this token from the previous one.
    pub preceded_by_block_comment: bool,
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

pub(crate) fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn is_single_byte_operator(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' | b':' | b'?' | b'$' | b'#'
    )
}

pub(crate) fn is_initial_number(c: u8) -> bool {
    c.is_ascii_digit() || c == b'.'
}

pub(crate) fn is_number_char(c: u8) -> bool {
    is_initial_number(c) || matches!(c, b'e' | b'E' | b'_')
}

pub(crate) fn is_scientific(c: u8) -> bool {
    c == b'e' || c == b'E'
}

fn is_control_flow(c: u8) -> bool {
    matches!(c, b'\'' | b';' | b'"' | b'.')
}

/// `StringUtil::CharacterIsOperator`: any ASCII punctuation except `_`.
fn is_string_util_operator(c: u8) -> bool {
    c != b'_' && matches!(c, b'!'..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~')
}

pub(crate) fn is_keyword_char(c: u8) -> bool {
    !is_single_byte_operator(c) && !is_string_util_operator(c) && !is_space(c) && !is_control_flow(c)
}

/// `Tokenizer::CharacterIsOperator`: characters that can continue an operator.
pub(crate) fn is_operator_char(c: u8) -> bool {
    matches!(
        c,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<' | b'>' | b'=' | b'~' | b'!' | b'@' | b'&' | b'|'
    )
}

fn is_special_string_prefix(c: u8) -> bool {
    matches!(c, b'N' | b'n' | b'X' | b'x' | b'E' | b'e' | b'B' | b'b')
}

fn is_dollar_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn syntax_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Parser, message)
}

struct Tokens<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    last_block_comment: Option<usize>,
}

impl Tokens<'_> {
    fn push_raw(&mut self, text: String, offset: usize, token_type: TokenType) {
        self.tokens.push(Token {
            token_type,
            text,
            offset,
            preceded_by_newline: false,
            preceded_by_block_comment: false,
        });
    }

    /// `ParserTokenizerBehavior::PushToken` / `TokenizerBehavior::PushToken`.
    fn push(&mut self, start: usize, end: usize, token_type: Option<TokenType>, unterminated: bool) -> Result<()> {
        let bytes = self.sql.as_bytes();
        let Some(token_type) = token_type else {
            // A comment.
            if unterminated {
                let comment = &self.sql[start..end];
                return Err(syntax_error(format!(
                    "syntax error: unterminated /* comment at or near \"{comment}\""
                )));
            }
            if end >= start + 2 && bytes[start] == b'/' && bytes[start + 1] == b'*' {
                self.last_block_comment = Some(start);
            }
            return Ok(());
        };
        if start >= end {
            return Ok(());
        }
        if token_type == TokenType::Identifier
            && end == start + 2
            && ((bytes[start] == b'"' && bytes[start + 1] == b'"')
                || (bytes[start] == b'`' && bytes[start + 1] == b'`'))
        {
            return Err(syntax_error("zero-length delimited identifier"));
        }
        let mut token = Token {
            token_type,
            text: self.sql[start..end].to_string(),
            offset: start,
            preceded_by_newline: false,
            preceded_by_block_comment: false,
        };
        if let Some(prev) = self.tokens.last() {
            let prev_end = prev.offset + prev.text.len();
            if self.last_block_comment.is_some_and(|c| c >= prev_end && c < start) {
                token.preceded_by_block_comment = true;
            }
            token.preceded_by_newline = bytes[prev_end.min(start)..start]
                .iter()
                .any(|&c| c == b'\n' || c == b'\r');
        }
        self.tokens.push(token);
        Ok(())
    }

    /// `PushOperatorToken`: an operator cannot end in `+`/`-` unless it contains one of
    /// `~ ! @ # % ^ & | \` ?` (the PostgreSQL rule); trailing signs become their own tokens.
    fn push_operator(&mut self, start: usize, end: usize) -> Result<()> {
        let bytes = self.sql.as_bytes();
        let special = bytes[start..end]
            .iter()
            .any(|c| matches!(c, b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?'));
        let mut end_pos = end;
        if !special {
            while end_pos > start + 1 && matches!(bytes[end_pos - 1], b'+' | b'-') {
                end_pos -= 1;
            }
        }
        self.push(start, end_pos, Some(TokenType::Operator), false)?;
        for (pos, &c) in bytes.iter().enumerate().take(end).skip(end_pos) {
            self.push_raw((c as char).to_string(), pos, TokenType::Operator);
        }
        Ok(())
    }
}

/// Splits SQL into tokens (`Tokenizer::TokenizeInput` with the parser behavior). Comments are
/// dropped, `;` becomes a terminator token, and an end-of-input token is appended.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut t = Tokens {
        sql,
        tokens: Vec::new(),
        last_block_comment: None,
    };
    let mut state = State::Standard;
    let mut last_pos = 0;
    let mut escape_string = false;
    let mut quote = b'"';
    let mut dollar_marker: &str = "";
    let mut comment_depth = 0usize;
    let mut i = 0;
    while i < n {
        let c = bytes[i];
        match state {
            State::Standard => {
                if c == b'\'' {
                    state = State::StringLiteral;
                    last_pos = i;
                    escape_string = false;
                } else if c == b'"' || c == b'`' {
                    state = State::QuotedIdentifier;
                    quote = c;
                    last_pos = i;
                } else if c == b';' {
                    t.push_raw(";".into(), i, TokenType::Terminator);
                    last_pos = i + 1;
                } else if c == b'$' {
                    if i + 1 < n {
                        if bytes[i + 1].is_ascii_digit() {
                            // $1 is a parameter.
                            t.push_raw("$".into(), i, TokenType::Operator);
                        } else {
                            last_pos = i;
                            let next_dollar = bytes[i + 1..]
                                .iter()
                                .position(|&b| b == b'$' || !is_dollar_tag_char(b))
                                .map(|p| i + 1 + p)
                                .filter(|&p| bytes[p] == b'$');
                            match next_dollar {
                                None => t.push_raw("$".into(), i, TokenType::Operator),
                                Some(p) => {
                                    state = State::DollarQuotedString;
                                    dollar_marker = &sql[i + 1..p];
                                    i = p;
                                }
                            }
                        }
                    }
                } else if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
                    i += 1;
                    state = State::SingleLineComment;
                } else if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    i += 1;
                    state = State::MultiLineComment;
                    comment_depth = 1;
                } else if is_space(c) {
                    last_pos = i + 1;
                } else if c == b':' && i + 1 < n && (bytes[i + 1] == b':' || bytes[i + 1] == b'=') {
                    if i + 2 < n && is_operator_char(bytes[i + 2]) {
                        state = State::Operator;
                        last_pos = i;
                    } else {
                        t.push_raw(sql[i..i + 2].to_string(), last_pos, TokenType::Operator);
                        i += 1;
                        last_pos = i + 1;
                    }
                } else if is_single_byte_operator(c) {
                    t.push_raw((c as char).to_string(), last_pos, TokenType::Operator);
                    last_pos = i + 1;
                } else if is_initial_number(c) {
                    state = State::Numeric;
                    last_pos = i;
                } else if is_special_string_prefix(c) && i + 1 < n && bytes[i + 1] == b'\'' {
                    state = State::StringLiteral;
                    last_pos = i;
                    escape_string = c == b'E' || c == b'e';
                    i += 1;
                } else if is_string_util_operator(c) {
                    state = State::Operator;
                    last_pos = i;
                } else {
                    state = State::Keyword;
                    last_pos = i;
                }
            }
            State::Numeric => {
                let continues = is_initial_number(c)
                    || (c == b'_' && bytes[i - 1].is_ascii_digit() && i + 1 < n && bytes[i + 1].is_ascii_digit())
                    || (is_scientific(c)
                        && !is_scientific(bytes[i - 1])
                        && (bytes[last_pos].is_ascii_digit() || bytes[i - 1].is_ascii_digit()))
                    || ((c == b'+' || c == b'-') && is_scientific(bytes[i - 1]));
                if !continues {
                    while !is_initial_number(bytes[i - 1]) {
                        i -= 1;
                    }
                    t.push(last_pos, i, Some(TokenType::NumberLiteral), false)?;
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::Operator => {
                let comment_start = (c == b'-' && i + 1 < n && bytes[i + 1] == b'-')
                    || (c == b'/' && i + 1 < n && bytes[i + 1] == b'*');
                if comment_start || !is_operator_char(c) {
                    t.push_operator(last_pos, i)?;
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::Keyword => {
                if c != b'$' && !is_keyword_char(c) {
                    let word = &sql[last_pos..i];
                    let ty = if keywords::is_keyword(word) {
                        TokenType::Keyword
                    } else {
                        TokenType::Identifier
                    };
                    t.push(last_pos, i, Some(ty), false)?;
                    state = State::Standard;
                    last_pos = i;
                    continue;
                }
            }
            State::StringLiteral => {
                if c == b'\\' && i + 1 < n && escape_string {
                    i += 1;
                } else if c == b'\'' {
                    if i + 1 < n && bytes[i + 1] == b'\'' {
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, Some(TokenType::StringLiteral), false)?;
                        last_pos = i + 1;
                        escape_string = false;
                        state = State::Standard;
                    }
                }
            }
            State::QuotedIdentifier => {
                if c == quote {
                    if i + 1 < n && bytes[i + 1] == quote {
                        i += 1;
                    } else {
                        t.push(last_pos, i + 1, Some(TokenType::Identifier), false)?;
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::SingleLineComment => {
                if c == b'\n' || c == b'\r' {
                    t.push(last_pos, i + 1, None, false)?;
                    last_pos = i + 1;
                    state = State::Standard;
                }
            }
            State::MultiLineComment => {
                if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    i += 1;
                    comment_depth += 1;
                } else if c == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    i += 1;
                    comment_depth -= 1;
                    if comment_depth == 0 {
                        t.push(last_pos, i + 1, None, false)?;
                        last_pos = i + 1;
                        state = State::Standard;
                    }
                }
            }
            State::DollarQuotedString => {
                if c == b'$' && i + 1 < n {
                    let start = i + 1;
                    if let Some(end) = bytes[start..].iter().position(|&b| b == b'$').map(|p| start + p)
                        && &sql[start..end] == dollar_marker
                    {
                        t.push(last_pos, end + 1, Some(TokenType::StringLiteral), false)?;
                        state = State::Standard;
                        i = end;
                        last_pos = i + 1;
                    }
                }
            }
        }
        i += 1;
    }
    match state {
        State::SingleLineComment => {}
        State::MultiLineComment => t.push(last_pos, n, None, true)?,
        State::Operator => t.push_operator(last_pos, n)?,
        State::DollarQuotedString => t.push(last_pos, n, Some(TokenType::StringLiteral), true)?,
        State::StringLiteral => return Err(syntax_error("syntax error: unterminated string literal")),
        State::QuotedIdentifier => return Err(syntax_error("syntax error: unterminated quoted identifier")),
        State::Keyword | State::Standard | State::Numeric if last_pos < n => {
            let word = &sql[last_pos..];
            let ty = match state {
                State::Keyword if keywords::is_keyword(word) => TokenType::Keyword,
                State::Numeric => TokenType::NumberLiteral,
                _ => TokenType::Identifier,
            };
            t.push(last_pos, n, Some(ty), false)?;
        }
        _ => {}
    }
    t.push_raw(String::new(), n, TokenType::EndOfInput);
    Ok(t.tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<(TokenType, String)> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|t| (t.token_type, t.text))
            .collect()
    }

    #[test]
    fn basic_tokens() {
        use TokenType::*;
        let t = texts("SELECT a.b, 'it''s', \"Quoted Id\" FROM t WHERE x>=1.5e-3 -- comment\n;");
        let expected: Vec<(TokenType, &str)> = vec![
            (Keyword, "SELECT"),
            (Identifier, "a"),
            (NumberLiteral, "."),
            (Identifier, "b"),
            (Operator, ","),
            (StringLiteral, "'it''s'"),
            (Operator, ","),
            (Identifier, "\"Quoted Id\""),
            (Keyword, "FROM"),
            (Identifier, "t"),
            (Keyword, "WHERE"),
            (Identifier, "x"),
            (Operator, ">="),
            (NumberLiteral, "1.5e-3"),
            (Terminator, ";"),
            (EndOfInput, ""),
        ];
        let expected: Vec<(TokenType, String)> = expected.into_iter().map(|(t, s)| (t, s.to_string())).collect();
        assert_eq!(t, expected);
    }

    #[test]
    fn operators_and_strings() {
        use TokenType::*;
        assert_eq!(
            texts("a::INT, 1+-2, x->>'$.a', E'a\\'b', $$x$y$$, $tag$q$tag$, $1, 1_000")
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>(),
            [
                "a",
                "::",
                "INT",
                ",",
                "1",
                "+",
                "-",
                "2",
                ",",
                "x",
                "->>",
                "'$.a'",
                ",",
                "E'a\\'b'",
                ",",
                "$$x$y$$",
                ",",
                "$tag$q$tag$",
                ",",
                "$",
                "1",
                ",",
                "1_000",
                ""
            ]
        );
        assert_eq!(texts("/* a /* nested */ c */ 1")[0], (NumberLiteral, "1".to_string()));
        assert!(tokenize("SELECT 'abc").is_err());
        assert!(tokenize("SELECT \"\"").is_err());
        assert!(tokenize("SELECT /* x").is_err());
    }

    #[test]
    fn newline_flags() {
        let t = tokenize("'a'\n'b' /* c */ 'd'").unwrap();
        assert!(t[1].preceded_by_newline && !t[1].preceded_by_block_comment);
        assert!(t[2].preceded_by_block_comment);
    }
}
