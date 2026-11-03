// Ported from duckdb/src/parser/peg/peg_parser.cpp (grammar text parser) and
// compiled_grammar.cpp / matcher_factory.cpp (rule compilation and terminal overrides)
// @ rust-port-baseline

use crate::keywords::IdentifierKind;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// DuckDB's combined grammar (all `.gram` files plus keyword-list rules).
const INLINED_GRAMMAR: &str = include_str!("../grammar/inlined_grammar.gram");

/// A parsed grammar expression (`PEGExpression`).
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Literal(String),
    Reference(String),
    Call(String, Box<Expr>),
    Regex(String),
    Seq(Vec<Expr>),
    Choice(Vec<Expr>),
    Optional(Box<Expr>),
    Star(Box<Expr>),
    Plus(Box<Expr>),
}

#[derive(Debug)]
struct RawRule {
    parameter: Option<String>,
    expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
enum GrammarToken {
    Literal(String),
    Reference(String),
    Operator(u8),
    Call(String),
    Regex(String),
}

/// Parses a token list into an expression (`PEGExpressionParser`).
struct ExprParser<'a> {
    tokens: &'a [GrammarToken],
    pos: usize,
}

impl ExprParser<'_> {
    fn is_op(&self, op: u8) -> bool {
        matches!(self.tokens.get(self.pos), Some(GrammarToken::Operator(o)) if *o == op)
    }

    fn choice(&mut self) -> Result<Expr, String> {
        let first = self.sequence()?;
        if !self.is_op(b'/') {
            return Ok(first);
        }
        let mut alternatives = vec![first];
        while self.is_op(b'/') {
            self.pos += 1;
            alternatives.push(self.sequence()?);
        }
        Ok(Expr::Choice(alternatives))
    }

    fn sequence(&mut self) -> Result<Expr, String> {
        let mut items = Vec::new();
        while self.pos < self.tokens.len() && !self.is_op(b'/') && !self.is_op(b')') {
            items.push(self.postfix()?);
        }
        Ok(if items.len() == 1 {
            items.pop().expect("one item")
        } else {
            Expr::Seq(items)
        })
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let node = self.primary()?;
        for (op, wrap) in [
            (b'?', Expr::Optional as fn(Box<Expr>) -> Expr),
            (b'*', Expr::Star as fn(Box<Expr>) -> Expr),
            (b'+', Expr::Plus as fn(Box<Expr>) -> Expr),
        ] {
            if self.is_op(op) {
                self.pos += 1;
                return Ok(wrap(Box::new(node)));
            }
        }
        Ok(node)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        if self.is_op(b'(') {
            self.pos += 1;
            let inner = self.choice()?;
            if !self.is_op(b')') {
                return Err("expected closing ')'".into());
            }
            self.pos += 1;
            return Ok(match inner {
                Expr::Seq(_) => inner,
                other => Expr::Seq(vec![other]),
            });
        }
        if self.is_op(b'!') {
            // Negative lookahead is parsed but ignored, as in DuckDB.
            self.pos += 1;
            return self.primary();
        }
        let token = self.tokens.get(self.pos).ok_or("unexpected end of rule")?.clone();
        self.pos += 1;
        Ok(match token {
            GrammarToken::Literal(s) => Expr::Literal(s),
            GrammarToken::Reference(s) => Expr::Reference(s),
            GrammarToken::Regex(s) => Expr::Regex(s),
            GrammarToken::Call(name) => {
                let body = self.choice()?;
                if !self.is_op(b')') {
                    return Err(format!("expected closing ')' after call to {name}"));
                }
                self.pos += 1;
                Expr::Call(name, Box::new(body))
            }
            GrammarToken::Operator(op) => return Err(format!("unexpected operator '{}'", op as char)),
        })
    }
}

fn build(tokens: &[GrammarToken]) -> Result<Expr, String> {
    let mut parser = ExprParser { tokens, pos: 0 };
    let expr = parser.choice()?;
    if parser.pos != tokens.len() {
        return Err("unexpected trailing tokens".into());
    }
    Ok(expr)
}

fn is_peg_operator(c: u8) -> bool {
    matches!(c, b'/' | b'?' | b'(' | b')' | b'*' | b'+' | b'!')
}

/// Parses grammar text into rules (`PEGParser::ParseRules`). A rule ends at a newline once all
/// brackets are closed and the line does not end in `/`.
fn parse_rules(grammar: &str) -> Result<HashMap<String, RawRule>, String> {
    #[derive(PartialEq)]
    enum State {
        Name,
        Separator,
        Definition,
    }
    let g = grammar.as_bytes();
    let mut rules = HashMap::new();
    let mut state = State::Name;
    let mut name = String::new();
    let mut parameter: Option<String> = None;
    let mut tokens: Vec<GrammarToken> = Vec::new();
    let mut brackets = 0usize;
    let mut in_or = false;
    let mut c = 0;
    let mut finish =
        |name: &mut String, parameter: &mut Option<String>, tokens: &mut Vec<GrammarToken>| -> Result<(), String> {
            let expr = build(tokens).map_err(|e| format!("rule {name}: {e}"))?;
            if rules
                .insert(
                    std::mem::take(name),
                    RawRule {
                        parameter: parameter.take(),
                        expr,
                    },
                )
                .is_some()
            {
                return Err("duplicate rule".into());
            }
            tokens.clear();
            Ok(())
        };
    while c < g.len() {
        if g[c] == b'#' {
            while c < g.len() && g[c] != b'\n' && g[c] != b'\r' {
                c += 1;
            }
            continue;
        }
        if state == State::Definition
            && (g[c] == b'\n' || g[c] == b'\r')
            && brackets == 0
            && !in_or
            && !tokens.is_empty()
        {
            finish(&mut name, &mut parameter, &mut tokens)?;
            state = State::Name;
            c += 1;
            continue;
        }
        if crate::tokenizer::is_space(g[c]) {
            c += 1;
            continue;
        }
        match state {
            State::Name => {
                let start = c;
                if g[c] == b'%' {
                    c += 1;
                }
                while c < g.len() && g[c].is_ascii_alphanumeric() {
                    c += 1;
                }
                if c == start {
                    return Err(format!("expected a rule name at byte {c}"));
                }
                name = grammar[start..c].to_string();
                tokens.clear();
                parameter = None;
                state = State::Separator;
            }
            State::Separator => {
                if g[c] == b'(' {
                    c += 1;
                    let start = c;
                    while c < g.len() && g[c].is_ascii_alphanumeric() {
                        c += 1;
                    }
                    if g.get(c) != Some(&b')') {
                        return Err(format!("expected ')' after parameter of {name}"));
                    }
                    parameter = Some(grammar[start..c].to_string());
                    c += 1;
                } else if g[c..].starts_with(b"<-") {
                    c += 2;
                    state = State::Definition;
                } else {
                    return Err(format!("expected '<-' in rule {name}"));
                }
            }
            State::Definition => {
                in_or = false;
                if g[c] == b'\'' {
                    c += 1;
                    let start = c;
                    while c < g.len() && g[c] != b'\'' {
                        if g[c] == b'\\' {
                            c += 1;
                        }
                        c += 1;
                    }
                    tokens.push(GrammarToken::Literal(grammar[start..c].to_string()));
                    c += 1;
                } else if g[c].is_ascii_alphanumeric() {
                    let start = c;
                    while c < g.len() && g[c].is_ascii_alphanumeric() {
                        c += 1;
                    }
                    let word = grammar[start..c].to_string();
                    if g.get(c) == Some(&b'(') {
                        c += 1;
                        brackets += 1;
                        tokens.push(GrammarToken::Call(word));
                    } else {
                        tokens.push(GrammarToken::Reference(word));
                    }
                } else if g[c] == b'[' || g[c] == b'<' {
                    let start = c;
                    let close = if g[c] == b'[' { b']' } else { b'>' };
                    while c < g.len() && g[c] != close {
                        if g[c] == b'\\' {
                            c += 1;
                        }
                        c += 1;
                    }
                    c += 1;
                    tokens.push(GrammarToken::Regex(grammar[start..c.min(g.len())].to_string()));
                } else if is_peg_operator(g[c]) {
                    match g[c] {
                        b'(' => brackets += 1,
                        b')' => {
                            brackets = brackets
                                .checked_sub(1)
                                .ok_or(format!("unbalanced ')' in rule {name}"))?;
                        }
                        b'/' => in_or = true,
                        _ => {}
                    }
                    tokens.push(GrammarToken::Operator(g[c]));
                    c += 1;
                } else {
                    return Err(format!("unrecognized character '{}' in rule {name}", g[c] as char));
                }
            }
        }
    }
    if state == State::Definition {
        finish(&mut name, &mut parameter, &mut tokens)?;
    }
    Ok(rules)
}

/// A token-level matcher that replaces a grammar rule (the `GENERATED RULE OVERRIDES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// `IdentifierMatcher`: non-keywords plus the keywords `kind` allows. `paths` accepts a
    /// single-quoted string as a (table or file) name.
    Identifier {
        kind: IdentifierKind,
        paths: bool,
    },
    /// `ReservedIdentifierMatcher`: any identifier-shaped token, keywords included.
    ReservedIdentifier {
        paths: bool,
    },
    Number,
    String,
    /// `OperatorMatcher`; `all` also accepts operators with their own precedence level.
    Operator {
        all: bool,
    },
    EndOfInput,
}

fn terminal_override(name: &str) -> Option<Terminal> {
    use IdentifierKind::*;
    let ident = |kind| Some(Terminal::Identifier { kind, paths: false });
    match name {
        "Identifier" | "CatalogName" | "SchemaName" | "ColumnName" | "IndexName" | "SequenceName" | "PragmaName"
        | "SettingName" => ident(General),
        "TableName" => Some(Terminal::Identifier {
            kind: General,
            paths: true,
        }),
        "BareColLabel" => ident(BareLabel),
        "FunctionName" | "TableFunctionName" => ident(FunctionName),
        "TypeName" => ident(TypeName),
        "ReservedIdentifier"
        | "ReservedSchemaName"
        | "ReservedColumnName"
        | "ReservedIndexName"
        | "ReservedFunctionName"
        | "ReservedKeyword"
        | "ReservedTypeName"
        | "CopyOptionName" => Some(Terminal::ReservedIdentifier { paths: false }),
        "ReservedTableName" => Some(Terminal::ReservedIdentifier { paths: true }),
        "NumberLiteral" => Some(Terminal::Number),
        "StringLiteral" => Some(Terminal::String),
        "OperatorLiteral" => Some(Terminal::Operator { all: false }),
        "AnyOperatorLiteral" => Some(Terminal::Operator { all: true }),
        "EndOfInput" => Some(Terminal::EndOfInput),
        _ => None,
    }
}

pub type RuleId = usize;

/// A compiled grammar node, ready for matching.
#[derive(Debug, Clone)]
pub enum Node {
    /// A keyword or punctuation literal, matched case-insensitively against the token text.
    Literal(String),
    /// A choice made only of literals (e.g. the keyword lists), matched with one lookup.
    LiteralSet(Vec<String>, HashSet<String>),
    Rule(RuleId),
    Seq(Vec<Node>),
    Choice(Vec<Node>),
    Optional(Box<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Terminal(Terminal),
    /// A rule that cannot be compiled (e.g. a regex rule nothing reachable uses).
    Unsupported,
}

#[derive(Debug)]
pub struct Rule {
    pub name: String,
    pub node: Node,
}

/// The compiled grammar (`CompiledGrammar`).
#[derive(Debug)]
pub struct Grammar {
    pub rules: Vec<Rule>,
    pub by_name: HashMap<String, RuleId>,
}

impl Grammar {
    /// The built-in DuckDB grammar, compiled once.
    pub fn duckdb() -> &'static Grammar {
        static GRAMMAR: OnceLock<Grammar> = OnceLock::new();
        GRAMMAR.get_or_init(|| Grammar::compile(INLINED_GRAMMAR).expect("the built-in grammar is valid"))
    }

    pub fn rule(&self, name: &str) -> Option<RuleId> {
        self.by_name.get(name).copied()
    }

    /// Compiles grammar text: macros (`List(D)`, `Parens(D)`) are expanded inline, rules with
    /// terminal overrides become terminals, and regex rules must all be overridden.
    pub fn compile(text: &str) -> Result<Grammar, String> {
        let raw = parse_rules(text)?;
        let mut names: Vec<&String> = raw
            .keys()
            .filter(|n| raw[*n].parameter.is_none() && !n.starts_with('%'))
            .collect();
        names.sort();
        let by_name: HashMap<String, RuleId> = names.iter().enumerate().map(|(i, n)| ((*n).clone(), i)).collect();
        let mut rules = Vec::with_capacity(names.len());
        let mut failures = HashMap::new();
        for name in &names {
            let node = match terminal_override(name) {
                Some(t) => Node::Terminal(t),
                None => match compile_expr(&raw[*name].expr, &raw, &by_name, None) {
                    Ok(node) => node,
                    // Undefined references are always errors; other failures only matter if the
                    // rule is reachable (see `check_reachable`).
                    Err(e) if e.starts_with("undefined") => return Err(format!("rule {name}: {e}")),
                    Err(e) => {
                        failures.insert(by_name[name.as_str()], e);
                        Node::Unsupported
                    }
                },
            };
            rules.push(Rule {
                name: (*name).clone(),
                node,
            });
        }
        let grammar = Grammar { rules, by_name };
        grammar.check_reachable(&failures)?;
        Ok(grammar)
    }

    /// Every rule reachable from the start rules must have compiled.
    fn check_reachable(&self, failures: &HashMap<RuleId, String>) -> Result<(), String> {
        let mut seen = HashSet::new();
        let mut stack: Vec<RuleId> = ["Program", "Statement", "Start"]
            .iter()
            .filter_map(|n| self.rule(n))
            .collect();
        if stack.is_empty() {
            stack = (0..self.rules.len()).collect();
        }
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(e) = failures.get(&id) {
                return Err(format!("rule {}: {e}", self.rules[id].name));
            }
            collect_refs(&self.rules[id].node, &mut stack);
        }
        Ok(())
    }
}

fn collect_refs(node: &Node, out: &mut Vec<RuleId>) {
    match node {
        Node::Rule(id) => out.push(*id),
        Node::Seq(items) | Node::Choice(items) => items.iter().for_each(|n| collect_refs(n, out)),
        Node::Optional(n) | Node::Star(n) | Node::Plus(n) => collect_refs(n, out),
        Node::Literal(_) | Node::LiteralSet(..) | Node::Terminal(_) | Node::Unsupported => {}
    }
}

/// Compiles an expression; `argument` is the macro argument when compiling a macro body.
fn compile_expr(
    expr: &Expr,
    raw: &HashMap<String, RawRule>,
    by_name: &HashMap<String, RuleId>,
    argument: Option<(&str, &Node)>,
) -> Result<Node, String> {
    let rec = |e: &Expr| compile_expr(e, raw, by_name, argument);
    Ok(match expr {
        Expr::Literal(s) => Node::Literal(s.replace("\\'", "'").to_ascii_lowercase()),
        Expr::Reference(name) => {
            if let Some((param, node)) = argument
                && param == name
            {
                return Ok(node.clone());
            }
            if name == "EndOfInput" {
                return Ok(Node::Terminal(Terminal::EndOfInput));
            }
            Node::Rule(*by_name.get(name).ok_or(format!("undefined rule {name}"))?)
        }
        Expr::Call(name, arg) => {
            let r#macro = raw.get(name).ok_or(format!("undefined macro {name}"))?;
            let param = r#macro
                .parameter
                .as_deref()
                .ok_or(format!("{name} takes no parameter"))?;
            let arg_node = rec(arg)?;
            compile_expr(&r#macro.expr, raw, by_name, Some((param, &arg_node)))?
        }
        Expr::Regex(r) => return Err(format!("regex {r} outside an overridden rule")),
        Expr::Seq(items) => Node::Seq(items.iter().map(rec).collect::<Result<_, _>>()?),
        Expr::Choice(items) => {
            let compiled: Vec<Node> = items.iter().map(rec).collect::<Result<_, _>>()?;
            let literals: Option<Vec<String>> = compiled
                .iter()
                .map(|n| match n {
                    Node::Literal(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            match literals {
                // Ordered choice over plain literals is a set lookup: a token equals at most one.
                Some(list) if list.len() > 4 => {
                    let set = list.iter().cloned().collect();
                    Node::LiteralSet(list, set)
                }
                _ => Node::Choice(compiled),
            }
        }
        Expr::Optional(e) => Node::Optional(Box::new(rec(e)?)),
        Expr::Star(e) => Node::Star(Box::new(rec(e)?)),
        Expr::Plus(e) => Node::Plus(Box::new(rec(e)?)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rule_syntax() {
        let g = Grammar::compile(
            "Start <- 'SELECT' List(Item) ('FROM' Name)? EndOfInput\nItem <- Name / '*'\n\
             Name <- Identifier\nIdentifier <- <[a-z]+>\nList(D) <- D (',' D)* ','?\n",
        )
        .unwrap();
        let start = g.rule("Start").unwrap();
        let Node::Seq(items) = &g.rules[start].node else {
            panic!()
        };
        assert!(matches!(&items[0], Node::Literal(s) if s == "select"));
        // List(Item) expands to Item (',' Item)* ','?
        assert!(matches!(&items[1], Node::Seq(list) if list.len() == 3));
        assert!(matches!(g.rules[g.rule("Identifier").unwrap()].node, Node::Terminal(_)));
    }

    #[test]
    fn compiles_the_duckdb_grammar() {
        let g = Grammar::duckdb();
        assert!(g.rules.len() > 500, "{} rules", g.rules.len());
        assert!(g.rule("Program").is_some());
        assert!(matches!(
            g.rules[g.rule("ReservedKeyword").unwrap()].node,
            Node::Terminal(_)
        ));
        assert!(matches!(
            g.rules[g.rule("UnreservedKeyword").unwrap()].node,
            Node::LiteralSet(..)
        ));
    }

    #[test]
    fn rejects_bad_grammar() {
        assert!(Grammar::compile("A <- B\n").is_err());
        assert!(Grammar::compile("A <- 'x' (\n").is_err());
        assert!(Grammar::compile("Start <- [a-z]\n").is_err());
        // An unreachable regex rule is fine.
        assert!(Grammar::compile("Start <- 'x'\nUnused <- [a-z]\n").is_ok());
    }
}
