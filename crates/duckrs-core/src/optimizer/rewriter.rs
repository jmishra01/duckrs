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
use std::collections::{BTreeMap, BTreeSet};

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
        || regex_optimization(expr)
        || like_optimizations(expr)
        || instr_prefix(expr)
        || string_prefix(expr)
        || glob_prefix(expr)
        || (in_filter && null_distinct_comparison(expr))
        || empty_needle_removal(expr)
        || least_greatest_simplification(expr)
        || not_constant_or_null_simplification(expr)
        || move_constants(expr)
        || case_simplification(expr)
        || conjunction_simplification(expr, in_filter)
        || comparison_simplification(expr)
        || cast_comparison_simplification(expr)
        || not_comparison_simplification(expr)
        || not_conjunction_simplification(expr)
        || join_dependent_filter(expr, in_filter)
}

/// The tables an expression reads, if all it reads is in the query itself.
fn tables_read(expr: &BoundExpr, tables: &mut BTreeSet<usize>) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => {
            tables.insert(binding.table_index);
        }
        BoundExprKind::ColumnRef { .. } => return false,
        _ => {}
    }
    expr.children().into_iter().all(|child| tables_read(child, tables))
}

/// `ExtractConjunctedExpressions`: the conjuncts of `expr` that read one table, by that table. They
/// leave out what can fail: they would run on rows the OR might not have got to.
fn conjuncts_per_table(expr: &BoundExpr, result: &mut BTreeMap<usize, Vec<BoundExpr>>) {
    if let BoundExprKind::Conjunction { and: true, children } = &expr.kind {
        for child in children {
            conjuncts_per_table(child, result);
        }
        return;
    }
    let mut tables = BTreeSet::new();
    if tables_read(expr, &mut tables) && tables.len() == 1 && !expr.is_volatile() && !can_throw(expr) {
        result
            .entry(*tables.first().expect("just checked"))
            .or_default()
            .push(expr.clone());
    }
}

/// `JoinDependentFilterRule`: `(a.x = 1 AND b.y = 2) OR (a.x = 3 AND b.y = 4)` needs a join to tell,
/// but a row of `a` can only pass with `a.x = 1 OR a.x = 3`, whichever `b` it meets. Those filters
/// go next to the original, which stays, and can then go down to the tables.
fn join_dependent_filter(expr: &mut BoundExpr, in_filter: bool) -> bool {
    if !in_filter {
        return false;
    }
    let BoundExprKind::Conjunction { and: false, children } = &expr.kind else {
        return false;
    };
    let eligible = children.iter().any(|child| {
        let mut tables = BTreeSet::new();
        tables_read(child, &mut tables);
        matches!(child.kind, BoundExprKind::Conjunction { and: true, .. }) && tables.len() > 1
    });
    if !eligible {
        return false;
    }
    let branches: Vec<BTreeMap<usize, Vec<BoundExpr>>> = children
        .iter()
        .map(|child| {
            let mut conjuncts = BTreeMap::new();
            conjuncts_per_table(child, &mut conjuncts);
            conjuncts
        })
        .collect();
    let mut derived = Vec::new();
    for table in branches[0].keys() {
        // a branch without the table says nothing about it
        let per_branch: Option<Vec<BoundExpr>> = branches
            .iter()
            .map(|branch| branch.get(table).map(|conjuncts| conjunction_of(conjuncts.clone())))
            .collect();
        if let Some(children) = per_branch {
            let kind = BoundExprKind::Conjunction { and: false, children };
            derived.push(BoundExpr::new(kind, LogicalType::BOOLEAN));
        }
    }
    if derived.is_empty() {
        return false;
    }
    derived.insert(0, take(expr));
    expr.kind = BoundExprKind::Conjunction {
        and: true,
        children: derived,
    };
    true
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

/// The smallest and largest value of an integer type.
fn integral_range(ty: &LogicalType) -> Option<(i128, i128)> {
    use LogicalTypeId::*;
    Some(match ty.id() {
        TinyInt => (i128::from(i8::MIN), i128::from(i8::MAX)),
        SmallInt => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Integer => (i128::from(i32::MIN), i128::from(i32::MAX)),
        BigInt => (i128::from(i64::MIN), i128::from(i64::MAX)),
        HugeInt => (i128::MIN, i128::MAX),
        UTinyInt => (0, i128::from(u8::MAX)),
        USmallInt => (0, i128::from(u16::MAX)),
        UInteger => (0, i128::from(u32::MAX)),
        UBigInt => (0, i128::from(u64::MAX)),
        _ => return None,
    })
}

/// The digits before the point that a numeric type holds all of, and its scale.
fn decimal_digits(ty: &LogicalType) -> Option<(i32, i32)> {
    use LogicalTypeId::*;
    let (width, scale) = match ty.id() {
        Decimal => {
            let (width, scale) = ty.decimal_properties()?;
            (i32::from(width), i32::from(scale))
        }
        TinyInt | UTinyInt => (3, 0),
        SmallInt | USmallInt => (5, 0),
        Integer | UInteger => (10, 0),
        BigInt | UBigInt => (20, 0),
        HugeInt | UHugeInt => (39, 0),
        _ => return None,
    };
    Some((width - scale, scale))
}

/// `BoundCastExpression::CastIsInvertible`, for the types comparisons of casts meet.
fn cast_is_invertible(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    let ids = (source.id(), target.id());
    if [ids.0, ids.1].iter().any(|id| matches!(id, Boolean | Float | Double | Variant | Enum)) {
        return false;
    }
    if source.id() == Decimal || target.id() == Decimal {
        let (Some((source_digits, source_scale)), Some((target_digits, target_scale))) =
            (decimal_digits(source), decimal_digits(target))
        else {
            return false;
        };
        let unsigned = matches!(target.id(), UTinyInt | USmallInt | UInteger | UBigInt | UHugeInt);
        if target_scale < source_scale || unsigned {
            return false;
        }
        // an integer type does not hold all numbers of its width
        let target_digits = if target.id() == Decimal { target_digits } else { target_digits - 1 };
        let source_digits = if matches!(source.id(), HugeInt | UHugeInt) { source_digits + 1 } else { source_digits };
        return source_digits <= target_digits;
    }
    match ids {
        (Date, Timestamp | TimestampTz) => true,
        (TimestampSec | TimestampMs | Timestamp, Timestamp | TimestampNs) => true,
        _ => match (integral_range(source), integral_range(target)) {
            (Some((source_min, source_max)), Some((target_min, target_max))) => {
                target_min <= source_min && source_max <= target_max
            }
            _ => false,
        },
    }
}

/// `IsLosslessIntegralToFloatingCast`.
fn lossless_integral_to_floating(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    match target.id() {
        Double => matches!(source.id(), TinyInt | SmallInt | Integer | UTinyInt | USmallInt | UInteger),
        Float => matches!(source.id(), TinyInt | SmallInt | UTinyInt | USmallInt),
        _ => false,
    }
}

/// `ComparisonSimplificationRule` for a cast compared to a constant: `CAST(x AS BIGINT) = 1` is
/// `x = 1`, with the constant cast to the type of `x` instead, when the cast loses nothing and the
/// constant survives the way back. A DATE compared to a TIMESTAMP that is not midnight compares
/// to one of the days around it.
fn cast_comparison_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &mut expr.kind else {
        return false;
    };
    let cast_left = matches!(right.kind, BoundExprKind::Constant(_));
    let (cast, constant) = if cast_left { (&mut *left, &mut *right) } else { (&mut *right, &mut *left) };
    let (BoundExprKind::Cast { child, try_cast: false }, BoundExprKind::Constant(value)) = (&cast.kind, &constant.kind)
    else {
        return false;
    };
    if value.is_null() {
        return false;
    }
    let (source, target) = (child.return_type.clone(), cast.return_type.clone());
    if !cast_is_invertible(&source, &target) && !lossless_integral_to_floating(&source, &target) {
        return false;
    }
    let Some(new_value) = try_evaluate_scalar(&BoundExpr::constant(value.clone()).cast_to(&source, false)) else {
        return false;
    };
    let round_trip = || try_evaluate_scalar(&BoundExpr::constant(new_value.clone()).cast_to(&target, false));
    let invertible = cast_is_invertible(&target, &source)
        || (target.is_integral() && source.is_integral())
        || ((lossless_integral_to_floating(&source, &target) || (target.id() == LogicalTypeId::Decimal
            && source.id() == LogicalTypeId::Decimal))
            && round_trip().as_ref() == Some(value));
    let mut new_value = new_value.clone();
    if !invertible {
        if source.id() != LogicalTypeId::Date || target.id() != LogicalTypeId::Timestamp {
            return false;
        }
        // `DateTimestampComparisonIsInvertible`
        let ValueData::Int64(micros) = value.data() else {
            return false;
        };
        // infinite timestamps have no date and time to split into
        if *micros == i64::MAX || *micros <= -i64::MAX {
            return false;
        }
        const DAY: i64 = 86_400_000_000;
        if micros.rem_euclid(DAY) != 0 {
            let column = (**child).clone();
            let replacement = match op {
                // a TIMESTAMP that is not midnight is no DATE
                ComparisonOp::Equal | ComparisonOp::NotEqual => crate::function::constant_or_null(vec![
                    BoundExpr::constant(Value::boolean(*op == ComparisonOp::NotEqual)),
                    column,
                ]),
                ComparisonOp::DistinctFrom => BoundExpr::constant(Value::boolean(true)),
                ComparisonOp::NotDistinctFrom => BoundExpr::constant(Value::boolean(false)),
                _ => {
                    // `d < T` and `d >= T` need the next day; the column on the right turns that around
                    let next_day = match op {
                        ComparisonOp::LessThan | ComparisonOp::GreaterThanOrEqual => cast_left,
                        _ => !cast_left,
                    };
                    if next_day {
                        let ValueData::Int32(days) = new_value.data() else {
                            return false;
                        };
                        let Some(next) = days.checked_add(1) else {
                            return false;
                        };
                        new_value = match Value::new(source.clone(), ValueData::Int32(next)) {
                            Ok(value) => value,
                            Err(_) => return false,
                        };
                    }
                    let child = (**child).clone();
                    **cast = child;
                    **constant = BoundExpr::constant(new_value);
                    return true;
                }
            };
            *expr = replacement;
            return true;
        }
    }
    let child = (**child).clone();
    **cast = child;
    **constant = BoundExpr::constant(new_value);
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
    if !matches!(children[0].kind, BoundExprKind::Conjunction { .. } | BoundExprKind::Between { .. })
        || children[0].is_volatile()
        || can_throw(&children[0])
    {
        return false;
    }
    // C++ binds BETWEEN as `input >= lower AND input <= upper`
    if let BoundExprKind::Between { input, lower, upper } = &children[0].kind {
        let compare = |op, left: &BoundExpr, right: &BoundExpr| {
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(left.clone()),
                    right: Box::new(right.clone()),
                },
                LogicalType::BOOLEAN,
            )
        };
        let either = vec![
            compare(ComparisonOp::LessThan, input, lower),
            compare(ComparisonOp::GreaterThan, input, upper),
        ];
        *expr = BoundExpr::new(
            BoundExprKind::Conjunction {
                and: false,
                children: either,
            },
            LogicalType::BOOLEAN,
        );
        return true;
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
    "constant_or_null",
    "~~",
    "!~~",
    "~~*",
    "!~~*",
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
    "quarter",
    "sign",
    "left",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "hour",
    "minute",
];

/// `Expression::CanThrow`, leaning towards true.
/// `ExpressionBarrier::Contains`.
pub(crate) fn contains_barrier(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Function { function, .. } if function.name == "__internal_barrier")
        || expr.children().into_iter().any(contains_barrier)
}

pub(crate) fn can_throw(expr: &BoundExpr) -> bool {
    let own = match &expr.kind {
        BoundExprKind::Subquery { .. } | BoundExprKind::ListLambda { .. } | BoundExprKind::Sequence { .. } => true,
        BoundExprKind::Cast { child, try_cast } => !try_cast && cast_can_throw(&child.return_type, &expr.return_type),
        BoundExprKind::Function { function, children, .. } => {
            let floating = matches!(function.name.as_str(), "+" | "-" | "*")
                && children
                    .iter()
                    .all(|c| matches!(c.return_type.id(), LogicalTypeId::Float | LogicalTypeId::Double));
            // a division by a constant that is neither 0 nor -1 can't divide by zero or overflow
            let safe_division = matches!(function.name.as_str(), "/" | "//" | "%")
                && children.len() == 2
                && matches!(&children[1].kind, BoundExprKind::Constant(value)
                    if !value.is_null()
                        && value.try_to_string().ok().and_then(|t| t.parse::<f64>().ok()).is_some_and(|v| v != 0.0 && v != -1.0));
            !floating && !safe_division && !INFALLIBLE_FUNCTIONS.contains(&function.name.as_str())
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
    let left_is_constant = matches!(left.kind, BoundExprKind::Constant(_));
    let (constant, arithmetic) = if left_is_constant { (left, right) } else { (right, left) };
    let negation_left = !left_is_constant;
    if let Some(moved) = move_unary_minus(*op, constant, arithmetic, negation_left) {
        *expr = moved;
        return true;
    }
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

/// `MoveUnaryMinusRule` for floating point and DECIMAL values: `-x = -2.5` is `x = 2.5`. NaN is
/// larger than every number, also negated, so an ordered comparison of floats gets a guard:
/// `-x < -2.5` is `x > 2.5 AND NOT isnan(x)`, and `-x > 2.5` is `x < -2.5 OR isnan(x)`. Integers
/// are left to `move_constants`.
fn move_unary_minus(
    op: ComparisonOp,
    constant: &BoundExpr,
    negation: &BoundExpr,
    negation_left: bool,
) -> Option<BoundExpr> {
    let BoundExprKind::Constant(value) = &constant.kind else {
        return None;
    };
    let BoundExprKind::Function {
        function,
        children,
        is_operator: true,
    } = &negation.kind
    else {
        return None;
    };
    let [input] = children.as_slice() else {
        return None;
    };
    let ty = &constant.return_type;
    let floating = matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double);
    if function.name != "-" || (!floating && ty.id() != LogicalTypeId::Decimal) || negation.return_type != *ty {
        return None;
    }
    if value.is_null() {
        return None;
    }
    let ordered = !matches!(
        op,
        ComparisonOp::Equal | ComparisonOp::NotEqual | ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom
    );
    if floating && ordered {
        let number = match value.data() {
            ValueData::Float(f) => f64::from(*f),
            ValueData::Double(d) => *d,
            _ => return None,
        };
        // `CanDuplicateForNaNGuard`: the guard reads the input again
        if number.is_nan() || input.is_volatile() || can_throw(input) {
            return None;
        }
    }
    let negate = BoundExpr::new(
        BoundExprKind::Function {
            function: function.clone(),
            children: vec![constant.clone()],
            is_operator: true,
        },
        ty.clone(),
    );
    let negated = BoundExpr::constant(try_evaluate_scalar(&negate)?);
    // both sides change sign, so the comparison turns around; each keeps its side
    let (left, right) = if negation_left { (input.clone(), negated) } else { (negated, input.clone()) };
    let comparison = BoundExpr::new(
        BoundExprKind::Comparison {
            op: super::statistics::flip(op),
            left: Box::new(left),
            right: Box::new(right),
        },
        LogicalType::BOOLEAN,
    );
    // the comparison as the negation sees it: `c > -x` is `-x < c`
    let op = if negation_left { op } else { super::statistics::flip(op) };
    if !(floating && ordered) {
        return Some(comparison);
    }
    let isnan = crate::function::binder::bind_scalar_function(
        crate::function::BuiltinFunctions::get().scalar("isnan")?,
        vec![input.clone()],
        false,
        &try_evaluate_scalar,
    )
    .ok()?;
    // `NaNGuardIsDisjunctive`: `-x > c` holds for NaN, `x < -c` does not
    let disjunctive = matches!(op, ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual);
    let guard = if disjunctive {
        isnan
    } else {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children: vec![isnan],
            },
            LogicalType::BOOLEAN,
        )
    };
    Some(BoundExpr::new(
        BoundExprKind::Conjunction {
            and: !disjunctive,
            children: vec![comparison, guard],
        },
        LogicalType::BOOLEAN,
    ))
}

/// A part of a regular expression that a LIKE pattern can say (RE2's `Regexp` ops).
#[derive(PartialEq)]
enum RegexPart {
    Literal(String),
    AnyChar,
    AnyString,
    BeginText,
    EndText,
}

/// The parts of `pattern`, if it consists only of what LIKE can say. With `dot_nl` a `.` matches a
/// newline too, which `_` does; otherwise a `.` cannot be said.
fn regex_parts(pattern: &str, dot_nl: bool) -> Option<Vec<RegexPart>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut parts: Vec<RegexPart> = Vec::new();
    let literal = |parts: &mut Vec<RegexPart>, c: char| match parts.last_mut() {
        Some(RegexPart::Literal(text)) => text.push(c),
        _ => parts.push(RegexPart::Literal(c.to_string())),
    };
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let quantified = matches!(chars.get(at + 1), Some('*' | '+' | '?' | '{'));
        match c {
            '^' if at == 0 => parts.push(RegexPart::BeginText),
            '$' if at + 1 == chars.len() => parts.push(RegexPart::EndText),
            '.' if !dot_nl => return None,
            '.' if chars.get(at + 1) == Some(&'*') => {
                parts.push(RegexPart::AnyString);
                at += 1;
            }
            '.' if quantified => return None,
            '.' => parts.push(RegexPart::AnyChar),
            '\\' => {
                let next = *chars.get(at + 1)?;
                if next.is_alphanumeric() || matches!(chars.get(at + 2), Some('*' | '+' | '?' | '{')) {
                    return None;
                }
                literal(&mut parts, next);
                at += 1;
            }
            '[' | ']' | '(' | ')' | '{' | '}' | '|' | '*' | '+' | '?' | '^' | '$' => return None,
            _ if quantified || c.is_control() => return None,
            _ => literal(&mut parts, c),
        }
        at += 1;
    }
    Some(parts)
}

/// `RegexOptimizationRule`: `regexp_matches(s, 'aa')` is `contains(s, 'aa')`, and a pattern of
/// literals, `.`, `.*` and anchors is a LIKE pattern (which `like_optimizations` may simplify
/// further): with the `s` option `regexp_matches(s, '^a.*b$', 's')` is `s LIKE 'a%b'`.
fn regex_optimization(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    if function.name != "regexp_matches" || !(2..=3).contains(&children.len()) {
        return false;
    }
    if children[..2].iter().any(|c| c.return_type.collation().is_some_and(|c| !c.is_empty())) {
        return false;
    }
    let string = |e: &BoundExpr| match &e.kind {
        BoundExprKind::Constant(value) if !value.is_null() => value.try_to_string().ok(),
        _ => None,
    };
    let Some(pattern) = string(&children[1]) else {
        return false;
    };
    // the options: `s` lets `.` match a newline, `m`, `n` and `p` keep it from that; case
    // insensitivity cannot be said
    let mut dot_nl = false;
    if let Some(options) = children.get(2) {
        let Some(options) = string(options) else {
            return false;
        };
        for option in options.chars() {
            match option {
                's' => dot_nl = true,
                'm' | 'n' | 'p' => dot_nl = false,
                'c' | 'g' => {}
                _ => return false,
            }
        }
    }
    let Some(parts) = regex_parts(&pattern, dot_nl) else {
        return false;
    };
    let input = children[0].clone();
    let bind = |name: &str, needle: String| {
        let set = crate::function::BuiltinFunctions::get().scalar(name)?;
        let arguments = vec![input.clone(), BoundExpr::constant(Value::varchar(needle))];
        crate::function::binder::bind_scalar_function(set, arguments, false, &try_evaluate_scalar).ok()
    };
    // a single literal is a substring
    if let [RegexPart::Literal(text)] = parts.as_slice() {
        let needle = BoundExpr::constant(Value::varchar(text.clone()));
        *expr = crate::function::string_needle_call("contains", input, needle);
        return true;
    }
    if parts.is_empty() {
        return false;
    }
    // `LikeMatchFromRegex`
    let mut like = String::new();
    let last = parts.len() - 1;
    for (at, part) in parts.iter().enumerate() {
        match part {
            RegexPart::AnyChar => {
                if at == 0 {
                    like.push('%');
                }
                like.push('_');
                if at == last {
                    like.push('%');
                }
            }
            RegexPart::AnyString => like.push('%'),
            RegexPart::Literal(text) => {
                // LIKE has no way to escape these here
                if text.contains(['%', '_']) {
                    return false;
                }
                if at == 0 {
                    like.push('%');
                }
                like.push_str(text);
                if at == last {
                    like.push('%');
                }
            }
            RegexPart::BeginText | RegexPart::EndText => {}
        }
    }
    let Some(matcher) = bind("~~", like) else {
        return false;
    };
    *expr = matcher;
    true
}

/// `instr(s, 'ab') = 1` is `prefix(s, 'ab')`, for a needle that is not NULL.
fn instr_prefix(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison {
        op: ComparisonOp::Equal,
        left,
        right,
    } = &expr.kind
    else {
        return false;
    };
    let (call, one) = if matches!(right.kind, BoundExprKind::Constant(_)) { (left, right) } else { (right, left) };
    if integer_constant(one) != Some(1) {
        return false;
    }
    let BoundExprKind::Function { function, children, .. } = &call.kind else {
        return false;
    };
    let [haystack, needle] = children.as_slice() else {
        return false;
    };
    let is_string = |e: &BoundExpr| matches!(&e.kind, BoundExprKind::Constant(v) if !v.is_null() && v.logical_type() == &LogicalType::VARCHAR);
    if function.name != "instr" || !is_string(needle) {
        return false;
    }
    *expr = crate::function::string_needle_call("prefix", haystack.clone(), needle.clone());
    true
}

/// `StringPrefixRule`: `left(s, 3) = 'abc'` is `prefix(s, 'abc')`, `substring(s, 1) = c` is `s = c`,
/// and a constant of another length than the prefix makes it an equality, or false.
fn string_prefix(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison {
        op: ComparisonOp::Equal,
        left,
        right,
    } = &expr.kind
    else {
        return false;
    };
    let (call, constant) = if matches!(right.kind, BoundExprKind::Constant(_)) { (left, right) } else { (right, left) };
    let BoundExprKind::Constant(constant_value) = &constant.kind else {
        return false;
    };
    let BoundExprKind::Function { function, children, .. } = &call.kind else {
        return false;
    };
    if call.return_type.id() != LogicalTypeId::Varchar || constant.return_type.id() != LogicalTypeId::Varchar {
        return false;
    }
    let number = |e: &BoundExpr| match &e.kind {
        BoundExprKind::Constant(v) => Some(if v.is_null() { None } else { v.as_i128() }),
        _ => None,
    };
    let null = BoundExpr::constant(Value::null(LogicalType::BOOLEAN));
    if constant_value.is_null() {
        *expr = null;
        return true;
    }
    let characters = match (function.name.as_str(), children.as_slice()) {
        ("left", [_, count]) => match number(count) {
            Some(None) => {
                *expr = null;
                return true;
            }
            Some(Some(n)) if (0..=i128::from(u32::MAX)).contains(&n) => n,
            _ => return false,
        },
        ("substring" | "substr", [input, start, rest @ ..]) => {
            match number(start) {
                Some(None) => {
                    *expr = null;
                    return true;
                }
                Some(Some(1)) => {}
                _ => return false,
            }
            match rest {
                [] => {
                    // the whole string
                    let (input, constant) = (input.clone(), (**constant).clone());
                    expr.kind = BoundExprKind::Comparison {
                        op: ComparisonOp::Equal,
                        left: Box::new(input),
                        right: Box::new(constant),
                    };
                    return true;
                }
                [length] => match number(length) {
                    Some(None) => {
                        *expr = null;
                        return true;
                    }
                    Some(Some(n)) if (0..=i128::from(u32::MAX)).contains(&n) => n,
                    _ => return false,
                },
                _ => return false,
            }
        }
        // `s[x:y]` is `array_slice(s, x, y)`; `s[0:]` is `s[1:]`, and a bound left out is an empty list
        ("array_slice" | "list_slice", [input, begin, end]) => {
            let is_list = |e: &BoundExpr| {
                matches!(&e.kind, BoundExprKind::Constant(v) if !v.is_null())
                    && e.return_type.id() == LogicalTypeId::List
            };
            if !is_list(begin) {
                match number(begin) {
                    Some(None) => {
                        *expr = null;
                        return true;
                    }
                    Some(Some(0 | 1)) => {}
                    _ => return false,
                }
            }
            if is_list(end) {
                let (input, constant) = (input.clone(), (**constant).clone());
                expr.kind = BoundExprKind::Comparison {
                    op: ComparisonOp::Equal,
                    left: Box::new(input),
                    right: Box::new(constant),
                };
                return true;
            }
            match number(end) {
                Some(None) => {
                    *expr = null;
                    return true;
                }
                Some(Some(n)) if n >= 0 => n,
                _ => return false,
            }
        }
        _ => return false,
    };
    let Ok(text) = constant_value.try_to_string() else {
        return false;
    };
    let input = children[0].clone();
    let length = text.chars().count() as i128;
    *expr = if length > characters {
        // a constant longer than the prefix is never it
        crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(false)), input])
    } else if length < characters {
        // a prefix shorter than asked for is the whole string
        BoundExpr::new(
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal,
                left: Box::new(input),
                right: Box::new(BoundExpr::constant(constant_value.clone())),
            },
            LogicalType::BOOLEAN,
        )
    } else {
        crate::function::string_needle_call("prefix", input, BoundExpr::constant(constant_value.clone()))
    };
    true
}

/// `s GLOB 'abc*'` is `prefix(s, 'abc')`, and `s GLOB 'abc'` is `s = 'abc'`.
fn glob_prefix(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    let [input, pattern] = children.as_slice() else {
        return false;
    };
    let BoundExprKind::Constant(value) = &pattern.kind else {
        return false;
    };
    if !matches!(function.name.as_str(), "glob" | "~~~") || value.is_null() || input.return_type.collation().is_some() {
        return false;
    }
    let Ok(pattern) = value.try_to_string() else {
        return false;
    };
    let special = |c: char| matches!(c, '*' | '?' | '[' | ']' | '\\');
    let (literal, prefix) = match pattern.strip_suffix('*') {
        Some(head) => (head, true),
        None => (pattern.as_str(), false),
    };
    if literal.is_empty() || literal.chars().any(special) {
        return false;
    }
    let needle = BoundExpr::constant(Value::varchar(literal));
    *expr = if prefix {
        crate::function::string_needle_call("prefix", input.clone(), needle)
    } else {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal,
                left: Box::new(input.clone()),
                right: Box::new(needle),
            },
            LogicalType::BOOLEAN,
        )
    };
    true
}

/// `x IS NOT DISTINCT FROM NULL` is `x IS NULL`, and `x IS DISTINCT FROM NULL` is `x IS NOT NULL`
/// (`CreateNullCheckExpression`), for a column.
fn null_distinct_comparison(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return false;
    };
    let null_check = match op {
        ComparisonOp::NotDistinctFrom => BoundOperator::IsNull,
        ComparisonOp::DistinctFrom => BoundOperator::IsNotNull,
        _ => return false,
    };
    let is_null = |e: &BoundExpr| matches!(&e.kind, BoundExprKind::Constant(v) if v.is_null());
    let column = if is_null(right) {
        left
    } else if is_null(left) {
        right
    } else {
        return false;
    };
    if !matches!(column.kind, BoundExprKind::ColumnRef { .. }) {
        return false;
    }
    *expr = BoundExpr::new(
        BoundExprKind::Operator {
            op: null_check,
            children: vec![(**column).clone()],
        },
        LogicalType::BOOLEAN,
    );
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

/// `LeastGreatestSimplificationRule`: `least(x, x)` and `greatest(x, x)` are `x`.
fn least_greatest_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    if !matches!(function.name.as_str(), "least" | "greatest") || children.len() != 2 {
        return false;
    }
    if children[0].is_volatile() || !expressions_equal(&children[0], &children[1]) {
        return false;
    }
    let mut child = children[0].clone();
    // `Expression::PreserveReturnType`
    if child.return_type != expr.return_type {
        child = BoundExpr::new(
            BoundExprKind::Cast {
                child: Box::new(child),
                try_cast: false,
            },
            expr.return_type.clone(),
        );
    }
    *expr = child;
    true
}

/// `NotConstantOrNullSimplificationRule`: `NOT constant_or_null(v, e...)` is `constant_or_null(NOT v, e...)`.
fn not_constant_or_null_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::Not,
        children,
    } = &expr.kind
    else {
        return false;
    };
    let [call] = children.as_slice() else {
        return false;
    };
    let BoundExprKind::Function { function, children, .. } = &call.kind else {
        return false;
    };
    if function.name != "constant_or_null" || call.return_type != LogicalType::BOOLEAN || children.len() < 2 {
        return false;
    }
    let BoundExprKind::Constant(value) = &children[0].kind else {
        return false;
    };
    let duckrs_common::value::ValueData::Bool(value) = *value.data() else {
        return false;
    };
    let mut arguments = vec![BoundExpr::constant(Value::boolean(!value))];
    arguments.extend(children[1..].iter().cloned());
    *expr = crate::function::constant_or_null(arguments);
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
