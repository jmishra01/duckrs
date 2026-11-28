// Ported from duckdb/src/planner/binder/tableref/bind_match_recognize_ref.cpp and
// duckdb/src/planner/expression_binder/match_recognize_binder.cpp @ rust-port-baseline
//
//! `Binder::Bind(MatchRecognizeRef)`. What a condition or a measure reads of the match - a column at a row
//! a navigation reaches, an aggregate over the rows mapped to a variable, CLASSIFIER(), MATCH_NUMBER() - is a
//! "slot" the operator fills in. What the slots read of a row (the operand of a navigation or an aggregate,
//! with the variable taken off), and the partitioning and the order, are computed below the operator as
//! columns of the input, by a SELECT over it. What is left of each expression is bound over the columns of
//! that input and the slots.

use super::{Binder, Binding, binder_error};
use crate::execution::match_recognize::{
    AggregateOperand, MatchExpression, MatchRecognizeData, Navigation, OutputColumn, PatternNode, Slot, SlotKind,
};
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use pintail_common::types::LogicalType;
use pintail_common::{Error, ErrorKind, Result};
use pintail_parser::ast::MacroBody;
use pintail_parser::ast::{
    AfterMatchSkip, Expr, ExprKind, MatchRecognizeRef, RowsPerMatch, SelectNode, SelectStatement, TableRef,
};
use std::collections::HashMap;
use std::sync::Arc;

/// The name of a slot in the binding of the slots.
fn slot_name(index: usize) -> String {
    format!("__mr_slot_{index}")
}

/// The name of a column computed below the operator.
fn hoisted_name(index: usize) -> String {
    format!("__mr_hoisted_{index}")
}

const SLOTS: &str = "__mr_slots";

/// A slot before binding: what it reads, with the expression of its column still to be computed.
#[derive(Debug, Clone)]
enum PendingKind {
    Column {
        navigation: Navigation,
        operand: usize,
    },
    Classifier {
        navigation: Navigation,
    },
    MatchNumber,
    Aggregate {
        symbols: Vec<usize>,
        name: String,
        operand: Option<PendingOperand>,
        distinct: bool,
        /// ORDER BY of the aggregate: hoisted columns, ascending or not, NULLS FIRST or not
        order: Vec<(usize, pintail_parser::ast::OrderType, pintail_parser::ast::NullOrder)>,
        /// FILTER of the aggregate, of each of its rows
        filter: Option<(Box<Expr>, Vec<PendingSlot>)>,
    },
    Measure(usize),
}

/// What an aggregate folds: a column computed below, or an expression of each of its rows that reads their
/// classifier, which nothing below can know.
#[derive(Debug, Clone)]
enum PendingOperand {
    Hoisted(usize),
    PerRow(Box<Expr>, Vec<PendingSlot>),
}

#[derive(Debug, Clone)]
struct PendingSlot {
    kind: PendingKind,
    running: Option<bool>,
}

/// What the clause calls its variables.
struct Symbols {
    /// the variables, as written first
    names: Vec<String>,
    /// SUBSET names, and the variables they stand for
    subsets: HashMap<String, Vec<usize>>,
}

impl Symbols {
    /// The variables `name` stands for: one, or a SUBSET's.
    fn lookup(&self, name: &str) -> Option<Vec<usize>> {
        let lower = name.to_lowercase();
        if let Some(at) = self.names.iter().position(|n| n.to_lowercase() == lower) {
            return Some(vec![at]);
        }
        self.subsets.get(&lower).cloned()
    }
}

/// Where an expression is: in DEFINE, or in MEASURES with the names of the measures before it.
struct Extraction<'s> {
    symbols: &'s Symbols,
    measures: Option<Vec<String>>,
    /// the operands to compute below, shared by all expressions
    hoisted: &'s mut Vec<Expr>,
    slots: Vec<PendingSlot>,
    running: Option<bool>,
    /// what the lambdas around the expression bind
    lambda_parameters: Vec<String>,
    /// in a DEFINE, the variable it defines
    defining: Option<usize>,
    /// the names of the input's columns, which an unqualified reference reads of the whole match
    input: &'s [String],
    /// in MEASURES, the names of the measures after this one
    later: &'s [String],
}

fn is_navigation(name: &str) -> bool {
    matches!(name, "prev" | "next" | "first" | "last")
}

impl Extraction<'_> {
    /// Whether a reference reads a column of the whole match: one of the input's, not a variable's or the
    /// surrounding query's.
    fn universal(&self, names: &[String]) -> bool {
        let column = match names {
            [column] => column,
            [qualifier, column] if self.symbols.lookup(qualifier).is_none() => column,
            _ => return false,
        };
        self.input.iter().any(|c| c.eq_ignore_ascii_case(column))
    }

    fn hoist(&mut self, expr: Expr) -> usize {
        if let Some(at) = self.hoisted.iter().position(|h| *h == expr) {
            return at;
        }
        self.hoisted.push(expr);
        self.hoisted.len() - 1
    }

    fn slot(&mut self, kind: PendingKind) -> Expr {
        self.slots.push(PendingSlot {
            kind,
            running: self.running,
        });
        Expr::column(vec![SLOTS.to_string(), slot_name(self.slots.len() - 1)])
    }

    /// The variables an operand names, and the operand without them: `A.price * 2` is `price * 2` of `A`.
    fn strip_variables(&self, mut operand: Expr) -> Result<(Option<Vec<usize>>, Expr)> {
        let mut found: Option<Vec<usize>> = None;
        let mut mixed = false;
        let mut parameters = self.lambda_parameters.clone();
        visit_scoped(&mut operand, &mut parameters, &mut |e, parameters| {
            if let ExprKind::ColumnRef(names) = &mut e.kind
                && names.len() >= 2
                && !parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0]))
                && let Some(symbols) = self.symbols.lookup(&names[0])
            {
                match &found {
                    Some(existing) if *existing != symbols => mixed = true,
                    _ => found = Some(symbols),
                }
                names.remove(0);
            }
        });
        if mixed {
            return Err(binder_error(
                "MATCH_RECOGNIZE navigation and aggregates may only reference a single pattern variable",
            ));
        }
        Ok((found, operand))
    }

    /// `n` of `PREV(x, n)` and the like: a constant that isn't negative.
    fn offset(&self, name: &str, args: &[Expr]) -> Result<usize> {
        let Some(arg) = args.get(1) else {
            return Ok(if matches!(name, "prev" | "next") { 1 } else { 0 });
        };
        let upper = name.to_uppercase();
        match &arg.kind {
            ExprKind::Constant(literal) if matches!(literal.kind, pintail_parser::ast::LiteralKind::Null) => {
                Err(binder_error(format!(
                    "The offset of {upper}() must be a non-negative integer, not NULL"
                )))
            }
            ExprKind::Constant(literal) => {
                let offset = literal
                    .text
                    .parse::<u64>()
                    .map_err(|_| binder_error(format!("The offset of {upper}() must be a non-negative integer")))?;
                if offset >= i64::MAX as u64 {
                    return Err(binder_error(format!(
                        "The offset of {upper}() is larger than any match can have rows"
                    )));
                }
                Ok(offset as usize)
            }
            _ => Err(binder_error(format!("The offset of {upper}() must be a constant"))),
        }
    }

    /// A navigation: what it reads at the row it reaches.
    fn navigation(&mut self, expr: &Expr, physical: i64) -> Result<Expr> {
        let ExprKind::Function(function) = &expr.kind else {
            unreachable!("a navigation is a function");
        };
        let name = function.name.name.to_lowercase();
        let args: Vec<Expr> = function.args.iter().map(|a| a.expr.clone()).collect();
        let Some(inner) = args.first() else {
            return Err(binder_error(format!("{} requires an argument", name.to_uppercase())));
        };
        let offset = self.offset(&name, &args)?;
        match name.as_str() {
            "prev" | "next" => {
                let step = if name == "prev" {
                    -(offset as i64)
                } else {
                    offset as i64
                };
                // PREV(LAST(A.x)) steps from the row LAST reaches, PREV(FINAL LAST(A.x)) from the one it reaches at the
                // end of the match
                let mut inner = inner;
                let mut running = None;
                if let ExprKind::Function(f) = &inner.kind
                    && matches!(f.name.name.as_str(), "__mr_running" | "__mr_final")
                    && let Some(wrapped) = f.args.first()
                {
                    running = Some(f.name.name == "__mr_running");
                    if running == Some(false) && self.measures.is_none() {
                        return Err(binder_error(
                            "FINAL reads the whole match, which a DEFINE condition is still assembling, so only \
                             RUNNING is available there",
                        ));
                    }
                    inner = &wrapped.expr;
                }
                if let ExprKind::Function(f) = &inner.kind
                    && matches!(f.name.name.to_lowercase().as_str(), "first" | "last")
                {
                    let previous = match running {
                        Some(running) => self.running.replace(running),
                        None => self.running,
                    };
                    let result = self.navigation(inner, physical + step);
                    self.running = previous;
                    return result;
                }
                if contains_aggregate(inner) {
                    return Err(binder_error(
                        "An aggregate reads the rows of the match, so it cannot be part of the expression a \
                         navigation reads, which is computed for every row of the input",
                    ));
                }
                if contains_step(inner) {
                    return Err(binder_error(format!(
                        "{}() steps from a row the match names, and a step inside it names none, so the two cannot \
                         be nested",
                        name.to_uppercase()
                    )));
                }
                self.check_navigated_operand(inner, &name)?;
                self.check_classifier_operand(inner, &name)?;
                if reads_navigation(inner) {
                    return Err(binder_error("Nested row pattern navigation is not supported"));
                }
                self.read_at(inner.clone(), None, physical + step)
            }
            _ => {
                // FIRST(PREV(x)): the step from the row FIRST reaches
                let mut inner = inner;
                let mut physical = physical;
                while let ExprKind::Function(f) = &inner.kind
                    && matches!(f.name.name.to_lowercase().as_str(), "prev" | "next")
                    && let Some(stepped) = f.args.first()
                {
                    let step_name = f.name.name.to_lowercase();
                    let step_args: Vec<Expr> = f.args.iter().map(|a| a.expr.clone()).collect();
                    let step = self.offset(&step_name, &step_args)? as i64;
                    physical += if step_name == "prev" { -step } else { step };
                    inner = &stepped.expr;
                }
                if contains_aggregate(inner) {
                    return Err(binder_error(
                        "An aggregate reads the rows of the match, so it cannot be part of the expression a \
                         navigation reads, which is computed for every row of the input",
                    ));
                }
                self.check_navigated_operand(inner, &name)?;
                self.check_classifier_operand(inner, &name)?;
                if reads_navigation(inner) {
                    return Err(binder_error("Nested row pattern navigation is not supported"));
                }
                let first = name == "first";
                self.read_at(inner.clone(), Some((first, offset)), physical)
            }
        }
    }

    /// What `operand` is at the row `logical` (FIRST or LAST, and how far in) and `physical` reach.
    fn read_at(&mut self, operand: Expr, logical: Option<(bool, usize)>, physical: i64) -> Result<Expr> {
        let classifier = is_classifier(&operand);
        let (symbols, stripped) = if classifier {
            // CLASSIFIER(X): of the last row mapped to X
            let ExprKind::Function(f) = &operand.kind else {
                unreachable!("CLASSIFIER() is a call")
            };
            match f.args.as_slice() {
                [] => (None, operand.clone()),
                [argument] => match &argument.expr.kind {
                    ExprKind::ColumnRef(names) if names.len() == 1 && self.symbols.lookup(&names[0]).is_some() => {
                        (self.symbols.lookup(&names[0]), operand.clone())
                    }
                    ExprKind::ColumnRef(names) if names.len() == 1 => {
                        return Err(binder_error(format!(
                            "Referenced column \"{}\" not found in FROM clause!",
                            names[0]
                        )));
                    }
                    _ => {
                        return Err(binder_error(
                            "CLASSIFIER() takes a pattern variable, or nothing for the match as a whole",
                        ));
                    }
                },
                _ => {
                    return Err(binder_error(
                        "CLASSIFIER() takes a pattern variable, or nothing for the match as a whole",
                    ));
                }
            }
        } else {
            self.strip_variables(operand)?
        };
        // in MEASURES a row is the match's last one unless something says which, in DEFINE the row tested
        let logical = match (logical, &symbols) {
            (Some((first, offset)), symbols) => Some((symbols.clone().unwrap_or_default(), first, offset)),
            // in the DEFINE of a variable, its last row so far is the row being tested
            (None, Some(symbols)) if self.defining.is_some_and(|d| symbols.contains(&d)) => None,
            (None, Some(symbols)) => Some((symbols.clone(), false, 0)),
            (None, None) if self.measures.is_some() => None,
            (None, None) => None,
        };
        let navigation = Navigation { logical, physical };
        if classifier {
            return Ok(self.slot(PendingKind::Classifier { navigation }));
        }
        let operand = self.hoist(stripped);
        Ok(self.slot(PendingKind::Column { navigation, operand }))
    }

    /// `expr` with what it reads of the match replaced by slots.
    fn extract(&mut self, expr: &Expr) -> Result<Expr> {
        let alias = expr.alias.clone();
        match &expr.kind {
            ExprKind::Function(function) if function.name.qualification.is_empty() => {
                let name = function.name.name.to_lowercase();
                let argument = |at: usize| function.args.get(at).map(|a| &a.expr);
                if name == "__mr_running" || name == "__mr_final" {
                    let running = name == "__mr_running";
                    if !running && self.measures.is_none() {
                        return Err(binder_error(
                            "FINAL reads the whole match, which a DEFINE condition is still assembling, so only \
                             RUNNING is available there",
                        ));
                    }
                    let inner = argument(0).expect("RUNNING and FINAL have a call");
                    if let ExprKind::Function(f) = &inner.kind
                        && matches!(f.name.name.to_lowercase().as_str(), "prev" | "next")
                    {
                        return Err(binder_error(format!(
                            "{}() always reads the row it steps to, so RUNNING and FINAL cannot be written in front \
                             of it",
                            f.name.name.to_uppercase()
                        )));
                    }
                    let previous = self.running.replace(running);
                    let result = self.extract(inner);
                    self.running = previous;
                    return result.map(|e| e.with_alias(alias));
                }
                if is_navigation(&name) {
                    if function.args.is_empty() || function.args.len() > 2 {
                        return Err(binder_error(format!(
                            "{}() takes an expression and an optional offset",
                            name.to_uppercase()
                        )));
                    }
                    return Ok(self.navigation(expr, 0)?.with_alias(alias));
                }
                if name == "classifier" {
                    return Ok(self.read_at(expr.clone(), None, 0)?.with_alias(alias));
                }
                if name == "match_number" {
                    return Ok(self.slot(PendingKind::MatchNumber).with_alias(alias));
                }
                if BuiltinFunctions::get().aggregate(&name).is_some() && !function.is_operator {
                    return Ok(self.aggregate(function, &name)?.with_alias(alias));
                }
                let mut result = expr.clone();
                self.extract_children(&mut result)?;
                Ok(result)
            }
            ExprKind::Lambda {
                lhs,
                expr: body,
                syntax,
            } => {
                let mut parameters = Vec::new();
                lambda_parameters(lhs, &mut parameters);
                let depth = self.lambda_parameters.len();
                self.lambda_parameters.extend(parameters);
                let body = self.extract(body);
                self.lambda_parameters.truncate(depth);
                Ok(Expr {
                    kind: ExprKind::Lambda {
                        lhs: lhs.clone(),
                        expr: Box::new(body?),
                        syntax: *syntax,
                    },
                    alias,
                })
            }
            ExprKind::ColumnRef(names) => {
                if self.lambda_parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0])) {
                    return Ok(expr.clone());
                }
                if names.len() >= 2 && self.symbols.lookup(&names[0]).is_some() {
                    return Ok(self.read_at(expr.clone(), None, 0)?.with_alias(alias));
                }
                if let Some(measures) = &self.measures {
                    // `n` or `alias.n`, the two spellings of a select list's earlier column
                    let earlier = match names.as_slice() {
                        // a column of the input by the same name is still the column
                        [name] if !self.input.iter().any(|c| c.eq_ignore_ascii_case(name)) => Some(name),
                        [qualifier, name] if qualifier.eq_ignore_ascii_case("alias") => Some(name),
                        _ => None,
                    };
                    if let Some(name) = earlier
                        && let Some(at) = measures.iter().position(|m| m.eq_ignore_ascii_case(name))
                    {
                        return Ok(self.slot(PendingKind::Measure(at)).with_alias(alias));
                    }
                    if let Some(name) = earlier
                        && self.later.iter().any(|m| m.eq_ignore_ascii_case(name))
                    {
                        return Err(binder_error(format!(
                            "Column {name} referenced that exists in the SELECT clause - but this column cannot be \
                             referenced before it is defined"
                        )));
                    }
                    // a column of the input in a measure is at the match's last row (`RUNNING`: the current one)
                    return Ok(self.read_at(expr.clone(), Some((false, 0)), 0)?.with_alias(alias));
                }
                // in a condition, at the row it tests
                Ok(self.read_at(expr.clone(), None, 0)?.with_alias(alias))
            }
            ExprKind::Window(_) => {
                if let Some(what) = match_construct(expr) {
                    return Err(binder_error(format!(
                        "{what} only means something while the matcher is assembling a match, so it cannot be part of \
                         a window function, which is computed over the whole partition before there is a match"
                    )));
                }
                let logical = self.measures.is_some().then_some((false, 0));
                Ok(self.read_at(expr.clone(), logical, 0)?.with_alias(alias))
            }
            ExprKind::Subquery { .. } if self.measures.is_none() => {
                Err(binder_error("A DEFINE condition may not contain a subquery"))
            }
            _ => {
                let mut result = expr.clone();
                self.extract_children(&mut result)?;
                Ok(result)
            }
        }
    }

    /// An aggregate over the rows the match mapped to a variable (or to any).
    fn aggregate(&mut self, function: &pintail_parser::ast::FunctionExpr, name: &str) -> Result<Expr> {
        let upper = name.to_uppercase();
        if self.measures.is_none() {
            if function.distinct || function.filter.is_some() || !function.order_by.is_empty() {
                return Err(binder_error(
                    "DISTINCT, FILTER and ORDER BY are not supported on an aggregate in a DEFINE condition",
                ));
            }
            if function.args.len() > 1 {
                return Err(binder_error(format!(
                    "An aggregate in a DEFINE condition reads one expression of the match's rows, so {upper}() is not \
                     supported there yet"
                )));
            }
        }
        let (symbols, operand) = match function.args.first().map(|a| &a.expr) {
            None => (None, None),
            Some(Expr {
                kind: ExprKind::Star(star),
                ..
            }) => {
                let symbols = if star.relation_name.is_empty() {
                    None
                } else {
                    Some(
                        self.symbols
                            .lookup(&star.relation_name)
                            .ok_or_else(|| binder_error(format!("{} is not a pattern variable", star.relation_name)))?,
                    )
                };
                (symbols, None)
            }
            Some(arg) => {
                // CLASSIFIER(X) of each row is the rows of X, each with its classifier
                let mut arg = arg.clone();
                let mut classified: Option<String> = None;
                pintail_parser::transformer::visit_expr_mut(&mut arg, &mut |e| {
                    if let ExprKind::Function(f) = &mut e.kind
                        && f.name.name.eq_ignore_ascii_case("classifier")
                        && let [argument] = f.args.as_slice()
                        && let ExprKind::ColumnRef(names) = &argument.expr.kind
                        && names.len() == 1
                    {
                        classified = Some(names[0].clone());
                        f.args.clear();
                    }
                });
                let arg = &arg;
                if reads_navigation(arg) {
                    return Err(binder_error(format!(
                        "{upper}() aggregates the rows of the match, so a row pattern navigation cannot be nested \
                         inside it"
                    )));
                }
                if contains_aggregate(arg) {
                    return Err(binder_error(
                        "An aggregate reads the rows of the match, so it cannot be part of the expression a \
                         navigation reads, which is computed for every row of the input",
                    ));
                }
                let mut whole = vec![arg.clone()];
                whole.extend(function.filter.iter().map(|f| (**f).clone()));
                whole.extend(function.order_by.iter().map(|o| o.expr.clone()));
                let whole = Expr::operator(pintail_parser::ast::OperatorType::Coalesce, whole);
                if let Some((variable, spelled)) = self.mixed_reference(&whole) {
                    return Err(binder_error(format!(
                        "{}() reads the rows of \"{variable}\", so {spelled} cannot also read the whole match",
                        function.name.name
                    )));
                }
                let (mut symbols, stripped) = self.strip_variables(arg.clone()).map_err(|error| {
                    if self.measures.is_some() {
                        binder_error(
                            "An aggregate in MEASURES reads the rows of one pattern variable, so it cannot also read \
                             those of another",
                        )
                    } else {
                        error
                    }
                })?;
                if let Some(name) = &classified {
                    let named = self.symbols.lookup(name).ok_or_else(|| {
                        binder_error("CLASSIFIER() takes a pattern variable, or nothing for the match as a whole")
                    })?;
                    symbols = Some(named);
                }
                if reads_match(&stripped) && self.measures.is_none() {
                    return Err(binder_error(
                        "CLASSIFIER() cannot be aggregated in a DEFINE condition, which is still assembling the match",
                    ));
                }
                if reads_match(&stripped) {
                    // CLASSIFIER() of each row, which is the operator's to know
                    let (residual, slots) = self.per_row(&stripped)?;
                    (symbols, Some(PendingOperand::PerRow(Box::new(residual), slots)))
                } else {
                    (symbols, Some(PendingOperand::Hoisted(self.hoist(stripped))))
                }
            }
        };
        let filter = match &function.filter {
            Some(filter) => {
                let (filtered, stripped) = self.strip_variables((**filter).clone())?;
                if let (Some(filtered), Some(aggregated)) = (&filtered, &symbols)
                    && filtered != aggregated
                {
                    let name_of = |set: &Vec<usize>| self.symbols.names[set[0]].clone();
                    return Err(binder_error(format!(
                        "An aggregate in MEASURES reads the rows of one pattern variable, so \"{}\" cannot also read \
                         those of \"{}\"",
                        name_of(aggregated),
                        name_of(filtered)
                    )));
                }
                let (residual, slots) = self.per_row(&stripped)?;
                Some((Box::new(residual), slots))
            }
            None => None,
        };
        let mut order = Vec::with_capacity(function.order_by.len());
        for node in &function.order_by {
            let (_, stripped) = self.strip_variables(node.expr.clone())?;
            order.push((self.hoist(stripped), node.order, node.nulls));
        }
        let name = if operand.is_none() && name == "count" {
            "count_star".to_string()
        } else {
            name.to_string()
        };
        Ok(self.slot(PendingKind::Aggregate {
            symbols: symbols.unwrap_or_default(),
            name,
            operand,
            distinct: function.distinct,
            order,
            filter,
        }))
    }

    /// What a navigation reads is one row of one variable, computed for every row of the input.
    fn check_navigated_operand(&self, inner: &Expr, name: &str) -> Result<()> {
        let upper = name.to_uppercase();
        let mut match_number = false;
        let mut variables: Vec<String> = Vec::new();
        let mut whole_classifier = false;
        let mut plain: Option<String> = None;
        let mut parameters = self.lambda_parameters.clone();
        visit_scoped(
            &mut inner.clone(),
            &mut parameters,
            &mut |e, parameters| match &e.kind {
                ExprKind::Function(f) if f.name.name.eq_ignore_ascii_case("match_number") => match_number = true,
                ExprKind::Function(f) if f.name.name.eq_ignore_ascii_case("classifier") && f.args.is_empty() => {
                    whole_classifier = true
                }
                ExprKind::ColumnRef(names)
                    if names.len() >= 2
                        && self.symbols.lookup(&names[0]).is_some()
                        && !parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0])) =>
                {
                    variables.push(names[0].clone());
                }
                ExprKind::ColumnRef(names)
                    if !parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0])) && self.universal(names) =>
                {
                    plain.get_or_insert_with(|| names.join("."));
                }
                _ => {}
            },
        );
        let mut seen: Vec<String> = Vec::new();
        variables.retain(|v| {
            let first = !seen.iter().any(|s| s.eq_ignore_ascii_case(v));
            seen.push(v.clone());
            first
        });
        if match_number {
            return Err(binder_error(
                "MATCH_NUMBER() only means something while the matcher is assembling a match, so it cannot be part \
                 of the expression a navigation reads, which is computed for every row of the input",
            ));
        }
        if let [first, second, ..] = variables.as_slice() {
            return Err(binder_error(format!(
                "{upper}() reads one row of the match, so \"{first}\" cannot also read a row of \"{second}\""
            )));
        }
        if let ([variable], true) = (variables.as_slice(), whole_classifier) {
            return Err(binder_error(format!(
                "{upper}() reads the rows of \"{variable}\", so CLASSIFIER() cannot also read the whole match"
            )));
        }
        if let ([variable], Some(plain)) = (variables.as_slice(), plain) {
            return Err(binder_error(format!(
                "{upper}() reads the rows of \"{variable}\", so \"{plain}\" cannot also read the whole match"
            )));
        }
        Ok(())
    }

    /// A navigation over CLASSIFIER() reads that and nothing else of the row.
    fn check_classifier_operand(&self, inner: &Expr, name: &str) -> Result<()> {
        let mut classified: Option<String> = None;
        let mut has_classifier = false;
        pintail_parser::transformer::visit_expr_mut(&mut inner.clone(), &mut |e| {
            if let ExprKind::Function(f) = &e.kind
                && f.name.name.eq_ignore_ascii_case("classifier")
            {
                has_classifier = true;
                if let [argument] = f.args.as_slice()
                    && let ExprKind::ColumnRef(names) = &argument.expr.kind
                {
                    classified = Some(names[0].clone());
                }
            }
        });
        if !has_classifier || is_classifier(inner) {
            return Ok(());
        }
        let upper = name.to_uppercase();
        if let Some(variable) = classified {
            let mut plain = None;
            pintail_parser::transformer::visit_expr_mut(&mut inner.clone(), &mut |e| {
                if let ExprKind::Function(f) = &mut e.kind
                    && f.name.name.eq_ignore_ascii_case("classifier")
                {
                    f.args.clear();
                }
                if let ExprKind::ColumnRef(names) = &e.kind
                    && !names[0].eq_ignore_ascii_case(&variable)
                {
                    plain.get_or_insert_with(|| names.join("."));
                }
            });
            if let Some(plain) = plain {
                return Err(binder_error(format!(
                    "{upper}() reads the rows of \"{variable}\", so \"{plain}\" cannot also read the whole match"
                )));
            }
        }
        Err(binder_error(format!(
            "{upper}() over CLASSIFIER() reads the classifier of the row it steps to, and nothing else of it"
        )))
    }

    /// A reference to a variable's column next to one to a column of the whole match, as `A.v + v`: the variable,
    /// and the reference to the whole match.
    fn mixed_reference(&self, expr: &Expr) -> Option<(String, String)> {
        let mut variable = None;
        let mut plain = None;
        let mut parameters = self.lambda_parameters.clone();
        visit_scoped(&mut expr.clone(), &mut parameters, &mut |e, parameters| match &e.kind {
            ExprKind::ColumnRef(names) => {
                if parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0])) {
                    return;
                }
                if names.len() >= 2 && self.symbols.lookup(&names[0]).is_some() {
                    variable.get_or_insert_with(|| names[0].clone());
                } else if self.universal(names) {
                    plain.get_or_insert_with(|| format!("\"{}\"", names.join(".")));
                }
            }
            ExprKind::Function(f) if f.name.name.eq_ignore_ascii_case("classifier") && f.args.is_empty() => {
                plain.get_or_insert_with(|| "CLASSIFIER()".to_string());
            }
            _ => {}
        });
        variable.zip(plain)
    }

    /// An expression of each row an aggregate folds: its columns, and its classifier, at that row.
    fn per_row(&mut self, expr: &Expr) -> Result<(Expr, Vec<PendingSlot>)> {
        let mut nested = Extraction {
            symbols: self.symbols,
            measures: None,
            hoisted: &mut *self.hoisted,
            slots: Vec::new(),
            running: None,
            lambda_parameters: self.lambda_parameters.clone(),
            defining: None,
            input: self.input,
            later: &[],
        };
        let residual = nested.extract(expr)?;
        Ok((residual, nested.slots))
    }

    /// [`Self::extract`] of each child of `expr`.
    fn extract_children(&mut self, expr: &mut Expr) -> Result<()> {
        let mut error = None;
        for_each_child(expr, &mut |child| match self.extract(child) {
            Ok(e) => *child = e,
            Err(e) => {
                error.get_or_insert(e);
            }
        });
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Whether an expression reads the match: a navigation, CLASSIFIER() or MATCH_NUMBER().
fn reads_match(expr: &Expr) -> bool {
    let mut found = false;
    pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::Function(f) = &e.kind {
            let name = f.name.name.to_lowercase();
            found |= is_navigation(&name) || name == "classifier" || name == "match_number";
        }
    });
    found
}

fn is_classifier(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function(f) if f.name.name.eq_ignore_ascii_case("classifier"))
}

/// What of the clause's own an expression reads, as an error names it.
fn match_construct(expr: &Expr) -> Option<&'static str> {
    let mut found = None;
    pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::Function(f) = &e.kind {
            let name = f.name.name.to_lowercase();
            let what = match name.as_str() {
                "classifier" => Some("CLASSIFIER()"),
                "match_number" => Some("MATCH_NUMBER()"),
                n if is_navigation(n) => Some("Reading a row of the match"),
                _ => None,
            };
            if found.is_none() {
                found = what;
            }
        }
    });
    found
}

/// Whether an expression steps: PREV or NEXT.
fn contains_step(expr: &Expr) -> bool {
    let mut found = false;
    pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::Function(f) = &e.kind {
            found |= matches!(f.name.name.to_lowercase().as_str(), "prev" | "next");
        }
    });
    found
}

/// Whether an expression navigates: PREV, NEXT, FIRST or LAST.
fn reads_navigation(expr: &Expr) -> bool {
    let mut found = false;
    pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::Function(f) = &e.kind {
            found |= is_navigation(&f.name.name.to_lowercase());
        }
    });
    found
}

/// Whether an expression calls an aggregate.
fn contains_aggregate(expr: &Expr) -> bool {
    let mut found = false;
    pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
        if let ExprKind::Function(f) = &e.kind
            && f.name.qualification.is_empty()
            && !f.is_operator
            && !is_navigation(&f.name.name.to_lowercase())
        {
            found |= BuiltinFunctions::get().aggregate(&f.name.name).is_some();
        }
    });
    found
}

/// The names a lambda binds.
fn lambda_parameters(lhs: &Expr, names: &mut Vec<String>) {
    match &lhs.kind {
        ExprKind::ColumnRef(n) => names.push(n[0].clone()),
        ExprKind::Function(f) => f.args.iter().for_each(|a| lambda_parameters(&a.expr, names)),
        ExprKind::Operator { children, .. } => children.iter().for_each(|c| lambda_parameters(c, names)),
        _ => {}
    }
}

/// Calls `f` on `expr` and everything in it, with the lambda parameters in scope where it is.
fn visit_scoped(expr: &mut Expr, parameters: &mut Vec<String>, f: &mut dyn FnMut(&mut Expr, &[String])) {
    f(expr, parameters);
    if let ExprKind::Lambda { lhs, expr: body, .. } = &mut expr.kind {
        let depth = parameters.len();
        lambda_parameters(lhs, parameters);
        visit_scoped(body, parameters, f);
        parameters.truncate(depth);
        return;
    }
    for_each_child(expr, &mut |child| visit_scoped(child, parameters, f));
}

/// Calls `f` on each direct child of `expr` (as [`pintail_parser::transformer::visit_expr_mut`] reaches them).
fn for_each_child(expr: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    match &mut expr.kind {
        ExprKind::Function(func) => {
            func.args.iter_mut().for_each(|a| f(&mut a.expr));
            if let Some(filter) = &mut func.filter {
                f(filter);
            }
            func.order_by.iter_mut().for_each(|o| f(&mut o.expr));
        }
        ExprKind::Window(w) => {
            w.args.iter_mut().for_each(|a| f(&mut a.expr));
            w.partitions.iter_mut().for_each(&mut *f);
            w.orders.iter_mut().for_each(|o| f(&mut o.expr));
            w.arg_orders.iter_mut().for_each(|o| f(&mut o.expr));
            for e in [&mut w.start_expr, &mut w.end_expr, &mut w.filter]
                .into_iter()
                .flatten()
            {
                f(e);
            }
        }
        ExprKind::Concatenation(parts) => parts.iter_mut().for_each(&mut *f),
        ExprKind::Comparison { left, right, .. } => {
            f(left);
            f(right);
        }
        ExprKind::Conjunction { children, .. } | ExprKind::Operator { children, .. } => children.iter_mut().for_each(f),
        ExprKind::Between { input, lower, upper } => {
            f(input);
            f(lower);
            f(upper);
        }
        ExprKind::Case { checks, else_expr } => {
            for (when, then) in checks {
                f(when);
                f(then);
            }
            f(else_expr);
        }
        ExprKind::Cast { child, .. } | ExprKind::Collate { child, .. } => f(child),
        ExprKind::Star(star) => {
            star.replace_list.iter_mut().for_each(|(_, e)| f(e));
            if let Some(e) = &mut star.expr {
                f(e);
            }
        }
        _ => {}
    }
}

/// The pattern, its variables by their index in `symbols`.
fn build_pattern(expr: &Expr, symbols: &Symbols) -> Result<PatternNode> {
    Ok(match &expr.kind {
        ExprKind::ColumnRef(names) => {
            let at = symbols
                .names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&names[0]))
                .ok_or_else(|| Error::new(ErrorKind::Internal, "MATCH_RECOGNIZE pattern symbol has no condition"))?;
            PatternNode::Symbol(at)
        }
        ExprKind::Anchor(at_end) => PatternNode::Anchor(*at_end),
        ExprKind::Concatenation(parts) => {
            PatternNode::Concatenation(parts.iter().map(|p| build_pattern(p, symbols)).collect::<Result<_>>()?)
        }
        ExprKind::Alternation(left, right) => PatternNode::Alternation(
            Box::new(build_pattern(left, symbols)?),
            Box::new(build_pattern(right, symbols)?),
        ),
        ExprKind::Quantified {
            child,
            min,
            max,
            excluded,
            reluctant,
        } => PatternNode::Quantifier {
            child: Box::new(build_pattern(child, symbols)?),
            min: min.unwrap_or(0),
            max: *max,
            reluctant: *reluctant,
            excluded: *excluded,
        },
        _ => {
            return Err(Error::new(
                ErrorKind::Internal,
                "Unsupported MATCH_RECOGNIZE pattern node",
            ));
        }
    })
}

/// The pattern variables a pattern names, in order.
fn pattern_variables(expr: &Expr, names: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::ColumnRef(n) if !names.iter().any(|x| x.eq_ignore_ascii_case(&n[0])) => names.push(n[0].clone()),
        ExprKind::Concatenation(parts) => parts.iter().for_each(|p| pattern_variables(p, names)),
        ExprKind::Alternation(left, right) => {
            pattern_variables(left, names);
            pattern_variables(right, names);
        }
        ExprKind::Quantified { child, .. } => pattern_variables(child, names),
        _ => {}
    }
}

fn has_exclusion(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Quantified { excluded: true, .. } => true,
        ExprKind::Quantified { child, .. } => has_exclusion(child),
        ExprKind::Concatenation(parts) => parts.iter().any(has_exclusion),
        ExprKind::Alternation(left, right) => has_exclusion(left) || has_exclusion(right),
        _ => false,
    }
}

/// The name a plain column reference gives its column; none for an expression.
fn column_name(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::ColumnRef(names) => names.last().cloned(),
        _ => None,
    }
}

impl Binder<'_> {
    pub(super) fn bind_match_recognize(
        &mut self,
        table_ref: &TableRef,
        mr: &MatchRecognizeRef,
    ) -> Result<LogicalOperator> {
        // `CollectSymbols`: the variables DEFINE names, then the ones only the PATTERN does
        let mut in_pattern = Vec::new();
        pattern_variables(&mr.pattern, &mut in_pattern);
        // DEFINE AUTO: each variable is the column of its name
        let defines_list: Vec<Expr> = if mr.define_auto {
            in_pattern
                .iter()
                .map(|name| Expr::column(vec![name.clone()]).with_alias(name.clone()))
                .collect()
        } else {
            mr.defines.clone()
        };
        let mut names: Vec<String> = Vec::new();
        for define in &defines_list {
            if names.iter().any(|n| n.eq_ignore_ascii_case(&define.alias)) {
                return Err(binder_error(format!(
                    "MATCH_RECOGNIZE defines pattern variable \"{}\" more than once",
                    define.alias
                )));
            }
            if !in_pattern.iter().any(|n| n.eq_ignore_ascii_case(&define.alias)) {
                return Err(binder_error(format!(
                    "MATCH_RECOGNIZE defines \"{}\", which its PATTERN does not use",
                    define.alias
                )));
            }
            names.push(define.alias.clone());
        }
        for name in &in_pattern {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
        // `ValidateClauses`
        if has_exclusion(&mr.pattern) && mr.rows_per_match == RowsPerMatch::AllUnmatched {
            return Err(binder_error(
                "Pattern exclusion syntax {- -} is not allowed with ALL ROWS PER MATCH WITH UNMATCHED ROWS",
            ));
        }
        let mut measure_names: Vec<String> = Vec::new();
        for measure in &mr.measures {
            if measure_names.iter().any(|n| n.eq_ignore_ascii_case(&measure.alias)) {
                return Err(binder_error(format!("MEASURES names \"{}\" twice", measure.alias)));
            }
            measure_names.push(measure.alias.clone());
        }
        // `BuildMeasureSymbols`
        let mut symbols = Symbols {
            names: names.clone(),
            subsets: HashMap::new(),
        };
        let mut union_names: Vec<String> = Vec::new();
        for (subset, members) in &mr.subsets {
            if symbols.names.iter().any(|n| n.eq_ignore_ascii_case(subset)) {
                return Err(binder_error(format!(
                    "SUBSET name \"{subset}\" is already a pattern variable"
                )));
            }
            let mut indexes = Vec::new();
            for member in members {
                if union_names.iter().any(|n| n.eq_ignore_ascii_case(member)) {
                    return Err(binder_error(format!(
                        "SUBSET \"{subset}\" is built out of \"{member}\", which is itself a SUBSET - a union row \
                         pattern variable is a union of pattern variables"
                    )));
                }
                let Some(at) = symbols.names.iter().position(|n| n.eq_ignore_ascii_case(member)) else {
                    return Err(binder_error(format!(
                        "SUBSET \"{subset}\" refers to unknown pattern variable \"{member}\""
                    )));
                };
                indexes.push(at);
            }
            union_names.push(subset.clone());
            symbols.subsets.insert(subset.to_lowercase(), indexes);
        }
        if !mr.after_match_variable.is_empty() && symbols.lookup(&mr.after_match_variable).is_none() {
            return Err(binder_error(format!(
                "AFTER MATCH SKIP TO \"{}\", which is not a pattern variable of this MATCH_RECOGNIZE",
                mr.after_match_variable
            )));
        }

        // the columns of the input
        let mut star_binder = self.child_binder();
        star_binder.bind_table_ref(&mr.input)?;
        let mut input_columns = Vec::new();
        star_binder.expand_star_expression(&Expr::star(), &mut input_columns)?;
        let input_names: Vec<String> = input_columns
            .iter()
            .map(|c| column_name(c).unwrap_or_else(|| c.to_string()))
            .collect();

        // what the conditions and the measures read, as slots
        let mut hoisted: Vec<Expr> = Vec::new();
        let walked = mr
            .partitions
            .iter()
            .map(|p| (p, "PARTITION BY"))
            .chain(mr.orders.iter().map(|o| (&o.expr, "ORDER BY")));
        for (expr, clause) in walked {
            let scope = format!("{clause}, which the matcher walks rather than produces");
            let mut neighbour = None;
            pintail_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e| {
                if let ExprKind::Function(f) = &e.kind
                    && matches!(f.name.name.to_lowercase().as_str(), "prev" | "next")
                {
                    neighbour.get_or_insert_with(|| f.name.name.to_uppercase());
                }
            });
            if let Some(neighbour) = neighbour {
                return Err(binder_error(format!(
                    "{neighbour}() reads a neighbour in the order the matcher walks, so it cannot be part of {scope}"
                )));
            }
            if let Some(what) = match_construct(expr) {
                return Err(binder_error(format!(
                    "{what} only means something while the matcher is assembling a match, so it cannot be part of \
                     {scope}"
                )));
            }
        }
        let partitions: Vec<usize> = mr
            .partitions
            .iter()
            .map(|p| {
                hoisted.push(p.clone());
                hoisted.len() - 1
            })
            .collect();
        let orders: Vec<usize> = mr
            .orders
            .iter()
            .map(|o| {
                hoisted.push(o.expr.clone());
                hoisted.len() - 1
            })
            .collect();
        let mut defines: Vec<Option<(Expr, Vec<PendingSlot>)>> = vec![None; names.len()];
        for define in &defines_list {
            let at = names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&define.alias))
                .expect("collected above");
            let mut define = define.clone();
            self.inline_scalar_macros(&mut define, 0)?;
            let mut extraction = Extraction {
                symbols: &symbols,
                measures: None,
                hoisted: &mut hoisted,
                slots: Vec::new(),
                running: None,
                lambda_parameters: Vec::new(),
                defining: Some(at),
                input: &input_names,
                later: &[],
            };
            let residual = extraction.extract(&define)?;
            defines[at] = Some((residual, extraction.slots));
        }
        let mut measures = Vec::new();
        for (index, measure) in mr.measures.iter().enumerate() {
            let mut measure = measure.clone();
            self.inline_scalar_macros(&mut measure, 0)?;
            let mut extraction = Extraction {
                symbols: &symbols,
                measures: Some(measure_names[..index].to_vec()),
                hoisted: &mut hoisted,
                slots: Vec::new(),
                running: None,
                lambda_parameters: Vec::new(),
                defining: None,
                input: &input_names,
                later: &measure_names[index + 1..],
            };
            let residual = extraction.extract(&measure)?;
            measures.push((residual, extraction.slots));
        }

        // the input, with the hoisted columns after its own
        let mut select = SelectNode {
            from_table: (*mr.input).clone(),
            ..SelectNode::default()
        };
        // named apart, so an operand never reads one of them as an alias in place of the input's column
        select.select_list = input_columns
            .iter()
            .enumerate()
            .map(|(index, column)| column.clone().with_alias(format!("__mr_input_{index}")))
            .collect();
        for (index, expr) in hoisted.iter().enumerate() {
            select.select_list.push(expr.clone().with_alias(hoisted_name(index)));
        }
        let mut input_binder = self.child_binder();
        let bound_input = input_binder.bind_select_statement(&SelectStatement { node: select.into() })?;
        self.next_table_index = input_binder.next_table_index;
        let input_types = bound_input.types.clone();
        let input_count = input_columns.len();
        let hoisted_column = |index: usize| input_count + index;

        // what is left of each expression, over the input and its slots
        let input_index = self.generate_table_index();
        let slots_index = self.generate_table_index();
        let mut all_names = input_names.clone();
        all_names.extend((0..hoisted.len()).map(hoisted_name));
        let input_alias = if mr.input.alias.is_empty() {
            match &mr.input.kind {
                pintail_parser::ast::TableRefKind::BaseTable { name, .. } => name.name.clone(),
                _ => "__mr_input".to_string(),
            }
        } else {
            mr.input.alias.clone()
        };
        let mut measure_types: Vec<LogicalType> = Vec::new();
        let context = ResidualContext {
            input_types: &input_types,
            input_count,
            names: &all_names,
            alias: &input_alias,
            input_index,
            slots_index,
        };
        let mut bound_defines = Vec::with_capacity(defines.len());
        for define in &defines {
            bound_defines.push(match define {
                Some((residual, pending)) => {
                    let mut bound = self.bind_match_expression(&context, residual, pending, &[])?;
                    if bound.expr.return_type != LogicalType::BOOLEAN {
                        bound.expr = bound.expr.cast_to(&LogicalType::BOOLEAN, false);
                    }
                    Some(bound)
                }
                None => None,
            });
        }
        let mut bound_measures = Vec::with_capacity(measures.len());
        for (residual, pending) in &measures {
            let bound = self.bind_match_expression(&context, residual, pending, &measure_types)?;
            measure_types.push(bound.expr.return_type.clone());
            bound_measures.push(bound);
        }

        // what the operator outputs
        let all_rows = !matches!(mr.rows_per_match, RowsPerMatch::Default | RowsPerMatch::One);
        if !all_rows && mr.measures.is_empty() && mr.partitions.is_empty() {
            return Err(binder_error(
                "MATCH_RECOGNIZE with ONE ROW PER MATCH reports the match rather than its rows, so without MEASURES or \
                 PARTITION BY it has no columns to return",
            ));
        }
        let mut output = Vec::new();
        let mut output_names = Vec::new();
        let mut output_types = Vec::new();
        if all_rows {
            // `CollectReportedNames`: the columns of the input that the partitioning and the order name, then the
            // measures, then the rest of the input
            let mut leading: Vec<usize> = Vec::new();
            for expr in mr.partitions.iter().chain(mr.orders.iter().map(|o| &o.expr)) {
                if let Some(name) = column_name(expr)
                    && let Some(at) = input_names.iter().position(|n| n.eq_ignore_ascii_case(&name))
                    && !leading.contains(&at)
                {
                    leading.push(at);
                }
            }
            for &at in &leading {
                output.push(OutputColumn::Input(at));
                output_names.push(input_names[at].clone());
                output_types.push(input_types[at].clone());
            }
            for (index, measure) in bound_measures.iter().enumerate() {
                output.push(OutputColumn::Measure(index));
                output_names.push(measure_names[index].clone());
                output_types.push(measure.expr.return_type.clone());
            }
            for at in 0..input_count {
                if !leading.contains(&at) {
                    output.push(OutputColumn::Input(at));
                    output_names.push(input_names[at].clone());
                    output_types.push(input_types[at].clone());
                }
            }
        } else {
            for (index, expr) in mr.partitions.iter().enumerate() {
                output.push(OutputColumn::Input(hoisted_column(partitions[index])));
                output_names.push(column_name(expr).unwrap_or_else(|| expr.to_string()));
                output_types.push(input_types[hoisted_column(partitions[index])].clone());
            }
            for (index, measure) in bound_measures.iter().enumerate() {
                output.push(OutputColumn::Measure(index));
                output_names.push(measure_names[index].clone());
                output_types.push(measure.expr.return_type.clone());
            }
        }

        let skip_symbols = if mr.after_match_variable.is_empty() {
            Vec::new()
        } else {
            symbols.lookup(&mr.after_match_variable).expect("checked above")
        };
        // AFTER MATCH SKIP TO a variable is TO LAST of it
        let after_match = match mr.after_match {
            AfterMatchSkip::ToFirst | AfterMatchSkip::ToLast => mr.after_match,
            other => other,
        };
        let data = MatchRecognizeData {
            input_types: input_types.clone(),
            partitions: partitions.iter().map(|p| hoisted_column(*p)).collect(),
            orders: mr
                .orders
                .iter()
                .zip(&orders)
                .map(|(o, at)| (hoisted_column(*at), self.options.resolve_order(o.order, o.nulls)))
                .collect(),
            symbols: names.clone(),
            defines: bound_defines,
            pattern: build_pattern(&mr.pattern, &symbols)?,
            measures: bound_measures,
            rows_per_match: mr.rows_per_match,
            after_match,
            skip_symbols,
            skip_name: mr.after_match_variable.clone(),
            output,
            output_types: output_types.clone(),
        };
        let table_index = self.generate_table_index();
        let alias = if table_ref.alias.is_empty() {
            "unnamed_match_recognize".to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = super::statements::alias_column_names(&alias, &output_names, &table_ref.column_name_alias)?;
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            catalog: None,
            table_index,
            names,
            types: output_types,
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let _ = BoundExprKind::Reference(0);
        Ok(LogicalOperator::new(
            LogicalOperatorKind::MatchRecognize {
                table_index,
                data: Arc::new(data),
            },
            vec![bound_input.plan],
        ))
    }
}

/// What the expressions left over are bound against: the input with its hoisted columns, and the slots.
struct ResidualContext<'c> {
    input_types: &'c [LogicalType],
    input_count: usize,
    names: &'c [String],
    alias: &'c str,
    input_index: usize,
    slots_index: usize,
}

impl Binder<'_> {
    /// `expr` with its calls to scalar macros replaced by their bodies, so the aggregates over the match a macro
    /// calls are seen before anything is bound.
    fn inline_scalar_macros(&mut self, expr: &mut Expr, depth: usize) -> Result<()> {
        if depth >= 1000 {
            return Err(binder_error(
                "Max expression depth limit of 1000 exceeded. Use \"SET max_expression_depth TO x\" to increase the \
                 maximum expression depth.",
            ));
        }
        if let ExprKind::Function(call) = &expr.kind
            && !call.is_operator
            && !is_navigation(&call.name.name.to_lowercase())
            && let Some(entry) = self.find_macro(&call.name, false)?
            && !entry.table_macro
        {
            let call = call.clone();
            let (function, arguments) = self.bind_macro_function(&entry, &call)?;
            if let (MacroBody::Scalar(body), None) = (&function.body, &function.return_type) {
                let mut body = (**body).clone();
                super::macros::replace_in_expr(&mut body, &super::macros::parameter_names(function), &arguments);
                // a macro spelled as an aggregate hands DISTINCT, FILTER and ORDER BY to the aggregate it is
                if call.distinct || call.filter.is_some() || !call.order_by.is_empty() {
                    let ExprKind::Function(inner) = &mut body.kind else {
                        return Ok(());
                    };
                    inner.distinct |= call.distinct;
                    if call.filter.is_some() {
                        inner.filter = call.filter.clone();
                    }
                    if !call.order_by.is_empty() {
                        inner.order_by = call.order_by.clone();
                    }
                }
                *expr = body.with_alias(expr.alias.clone());
                return self.inline_scalar_macros(expr, depth + 1);
            }
        }
        let mut error = None;
        for_each_child(expr, &mut |child| {
            if let Err(e) = self.inline_scalar_macros(child, depth) {
                error.get_or_insert(e);
            }
        });
        error.map_or(Ok(()), Err)
    }

    fn bind_slot(
        &mut self,
        context: &ResidualContext<'_>,
        slot: &PendingSlot,
        measure_types: &[LogicalType],
    ) -> Result<Slot> {
        let hoisted = |index: usize| context.input_count + index;
        let (kind, ty) = match &slot.kind {
            PendingKind::Column { navigation, operand } => (
                SlotKind::Column {
                    navigation: navigation.clone(),
                    column: hoisted(*operand),
                },
                context.input_types[hoisted(*operand)].clone(),
            ),
            PendingKind::Classifier { navigation } => (
                SlotKind::Classifier {
                    navigation: navigation.clone(),
                },
                LogicalType::VARCHAR,
            ),
            PendingKind::MatchNumber => (SlotKind::MatchNumber, LogicalType::BIGINT),
            PendingKind::Measure(at) => (SlotKind::Measure(*at), measure_types[*at].clone()),
            PendingKind::Aggregate {
                symbols,
                name,
                operand,
                distinct,
                order,
                filter,
            } => {
                let filter = match filter {
                    Some((residual, pending)) => {
                        let mut bound = self.bind_match_expression(context, residual, pending, measure_types)?;
                        if bound.expr.return_type != LogicalType::BOOLEAN {
                            bound.expr = bound.expr.cast_to(&LogicalType::BOOLEAN, false);
                        }
                        Some(Box::new(bound))
                    }
                    None => None,
                };
                let set = BuiltinFunctions::get()
                    .aggregate(name)
                    .ok_or_else(|| binder_error(format!("Aggregate Function with name {name} does not exist!")))?;
                let operand = match operand {
                    None => None,
                    Some(PendingOperand::Hoisted(at)) => Some(AggregateOperand::Column(hoisted(*at))),
                    Some(PendingOperand::PerRow(residual, pending)) => Some(AggregateOperand::Expression(Box::new(
                        self.bind_match_expression(context, residual, pending, measure_types)?,
                    ))),
                };
                let children: Vec<BoundExpr> = match &operand {
                    Some(AggregateOperand::Column(column)) => vec![BoundExpr::column_ref(
                        ColumnBinding::new(0, 0),
                        context.input_types[*column].clone(),
                        "",
                    )],
                    Some(AggregateOperand::Expression(expr)) => {
                        vec![BoundExpr::column_ref(
                            ColumnBinding::new(0, 0),
                            expr.expr.return_type.clone(),
                            "",
                        )]
                    }
                    None => Vec::new(),
                };
                let bound = bind_aggregate_function(set, children, None, *distinct)?;
                let ty = bound.return_type.clone();
                (
                    SlotKind::Aggregate {
                        symbols: symbols.clone(),
                        function: bound.function.clone(),
                        return_type: ty.clone(),
                        operand,
                        distinct: *distinct,
                        order: order
                            .iter()
                            .map(|(at, o, n)| (hoisted(*at), self.options.resolve_order(*o, *n)))
                            .collect(),
                        filter,
                    },
                    ty,
                )
            }
        };
        Ok(Slot {
            kind,
            ty,
            running: slot.running,
        })
    }

    /// What is left of a condition or a measure, bound over the input and its slots.
    fn bind_match_expression(
        &mut self,
        context: &ResidualContext<'_>,
        residual: &Expr,
        pending: &[PendingSlot],
        measure_types: &[LogicalType],
    ) -> Result<MatchExpression> {
        let mut slots = Vec::with_capacity(pending.len());
        for slot in pending {
            slots.push(self.bind_slot(context, slot, measure_types)?);
        }
        let mut residual_binder = self.child_binder();
        residual_binder.bind_context.add_binding(Binding {
            alias: context.alias.to_string(),
            schema: None,
            catalog: None,
            table_index: context.input_index,
            names: context.names.to_vec(),
            types: context.input_types.to_vec(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        residual_binder.bind_context.add_binding(Binding {
            alias: SLOTS.to_string(),
            schema: None,
            catalog: None,
            table_index: context.slots_index,
            names: (0..slots.len()).map(slot_name).collect(),
            types: slots.iter().map(|s| s.ty.clone()).collect(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let mut expr = residual_binder.bind_expression(residual)?;
        self.next_table_index = self.next_table_index.max(residual_binder.next_table_index);
        let mut bindings: Vec<ColumnBinding> = (0..context.input_types.len())
            .map(|c| ColumnBinding::new(context.input_index, c))
            .collect();
        bindings.extend((0..slots.len()).map(|s| ColumnBinding::new(context.slots_index, s)));
        resolve_expression(&mut expr, &bindings)?;
        Ok(MatchExpression { expr, slots })
    }
}
