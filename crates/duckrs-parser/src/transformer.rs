// Ported from duckdb/src/parser/peg/transformer/{transform_expression,transform_select,transform_common,
// transform_create_view}.cpp and peg_transformer_factory.cpp @ rust-port-baseline
//
//! Turns parse trees into the AST.
//!
//! The C++ transformer has one function per grammar rule, with generated trampolines that unpack
//! each rule's sequence positionally. Here each rule is handled by name and its parts are found
//! by the names of the sub-rules it matched (`sub`, `subs`), which descends through sequences,
//! choices, optionals and repetitions but not into other rules. Statements are in `statements`;
//! those not yet ported report NotImplemented.

use crate::ast::*;
use crate::grammar::Grammar;
use crate::matcher::{ParseNode, TerminalValue};
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::{BTreeSet, HashMap};

mod statements;

fn parser_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Parser, message)
}

fn not_implemented(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::NotImplemented, message)
}

/// The regex function the `~` operators map to (`RegexMatchOperatorSemantics::PARTIAL`).
const REGEX_MATCH_FUNCTION: &str = "regexp_matches";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatePart {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Milliseconds,
    Microseconds,
    Week,
    Quarter,
    Decade,
    Century,
    Millennium,
}

impl DatePart {
    fn from_rule(name: &str) -> Option<Self> {
        Some(match name {
            "YearKeyword" => Self::Year,
            "MonthKeyword" => Self::Month,
            "DayKeyword" => Self::Day,
            "HourKeyword" => Self::Hour,
            "MinuteKeyword" => Self::Minute,
            "SecondKeyword" => Self::Second,
            "MillisecondKeyword" => Self::Milliseconds,
            "MicrosecondKeyword" => Self::Microseconds,
            "WeekKeyword" => Self::Week,
            "QuarterKeyword" => Self::Quarter,
            "DecadeKeyword" => Self::Decade,
            "CenturyKeyword" => Self::Century,
            "MillenniumKeyword" => Self::Millennium,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Year => "YEAR",
            Self::Month => "MONTH",
            Self::Day => "DAY",
            Self::Hour => "HOUR",
            Self::Minute => "MINUTE",
            Self::Second => "SECOND",
            Self::Milliseconds => "MILLISECONDS",
            Self::Microseconds => "MICROSECONDS",
            Self::Week => "WEEK",
            Self::Quarter => "QUARTER",
            Self::Decade => "DECADE",
            Self::Century => "CENTURY",
            Self::Millennium => "MILLENNIUM",
        }
    }
    /// `DateTruncSimplificationRule::DatePartToFunc`.
    fn interval_function(self) -> &'static str {
        match self {
            Self::Year => "to_years",
            Self::Month => "to_months",
            Self::Day => "to_days",
            Self::Decade => "to_decades",
            Self::Century => "to_centuries",
            Self::Millennium => "to_millennia",
            Self::Microseconds => "to_microseconds",
            Self::Milliseconds => "to_milliseconds",
            Self::Second => "to_seconds",
            Self::Minute => "to_minutes",
            Self::Hour => "to_hours",
            Self::Week => "to_weeks",
            Self::Quarter => "to_quarters",
        }
    }
    /// `GetIntervalTargetType`.
    fn interval_target(self) -> &'static str {
        match self {
            Self::Hour | Self::Minute | Self::Microseconds => "BIGINT",
            Self::Milliseconds | Self::Second => "DOUBLE",
            _ => "INTEGER",
        }
    }
}

/// A postfix applied to a base expression (`Indirection`).
enum Indirection {
    Cast(CastType),
    /// ARRAY_EXTRACT or ARRAY_SLICE bounds, the base not yet inserted.
    Slice(OperatorType, Vec<Expr>),
    Method(FunctionExpr),
    Field(String),
}

/// A tail of an IS expression (`IsExpressionTail`).
enum IsTail {
    Test(Expr),
    Distinct(ComparisonOp, Expr),
    Comparison(ComparisonOp, Expr),
    Other(ParsedOperator, Expr),
    Cast(CastType),
}

struct ParsedOperator {
    name: String,
    /// `Some(is_any)` for `op ANY` / `op ALL`.
    any_all: Option<bool>,
}

#[derive(Default)]
struct FunctionArguments {
    distinct: bool,
    args: Vec<FunctionArgument>,
    order_bys: Vec<OrderByNode>,
    has_ignore_nulls: bool,
    ignore_nulls: bool,
}

enum GroupByItem {
    Empty,
    Expression(Expr),
    Cube(Vec<Expr>),
    Rollup(Vec<Expr>),
    GroupingSets(Vec<GroupByItem>),
}

struct LimitValue {
    expr: Option<Expr>,
    percent: bool,
}

const MAX_GROUPING_SETS: usize = 65535;

/// The default `max_expression_depth` (`ParserOptions::max_expression_depth`).
pub const DEFAULT_MAX_EXPRESSION_DEPTH: usize = 1000;

/// Rules whose transform nests one level deeper (`ExpressionDepthKind::NESTING`).
const NESTING_RULES: &[&str] = &["SingleExpression", "CustomPrefixExpression"];

/// Rules whose operator chains add one level per link (`ExpressionDepthKind::CHAIN`).
const CHAIN_RULES: &[&str] = &[
    "LambdaArrowExpression",
    "IsDistinctFromExpression",
    "ComparisonExpression",
    "BetweenInLikeExpression",
    "InfixOtherOperatorExpression",
    "BitwiseExpression",
    "TildeExpression",
    "AdditiveExpression",
    "MultiplicativeExpression",
    "ExponentiationExpression",
    "CollateExpression",
    "AtTimeZoneExpression",
    "PrefixExpression",
];

/// `CountChainLinks`: the repetitions in a rule's sequence.
fn chain_links(node: &ParseNode) -> usize {
    match node {
        ParseNode::Repeat(items) => items.len(),
        ParseNode::Optional(Some(inner)) => chain_links(inner),
        ParseNode::List(items) => items
            .iter()
            .filter(|i| matches!(i, ParseNode::Repeat(_) | ParseNode::Optional(_)))
            .map(chain_links)
            .sum(),
        ParseNode::Rule { child, .. } => chain_links(child),
        _ => 0,
    }
}

pub struct Transformer<'g> {
    grammar: &'g Grammar,
    window_clauses: Vec<HashMap<String, WindowExpr>>,
    in_window_definition: bool,
    param_count: u64,
    named_params: HashMap<String, u64>,
    /// whether the last parameter was a named one (`last_param_type`)
    param_named: Option<bool>,
    pivot_entries: Vec<statements::PivotEntry>,
    /// The WITH clauses in scope, outermost first.
    cte_stack: Vec<CteMap>,
    /// `max_expression_depth`: how deep expression trees may nest.
    pub max_expression_depth: usize,
    /// `ParserOptions::integer_division`
    pub integer_division: bool,
    /// `regex_match_operator_semantics = full`: the regex operators match the whole string
    pub regex_full_match: bool,
    /// An upper bound of the depth of the expression being transformed (`expression_depth`).
    expression_depth: usize,
    /// Where the stack was when the statement's transform started; deep trees stop before
    /// using more than `DEFAULT_STACK_BUDGET` of it.
    stack_base: usize,
}

impl<'g> Transformer<'g> {
    pub fn new(grammar: &'g Grammar) -> Self {
        Self {
            grammar,
            window_clauses: Vec::new(),
            in_window_definition: false,
            param_count: 0,
            named_params: HashMap::new(),
            param_named: None,
            pivot_entries: Vec::new(),
            cte_stack: Vec::new(),
            max_expression_depth: DEFAULT_MAX_EXPRESSION_DEPTH,
            integer_division: false,
            regex_full_match: false,
            expression_depth: 0,
            stack_base: crate::matcher::stack_position(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Parse tree navigation
    // -----------------------------------------------------------------------------------------

    fn name<'n>(&self, node: &'n ParseNode) -> &'g str {
        match node {
            ParseNode::Rule { rule, .. } => &self.grammar.rules[*rule].name,
            _ => "",
        }
    }

    /// The rule results and terminals directly inside `node`, in order.
    fn collect<'n>(node: &'n ParseNode, out: &mut Vec<&'n ParseNode>) {
        match node {
            ParseNode::Terminal { .. } | ParseNode::Rule { .. } => out.push(node),
            ParseNode::List(items) | ParseNode::Repeat(items) => {
                for item in items {
                    Self::collect(item, out);
                }
            }
            ParseNode::Choice { child, .. } => Self::collect(child, out),
            ParseNode::Optional(child) => {
                if let Some(child) = child {
                    Self::collect(child, out);
                }
            }
        }
    }

    /// The parts of a rule's result: its sub-rule results and literal terminals.
    fn parts(node: &ParseNode) -> Vec<&ParseNode> {
        let mut out = Vec::new();
        match node {
            ParseNode::Rule { child, .. } => Self::collect(child, &mut out),
            _ => Self::collect(node, &mut out),
        }
        out
    }

    fn sub<'n>(&self, node: &'n ParseNode, name: &str) -> Option<&'n ParseNode> {
        Self::parts(node).into_iter().find(|p| self.name(p) == name)
    }

    fn subs<'n>(&self, node: &'n ParseNode, name: &str) -> Vec<&'n ParseNode> {
        Self::parts(node).into_iter().filter(|p| self.name(p) == name).collect()
    }

    fn req<'n>(&self, node: &'n ParseNode, name: &str) -> Result<&'n ParseNode> {
        self.sub(node, name).ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("{} is missing its {name}", self.name(node)),
            )
        })
    }

    /// The single sub-rule of a choice rule.
    fn alt<'n>(&self, node: &'n ParseNode) -> Result<&'n ParseNode> {
        Self::parts(node)
            .into_iter()
            .find(|p| matches!(p, ParseNode::Rule { .. }))
            .ok_or_else(|| Error::new(ErrorKind::Internal, format!("{} has no sub-rule", self.name(node))))
    }

    /// Whether a literal keyword or symbol appears directly in the rule.
    fn has(&self, node: &ParseNode, literal: &str) -> bool {
        Self::parts(node).into_iter().any(|p| {
            matches!(p, ParseNode::Terminal { value: TerminalValue::Literal(t), .. } if t.eq_ignore_ascii_case(literal))
        })
    }

    /// The first terminal anywhere under `node`.
    fn terminal(node: &ParseNode) -> Option<&TerminalValue> {
        match node {
            ParseNode::Terminal { value, .. } => Some(value),
            ParseNode::Rule { child, .. } => Self::terminal(child),
            ParseNode::List(items) | ParseNode::Repeat(items) => items.iter().find_map(Self::terminal),
            ParseNode::Choice { child, .. } => Self::terminal(child),
            ParseNode::Optional(child) => child.as_deref().and_then(Self::terminal),
        }
    }

    /// The text of the first terminal under `node`: an identifier, keyword, number or string.
    fn text(node: &ParseNode) -> String {
        match Self::terminal(node) {
            Some(TerminalValue::Literal(t) | TerminalValue::Identifier(t) | TerminalValue::Number(t))
            | Some(TerminalValue::Operator(t)) => t.clone(),
            Some(TerminalValue::String { value, .. }) => value.clone(),
            _ => String::new(),
        }
    }

    fn texts(&self, node: &ParseNode, name: &str) -> Vec<String> {
        self.subs(node, name).into_iter().map(Self::text).collect()
    }

    /// Texts of every sub-rule (each a name component).
    fn rule_texts(&self, node: &ParseNode) -> Vec<String> {
        Self::parts(node)
            .into_iter()
            .filter(|p| matches!(p, ParseNode::Rule { .. }))
            .map(Self::text)
            .collect()
    }

    fn opt_expr(&mut self, node: &ParseNode, name: &str) -> Result<Option<Expr>> {
        match self.sub(node, name) {
            Some(n) => Ok(Some(self.expr(n)?)),
            None => Ok(None),
        }
    }

    fn exprs(&mut self, node: &ParseNode, name: &str) -> Result<Vec<Expr>> {
        self.subs(node, name).into_iter().map(|n| self.expr(n)).collect()
    }

    // -----------------------------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------------------------

    /// Transforms a `Statement` parse tree.
    pub fn statement(&mut self, node: &ParseNode) -> Result<Statement> {
        self.statement_kind(self.alt(node)?)
    }

    // -----------------------------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------------------------

    pub fn expr(&mut self, node: &ParseNode) -> Result<Expr> {
        let name = self.name(node);
        let weight = if NESTING_RULES.contains(&name) {
            1
        } else if CHAIN_RULES.contains(&name) {
            let links = match node {
                ParseNode::Rule { child, .. } => chain_links(child),
                _ => 0,
            };
            self.stack_check(links)?;
            links
        } else {
            0
        };
        self.enter_expression(weight)?;
        let result = self.expr_inner(node);
        self.expression_depth -= weight;
        result
    }

    /// `EnterExpression`: reject expression trees deeper than `max_expression_depth`.
    fn enter_expression(&mut self, weight: usize) -> Result<()> {
        let stack_used = self.stack_base.abs_diff(crate::matcher::stack_position());
        if self.expression_depth + weight > self.max_expression_depth
            || stack_used > crate::matcher::DEFAULT_STACK_BUDGET
        {
            return Err(self.depth_error());
        }
        self.expression_depth += weight;
        Ok(())
    }

    /// `StackCheck`: a single operator chain may not reach the limit either.
    fn stack_check(&self, links: usize) -> Result<()> {
        if links >= self.max_expression_depth {
            return Err(self.depth_error());
        }
        Ok(())
    }

    fn depth_error(&self) -> Error {
        parser_error(format!(
            "Max expression depth limit of {} exceeded. Use \"SET max_expression_depth TO x\" to increase the \
             maximum expression depth.",
            self.max_expression_depth
        ))
    }

    /// `SubqueryReference <- Parens(SelectStatementInternal)`, one nesting level deeper.
    pub(crate) fn subquery_reference(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        self.enter_expression(1)?;
        let result = self.select_internal(self.req(node, "SelectStatementInternal")?);
        self.expression_depth -= 1;
        result
    }

    fn expr_inner(&mut self, node: &ParseNode) -> Result<Expr> {
        let name = self.name(node);
        match name {
            "Expression"
            | "SingleExpression"
            | "UnaryExpression"
            | "OtherOperatorExpression"
            | "SpecialFunctionExpression"
            | "ParensExpression"
            | "SingleArrowPair"
            | "LogicalOrExpressionTail"
            | "LogicalAndExpressionTail"
            | "ColDefOrExpressionTail"
            | "ColDefAndExpressionTail"
            | "CaseElse"
            | "ColumnDefaultExpr"
            | "FromExpression"
            | "ForExpression"
            | "EscapeClause"
            | "WhereClause"
            | "HavingClause"
            | "QualifyClause"
            | "ListComprehensionFilter"
            | "PositionalFunctionArgument"
            | "GroupByBaseExpression"
            | "FetchValue"
            | "PivotHeader"
            | "CaseExpression"
            | "ListExpression"
            | "SimpleCaseWhenEquals" => {
                let inner = Self::parts(node)
                    .into_iter()
                    .find(|p| matches!(p, ParseNode::Rule { .. }))
                    .ok_or_else(|| Error::new(ErrorKind::Internal, format!("{name} has no expression")))?;
                self.expr(inner)
            }
            "LambdaArrowExpression" => {
                let mut expr = self.expr(self.req(node, "LogicalOrExpression")?)?;
                for pair in self.subs(node, "SingleArrowPair") {
                    let right = self.expr(pair)?;
                    expr = ExprKind::Lambda {
                        lhs: Box::new(expr),
                        expr: Box::new(right),
                        syntax: LambdaSyntax::Arrow,
                    }
                    .into();
                }
                Ok(expr)
            }
            "LogicalOrExpression" => self.conjunction(node, "LogicalAndExpression", "LogicalOrExpressionTail", false),
            "LogicalAndExpression" => self.conjunction(node, "LogicalNotExpression", "LogicalAndExpressionTail", true),
            "ColDefOrExpr" => self.conjunction(node, "ColDefAndExpr", "ColDefOrExpressionTail", false),
            "ColDefAndExpr" => self.conjunction(node, "IsDistinctFromExpression", "ColDefAndExpressionTail", true),
            "LogicalNotExpression" => {
                let expr = self.expr(self.req(node, "IsExpression")?)?;
                let nots = self
                    .sub(node, "NotExpression")
                    .map_or(0, |n| self.subs(n, "NotKeyword").len());
                self.check_not_chain(nots)?;
                Ok(apply_nots(nots, expr))
            }
            "NotComparisonOperand" => {
                let expr = self.expr(self.req(node, "IsExpression")?)?;
                let nots = self.subs(self.req(node, "NotExpression")?, "NotKeyword").len();
                self.check_not_chain(nots)?;
                Ok(apply_nots(nots, expr))
            }
            "IsExpression" => self.is_expression(node),
            "IsDistinctFromExpression" => {
                let mut expr = self.expr(self.req(node, "ComparisonExpression")?)?;
                let tails = self.subs(node, "IsDistinctFromTail");
                if tails.len() > 1 {
                    return Err(chained_comparison());
                }
                for tail in tails {
                    let (op, right) = self.is_distinct_tail(tail)?;
                    expr = Expr::comparison(op, expr, right);
                }
                Ok(expr)
            }
            "ComparisonExpression" => {
                let mut expr = self.expr(self.req(node, "BetweenInLikeExpression")?)?;
                let tails = self.subs(node, "ComparisonExpressionTail");
                if tails.len() > 1 {
                    return Err(chained_comparison());
                }
                for tail in tails {
                    let (op, right) = self.comparison_tail(tail)?;
                    expr = Expr::comparison(op, expr, right);
                }
                Ok(expr)
            }
            "ComparisonOperand" => self.expr(self.alt(node)?),
            "BetweenInLikeExpression" => self.between_in_like(node),
            "InfixOtherOperatorExpression" => {
                let expr = self.expr(self.req(node, "BitwiseExpression")?)?;
                let mut tails = Vec::new();
                for tail in self.subs(node, "OtherOperatorTail") {
                    tails.push(self.other_operator_tail(tail)?);
                }
                apply_other_operators(expr, tails, self.regex_full_match)
            }
            "BitwiseExpression" => self.binary_chain(node, "TildeExpression", "BitwiseExpressionTail", "BitOperator"),
            "AdditiveExpression" => {
                self.binary_chain(node, "MultiplicativeExpression", "AdditiveExpressionTail", "Term")
            }
            "MultiplicativeExpression" => self.binary_chain(
                node,
                "ExponentiationExpression",
                "MultiplicativeExpressionTail",
                "Factor",
            ),
            "ExponentiationExpression" => self.binary_chain(
                node,
                "CollateExpression",
                "ExponentiationExpressionTail",
                "ExponentOperator",
            ),
            "TildeExpression" => {
                let mut expr = self.expr(self.req(node, "AdditiveExpression")?)?;
                for _ in self.subs(node, "TildePrefixOperator") {
                    expr = operator_function("~", vec![expr])?;
                }
                Ok(expr)
            }
            "CollateExpression" => {
                let mut expr = self.expr(self.req(node, "AtTimeZoneExpression")?)?;
                for tail in self.subs(node, "CollateExpressionTail") {
                    let collation = match self.expr(self.req(tail, "AtTimeZoneExpression")?)?.kind {
                        ExprKind::Constant(literal) => literal.text,
                        ExprKind::ColumnRef(names) => names.join("."),
                        _ => return Err(not_implemented("Unexpected expression encountered for collate")),
                    };
                    expr = ExprKind::Collate {
                        child: Box::new(expr),
                        collation,
                    }
                    .into();
                }
                Ok(expr)
            }
            "AtTimeZoneExpression" => {
                let mut expr = self.expr(self.req(node, "UnaryExpression")?)?;
                for tail in self.subs(node, "AtTimeZoneExpressionTail") {
                    let zone = self.expr(self.req(tail, "UnaryExpression")?)?;
                    expr = FunctionExpr::new("timezone", vec![zone, expr]).into();
                }
                Ok(expr)
            }
            "PrefixExpression" => {
                let mut expr = self.expr(self.req(node, "BaseExpression")?)?;
                let prefixes: Vec<String> = self
                    .subs(node, "PrefixOperator")
                    .into_iter()
                    .map(|p| self.prefix_operator(p))
                    .collect::<Result<_>>()?;
                for prefix in prefixes.iter().rev() {
                    if prefix == "-"
                        && let ExprKind::Constant(literal) = &expr.kind
                        && literal.is_numeric()
                    {
                        expr = Expr::constant(literal.negate());
                        continue;
                    }
                    expr = FunctionExpr::operator(prefix.clone(), vec![expr]).into();
                }
                Ok(expr)
            }
            "CustomPrefixExpression" => {
                let op = Self::text(self.req(node, "OperatorLiteral")?);
                let expr = self.expr(self.req(node, "TildeExpression")?)?;
                operator_function(&op, vec![expr])
            }
            "BaseExpression" => self.base_expression(node),
            "LiteralExpression" => self.literal_expression(self.alt(node)?),
            "StringLiteral" => string_literal_expr(node),
            "NumberLiteral" => Ok(Expr::constant(Literal::number(Self::text(node)))),
            "Parameter" => self.parameter(self.alt(node)?),
            "PositionalExpression" => {
                let literal = Literal::number(Self::text(self.req(node, "NumberLiteral")?));
                match literal.try_get_i64() {
                    Some(index) if index > 0 => Ok(ExprKind::Positional(index as u64).into()),
                    _ => Err(parser_error("Positional reference node needs to be >= 1")),
                }
            }
            "DefaultExpression" => Ok(ExprKind::Default.into()),
            "ColumnReference" => Ok(Expr::column(self.rule_texts(self.alt(node)?))),
            "SubqueryExpression" => {
                let statement = self.subquery_reference(self.req(node, "SubqueryReference")?)?;
                let subquery_type = if self.sub(node, "SubqueryExists").is_some() {
                    SubqueryType::Exists
                } else {
                    SubqueryType::Scalar
                };
                let expr = subquery(statement, subquery_type, None, ComparisonOp::Equal);
                Ok(if self.sub(node, "SubqueryNot").is_some() {
                    Expr::not_op(expr)
                } else {
                    expr
                })
            }
            "ParenthesisExpression" => Ok(FunctionExpr::new("row", self.exprs(node, "Expression")?).into()),
            "RowExpression" => Ok(FunctionExpr::new("row", self.exprs(node, "Expression")?).into()),
            "CoalesceExpression" => Ok(Expr::operator(OperatorType::Coalesce, self.exprs(node, "Expression")?)),
            "UnpackExpression" => Ok(Expr::operator(OperatorType::Unpack, self.exprs(node, "Expression")?)),
            "TryExpression" => Ok(Expr::operator(OperatorType::Try, self.exprs(node, "Expression")?)),
            "ColumnsExpression" => self.columns_expression(node),
            "ExtractExpression" => {
                let args = self.req(node, "ExtractArguments")?;
                let part = self.extract_argument(self.alt(self.req(args, "ExtractArgument")?)?)?;
                let expr = self.expr(self.req(args, "Expression")?)?;
                Ok(FunctionExpr::new("date_part", vec![part, expr]).into())
            }
            "LambdaExpression" => {
                let params = self.texts(node, "ColIdOrString");
                let body = self.expr(self.req(node, "Expression")?)?;
                Ok(Expr::lambda(&params, body))
            }
            "NullIfExpression" => {
                let args = self.exprs(self.req(node, "NullIfArguments")?, "Expression")?;
                Ok(FunctionExpr::new("nullif", args).into())
            }
            "PositionExpression" => {
                let args = self.req(node, "PositionArguments")?;
                let needle = self.expr(self.req(args, "OtherOperatorExpression")?)?;
                let haystack = self.expr(self.req(args, "Expression")?)?;
                Ok(FunctionExpr::new("position", vec![haystack, needle]).into())
            }
            "SubstringExpression" => {
                let args = self.alt(self.req(node, "SubstringArguments")?)?;
                let mut exprs = Vec::new();
                if self.name(args) == "SubstringParameters" {
                    exprs.push(self.expr(self.req(args, "Expression")?)?);
                    let from_for = self.alt(self.req(args, "SubstringFromFor")?)?;
                    if self.name(from_for) == "SubstringFor" {
                        exprs.push(Expr::constant(Literal::integer(1)));
                    } else {
                        exprs.push(self.expr(self.req(from_for, "FromExpression")?)?);
                    }
                    if let Some(for_expr) = self.opt_expr(from_for, "ForExpression")? {
                        exprs.push(for_expr);
                    }
                } else {
                    exprs = self.exprs(args, "Expression")?;
                }
                Ok(FunctionExpr::new("substring", exprs).into())
            }
            "TrimExpression" => {
                let args = self.req(node, "TrimArguments")?;
                let function = match self.sub(args, "TrimDirection") {
                    Some(d) => match self.name(self.alt(d)?) {
                        "TrimLeading" => "ltrim",
                        "TrimTrailing" => "rtrim",
                        _ => "trim",
                    },
                    None => "trim",
                };
                let mut exprs = self.exprs(args, "Expression")?;
                if let Some(source) = self.sub(args, "TrimSource")
                    && let Some(e) = self.opt_expr(source, "Expression")?
                {
                    exprs.push(e);
                }
                Ok(FunctionExpr::new(function, exprs).into())
            }
            "OverlayExpression" => {
                let args = self.alt(self.req(node, "OverlayArguments")?)?;
                let mut exprs = self.exprs(args, "Expression")?;
                if let Some(from) = self.opt_expr(args, "FromExpression")? {
                    exprs.push(from);
                }
                if let Some(for_expr) = self.opt_expr(args, "ForExpression")? {
                    exprs.push(for_expr);
                }
                Ok(FunctionExpr::new("overlay", exprs).into())
            }
            "IntervalLiteral" => self.interval_literal(node),
            "TypeLiteral" => {
                let target = self.type_(self.req(node, "Type")?)?;
                let value = Self::text(self.req(node, "StringLiteral")?);
                Ok(Expr::cast(Expr::constant(Literal::string(value)), target))
            }
            "SimpleCaseExpression" => self.simple_case(node),
            "SearchedCaseExpression" => {
                let mut checks = Vec::new();
                for when_then in self.subs(node, "CaseWhenThen") {
                    let mut e = self.exprs(when_then, "Expression")?;
                    let then = e.pop().expect("THEN expression");
                    checks.push((e.pop().expect("WHEN expression"), then));
                }
                Ok(case(checks, self.opt_expr(node, "CaseElse")?))
            }
            "StarExpression" => self.star_expression(node),
            "CastExpression" => {
                let try_cast = self.name(self.alt(self.req(node, "CastOrTryCast")?)?) == "TryCastKeyword";
                let args = self.req(node, "CastArguments")?;
                let child = self.expr(self.req(args, "Expression")?)?;
                let target = self.type_(self.req(args, "Type")?)?;
                Ok(ExprKind::Cast {
                    child: Box::new(child),
                    target,
                    try_cast,
                }
                .into())
            }
            "GroupingExpression" => Ok(Expr::operator(OperatorType::Grouping, self.exprs(node, "Expression")?)),
            "MapExpression" => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for field in self.subs(self.req(node, "MapStructExpression")?, "MapStructField") {
                    let mut kv = self.exprs(field, "Expression")?;
                    values.push(kv.pop().expect("map value"));
                    keys.push(kv.pop().expect("map key"));
                }
                Ok(FunctionExpr::new(
                    "map",
                    vec![
                        FunctionExpr::new("list_value", keys).into(),
                        FunctionExpr::new("list_value", values).into(),
                    ],
                )
                .into())
            }
            "FunctionExpression" => self.function_expression(node),
            "ListComprehensionExpression" => self.list_comprehension(node),
            "ArrayBoundedListExpression" => {
                let items = self.exprs(self.req(node, "BoundedListExpression")?, "Expression")?;
                if self.has(node, "ARRAY") {
                    Ok(Expr::operator(OperatorType::ArrayConstructor, items))
                } else {
                    Ok(FunctionExpr::new("list_value", items).into())
                }
            }
            "ArrayParensSelect" => self.array_parens_select(node),
            "StructExpression" => {
                let mut fields = Vec::new();
                for field in self.subs(node, "StructField") {
                    let name = Self::text(self.req(field, "ColIdOrString")?);
                    let expr = self.expr(self.req(field, "Expression")?)?.with_alias(name.clone());
                    fields.push(FunctionArgument { name, expr });
                }
                let mut f = FunctionExpr::new("struct_pack", Vec::new());
                f.args = fields;
                Ok(f.into())
            }
            "MeasureSemanticsExpression" => {
                let marker = if self.name(self.alt(self.req(node, "MeasureSemantics")?)?) == "FinalSemantics" {
                    "__mr_final"
                } else {
                    "__mr_running"
                };
                let function = self.expr(self.req(node, "FunctionExpression")?)?;
                Ok(FunctionExpr::new(marker, vec![function]).into())
            }
            other => Err(Error::new(
                ErrorKind::Internal,
                format!("no expression transformer for rule {other}"),
            )),
        }
    }

    fn conjunction(&mut self, node: &ParseNode, first: &str, tail: &str, and: bool) -> Result<Expr> {
        let expr = self.expr(self.req(node, first)?)?;
        let tails = self.subs(node, tail);
        if tails.is_empty() {
            return Ok(expr);
        }
        let mut children = vec![expr];
        for t in tails {
            children.push(self.expr(t)?);
        }
        Ok(ExprKind::Conjunction { and, children }.into())
    }

    fn binary_chain(&mut self, node: &ParseNode, first: &str, tail: &str, op_rule: &str) -> Result<Expr> {
        let mut expr = self.expr(self.req(node, first)?)?;
        let operand = match tail {
            "BitwiseExpressionTail" => "TildeExpression",
            "AdditiveExpressionTail" => "MultiplicativeExpression",
            "MultiplicativeExpressionTail" => "ExponentiationExpression",
            _ => "CollateExpression",
        };
        for t in self.subs(node, tail) {
            let mut op = Self::text(self.req(t, op_rule)?);
            // `TransformMultiplicativeExpression`: with `integer_division`, `/` is `//`
            if self.integer_division && op == "/" {
                op = "//".to_string();
            }
            let right = self.expr(self.req(t, operand)?)?;
            expr = FunctionExpr::operator(op, vec![expr, right]).into();
        }
        Ok(expr)
    }

    fn comparison_operator(&self, node: &ParseNode) -> Result<ComparisonOp> {
        Ok(match self.name(self.alt(node)?) {
            "OperatorEqual" => ComparisonOp::Equal,
            "OperatorNotEqual" => ComparisonOp::NotEqual,
            "OperatorLessThan" => ComparisonOp::LessThan,
            "OperatorGreaterThan" => ComparisonOp::GreaterThan,
            "OperatorLessThanEquals" => ComparisonOp::LessThanOrEqual,
            _ => ComparisonOp::GreaterThanOrEqual,
        })
    }

    fn comparison_tail(&mut self, node: &ParseNode) -> Result<(ComparisonOp, Expr)> {
        let op = self.comparison_operator(self.req(node, "ComparisonOperator")?)?;
        let right = self.expr(self.req(node, "ComparisonOperand")?)?;
        Ok((op, right))
    }

    fn is_distinct_tail(&mut self, node: &ParseNode) -> Result<(ComparisonOp, Expr)> {
        let op = if self.has(self.req(node, "IsDistinctFromOp")?, "NOT") {
            ComparisonOp::NotDistinctFrom
        } else {
            ComparisonOp::DistinctFrom
        };
        Ok((op, self.expr(self.req(node, "ComparisonExpression")?)?))
    }

    /// An IS test without its subject: an IS [NOT] NULL operator or an IS [NOT] DISTINCT FROM
    /// comparison against a boolean.
    fn is_test(&self, node: &ParseNode) -> Result<Expr> {
        let test = self.alt(node)?;
        Ok(match self.name(test) {
            "IsLiteral" => {
                let negated = self.has(test, "NOT");
                let value = self.name(self.alt(self.req(test, "IsLiteralValue")?)?);
                match value {
                    "TrueLiteral" | "FalseLiteral" => {
                        let op = if negated {
                            ComparisonOp::DistinctFrom
                        } else {
                            ComparisonOp::NotDistinctFrom
                        };
                        let constant = Expr::constant(Literal::boolean(value == "TrueLiteral"));
                        Expr::comparison(op, Expr::constant(Literal::null()), constant)
                    }
                    _ => Expr::operator(
                        if negated {
                            OperatorType::IsNotNull
                        } else {
                            OperatorType::IsNull
                        },
                        Vec::new(),
                    ),
                }
            }
            "NotNull" => Expr::operator(OperatorType::IsNotNull, Vec::new()),
            _ => Expr::operator(OperatorType::IsNull, Vec::new()),
        })
    }

    /// `NotExpression` is a chain of NOTs.
    fn check_not_chain(&mut self, nots: usize) -> Result<()> {
        self.stack_check(nots)?;
        self.enter_expression(nots)?;
        self.expression_depth -= nots;
        Ok(())
    }

    fn is_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        let links = self
            .sub(node, "IsExpressionContinuation")
            .map_or(0, |c| self.subs(c, "IsExpressionTail").len());
        self.enter_expression(links)?;
        let result = self.is_expression_inner(node);
        self.expression_depth -= links;
        result
    }

    fn is_expression_inner(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut expr = self.expr(self.req(node, "IsDistinctFromExpression")?)?;
        let Some(continuation) = self.sub(node, "IsExpressionContinuation") else {
            return Ok(expr);
        };
        let mut tails = vec![IsTail::Test(self.is_test(self.req(continuation, "IsTest")?)?)];
        for tail in self.subs(continuation, "IsExpressionTail") {
            let inner = self.alt(tail)?;
            let part = self.alt(inner)?;
            tails.push(match self.name(inner) {
                "IsTestTail" => IsTail::Test(self.is_test(part)?),
                "IsDistinctTail" => {
                    let (op, e) = self.is_distinct_tail(part)?;
                    IsTail::Distinct(op, e)
                }
                "IsComparisonTail" => {
                    let (op, e) = self.comparison_tail(part)?;
                    IsTail::Comparison(op, e)
                }
                "IsOtherOperatorTail" => {
                    let (op, e) = self.other_operator_tail(part)?;
                    IsTail::Other(op, e)
                }
                _ => IsTail::Cast(self.type_(self.req(part, "Type")?)?),
            });
        }
        let mut previous = 0;
        for tail in tails {
            let kind = match &tail {
                IsTail::Test(_) => 0,
                IsTail::Distinct(..) => 1,
                IsTail::Comparison(..) => 2,
                IsTail::Other(..) => 3,
                IsTail::Cast(_) => 4,
            };
            if kind == previous && (kind == 1 || kind == 2) {
                return Err(chained_comparison());
            }
            expr = match tail {
                IsTail::Test(test) => apply_is_test(expr, test),
                IsTail::Distinct(op, right) | IsTail::Comparison(op, right) => Expr::comparison(op, expr, right),
                IsTail::Other(op, right) => apply_other_operators(expr, vec![(op, right)], self.regex_full_match)?,
                IsTail::Cast(target) => Expr::cast(expr, target),
            };
            previous = kind;
        }
        Ok(expr)
    }

    fn between_in_like(&mut self, node: &ParseNode) -> Result<Expr> {
        // `TransformBetweenInLikeExpression`: each IN and each cast after it is a level
        let predicates: usize = self
            .subs(node, "InPredicate")
            .iter()
            .map(|predicate| 1 + self.subs(predicate, "CastOperator").len())
            .sum::<usize>()
            + usize::from(self.sub(node, "BetweenLikeOp").is_some());
        self.stack_check(predicates)?;
        let mut expr = self.expr(self.req(node, "OtherOperatorExpression")?)?;
        for predicate in self.subs(node, "InPredicate") {
            let negated = self.has(predicate, "NOT");
            let in_expr = self.in_expression(self.alt(self.req(self.req(predicate, "InClause")?, "InExpression")?)?)?;
            expr = apply_between_in_like(expr, in_expr, negated);
            for cast in self.subs(predicate, "CastOperator") {
                expr = Expr::cast(expr, self.type_(self.req(cast, "Type")?)?);
            }
        }
        if let Some(op) = self.sub(node, "BetweenLikeOp") {
            let negated = self.has(op, "NOT");
            let inner = self.alt(self.req(op, "BetweenLikeOpExpression")?)?;
            let pending = if self.name(inner) == "BetweenClause" {
                let mut bounds = self.exprs(inner, "OtherOperatorExpression")?;
                let upper = bounds.pop().expect("BETWEEN upper bound");
                let lower = bounds.pop().expect("BETWEEN lower bound");
                ExprKind::Between {
                    input: Box::new(Expr::constant(Literal::null())),
                    lower: Box::new(lower),
                    upper: Box::new(upper),
                }
                .into()
            } else {
                self.like_clause(inner)?
            };
            if negated
                && let ExprKind::Function(f) = &pending.kind
                && f.name.name.starts_with('!')
            {
                return Err(parser_error(
                    "NOT cannot precede an operator that is already negated (!~~, !~~*, !~ or !~*)",
                ));
            }
            expr = apply_between_in_like(expr, pending, negated);
        }
        Ok(expr)
    }

    fn in_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        match self.name(node) {
            "InExpressionList" => {
                let mut children = self.exprs(node, "Expression")?;
                if children.len() == 1
                    && let ExprKind::Subquery { .. } = &children[0].kind
                {
                    let ExprKind::Subquery { subquery: s, .. } = children.pop().unwrap().kind else {
                        unreachable!()
                    };
                    return Ok(subquery(*s, SubqueryType::Any, None, ComparisonOp::Equal));
                }
                Ok(Expr::operator(OperatorType::In, children))
            }
            "InSelectStatement" => {
                let s = self.select_internal(self.req(node, "SelectStatementInternal")?)?;
                Ok(subquery(s, SubqueryType::Any, None, ComparisonOp::Equal))
            }
            _ => {
                let haystack = self.expr(self.req(node, "OtherOperatorExpression")?)?;
                Ok(FunctionExpr::new("contains", vec![haystack]).into())
            }
        }
    }

    fn like_clause(&mut self, node: &ParseNode) -> Result<Expr> {
        // `RegexMatchOperatorFunctionName`
        let regex = if self.regex_full_match { "regexp_full_match" } else { REGEX_MATCH_FUNCTION };
        let mut variation = match self.name(self.alt(self.req(node, "LikeVariations")?)?) {
            "LikeToken" => "~~".to_string(),
            "ILikeToken" => "~~*".to_string(),
            "GlobToken" => "~~~".to_string(),
            "SimilarToToken" => "regexp_full_match".to_string(),
            "RegexMatchToken" => regex.to_string(),
            "RegexInsensitiveMatchToken" => format!("{regex}__case_insensitive"),
            "NotILikeOp" => "!~~*".to_string(),
            "NotLikeOp" => "!~~".to_string(),
            "NotRegexInsensitiveMatchOp" => format!("!{regex}__case_insensitive"),
            _ => format!("!{regex}"),
        };
        let case_insensitive = match variation.strip_suffix("__case_insensitive") {
            Some(base) => {
                variation = base.to_string();
                true
            }
            None => false,
        };
        let base = variation.trim_start_matches('!');
        let is_regex = base == "regexp_matches" || base == "regexp_full_match";
        let mut children = vec![self.expr(self.req(node, "OtherOperatorExpression")?)?];
        let escape = self.opt_expr(node, "EscapeClause")?;
        if case_insensitive && escape.is_some() {
            return Err(parser_error(
                "ESCAPE clause is not supported with case-insensitive regular expression match operators",
            ));
        }
        if is_regex && escape.is_some() {
            return Err(parser_error(
                "ESCAPE clause is not supported with SIMILAR TO and regular expression match operators",
            ));
        }
        if let Some(escape) = escape {
            if variation == "~~" {
                variation = "like_escape".into();
            } else if variation == "~~*" {
                variation = "ilike_escape".into();
            }
            children.push(escape);
        }
        if case_insensitive {
            children.push(Expr::constant(Literal::string("i")));
        }
        let mut f = FunctionExpr::new(variation, children);
        f.is_operator = !is_regex;
        Ok(f.into())
    }

    fn other_operator_tail(&mut self, node: &ParseNode) -> Result<(ParsedOperator, Expr)> {
        let op_node = self.alt(self.req(node, "OtherOperator")?)?;
        let op = if self.name(op_node) == "AnyAllParsedOperator" {
            let any_all = self.req(op_node, "AnyAllOperator")?;
            let is_any = self.name(self.alt(self.req(any_all, "AnyOrAll")?)?) == "SubqueryAny";
            ParsedOperator {
                name: Self::text(self.req(any_all, "AnyOperatorLiteral")?),
                any_all: Some(is_any),
            }
        } else {
            let named = self.alt(op_node)?;
            let name = if self.name(named) == "QualifiedOperator" {
                self.qualified_operator(named)?
            } else {
                Self::text(named)
            };
            ParsedOperator { name, any_all: None }
        };
        Ok((op, self.expr(self.req(node, "BitwiseExpression")?)?))
    }

    fn qualified_operator(&self, node: &ParseNode) -> Result<String> {
        let contents = self.req(node, "QualifiedOperatorContents")?;
        let mut parts = self.texts(contents, "ColIdDot");
        parts.push(Self::text(self.req(contents, "AnyOperatorLiteral")?));
        Ok(parts.join("."))
    }

    fn prefix_operator(&self, node: &ParseNode) -> Result<String> {
        let inner = self.alt(node)?;
        Ok(match self.name(inner) {
            "QualifiedOperator" => self.qualified_operator(inner)?,
            "MinusPrefixOperator" => "-".into(),
            _ => "+".into(),
        })
    }

    fn base_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        let links = self
            .sub(node, "IndirectionList")
            .map_or(0, |l| self.subs(l, "Indirection").len());
        self.enter_expression(links)?;
        let result = self.base_expression_inner(node);
        self.expression_depth -= links;
        result
    }

    fn base_expression_inner(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut expr = self.expr(self.req(node, "SingleExpression")?)?;
        let Some(list) = self.sub(node, "IndirectionList") else {
            return Ok(expr);
        };
        let mut previous_was_cast = false;
        for indirection in self.subs(list, "Indirection") {
            let ind = self.indirection(self.alt(indirection)?)?;
            expr = match ind {
                Indirection::Cast(target) => {
                    previous_was_cast = true;
                    Expr::cast(expr, target)
                }
                Indirection::Slice(op, mut bounds) => {
                    if previous_was_cast {
                        return Err(parser_error(
                            "Subscript/slice cannot be applied directly after a cast operator (e.g. x::TYPE[1:3] is not \
                             allowed). Wrap the cast in parentheses: (x::TYPE)[1:3]",
                        ));
                    }
                    bounds.insert(0, expr);
                    Expr::operator(op, bounds)
                }
                Indirection::Method(f) => {
                    previous_was_cast = false;
                    method_call(expr, f)?
                }
                Indirection::Field(name) => {
                    previous_was_cast = false;
                    Expr::operator(
                        OperatorType::StructExtract,
                        vec![expr, Expr::constant(Literal::string(name))],
                    )
                }
            };
        }
        Ok(expr)
    }

    fn indirection(&mut self, node: &ParseNode) -> Result<Indirection> {
        match self.name(node) {
            "CastOperator" => Ok(Indirection::Cast(self.type_(self.req(node, "Type")?)?)),
            "DotOperator" => {
                let dot = self.alt(node)?;
                if self.name(dot) == "DotColumnOperator" {
                    return Ok(Indirection::Field(Self::text(self.req(dot, "ColLabel")?)));
                }
                let method = self.req(dot, "MethodExpression")?;
                let name = Self::text(self.req(method, "ColLabel")?);
                let list = self.req(
                    self.req(method, "MethodExpressionArguments")?,
                    "MethodExpressionArgumentList",
                )?;
                let mut args = self.function_arguments(list, "MethodFunctionArguments")?;
                strip_count_star(&mut args.args);
                if args.has_ignore_nulls {
                    return Err(parser_error(
                        "RESPECT/IGNORE NULLS is not supported for non-window functions",
                    ));
                }
                let mut f = FunctionExpr::new(name, Vec::new());
                f.args = args.args;
                f.distinct = args.distinct;
                f.order_by = args.order_bys;
                Ok(Indirection::Method(f))
            }
            "SliceExpression" => {
                let bound = self.req(node, "SliceBound")?;
                let mut bounds = Vec::new();
                let lower = self.opt_expr(bound, "Expression")?;
                match self.sub(bound, "SliceUpperBound") {
                    None => bounds.extend(lower),
                    Some(upper) => {
                        bounds.push(lower.unwrap_or_else(empty_slice_bound));
                        let upper = self.alt(upper)?;
                        if self.name(upper) == "MinusStepSliceBound" {
                            bounds.push(empty_slice_bound());
                            bounds.push(self.step_bound(self.req(upper, "StepSliceBound")?)?);
                        } else {
                            let end = self.req(upper, "EndSliceBound")?;
                            bounds.push(self.opt_expr(end, "Expression")?.unwrap_or_else(empty_slice_bound));
                            if let Some(step) = self.sub(upper, "StepSliceBound") {
                                bounds.push(self.step_bound(step)?);
                            }
                        }
                    }
                }
                match bounds.len() {
                    0 => Err(parser_error("Empty subscript '[]' is not allowed")),
                    1 => Ok(Indirection::Slice(OperatorType::ArrayExtract, bounds)),
                    _ => Ok(Indirection::Slice(OperatorType::ArraySlice, bounds)),
                }
            }
            _ => Ok(Indirection::Method(FunctionExpr::new("factorial", Vec::new()))),
        }
    }

    fn step_bound(&mut self, node: &ParseNode) -> Result<Expr> {
        Ok(self.opt_expr(node, "Expression")?.unwrap_or_else(empty_slice_bound))
    }

    fn literal_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        match self.name(node) {
            "StringLiteral" => string_literal_expr(node),
            "NumberLiteral" => Ok(Expr::constant(Literal::number(Self::text(node)))),
            _ => Ok(Expr::constant(match self.name(self.alt(node)?) {
                "TrueLiteral" => Literal::boolean(true),
                "FalseLiteral" => Literal::boolean(false),
                _ => Literal::null(),
            })),
        }
    }

    fn parameter(&mut self, node: &ParseNode) -> Result<Expr> {
        // `ParamTypeCheck`
        let named = !matches!(
            self.name(node),
            "AnonymousParameter" | "QuestionMarkNumberedParameter" | "NumberedParameter"
        );
        if self.param_named.is_some_and(|last| last != named) {
            return Err(Error::not_implemented(
                "Mixing named and positional parameters is not supported yet",
            ));
        }
        self.param_named = Some(named);
        let identifier = match self.name(node) {
            "AnonymousParameter" => {
                let index = self.param_count + 1;
                self.param_count = index;
                index.to_string()
            }
            "QuestionMarkNumberedParameter" | "NumberedParameter" => {
                let literal = Literal::number(Self::text(self.req(node, "NumberLiteral")?));
                let number = match literal.try_get_i64() {
                    Some(n) if n > 0 => n as u64,
                    _ => return Err(parser_error("Parameter numbers must be greater than 0")),
                };
                self.param_count = self.param_count.max(number);
                number.to_string()
            }
            _ => {
                let name = Self::text(self.req(node, "ColLabel")?);
                let key = name.to_lowercase();
                let index = match self.named_params.get(&key) {
                    Some(&i) => i,
                    None => {
                        let i = self.param_count + 1;
                        self.named_params.insert(key, i);
                        i
                    }
                };
                self.param_count = self.param_count.max(index);
                name
            }
        };
        Ok(ExprKind::Parameter(identifier).into())
    }

    fn columns_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        let inner = self.expr(self.req(node, "Expression")?)?;
        let mut result = StarExpr::default();
        match inner.kind {
            ExprKind::Star(star) if star.columns => result.expr = Some(Box::new(ExprKind::Star(star).into())),
            ExprKind::Star(star) => result = star,
            ExprKind::Lambda { .. } => {
                result.expr = Some(Box::new(
                    FunctionExpr::new("list_filter", vec![Expr::star(), inner]).into(),
                ));
            }
            _ => result.expr = Some(Box::new(inner)),
        }
        result.columns = true;
        let expr: Expr = ExprKind::Star(result).into();
        if self.sub(node, "StarSymbol").is_some() {
            return Ok(Expr::operator(OperatorType::Unpack, vec![expr]));
        }
        Ok(expr)
    }

    fn extract_argument(&self, node: &ParseNode) -> Result<Expr> {
        let text = match self.name(node) {
            "ExtractDatePartArgument" => {
                let part = self.alt(self.req(node, "ExtractDatePart")?)?;
                DatePart::from_rule(self.name(part)).map_or(String::new(), |p| p.name().to_string())
            }
            _ => Self::text(node),
        };
        Ok(Expr::constant(Literal::string(text)))
    }

    fn interval_literal(&mut self, node: &ParseNode) -> Result<Expr> {
        let param = self.alt(self.req(node, "IntervalParameter")?)?;
        let mut expr = match self.name(param) {
            "IntervalStringParameter" => Expr::constant(Literal::string(Self::text(param))),
            "NumberLiteral" => Expr::constant(Literal::number(Self::text(param))),
            _ => self.expr(param)?,
        };
        let unit = match self.sub(node, "Interval") {
            Some(interval) => Some(self.interval_unit(interval)?),
            None => None,
        };
        let Some(unit) = unit else {
            return Ok(Expr::cast(expr, CastType::Builtin("INTERVAL")));
        };
        expr = Expr::cast(expr, CastType::Builtin("DOUBLE"));
        let target = unit.interval_target();
        if target != "DOUBLE" {
            expr = Expr::cast(FunctionExpr::new("trunc", vec![expr]).into(), CastType::Builtin(target));
        }
        Ok(FunctionExpr::new(unit.interval_function(), vec![expr]).into())
    }

    fn interval_unit(&self, node: &ParseNode) -> Result<DatePart> {
        let unit = self.alt(node)?;
        if self.name(unit) == "IntervalToInterval" {
            let range = self.alt(unit)?;
            let parts: Vec<_> = Self::parts(range)
                .into_iter()
                .filter_map(|p| DatePart::from_rule(self.name(p)))
                .collect();
            return Err(parser_error(format!(
                "{} TO {} is not supported",
                parts[0].name(),
                parts[1].name()
            )));
        }
        DatePart::from_rule(self.name(unit)).ok_or_else(|| Error::new(ErrorKind::Internal, "unknown interval unit"))
    }

    fn simple_case(&mut self, node: &ParseNode) -> Result<Expr> {
        let operand = self.expr(self.req(node, "Expression")?)?;
        let mut checks = Vec::new();
        for when_then in self.subs(node, "SimpleCaseWhenThen") {
            let mut comparisons = Vec::new();
            for when in self.subs(when_then, "SimpleCaseWhenOperand") {
                let inner = self.alt(when)?;
                let (op, right) = if self.name(inner) == "ComparisonExpressionTail" {
                    self.comparison_tail(inner)?
                } else {
                    (ComparisonOp::Equal, self.expr(inner)?)
                };
                comparisons.push(Expr::comparison(op, operand.clone(), right));
            }
            let when = if comparisons.len() == 1 {
                comparisons.pop().unwrap()
            } else {
                ExprKind::Conjunction {
                    and: false,
                    children: comparisons,
                }
                .into()
            };
            checks.push((when, self.expr(self.req(when_then, "Expression")?)?));
        }
        Ok(case(checks, self.opt_expr(node, "CaseElse")?))
    }

    fn exclude_name(&self, node: &ParseNode) -> Result<QualifiedColumnName> {
        let inner = self.alt(node)?;
        if self.name(inner) == "ExcludeDottedName" {
            let dotted = self.req(inner, "DottedIdentifier")?;
            let mut parts = vec![Self::text(self.req(dotted, "Identifier")?)];
            for label in self.subs(dotted, "DotColLabel") {
                parts.push(Self::text(self.req(label, "ColLabel")?));
            }
            if parts.len() > 4 {
                return Err(parser_error("Expected at most 4 entries in a qualified column name"));
            }
            let column = parts.pop().unwrap();
            return Ok(QualifiedColumnName {
                qualification: parts,
                column,
            });
        }
        Ok(QualifiedColumnName {
            qualification: Vec::new(),
            column: Self::text(inner),
        })
    }

    fn star_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut star = StarExpr::default();
        if let Some(qualifiers) = self.sub(node, "StarQualifierList") {
            let names = self.texts(qualifiers, "ColIdDot");
            if names.len() > 1 {
                return Err(parser_error(
                    "Did not expect more than one column in front of a star expression",
                ));
            }
            star.relation_name = names[0].clone();
        }
        // `QualifiedColumnEquality`: a prefix comparison, "tbl.i" being the same as "i" but not as "tbl2.i"
        let same = |a: &QualifiedColumnName, b: &QualifiedColumnName| {
            a.column.eq_ignore_ascii_case(&b.column)
                && a.qualification
                    .iter()
                    .rev()
                    .zip(b.qualification.iter().rev())
                    .all(|(x, y)| x.is_empty() || y.is_empty() || x.eq_ignore_ascii_case(y))
        };
        if let Some(exclude) = self.sub(node, "ExcludeList") {
            let names = self.alt(self.req(exclude, "ExcludeNames")?)?;
            for name in self.subs(names, "ExcludeName") {
                let column = self.exclude_name(name)?;
                if star.exclude_list.iter().any(|c| same(c, &column)) {
                    return Err(parser_error(format!("Duplicate entry \"{column}\" in EXCLUDE list")));
                }
                star.exclude_list.push(column);
            }
        }
        if let Some(replace) = self.sub(node, "ReplaceList") {
            let entries = self.alt(self.req(replace, "ReplaceEntries")?)?;
            let list = self.name(entries) == "ReplaceEntryList";
            for entry in self.subs(entries, "ReplaceEntry") {
                let expr = self.expr(self.req(entry, "Expression")?)?;
                let column = match self.expr(self.req(entry, "ColumnReference")?)?.kind {
                    ExprKind::ColumnRef(names) => names.last().cloned().unwrap_or_default(),
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Internal,
                            "Expected a column reference in the replace entry",
                        ));
                    }
                };
                if let Some(i) = star
                    .replace_list
                    .iter()
                    .position(|(c, _)| c.eq_ignore_ascii_case(&column))
                {
                    if list {
                        return Err(parser_error(format!("Duplicate entry \"{column}\" in REPLACE list")));
                    }
                    star.replace_list.remove(i);
                }
                star.replace_list.push((column, expr));
            }
            for (column, _) in &star.replace_list {
                if star
                    .exclude_list
                    .iter()
                    .any(|c| c.qualification.is_empty() && c.column.eq_ignore_ascii_case(column))
                {
                    return Err(parser_error(format!(
                        "Column {column} cannot occur in both EXCLUDE and REPLACE list"
                    )));
                }
            }
        }
        if let Some(rename) = self.sub(node, "RenameList") {
            let entries = self.alt(self.req(rename, "RenameEntries")?)?;
            for entry in self.subs(entries, "RenameEntry") {
                let column = self.exclude_name(self.req(entry, "ExcludeName")?)?;
                let new_name = Self::text(self.req(entry, "Identifier")?);
                star.rename_list.retain(|(c, _)| !same(c, &column));
                star.rename_list.push((column, new_name));
            }
            for (column, _) in &star.rename_list {
                if star.exclude_list.iter().any(|c| same(c, column)) {
                    return Err(parser_error(format!(
                        "Column \"{column}\" cannot occur in both EXCLUDE and RENAME list"
                    )));
                }
                if star
                    .replace_list
                    .iter()
                    .any(|(c, _)| c.eq_ignore_ascii_case(&column.column))
                {
                    return Err(parser_error(format!(
                        "Column \"{column}\" cannot occur in both REPLACE and RENAME list"
                    )));
                }
            }
        }
        Ok(ExprKind::Star(star).into())
    }

    fn function_arguments(&mut self, list: &ParseNode, args_rule: &str) -> Result<FunctionArguments> {
        let mut result = FunctionArguments::default();
        let distinct_or_all = self.sub(list, "DistinctOrAll");
        if let Some(args) = self.sub(list, args_rule) {
            for arg in self.subs(args, "FunctionArgument") {
                result.args.push(self.function_argument(arg)?);
            }
        } else if let Some(d) = distinct_or_all {
            let keyword = if self.name(self.alt(d)?) == "DistinctKeyword" {
                "DISTINCT"
            } else {
                "ALL"
            };
            return Err(parser_error(format!("{keyword} requires at least one argument")));
        }
        if let Some(d) = distinct_or_all {
            result.distinct = self.name(self.alt(d)?) == "DistinctKeyword";
        }
        if let Some(order) = self.sub(list, "OrderByClause") {
            result.order_bys = self.order_by_clause(order)?;
        }
        if let Some(nulls) = self.sub(list, "IgnoreOrRespectNulls") {
            result.has_ignore_nulls = true;
            result.ignore_nulls = self.name(self.alt(nulls)?) == "IgnoreNulls";
        }
        Ok(result)
    }

    fn function_argument(&mut self, node: &ParseNode) -> Result<FunctionArgument> {
        let inner = self.alt(node)?;
        if self.name(inner) == "NamedFunctionArgument" {
            let name = Self::text(self.req(inner, "TypeFuncName")?);
            let expr = self.expr(self.req(inner, "Expression")?)?.with_alias(name.clone());
            return Ok(FunctionArgument { name, expr });
        }
        Ok(FunctionArgument::positional(self.expr(inner)?))
    }

    fn function_expression(&mut self, node: &ParseNode) -> Result<Expr> {
        let name = QualifiedName::from_parts(self.rule_texts(self.alt(self.req(node, "FunctionIdentifier")?)?));
        let list = self.req(
            self.req(node, "FunctionExpressionArguments")?,
            "FunctionExpressionArgumentList",
        )?;
        let mut args = self.function_arguments(list, "FunctionArgumentList")?;
        let filter = match self.sub(node, "FilterClause") {
            Some(f) => {
                let contents = self.req(self.req(f, "FilterClauseExpression")?, "FilterClauseContents")?;
                Some(Box::new(self.expr(self.req(contents, "Expression")?)?))
            }
            None => None,
        };
        let within_group = match self.sub(node, "WithinGroupClause") {
            Some(w) => Some(self.order_by_clause(self.req(w, "OrderByClause")?)?),
            None => None,
        };
        let export_state = self.sub(node, "ExportClause").is_some();
        if let Some(orders) = &within_group {
            if args.distinct {
                return Err(parser_error("DISTINCT is not allowed in combination with WITHIN GROUP"));
            }
            if !args.order_bys.is_empty() {
                return Err(parser_error(
                    "Cannot use multiple ORDER BY statements with WITHIN GROUP",
                ));
            }
            if orders.len() != 1 {
                return Err(parser_error("Cannot use multiple ORDER BY clauses with WITHIN GROUP"));
            }
            args.order_bys = orders.clone();
        }
        if args.args.len() == 1
            && is_unqualified_star(&args.args[0].expr)
            && !args.distinct
            && args.order_bys.is_empty()
        {
            args.args.clear();
        }
        let mut lower = name.name.to_lowercase();
        if let Some(over) = self.sub(node, "OverClause") {
            if self.in_window_definition {
                return Err(parser_error("window functions are not allowed in window definitions"));
            }
            if lower == "first" || lower == "last" {
                lower += "_value";
            }
            if export_state {
                return Err(parser_error("EXPORT_STATE is not supported for window functions!"));
            }
            self.in_window_definition = true;
            let window = self.window_frame(self.req(over, "WindowFrame")?);
            self.in_window_definition = false;
            let mut window = window?;
            window.name = QualifiedName {
                qualification: name.qualification,
                name: lower,
            };
            window.args = args.args;
            window.has_ignore_nulls = args.has_ignore_nulls;
            window.ignore_nulls = args.ignore_nulls;
            window.filter = filter;
            window.arg_orders = args.order_bys;
            window.distinct = args.distinct;
            return Ok(ExprKind::Window(Box::new(window)).into());
        }
        if lower == "count" && args.args.is_empty() {
            lower = "count_star".into();
        }
        // `x.ifnull(y)` parses as a call in schema x. The rewrites are not catalog functions, so for
        // them a qualification other than main is the column the function is called on
        let call_on_column = !name.qualification.is_empty()
            && is_function_rewrite(&lower)
            && !name
                .qualification
                .last()
                .is_some_and(|s| s.eq_ignore_ascii_case("main"));
        if !call_on_column && let Some(rewritten) = function_rewrite(&lower, &mut args.args, false)? {
            return Ok(rewritten);
        }
        if args.has_ignore_nulls {
            return Err(parser_error(
                "RESPECT/IGNORE NULLS is not supported for non-window functions",
            ));
        }
        if within_group.is_some() {
            match lower.as_str() {
                "percentile_cont" | "percentile_disc" => {
                    if args.args.len() != 1 {
                        return Err(parser_error(format!(
                            "Wrong number of arguments for {}",
                            lower.to_uppercase()
                        )));
                    }
                    lower = lower.replace("percentile", "quantile");
                }
                "mode" => {
                    if !args.args.is_empty() {
                        return Err(parser_error("Wrong number of arguments for MODE"));
                    }
                }
                _ => {
                    return Err(parser_error(format!(
                        "Unknown ordered aggregate {}.",
                        sql_quoted_identifier(&name.name)
                    )));
                }
            }
        }
        let column = call_on_column.then(|| name.qualification.clone());
        let function = FunctionExpr {
            name: QualifiedName {
                qualification: if call_on_column { Vec::new() } else { name.qualification },
                name: lower,
            },
            args: args.args,
            filter,
            order_by: args.order_bys,
            distinct: args.distinct,
            is_operator: false,
            export_state,
        };
        match column {
            Some(column) => method_call(Expr::column(column), function),
            None => Ok(function.into()),
        }
    }

    fn window_frame(&mut self, node: &ParseNode) -> Result<WindowExpr> {
        let inner = self.alt(node)?;
        match self.name(inner) {
            "IdentifierWindowFrame" => self.window_clause(&Self::text(inner)),
            "ParensIdentifier" => {
                let name = Self::text(self.req(inner, "Identifier")?);
                let window = self.window_clause(&name)?;
                check_copyable_window(&window, &name)?;
                Ok(window)
            }
            _ => {
                let definition = self.alt(inner)?;
                let contents_parent = if self.name(definition) == "WindowFrameNameContentsParens" {
                    self.req(definition, "WindowFrameNameContents")?
                } else {
                    definition
                };
                self.window_with_base(contents_parent)
            }
        }
    }

    /// A window's contents on top of the window it names as its base, if any.
    fn window_with_base(&mut self, contents_parent: &ParseNode) -> Result<WindowExpr> {
        let contents = self.window_frame_contents(self.req(contents_parent, "WindowFrameContents")?)?;
        let Some(base) = self.sub(contents_parent, "BaseWindowName") else {
            return Ok(contents);
        };
        let base_name = Self::text(base);
        if ["partition", "range", "rows", "groups"].contains(&base_name.to_lowercase().as_str()) {
            return Err(parser_error(format!("Invalid window name \"{base_name}\"")));
        }
        let mut copied = self.window_clause(&base_name)?;
        check_copyable_window(&copied, &base_name)?;
        copied.start = contents.start;
        copied.end = contents.end;
        copied.exclude = contents.exclude;
        copied.start_expr = contents.start_expr;
        copied.end_expr = contents.end_expr;
        if !copied.orders.is_empty() && !contents.orders.is_empty() {
            return Err(parser_error(format!(
                "Cannot override ORDER BY clause of window \"{base_name}\""
            )));
        }
        if copied.orders.is_empty() {
            copied.orders = contents.orders;
        }
        if !copied.partitions.is_empty() && !contents.partitions.is_empty() {
            return Err(parser_error(format!(
                "Cannot override PARTITION BY clause of window \"{base_name}\""
            )));
        }
        if copied.partitions.is_empty() {
            copied.partitions = contents.partitions;
        }
        Ok(copied)
    }

    fn window_clause(&self, name: &str) -> Result<WindowExpr> {
        self.window_clauses
            .last()
            .and_then(|windows| windows.get(&name.to_lowercase()))
            .cloned()
            .ok_or_else(|| parser_error(format!("window {} does not exist", sql_quoted_identifier(name))))
    }

    fn window_frame_contents(&mut self, node: &ParseNode) -> Result<WindowExpr> {
        let mut window = WindowExpr::default();
        if let Some(partition) = self.sub(node, "WindowPartition") {
            window.partitions = self.exprs(partition, "Expression")?;
        }
        if let Some(order) = self.sub(node, "OrderByClause") {
            window.orders = self.order_by_clause(order)?;
            for o in &window.orders {
                if let ExprKind::Star(star) = &o.expr.kind
                    && star.expr.is_none()
                {
                    return Err(parser_error("Cannot ORDER BY ALL in a window expression"));
                }
            }
        }
        match self.sub(node, "FrameClause") {
            Some(frame) => self.frame_clause(frame, &mut window)?,
            None => {
                window.start = WindowBoundary::UnboundedPreceding;
                window.end = WindowBoundary::CurrentRowRange;
            }
        }
        Ok(window)
    }

    fn frame_bound(&mut self, node: &ParseNode) -> Result<(WindowBoundary, Option<Expr>)> {
        let bound = self.alt(node)?;
        let preceding = |s: &Self, n: &ParseNode| -> Result<bool> {
            Ok(s.name(s.alt(s.req(n, "PrecedingOrFollowing")?)?) == "PrecedingFrame")
        };
        Ok(match self.name(bound) {
            "FrameUnbounded" => (
                if preceding(self, bound)? {
                    WindowBoundary::UnboundedPreceding
                } else {
                    WindowBoundary::UnboundedFollowing
                },
                None,
            ),
            "FrameCurrentRow" => (WindowBoundary::CurrentRowRange, None),
            _ => {
                let expr = self.expr(self.req(bound, "Expression")?)?;
                let boundary = if preceding(self, bound)? {
                    WindowBoundary::ExprPrecedingRange
                } else {
                    WindowBoundary::ExprFollowingRange
                };
                (boundary, Some(expr))
            }
        })
    }

    fn frame_clause(&mut self, node: &ParseNode, window: &mut WindowExpr) -> Result<()> {
        use WindowBoundary::*;
        let framing = self.name(self.alt(self.req(node, "Framing")?)?);
        let extent = self.alt(self.req(node, "FrameExtent")?)?;
        let mut bounds = Vec::new();
        for b in self.subs(extent, "FrameBound") {
            bounds.push(self.frame_bound(b)?);
        }
        if self.name(extent) == "SingleFrameExtent" {
            if bounds[0].0 == ExprFollowingRange {
                return Err(parser_error(
                    "Frame starting from following row cannot end with current row",
                ));
            }
            bounds.push((Invalid, None));
        }
        if bounds[0].0 == CurrentRowRange && bounds[1].0 == ExprPrecedingRange {
            return Err(parser_error(
                "Frame starting from current row cannot have preceding rows",
            ));
        }
        if bounds[0].0 == ExprFollowingRange && matches!(bounds[1].0, CurrentRowRange | ExprPrecedingRange) {
            return Err(parser_error(
                "Frame starting from following row cannot have preceding rows",
            ));
        }
        for (boundary, _) in bounds.iter_mut() {
            *boundary = match (framing, *boundary) {
                ("RowsFraming", CurrentRowRange | Invalid) => CurrentRowRows,
                ("RowsFraming", ExprPrecedingRange) => ExprPrecedingRows,
                ("RowsFraming", ExprFollowingRange) => ExprFollowingRows,
                ("GroupsFraming", CurrentRowRange | Invalid) => CurrentRowGroups,
                ("GroupsFraming", ExprPrecedingRange) => ExprPrecedingGroups,
                ("GroupsFraming", ExprFollowingRange) => ExprFollowingGroups,
                ("RangeFraming", Invalid) => CurrentRowRange,
                (_, b) => b,
            };
        }
        if bounds[0].0 == UnboundedFollowing {
            return Err(parser_error("Frame start cannot be UNBOUNDED FOLLOWING"));
        }
        if bounds[1].0 == UnboundedPreceding {
            return Err(parser_error("Frame end cannot be UNBOUNDED PRECEDING"));
        }
        let (end, end_expr) = bounds.pop().unwrap();
        let (start, start_expr) = bounds.pop().unwrap();
        window.start = start;
        window.start_expr = start_expr.map(Box::new);
        window.end = end;
        window.end_expr = end_expr.map(Box::new);
        if let Some(exclude) = self.sub(node, "WindowExcludeClause") {
            window.exclude = match self.name(self.alt(self.req(exclude, "WindowExcludeElement")?)?) {
                "ExcludeCurrentRow" => WindowExclude::CurrentRow,
                "ExcludeGroup" => WindowExclude::Group,
                "ExcludeTies" => WindowExclude::Ties,
                _ => WindowExclude::NoOther,
            };
        }
        Ok(())
    }

    fn list_comprehension(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut exprs = self.exprs(node, "Expression")?;
        let in_expr = exprs.pop().expect("list comprehension source");
        let result = exprs.pop().expect("list comprehension result");
        let params = self.texts(node, "ColIdOrString");
        let Some(filter) = self.opt_expr(node, "ListComprehensionFilter")? else {
            return Ok(FunctionExpr::new("list_apply", vec![in_expr, Expr::lambda(&params, result)]).into());
        };
        let mut pack = FunctionExpr::new("struct_pack", Vec::new());
        pack.args = vec![
            FunctionArgument {
                name: "filter".into(),
                expr: filter,
            },
            FunctionArgument {
                name: "result".into(),
                expr: result,
            },
        ];
        let stage1 = FunctionExpr::new("list_apply", vec![in_expr, Expr::lambda(&params, pack.into())]);
        let extract = |field: &str| -> Expr {
            let e = FunctionExpr::new(
                "struct_extract",
                vec![
                    Expr::column(vec!["elem".into()]),
                    Expr::constant(Literal::string(field)),
                ],
            );
            Expr::lambda(&["elem".to_string()], e.into())
        };
        let stage2 = FunctionExpr::new("list_filter", vec![stage1.into(), extract("filter")]);
        Ok(FunctionExpr::new("list_apply", vec![stage2.into(), extract("result")]).into())
    }

    fn array_parens_select(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut inner = self.select_internal(self.req(node, "SelectStatementInternal")?)?;
        let mut is_select = false;
        let mut agg_child = Expr::star();
        if let QueryNodeKind::Select(select) = &inner.node.kind {
            is_select = true;
            if select.select_list.len() != 1 {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Subquery returns {} columns - expected 1", select.select_list.len()),
                ));
            }
            agg_child = ExprKind::Positional(1).into();
        } else if let ExprKind::Star(star) = &mut agg_child.kind {
            star.columns = true;
        }
        let mut aggr = FunctionExpr::new("array_agg", vec![agg_child]);
        if let Some(ResultModifier::Order(orders)) = inner
            .node
            .modifiers
            .iter()
            .find(|m| matches!(m, ResultModifier::Order(_)))
        {
            aggr.order_by = orders.clone();
        }
        let mut array_idx = 0;
        for order in aggr.order_by.iter_mut() {
            if let ExprKind::Constant(literal) = &order.expr.kind {
                if let Some(index) = literal.try_get_i64() {
                    let positional = if index < 0 { u64::MAX } else { index as u64 };
                    order.expr = ExprKind::Positional(positional).into();
                }
            } else if is_select {
                array_idx += 1;
                let alias = format!("__array_internal_idx_{array_idx}");
                let pushed = std::mem::replace(&mut order.expr, Expr::column(vec![alias.clone()]));
                if let QueryNodeKind::Select(select) = &mut inner.node.kind {
                    select.select_list.push(pushed.with_alias(alias));
                }
            } else {
                remove_order_qualification(&mut order.expr);
            }
        }
        let aggr: Expr = aggr.into();
        let is_null = Expr::operator(OperatorType::IsNull, vec![aggr.clone()]);
        let case_expr = case(
            vec![(is_null, FunctionExpr::new("list_value", Vec::new()).into())],
            Some(aggr),
        );
        let outer = SelectNode {
            select_list: vec![case_expr],
            from_table: TableRef::subquery(inner),
            ..SelectNode::default()
        };
        Ok(subquery(
            SelectStatement { node: outer.into() },
            SubqueryType::Scalar,
            None,
            ComparisonOp::Equal,
        ))
    }

    // -----------------------------------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------------------------------

    /// `Type <- TypeVariations ArrayBounds*`.
    fn type_(&mut self, node: &ParseNode) -> Result<CastType> {
        Ok(CastType::Parsed(Box::new(self.nested_type(node)?)))
    }

    /// `Type` and `NestedType` each nest one level deeper.
    fn nested_type(&mut self, node: &ParseNode) -> Result<Expr> {
        self.enter_expression(1)?;
        let result = self.nested_type_inner(node);
        self.expression_depth -= 1;
        result
    }

    fn nested_type_inner(&mut self, node: &ParseNode) -> Result<Expr> {
        let mut ty = self.type_variation(self.alt(self.req(node, "TypeVariations")?)?)?;
        for bound in self.subs(node, "ArrayBounds") {
            let size = self.array_bound(self.alt(bound)?)?;
            ty = match size {
                None => type_expr("list", vec![ty]),
                Some(n) => type_expr("array", vec![ty, Expr::constant(Literal::integer(n))]),
            };
        }
        Ok(ty)
    }

    /// Wraps `ty` in a LIST or ARRAY for one `ArrayBounds`.
    fn wrap_array_bound(&mut self, ty: Expr, bound: &ParseNode) -> Result<Expr> {
        Ok(match self.array_bound(bound)? {
            None => type_expr("list", vec![ty]),
            Some(n) => type_expr("array", vec![ty, Expr::constant(Literal::integer(n))]),
        })
    }

    fn array_bound(&mut self, node: &ParseNode) -> Result<Option<i64>> {
        let brackets = match self.name(node) {
            "ArrayKeyword" => return Ok(None),
            "ArrayKeywordWithBounds" => self.req(node, "SquareBracketsArray")?,
            _ => node,
        };
        let Some(size) = self.opt_expr(brackets, "Expression")? else {
            return Ok(None);
        };
        let ExprKind::Constant(literal) = &size.kind else {
            return Err(parser_error("Expected a constant number as array size"));
        };
        match literal.try_get_i64() {
            Some(n) if n < 0 => Err(parser_error("Array size must be greater than 0")),
            Some(n) => Ok(Some(n)),
            None => Err(Error::new(
                ErrorKind::Binder,
                format!("Expected an integer as array bound instead of {}", literal.text),
            )),
        }
    }

    fn type_modifiers(&mut self, node: &ParseNode) -> Result<Vec<Expr>> {
        let Some(modifiers) = self.sub(node, "TypeModifiers") else {
            return Ok(Vec::new());
        };
        let exprs = self.exprs(modifiers, "Expression")?;
        if exprs.iter().any(|e| !matches!(e.kind, ExprKind::Constant(_))) {
            return Err(parser_error("Expected a constant as type modifier"));
        }
        Ok(exprs)
    }

    fn col_id_types(&mut self, node: &ParseNode) -> Result<Vec<Expr>> {
        let Some(list) = self.sub(node, "ColIdTypeList") else {
            return Ok(Vec::new());
        };
        let mut members = Vec::new();
        for member in self.subs(list, "ColIdType") {
            let name = Self::text(self.req(member, "ColId")?);
            members.push(self.nested_type(self.req(member, "NestedType")?)?.with_alias(name));
        }
        Ok(members)
    }

    fn type_variation(&mut self, node: &ParseNode) -> Result<Expr> {
        Ok(match self.name(node) {
            "TimeType" => {
                let is_time = self.name(self.alt(self.req(node, "TimeOrTimestamp")?)?) == "TimeTypeId";
                let modifiers = self.type_modifiers(node)?;
                let with_tz = match self.sub(node, "TimeZone") {
                    Some(tz) => self.name(self.alt(self.req(tz, "WithOrWithout")?)?) == "WithRule",
                    None => false,
                };
                type_expr(time_type_name(is_time, with_tz, &modifiers)?, Vec::new())
            }
            "IntervalType" => type_expr("INTERVAL", Vec::new()),
            "BitType" => type_expr("BIT", Vec::new()),
            "VariantType" => type_expr("VARIANT", Vec::new()),
            "RowType" => type_expr("STRUCT", self.col_id_types(node)?),
            "UnionType" => type_expr("UNION", self.col_id_types(node)?),
            "MapType" => {
                let mut children = Vec::new();
                for t in self.subs(node, "NestedType") {
                    children.push(self.nested_type(t)?);
                }
                type_expr("MAP", children)
            }
            "TupleType" => {
                let mut children = Vec::new();
                for t in self.subs(node, "NestedType") {
                    children.push(self.nested_type(t)?);
                }
                type_expr("TUPLE", children)
            }
            "SetofType" => self.nested_type(self.req(node, "NestedType")?)?,
            "GeometryType" => {
                let children = match self.opt_expr(node, "Expression")? {
                    Some(e) if matches!(e.kind, ExprKind::Constant(_)) => vec![e],
                    Some(_) => return Err(parser_error("Expected a constant as type modifier")),
                    None => Vec::new(),
                };
                type_expr("GEOMETRY", children)
            }
            "NumericType" => {
                let inner = self.alt(node)?;
                let ty = self.alt(inner)?;
                match self.name(ty) {
                    "IntType" | "IntegerType" => type_expr("INTEGER", Vec::new()),
                    "SmallintType" => type_expr("SMALLINT", Vec::new()),
                    "BigintType" => type_expr("BIGINT", Vec::new()),
                    "RealType" => type_expr("FLOAT", Vec::new()),
                    "BooleanType" => type_expr("BOOLEAN", Vec::new()),
                    "DoubleType" => type_expr("DOUBLE", Vec::new()),
                    "FloatType" => {
                        let children = match self.sub(ty, "NumberLiteral") {
                            Some(n) => vec![Expr::constant(Literal::number(Self::text(n)))],
                            None => Vec::new(),
                        };
                        type_expr("FLOAT", children)
                    }
                    _ => type_expr("DECIMAL", self.type_modifiers(ty)?),
                }
            }
            "SimpleType" => {
                let inner = self.alt(node)?;
                let modifiers = self.type_modifiers(inner)?;
                if self.name(inner) == "CharacterSimpleType" {
                    type_expr("VARCHAR", modifiers)
                } else {
                    let qualified = self.alt(self.req(inner, "QualifiedTypeName")?)?;
                    ExprKind::Type(TypeExpr {
                        name: QualifiedName::from_parts(self.rule_texts(qualified)),
                        children: modifiers,
                    })
                    .into()
                }
            }
            other => return Err(Error::new(ErrorKind::Internal, format!("unknown type rule {other}"))),
        })
    }

    // -----------------------------------------------------------------------------------------
    // SELECT
    // -----------------------------------------------------------------------------------------

    /// `SelectStatementInternal <- WithClause? SelectSetOpChain ResultModifiers?`.
    pub fn select_internal(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let cte_map = match self.sub(node, "WithClause") {
            Some(with) => self.with_clause(with)?,
            None => CteMap::default(),
        };
        let scoped = !cte_map.0.is_empty();
        if scoped {
            self.cte_stack.push(cte_map.clone());
        }
        let result = self.select_body(node, cte_map);
        if scoped {
            self.cte_stack.pop();
        }
        result
    }

    /// The set operations and result modifiers of a `SelectStatementInternal`.
    fn select_body(&mut self, node: &ParseNode, cte_map: CteMap) -> Result<SelectStatement> {
        let mut statement = self.set_op_chain(self.req(node, "SelectSetOpChain")?)?;
        if !cte_map.0.is_empty() {
            statement.node.cte_map = cte_map;
        }
        if let Some(modifiers) = self.sub(node, "ResultModifiers") {
            if let Some(order) = self.sub(modifiers, "OrderByClause") {
                let orders = self.order_by_clause(order)?;
                statement.node.modifiers.push(ResultModifier::Order(orders));
            }
            if let Some(limit) = self.sub(modifiers, "LimitOffset")
                && let Some(m) = self.limit_offset(self.alt(limit)?)?
            {
                statement.node.modifiers.push(m);
            }
        }
        if let QueryNodeKind::Select(select) = &statement.node.kind
            && let TableRefKind::Show(show) = &select.from_table.kind
        {
            let kind = match show.show_type {
                ShowType::Show => "SHOW",
                ShowType::Describe => "DESCRIBE",
                ShowType::Summary => "SUMMARY",
                ShowType::ShowFrom => "SHOW_FROM",
                ShowType::ShowSpecial => "SHOW_SPECIAL",
            };
            if !statement.node.cte_map.0.is_empty() {
                return Err(parser_error(format!(
                    "{kind} with CTE not allowed - wrap the statement in a subquery instead"
                )));
            }
            if !statement.node.modifiers.is_empty() {
                return Err(parser_error(format!(
                    "{kind} with ORDER BY not allowed - wrap the statement in a subquery instead"
                )));
            }
        }
        Ok(statement)
    }

    fn set_op_chain(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let mut select = self.intersect_chain(self.req(node, "IntersectChain")?)?;
        for tail in self.subs(node, "SelectSetOpChainTail") {
            let clause = self.req(tail, "SetopClause")?;
            let mut setop_type = if self.name(self.alt(self.req(clause, "SetopType")?)?) == "SetopUnion" {
                SetOpType::Union
            } else {
                SetOpType::Except
            };
            let setop_all = self.setop_all(clause)?;
            if self.sub(clause, "ByName").is_some() {
                if setop_type != SetOpType::Union {
                    return Err(parser_error("Invalid combination of EXCEPT and BY NAME"));
                }
                setop_type = SetOpType::UnionByName;
            }
            let right = self.intersect_chain(self.req(tail, "IntersectChain")?)?;
            if select.node.modifiers.is_empty()
                && select.node.cte_map.0.is_empty()
                && let QueryNodeKind::SetOperation(existing) = &mut select.node.kind
                && existing.setop_type == setop_type
                && existing.setop_all == setop_all
                && matches!(setop_type, SetOpType::Union | SetOpType::UnionByName)
            {
                existing.children.push(right.node);
                continue;
            }
            select.node = QueryNodeKind::SetOperation(SetOperationNode {
                setop_type,
                setop_all,
                children: vec![select.node, right.node],
            })
            .into();
        }
        Ok(select)
    }

    fn setop_all(&self, clause: &ParseNode) -> Result<bool> {
        Ok(match self.sub(clause, "DistinctOrAll") {
            Some(d) => self.name(self.alt(d)?) == "AllKeyword",
            None => false,
        })
    }

    fn intersect_chain(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let mut select = self.select_atom(self.req(node, "SelectAtom")?)?;
        for tail in self.subs(node, "IntersectChainTail") {
            let setop_all = self.setop_all(self.req(tail, "SetIntersectClause")?)?;
            let right = self.select_atom(self.req(tail, "SelectAtom")?)?;
            select.node = QueryNodeKind::SetOperation(SetOperationNode {
                setop_type: SetOpType::Intersect,
                setop_all,
                children: vec![select.node, right.node],
            })
            .into();
        }
        Ok(select)
    }

    fn select_atom(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let inner = self.alt(node)?;
        if self.name(inner) == "SelectParens" {
            return self.select_internal(self.req(inner, "SelectStatementInternal")?);
        }
        let kind = self.alt(inner)?;
        match self.name(kind) {
            "OptionalParensSimpleSelect" => {
                let simple = self.alt(kind)?;
                let simple = if self.name(simple) == "SimpleSelectParens" {
                    self.req(simple, "SimpleSelect")?
                } else {
                    simple
                };
                self.simple_select(simple)
            }
            "ValuesClause" => self.values_clause(kind),
            "DescribeStatement" => self.describe(kind),
            "PivotStatement" => self.pivot_statement(kind),
            "UnpivotStatement" => self.unpivot_statement(kind),
            "TableStatement" => {
                let table = self.base_table_name(self.req(kind, "BaseTableName")?)?;
                let node = SelectNode {
                    select_list: vec![Expr::star()],
                    from_table: table,
                    ..SelectNode::default()
                };
                Ok(SelectStatement { node: node.into() })
            }
            other => Err(not_implemented(format!(
                "the transformer for {other} is not yet ported"
            ))),
        }
    }

    fn simple_select(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        self.window_clauses.push(HashMap::new());
        let result = self.simple_select_inner(node);
        self.window_clauses.pop();
        result
    }

    fn simple_select_inner(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        if let Some(window) = self.sub(node, "WindowClause") {
            for definition in self.subs(window, "WindowDefinition") {
                let name = Self::text(self.req(definition, "Identifier")?);
                self.in_window_definition = true;
                let frame = self.alt(self.req(definition, "WindowFrameDefinition")?)?;
                let contents_parent = if self.name(frame) == "WindowFrameNameContentsParens" {
                    self.req(frame, "WindowFrameNameContents")?
                } else {
                    frame
                };
                let window = self.window_with_base(contents_parent);
                self.in_window_definition = false;
                let window = window?;
                let windows = self.window_clauses.last_mut().expect("window scope");
                if windows.contains_key(&name.to_lowercase()) {
                    return Err(parser_error(format!(
                        "window {} is already defined",
                        sql_quoted_identifier(&name)
                    )));
                }
                windows.insert(name.to_lowercase(), window);
            }
        }
        let from = self.alt(self.req(node, "SelectFrom")?)?;
        // in the order of the text, which is how `?` parameters number
        let from_clause = self.sub(from, "FromClause");
        let select_clause = self.sub(from, "SelectClause");
        let from_first = self.name(from) == "FromSelectClause";
        let mut from_table = None;
        if from_first {
            from_table = from_clause.map(|f| self.transform_from(f)).transpose()?;
        }
        let (mut select, mut modifiers) = match select_clause {
            Some(clause) => self.select_clause(clause)?,
            None => (
                SelectNode {
                    select_list: vec![Expr::star()],
                    ..SelectNode::default()
                },
                Vec::new(),
            ),
        };
        if !from_first {
            from_table = from_clause.map(|f| self.transform_from(f)).transpose()?;
        }
        select.from_table = from_table.unwrap_or_else(TableRef::empty);
        select.where_clause = self.opt_expr(node, "WhereClause")?.map(Box::new);
        if let Some(group) = self.sub(node, "GroupByClause") {
            let groups = self.group_by(self.alt(self.req(group, "GroupByExpressions")?)?)?;
            if groups.group_expressions.len() == 1 && groups.group_expressions[0].is_empty_star() {
                select.force_aggregates = true;
            } else {
                select.groups = groups;
            }
        }
        select.having = self.opt_expr(node, "HavingClause")?.map(Box::new);
        select.qualify = self.opt_expr(node, "QualifyClause")?.map(Box::new);
        if let Some(sample) = self.sub(node, "SelectSampleClause") {
            select.sample = Some(self.sample_entry(self.req(sample, "SampleEntry")?)?);
        }
        let mut node: QueryNode = select.into();
        node.modifiers.append(&mut modifiers);
        Ok(SelectStatement { node })
    }

    fn select_clause(&mut self, node: &ParseNode) -> Result<(SelectNode, Vec<ResultModifier>)> {
        let mut modifiers = Vec::new();
        if let Some(distinct) = self.sub(node, "DistinctClause") {
            let inner = self.alt(distinct)?;
            if self.name(inner) == "DistinctOn" {
                let on = match self.sub(inner, "DistinctOnTargets") {
                    Some(t) => self.exprs(t, "Expression")?,
                    None => Vec::new(),
                };
                modifiers.push(ResultModifier::Distinct(on));
            }
        }
        let Some(targets) = self.sub(node, "TargetList") else {
            return Err(parser_error("SELECT clause without selection list"));
        };
        let select = SelectNode {
            select_list: self.target_list(targets)?,
            ..SelectNode::default()
        };
        Ok((select, modifiers))
    }

    fn target_list(&mut self, node: &ParseNode) -> Result<Vec<Expr>> {
        let mut list = Vec::new();
        for aliased in self.subs(node, "AliasedExpression") {
            let inner = self.alt(aliased)?;
            let expr = self.expr(self.req(inner, "Expression")?)?;
            let alias = match self.name(inner) {
                "ColIdExpression" => Some(Self::text(self.req(inner, "ColId")?)),
                "ExpressionAsCollabel" => Some(Self::text(self.req(inner, "ColLabelOrString")?)),
                _ => self.sub(inner, "BareColLabel").map(Self::text),
            };
            list.push(match alias {
                Some(a) => expr.with_alias(a),
                None => expr,
            });
        }
        Ok(list)
    }

    fn values_clause(&mut self, node: &ParseNode) -> Result<SelectStatement> {
        let mut rows = Vec::new();
        for row in self.subs(node, "ValuesExpressions") {
            rows.push(self.exprs(row, "Expression")?);
        }
        for row in rows.iter().skip(1) {
            if row.len() != rows[0].len() {
                let entries = |n: usize| if n == 1 { "entry" } else { "entries" };
                return Err(parser_error(format!(
                    "VALUES lists must all be the same length, expected {} {} but found a list with {} {}",
                    rows[0].len(),
                    entries(rows[0].len()),
                    row.len(),
                    entries(row.len())
                )));
            }
        }
        let mut values = TableRef::from(TableRefKind::ExpressionList(rows));
        values.alias = "valueslist".into();
        let select = SelectNode {
            select_list: vec![Expr::star()],
            from_table: values,
            ..SelectNode::default()
        };
        Ok(SelectStatement { node: select.into() })
    }

    fn with_clause(&mut self, node: &ParseNode) -> Result<CteMap> {
        let recursive = self.sub(node, "Recursive").is_some();
        let mut map = CteMap::default();
        for entry in self.subs(node, "WithStatement") {
            let name = Self::text(self.req(entry, "ColIdOrString")?);
            let aliases = match self.sub(entry, "ColumnAliases") {
                Some(a) => self.texts(a, "ColIdOrString"),
                None => Vec::new(),
            };
            let key_targets = match self.sub(entry, "UsingKey") {
                Some(k) => self.target_list(self.req(k, "TargetList")?)?,
                None => Vec::new(),
            };
            let materialized = match self.sub(entry, "Materialized") {
                Some(m) if self.has(m, "NOT") => CteMaterialize::Never,
                Some(_) => CteMaterialize::Always,
                None => CteMaterialize::Default,
            };
            let body = self.alt(self.req(entry, "CTEBody")?)?;
            let mut query = if self.name(body) == "CTEDMLBody" {
                let kind = match self.statement(self.req(body, "Statement")?)? {
                    Statement::Insert(s) => QueryNodeKind::Insert(s),
                    Statement::Update(s) => QueryNodeKind::Update(s),
                    Statement::Delete(s) => QueryNodeKind::Delete(s),
                    Statement::Copy(info) if !info.is_from => QueryNodeKind::Copy(info),
                    Statement::Copy(_) => return Err(parser_error("COPY FROM cannot be used as a CTE body")),
                    _ => {
                        return Err(parser_error(
                            "A CTE body must be a SELECT, INSERT, UPDATE, DELETE, or COPY TO statement",
                        ));
                    }
                };
                if recursive {
                    if matches!(kind, QueryNodeKind::Copy(_)) {
                        return Err(parser_error("Recursive CTEs with COPY statements are not supported"));
                    }
                    return Err(parser_error("Recursive CTEs with DML statements are not supported"));
                }
                kind.into()
            } else {
                self.select_internal(self.req(body, "SelectStatementInternal")?)?.node
            };
            if recursive {
                query = to_recursive_cte(query, &name, &aliases, &key_targets)?;
            }
            if map.0.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                return Err(parser_error(format!("Duplicate CTE name {name}")));
            }
            map.0.push((
                name,
                CteInfo {
                    aliases,
                    query: Box::new(query),
                    key_targets,
                    materialized,
                },
            ));
        }
        Ok(map)
    }

    fn order_by_clause(&mut self, node: &ParseNode) -> Result<Vec<OrderByNode>> {
        let exprs = self.alt(self.req(node, "OrderByExpressions")?)?;
        if self.name(exprs) == "OrderByAll" {
            let (order, nulls) = self.order_modifiers(exprs)?;
            let mut star = Expr::star();
            if let ExprKind::Star(s) = &mut star.kind {
                s.columns = true;
            }
            return Ok(vec![OrderByNode {
                order,
                nulls,
                expr: star,
            }]);
        }
        let mut orders = Vec::new();
        for o in self.subs(exprs, "OrderByExpression") {
            let expr = self.expr(self.req(o, "Expression")?)?;
            let (order, nulls) = self.order_modifiers(o)?;
            orders.push(OrderByNode { order, nulls, expr });
        }
        Ok(orders)
    }

    fn order_modifiers(&self, node: &ParseNode) -> Result<(OrderType, NullOrder)> {
        let order = match self.sub(node, "DescOrAsc") {
            Some(d) if self.name(self.alt(d)?) == "DescendingOrder" => OrderType::Descending,
            Some(_) => OrderType::Ascending,
            None => OrderType::Default,
        };
        let nulls = match self.sub(node, "NullsFirstOrLast") {
            Some(n) if self.name(self.alt(n)?) == "NullsFirst" => NullOrder::NullsFirst,
            Some(_) => NullOrder::NullsLast,
            None => NullOrder::Default,
        };
        Ok((order, nulls))
    }

    fn limit_value(&mut self, node: &ParseNode) -> Result<LimitValue> {
        Ok(match self.name(node) {
            "LimitClause" => {
                let value = self.alt(self.req(node, "LimitValue")?)?;
                match self.name(value) {
                    "LimitAll" => LimitValue {
                        expr: Some(Expr::constant(Literal::null())),
                        percent: false,
                    },
                    "LimitLiteralPercent" => LimitValue {
                        expr: Some(Expr::constant(Literal::number(Self::text(value)))),
                        percent: true,
                    },
                    _ => LimitValue {
                        expr: Some(self.expr(self.req(value, "Expression")?)?),
                        percent: self.has(value, "%"),
                    },
                }
            }
            "OffsetClause" => LimitValue {
                expr: Some(self.expr(self.req(self.req(node, "OffsetValue")?, "Expression")?)?),
                percent: false,
            },
            _ => {
                let fetch = self.alt(node)?;
                let expr = match self.sub(fetch, "FetchValue") {
                    Some(v) => self.expr(v)?,
                    None => Expr::constant(Literal::integer(1)),
                };
                LimitValue {
                    expr: Some(expr),
                    percent: false,
                }
            }
        })
    }

    fn limit_offset(&mut self, node: &ParseNode) -> Result<Option<ResultModifier>> {
        let none = || LimitValue {
            expr: None,
            percent: false,
        };
        let mut limit = none();
        let mut offset = none();
        for part in Self::parts(node) {
            match self.name(part) {
                "LimitClause" | "FetchClause" => limit = self.limit_value(part)?,
                "OffsetClause" => offset = self.limit_value(part)?,
                _ => {}
            }
        }
        if offset.percent {
            return Err(parser_error("Percentage for offsets are not supported."));
        }
        if limit.expr.is_none() && offset.expr.is_none() {
            return Ok(None);
        }
        Ok(Some(ResultModifier::Limit {
            limit: limit.expr.map(Box::new),
            offset: offset.expr.map(Box::new),
            percent: limit.percent,
        }))
    }

    fn group_by(&mut self, node: &ParseNode) -> Result<GroupByNode> {
        if self.name(node) == "GroupByAll" {
            return Ok(GroupByNode {
                group_expressions: vec![Expr::star()],
                grouping_sets: Vec::new(),
            });
        }
        let mut result = GroupByNode::default();
        for item in self.subs(node, "GroupByExpression") {
            let item = self.group_by_item(item)?;
            let next = group_by_unfolding(item, &mut result)?;
            if result.grouping_sets.is_empty() {
                result.grouping_sets = next;
            } else {
                let count = result.grouping_sets.len() * next.len();
                check_grouping_set_max(count)?;
                let mut combined = Vec::with_capacity(count);
                for current in &result.grouping_sets {
                    for n in &next {
                        combined.push(current.union(n).copied().collect());
                    }
                }
                result.grouping_sets = combined;
            }
        }
        Ok(result)
    }

    fn group_by_item(&mut self, node: &ParseNode) -> Result<GroupByItem> {
        let inner = self.alt(node)?;
        Ok(match self.name(inner) {
            "EmptyGroupingItem" => GroupByItem::Empty,
            "CubeOrRollupClause" => {
                let exprs = self.exprs(inner, "Expression")?;
                if self.name(self.alt(self.req(inner, "CubeOrRollup")?)?) == "CubeKeyword" {
                    GroupByItem::Cube(exprs)
                } else {
                    GroupByItem::Rollup(exprs)
                }
            }
            "GroupingSetsClause" => {
                let mut children = Vec::new();
                for child in self.subs(inner, "GroupByExpression") {
                    children.push(self.group_by_item(child)?);
                }
                GroupByItem::GroupingSets(children)
            }
            _ => GroupByItem::Expression(self.expr(inner)?),
        })
    }

    fn transform_from(&mut self, node: &ParseNode) -> Result<TableRef> {
        let mut refs = Vec::new();
        for r in self.subs(node, "TableRef") {
            refs.push(self.table_ref(r)?);
        }
        let mut iter = refs.into_iter();
        let mut result = iter.next().expect("FROM has a table reference");
        for right in iter {
            result = TableRefKind::Join(JoinRef {
                left: Box::new(result),
                right: Box::new(right),
                condition: None,
                using_columns: Vec::new(),
                join_type: JoinType::Inner,
                ref_type: JoinRefType::Cross,
                is_implicit: true,
                nearest_count: 0,
                nearest_descending: false,
                nearest_approx: false,
                ranking_expression: None,
            })
            .into();
        }
        Ok(result)
    }

    fn table_ref(&mut self, node: &ParseNode) -> Result<TableRef> {
        let mut table = self.inner_table_ref(self.alt(self.req(node, "InnerTableRef")?)?)?;
        for join in self.subs(node, "JoinOrPivot") {
            let inner = self.alt(join)?;
            if self.name(inner) == "TableMatchRecognizeClause" {
                table = self.table_match_recognize(inner, table)?;
                continue;
            }
            if matches!(self.name(inner), "TablePivotClause" | "TableUnpivotClause") {
                table = self.table_pivot(inner, table)?;
                continue;
            }
            if self.name(inner) != "JoinClause" {
                return Err(not_implemented(format!(
                    "the transformer for {} is not yet ported",
                    self.name(inner)
                )));
            }
            let clause = self.alt(inner)?;
            let mut j = if self.name(clause) == "NearestJoinClause" {
                self.nearest_join(clause)?
            } else {
                self.join_clause(clause)?
            };
            j.left = Box::new(table);
            table = TableRefKind::Join(j).into();
        }
        Ok(table)
    }

    fn table_alias(&self, node: &ParseNode) -> Result<Option<(String, Vec<String>)>> {
        let Some(alias) = self.sub(node, "TableAlias") else {
            return Ok(None);
        };
        let inner = self.alt(alias)?;
        let columns = match self.sub(inner, "ColumnAliases") {
            Some(c) => self.texts(c, "ColIdOrString"),
            None => Vec::new(),
        };
        let name_rule = if self.name(inner) == "TableAliasAs" {
            "IdentifierOrStringLiteral"
        } else {
            "Identifier"
        };
        Ok(Some((Self::text(self.req(inner, name_rule)?), columns)))
    }

    /// Applies the aliases and sample shared by the table reference forms.
    fn decorate(&mut self, node: &ParseNode, mut table: TableRef, describe: &str) -> Result<TableRef> {
        let colon = self.sub(node, "TableAliasColon").map(Self::text);
        let alias = self.table_alias(node)?;
        if colon.is_some() && alias.is_some() {
            return Err(parser_error(format!(
                "Table reference {describe}cannot have two aliases"
            )));
        }
        if let Some(c) = colon {
            table.alias = c;
        }
        if let Some((name, columns)) = alias {
            table.alias = name;
            table.column_name_alias = columns;
        }
        if let Some(sample) = self.sub(node, "SampleClause") {
            table.sample = Some(self.sample_entry(self.req(sample, "SampleEntry")?)?);
        }
        Ok(table)
    }

    fn inner_table_ref(&mut self, node: &ParseNode) -> Result<TableRef> {
        match self.name(node) {
            "BaseTableRef" => {
                let mut table = self.base_table_name(self.req(node, "BaseTableName")?)?;
                if let Some(at) = self.sub(node, "AtClause") {
                    let spec = self.req(at, "AtSpecifier")?;
                    let unit = if self.name(self.alt(self.req(spec, "AtUnit")?)?) == "VersionAtUnit" {
                        "VERSION"
                    } else {
                        "TIMESTAMP"
                    };
                    let expr = self.expr(self.req(spec, "Expression")?)?;
                    if let TableRefKind::BaseTable { at_clause, .. } = &mut table.kind {
                        *at_clause = Some(AtClause {
                            unit: unit.into(),
                            expr: Box::new(expr),
                        });
                    }
                }
                let describe = format!("{table} ");
                self.decorate(node, table, &describe)
            }
            "TableSubquery" => {
                let lateral = self.sub(node, "Lateral").is_some();
                if lateral && self.sub(node, "SampleClause").is_some() {
                    return Err(parser_error("TABLESAMPLE cannot be applied to a LATERAL subquery"));
                }
                let s = self.subquery_reference(self.req(node, "SubqueryReference")?)?;
                let table = TableRef::subquery(s);
                let describe = format!("{table} ");
                self.decorate(node, table, &describe)
            }
            "ValuesRef" => {
                let values = self.values_clause(self.req(node, "ValuesClause")?)?;
                self.decorate(node, TableRef::subquery(values), "")
            }
            "ParensTableRef" => {
                let inner = self.table_ref(self.req(node, "TableRef")?)?;
                if self.sub(node, "TableAliasColon").is_none()
                    && self.sub(node, "TableAlias").is_none()
                    && self.sub(node, "SampleClause").is_none()
                {
                    return Ok(inner);
                }
                let describe = format!("{inner} ");
                let select = SelectNode {
                    select_list: vec![Expr::star()],
                    from_table: inner,
                    ..SelectNode::default()
                };
                let table = TableRef::subquery(SelectStatement { node: select.into() });
                self.decorate(node, table, &describe)
            }
            "TableFunction" => {
                let inner = self.alt(node)?;
                if self.sub(inner, "Lateral").is_some() && self.sub(inner, "SampleClause").is_some() {
                    return Err(parser_error(
                        "TABLESAMPLE cannot be applied to a LATERAL table function",
                    ));
                }
                let qualified = self.req(inner, "QualifiedTableFunction")?;
                let name = QualifiedName::from_parts(self.rule_texts(qualified));
                let mut args = Vec::new();
                for arg in self.subs(self.req(inner, "TableFunctionArguments")?, "FunctionArgument") {
                    args.push(self.function_argument(arg)?);
                }
                let mut function = FunctionExpr::new("", Vec::new());
                function.name = name;
                function.args = args;
                let table = TableRef::from(TableRefKind::TableFunction {
                    function: Box::new(function.into()),
                    with_ordinality: self.sub(inner, "WithOrdinality").is_some(),
                });
                self.decorate(inner, table, "")
            }
            other => Err(not_implemented(format!(
                "the transformer for {other} is not yet ported"
            ))),
        }
    }

    fn base_table_name(&self, node: &ParseNode) -> Result<TableRef> {
        let inner = self.alt(node)?;
        let parts = if self.name(inner) == "QualifiedTableName" {
            self.rule_texts(self.alt(inner)?)
        } else {
            vec![Self::text(inner)]
        };
        Ok(TableRefKind::BaseTable {
            name: QualifiedName::from_parts(parts),
            at_clause: None,
        }
        .into())
    }

    fn join_type(&self, node: Option<&ParseNode>) -> Result<JoinType> {
        let Some(node) = node else {
            return Ok(JoinType::Inner);
        };
        Ok(match self.name(self.alt(node)?) {
            "FullJoin" => JoinType::Outer,
            "LeftJoin" => JoinType::Left,
            "RightJoin" => JoinType::Right,
            "SemiJoin" => JoinType::Semi,
            "AntiJoin" => JoinType::Anti,
            _ => JoinType::Inner,
        })
    }

    fn join_clause(&mut self, node: &ParseNode) -> Result<JoinRef> {
        let mut join = JoinRef {
            left: Box::new(TableRef::empty()),
            right: Box::new(TableRef::empty()),
            condition: None,
            using_columns: Vec::new(),
            join_type: JoinType::Inner,
            ref_type: JoinRefType::Regular,
            is_implicit: false,
            nearest_count: 0,
            nearest_descending: false,
            nearest_approx: false,
            ranking_expression: None,
        };
        match self.name(node) {
            "RegularJoinClause" | "JoinByClause" => {
                if self.name(node) == "JoinByClause" {
                    let label = Self::text(self.req(node, "ColLabel")?);
                    let base = if label.len() > 5 && label[label.len() - 5..].eq_ignore_ascii_case("_join") {
                        &label[..label.len() - 5]
                    } else {
                        &label
                    };
                    join.join_type = JoinType::from_name(base)
                        .ok_or_else(|| parser_error(format!("\"{label}\" is not a valid join type for JOIN BY")))?;
                } else {
                    if self.sub(node, "Asof").is_some() {
                        join.ref_type = JoinRefType::Asof;
                    }
                    join.join_type = self.join_type(self.sub(node, "JoinType"))?;
                }
                join.right = Box::new(self.table_ref(self.req(node, "TableRef")?)?);
                let qualifier = self.alt(self.req(node, "JoinQualifier")?)?;
                if self.name(qualifier) == "OnClause" {
                    join.condition = Some(Box::new(self.expr(self.req(qualifier, "Expression")?)?));
                } else {
                    for column in self.subs(qualifier, "UsingColumnName") {
                        let name = Self::text(column);
                        if name.is_empty() {
                            return Err(parser_error("Column identifier cannot be empty"));
                        }
                        join.using_columns.push(name);
                    }
                }
            }
            "JoinWithoutOnClause" => {
                let prefix = self.alt(self.req(node, "JoinPrefix")?)?;
                match self.name(prefix) {
                    "CrossJoinPrefix" => join.ref_type = JoinRefType::Cross,
                    "NaturalJoinPrefix" => {
                        join.ref_type = JoinRefType::Natural;
                        join.join_type = self.join_type(self.sub(prefix, "JoinType"))?;
                    }
                    _ => join.ref_type = JoinRefType::Positional,
                }
                join.right = Box::new(self.inner_table_ref(self.alt(self.req(node, "InnerTableRef")?)?)?);
            }
            other => {
                return Err(not_implemented(format!(
                    "the transformer for {other} is not yet ported"
                )));
            }
        }
        Ok(join)
    }

    fn sample_entry(&mut self, node: &ParseNode) -> Result<SampleOptions> {
        let entry = self.alt(node)?;
        let mut options = self.sample_count(self.req(entry, "SampleCount")?)?;
        if self.name(entry) == "SampleEntryFunction" {
            if let Some(f) = self.sub(entry, "SampleFunction") {
                options.method = sample_method(&Self::text(f))?;
            }
            if let Some(r) = self.sub(entry, "RepeatableSample") {
                options.seed = Some(self.sample_seed(self.req(r, "SampleSeed")?)?);
                options.repeatable = true;
            }
        } else if let Some(props) = self.sub(entry, "SampleProperties") {
            options.method = sample_method(&Self::text(self.req(props, "ColId")?))?;
            if let Some(seed) = self.sub(props, "SampleSeed") {
                options.seed = Some(self.sample_seed(seed)?);
                options.repeatable = true;
            } else {
                options.seed = None;
            }
        }
        Ok(options)
    }

    fn sample_seed(&self, node: &ParseNode) -> Result<u64> {
        let text = Self::text(node);
        let digits = text.replace('_', "");
        let seed: u64 = match digits.parse::<u64>() {
            Ok(v) => v,
            Err(_) => digits
                .parse::<f64>()
                .map(|v| v as u64)
                .map_err(|_| parser_error(format!("Invalid sample seed {text}")))?,
        };
        if seed > i64::MAX as u64 {
            return Err(parser_error(format!(
                "REPEATABLE seed {seed} is out of range, the maximum is {}",
                i64::MAX
            )));
        }
        Ok(seed)
    }

    fn sample_count(&mut self, node: &ParseNode) -> Result<SampleOptions> {
        let value = self.expr(self.alt(self.req(node, "SampleValue")?)?)?;
        let ExprKind::Constant(literal) = &value.kind else {
            return Err(parser_error("Only constants are supported in sample clause currently"));
        };
        let is_percentage = match self.sub(node, "SampleUnit") {
            Some(u) => self.name(self.alt(u)?) == "SamplePercentage",
            None => false,
        };
        let number: f64 = literal
            .text
            .replace('_', "")
            .parse()
            .map_err(|_| parser_error(format!("Invalid sample size {literal}")))?;
        if is_percentage {
            if !(0.0..=100.0).contains(&number) {
                return Err(parser_error(format!(
                    "Sample sample_size {number:.6} out of range, must be between 0 and 100"
                )));
            }
            let size = duckrs_common::value::Value::double(number).try_to_string()?;
            Ok(SampleOptions {
                sample_size: size,
                is_percentage,
                method: SampleMethod::System,
                seed: None,
                repeatable: false,
            })
        } else {
            // `SampleOptions::MAX_SAMPLE_ROWS`
            const MAX_SAMPLE_ROWS: i64 = 1_000_000_000;
            let rows = number as i64;
            if !(0..=MAX_SAMPLE_ROWS).contains(&rows) {
                return Err(parser_error(format!(
                    "Sample rows {rows} out of range, must be between 0 and {MAX_SAMPLE_ROWS}"
                )));
            }
            Ok(SampleOptions {
                sample_size: rows.to_string(),
                is_percentage,
                method: SampleMethod::Reservoir,
                seed: None,
                repeatable: false,
            })
        }
    }
}

fn chained_comparison() -> Error {
    parser_error("Chained comparisons are not supported, use AND to combine comparisons")
}

fn apply_nots(count: usize, mut expr: Expr) -> Expr {
    for _ in 0..count {
        expr = Expr::not_op(expr);
    }
    expr
}

fn case(checks: Vec<(Expr, Expr)>, else_expr: Option<Expr>) -> Expr {
    ExprKind::Case {
        checks,
        else_expr: Box::new(else_expr.unwrap_or_else(|| Expr::constant(Literal::null()))),
    }
    .into()
}

fn subquery(statement: SelectStatement, subquery_type: SubqueryType, child: Option<Expr>, op: ComparisonOp) -> Expr {
    ExprKind::Subquery {
        subquery: Box::new(statement),
        subquery_type,
        child: child.map(Box::new),
        comparison: op,
    }
    .into()
}

fn type_expr(name: &str, children: Vec<Expr>) -> Expr {
    ExprKind::Type(TypeExpr {
        name: QualifiedName::new(name),
        children,
    })
    .into()
}

fn empty_slice_bound() -> Expr {
    FunctionExpr::new("list_value", Vec::new()).into()
}

fn is_unqualified_star(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Star(s) if s.relation_name.is_empty()) && expr.is_empty_star()
}

fn strip_count_star(args: &mut Vec<FunctionArgument>) {
    if args.len() == 1 && is_unqualified_star(&args[0].expr) {
        args.clear();
    }
}

fn check_copyable_window(window: &WindowExpr, name: &str) -> Result<()> {
    if window.start_expr.is_some()
        || window.end_expr.is_some()
        || window.start != WindowBoundary::UnboundedPreceding
        || window.end != WindowBoundary::CurrentRowRange
    {
        return Err(parser_error(format!(
            "cannot copy window \"{name}\" because it has a frame clause"
        )));
    }
    Ok(())
}

fn time_type_name(is_time: bool, with_tz: bool, modifiers: &[Expr]) -> Result<&'static str> {
    if is_time {
        if !modifiers.is_empty() {
            return Err(parser_error("Type TIME does not allow any modifiers"));
        }
        return Ok(if with_tz { "TIME WITH TIME ZONE" } else { "TIME" });
    }
    if modifiers.is_empty() {
        return Ok(if with_tz {
            "TIMESTAMP WITH TIME ZONE"
        } else {
            "TIMESTAMP"
        });
    }
    if with_tz {
        return Err(parser_error("TIMESTAMP WITH TIME ZONE does not take a precision"));
    }
    if modifiers.len() > 1 {
        return Err(parser_error("TIMESTAMP only supports a single modifier"));
    }
    let ExprKind::Constant(literal) = &modifiers[0].kind else {
        return Err(parser_error("Expected a constant expression for timestamp precision"));
    };
    if literal.kind == LiteralKind::Null {
        return Err(parser_error("TIMESTAMP precision cannot be NULL"));
    }
    let Some(precision) = literal.try_get_i64() else {
        return Err(parser_error("TIMESTAMP precision must be an integral type"));
    };
    Ok(match precision {
        p if p > 10 => return Err(parser_error("TIMESTAMP only supports until nano-second precision (9)")),
        p if p < 0 => {
            return Err(parser_error(
                "TIMESTAMP precision should be between 0 and 10 (inclusive)",
            ));
        }
        0 => "TIMESTAMP_S",
        1..=3 => "TIMESTAMP_MS",
        4..=6 => "TIMESTAMP",
        _ => "TIMESTAMP_NS",
    })
}

fn sample_method(name: &str) -> Result<SampleMethod> {
    [SampleMethod::System, SampleMethod::Bernoulli, SampleMethod::Reservoir]
        .into_iter()
        .find(|m| m.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Conversion,
                format!("Enum value: '{name}' not found in SampleMethod"),
            )
        })
}

/// A string literal: E'' strings are unescaped, X'' and B'' become hex and bit literals, and N''
/// strings are cast to VARCHAR.
fn string_literal_expr(node: &ParseNode) -> Result<Expr> {
    let Some(TerminalValue::String { value, prefix }) = Transformer::terminal(node) else {
        return Err(Error::new(ErrorKind::Internal, "expected a string literal"));
    };
    let value = value.clone();
    Ok(match prefix.map(|c| c.to_ascii_uppercase()) {
        Some('N') => Expr::cast(Expr::constant(Literal::string(value)), CastType::Builtin("VARCHAR")),
        Some('X') => {
            if value.len() % 2 != 0 {
                return Err(parser_error(
                    "Hex string literal must have an even number of hex digits",
                ));
            }
            if let Some(c) = value.chars().find(|c| !c.is_ascii_hexdigit()) {
                return Err(parser_error(format!(
                    "Hex string literal contains a non-hexadecimal character '{c}'"
                )));
            }
            Expr::constant(Literal {
                kind: LiteralKind::Hex,
                text: value,
            })
        }
        Some('B') => {
            if let Some(c) = value.chars().find(|&c| c != '0' && c != '1') {
                return Err(parser_error(format!(
                    "Bit string literal contains a non-binary character '{c}'"
                )));
            }
            Expr::constant(Literal {
                kind: LiteralKind::Bit,
                text: value,
            })
        }
        Some('E') => Expr::constant(Literal::string(unescape(&value)?)),
        _ => Expr::constant(Literal::string(value)),
    })
}

/// Decodes the backslash escapes of an E'' string (`StringLiteralParseResult::ToExpression`).
fn unescape(input: &str) -> Result<String> {
    let b = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'0'..=b'7' => {
                    let start = i;
                    let mut end = start + 1;
                    while end < b.len() && end - start < 3 && (b'0'..=b'7').contains(&b[end]) {
                        end += 1;
                    }
                    let v = u32::from_str_radix(&input[start..end], 8).unwrap_or(0);
                    out.push(v as u8);
                    i = end - 1;
                }
                b'x' => {
                    let start = i + 1;
                    let mut end = start;
                    while end < b.len() && end - start < 2 && b[end].is_ascii_hexdigit() {
                        end += 1;
                    }
                    if end > start {
                        out.push(u8::from_str_radix(&input[start..end], 16).unwrap_or(0));
                        i = end - 1;
                    } else {
                        out.push(b'x');
                    }
                }
                c => out.push(c),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    if out.contains(&0) {
        return Err(parser_error("Null character not permitted in escape string literal"));
    }
    String::from_utf8(out).map_err(|e| {
        parser_error(format!(
            "Invalid UTF-8 in escape string literal at byte offset {}: byte mismatch",
            e.utf8_error().valid_up_to()
        ))
    })
}

/// `ApplyIsTest`.
fn apply_is_test(expr: Expr, test: Expr) -> Expr {
    match test.kind {
        ExprKind::Comparison { op, right, .. } => {
            Expr::comparison(op, Expr::cast(expr, CastType::Builtin("BOOLEAN")), *right)
        }
        ExprKind::Operator { op, mut children } => {
            children.insert(0, expr);
            Expr::operator(op, children)
        }
        _ => expr,
    }
}

/// `ApplyBetweenInLikeOperator`: `pending` is a BETWEEN, LIKE-style function, IN operator or ANY
/// subquery still missing its left-hand side.
fn apply_between_in_like(expr: Expr, pending: Expr, negated: bool) -> Expr {
    match pending.kind {
        ExprKind::Between { lower, upper, .. } => {
            let between: Expr = ExprKind::Between {
                input: Box::new(expr),
                lower,
                upper,
            }
            .into();
            if negated { Expr::not_op(between) } else { between }
        }
        ExprKind::Function(mut f) => {
            if f.name.name == "contains" {
                f.args.push(FunctionArgument::positional(expr));
            } else {
                f.args.insert(0, FunctionArgument::positional(expr));
            }
            let mut name = f.name.name.clone();
            let regex_negated = name == "!regexp_matches" || name == "!regexp_full_match";
            if regex_negated {
                name.remove(0);
            }
            let negated_like = negated && !regex_negated && (name == "~~" || name == "~~*");
            if negated_like {
                name.insert(0, '!');
            }
            f.name.name = name;
            let f: Expr = f.into();
            if negated {
                if regex_negated || negated_like {
                    f
                } else {
                    Expr::not_op(f)
                }
            } else if regex_negated {
                Expr::not_op(f)
            } else {
                f
            }
        }
        ExprKind::Operator { op, mut children } => {
            children.insert(0, expr);
            let e = Expr::operator(op, children);
            if negated { Expr::not_op(e) } else { e }
        }
        ExprKind::Subquery {
            subquery: s,
            subquery_type,
            comparison,
            ..
        } => {
            let e = subquery(*s, subquery_type, Some(expr), comparison);
            if negated { Expr::not_op(e) } else { e }
        }
        _ => expr,
    }
}

/// `TransformOperatorFunction`: an operator call, optionally `schema.op`.
fn operator_function(name: &str, children: Vec<Expr>) -> Result<Expr> {
    let parts: Vec<&str> = name.split('.').collect();
    let (schema, function) = match parts.as_slice() {
        [f] => ("", *f),
        [s, f] => (*s, *f),
        _ => {
            return Err(parser_error(
                "Too many identifiers found, expected schema.operator or operator",
            ));
        }
    };
    let mut f = FunctionExpr::operator(function, children);
    f.name.qualification = vec![String::new(), schema.to_string()];
    Ok(f.into())
}

/// `TransformInfixOtherOperatorExpression`.
fn apply_other_operators(mut expr: Expr, tails: Vec<(ParsedOperator, Expr)>, regex_full_match: bool) -> Result<Expr> {
    for (op, right) in tails {
        let Some(is_any) = op.any_all else {
            expr = operator_function(&op.name, vec![expr, right])?;
            continue;
        };
        let comparison = ComparisonOp::from_operator(&op.name);
        if let ExprKind::Subquery { subquery: s, .. } = right.kind {
            let Some(cmp) = comparison else {
                return Err(parser_error(
                    "ANY and ALL operators require one of =,<>,>,<,>=,<= comparisons!",
                ));
            };
            if !is_any {
                return Ok(Expr::not_op(subquery(*s, SubqueryType::Any, Some(expr), cmp.negate())));
            }
            expr = subquery(*s, SubqueryType::Any, Some(expr), cmp);
            continue;
        }
        let regex = match op.name.as_str() {
            "~" => Some((false, false)),
            "!~" => Some((true, false)),
            "~*" => Some((false, true)),
            "!~*" => Some((true, true)),
            _ => None,
        };
        if let Some((negated, case_insensitive)) = regex {
            expr = regex_any_all(expr, right, negated, case_insensitive, is_any, regex_full_match);
            continue;
        }
        let Some(cmp) = comparison else {
            return Err(parser_error(format!(
                "Unsupported comparison \"{}\" for ANY/ALL subquery",
                op.name
            )));
        };
        let unnest = SelectNode {
            select_list: vec![FunctionExpr::new("UNNEST", vec![right]).into()],
            ..SelectNode::default()
        };
        let statement = SelectStatement { node: unnest.into() };
        if !is_any {
            return Ok(Expr::not_op(subquery(
                statement,
                SubqueryType::Any,
                Some(expr),
                cmp.negate(),
            )));
        }
        return Ok(subquery(statement, SubqueryType::Any, Some(expr), cmp));
    }
    Ok(expr)
}

/// `TransformRegexAnyAllList`.
fn regex_any_all(
    left: Expr,
    right: Expr,
    negated: bool,
    case_insensitive: bool,
    is_any: bool,
    regex_full_match: bool,
) -> Expr {
    let mut children = vec![left, Expr::column(vec!["__regex_pattern".into()])];
    if case_insensitive {
        children.push(Expr::constant(Literal::string("i")));
    }
    let function = if regex_full_match { "regexp_full_match" } else { REGEX_MATCH_FUNCTION };
    let mut regex_match: Expr = FunctionExpr::new(function, children).into();
    if negated {
        regex_match = Expr::not_op(regex_match);
    }
    let lambda = Expr::lambda(&["__regex_pattern".to_string()], regex_match);
    let match_list: Expr = FunctionExpr::new("list_transform", vec![right, lambda]).into();
    let contains = FunctionExpr::new(
        "list_contains",
        vec![match_list.clone(), Expr::constant(Literal::boolean(is_any))],
    );
    let is_null = Expr::operator(OperatorType::IsNull, vec![Expr::column(vec!["__regex_match".into()])]);
    let null_filter = FunctionExpr::new(
        "list_filter",
        vec![match_list, Expr::lambda(&["__regex_match".to_string()], is_null)],
    );
    let has_null = Expr::comparison(
        ComparisonOp::GreaterThan,
        FunctionExpr::new("len", vec![null_filter.into()]).into(),
        Expr::constant(Literal::integer(0)),
    );
    case(
        vec![
            (contains.into(), Expr::constant(Literal::boolean(is_any))),
            (has_null, Expr::constant(Literal::null())),
        ],
        Some(Expr::constant(Literal::boolean(!is_any))),
    )
}

/// `GetParserFunctionRewrite`: whether the parser turns calls of `name` into a syntax node.
fn is_function_rewrite(name: &str) -> bool {
    matches!(
        name,
        "unpack" | "try" | "construct_array" | "ifnull" | "coalesce" | "date"
    )
}

/// `TransformMethodCall`: a call on a value, as `x.f(y)`, is the call `f(x, y)` with the rewrites
/// of a method call.
fn method_call(value: Expr, mut function: FunctionExpr) -> Result<Expr> {
    function.args.insert(0, FunctionArgument::positional(value));
    let lower = function.name.name.to_lowercase();
    let Some(rewritten) = function_rewrite(&lower, &mut function.args, true)? else {
        return Ok(function.into());
    };
    // a rewrite yields a syntax node with nowhere to put the modifiers
    let upper = lower.to_uppercase();
    if function.distinct {
        return Err(parser_error(format!(
            "DISTINCT is not supported in {upper} expressions"
        )));
    }
    if !function.order_by.is_empty() {
        return Err(parser_error(format!(
            "ORDER BY is not supported in {upper} expressions"
        )));
    }
    if function.filter.is_some() {
        return Err(parser_error(format!("FILTER is not supported in {upper} expressions")));
    }
    Ok(rewritten)
}

/// `TransformParserFunctionRewrite`: names the parser turns into syntax nodes.
fn function_rewrite(name: &str, args: &mut Vec<FunctionArgument>, method_chain: bool) -> Result<Option<Expr>> {
    let reject_named = |args: &[FunctionArgument], context: &str| -> Result<()> {
        if args.iter().any(|a| !a.name.is_empty()) {
            return Err(parser_error(format!("Named arguments are not supported in {context}")));
        }
        Ok(())
    };
    let take = |args: &mut Vec<FunctionArgument>| -> Vec<Expr> { args.drain(..).map(|a| a.expr).collect() };
    Ok(Some(match name {
        "unpack" => {
            if args.len() != 1 {
                return Err(parser_error("Wrong number of arguments to the UNPACK operator"));
            }
            reject_named(args, "UNPACK operator")?;
            Expr::operator(OperatorType::Unpack, take(args))
        }
        "try" => {
            if args.len() != 1 {
                return Err(parser_error("Wrong number of arguments provided to TRY expression"));
            }
            reject_named(args, "TRY expression")?;
            Expr::operator(OperatorType::Try, take(args))
        }
        "construct_array" => {
            reject_named(args, "array constructors")?;
            Expr::operator(OperatorType::ArrayConstructor, take(args))
        }
        "ifnull" => {
            if args.len() != 2 {
                return Err(parser_error("Wrong number of arguments to IFNULL."));
            }
            reject_named(args, "IFNULL expressions")?;
            Expr::operator(OperatorType::Coalesce, take(args))
        }
        "coalesce" if method_chain => {
            reject_named(args, "COALESCE expressions")?;
            Expr::operator(OperatorType::Coalesce, take(args))
        }
        "date" => {
            if args.len() != 1 {
                return Err(parser_error("Wrong number of arguments provided to DATE function"));
            }
            let child = args.pop().unwrap().expr;
            Expr::cast(child, CastType::Builtin("DATE"))
        }
        _ => return Ok(None),
    }))
}

/// `RemoveOrderQualificationRecursive`: keeps only the column name of qualified references.
fn remove_order_qualification(expr: &mut Expr) {
    visit_expr_mut(expr, &mut |e| {
        if let ExprKind::ColumnRef(names) = &mut e.kind
            && names.len() > 1
        {
            let last = names.pop().unwrap();
            *names = vec![last];
        }
    });
}

/// Visits `expr` and every expression nested in it (not descending into subqueries).
pub fn visit_expr_mut(expr: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    f(expr);
    let mut visit = |e: &mut Expr| visit_expr_mut(e, f);
    match &mut expr.kind {
        ExprKind::Function(func) => {
            func.args.iter_mut().for_each(|a| visit(&mut a.expr));
            if let Some(filter) = &mut func.filter {
                visit(filter);
            }
            func.order_by.iter_mut().for_each(|o| visit(&mut o.expr));
        }
        ExprKind::Window(w) => {
            w.args.iter_mut().for_each(|a| visit(&mut a.expr));
            w.partitions.iter_mut().for_each(&mut visit);
            w.orders.iter_mut().for_each(|o| visit(&mut o.expr));
            w.arg_orders.iter_mut().for_each(|o| visit(&mut o.expr));
            for e in [&mut w.start_expr, &mut w.end_expr, &mut w.filter]
                .into_iter()
                .flatten()
            {
                visit(e);
            }
        }
        ExprKind::Comparison { left, right, .. } => {
            visit(left);
            visit(right);
        }
        ExprKind::Conjunction { children, .. } | ExprKind::Operator { children, .. } => {
            children.iter_mut().for_each(visit)
        }
        ExprKind::Between { input, lower, upper } => {
            visit(input);
            visit(lower);
            visit(upper);
        }
        ExprKind::Case { checks, else_expr } => {
            for (w, t) in checks {
                visit(w);
                visit(t);
            }
            visit(else_expr);
        }
        ExprKind::Cast { child, .. } | ExprKind::Collate { child, .. } => visit(child),
        ExprKind::Subquery { child: Some(child), .. } => visit(child),
        ExprKind::Lambda { lhs, expr, .. } => {
            visit(lhs);
            visit(expr);
        }
        ExprKind::Concatenation(parts) => parts.iter_mut().for_each(visit),
        ExprKind::Quantified { child, .. } => visit(child),
        ExprKind::Alternation(left, right) => {
            visit(left);
            visit(right);
        }
        ExprKind::Star(star) => {
            star.replace_list.iter_mut().for_each(|(_, e)| visit(e));
            if let Some(e) = &mut star.expr {
                visit(e);
            }
        }
        _ => {}
    }
}

/// Calls `f` on every expression of a SELECT, like [`visit_expr_mut`]
/// (`ParsedExpressionIterator::EnumerateQueryNodeChildren`).
pub fn visit_query_node_exprs_mut(node: &mut QueryNode, f: &mut impl FnMut(&mut Expr)) {
    if let QueryNodeKind::Select(select) = &mut node.kind {
        select.select_list.iter_mut().for_each(|e| visit_expr_mut(e, f));
        visit_table_ref_exprs_mut(&mut select.from_table, f);
        for clause in [&mut select.where_clause, &mut select.having, &mut select.qualify]
            .into_iter()
            .flatten()
        {
            visit_expr_mut(clause, f);
        }
        select
            .groups
            .group_expressions
            .iter_mut()
            .for_each(|e| visit_expr_mut(e, f));
    }
    for modifier in &mut node.modifiers {
        match modifier {
            ResultModifier::Order(nodes) => nodes.iter_mut().for_each(|n| visit_expr_mut(&mut n.expr, f)),
            ResultModifier::Limit { limit, offset, .. } => {
                for e in [limit, offset].into_iter().flatten() {
                    visit_expr_mut(e, f);
                }
            }
            ResultModifier::Distinct(targets) => targets.iter_mut().for_each(|e| visit_expr_mut(e, f)),
        }
    }
}

/// [`visit_query_node_exprs_mut`] that also goes into the WITH clause, the children of set
/// operations and recursive CTEs, and the same in the subqueries of the FROM clause
/// (`ParsedExpressionIterator::EnumerateQueryNodeChildren`).
pub fn visit_all_query_node_exprs_mut(node: &mut QueryNode, f: &mut impl FnMut(&mut Expr)) {
    for (_, cte) in &mut node.cte_map.0 {
        visit_all_query_node_exprs_mut(&mut cte.query, f);
    }
    match &mut node.kind {
        QueryNodeKind::Select(select) => {
            select.select_list.iter_mut().for_each(|e| visit_expr_mut(e, f));
            visit_all_table_ref_exprs_mut(&mut select.from_table, f);
            for clause in [&mut select.where_clause, &mut select.having, &mut select.qualify]
                .into_iter()
                .flatten()
            {
                visit_expr_mut(clause, f);
            }
            select
                .groups
                .group_expressions
                .iter_mut()
                .for_each(|e| visit_expr_mut(e, f));
        }
        QueryNodeKind::SetOperation(set) => {
            set.children
                .iter_mut()
                .for_each(|child| visit_all_query_node_exprs_mut(child, f));
        }
        QueryNodeKind::RecursiveCte(cte) => {
            visit_all_query_node_exprs_mut(&mut cte.left, f);
            visit_all_query_node_exprs_mut(&mut cte.right, f);
        }
        _ => {}
    }
    for modifier in &mut node.modifiers {
        match modifier {
            ResultModifier::Order(nodes) => nodes.iter_mut().for_each(|n| visit_expr_mut(&mut n.expr, f)),
            ResultModifier::Limit { limit, offset, .. } => {
                for e in [limit, offset].into_iter().flatten() {
                    visit_expr_mut(e, f);
                }
            }
            ResultModifier::Distinct(targets) => targets.iter_mut().for_each(|e| visit_expr_mut(e, f)),
        }
    }
}

fn visit_all_table_ref_exprs_mut(table_ref: &mut TableRef, f: &mut impl FnMut(&mut Expr)) {
    match &mut table_ref.kind {
        TableRefKind::Join(join) => {
            visit_all_table_ref_exprs_mut(&mut join.left, f);
            visit_all_table_ref_exprs_mut(&mut join.right, f);
            if let Some(condition) = &mut join.condition {
                visit_expr_mut(condition, f);
            }
        }
        TableRefKind::Subquery(select) => visit_all_query_node_exprs_mut(&mut select.node, f),
        TableRefKind::TableFunction { function, .. } => visit_expr_mut(function, f),
        TableRefKind::ExpressionList(rows) => rows.iter_mut().flatten().for_each(|e| visit_expr_mut(e, f)),
        TableRefKind::Pivot(pivot) => {
            visit_all_table_ref_exprs_mut(&mut pivot.source, f);
            pivot.aggregates.iter_mut().for_each(|e| visit_expr_mut(e, f));
            for column in &mut pivot.pivots {
                column.pivot_expressions.iter_mut().for_each(|e| visit_expr_mut(e, f));
                column.entries.iter_mut().filter_map(|e| e.expr.as_mut()).for_each(|e| visit_expr_mut(e, f));
                if let Some(subquery) = &mut column.subquery {
                    visit_all_query_node_exprs_mut(subquery, f);
                }
            }
        }
        _ => {}
    }
}

fn visit_table_ref_exprs_mut(table_ref: &mut TableRef, f: &mut impl FnMut(&mut Expr)) {
    match &mut table_ref.kind {
        TableRefKind::Join(join) => {
            visit_table_ref_exprs_mut(&mut join.left, f);
            visit_table_ref_exprs_mut(&mut join.right, f);
            if let Some(condition) = &mut join.condition {
                visit_expr_mut(condition, f);
            }
        }
        TableRefKind::Subquery(select) => visit_query_node_exprs_mut(&mut select.node, f),
        TableRefKind::TableFunction { function, .. } => visit_expr_mut(function, f),
        TableRefKind::ExpressionList(rows) => rows.iter_mut().flatten().for_each(|e| visit_expr_mut(e, f)),
        TableRefKind::Pivot(pivot) => {
            visit_table_ref_exprs_mut(&mut pivot.source, f);
            pivot.aggregates.iter_mut().for_each(|e| visit_expr_mut(e, f));
            for column in &mut pivot.pivots {
                column.pivot_expressions.iter_mut().for_each(|e| visit_expr_mut(e, f));
                column.entries.iter_mut().filter_map(|e| e.expr.as_mut()).for_each(|e| visit_expr_mut(e, f));
                if let Some(subquery) = &mut column.subquery {
                    visit_query_node_exprs_mut(subquery, f);
                }
            }
        }
        _ => {}
    }
}

/// `ToRecursiveCTE`: a UNION in a recursive CTE becomes a recursive CTE node.
fn to_recursive_cte(node: QueryNode, name: &str, aliases: &[String], key_targets: &[Expr]) -> Result<QueryNode> {
    let QueryNode {
        kind: QueryNodeKind::SetOperation(mut set),
        modifiers,
        cte_map,
    } = node
    else {
        return Ok(node);
    };
    if set.setop_type != SetOpType::Union {
        return Ok(QueryNode {
            kind: QueryNodeKind::SetOperation(set),
            modifiers,
            cte_map,
        });
    }
    if set.children.len() < 2 {
        return Err(parser_error(
            "Expected at least two children to set operation node in recursive CTE",
        ));
    }
    for modifier in &modifiers {
        match modifier {
            ResultModifier::Limit { .. } => {
                return Err(parser_error("LIMIT or OFFSET in a recursive query is not allowed"));
            }
            ResultModifier::Order(_) => return Err(parser_error("ORDER BY in a recursive query is not allowed")),
            ResultModifier::Distinct(_) => {}
        }
    }
    let right = set.children.pop().unwrap();
    let left = if set.children.len() == 1 {
        set.children.pop().unwrap()
    } else {
        QueryNode {
            kind: QueryNodeKind::SetOperation(SetOperationNode {
                setop_type: set.setop_type,
                setop_all: set.setop_all,
                children: set.children,
            }),
            modifiers,
            cte_map: CteMap::default(),
        }
    };
    Ok(QueryNode {
        kind: QueryNodeKind::RecursiveCte(RecursiveCteNode {
            ctename: name.to_string(),
            union_all: set.setop_all,
            left: Box::new(left),
            right: Box::new(right),
            aliases: aliases.to_vec(),
            key_targets: key_targets.to_vec(),
        }),
        modifiers: Vec::new(),
        cte_map,
    })
}

fn check_grouping_set_max(count: usize) -> Result<()> {
    if count > MAX_GROUPING_SETS {
        return Err(parser_error(format!(
            "Maximum grouping set count of {MAX_GROUPING_SETS} exceeded"
        )));
    }
    Ok(())
}

/// `AddGroupByExpression`: row(...) groups by each of its arguments; equal expressions share an
/// index.
fn add_group_expression(expr: Expr, result: &mut GroupByNode, set: &mut BTreeSet<usize>) {
    if let ExprKind::Function(f) = &expr.kind
        && f.name.name == "row"
        && f.name.qualification.is_empty()
        && !f.is_operator
    {
        let ExprKind::Function(f) = expr.kind else {
            unreachable!()
        };
        for arg in f.args {
            add_group_expression(arg.expr, result, set);
        }
        return;
    }
    let index = match result.group_expressions.iter().position(|e| e == &expr) {
        Some(i) => i,
        None => {
            result.group_expressions.push(expr);
            result.group_expressions.len() - 1
        }
    };
    set.insert(index);
}

fn add_cube_sets(
    current: &BTreeSet<usize>,
    cube: &[BTreeSet<usize>],
    out: &mut Vec<BTreeSet<usize>>,
    start: usize,
) -> Result<()> {
    check_grouping_set_max(out.len())?;
    out.push(current.clone());
    for k in start..cube.len() {
        let child: BTreeSet<usize> = current.union(&cube[k]).copied().collect();
        add_cube_sets(&child, cube, out, k + 1)?;
    }
    Ok(())
}

/// `GroupByExpressionUnfolding`.
fn group_by_unfolding(item: GroupByItem, result: &mut GroupByNode) -> Result<Vec<BTreeSet<usize>>> {
    let mut sets = Vec::new();
    match item {
        GroupByItem::Empty => sets.push(BTreeSet::new()),
        GroupByItem::Expression(expr) => {
            let mut set = BTreeSet::new();
            add_group_expression(expr, result, &mut set);
            sets.push(set);
        }
        GroupByItem::GroupingSets(children) => {
            for child in children {
                sets.extend(group_by_unfolding(child, result)?);
                check_grouping_set_max(sets.len())?;
            }
        }
        GroupByItem::Cube(exprs) | GroupByItem::Rollup(exprs) if exprs.is_empty() => {
            return Err(parser_error("CUBE or ROLLUP column list cannot be empty"));
        }
        GroupByItem::Cube(exprs) => {
            let mut unfolding = Vec::new();
            for expr in exprs {
                let mut set = BTreeSet::new();
                add_group_expression(expr, result, &mut set);
                unfolding.push(set);
            }
            let mut combinations = 1usize;
            for _ in 0..unfolding.len() {
                combinations = combinations.saturating_mul(2);
                check_grouping_set_max(combinations)?;
            }
            add_cube_sets(&BTreeSet::new(), &unfolding, &mut sets, 0)?;
        }
        GroupByItem::Rollup(exprs) => {
            let mut current = BTreeSet::new();
            sets.push(current.clone());
            for expr in exprs {
                let mut set = BTreeSet::new();
                add_group_expression(expr, result, &mut set);
                current.extend(set);
                sets.push(current.clone());
            }
        }
    }
    Ok(sets)
}
