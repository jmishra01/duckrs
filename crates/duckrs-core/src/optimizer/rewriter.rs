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
use crate::function::{date_part_call_of, ScalarFunction};
use crate::function::aggregates::AggregateFunction;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, BoundWindow, ColumnBinding, WindowFunction};
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
    rewrite_with(expr, in_filter, true);
}

/// [`rewrite_expression`] for a filter after the pushdown, without `ConstantFoldingRule` and
/// `MonotonePreimageRule`: C++ applies the rules once before it, so the auxiliary `e(b) op e(c)`
/// filters of windows stay as the pushdown makes them.
pub(crate) fn rewrite_pushed_filter(expr: &mut BoundExpr) {
    rewrite_with(expr, true, false);
}

fn rewrite_with(expr: &mut BoundExpr, in_filter: bool, preimage: bool) {
    for _ in 0..MAX_ROUNDS {
        for child in expr.children_mut() {
            rewrite_with(child, false, preimage);
        }
        let alias = expr.alias.clone();
        if !apply_rules(expr, in_filter, preimage) {
            return;
        }
        if !alias.is_empty() {
            expr.alias = alias;
        }
    }
}

fn apply_rules(expr: &mut BoundExpr, in_filter: bool, preimage: bool) -> bool {
    (preimage && constant_folding(expr))
        || equal_or_null_simplification(expr, in_filter)
        || date_part_simplification(expr)
        || date_trunc_simplification(expr)
        || distributivity(expr)
        || arithmetic_simplification(expr)
        || constant_order_normalization(expr)
        || regex_optimization(expr)
        || like_optimizations(expr)
        || instr_prefix(expr)
        || string_prefix(expr)
        || glob_prefix(expr)
        || identity_string_functions(expr)
        || (in_filter && null_distinct_comparison(expr))
        || empty_needle_removal(expr)
        || least_greatest_simplification(expr)
        || not_constant_or_null_simplification(expr)
        || move_constants(expr)
        || (preimage && super::monotone_preimage::monotone_preimage(expr, in_filter))
        || case_simplification(expr)
        || conjunction_simplification(expr, in_filter)
        || super::or_range_merge::or_range_merge(expr)
        || comparison_simplification(expr)
        || cast_comparison_simplification(expr)
        || not_comparison_simplification(expr)
        || not_conjunction_simplification(expr)
        || join_dependent_filter(expr, in_filter)
        || (in_filter && predicate_factoring(expr))
        || list_comprehension_rewrite(expr)
        || contains_to_in_clause(expr)
        || enum_compare_simplification(expr)
        || enum_in_simplification(expr)
        || enum_full_domain(expr)
}

/// The ENUM of `CAST(e AS VARCHAR)`, or of an ENUM itself (`GetEnumInProbe`).
fn enum_probe(expr: &BoundExpr) -> Option<&BoundExpr> {
    if expr.return_type.id() == LogicalTypeId::Enum {
        return Some(expr);
    }
    match &expr.kind {
        BoundExprKind::Cast { child, try_cast: false }
            if expr.return_type.id() == LogicalTypeId::Varchar && child.return_type.id() == LogicalTypeId::Enum =>
        {
            Some(child)
        }
        _ => None,
    }
}

/// The ENUM of `ty` that is `value`, by its text, if it is one.
fn enum_constant(ty: &LogicalType, value: &Value) -> Option<Value> {
    let text = value.try_to_string().ok()?;
    let position = ty.enum_values().iter().position(|v| *v == text)?;
    Some(Value::enum_value(ty, position))
}

/// `EnumCompareSimplificationRule`: `CAST(e AS VARCHAR) op 'x'` compares the ENUM with the ENUM `'x'`; a
/// string outside of its values is equal to none of them.
fn enum_compare_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return false;
    };
    let op = *op;
    if !matches!(
        op,
        ComparisonOp::Equal | ComparisonOp::NotEqual | ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom
    ) {
        return false;
    }
    let cast_left = matches!(left.kind, BoundExprKind::Cast { .. });
    let (cast, constant) = if cast_left { (left, right) } else { (right, left) };
    let BoundExprKind::Cast { child, try_cast: false } = &cast.kind else {
        return false;
    };
    if cast.return_type.id() != LogicalTypeId::Varchar || child.return_type.id() != LogicalTypeId::Enum {
        return false;
    }
    let BoundExprKind::Constant(value) = &constant.kind else {
        return false;
    };
    if constant.return_type.id() != LogicalTypeId::Varchar {
        return false;
    }
    let probe = (**child).clone();
    let ty = probe.return_type.clone();
    let enum_value = if value.is_null() {
        Some(Value::null(ty.clone()))
    } else {
        enum_constant(&ty, value)
    };
    *expr = match enum_value {
        Some(enum_value) => {
            let enum_value = BoundExpr::constant(enum_value);
            let (left, right) = if cast_left { (probe, enum_value) } else { (enum_value, probe) };
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                LogicalType::BOOLEAN,
            )
        }
        None => match op {
            ComparisonOp::Equal => crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(false)), probe]),
            ComparisonOp::NotEqual => crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(true)), probe]),
            ComparisonOp::NotDistinctFrom => BoundExpr::constant(Value::boolean(false)),
            _ => BoundExpr::constant(Value::boolean(true)),
        },
    };
    true
}

/// `InEnumSimplificationRule`: `e IN (...)` of an ENUM (or `CAST(e AS VARCHAR)`) and strings or ENUMs is of the
/// ENUM's values among them; all of them make it a constant, none too.
fn enum_in_simplification(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Operator { op, children } = &expr.kind else {
        return false;
    };
    let in_clause = match op {
        BoundOperator::In => true,
        BoundOperator::NotIn => false,
        _ => return false,
    };
    let Some(probe) = enum_probe(&children[0]) else {
        return false;
    };
    // nothing to do for an ENUM already compared with its own values
    let cast_probe = !std::ptr::eq(probe, &children[0]);
    let ty = probe.return_type.clone();
    let mut list = vec![probe.clone()];
    let mut matched = std::collections::BTreeSet::new();
    let mut changed = cast_probe;
    for child in &children[1..] {
        let BoundExprKind::Constant(value) = &child.kind else {
            return false;
        };
        if value.is_null() {
            list.push(BoundExpr::constant(Value::null(ty.clone())));
            continue;
        }
        if !matches!(value.logical_type().id(), LogicalTypeId::Varchar | LogicalTypeId::Enum) {
            return false;
        }
        match enum_constant(&ty, value) {
            Some(enum_value) => {
                if value.logical_type() != &ty {
                    changed = true;
                }
                if let Some(position) = ty.enum_values().iter().position(|v| Some(v.as_str()) == value.try_to_string().ok().as_deref()) {
                    matched.insert(position);
                }
                list.push(BoundExpr::constant(enum_value));
            }
            // not one of its values: compares with none
            None => changed = true,
        }
    }
    let probe = list[0].clone();
    if matched.len() == ty.enum_values().len() {
        *expr = crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(in_clause)), probe]);
        return true;
    }
    if list.len() == 1 {
        *expr = crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(!in_clause)), probe]);
        return true;
    }
    if !changed {
        return false;
    }
    let BoundExprKind::Operator { children, .. } = &mut expr.kind else {
        unreachable!()
    };
    *children = list;
    true
}

/// `InEnumSimplificationRule` of an IN list the binder spelled out: `e = a OR e = b ...` (`e != a AND ...` of
/// NOT IN) over all of an ENUM's values is a constant, NULL where `e` is.
fn enum_full_domain(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Conjunction { and, children } = &expr.kind else {
        return false;
    };
    let and = *and;
    let wanted = if and { ComparisonOp::NotEqual } else { ComparisonOp::Equal };
    let mut probe: Option<&BoundExpr> = None;
    let mut matched = std::collections::BTreeSet::new();
    for child in children {
        // `constant_or_null(c, e)` of a value it has none of: of no consequence here
        if let BoundExprKind::Function { function, children: args, .. } = &child.kind
            && function.name == "constant_or_null"
            && args.len() == 2
            && matches!(&args[0].kind, BoundExprKind::Constant(v) if *v == Value::boolean(and))
        {
            if probe.is_some_and(|p| !expressions_equal(p, &args[1])) {
                return false;
            }
            probe = Some(&args[1]);
            continue;
        }
        let BoundExprKind::Comparison { op, left, right } = &child.kind else {
            return false;
        };
        if *op != wanted {
            return false;
        }
        let (side, constant) = match (&left.kind, &right.kind) {
            (_, BoundExprKind::Constant(value)) => (&**left, value),
            (BoundExprKind::Constant(value), _) => (&**right, value),
            _ => return false,
        };
        if side.return_type.id() != LogicalTypeId::Enum || probe.is_some_and(|p| !expressions_equal(p, side)) {
            return false;
        }
        probe = Some(side);
        if constant.is_null() {
            continue;
        }
        let Some(position) = constant.try_to_string().ok().and_then(|t| side.return_type.enum_values().iter().position(|v| *v == t))
        else {
            return false;
        };
        matched.insert(position);
    }
    let Some(probe) = probe else {
        return false;
    };
    if probe.return_type.id() != LogicalTypeId::Enum || matched.len() != probe.return_type.enum_values().len() {
        return false;
    }
    let probe = probe.clone();
    *expr = crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(!and)), probe]);
    true
}

/// `MatchesStructFieldProjection`: `struct_extract` of the lambda's element, the field `name`; with
/// `boolean_cast`, under one cast to BOOLEAN too.
fn extracts_field(expr: &BoundExpr, name: &str, boolean_cast: bool) -> bool {
    let mut base = expr;
    if boolean_cast
        && let BoundExprKind::Cast { child, try_cast: false } = &base.kind
        && base.return_type == LogicalType::BOOLEAN
    {
        base = child;
    }
    let BoundExprKind::Function { function, children, .. } = &base.kind else {
        return false;
    };
    let [element, field] = children.as_slice() else {
        return false;
    };
    if function.name != "struct_extract" || !matches!(element.kind, BoundExprKind::Reference(0)) {
        return false;
    }
    let BoundExprKind::Constant(field) = &field.kind else {
        return false;
    };
    let fields = element.return_type.struct_children();
    match field.logical_type().id() {
        LogicalTypeId::Varchar => field.try_to_string().is_ok_and(|f| f == name),
        _ => field
            .as_i128()
            .and_then(|at| fields.get(usize::try_from(at).ok()?))
            .is_some_and(|(field, _)| field == name),
    }
}

/// `ListComprehensionRewriteRule`: `[r FOR x IN l IF f]` binds to `list_apply(list_filter(list_apply(l,
/// lambda x: struct_pack(filter := f, result := r)), lambda s: s.filter), lambda s: s.result)`; where `r`
/// does not read the index it is `list_apply(list_filter(l, lambda x: f), lambda x: r)`, which builds
/// no structs.
fn list_comprehension_rewrite(expr: &mut BoundExpr) -> bool {
    use crate::planner::expression::LambdaKind;
    let BoundExprKind::ListLambda {
        kind: LambdaKind::Transform,
        list: filtered,
        captures: root_captures,
        body: root_body,
        ..
    } = &expr.kind
    else {
        return false;
    };
    let BoundExprKind::ListLambda {
        kind: LambdaKind::Filter,
        list: inner,
        captures: filter_captures,
        body: filter_body,
        ..
    } = &filtered.kind
    else {
        return false;
    };
    if !root_captures.is_empty()
        || !filter_captures.is_empty()
        || !extracts_field(filter_body, "filter", true)
        || !extracts_field(root_body, "result", false)
    {
        return false;
    }
    let BoundExprKind::ListLambda {
        kind: LambdaKind::Transform,
        list,
        captures,
        body: inner_body,
        ..
    } = &inner.kind
    else {
        return false;
    };
    let BoundExprKind::Function { function, children, .. } = &inner_body.kind else {
        return false;
    };
    if function.name != "struct_pack" {
        return false;
    }
    let fields = inner_body.return_type.struct_children();
    let field = |name: &str| fields.iter().position(|(n, _)| n == name).and_then(|at| children.get(at));
    let (Some(filter), Some(result)) = (field("filter"), field("result")) else {
        return false;
    };
    // the index, which the rewritten apply does not have
    let mut reads_index = false;
    result.clone().walk_mut(&mut |e| reads_index |= matches!(e.kind, BoundExprKind::Reference(1)));
    if reads_index || result.is_volatile() {
        return false;
    }
    let filter = filter.clone().cast_to(&LogicalType::BOOLEAN, false);
    let result = result.clone();
    let new_filter = BoundExpr::new(
        BoundExprKind::ListLambda {
            kind: LambdaKind::Filter,
            list: list.clone(),
            initial: None,
            captures: captures.clone(),
            body: Box::new(filter),
        },
        list.return_type.clone(),
    );
    let return_type = LogicalType::list(result.return_type.clone());
    *expr = BoundExpr::new(
        BoundExprKind::ListLambda {
            kind: LambdaKind::Transform,
            list: Box::new(new_filter),
            initial: None,
            captures: captures.clone(),
            body: Box::new(result),
        },
        return_type,
    )
    .with_alias(expr.alias.clone());
    true
}

/// `GetSingleColumnBinding`: the one column a non-volatile expression reads.
fn single_column_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    if expr.is_volatile() {
        return None;
    }
    let mut found: Option<ColumnBinding> = None;
    let mut multiple = false;
    expr.clone().walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = &e.kind {
            match found {
                None => found = Some(*binding),
                Some(b) if b != *binding => multiple = true,
                _ => {}
            }
        }
    });
    found.filter(|_| !multiple)
}

fn extract_disjuncts<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        BoundExprKind::Conjunction { and: false, children } => {
            for child in children {
                extract_disjuncts(child, out);
            }
        }
        _ => out.push(expr),
    }
}

/// `ExtractDisjunctedPredicates` into a map: the conjuncts of `expr` that read one column, by it.
fn extract_column_predicates<'a>(expr: &'a BoundExpr, map: &mut Vec<(ColumnBinding, Vec<&'a BoundExpr>)>) {
    if let BoundExprKind::Conjunction { and: true, children } = &expr.kind {
        for child in children {
            extract_column_predicates(child, map);
        }
        return;
    }
    if let Some(binding) = single_column_binding(expr) {
        match map.iter_mut().find(|(b, _)| *b == binding) {
            Some((_, predicates)) => predicates.push(expr),
            None => map.push((binding, vec![expr])),
        }
    }
}

/// `PredicateFactoringRule`: an OR at the root of a filter whose every alternative restricts a
/// column gets, ANDed to it, the OR of those restrictions of each such column, which can go into
/// a scan: `(a = 1 AND b = 2) OR (a = 3 AND b = 4)` also gives `(a = 1 OR a = 3) AND (b = 2 OR b = 4)`.
fn predicate_factoring(expr: &mut BoundExpr) -> bool {
    if !matches!(expr.kind, BoundExprKind::Conjunction { and: false, .. }) || single_column_binding(expr).is_some() {
        return false;
    }
    let mut disjuncts = Vec::new();
    extract_disjuncts(expr, &mut disjuncts);
    let mut remaining = Vec::new();
    extract_column_predicates(disjuncts[0], &mut remaining);
    for child in &disjuncts[1..] {
        let mut child_map = Vec::new();
        extract_column_predicates(child, &mut child_map);
        remaining.retain_mut(|(binding, predicates)| {
            let Some((_, new)) = child_map.iter().find(|(b, _)| b == binding) else {
                return false;
            };
            for predicate in new {
                if !predicates.iter().any(|existing| expressions_equal(existing, predicate)) {
                    predicates.push(predicate);
                }
            }
            true
        });
    }
    if remaining.is_empty() {
        return false;
    }
    let mut derived: Option<BoundExpr> = None;
    for (_, predicates) in remaining {
        let mut column_filter: Option<BoundExpr> = None;
        for predicate in predicates {
            column_filter = Some(match column_filter {
                None => predicate.clone(),
                Some(filter) => BoundExpr::new(
                    BoundExprKind::Conjunction {
                        and: false,
                        children: vec![filter, predicate.clone()],
                    },
                    LogicalType::BOOLEAN,
                ),
            });
        }
        let column_filter = column_filter.expect("a column has predicates");
        derived = Some(match derived {
            None => column_filter,
            Some(derived) => BoundExpr::new(
                BoundExprKind::Conjunction {
                    and: true,
                    children: vec![column_filter, derived],
                },
                LogicalType::BOOLEAN,
            ),
        });
    }
    let original = expr.clone();
    *expr = BoundExpr::new(
        BoundExprKind::Conjunction {
            and: true,
            children: vec![original, derived.expect("a column qualified")],
        },
        LogicalType::BOOLEAN,
    );
    true
}

/// `ContainsToInClauseRule`: `list_contains` of a constant list is an IN of its elements that are
/// not NULL; of a NULL list it is NULL, of an empty one false.
fn contains_to_in_clause(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    if !matches!(
        function.name.as_str(),
        "contains" | "list_contains" | "list_has" | "array_contains" | "array_has"
    ) || children.len() != 2
        || !children[0].is_foldable()
    {
        return false;
    }
    let Some(list) = try_evaluate_scalar(&children[0]) else {
        return false;
    };
    let probe = children[1].clone();
    if list.is_null() {
        *expr = BoundExpr::constant(Value::null(LogicalType::BOOLEAN));
        return true;
    }
    if list.logical_type().id() != LogicalTypeId::List {
        return false;
    }
    let ValueData::List(items) = list.data() else {
        return false;
    };
    let Some(child_type) = list.logical_type().child_type().cloned() else {
        return false;
    };
    let elements: Vec<Value> = items.iter().filter(|item| !item.is_null()).cloned().collect();
    if elements.is_empty() {
        if items.is_empty() {
            // an empty list contains nothing, not even NULL, but the probe still runs if it can fail
            if probe.is_volatile() || can_throw(&probe) {
                return false;
            }
            *expr = BoundExpr::constant(Value::boolean(false));
            return true;
        }
        *expr = crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(false)), probe]);
        return true;
    }
    // a constant probe is constant folding's
    if probe.is_foldable() || probe.return_type != child_type {
        return false;
    }
    let mut operands = vec![probe];
    for element in elements {
        let element = BoundExpr::constant(element);
        operands.push(if element.return_type == child_type {
            element
        } else {
            element.cast_to(&child_type, false)
        });
    }
    // as the binder has IN: an operator for lists `InClauseRewriter` takes, comparisons otherwise
    *expr = if operands.len() >= super::in_clause_rewriter::IN_CLAUSE_REWRITE_THRESHOLD {
        BoundExpr::new(
            BoundExprKind::Operator {
                op: BoundOperator::In,
                children: operands,
            },
            LogicalType::BOOLEAN,
        )
    } else {
        crate::planner::expression::in_as_comparisons(false, &operands)
    };
    true
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

/// A call of `name`, bound as the binder binds it.
fn bind_call(name: &str, children: Vec<BoundExpr>) -> Option<BoundExpr> {
    if crate::function::dates::is_date_function(name) {
        return crate::function::dates::bind_date_function(name, children).ok();
    }
    if crate::function::generic::is_generic_function(name) {
        return crate::function::generic::bind_generic_function(name, children).ok();
    }
    let set = crate::function::BuiltinFunctions::get().scalar(name)?;
    let operator = matches!(name, "+" | "-" | "*" | "/");
    crate::function::binder::bind_scalar_function(set, children, operator, &try_evaluate_scalar).ok()
}

/// The expression as `ty`, folded if it is constant (`AddDefaultCastToType` and `TryEvaluateScalar`).
fn cast_and_fold(expr: BoundExpr, ty: &LogicalType) -> BoundExpr {
    let expr = if expr.return_type.id() == ty.id() { expr } else { expr.cast_to(ty, true) };
    if expr.is_foldable()
        && let Some(value) = try_evaluate_scalar(&expr)
    {
        return BoundExpr::constant(value);
    }
    expr
}

/// `DateTruncSimplificationRule::CreateTrunc`.
fn create_trunc(part: &BoundExpr, constant: &BoundExpr, ty: &LogicalType) -> Option<BoundExpr> {
    Some(cast_and_fold(bind_call("date_trunc", vec![part.clone(), constant.clone()])?, ty))
}

/// `DateTruncSimplificationRule::CreateTruncAdd`: `date_trunc(part, constant + INTERVAL 1 part)`.
fn create_trunc_add(part: &BoundExpr, constant: &BoundExpr, ty: &LogicalType) -> Option<BoundExpr> {
    use duckrs_common::types::DatePart as P;
    let name = try_evaluate_scalar(part)?.try_to_string().ok()?;
    // `DatePartToFunc`: the parts that are intervals
    let interval = match P::parse(&name)? {
        P::Year => "to_years",
        P::Month => "to_months",
        P::Day => "to_days",
        P::Decade => "to_decades",
        P::Century => "to_centuries",
        P::Millennium => "to_millennia",
        P::Microseconds => "to_microseconds",
        P::Milliseconds => "to_milliseconds",
        P::Second => "to_seconds",
        P::Minute => "to_minutes",
        P::Hour => "to_hours",
        P::Week => "to_weeks",
        P::Quarter => "to_quarters",
        _ => return None,
    };
    let interval = bind_call(interval, vec![BoundExpr::constant(Value::integer(1))])?;
    let add = bind_call("+", vec![constant.clone(), interval])?;
    Some(cast_and_fold(bind_call("date_trunc", vec![part.clone(), add])?, ty))
}

/// `DateTruncSimplificationRule`: a comparison of `date_trunc(part, column)` with a constant as
/// one of the column, with the constant truncated: `date_trunc('day', d) < c` is `d < c` when `c`
/// is a day, `d < date_trunc('day', c + INTERVAL 1 DAY)` otherwise.
fn date_trunc_simplification(expr: &mut BoundExpr) -> bool {
    use duckrs_common::types::DatePart as P;
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return false;
    };
    let is_trunc = |e: &BoundExpr| {
        matches!(&e.kind, BoundExprKind::Function { function, children, .. }
            if matches!(function.name.as_str(), "date_trunc" | "datetrunc")
                && children.len() == 2
                && matches!(children[0].kind, BoundExprKind::Constant(_)))
    };
    let column_is_left = is_trunc(left) && matches!(right.kind, BoundExprKind::Constant(_));
    if !(column_is_left || (is_trunc(right) && matches!(left.kind, BoundExprKind::Constant(_)))) {
        return false;
    }
    let (trunc, constant) = if column_is_left { (&**left, &**right) } else { (&**right, &**left) };
    let BoundExprKind::Function { children, .. } = &trunc.kind else {
        return false;
    };
    let (part, column) = (children[0].clone(), children[1].clone());
    if !matches!(column.kind, BoundExprKind::ColumnRef { .. }) {
        return false;
    }
    let BoundExprKind::Constant(value) = &constant.kind else {
        return false;
    };
    let value = value.clone();
    let constant = constant.clone();
    // as if the constant were on the right
    let comparison = if column_is_left { *op } else { super::statistics::flip(*op) };
    let column_type = column.return_type.clone();
    let rebuild = |op: ComparisonOp, constant: BoundExpr| {
        let (left, right) = if column_is_left { (column.clone(), constant) } else { (constant, column.clone()) };
        BoundExpr::new(
            BoundExprKind::Comparison {
                op: if column_is_left { op } else { super::statistics::flip(op) },
                left: Box::new(left),
                right: Box::new(right),
            },
            LogicalType::BOOLEAN,
        )
    };
    let comparison_of = |op: ComparisonOp, constant: BoundExpr| {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(column.clone()),
                right: Box::new(constant),
            },
            LogicalType::BOOLEAN,
        )
    };
    let operator = |op: BoundOperator| {
        BoundExpr::new(
            BoundExprKind::Operator {
                op,
                children: vec![column.clone()],
            },
            LogicalType::BOOLEAN,
        )
    };
    let conjunction = |and: bool, children: Vec<BoundExpr>| {
        BoundExpr::new(BoundExprKind::Conjunction { and, children }, LogicalType::BOOLEAN)
    };
    // date_trunc keeps temporal infinities: only the call goes
    let infinite = !value.is_null()
        && matches!(
            value.logical_type().id(),
            LogicalTypeId::Timestamp
                | LogicalTypeId::TimestampSec
                | LogicalTypeId::TimestampMs
                | LogicalTypeId::TimestampNs
                | LogicalTypeId::TimestampTz
        )
        && value.try_to_string().is_ok_and(|t| t == "infinity" || t == "-infinity");
    if infinite {
        *expr = rebuild(comparison, cast_and_fold(constant, &column_type));
        return true;
    }
    // decades, centuries and millennia round the years before the common era towards zero
    let part_name = try_evaluate_scalar(&part).and_then(|v| v.try_to_string().ok()).unwrap_or_default();
    let towards_zero = !matches!(P::parse(&part_name), Some(p) if !matches!(p, P::Decade | P::Century | P::Millennium));
    if towards_zero {
        let after_first_year = value.is_null()
            || try_evaluate_scalar(&constant.clone().cast_to(&LogicalType::DATE, true))
                .filter(|d| !d.is_null())
                .and_then(|d| d.try_to_string().ok())
                .and_then(|text| text.split('-').next().and_then(|y| y.parse::<i64>().ok()))
                .is_some_and(|year| year > 1 && !text_is_bc(&constant));
        if !after_first_year {
            return false;
        }
    }
    // `DateIsTruncated`
    let truncated = value.is_null()
        || create_trunc(&part, &constant, &constant.return_type)
            .and_then(|t| try_evaluate_scalar(&t))
            .is_some_and(|t| t == value);
    let result = match comparison {
        ComparisonOp::Equal | ComparisonOp::NotDistinctFrom => {
            if comparison == ComparisonOp::NotDistinctFrom && value.is_null() {
                operator(BoundOperator::IsNull)
            } else if !truncated {
                if comparison == ComparisonOp::NotDistinctFrom {
                    BoundExpr::constant(Value::boolean(false))
                } else {
                    crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(false)), column.clone()])
                }
            } else {
                let (Some(trunc), Some(next)) = (
                    create_trunc(&part, &constant, &column_type),
                    create_trunc_add(&part, &constant, &column_type),
                ) else {
                    return false;
                };
                let range = conjunction(
                    true,
                    vec![
                        comparison_of(ComparisonOp::GreaterThanOrEqual, trunc),
                        comparison_of(ComparisonOp::LessThan, next),
                    ],
                );
                if comparison == ComparisonOp::NotDistinctFrom {
                    conjunction(true, vec![range, operator(BoundOperator::IsNotNull)])
                } else {
                    range
                }
            }
        }
        ComparisonOp::NotEqual | ComparisonOp::DistinctFrom => {
            if comparison == ComparisonOp::DistinctFrom && value.is_null() {
                operator(BoundOperator::IsNotNull)
            } else if !truncated {
                if comparison == ComparisonOp::DistinctFrom {
                    BoundExpr::constant(Value::boolean(true))
                } else {
                    crate::function::constant_or_null(vec![BoundExpr::constant(Value::boolean(true)), column.clone()])
                }
            } else {
                let (Some(trunc), Some(next)) = (
                    create_trunc(&part, &constant, &column_type),
                    create_trunc_add(&part, &constant, &column_type),
                ) else {
                    return false;
                };
                let outside = conjunction(
                    false,
                    vec![
                        comparison_of(ComparisonOp::GreaterThanOrEqual, next),
                        comparison_of(ComparisonOp::LessThan, trunc),
                    ],
                );
                if comparison == ComparisonOp::DistinctFrom {
                    conjunction(false, vec![outside, operator(BoundOperator::IsNull)])
                } else {
                    outside
                }
            }
        }
        ComparisonOp::LessThan | ComparisonOp::GreaterThanOrEqual => {
            if truncated {
                rebuild(comparison, cast_and_fold(constant, &column_type))
            } else {
                let Some(next) = create_trunc_add(&part, &constant, &column_type) else {
                    return false;
                };
                rebuild(comparison, next)
            }
        }
        ComparisonOp::LessThanOrEqual | ComparisonOp::GreaterThan => {
            let Some(next) = create_trunc_add(&part, &constant, &column_type) else {
                return false;
            };
            // <= becomes <, > becomes >=
            let op = if comparison == ComparisonOp::GreaterThan {
                ComparisonOp::GreaterThanOrEqual
            } else {
                ComparisonOp::LessThan
            };
            rebuild(op, next)
        }
    };
    *expr = result;
    true
}

/// Whether the constant is a date of the years before the common era.
fn text_is_bc(constant: &BoundExpr) -> bool {
    try_evaluate_scalar(&constant.clone().cast_to(&LogicalType::DATE, true))
        .and_then(|d| d.try_to_string().ok())
        .is_some_and(|text| text.ends_with("(BC)") || text.starts_with('-'))
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
    if is_integral(&expr.return_type) {
        // multiply by zero: zero, or NULL where the other side is
        if name == "*" && is(0) {
            let return_type = expr.return_type.clone();
            let zero = BoundExpr::constant(Value::bigint(0)).cast_to(&return_type, false);
            let zero = try_evaluate_scalar(&zero).map_or(zero, BoundExpr::constant);
            let alias = expr.alias.clone();
            *expr = crate::function::constant_or_null(vec![zero, take(&mut children[other])]).with_alias(alias);
            return true;
        }
        // divide by zero where that gives NULL (`error_on_division_by_zero = false`)
        if name == "//" && constant_child == 1 && is(0) && function.null_on_zero {
            expr.kind = BoundExprKind::Constant(Value::null(expr.return_type.clone()));
            return true;
        }
    }
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
pub(super) fn cast_is_invertible(source: &LogicalType, target: &LogicalType) -> bool {
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

/// The scalar functions C++ declares fallible (`SetFallible`, `FunctionErrors::CAN_THROW_RUNTIME_ERROR`),
/// by name: an overload of one is taken to fail as the most fallible one does. The list functions
/// that are `list_aggr` macros in C++ fail as it does. Every other function cannot fail, as C++'s
/// default `FunctionErrors::CANNOT_ERROR` has it.
const FALLIBLE_FUNCTIONS: &[&str] = &[
    "!__postfix", "%", "&", "*", "**", "+", "-", "->>", "/", "//", "<<", ">>", "@", "__internal_barrier",
    "__internal_try_rethrow", "abs", "acos", "add", "age", "aggregate", "apply", "array_aggr", "array_aggregate",
    "array_apply", "array_cosine_distance", "array_cosine_similarity", "array_cross_product", "array_distance",
    "array_dot_product", "array_filter", "array_inner_product", "array_length", "array_negative_dot_product",
    "array_negative_inner_product", "array_reduce", "array_resize", "array_select", "array_slice", "array_to_json",
    "array_transform", "array_value", "array_where", "asin", "atanh", "bar", "binom", "bitstring", "century", "chr",
    "cos", "cot", "currval", "date_diff", "date_part", "date_sub", "date_trunc", "datediff", "datepart", "datesub",
    "datetrunc", "day", "dayofmonth", "dayofweek", "dayofyear", "decade", "decimal_division", "decode", "divide",
    "epoch_ms", "epoch_ns", "epoch_us", "equi_width_bins", "era", "error", "factorial", "filter", "fmod", "format",
    "from_base64", "from_binary", "from_hex", "from_json", "from_json_strict", "gamma", "gcd", "generate_series",
    "get_bit", "get_current_time", "greatest_common_divisor", "hamming", "ilike_escape", "invoke", "isodow",
    "isoyear", "jaccard", "json_array", "json_array_length", "json_contains", "json_deep_merge",
    "json_deserialize_sql", "json_exists", "json_extract", "json_extract_path", "json_extract_path_text",
    "json_extract_string", "json_insert", "json_keys", "json_merge_patch_diff", "json_object", "json_quote",
    "json_remove", "json_replace", "json_set", "json_structure", "json_transform", "json_transform_strict",
    "json_type", "json_value", "last_day", "lcm", "least_common_multiple", "left", "left_grapheme", "lgamma",
    "like_escape", "list_aggr", "list_aggregate", "list_any_value", "list_apply", "list_approx_count_distinct",
    "list_avg", "list_bit_and", "list_bit_or", "list_bit_xor", "list_bool_and", "list_bool_or",
    "list_cosine_distance", "list_cosine_similarity", "list_count", "list_distance", "list_dot_product",
    "list_entropy", "list_filter", "list_first", "list_histogram", "list_inner_product", "list_kurtosis",
    "list_kurtosis_pop", "list_last", "list_mad", "list_max", "list_median", "list_min", "list_mode",
    "list_negative_dot_product", "list_negative_inner_product", "list_product", "list_reduce", "list_resize",
    "list_select", "list_sem", "list_skewness", "list_slice", "list_stddev_pop", "list_stddev_samp",
    "list_string_agg", "list_sum", "list_transform", "list_var_pop", "list_var_samp", "list_where", "ln", "log",
    "log10", "log2", "lpad", "make_date", "make_time", "make_timestamp", "make_timestamp_ms", "make_timestamp_ns",
    "make_timestamptz", "map", "map_from_entries", "map_keys", "map_values", "millennium", "mismatches", "mod",
    "month", "multiply", "nextval", "not_ilike_escape", "not_like_escape", "parse_formatted_bytes", "path_join",
    "pow", "power", "printf", "quarter", "range", "reduce", "regexp_extract", "regexp_extract_all",
    "regexp_full_match", "regexp_matches", "regexp_replace", "regexp_split_to_array", "repeat", "right",
    "right_grapheme", "round", "round_even", "roundbankers", "row_to_json", "rpad", "set_bit", "setseed", "setval",
    "sin", "sqrt", "st_asgeojson", "st_geomfromgeojson", "st_geomfromwkb", "str_split_regex", "strftime",
    "string_split_regex", "strptime", "subtract", "tan", "time_bucket", "timezone", "timezone_hour",
    "timezone_minute", "to_base", "to_centuries", "to_days", "to_decades", "to_hours", "to_json", "to_microseconds",
    "to_millennia", "to_milliseconds", "to_minutes", "to_months", "to_quarters", "to_seconds", "to_timestamp",
    "to_weeks", "to_years", "translate", "unbin", "unhex", "url_decode", "uuid_extract_timestamp",
    "variant_extract_string", "variant_normalize", "week", "weekday", "weekofyear", "write_log", "xor", "year",
    "yearweek", "|", "~",
];

/// ICU's date parts: the overloads of TIMESTAMP WITH TIME ZONE are fallible (`ICUDatePart`).
const ICU_DATE_PARTS: &[&str] = &[
    "century", "date_part", "datepart", "day", "dayname", "dayofmonth", "dayofweek", "dayofyear", "decade", "epoch",
    "era", "hour", "isodow", "isoyear", "julian", "last_day", "microsecond", "millennium", "millisecond", "minute",
    "month", "monthname", "quarter", "second", "timezone", "timezone_hour", "timezone_minute", "week", "weekday",
    "weekofyear", "year", "yearweek",
];

/// `BoundAggregateExpression::CanThrow` of the aggregate itself: the aggregates C++ declares
/// fallible (`SetFallible`), by the overload where it decides by the input type.
pub(crate) fn aggregate_can_throw(function: &AggregateFunction, children: &[BoundExpr]) -> bool {
    use LogicalTypeId::*;
    let first = children.first().map(|c| &c.return_type);
    let id = first.map(LogicalType::id);
    // the DECIMALs whose sum is a HUGEINT, which throws on overflow
    let wide = first.is_some_and(|t| t.id() == Decimal && t.decimal_properties().is_some_and(|(w, _)| w > 18));
    match function.name.as_str() {
        "approx_top_k" | "lttb" | "mad" | "kurtosis" | "kurtosis_pop" | "skewness" | "string_agg" | "group_concat"
        | "listagg" | "sum_no_overflow" | "variant_group_object" | "bitstring_agg" | "combine_aggr" => true,
        "sum" => id == Some(HugeInt) || wide,
        "avg" | "mean" => matches!(id, Some(HugeInt | Interval)) || wide,
        "bit_and" | "bit_or" | "bit_xor" => id == Some(Bit),
        // the `n` variants reject NULL and out-of-range `n` when they run
        "min" | "max" => children.len() == 2,
        "arg_min" | "argmin" | "min_by" | "arg_max" | "argmax" | "max_by" => children.len() == 3,
        // interpolating temporal values throws for the infinities and out-of-range dates
        "quantile_cont" | "median" => matches!(
            id,
            Some(Date | Timestamp | TimestampTz | TimestampTzNs | TimestampSec | TimestampMs | TimestampNs | Interval)
        ),
        // `HasExtremeDecimalQuantile`
        "quantile_disc" | "quantile" => id == Some(Decimal),
        _ => false,
    }
}

/// `BoundWindowExpression::CanThrow` of the function itself: its aggregate's, or `ntile`'s, whose
/// bucket count must be positive.
pub(crate) fn window_can_throw(window: &BoundWindow) -> bool {
    match &window.aggregate {
        Some(aggregate) => aggregate_can_throw(aggregate, &window.children),
        None => matches!(window.function, WindowFunction::Ntile),
    }
}

/// `BoundFunctionExpression::CanThrow` of the function itself, by its overload where C++ decides
/// it by the arguments.
fn function_can_throw(function: &ScalarFunction, children: &[BoundExpr], return_type: &LogicalType) -> bool {
    use LogicalTypeId::*;
    if function.cannot_error {
        return false;
    }
    let name = function.name.as_str();
    let first = children.first().map(|c| c.return_type.id());
    match name {
        // `StringExtractBind`: a constant index in substring's range cannot fail; `array_extract` of
        // a LIST, STRUCT or tuple cannot fail, `list_extract` of a LIST can
        "array_extract" | "list_extract" | "list_element" if first == Some(Varchar) => {
            children.len() != 2 || !offset_in_range(&children[1], None)
        }
        "array_extract" => false,
        "list_extract" | "list_element" => true,
        // `SubstringBind`: constants in the supported range cannot fail
        "substring" | "substr" | "substring_grapheme" => !substring_in_range(name, children),
        // unary `+` is `NopFunction`; negating a DECIMAL or BIGNUM, and subtracting BIGNUMs, cannot fail
        "+" if children.len() == 1 => false,
        "-" if children.len() == 1 => !matches!(first, Some(Decimal | Bignum)),
        "-" if children.iter().all(|c| c.return_type.id() == Bignum) => false,
        // `ListValueResolveTypes`: the arguments are cast to the child type when it runs
        "list_value" | "list_pack" => return_type.child_type().is_some_and(|child| {
            children.iter().any(|c| cast_can_throw(&c.return_type, child))
        }),
        _ if ICU_DATE_PARTS.contains(&name)
            && children.iter().any(|c| matches!(c.return_type.id(), TimestampTz | TimestampTzNs)) =>
        {
            true
        }
        // compressed materialization's functions throw when assertions are on (`CMIntegralCompressFun`)
        _ if name.starts_with("__internal_compress") => true,
        _ => FALLIBLE_FUNCTIONS.contains(&name),
    }
}

/// `Expression::CanThrow`, leaning towards true.
/// `ExpressionBarrier::Contains`.
pub(crate) fn contains_barrier(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Function { function, .. } if function.name == "__internal_barrier")
        || expr.children().into_iter().any(contains_barrier)
}

pub(crate) fn can_throw(expr: &BoundExpr) -> bool {
    let own = match &expr.kind {
        BoundExprKind::Subquery { .. } | BoundExprKind::ListLambda { .. } | BoundExprKind::Sequence { .. } => true,
        BoundExprKind::Cast { child, try_cast } => cast_can_throw_of(&child.return_type, &expr.return_type, *try_cast),
        BoundExprKind::Function { function, children, .. } => function_can_throw(function, children, &expr.return_type),
        _ => false,
    };
    own || expr.children().into_iter().any(can_throw)
}

/// `SubstringBind`: a substring whose offset and length are constants in the supported range cannot fail.
fn substring_in_range(name: &str, children: &[BoundExpr]) -> bool {
    if !matches!(name, "substring" | "substr" | "substring_grapheme") || !(2..=3).contains(&children.len()) {
        return false;
    }
    offset_in_range(&children[1], children.get(2))
}

/// `SubstringInSupportedRange` of a constant offset and length; a missing length (`StringExtractBind`
/// has 1, a substring without one runs to the end) is in the range.
fn offset_in_range(offset: &BoundExpr, length: Option<&BoundExpr>) -> bool {
    let constant = |expr: &BoundExpr| -> Option<i128> {
        let folded = match &expr.kind {
            BoundExprKind::Constant(_) => expr.clone(),
            _ if expr.is_foldable() => BoundExpr::constant(try_evaluate_scalar(expr)?),
            _ => return None,
        };
        match &folded.kind {
            BoundExprKind::Constant(value) if !value.is_null() => integer_of(value),
            _ => None,
        }
    };
    const UPPER: i128 = u32::MAX as i128;
    const LOWER: i128 = -UPPER - 1;
    let offset = constant(offset);
    let length = match length {
        Some(length) => constant(length),
        None => Some(UPPER),
    };
    matches!((offset, length), (Some(o), Some(l)) if (LOWER..=UPPER).contains(&o) && (LOWER..=UPPER).contains(&l))
}

/// `BoundCastExpression::CastCanThrow` of a cast that is not a TRY_CAST.
pub(crate) fn cast_can_throw(source: &LogicalType, target: &LogicalType) -> bool {
    cast_can_throw_of(source, target, false)
}

/// `BoundCastExpression::CastCanThrow`.
pub(crate) fn cast_can_throw_of(source: &LogicalType, target: &LogicalType, try_cast: bool) -> bool {
    use LogicalTypeId::*;
    if source == target {
        return false;
    }
    // converting VARIANT can hit its traversal depth limit, and extensions implement the casts of
    // custom types, which can throw even as TRY_CAST
    if source.id() == Variant || source.alias().is_some() || target.alias().is_some() {
        return true;
    }
    if try_cast || target.id() == Varchar || (source.id() == Geometry && target.id() == Geometry) {
        return false;
    }
    // parsing a string, or an ENUM's, throws for what is not the target type
    if matches!(source.id(), Varchar | Enum | Bignum) {
        return true;
    }
    if target.id() == Bignum && matches!(source.id(), Float | Double) {
        return true;
    }
    if source.is_nested() || target.is_nested() {
        return nested_cast_can_throw(source, target);
    }
    if target.id() == Decimal {
        return !decimal_holds_every_value(source, target);
    }
    // a DATE far in the future does not fit in a TIMESTAMP
    if matches!(target.id(), Timestamp | TimestampMs | TimestampNs | TimestampSec | TimestampTz | TimestampTzNs) {
        return true;
    }
    // the target must hold every value of the source: be the larger of the two types
    crate::function::combine_types::try_max_logical_type_unchecked(source, target).is_none_or(|max| &max != target)
}

/// `NestedCastCanThrow`: a nested cast runs the casts of its children, and throws where the shapes
/// do not fit.
fn nested_cast_can_throw(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    if source.id() == Union || target.id() == Union {
        return true;
    }
    match source.id() {
        List | Array => {
            if target.id() == Array {
                // a fixed-size ARRAY throws for other lengths
                let any = |t: &LogicalType| t.array_size().is_none_or(|n| n == 0);
                if source.id() != Array || any(source) || any(target) || source.array_size() != target.array_size() {
                    return true;
                }
            } else if target.id() != List {
                return true;
            }
            match (source.child_type(), target.child_type()) {
                (Some(from), Some(to)) => cast_can_throw(from, to),
                _ => true,
            }
        }
        Struct | Tuple => {
            if !matches!(target.id(), Struct | Tuple) {
                return true;
            }
            // the fields pair by name, or by position for unnamed structs; a field without a pair is
            // dropped or NULL, which does not throw
            let Ok(mapping) = crate::function::struct_field_mapping(source, target) else {
                return true;
            };
            let (from, to) = (source.struct_children(), target.struct_children());
            mapping
                .iter()
                .enumerate()
                .any(|(t, s)| s.is_some_and(|s| cast_can_throw(&from[s].1, &to[t].1)))
        }
        Map => {
            if target.id() != Map {
                return true;
            }
            let entry = |t: &LogicalType| t.child_type().map(|c| c.struct_children().to_vec());
            let (Some(from), Some(to)) = (entry(source), entry(target)) else {
                return true;
            };
            if from.len() != 2 || to.len() != 2 {
                return true;
            }
            // keys must stay unique, and cannot be NULL
            (from[0].1 != to[0].1 && !cast_is_invertible(&from[0].1, &to[0].1))
                || cast_can_throw(&from[0].1, &to[0].1)
                || cast_can_throw(&from[1].1, &to[1].1)
        }
        _ => true,
    }
}

/// `DecimalHoldsEveryValue`: the DECIMAL keeps every digit of the source on both sides of the point.
fn decimal_holds_every_value(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    let Some((width, scale)) = target.decimal_properties() else {
        return false;
    };
    let integral = i32::from(width) - i32::from(scale);
    if let Some((source_width, source_scale)) = source.decimal_properties() {
        return scale >= source_scale && integral >= i32::from(source_width) - i32::from(source_scale);
    }
    // `IntegerTypeDecimalDigits`
    let digits = match source.id() {
        TinyInt | UTinyInt => 3,
        SmallInt | USmallInt => 5,
        Integer | UInteger => 10,
        BigInt => 19,
        UBigInt => 20,
        _ => 0,
    };
    digits > 0 && integral >= digits
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

/// `replace(s, 'z', 'z')` is `s`, and so are `s || ''` and `'' || s`.
fn identity_string_functions(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return false;
    };
    let text = |e: &BoundExpr| match &e.kind {
        BoundExprKind::Constant(v) if !v.is_null() && e.return_type.id() == LogicalTypeId::Varchar => v.try_to_string().ok(),
        _ => None,
    };
    let input = match (function.name.as_str(), children.as_slice()) {
        ("replace", [input, from, to]) if text(from).is_some() && text(from) == text(to) => input,
        ("||", [input, other]) if text(other).as_deref() == Some("") => input,
        ("||", [other, input]) if text(other).as_deref() == Some("") => input,
        _ => return false,
    };
    if input.return_type != expr.return_type {
        return false;
    }
    *expr = input.clone();
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
