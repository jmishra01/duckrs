// Ported from duckdb/src/planner/subquery/flatten_dependent_join.cpp,
// duckdb/src/planner/subquery/rewrite_correlated_expressions.cpp and
// duckdb/src/planner/binder/query_node/plan_subquery.cpp @ rust-port-baseline
//
//! Flattening correlated subqueries ("Unnesting Arbitrary Subqueries"): a subquery in a filter or a
//! projection becomes a duplicate eliminated join. The distinct values of its correlated columns
//! (the domain) go into the subquery through `DelimGet`, down to where the plan stops depending on
//! the query around it, and the subquery then runs once for all of them.
//!
//! This covers less than C++, and what it does not cover keeps working: a subquery that cannot be
//! flattened stays an expression, which runs its own plan per combination of correlated values. So
//! a case here gives up with [`Unsupported`], where C++ handles it or raises an error. That is the
//! case for subqueries in other operators or under TRY, volatile ones, columns of a query further
//! out, and operators that `push` has no rule for.

use super::column_binding_resolver::resolve_column_bindings;
use super::expression::{BoundExpr, BoundExprKind, BoundOperator, BoundSubquery, BoundWindow, ColumnBinding};
use super::expression::{SubqueryKind, WindowFunction};
use super::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use crate::execution::run_plan;
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::optimizer::conjunction_of;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType, WindowBoundary, WindowExclude};
use parking_lot::Mutex;
use std::sync::Arc;

/// This subquery stays an expression.
struct Unsupported;

type Flat<T> = std::result::Result<T, Unsupported>;

fn numbered(table_index: usize, offset: usize, count: usize) -> Vec<ColumnBinding> {
    (offset..offset + count)
        .map(|i| ColumnBinding::new(table_index, i))
        .collect()
}

/// Flattens the subqueries of a plan. Its output stays what it was.
pub fn flatten_subqueries(plan: &mut LogicalOperator, next_table_index: &mut usize) {
    let bindings = plan.column_bindings();
    let types = plan.types();
    flatten_operator(plan, next_table_index);
    restore_output(plan, bindings, types, next_table_index);
}

/// A join below adds columns at the end of the output. Most operators read their child through
/// bindings, and the others get a projection that takes the added columns off again.
fn restore_output(
    plan: &mut LogicalOperator,
    bindings: Vec<ColumnBinding>,
    types: Vec<LogicalType>,
    next_table_index: &mut usize,
) {
    if plan.column_bindings().len() == bindings.len() {
        return;
    }
    let expressions = bindings
        .into_iter()
        .zip(types)
        .map(|(binding, ty)| BoundExpr::column_ref(binding, ty, ""))
        .collect();
    let table_index = take_index(next_table_index);
    let child = std::mem::replace(plan, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *plan = LogicalOperator::new(
        K::Projection {
            table_index,
            expressions,
        },
        vec![child],
    );
}

fn take_index(next_table_index: &mut usize) -> usize {
    let index = *next_table_index;
    *next_table_index += 1;
    index
}

fn flatten_operator(op: &mut LogicalOperator, next_table_index: &mut usize) {
    let by_binding = matches!(
        op.kind,
        K::Filter { .. }
            | K::Projection { .. }
            | K::Aggregate { .. }
            | K::Order { .. }
            | K::Limit { .. }
            | K::LimitPercent { .. }
            | K::Sample { .. }
            | K::Window { .. }
            | K::Distinct { .. }
            | K::Unnest { .. }
            | K::CrossProduct
            | K::Join { .. }
            | K::DelimJoin { .. }
            | K::MarkJoin { .. }
            | K::AsOfJoin { .. }
            | K::LateralJoin { .. }
    );
    for child in &mut op.children {
        let bindings = child.column_bindings();
        let types = child.types();
        flatten_operator(child, next_table_index);
        if !by_binding {
            restore_output(child, bindings, types, next_table_index);
        }
    }
    if let K::LateralJoin { right, .. } = &mut op.kind {
        flatten_subqueries(right, next_table_index);
    }
    if op.children.len() != 1 {
        return;
    }
    if let K::Filter { expressions } | K::Projection { expressions, .. } = &mut op.kind {
        for expr in expressions {
            flatten_expression(expr, &mut op.children[0], next_table_index);
        }
    }
}

/// `Binder::PlanSubqueries`: `left` is the child of the operator `expr` belongs to.
fn flatten_expression(expr: &mut BoundExpr, left: &mut LogicalOperator, next_table_index: &mut usize) {
    // TRY runs a failed chunk again row by row, and a lambda has parameters for columns
    if matches!(
        expr.kind,
        BoundExprKind::Operator {
            op: BoundOperator::Try | BoundOperator::HasError,
            ..
        } | BoundExprKind::ListLambda { .. }
    ) {
        return;
    }
    for child in expr.children_mut() {
        flatten_expression(child, left, next_table_index);
    }
    if let Ok(flattened) = flatten_subquery(expr, left, next_table_index) {
        *expr = flattened;
    }
}

/// Whether an expression can run on the output of `left`.
fn is_local(expr: &BoundExpr, bindings: &[ColumnBinding]) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth } => *depth == 0 && bindings.contains(binding),
        BoundExprKind::Subquery { .. } => false,
        _ => expr.children().into_iter().all(|child| is_local(child, bindings)),
    }
}

/// `PlanReturnsExactlyOneRow`.
fn returns_exactly_one_row(op: &LogicalOperator) -> bool {
    match &op.kind {
        K::Aggregate {
            groups,
            grouping_sets,
            grouping_functions,
            ..
        } => groups.is_empty() && grouping_sets.is_empty() && grouping_functions.is_empty(),
        K::DummyScan { .. } => true,
        K::Limit {
            limit: Some(1),
            offset: 0,
            failure: None,
        }
        | K::Projection { .. } => op.children.len() == 1 && returns_exactly_one_row(&op.children[0]),
        _ => false,
    }
}

/// `PlanUncorrelatedSubquery` for a scalar subquery: a cross product with it. Unless it returns
/// exactly one row, an aggregate takes its first row, and with `error_on_multiple_rows` the count
/// of its rows says whether there are too many.
fn flatten_scalar(
    expr: &BoundExpr,
    raw: &LogicalOperator,
    error_on_multiple_rows: bool,
    left: &mut LogicalOperator,
    next_table_index: &mut usize,
) -> Flat<BoundExpr> {
    let left_bindings = left.column_bindings();
    let mut right = raw.clone();
    let right_bindings = right.column_bindings();
    if right_bindings.len() != 1 || right_bindings.iter().any(|b| left_bindings.contains(b)) {
        return Err(Unsupported);
    }
    let ty = expr.return_type.clone();
    let mut output = BoundExpr::column_ref(right_bindings[0], ty.clone(), "");
    if ty.id() == duckrs_common::types::LogicalTypeId::SqlNull || !returns_exactly_one_row(raw) {
        let functions = BuiltinFunctions::get();
        let first = functions.aggregate("first").ok_or(Unsupported)?;
        let mut aggregates = vec![bind_aggregate_function(first, vec![output], None, false).map_err(|_| Unsupported)?];
        if error_on_multiple_rows {
            let count_star = functions.aggregate("count_star").ok_or(Unsupported)?;
            aggregates.push(bind_aggregate_function(count_star, Vec::new(), None, false).map_err(|_| Unsupported)?);
        }
        let aggregate_index = take_index(next_table_index);
        let aggregate = LogicalOperator::new(
            K::Aggregate {
                group_index: take_index(next_table_index),
                aggregate_index,
                groupings_index: take_index(next_table_index),
                groups: Vec::new(),
                aggregates,
                grouping_sets: Vec::new(),
                grouping_functions: Vec::new(),
                perfect_hash: false,
                domain_cap: None,
            },
            vec![right],
        );
        let first_ref = BoundExpr::column_ref(ColumnBinding::new(aggregate_index, 0), ty.clone(), "");
        if !error_on_multiple_rows {
            right = aggregate;
            output = first_ref;
        } else {
            // CASE WHEN count > 1 THEN error(...) ELSE first END
            let count_ref = BoundExpr::column_ref(ColumnBinding::new(aggregate_index, 1), LogicalType::BIGINT, "");
            let count_check = BoundExpr::new(
                BoundExprKind::Comparison {
                    op: ComparisonOp::GreaterThan,
                    left: Box::new(count_ref),
                    right: Box::new(BoundExpr::constant(Value::bigint(1))),
                },
                LogicalType::BOOLEAN,
            );
            let error = functions.scalar("error").ok_or(Unsupported)?;
            let message = BoundExpr::constant(Value::varchar(crate::execution::expression_executor::MULTIPLE_ROWS));
            let mut error = crate::function::binder::bind_scalar_function(error, vec![message], false, &|_| None)
                .map_err(|_| Unsupported)?;
            error.return_type = ty.clone();
            let case = BoundExpr::new(
                BoundExprKind::Case {
                    checks: vec![(count_check, error)],
                    else_expr: Box::new(first_ref),
                },
                ty.clone(),
            );
            let projection_index = take_index(next_table_index);
            right = LogicalOperator::new(
                K::Projection {
                    table_index: projection_index,
                    expressions: vec![case],
                },
                vec![aggregate],
            );
            output = BoundExpr::column_ref(ColumnBinding::new(projection_index, 0), ty, "");
        }
    }
    let outer = std::mem::replace(left, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *left = LogicalOperator::new(K::CrossProduct, vec![outer, right]);
    Ok(output.with_alias(expr.alias.clone()))
}

/// `PlanUncorrelatedSubquery` for EXISTS: a cross product with `count_star() = 1` of its first row
/// (`LIMIT 1`), which is one row of either TRUE or FALSE.
fn flatten_exists(
    expr: &BoundExpr,
    raw: &LogicalOperator,
    left: &mut LogicalOperator,
    next_table_index: &mut usize,
) -> Flat<BoundExpr> {
    let left_bindings = left.column_bindings();
    if raw.column_bindings().iter().any(|b| left_bindings.contains(b)) {
        return Err(Unsupported);
    }
    let limit = LogicalOperator::new(
        K::Limit {
            limit: Some(1),
            offset: 0,
            failure: None,
        },
        vec![raw.clone()],
    );
    let count_star = BuiltinFunctions::get().aggregate("count_star").ok_or(Unsupported)?;
    let count_star = bind_aggregate_function(count_star, Vec::new(), None, false).map_err(|_| Unsupported)?;
    let count_type = count_star.return_type.clone();
    let aggregate_index = take_index(next_table_index);
    let aggregate = LogicalOperator::new(
        K::Aggregate {
            group_index: take_index(next_table_index),
            aggregate_index,
            groupings_index: take_index(next_table_index),
            groups: Vec::new(),
            aggregates: vec![count_star],
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
            perfect_hash: false,
            domain_cap: None,
        },
        vec![limit],
    );
    let comparison = BoundExpr::new(
        BoundExprKind::Comparison {
            op: ComparisonOp::Equal,
            left: Box::new(BoundExpr::column_ref(ColumnBinding::new(aggregate_index, 0), count_type, "")),
            right: Box::new(BoundExpr::constant(Value::bigint(1))),
        },
        LogicalType::BOOLEAN,
    );
    let projection_index = take_index(next_table_index);
    let right = LogicalOperator::new(
        K::Projection {
            table_index: projection_index,
            expressions: vec![comparison],
        },
        vec![aggregate],
    );
    let outer = std::mem::replace(left, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *left = LogicalOperator::new(K::CrossProduct, vec![outer, right]);
    let output = BoundExpr::column_ref(ColumnBinding::new(projection_index, 0), LogicalType::BOOLEAN, "");
    Ok(output.with_alias(expr.alias.clone()))
}

/// `PlanUncorrelatedSubquery` for `x IN (...)`, the case that gains from a join; the other ANY
/// comparisons run once as they are. A MARK join puts whether a right row equals `x` next to each left row.
fn flatten_in(expr: &BoundExpr, left: &mut LogicalOperator, next_table_index: &mut usize) -> Flat<BoundExpr> {
    let BoundExprKind::Subquery {
        subquery,
        child: Some(child),
        ..
    } = &expr.kind
    else {
        return Err(Unsupported);
    };
    // uncorrelated, it runs once, volatile or not
    let BoundSubquery {
        kind: SubqueryKind::Any(ComparisonOp::Equal),
        raw: Some(raw),
        row,
        ..
    } = subquery.as_ref()
    else {
        return Err(Unsupported);
    };
    let left_bindings = left.column_bindings();
    let right_bindings = raw.column_bindings();
    if !is_local(child, &left_bindings)
        || right_bindings.len() != 1
        || right_bindings.iter().any(|b| left_bindings.contains(b))
    {
        return Err(Unsupported);
    }
    let table_index = take_index(next_table_index);
    let right = BoundExpr::column_ref(right_bindings[0], raw.types()[0].clone(), "");
    // `MarkJoinRowComparison`: a row compares column by column, each pair of rows in turn
    let (conditions, predicate) = match row_comparison(ComparisonOp::Equal, child, &right).filter(|_| *row) {
        Some(predicate) => (Vec::new(), Some(predicate)),
        None => (
            vec![JoinCondition {
                left: (**child).clone(),
                right,
                comparison: ComparisonOp::Equal,
                filter: None,
            }],
            None,
        ),
    };
    let outer = std::mem::replace(left, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *left = LogicalOperator::new(
        K::MarkJoin {
            table_index,
            conditions,
            predicate,
        },
        vec![outer, raw.clone()],
    );
    let marker = BoundExpr::column_ref(ColumnBinding::new(table_index, 0), LogicalType::BOOLEAN, "");
    Ok(marker.with_alias(expr.alias.clone()))
}

/// `MarkJoinRowComparison::CompareEquality`: a row `=` or `<>` of a MARK join under SQL's
/// three-valued logic, `a = x AND b = y` and `a <> x OR b <> y`. Other comparisons are of the whole
/// row, ordered as a struct is.
fn row_comparison(op: ComparisonOp, child: &BoundExpr, right: &BoundExpr) -> Option<BoundExpr> {
    let and = match op {
        ComparisonOp::Equal => true,
        ComparisonOp::NotEqual => false,
        _ => return None,
    };
    let children = split_row(child, right)?
        .into_iter()
        .map(|(left, right)| {
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                LogicalType::BOOLEAN,
            )
        })
        .collect();
    Some(BoundExpr::new(
        BoundExprKind::Conjunction { and, children },
        LogicalType::BOOLEAN,
    ))
}

/// The pairs of columns of `(a, b) op ANY (SELECT x, y ...)`, if the left side is a row.
fn split_row(child: &BoundExpr, right: &BoundExpr) -> Option<Vec<(BoundExpr, BoundExpr)>> {
    let BoundExprKind::Function { function, children, .. } = &child.kind else {
        return None;
    };
    let width = right.return_type.struct_children().len();
    if function.name != "row"
        || right.return_type.id() != duckrs_common::types::LogicalTypeId::Tuple
        || width < 2
        || children.len() != width
    {
        return None;
    }
    children
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let field = crate::function::nested::bind_nested_function(
                "struct_extract_at",
                vec![String::new(); 2],
                vec![right.clone(), BoundExpr::constant(Value::bigint(i as i64 + 1))],
            )
            .ok()?;
            Some((column.clone(), field))
        })
        .collect()
}

/// `PlanCorrelatedSubquery`: puts the join on top of `left`, and gives what stands in for `expr`.
fn flatten_subquery(expr: &BoundExpr, left: &mut LogicalOperator, next_table_index: &mut usize) -> Flat<BoundExpr> {
    let BoundExprKind::Subquery {
        subquery,
        child,
        correlated,
    } = &expr.kind
    else {
        return Err(Unsupported);
    };
    // an uncorrelated scalar subquery runs once, volatile or not
    if let BoundSubquery {
        kind: SubqueryKind::Scalar,
        raw: Some(raw),
        error_on_multiple_rows,
        ..
    } = subquery.as_ref()
        && correlated.is_empty()
    {
        return flatten_scalar(expr, raw, *error_on_multiple_rows, left, next_table_index);
    }
    if let BoundSubquery {
        kind: SubqueryKind::Exists,
        raw: Some(raw),
        ..
    } = subquery.as_ref()
        && correlated.is_empty()
    {
        return flatten_exists(expr, raw, left, next_table_index);
    }
    if correlated.is_empty() {
        return flatten_in(expr, left, next_table_index);
    }
    let BoundSubquery {
        kind,
        raw: Some(raw),
        correlated: bindings,
        volatile: false,
        error_on_multiple_rows,
        row,
        ..
    } = subquery.as_ref()
    else {
        return Err(Unsupported);
    };
    let left_bindings = left.column_bindings();
    if correlated.is_empty() {
        return flatten_in(expr, left, next_table_index);
    }
    if !correlated.iter().all(|c| is_local(c, &left_bindings)) || !child.iter().all(|c| is_local(c, &left_bindings)) {
        return Err(Unsupported);
    }
    let mut right = raw.clone();
    let output = right.column_bindings()[0];
    let output_type = right.types()[0].clone();
    let domain: Vec<(ColumnBinding, LogicalType)> = bindings
        .iter()
        .zip(correlated)
        .map(|(binding, value)| (*binding, value.return_type.clone()))
        .collect();
    let types = domain.iter().map(|(_, ty)| ty.clone()).collect::<Vec<_>>();
    let delim = Arc::new(Mutex::new(ColumnDataCollection::new(types)));
    let names = correlated.iter().map(|value| value.alias.clone()).collect();
    let mut flatten = Flatten {
        domain,
        names,
        delim: delim.clone(),
        next_table_index,
        empty_values: Vec::new(),
        propagate_null: matches!(kind, SubqueryKind::Scalar),
    };
    let state = flatten.push(&mut right)?;
    flatten.apply_empty_values(&mut right);
    if flatten.reads_domain(&mut right) {
        return Err(Unsupported);
    }
    // a copy of a subquery that is flattened already has the same bindings as that one
    let right_bindings = right.column_bindings();
    if right_bindings.iter().any(|b| left_bindings.contains(b)) {
        return Err(Unsupported);
    }
    let result_value = flatten.empty_value_of(output, &expr.return_type);
    let conditions = correlated
        .iter()
        .zip(&state)
        .map(|(value, binding)| JoinCondition {
            left: value.clone(),
            right: BoundExpr::column_ref(*binding, value.return_type.clone(), value.alias.clone()),
            comparison: ComparisonOp::NotDistinctFrom,
            filter: None,
        })
        .collect();
    let (join_type, mark_index, predicate) = match kind {
        SubqueryKind::Scalar => (JoinType::Single, None, None),
        SubqueryKind::Exists => (JoinType::Mark, Some(take_index(next_table_index)), None),
        SubqueryKind::Any(op) => {
            let Some(child) = child else {
                return Err(Unsupported);
            };
            let right = BoundExpr::column_ref(output, output_type, "");
            let comparison = row_comparison(*op, child, &right).filter(|_| *row).unwrap_or_else(|| {
                BoundExpr::new(
                    BoundExprKind::Comparison {
                        op: *op,
                        left: child.clone(),
                        right: Box::new(right),
                    },
                    LogicalType::BOOLEAN,
                )
            });
            (JoinType::Mark, Some(take_index(next_table_index)), Some(comparison))
        }
    };
    let replacement = match mark_index {
        Some(index) => BoundExpr::column_ref(ColumnBinding::new(index, 0), LogicalType::BOOLEAN, ""),
        None => result_value,
    };
    let outer = std::mem::replace(left, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *left = LogicalOperator::new(
        K::DelimJoin {
            join_type,
            conditions,
            predicate,
            delim,
            mark_index,
            error_on_multiple_rows: *error_on_multiple_rows,
        },
        vec![outer, right],
    );
    Ok(replacement.with_alias(expr.alias.clone()))
}

/// An aggregate without groups gives a row for no rows, and now has a group per domain value. A
/// domain value without rows gets `value`: what the aggregate gives for no rows, where that is not
/// NULL. `marker` is NULL for those.
struct EmptyValue {
    aggregate: ColumnBinding,
    marker: ColumnBinding,
    value: Value,
}

/// `FlattenDependentJoins`.
struct Flatten<'a> {
    domain: Vec<(ColumnBinding, LogicalType)>,
    /// The names of the correlated columns, which the columns standing in for them take.
    names: Vec<String>,
    delim: Arc<Mutex<ColumnDataCollection>>,
    next_table_index: &'a mut usize,
    empty_values: Vec<EmptyValue>,
    /// `propagate_null_values`: a row of NULLs at this point in the plan gives what the join
    /// gives for no row at all, so such a row need not exist.
    propagate_null: bool,
}

/// `ExpressionBecomesNull`, leaning towards false: whether `expr` is NULL for NULL in `columns`.
fn becomes_null(expr: &BoundExpr, columns: &[ColumnBinding]) -> bool {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => columns.contains(binding),
        BoundExprKind::Cast { child, .. } => becomes_null(child, columns),
        BoundExprKind::Function {
            function,
            children,
            is_operator: true,
        } if matches!(function.name.as_str(), "+" | "-" | "*" | "/" | "%") => {
            children.iter().any(|child| becomes_null(child, columns))
        }
        _ => false,
    }
}

impl Flatten<'_> {
    fn domain_index(&self, binding: &ColumnBinding) -> Option<usize> {
        self.domain.iter().position(|(b, _)| b == binding)
    }

    fn expression_reads_domain(&self, expr: &BoundExpr) -> bool {
        if let BoundExprKind::ColumnRef { binding, depth } = &expr.kind
            && *depth > 0
            && self.domain_index(binding).is_some()
        {
            return true;
        }
        expr.children().into_iter().any(|c| self.expression_reads_domain(c))
    }

    /// `FlattenDependentJoins::RequiresDomain`.
    fn reads_domain(&self, plan: &mut LogicalOperator) -> bool {
        let mut found = false;
        plan.visit_expressions(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth } = &e.kind {
                found |= *depth > 0 && self.domain_index(binding).is_some();
            }
        });
        found
    }

    fn state_refs(&self, state: &[ColumnBinding]) -> Vec<BoundExpr> {
        state
            .iter()
            .zip(self.domain.iter().zip(&self.names))
            .map(|(binding, ((_, ty), name))| BoundExpr::column_ref(*binding, ty.clone(), name.clone()))
            .collect()
    }

    fn delim_get(&mut self) -> (LogicalOperator, Vec<ColumnBinding>) {
        let table_index = take_index(self.next_table_index);
        let types: Vec<LogicalType> = self.domain.iter().map(|(_, ty)| ty.clone()).collect();
        let state = numbered(table_index, 0, types.len());
        let get = K::DelimGet {
            table_index,
            types,
            delim: self.delim.clone(),
        };
        (LogicalOperator::new(get, Vec::new()), state)
    }

    /// `AttachDomainToIndependentSubtree`: the plan does not depend on the domain, so each of its
    /// rows goes with each domain value.
    fn attach(&mut self, plan: &mut LogicalOperator) -> Vec<ColumnBinding> {
        let (get, state) = self.delim_get();
        if matches!(plan.kind, K::DummyScan { .. }) {
            *plan = get;
        } else {
            let independent = std::mem::replace(plan, LogicalOperator::new(K::CrossProduct, Vec::new()));
            *plan = LogicalOperator::new(K::CrossProduct, vec![independent, get]);
        }
        state
    }

    /// `RewriteCorrelatedExpressions`: the expressions of this operator read `state`.
    fn rewrite(&self, op: &mut LogicalOperator, state: &[ColumnBinding]) {
        let rewrite_expression = |expr: &mut BoundExpr| {
            expr.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth } = &e.kind
                    && *depth > 0
                    && let Some(index) = self.domain_index(binding)
                {
                    e.kind = BoundExprKind::ColumnRef {
                        binding: state[index],
                        depth: 0,
                    };
                }
            });
        };
        for expr in op.expressions_mut() {
            rewrite_expression(expr);
        }
        if let K::Join { conditions, .. } = &mut op.kind {
            for condition in conditions {
                rewrite_expression(&mut condition.left);
                rewrite_expression(&mut condition.right);
            }
        }
    }

    /// `PushDownCorrelatedNode`: makes `plan` independent of the query around it. It then has the
    /// domain values among its columns, which the result says where.
    fn push(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        if !self.reads_domain(plan) {
            return Ok(self.attach(plan));
        }
        self.propagate_null &= match &plan.kind {
            K::Projection { expressions, .. } => {
                let columns = plan.children[0].column_bindings();
                expressions.iter().all(|expr| becomes_null(expr, &columns))
            }
            K::Aggregate { .. } => true,
            _ => false,
        };
        match &plan.kind {
            K::Filter { .. } | K::Order { .. } | K::Unnest { .. } => self.push_through(plan),
            K::Window { .. } => {
                let state = self.push_through(plan)?;
                let refs = self.state_refs(&state);
                let K::Window { windows, .. } = &mut plan.kind else {
                    unreachable!()
                };
                for window in windows {
                    window.partitions.extend(refs.iter().cloned());
                }
                Ok(state)
            }
            K::Distinct { .. } => {
                let state = self.push_through(plan)?;
                let refs = self.state_refs(&state);
                let K::Distinct { targets, .. } = &mut plan.kind else {
                    unreachable!()
                };
                targets.extend(refs);
                Ok(state)
            }
            K::Projection { .. } => {
                let state = self.push_through(plan)?;
                let refs = self.state_refs(&state);
                let K::Projection {
                    table_index,
                    expressions,
                } = &mut plan.kind
                else {
                    unreachable!()
                };
                let offset = expressions.len();
                expressions.extend(refs);
                Ok(numbered(*table_index, offset, state.len()))
            }
            K::Aggregate { .. } => self.push_aggregate(plan),
            K::Limit { .. } => self.push_limit(plan),
            K::Union { .. } | K::ExceptIntersect { .. } => self.push_set_operation(plan),
            K::CrossProduct => self.push_cross_product(plan),
            K::Join { .. } => self.push_join(plan),
            K::DelimJoin { .. } | K::MarkJoin { .. } => {
                // the right side has no values of the left side's domain to tell apart
                if self.reads_domain(&mut plan.children[1]) {
                    return Err(Unsupported);
                }
                self.push_through(plan)
            }
            _ => Err(Unsupported),
        }
    }

    /// `PushDownChild` of the first child, for an operator with the columns of that child.
    fn push_through(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        let state = self.push(&mut plan.children[0])?;
        self.rewrite(plan, &state);
        Ok(state)
    }

    /// `PushDownAggregate`: the domain values are groups too.
    fn push_aggregate(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        let K::Aggregate { grouping_sets, .. } = &plan.kind else {
            unreachable!()
        };
        if grouping_sets.iter().any(|set| set.is_empty()) {
            return Err(Unsupported);
        }
        let propagate_null = std::mem::take(&mut self.propagate_null);
        let state = self.push_through(plan)?;
        let refs = self.state_refs(&state);
        let child_types = plan.children[0].types();
        let child_bindings = plan.children[0].column_bindings();
        let K::Aggregate {
            group_index,
            aggregate_index,
            groups,
            aggregates,
            grouping_sets,
            ..
        } = &mut plan.kind
        else {
            unreachable!()
        };
        let offset = groups.len();
        let has_groups = offset > 0;
        let group_state = numbered(*group_index, offset, refs.len());
        groups.extend(refs.iter().cloned());
        for set in grouping_sets.iter_mut() {
            set.extend(offset..offset + refs.len());
        }
        if has_groups {
            return Ok(group_state);
        }
        // a domain value without rows still has its row: join all of them with the groups
        let values = self.aggregates_of_no_rows(aggregates, child_types, child_bindings)?;
        let drop_missing_keys = propagate_null && values.iter().all(Value::is_null);
        if values.iter().any(|value| !value.is_null()) {
            let marker = ColumnBinding::new(*aggregate_index, aggregates.len());
            let table_index = *aggregate_index;
            for (index, value) in values.into_iter().enumerate() {
                if !value.is_null() {
                    self.empty_values.push(EmptyValue {
                        aggregate: ColumnBinding::new(table_index, index),
                        marker,
                        value,
                    });
                }
            }
            let count_star = BuiltinFunctions::get().aggregate("count_star").ok_or(Unsupported)?;
            aggregates.push(bind_aggregate_function(count_star, Vec::new(), None, false).map_err(|_| Unsupported)?);
        }
        let (get, left_state) = self.delim_get();
        let conditions = self.state_conditions(&left_state, &group_state);
        let aggregate = std::mem::replace(plan, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *plan = LogicalOperator::new(
            K::Join {
                join_type: if drop_missing_keys {
                    JoinType::Inner
                } else {
                    JoinType::Left
                },
                conditions,
                predicate: None,
                algorithm: Default::default(),
            },
            vec![get, aggregate],
        );
        Ok(left_state)
    }

    /// What `aggregates` give for no rows.
    fn aggregates_of_no_rows(
        &mut self,
        aggregates: &[super::expression::BoundAggregate],
        types: Vec<LogicalType>,
        bindings: Vec<ColumnBinding>,
    ) -> Flat<Vec<Value>> {
        let input = LogicalOperator::new(K::EmptyResult { types, bindings }, Vec::new());
        let mut plan = LogicalOperator::new(
            K::Aggregate {
                group_index: take_index(self.next_table_index),
                aggregate_index: take_index(self.next_table_index),
                groupings_index: take_index(self.next_table_index),
                groups: Vec::new(),
                aggregates: aggregates.to_vec(),
                grouping_sets: Vec::new(),
                grouping_functions: Vec::new(),
                perfect_hash: false,
                domain_cap: None,
            },
            vec![input],
        );
        resolve_column_bindings(&mut plan).map_err(|_| Unsupported)?;
        let rows = run_plan(plan).map_err(|_| Unsupported)?;
        Ok((0..aggregates.len()).map(|column| rows.get_value(column, 0)).collect())
    }

    /// `PushDownLimit`: LIMIT holds per domain value, which takes a row number per domain value.
    fn push_limit(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        let K::Limit { limit, offset, .. } = plan.kind else {
            unreachable!()
        };
        let mut child = plan.children.remove(0);
        let mut orders = Vec::new();
        if let K::Order { orders: child_orders } = &mut child.kind {
            orders = std::mem::take(child_orders);
            child = child.children.remove(0);
        }
        let state = self.push(&mut child)?;
        let mut sort = LogicalOperator::new(K::Order { orders }, Vec::new());
        self.rewrite(&mut sort, &state);
        let K::Order { orders } = sort.kind else { unreachable!() };
        let window_index = take_index(self.next_table_index);
        let window = BoundWindow {
            function: WindowFunction::RowNumber,
            aggregate: None,
            return_type: LogicalType::BIGINT,
            children: Vec::new(),
            filter: None,
            distinct: false,
            ignore_nulls: false,
            partitions: self.state_refs(&state),
            orders: orders.into_iter().map(|o| (o.expr, o.modifiers)).collect(),
            arg_orders: Vec::new(),
            start: WindowBoundary::UnboundedPreceding,
            end: WindowBoundary::CurrentRowRows,
            start_expr: None,
            end_expr: None,
            exclude: WindowExclude::NoOther,
        };
        let row_number = BoundExpr::column_ref(ColumnBinding::new(window_index, 0), LogicalType::BIGINT, "");
        let compare = |op: ComparisonOp, bound: u64| {
            let bound = i64::try_from(bound).unwrap_or(i64::MAX);
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: Box::new(row_number.clone()),
                    right: Box::new(BoundExpr::constant(Value::bigint(bound))),
                },
                LogicalType::BOOLEAN,
            )
        };
        let mut expressions = Vec::new();
        if let Some(limit) = limit {
            expressions.push(compare(ComparisonOp::LessThanOrEqual, limit.saturating_add(offset)));
        }
        if offset > 0 {
            expressions.push(compare(ComparisonOp::GreaterThan, offset));
        }
        let window = LogicalOperator::new(
            K::Window {
                table_index: window_index,
                windows: vec![window],
            },
            vec![child],
        );
        *plan = if expressions.is_empty() {
            window
        } else {
            LogicalOperator::new(K::Filter { expressions }, vec![window])
        };
        Ok(state)
    }

    /// `PushDownSetOperation`: its children line up by position, with the domain values last.
    fn push_set_operation(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        let domain_types: Vec<LogicalType> = self.domain.iter().map(|(_, ty)| ty.clone()).collect();
        for child in &mut plan.children {
            let bindings = child.column_bindings();
            let types = child.types();
            let state = self.push(child)?;
            let mut expressions: Vec<BoundExpr> = bindings
                .into_iter()
                .zip(types)
                .map(|(binding, ty)| BoundExpr::column_ref(binding, ty, ""))
                .collect();
            expressions.extend(self.state_refs(&state));
            let table_index = take_index(self.next_table_index);
            let input = std::mem::replace(child, LogicalOperator::new(K::CrossProduct, Vec::new()));
            *child = LogicalOperator::new(
                K::Projection {
                    table_index,
                    expressions,
                },
                vec![input],
            );
        }
        let (K::Union { table_index, types } | K::ExceptIntersect { table_index, types, .. }) = &mut plan.kind else {
            unreachable!()
        };
        let offset = types.len();
        types.extend(domain_types);
        Ok(numbered(*table_index, offset, self.domain.len()))
    }

    /// `AddCorrelatedJoinConditions`: both sides have the domain, so they join on it.
    fn state_conditions(&self, left: &[ColumnBinding], right: &[ColumnBinding]) -> Vec<JoinCondition> {
        self.state_refs(left)
            .into_iter()
            .zip(self.state_refs(right))
            .map(|(left, right)| JoinCondition {
                left,
                right,
                comparison: ComparisonOp::NotDistinctFrom,
                filter: None,
            })
            .collect()
    }

    /// `PushDownCrossProduct`.
    fn push_cross_product(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        let left_reads = self.reads_domain(&mut plan.children[0]);
        let right_reads = self.reads_domain(&mut plan.children[1]);
        if !right_reads {
            return self.push(&mut plan.children[0]);
        }
        if !left_reads {
            return self.push(&mut plan.children[1]);
        }
        let left_state = self.push(&mut plan.children[0])?;
        let right_state = self.push(&mut plan.children[1])?;
        plan.kind = K::Join {
            join_type: JoinType::Inner,
            conditions: self.state_conditions(&left_state, &right_state),
            predicate: None,
            algorithm: Default::default(),
        };
        Ok(left_state)
    }

    /// `PushDownJoin`.
    fn push_join(&mut self, plan: &mut LogicalOperator) -> Flat<Vec<ColumnBinding>> {
        // a condition reads one side, and the domain may be on the other one: the predicate sees both
        let K::Join {
            join_type,
            conditions,
            predicate,
            ..
        } = &mut plan.kind
        else {
            unreachable!()
        };
        let join_type = *join_type;
        let (moved, kept): (Vec<_>, Vec<_>) = std::mem::take(conditions)
            .into_iter()
            .partition(|c| self.expression_reads_domain(&c.left) || self.expression_reads_domain(&c.right));
        *conditions = kept;
        if !moved.is_empty() {
            let mut conjuncts: Vec<BoundExpr> = predicate.take().into_iter().collect();
            conjuncts.extend(moved.into_iter().map(|condition| {
                let op = if condition.null_safe() {
                    ComparisonOp::NotDistinctFrom
                } else {
                    ComparisonOp::Equal
                };
                BoundExpr::new(
                    BoundExprKind::Comparison {
                        op,
                        left: Box::new(condition.left),
                        right: Box::new(condition.right),
                    },
                    LogicalType::BOOLEAN,
                )
            }));
            *predicate = Some(conjunction_of(conjuncts));
        }
        let left_reads = self.reads_domain(&mut plan.children[0]);
        let right_reads = self.reads_domain(&mut plan.children[1]);
        let state = match join_type {
            JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti if !right_reads => {
                self.push(&mut plan.children[0])?
            }
            JoinType::Inner | JoinType::Right if !left_reads => self.push(&mut plan.children[1])?,
            JoinType::Inner | JoinType::Left | JoinType::Right => {
                let left_state = self.push(&mut plan.children[0])?;
                let right_state = self.push(&mut plan.children[1])?;
                let added = self.state_conditions(&left_state, &right_state);
                let K::Join { conditions, .. } = &mut plan.kind else {
                    unreachable!()
                };
                conditions.extend(added);
                if join_type == JoinType::Right {
                    right_state
                } else {
                    left_state
                }
            }
            _ => return Err(Unsupported),
        };
        self.rewrite(plan, &state);
        Ok(state)
    }

    /// The column `binding`, or what stands in for it if it is an aggregate of no rows.
    fn empty_value_of(&self, binding: ColumnBinding, ty: &LogicalType) -> BoundExpr {
        let mut expr = BoundExpr::column_ref(binding, ty.clone(), "");
        self.replace_empty_values(&mut expr);
        expr
    }

    /// `RewriteCorrelatedAggregates`.
    fn apply_empty_values(&self, plan: &mut LogicalOperator) {
        if self.empty_values.is_empty() {
            return;
        }
        for child in &mut plan.children {
            self.apply_empty_values(child);
        }
        for expr in plan.expressions_mut() {
            self.replace_empty_values(expr);
        }
        if let K::Join { conditions, .. } | K::DelimJoin { conditions, .. } = &mut plan.kind {
            for condition in conditions {
                self.replace_empty_values(&mut condition.left);
                self.replace_empty_values(&mut condition.right);
            }
        }
    }

    fn replace_empty_values(&self, expr: &mut BoundExpr) {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
            if let Some(empty) = self.empty_values.iter().find(|e| e.aggregate == *binding) {
                let marker = BoundExpr::column_ref(empty.marker, LogicalType::BIGINT, "");
                let is_null = BoundExpr::new(
                    BoundExprKind::Operator {
                        op: BoundOperator::IsNull,
                        children: vec![marker],
                    },
                    LogicalType::BOOLEAN,
                );
                let value = BoundExpr::constant(empty.value.clone());
                let ty = expr.return_type.clone();
                let alias = expr.alias.clone();
                let column = expr.clone();
                *expr = BoundExpr::new(
                    BoundExprKind::Case {
                        checks: vec![(is_null, value)],
                        else_expr: Box::new(column),
                    },
                    ty,
                )
                .with_alias(alias);
            }
            return;
        }
        for child in expr.children_mut() {
            self.replace_empty_values(child);
        }
    }
}
