// Ported from duckdb/src/optimizer/expression_rewriter.cpp and duckdb/src/optimizer/rule/
// {constant_folding,conjunction_simplification,arithmetic_simplification,case_simplification,
// constant_order_normalization,empty_needle_removal,like_optimizations,move_constants,
// comparison_simplification,distributivity,not_comparison_simplification,
// not_conjunction_simplification}.cpp @ rust-port-baseline
//
//! `ExpressionRewriter`: rules that simplify expressions without changing the plan's shape. C++
//! has a matcher per rule; here each rule is a function that looks at the expression's kind. Rules
//! that need something the engine lacks are left out.

use super::conjunction_of;
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::function::date_part_call_of;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_parser::ast::ComparisonOp;

/// A rule changing an expression can enable another, also on the expressions below it; this many
/// rounds is more than any expression takes.
const MAX_ROUNDS: usize = 32;

/// `ExpressionRewriter::VisitOperator`: rewrites every expression of the plan, bottom-up.
pub fn rewrite_plan(op: &mut LogicalOperator) {
    for child in &mut op.children {
        rewrite_plan(child);
    }
    if let LogicalOperatorKind::LateralJoin { right, .. } = &mut op.kind {
        rewrite_plan(right);
    }
    // the expressions of RecursiveCte's recursive part are resolved already; the binder rewrote them
    let is_filter = matches!(op.kind, LogicalOperatorKind::Filter { .. });
    // each conjunct of a filter is a root of its own
    if let LogicalOperatorKind::Filter { expressions } = &mut op.kind {
        split_predicates(expressions);
    }
    for expr in op.expressions_mut() {
        rewrite_expression(expr, is_filter);
    }
    if let LogicalOperatorKind::Join { conditions, .. }
    | LogicalOperatorKind::DelimJoin { conditions, .. }
    | LogicalOperatorKind::MarkJoin { conditions, .. } = &mut op.kind
    {
        for condition in conditions {
            rewrite_expression(&mut condition.left, false);
            rewrite_expression(&mut condition.right, false);
        }
    }
    if let LogicalOperatorKind::Filter { expressions } = &mut op.kind {
        split_predicates(expressions);
    }
}

/// `LogicalFilter::SplitPredicates`: the conjuncts of a filter become expressions of their own.
pub(crate) fn split_predicates(expressions: &mut Vec<BoundExpr>) {
    let mut result = Vec::with_capacity(expressions.len());
    for expr in expressions.drain(..) {
        split_conjuncts(expr, &mut result);
    }
    *expressions = result;
}

pub(crate) fn split_conjuncts(expr: BoundExpr, result: &mut Vec<BoundExpr>) {
    match expr.kind {
        BoundExprKind::Conjunction { and: true, children } => {
            for child in children {
                split_conjuncts(child, result);
            }
        }
        _ => result.push(expr),
    }
}

/// `ExpressionRewriter::ApplyRules`. `in_filter` says the expression is the root of a filter,
/// where NULL has the same effect as false.
pub(crate) fn rewrite_expression(expr: &mut BoundExpr, in_filter: bool) {
    for _ in 0..MAX_ROUNDS {
        for child in expr.children_mut() {
            rewrite_expression(child, false);
        }
        let alias = expr.alias.clone();
        if !apply_rules(expr, in_filter) {
            return;
        }
        if !alias.is_empty() {
            expr.alias = alias;
        }
    }
}

fn apply_rules(expr: &mut BoundExpr, in_filter: bool) -> bool {
    constant_folding(expr)
        || equal_or_null_simplification(expr, in_filter)
        || date_part_simplification(expr)
        || distributivity(expr)
        || arithmetic_simplification(expr)
        || constant_order_normalization(expr)
        || like_optimizations(expr)
        || empty_needle_removal(expr)
        || move_constants(expr)
        || case_simplification(expr)
        || conjunction_simplification(expr, in_filter)
        || comparison_simplification(expr)
        || not_comparison_simplification(expr)
        || not_conjunction_simplification(expr)
}

/// `EqualOrNullSimplification`: `a = b OR (a IS NULL AND b IS NULL)` is `a IS NOT DISTINCT FROM b`
/// where NULL counts as false. Elsewhere they differ: the first is NULL for a NULL and a value.
fn equal_or_null_simplification(expr: &mut BoundExpr, in_filter: bool) -> bool {
    if !in_filter {
        return false;
    }
    let BoundExprKind::Conjunction { and: false, children } = &expr.kind else {
        return false;
    };
    let [first, second] = children.as_slice() else {
        return false;
    };
    let Some(rewritten) = equal_or_is_null(first, second).or_else(|| equal_or_is_null(second, first)) else {
        return false;
    };
    expr.kind = rewritten;
    true
}

/// `TryRewriteEqualOrIsNull`.
fn equal_or_is_null(equal: &BoundExpr, nulls: &BoundExpr) -> Option<BoundExprKind> {
    let BoundExprKind::Comparison {
        op: ComparisonOp::Equal,
        left,
        right,
    } = &equal.kind
    else {
        return None;
    };
    let BoundExprKind::Conjunction { and: true, children } = &nulls.kind else {
        return None;
    };
    let [first, second] = children.as_slice() else {
        return None;
    };
    let tested = |expr: &'_ BoundExpr| match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::IsNull,
            children,
        } => children.first().cloned(),
        _ => None,
    };
    let (first, second) = (tested(first)?, tested(second)?);
    let same = |a: &BoundExpr, b: &BoundExpr| expressions_equal(a, b);
    let both = (same(&first, left) && same(&second, right)) || (same(&first, right) && same(&second, left));
    both.then(|| BoundExprKind::Comparison {
        op: ComparisonOp::NotDistinctFrom,
        left: left.clone(),
        right: right.clone(),
    })
}

/// `DatePartSimplificationRule`: `date_part('year', x)` is `year(x)`.
fn date_part_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    let [specifier, value] = children.as_slice() else {
        return false;
    };
    let BoundExprKind::Constant(constant) = &specifier.kind else {
        return false;
    };
    if function.name != "date_part" || constant.logical_type() != &LogicalType::VARCHAR {
        return false;
    }
    if constant.is_null() {
        expr.kind = BoundExprKind::Constant(Value::null(expr.return_type.clone()));
        return true;
    }
    let Some(call) = constant
        .try_to_string()
        .ok()
        .and_then(|name| date_part_call_of(&name, value.clone()))
    else {
        return false;
    };
    if call.return_type != expr.return_type {
        return false;
    }
    expr.kind = call.kind;
    true
}

/// `ConstantFoldingRule`. A value of another type than the expression's stays an expression: a
/// constant's vector has the type of its value.
fn constant_folding(expr: &mut BoundExpr) -> bool {
    if matches!(expr.kind, BoundExprKind::Constant(_)) || !expr.is_foldable() {
        return false;
    }
    let Some(value) = try_evaluate_scalar(expr) else {
        return false;
    };
    if value.logical_type() != &expr.return_type {
        return false;
    }
    expr.kind = BoundExprKind::Constant(value);
    true
}

fn take(expr: &mut BoundExpr) -> BoundExpr {
    std::mem::replace(expr, BoundExpr::constant(Value::boolean(false)))
}

/// The value of a foldable expression as a boolean: `Some(None)` for NULL, and `None` if it does
/// not evaluate to one.
pub(crate) fn fold_boolean(expr: &BoundExpr) -> Option<Option<bool>> {
    if !expr.is_foldable() {
        return None;
    }
    let value = try_evaluate_scalar(expr)?;
    match value.data() {
        ValueData::Null => Some(None),
        ValueData::Bool(b) => Some(Some(*b)),
        _ => None,
    }
}

/// `ConjunctionSimplificationRule`.
fn conjunction_simplification(expr: &mut BoundExpr, in_filter: bool) -> bool {
    let BoundExprKind::Conjunction { and, children } = &mut expr.kind else {
        return false;
    };
    let and = *and;
    let mut changed = false;
    let mut index = 0;
    while index < children.len() {
        let Some(value) = fold_boolean(&children[index]) else {
            index += 1;
            continue;
        };
        match value {
            // FALSE and NULL have the same effect in filters
            None if and || !in_filter => index += 1,
            Some(v) if v != and => {
                *expr = BoundExpr::constant(Value::boolean(v));
                return true;
            }
            _ => {
                children.remove(index);
                changed = true;
            }
        }
    }
    match children.len() {
        0 => {
            // everything was removed: only the neutral values, or NULLs in a filter's OR
            *expr = BoundExpr::constant(if and {
                Value::boolean(true)
            } else {
                Value::null(LogicalType::BOOLEAN)
            });
            true
        }
        1 => {
            *expr = children.remove(0);
            true
        }
        _ => changed,
    }
}

/// `ArithmeticSimplificationRule`, without `x * 0`, which needs `constant_or_null`.
fn arithmetic_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function {
        function,
        children,
        is_operator: true,
    } = &mut expr.kind
    else {
        return false;
    };
    if children.len() != 2 || !matches!(function.name.as_str(), "+" | "-" | "*" | "/" | "//") {
        return false;
    }
    if !is_numeric(&expr.return_type) || !children.iter().all(|c| is_numeric(&c.return_type)) {
        return false;
    }
    let constant_child = match (&children[0].kind, &children[1].kind) {
        (BoundExprKind::Constant(_), _) => 0,
        (_, BoundExprKind::Constant(_)) => 1,
        _ => return false,
    };
    let BoundExprKind::Constant(constant) = &children[constant_child].kind else {
        return false;
    };
    if constant.is_null() {
        expr.kind = BoundExprKind::Constant(Value::null(expr.return_type.clone()));
        return true;
    }
    let number = constant.as_i128();
    let is = |n: i128| number == Some(n);
    let name = function.name.as_str();
    let other = 1 - constant_child;
    let keep_other = if name == "/" {
        let float = matches!(
            children[0].return_type.id(),
            LogicalTypeId::Float | LogicalTypeId::Double
        );
        let one = matches!(constant.data(), ValueData::Float(v) if *v == 1.0)
            || matches!(constant.data(), ValueData::Double(v) if *v == 1.0)
            || is(1);
        constant_child == 1 && one && float
    } else if !is_integral(&expr.return_type) {
        false
    } else {
        match name {
            "+" => is(0),
            "-" => constant_child == 1 && is(0),
            "*" => is(1),
            _ => constant_child == 1 && is(1),
        }
    };
    if !keep_other {
        return false;
    }
    let return_type = expr.return_type.clone();
    let child = take(&mut children[other]);
    *expr = child.cast_to(&return_type, false);
    true
}

fn is_integral(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(
        ty.id(),
        TinyInt | SmallInt | Integer | BigInt | HugeInt | UTinyInt | USmallInt | UInteger | UBigInt | UHugeInt
    )
}

fn is_numeric(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    is_integral(ty) || matches!(ty.id(), Float | Double | Decimal)
}

/// `CaseSimplificationRule`.
fn case_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Case { checks, else_expr } = &mut expr.kind else {
        return false;
    };
    let mut changed = false;
    let mut index = 0;
    while index < checks.len() {
        match fold_boolean(&checks[index].0) {
            Some(Some(true)) => {
                **else_expr = checks[index].1.clone();
                checks.truncate(index);
                changed = true;
                break;
            }
            Some(_) => {
                checks.remove(index);
                changed = true;
            }
            None => index += 1,
        }
    }
    if checks.is_empty() {
        let return_type = expr.return_type.clone();
        let result = take(else_expr);
        *expr = result.cast_to(&return_type, false);
        return true;
    }
    changed
}

/// The start of `ComparisonSimplificationRule`: comparing with NULL gives NULL, unless it is
/// `[NOT] DISTINCT FROM`.
fn comparison_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return false;
    };
    if matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
        return false;
    }
    let is_null = |e: &BoundExpr| matches!(&e.kind, BoundExprKind::Constant(v) if v.is_null());
    if !is_null(left) && !is_null(right) || expr.is_volatile() {
        return false;
    }
    expr.kind = BoundExprKind::Constant(Value::null(LogicalType::BOOLEAN));
    true
}

/// `NotComparisonSimplificationRule`.
fn not_comparison_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::Not,
        children,
    } = &mut expr.kind
    else {
        return false;
    };
    let BoundExprKind::Comparison { op, .. } = &mut children[0].kind else {
        return false;
    };
    *op = op.negate();
    let child = children.remove(0);
    *expr = child;
    true
}

/// `NotConjunctionSimplificationRule`.
fn not_conjunction_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::Not,
        children,
    } = &mut expr.kind
    else {
        return false;
    };
    if !matches!(children[0].kind, BoundExprKind::Conjunction { .. })
        || children[0].is_volatile()
        || can_throw(&children[0])
    {
        return false;
    }
    let BoundExprKind::Conjunction { and, children: inner } = children.remove(0).kind else {
        return false;
    };
    let negated = inner
        .into_iter()
        .map(|child| {
            BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::Not,
                    children: vec![child],
                },
                LogicalType::BOOLEAN,
            )
        })
        .collect();
    *expr = BoundExpr::new(
        BoundExprKind::Conjunction {
            and: !and,
            children: negated,
        },
        LogicalType::BOOLEAN,
    );
    true
}

/// `DistributivityRule`: `(X AND A) OR (X AND B)` is `X AND (A OR B)`.
fn distributivity(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Conjunction { and: false, children } = &mut expr.kind else {
        return false;
    };
    let branch = |e: &BoundExpr| -> Vec<BoundExpr> {
        match &e.kind {
            BoundExprKind::Conjunction { and: true, children } => children.clone(),
            _ => vec![e.clone()],
        }
    };
    let mut common = branch(&children[0]);
    for child in &children[1..] {
        let next = branch(child);
        common.retain(|c| next.iter().any(|n| expressions_equal(c, n)));
    }
    common.retain(|c| !c.is_volatile());
    // the first of equal expressions is enough
    let mut unique: Vec<BoundExpr> = Vec::new();
    for c in common {
        if !unique.iter().any(|u| expressions_equal(u, &c)) {
            unique.push(c);
        }
    }
    if unique.is_empty() {
        return false;
    }
    // each branch without the common expressions; one with nothing left makes the OR redundant:
    // `X OR (X AND A)` is `X`
    let mut remaining = Vec::with_capacity(children.len());
    let mut redundant = false;
    for child in children.iter() {
        let mut rest = branch(child);
        rest.retain(|c| !unique.iter().any(|u| expressions_equal(u, c)));
        if rest.is_empty() {
            redundant = true;
            break;
        }
        remaining.push(conjunction_of(rest));
    }
    if !redundant {
        unique.push(if remaining.len() == 1 {
            remaining.remove(0)
        } else {
            BoundExpr::new(
                BoundExprKind::Conjunction {
                    and: false,
                    children: remaining,
                },
                LogicalType::BOOLEAN,
            )
        });
    }
    *expr = conjunction_of(unique);
    true
}

/// `Expression::Equals`. Expressions of a kind no rule compares are never equal, which only
/// misses a rewrite.
pub(crate) fn expressions_equal(a: &BoundExpr, b: &BoundExpr) -> bool {
    use BoundExprKind as K;
    let all_equal =
        |x: &[BoundExpr], y: &[BoundExpr]| x.len() == y.len() && x.iter().zip(y).all(|(x, y)| expressions_equal(x, y));
    if a.return_type != b.return_type {
        return false;
    }
    match (&a.kind, &b.kind) {
        (K::Constant(x), K::Constant(y)) => x == y,
        (K::ColumnRef { binding: x, depth: d }, K::ColumnRef { binding: y, depth: e }) => x == y && d == e,
        (K::Reference(x), K::Reference(y)) => x == y,
        (K::Cast { child: x, try_cast: p }, K::Cast { child: y, try_cast: q }) => p == q && expressions_equal(x, y),
        (
            K::Comparison {
                op: o,
                left: l,
                right: r,
            },
            K::Comparison {
                op: p,
                left: m,
                right: s,
            },
        ) => o == p && expressions_equal(l, m) && expressions_equal(r, s),
        (K::Conjunction { and: x, children: c }, K::Conjunction { and: y, children: d }) => x == y && all_equal(c, d),
        (K::Operator { op: x, children: c }, K::Operator { op: y, children: d }) => x == y && all_equal(c, d),
        (
            K::Between {
                input: i,
                lower: l,
                upper: u,
            },
            K::Between {
                input: j,
                lower: m,
                upper: v,
            },
        ) => expressions_equal(i, j) && expressions_equal(l, m) && expressions_equal(u, v),
        (
            K::Function {
                function: f,
                children: c,
                ..
            },
            K::Function {
                function: g,
                children: d,
                ..
            },
        ) => f.name == g.name && f.arguments == g.arguments && all_equal(c, d),
        _ => false,
    }
}

/// The functions that never fail on any input. C++ marks the ones that can
/// (`FunctionErrors::CAN_THROW_RUNTIME_ERROR`); the opposite is the safe side here, since an
/// expression that can fail only stays where it is.
const INFALLIBLE_FUNCTIONS: &[&str] = &[
    "floor",
    "ceil",
    "ceiling",
    "year",
    "decade",
    "century",
    "millennium",
    "/",
    "%",
    "lower",
    "upper",
    "lcase",
    "ucase",
    "length",
    "strlen",
    "concat",
    "||",
    "prefix",
    "suffix",
    "contains",
    "starts_with",
    "ends_with",
    "~~",
    "!~~",
    "isnan",
    "isinf",
    "isfinite",
    "greatest",
    "least",
    "year",
    "month",
    "day",
];

/// `Expression::CanThrow`, leaning towards true.
pub(crate) fn can_throw(expr: &BoundExpr) -> bool {
    let own = match &expr.kind {
        BoundExprKind::Subquery { .. } | BoundExprKind::ListLambda { .. } | BoundExprKind::Sequence { .. } => true,
        BoundExprKind::Cast { child, try_cast } => !try_cast && cast_can_throw(&child.return_type, &expr.return_type),
        BoundExprKind::Function { function, children, .. } => {
            let floating = matches!(function.name.as_str(), "+" | "-" | "*")
                && children
                    .iter()
                    .all(|c| matches!(c.return_type.id(), LogicalTypeId::Float | LogicalTypeId::Double));
            !floating && !INFALLIBLE_FUNCTIONS.contains(&function.name.as_str())
        }
        _ => false,
    };
    own || expr.children().into_iter().any(can_throw)
}

fn cast_can_throw(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    if source == target || target.id() == Varchar {
        return false;
    }
    let width = |id: LogicalTypeId| match id {
        TinyInt => Some(1),
        SmallInt => Some(2),
        Integer => Some(4),
        BigInt => Some(8),
        HugeInt => Some(16),
        _ => None,
    };
    match (width(source.id()), target.id()) {
        (Some(_), Float | Double) => false,
        (Some(from), id) => width(id).is_none_or(|to| from > to),
        (None, _) => true,
    }
}

fn integer_of(value: &Value) -> Option<i128> {
    value.try_to_string().ok()?.parse().ok()
}

/// `n` as a value of the integer type `ty`, if it fits.
fn integer_value(n: i128, ty: &LogicalType) -> Option<Value> {
    let cast = BoundExpr::constant(Value::varchar(n.to_string())).cast_to(ty, false);
    try_evaluate_scalar(&cast).filter(|value| value.logical_type() == ty)
}

fn integer_constant(expr: &BoundExpr) -> Option<i128> {
    match &expr.kind {
        BoundExprKind::Constant(value) if expr.return_type.is_integral() && !value.is_null() => integer_of(value),
        _ => None,
    }
}

/// `MoveConstantsRule`: `x + 1 = 10` becomes `x = 9`, for integers.
fn move_constants(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &mut expr.kind else {
        return false;
    };
    let (constant, arithmetic) = if matches!(left.kind, BoundExprKind::Constant(_)) {
        (left, right)
    } else {
        (right, left)
    };
    let Some(outer) = integer_constant(constant) else {
        return false;
    };
    let BoundExprKind::Function {
        function,
        children,
        is_operator: true,
    } = &arithmetic.kind
    else {
        return false;
    };
    if !arithmetic.return_type.is_integral() {
        return false;
    }
    // `-x < -5` is `x > 5`
    if let ("-", [child]) = (function.name.as_str(), children.as_slice()) {
        let Some(value) = outer
            .checked_neg()
            .and_then(|n| integer_value(n, &constant.return_type))
        else {
            return false;
        };
        let child = child.clone();
        **constant = BoundExpr::constant(value);
        **arithmetic = child;
        *op = super::statistics::flip(*op);
        return true;
    }
    if children.len() != 2 {
        return false;
    }
    let (inner, column) = match (integer_constant(&children[0]), integer_constant(&children[1])) {
        (Some(inner), None) => (inner, 1),
        (None, Some(inner)) => (inner, 0),
        _ => return false,
    };
    let is_equal = *op == ComparisonOp::Equal;
    let is_not_equal = *op == ComparisonOp::NotEqual;
    let mut flip = false;
    let (moved, unequal) = match (function.name.as_str(), column) {
        ("+", _) => (outer.checked_sub(inner), is_equal),
        ("-", 0) => (outer.checked_add(inner), is_equal),
        ("-", _) => {
            flip = true;
            (inner.checked_sub(outer), is_equal)
        }
        ("*", _) if inner == 0 => return false,
        ("*", _) if outer.checked_rem(inner) != Some(0) => {
            if !is_equal && !is_not_equal {
                return false;
            }
            let child = children[column].clone();
            let answer = BoundExpr::constant(Value::boolean(is_not_equal));
            *expr = crate::function::constant_or_null(vec![answer, child]);
            return true;
        }
        ("*", _) => {
            flip = inner < 0;
            (outer.checked_div(inner), is_equal || is_not_equal)
        }
        _ => return false,
    };
    let Some(moved) = moved else {
        return false;
    };
    let Some(value) = integer_value(moved, &constant.return_type) else {
        // no value of the type is equal to it
        if !unequal {
            return false;
        }
        let child = children[column].clone();
        let answer = BoundExpr::constant(Value::boolean(is_not_equal));
        *expr = crate::function::constant_or_null(vec![answer, child]);
        return true;
    };
    let child = children[column].clone();
    **constant = BoundExpr::constant(value);
    **arithmetic = child;
    if flip {
        *op = super::statistics::flip(*op);
    }
    true
}

/// `ConstantOrderNormalizationRule`: the constants of a chain of `+` or `*` of integers go in
/// front, so that they fold, and a single constant of a `*` goes last.
fn constant_order_normalization(expr: &mut BoundExpr) -> bool {
    fn leaves<'a>(expr: &'a BoundExpr, name: &str, ty: &LogicalType, result: &mut Vec<&'a BoundExpr>) {
        if let BoundExprKind::Function { function, children, .. } = &expr.kind
            && function.name == name
            && children.len() == 2
            && &expr.return_type == ty
        {
            leaves(&children[0], name, ty, result);
            leaves(&children[1], name, ty, result);
        } else {
            result.push(expr);
        }
    }
    let BoundExprKind::Function {
        function,
        children,
        is_operator: true,
    } = &expr.kind
    else {
        return false;
    };
    let name = function.name.as_str();
    if !matches!(name, "+" | "*") || children.len() != 2 || !expr.return_type.is_integral() {
        return false;
    }
    let mut all = Vec::new();
    leaves(expr, name, &expr.return_type, &mut all);
    let last = all.iter().rposition(|leaf| leaf.is_foldable());
    let (constants, others): (Vec<&BoundExpr>, Vec<&BoundExpr>) = all.iter().partition(|leaf| leaf.is_foldable());
    let Some(last) = last else {
        return false;
    };
    let order: Vec<&BoundExpr> = if constants.len() == 1 {
        if name != "*" || last == all.len() - 1 {
            return false;
        }
        others.iter().chain(&constants).copied().collect()
    } else {
        if last == constants.len() - 1 {
            return false;
        }
        constants.iter().chain(&others).copied().collect()
    };
    let (function, ty, alias) = (function.clone(), expr.return_type.clone(), expr.alias.clone());
    let mut leaves = order.into_iter().cloned();
    let first = leaves.next().expect("a chain has leaves");
    let rebuilt = leaves.fold(first, |left, right| {
        BoundExpr::new(
            BoundExprKind::Function {
                function: function.clone(),
                children: vec![left, right],
                is_operator: true,
            },
            ty.clone(),
        )
    });
    *expr = rebuilt.with_alias(alias);
    true
}

/// `LikeOptimizationRule`: a LIKE pattern with only wildcards at its ends is a plain comparison.
fn like_optimizations(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    let negated = match function.name.as_str() {
        "~~" => false,
        "!~~" => true,
        _ => return false,
    };
    let [input, pattern] = children.as_slice() else {
        return false;
    };
    let BoundExprKind::Constant(pattern) = &pattern.kind else {
        return false;
    };
    if pattern.is_null() || pattern.logical_type() != &LogicalType::VARCHAR {
        return false;
    }
    let Ok(pattern) = pattern.try_to_string() else {
        return false;
    };
    let needle = pattern.trim_matches('%');
    if needle.contains(['%', '_']) {
        return false;
    }
    let needle_expr = BoundExpr::constant(Value::varchar(needle));
    let (leading, trailing) = (pattern.starts_with('%'), pattern.ends_with('%'));
    let input = input.clone();
    let matcher = match (leading, trailing) {
        (false, false) => {
            let op = if negated {
                ComparisonOp::NotEqual
            } else {
                ComparisonOp::Equal
            };
            *expr = BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(input),
                    right: Box::new(needle_expr),
                },
                LogicalType::BOOLEAN,
            );
            return true;
        }
        (false, true) => "prefix",
        (true, false) => "suffix",
        (true, true) => "contains",
    };
    let call = crate::function::string_needle_call(matcher, input, needle_expr);
    *expr = if negated {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children: vec![call],
            },
            LogicalType::BOOLEAN,
        )
    } else {
        call
    };
    true
}

/// `EmptyNeedleRemovalRule`: every string starts with, ends with and contains the empty string.
fn empty_needle_removal(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    if !matches!(function.name.as_str(), "prefix" | "suffix" | "contains") || children.len() != 2 {
        return false;
    }
    if !children[1].is_foldable() || children[1].return_type != LogicalType::VARCHAR {
        return false;
    }
    let Some(needle) = try_evaluate_scalar(&children[1]) else {
        return false;
    };
    if needle.is_null() {
        *expr = BoundExpr::constant(Value::null(LogicalType::BOOLEAN));
        return true;
    }
    if needle.try_to_string().ok().as_deref() != Some("") {
        return false;
    }
    let input = children[0].clone();
    *expr = crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(true)), input]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::expression::ColumnBinding;

    fn column(index: usize) -> BoundExpr {
        BoundExpr::column_ref(ColumnBinding::new(1, index), LogicalType::BOOLEAN, "c")
    }

    fn conjunction(and: bool, children: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr::new(BoundExprKind::Conjunction { and, children }, LogicalType::BOOLEAN)
    }

    fn not(child: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children: vec![child],
            },
            LogicalType::BOOLEAN,
        )
    }

    fn truth(value: Option<bool>) -> BoundExpr {
        match value {
            Some(v) => BoundExpr::constant(Value::boolean(v)),
            None => BoundExpr::constant(Value::null(LogicalType::BOOLEAN)),
        }
    }

    fn rewritten(mut expr: BoundExpr, in_filter: bool) -> BoundExpr {
        rewrite_expression(&mut expr, in_filter);
        expr
    }

    #[test]
    fn true_in_and_goes_away_and_false_decides() {
        let rest = rewritten(conjunction(true, vec![truth(Some(true)), column(0)]), false);
        assert!(expressions_equal(&rest, &column(0)));
        let decided = rewritten(conjunction(true, vec![column(0), truth(Some(false))]), false);
        assert!(expressions_equal(&decided, &truth(Some(false))));
    }

    #[test]
    fn null_in_or_only_goes_away_in_a_filter() {
        let expr = conjunction(false, vec![truth(None), column(0)]);
        assert!(expressions_equal(&rewritten(expr.clone(), true), &column(0)));
        // outside of a filter NULL OR x is not x: NOT (NULL OR false) is NULL, not true
        assert!(matches!(
            rewritten(expr, false).kind,
            BoundExprKind::Conjunction { and: false, .. }
        ));
    }

    #[test]
    fn distributivity_takes_out_the_common_part() {
        let branch = |other| conjunction(true, vec![column(0), column(other)]);
        let expr = rewritten(conjunction(false, vec![branch(1), branch(2)]), false);
        let expected = conjunction(true, vec![column(0), conjunction(false, vec![column(1), column(2)])]);
        assert!(expressions_equal(&expr, &expected));
    }

    #[test]
    fn distributivity_drops_a_branch_that_adds_nothing() {
        let expr = conjunction(false, vec![column(0), conjunction(true, vec![column(0), column(1)])]);
        assert!(expressions_equal(&rewritten(expr, false), &column(0)));
    }

    #[test]
    fn not_moves_into_conjunctions() {
        let expr = rewritten(not(conjunction(true, vec![column(0), column(1)])), false);
        let expected = conjunction(false, vec![not(column(0)), not(column(1))]);
        assert!(expressions_equal(&expr, &expected));
    }

    #[test]
    fn a_failing_constant_stays_an_expression() {
        let cast = BoundExpr::new(
            BoundExprKind::Cast {
                child: Box::new(BoundExpr::constant(Value::varchar("abc"))),
                try_cast: false,
            },
            LogicalType::INTEGER,
        );
        assert!(matches!(rewritten(cast, false).kind, BoundExprKind::Cast { .. }));
    }
}
