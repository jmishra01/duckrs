// Ported from duckdb/src/parser/peg/{matcher,matcher_stack,parser_packrat}.cpp and the terminal
// matchers in include/duckdb/parser/peg/matcher/ @ rust-port-baseline
//
// The C++ matcher is an explicit-stack machine with packrat caching and autocomplete hooks.
// This is a recursive interpreter with the same matching semantics: ordered choice, greedy
// repetition, case-insensitive literals, and packrat memoization of the rules DuckDB memoizes
// (`Grammar::memoized`), whose results are shared rather than copied.

use crate::grammar::{Grammar, Node, RuleId, Terminal};
use crate::keywords;
use crate::tokenizer::{
    Token, TokenType, is_initial_number, is_keyword_char, is_number_char, is_operator_char, is_scientific,
};
use std::collections::HashMap;
use std::sync::Arc;

/// What a terminal matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalValue {
    /// A keyword or punctuation literal from the grammar; the token text as written.
    Literal(String),
    /// An identifier, unquoted (`"a""b"` becomes `a"b`).
    Identifier(String),
    Number(String),
    /// A string literal without its quotes (`''` un-doubled, continuation lines joined), with
    /// its prefix (`E`, `X`, `B`, `N`) or `None` for a plain or dollar-quoted string.
    String {
        value: String,
        prefix: Option<char>,
    },
    Operator(String),
    EndOfInput,
}

/// A node of the parse tree (`ParseResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseNode {
    Terminal {
        token: usize,
        value: TerminalValue,
    },
    /// A sequence (`ListParseResult`).
    List(Vec<ParseNode>),
    /// The alternative of a choice that matched (`ChoiceParseResult`).
    Choice {
        alternative: usize,
        child: Box<ParseNode>,
    },
    Optional(Option<Box<ParseNode>>),
    Repeat(Vec<ParseNode>),
    /// A named rule's result.
    Rule {
        rule: RuleId,
        child: Arc<ParseNode>,
    },
}

thread_local! {
    /// How deep the current thread is in dropping parse nodes.
    static DROP_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Nesting up to which parse trees drop recursively; deeper subtrees drop iteratively, so
/// dropping a deep tree on a small-stack thread cannot overflow it.
const RECURSIVE_DROP_DEPTH: usize = 256;

/// Moves a node's only child out (leaving an empty placeholder), so it drops in our frame.
fn take_child(node: &mut ParseNode) -> Option<ParseNode> {
    match node {
        ParseNode::Terminal { .. } => None,
        ParseNode::List(_) | ParseNode::Repeat(_) => None,
        ParseNode::Choice { child, .. } => Some(std::mem::replace(child.as_mut(), ParseNode::List(Vec::new()))),
        ParseNode::Optional(child) => child.take().map(|c| *c),
        ParseNode::Rule { child, .. } => {
            // a shared subtree is dropped by its last owner
            Arc::get_mut(child).map(|inner| std::mem::replace(inner, ParseNode::List(Vec::new())))
        }
    }
}

impl Drop for ParseNode {
    fn drop(&mut self) {
        let depth = DROP_DEPTH.with(|d| d.get());
        if depth < RECURSIVE_DROP_DEPTH {
            DROP_DEPTH.with(|d| d.set(depth + 1));
            match self {
                ParseNode::List(items) | ParseNode::Repeat(items) => drop(std::mem::take(items)),
                _ => drop(take_child(self)),
            }
            DROP_DEPTH.with(|d| d.set(depth));
            return;
        }
        let mut pending = Vec::new();
        take_children(self, &mut pending);
        while let Some(mut node) = pending.pop() {
            take_children(&mut node, &mut pending);
        }
    }
}

/// Moves all of a node's children into `out`, leaving it childless.
fn take_children(node: &mut ParseNode, out: &mut Vec<ParseNode>) {
    match node {
        ParseNode::List(items) | ParseNode::Repeat(items) => out.append(items),
        _ => out.extend(take_child(node)),
    }
}

/// A fast hasher for the memo's (rule, position) keys; they are small integers, not attacker
/// controlled in a way that matters for a per-parse table.
#[derive(Default)]
struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }
}

type Memo = HashMap<(RuleId, usize), Option<(usize, Arc<ParseNode>)>, std::hash::BuildHasherDefault<KeyHasher>>;

pub struct Matcher<'a> {
    grammar: &'a Grammar,
    tokens: &'a [Token],
    memo: Memo,
    /// The furthest token any terminal reached; syntax errors point there.
    pub max_position: usize,
    /// Set when the input nested deeper than the matcher's stack budget allows.
    pub depth_exceeded: bool,
    /// Where the stack was when matching started, and how much of it matching may use.
    stack_base: usize,
    stack_budget: usize,
}

/// The stack the matcher may use by default: deep nesting fails cleanly instead of overflowing.
/// Input that deep (thousands of levels) is far past any expression depth limit.
pub const DEFAULT_STACK_BUDGET: usize = 448 * 1024 * 1024;

/// The address of a local on the current stack frame.
#[inline(always)]
pub(crate) fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

impl<'a> Matcher<'a> {
    pub fn new(grammar: &'a Grammar, tokens: &'a [Token]) -> Self {
        Self::with_stack_budget(grammar, tokens, DEFAULT_STACK_BUDGET)
    }

    /// A matcher that uses at most `stack_budget` bytes of the current thread's stack.
    pub fn with_stack_budget(grammar: &'a Grammar, tokens: &'a [Token], stack_budget: usize) -> Self {
        Self {
            grammar,
            tokens,
            memo: Memo::default(),
            max_position: 0,
            depth_exceeded: false,
            stack_base: stack_position(),
            stack_budget,
        }
    }

    /// Matches `rule` at token `pos`, returning the end position and the parse tree.
    pub fn match_rule(&mut self, rule: RuleId, pos: usize) -> Option<(usize, Arc<ParseNode>)> {
        self.match_rule_body(rule, pos)
            .map(|(end, child)| (end, Arc::new(ParseNode::Rule { rule, child })))
    }

    /// Matches `rule`, returning its body's parse tree; memoized for the packrat rules.
    fn match_rule_body(&mut self, rule: RuleId, pos: usize) -> Option<(usize, Arc<ParseNode>)> {
        let grammar = self.grammar;
        let memoized = grammar.memoized[rule];
        if memoized && let Some(cached) = self.memo.get(&(rule, pos)) {
            return cached.clone();
        }
        if self.stack_base.abs_diff(stack_position()) > self.stack_budget {
            self.depth_exceeded = true;
            return None;
        }
        let result = self
            .match_node(&grammar.rules[rule].node, pos)
            .map(|(end, child)| (end, Arc::new(child)));
        if memoized {
            self.memo.insert((rule, pos), result.clone());
        }
        result
    }

    fn match_node(&mut self, node: &Node, pos: usize) -> Option<(usize, ParseNode)> {
        match node {
            Node::Literal(lit) => self.match_literal(pos, |text| text.eq_ignore_ascii_case(lit)),
            Node::LiteralSet(_, set) => {
                self.match_literal(pos, |text| text.len() <= 64 && set.contains(&text.to_ascii_lowercase()))
            }
            Node::Rule(id) => self
                .match_rule_body(*id, pos)
                .map(|(end, child)| (end, ParseNode::Rule { rule: *id, child })),
            Node::Seq(items) => {
                let mut children = Vec::new();
                let mut cur = pos;
                for item in items {
                    let (end, child) = self.match_node(item, cur)?;
                    children.push(child);
                    cur = end;
                }
                Some((cur, ParseNode::List(children)))
            }
            Node::Choice(alternatives) => alternatives.iter().enumerate().find_map(|(i, alt)| {
                self.match_node(alt, pos).map(|(end, child)| {
                    (
                        end,
                        ParseNode::Choice {
                            alternative: i,
                            child: Box::new(child),
                        },
                    )
                })
            }),
            Node::Optional(inner) => Some(match self.match_node(inner, pos) {
                Some((end, child)) => (end, ParseNode::Optional(Some(Box::new(child)))),
                None => (pos, ParseNode::Optional(None)),
            }),
            Node::Star(inner) | Node::Plus(inner) => {
                let mut children = Vec::new();
                let mut cur = pos;
                while let Some((end, child)) = self.match_node(inner, cur) {
                    children.push(child);
                    if end == cur {
                        // An empty match would repeat forever.
                        break;
                    }
                    cur = end;
                }
                if matches!(node, Node::Plus(_)) && children.is_empty() {
                    return None;
                }
                Some((cur, ParseNode::Repeat(children)))
            }
            Node::Terminal(t) => self.match_terminal(*t, pos),
            Node::Unsupported => None,
        }
    }

    fn token(&self, pos: usize) -> Option<&'a Token> {
        self.tokens.get(pos)
    }

    fn advance(&mut self, pos: usize) -> usize {
        self.max_position = self.max_position.max(pos + 1);
        pos + 1
    }

    fn match_literal(&mut self, pos: usize, matches: impl Fn(&str) -> bool) -> Option<(usize, ParseNode)> {
        let token = self.token(pos)?;
        if token.token_type == TokenType::EndOfInput || !matches(&token.text) {
            return None;
        }
        let end = self.advance(pos);
        Some((
            end,
            ParseNode::Terminal {
                token: pos,
                value: TerminalValue::Literal(token.text.clone()),
            },
        ))
    }

    fn match_terminal(&mut self, terminal: Terminal, pos: usize) -> Option<(usize, ParseNode)> {
        let token = self.token(pos)?;
        let leaf = |value| ParseNode::Terminal { token: pos, value };
        match terminal {
            Terminal::EndOfInput => {
                if token.token_type != TokenType::EndOfInput {
                    return None;
                }
                Some((pos + 1, leaf(TerminalValue::EndOfInput)))
            }
            Terminal::Identifier { kind, paths } => {
                let cats = keywords::categories(&token.text);
                if cats != 0 && cats & kind.mask() == 0 {
                    return None;
                }
                let text = identifier_text(&token.text, paths, true)?;
                Some((self.advance(pos), leaf(TerminalValue::Identifier(text))))
            }
            Terminal::ReservedIdentifier { paths } => {
                let text = identifier_text(&token.text, paths, false)?;
                Some((self.advance(pos), leaf(TerminalValue::Identifier(text))))
            }
            Terminal::Number => {
                if !is_number_literal(&token.text) {
                    return None;
                }
                Some((self.advance(pos), leaf(TerminalValue::Number(token.text.clone()))))
            }
            Terminal::Operator { all } => {
                let text = &token.text;
                if token.token_type == TokenType::EndOfInput
                    || text.is_empty()
                    || (!all && has_special_precedence(text))
                    || !text.bytes().all(is_operator_char)
                {
                    return None;
                }
                Some((self.advance(pos), leaf(TerminalValue::Operator(text.clone()))))
            }
            Terminal::String => {
                let (mut value, prefix) = string_literal(token)?;
                let mut end = self.advance(pos);
                // A single-quoted string (any prefix) continues across a newline with plain
                // single-quoted strings: 'a'\n'b' is 'ab'.
                if !token.text.starts_with('$') {
                    while let Some(next) = self.token(end)
                        && next.preceded_by_newline
                        && !next.preceded_by_block_comment
                        && next.token_type == TokenType::StringLiteral
                        && next.text.starts_with('\'')
                        && let Some((more, None)) = string_literal(next)
                    {
                        value.push_str(&more);
                        end = self.advance(end);
                    }
                }
                Some((end, leaf(TerminalValue::String { value, prefix })))
            }
        }
    }
}

/// `IdentifierMatcher::IsIdentifier` plus unquoting. `unwrap_paths` is the plain identifier
/// matcher's behavior of unwrapping single-quoted path names.
fn identifier_text(text: &str, paths: bool, unwrap_paths: bool) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let single_quoted = bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'';
    if single_quoted && paths {
        return Some(if unwrap_paths {
            text[1..text.len() - 1].replace("''", "'")
        } else {
            text.to_string()
        });
    }
    let quoted = bytes.len() >= 2 && (bytes[0] == b'"' || bytes[0] == b'`') && bytes[0] == bytes[bytes.len() - 1];
    if quoted {
        let delimiter = bytes[0] as char;
        let inner = &text[1..text.len() - 1];
        return Some(inner.replace(&format!("{delimiter}{delimiter}"), &delimiter.to_string()));
    }
    if is_initial_number(bytes[0]) || bytes[bytes.len() - 1] == b'\'' || !is_keyword_char(bytes[0]) {
        return None;
    }
    Some(text.to_string())
}

/// `NumberLiteralMatcher::MatchNumberLiteral`.
fn is_number_literal(text: &str) -> bool {
    let b = text.as_bytes();
    if b.is_empty() || !is_initial_number(b[0]) || text == "." {
        return false;
    }
    let mut scientific = false;
    let mut has_dot = b[0] == b'.';
    for &c in &b[1..] {
        if c == b'.' {
            if has_dot || scientific {
                return false;
            }
            has_dot = true;
        }
        if is_scientific(c) {
            if scientific {
                return false;
            }
            scientific = true;
        }
        if scientific && (c == b'+' || c == b'-') {
            continue;
        }
        if !is_number_char(c) {
            return false;
        }
    }
    true
}

/// `OperatorMatcher::HasSpecialPrecedence`: operators the grammar names explicitly.
fn has_special_precedence(op: &str) -> bool {
    matches!(
        op,
        "=" | "<"
            | ">"
            | "~"
            | "->"
            | "<="
            | ">="
            | "!="
            | "=="
            | "<>"
            | "~~"
            | "~*"
            | "!~"
            | "~~*"
            | "~~~"
            | "!~~"
            | "!~*"
            | "!~~*"
    )
}

/// The contents of a string literal token (`StringLiteralMatcher`), or `None` if the token is
/// not a complete string literal.
fn string_literal(token: &Token) -> Option<(String, Option<char>)> {
    let text = &token.text;
    let b = text.as_bytes();
    if b.first() == Some(&b'$') {
        let delimiter_len = text[1..].find('$')? + 2;
        if b.len() < 2 * delimiter_len || !text.ends_with(&text[..delimiter_len]) {
            return None;
        }
        return Some((text[delimiter_len..text.len() - delimiter_len].to_string(), None));
    }
    let (prefix, start) = match b {
        [b'\'', ..] => (None, 1),
        [p, b'\'', ..] if matches!(p, b'N' | b'n' | b'X' | b'x' | b'B' | b'b' | b'E' | b'e') => {
            (Some(p.to_ascii_uppercase() as char), 2)
        }
        _ => return None,
    };
    if b.len() < start + 1 || b[b.len() - 1] != b'\'' {
        return None;
    }
    Some((text[start..text.len() - 1].replace("''", "'"), prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_rules() {
        assert_eq!(identifier_text("\"a\"\"b\"", false, true).as_deref(), Some("a\"b"));
        assert_eq!(identifier_text("`x`", false, true).as_deref(), Some("x"));
        assert_eq!(identifier_text("'f.csv'", true, true).as_deref(), Some("f.csv"));
        assert_eq!(identifier_text("'f.csv'", false, true), None);
        assert_eq!(identifier_text("1abc", false, true), None);
        assert_eq!(identifier_text("x'41'", false, true), None);
        assert_eq!(identifier_text("tbl", false, true).as_deref(), Some("tbl"));
    }

    #[test]
    fn number_and_string_rules() {
        assert!(is_number_literal("1.5e-3") && is_number_literal(".5") && is_number_literal("1_000"));
        assert!(!is_number_literal(".") && !is_number_literal("1.2.3") && !is_number_literal("1e5e5"));
        let tok = |text: &str| Token {
            token_type: TokenType::StringLiteral,
            text: text.into(),
            offset: 0,
            preceded_by_newline: false,
            preceded_by_block_comment: false,
        };
        assert_eq!(string_literal(&tok("'it''s'")), Some(("it's".into(), None)));
        assert_eq!(string_literal(&tok("E'a\\nb'")), Some(("a\\nb".into(), Some('E'))));
        assert_eq!(string_literal(&tok("$tag$x$y$tag$")), Some(("x$y".into(), None)));
        assert_eq!(string_literal(&tok("$$$")), None);
    }
}
