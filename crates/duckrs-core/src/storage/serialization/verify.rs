// Ported from duckdb/src/parser/{parsed_expression,tableref,query_node}.cpp (`VerifyDeserialized`),
// duckdb/src/parser/parsed_data/sample_options.cpp (`Verify`) and the checks of
// duckdb/src/parser/expression/{cast,function}_expression.cpp @ rust-port-baseline
//
//! What C++ checks of a statement it deserialized before it uses it: no missing children, and
//! no values that the parser couldn't have made.

use super::custom::{CastExpression, FunctionExpression, LambdaExpression, WindowExpression};
use super::generated::{self as g, enums};
use duckrs_common::serializer::EnumNames;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};

fn serialization_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Serialization, message.into())
}

/// `EnumUtil::ToString` of a value of `N`.
fn name_of<N: EnumNames>(value: u64) -> String {
    N::NAMES
        .iter()
        .find(|(_, v)| *v == value)
        .map_or_else(|| value.to_string(), |(name, _)| (*name).to_string())
}

type Child = Option<Box<g::ParsedExpression>>;

/// `SelectStatement` as `DeserializeSelectStatement` takes it.
pub fn select_statement(statement: &g::SelectStatement) -> Result<()> {
    query_node(statement.node.as_deref())
}

/// `QueryNode::VerifyDeserialized`, and of all that is in it.
fn query_node(node: Option<&g::QueryNode>) -> Result<()> {
    let invalid = |reason: &str| serialization_error(format!("Failed to deserialize query node: {reason}"));
    let Some(node) = node else {
        return Err(invalid("missing query node"));
    };
    for modifier in &node.modifiers {
        let Some(modifier) = modifier else {
            return Err(invalid("missing result modifier"));
        };
        match &modifier.kind {
            g::ResultModifierKind::OrderModifier(order) => {
                for order in &order.orders {
                    let Some(expression) = &order.expression else {
                        return Err(invalid("missing ORDER BY expression"));
                    };
                    expression_tree(expression)?;
                }
            }
            g::ResultModifierKind::DistinctModifier(distinct) => {
                for target in &distinct.distinct_on_targets {
                    let Some(target) = target else {
                        return Err(invalid("missing DISTINCT ON expression"));
                    };
                    expression_tree(target)?;
                }
            }
            g::ResultModifierKind::LimitModifier(limit) => {
                optional(&limit.limit)?;
                optional(&limit.offset)?;
            }
            g::ResultModifierKind::LegacyLimitPercentModifier(_) => {}
        }
    }
    for (_, cte) in &node.cte_map.map {
        let Some(cte) = cte else {
            return Err(invalid("missing CTE query"));
        };
        match (&cte.query_node, &cte.query) {
            (Some(query), _) => query_node(Some(query))?,
            (None, Some(query)) => query_node(query.node.as_deref())?,
            (None, None) => return Err(invalid("missing CTE query")),
        }
    }
    match &node.kind {
        g::QueryNodeKind::SelectNode(select) => {
            if select.select_list.is_empty() {
                return Err(invalid("empty select list"));
            }
            for expression in &select.select_list {
                let Some(expression) = expression else {
                    return Err(invalid("missing select list expression"));
                };
                expression_tree(expression)?;
            }
            for expression in &select.group_expressions {
                let Some(expression) = expression else {
                    return Err(invalid("missing group expression"));
                };
                expression_tree(expression)?;
            }
            if let Some(from) = &select.from_table {
                table_ref(from)?;
            }
            optional(&select.where_clause)?;
            optional(&select.having)?;
            optional(&select.qualify)?;
            if let Some(sample) = &select.sample {
                sample_options(sample)?;
            }
        }
        // `SetOperationNode`'s constructor
        g::QueryNodeKind::SetOperationNode(set) => {
            let both_or_children =
                || serialization_error("Error deserializing SetOperationNode - left/right or children must be set");
            let children: Vec<&Option<Box<g::QueryNode>>> = if set.children.is_empty() {
                if set.left.is_none() || set.right.is_none() {
                    return Err(both_or_children());
                }
                vec![&set.left, &set.right]
            } else {
                if set.left.is_some() || set.right.is_some() {
                    return Err(both_or_children());
                }
                set.children.iter().collect()
            };
            use enums::set_operation_type::{NONE, UNION, UNION_BY_NAME};
            if children.len() < 2 {
                return Err(serialization_error("SetOperationNode must have at least two children"));
            }
            if children.len() > 2 && set.setop_type != UNION && set.setop_type != UNION_BY_NAME {
                return Err(serialization_error(
                    "SetOperationNode must have exactly two children, except for UNION",
                ));
            }
            if set.setop_type == NONE {
                return Err(serialization_error("SetOperationNode must have a set operation type"));
            }
            for child in children {
                let Some(child) = child else {
                    return Err(serialization_error("SetOperationNode children cannot be NULL"));
                };
                query_node(Some(child))?;
            }
        }
        g::QueryNodeKind::RecursiveCTENode(cte) => {
            let (Some(left), Some(right)) = (&cte.left, &cte.right) else {
                return Err(invalid("missing recursive CTE child"));
            };
            query_node(Some(left))?;
            query_node(Some(right))?;
        }
        g::QueryNodeKind::CTENode(cte) => {
            if let Some(query) = &cte.query {
                query_node(Some(query))?;
            }
            if let Some(child) = &cte.child {
                query_node(Some(child))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `TableRef::VerifyDeserialized`, and of all that is in it.
fn table_ref(table: &g::TableRef) -> Result<()> {
    let kind = name_of::<enums::table_reference_type::Names>(table.type_);
    let invalid = |reason: &str| serialization_error(format!("Failed to deserialize {kind} table reference: {reason}"));
    if let Some(sample) = &table.sample {
        sample_options(sample)?;
    }
    match &table.kind {
        g::TableRefKind::JoinRef(join) => {
            let (Some(left), Some(right)) = (&join.left, &join.right) else {
                return Err(invalid("missing join side"));
            };
            if join.ref_type == enums::join_ref_type::REGULAR
                && join.condition.is_none()
                && join.using_columns.is_empty()
            {
                return Err(invalid("a regular join requires a condition or USING columns"));
            }
            table_ref(left)?;
            table_ref(right)?;
            optional(&join.condition)?;
        }
        g::TableRefKind::SubqueryRef(subquery) => {
            let Some(node) = subquery.subquery.as_ref().and_then(|s| s.node.as_deref()) else {
                return Err(invalid("missing subquery"));
            };
            query_node(Some(node))?;
        }
        g::TableRefKind::PivotRef(pivot) => {
            let Some(source) = &pivot.source else {
                return Err(invalid("missing source"));
            };
            if pivot.pivots.is_empty() {
                return Err(invalid("no PIVOT or UNPIVOT columns"));
            }
            table_ref(source)?;
        }
        g::TableRefKind::BaseTableRef(base) => {
            if let Some(at) = &base.at_clause {
                let Some(expression) = &at.expr else {
                    return Err(invalid("missing AT expression"));
                };
                expression_tree(expression)?;
            }
        }
        g::TableRefKind::TableFunctionRef(function) => {
            let Some(expression) = function
                .function
                .as_ref()
                .filter(|f| f.class == enums::expression_class::FUNCTION)
            else {
                return Err(invalid("the table function must be a function expression"));
            };
            expression_tree(expression)?;
        }
        g::TableRefKind::ExpressionListRef(list) => {
            for value in list.values.iter().flatten() {
                let Some(value) = value else {
                    return Err(invalid("missing value"));
                };
                expression_tree(value)?;
            }
        }
        g::TableRefKind::EmptyTableRef(_) => {}
    }
    Ok(())
}

/// `SampleOptions::Verify`.
fn sample_options(sample: &g::SampleOptions) -> Result<()> {
    let size = &sample.sample_size;
    if size.is_null() {
        return Err(serialization_error("Sample size cannot be NULL"));
    }
    const MAX_SAMPLE_ROWS: i64 = 1_000_000_000_000_000_000;
    if sample.is_percentage {
        let Some(percentage) = number(size) else {
            return Err(serialization_error("Sample size must be a number"));
        };
        if !(0.0..=100.0).contains(&percentage) {
            return Err(serialization_error(format!(
                "Sample percentage {percentage:.6} out of range, must be between 0 and 100"
            )));
        }
    } else {
        let Some(rows) = number(size).filter(|n| n.is_finite()).map(|n| n.round() as i128) else {
            return Err(serialization_error("Sample size must be a number"));
        };
        if rows < 0 || rows > i128::from(MAX_SAMPLE_ROWS) {
            return Err(serialization_error(format!(
                "Sample rows {rows} out of range, must be between 0 and {MAX_SAMPLE_ROWS}"
            )));
        }
    }
    Ok(())
}

/// A sample size as a number, as `DefaultTryCastAs` makes it one.
fn number(value: &Value) -> Option<f64> {
    let scale = value.logical_type().decimal_properties().map_or(0, |(_, scale)| scale);
    let integer = |n: f64| n / 10f64.powi(i32::from(scale));
    Some(match value.data() {
        ValueData::Int8(x) => integer(f64::from(*x)),
        ValueData::Int16(x) => integer(f64::from(*x)),
        ValueData::Int32(x) => integer(f64::from(*x)),
        ValueData::Int64(x) => integer(*x as f64),
        ValueData::Int128(x) => integer(*x as f64),
        ValueData::UInt8(x) => f64::from(*x),
        ValueData::UInt16(x) => f64::from(*x),
        ValueData::UInt32(x) => f64::from(*x),
        ValueData::UInt64(x) => *x as f64,
        ValueData::UInt128(x) => *x as f64,
        ValueData::Float(x) => f64::from(*x),
        ValueData::Double(x) => *x,
        ValueData::Bytes(text) => crate::function::cast::try_cast_string_to_double(text)?,
        _ => return None,
    })
}

fn optional(child: &Child) -> Result<()> {
    match child {
        Some(child) => expression_tree(child),
        None => Ok(()),
    }
}

/// `IsComparisonType`.
fn is_comparison(type_: u64) -> bool {
    use enums::expression_type::*;
    matches!(
        type_,
        COMPARE_EQUAL
            | COMPARE_NOTEQUAL
            | COMPARE_LESSTHAN
            | COMPARE_GREATERTHAN
            | COMPARE_LESSTHANOREQUALTO
            | COMPARE_GREATERTHANOREQUALTO
            | COMPARE_DISTINCT_FROM
            | COMPARE_NOT_DISTINCT_FROM
    )
}

/// `IsValidOperator`.
fn is_valid_operator(type_: u64, count: usize) -> bool {
    use enums::expression_type::*;
    match type_ {
        COMPARE_IN | COMPARE_NOT_IN => count >= 2,
        OPERATOR_UNPACK | OPERATOR_TRY | OPERATOR_HAS_ERROR | OPERATOR_NOT | OPERATOR_IS_NULL
        | OPERATOR_IS_NOT_NULL => count == 1,
        GROUPING_FUNCTION | ARRAY_CONSTRUCTOR => true,
        OPERATOR_COALESCE => count >= 1,
        ARRAY_EXTRACT | STRUCT_EXTRACT => count == 2,
        ARRAY_SLICE => count == 3 || count == 4,
        other => is_comparison(other) && count == 2,
    }
}

/// `ParsedExpression::VerifyDeserialized` of an expression and of those in it.
fn expression_tree(expression: &g::ParsedExpression) -> Result<()> {
    use g::ParsedExpressionKind as K;
    let class = name_of::<enums::expression_class::Names>(expression.class);
    let invalid = |reason: String| serialization_error(format!("Failed to deserialize {class} expression: {reason}"));
    let type_name = || name_of::<enums::expression_type::Names>(expression.type_);
    // the children, which must all be there
    let mut children: Vec<&Child> = Vec::new();
    match &expression.kind {
        K::BetweenExpression(e) => children.extend([&e.input, &e.lower, &e.upper]),
        K::CaseExpression(e) => {
            if e.case_checks.is_empty() {
                return Err(invalid("no WHEN clauses".into()));
            }
            for check in &e.case_checks {
                children.extend([&check.when_expr, &check.then_expr]);
            }
            optional(&e.else_expr)?;
        }
        K::CastExpression(CastExpression {
            child,
            cast_type,
            type_expr,
            ..
        }) => {
            match type_expr {
                Some(target) if target.class != enums::expression_class::TYPE => {
                    return Err(serialization_error(
                        "Failed to deserialize CAST expression: the target is not a type expression",
                    ));
                }
                Some(target) => expression_tree(target)?,
                None if cast_type.is_none() => {
                    return Err(serialization_error(
                        "Failed to deserialize CAST expression: missing target type",
                    ));
                }
                None => {}
            }
            children.push(child);
        }
        K::CollateExpression(e) => children.push(&e.child),
        K::ColumnRefExpression(e) => {
            if e.column_names.is_empty() {
                return Err(invalid("no column names".into()));
            }
        }
        K::LambdaRefExpression(_) => return Err(invalid("lambda references cannot be deserialized".into())),
        K::ComparisonExpression(e) => {
            children.extend([&e.left, &e.right]);
            if !is_comparison(expression.type_) {
                present(&children, &invalid)?;
                return Err(invalid(format!("invalid comparison type {}", type_name())));
            }
        }
        K::ConjunctionExpression(e) => {
            children.extend(&e.children);
            present(&children, &invalid)?;
            use enums::expression_type::{CONJUNCTION_AND, CONJUNCTION_OR};
            if expression.type_ != CONJUNCTION_AND && expression.type_ != CONJUNCTION_OR {
                return Err(invalid(format!("invalid conjunction type {}", type_name())));
            }
            if e.children.is_empty() {
                return Err(invalid("no children".into()));
            }
        }
        K::OperatorExpression(e) => {
            children.extend(&e.children);
            present(&children, &invalid)?;
            if !is_valid_operator(expression.type_, e.children.len()) {
                return Err(invalid(format!(
                    "invalid operator type {} for {} children",
                    type_name(),
                    e.children.len()
                )));
            }
        }
        K::FunctionExpression(FunctionExpression {
            arguments,
            children: legacy,
            filter,
            order_bys,
            distinct,
            is_operator,
            ..
        }) => {
            children.extend(arguments.iter().map(|argument| &argument.expression));
            children.extend(legacy);
            present(&children, &invalid)?;
            optional(filter)?;
            match order_bys.as_deref() {
                None => return Err(invalid("missing ORDER BY modifier".into())),
                Some(g::ResultModifier {
                    kind: g::ResultModifierKind::OrderModifier(order),
                    ..
                }) => {
                    for order in &order.orders {
                        let Some(expression) = &order.expression else {
                            return Err(invalid("missing child expression".into()));
                        };
                        expression_tree(expression)?;
                    }
                }
                Some(_) => {
                    return Err(serialization_error(
                        "Failed to deserialize FUNCTION expression: order_bys is not an ORDER BY modifier",
                    ));
                }
            }
            if *is_operator && *distinct {
                return Err(invalid("an operator cannot be DISTINCT".into()));
            }
        }
        K::LambdaExpression(LambdaExpression { lhs, expr, .. }) => children.extend([lhs, expr]),
        K::StarExpression(e) => {
            optional(&e.expr)?;
            for (_, replace) in &e.replace_list {
                children.push(replace);
            }
        }
        K::SubqueryExpression(e) => {
            let Some(node) = e.subquery.as_ref().and_then(|s| s.node.as_deref()) else {
                return Err(invalid("missing subquery".into()));
            };
            optional(&e.child)?;
            use enums::subquery_type::{ANY, INVALID};
            if (e.subquery_type == ANY) != e.child.is_some() {
                return Err(invalid("a child is required for, and only for, ANY subqueries".into()));
            }
            if e.subquery_type == INVALID {
                return Err(invalid("invalid subquery type".into()));
            }
            query_node(Some(node))?;
        }
        K::WindowExpression(WindowExpression {
            arguments,
            children: legacy,
            partitions,
            orders,
            start,
            end,
            start_expr,
            end_expr,
            offset_expr,
            default_expr,
            filter_expr,
            ..
        }) => {
            children.extend(arguments.iter().map(|argument| &argument.expression));
            children.extend(legacy);
            children.extend(partitions);
            present(&children, &invalid)?;
            for order in orders {
                let Some(expression) = &order.expression else {
                    return Err(invalid("missing child expression".into()));
                };
                expression_tree(expression)?;
            }
            for child in [start_expr, end_expr, offset_expr, default_expr, filter_expr] {
                optional(child)?;
            }
            use enums::window_boundary::*;
            let needs_expression = |boundary: u64| {
                matches!(
                    boundary,
                    EXPR_PRECEDING_ROWS
                        | EXPR_PRECEDING_RANGE
                        | EXPR_PRECEDING_GROUPS
                        | EXPR_FOLLOWING_ROWS
                        | EXPR_FOLLOWING_RANGE
                        | EXPR_FOLLOWING_GROUPS
                )
            };
            if *start == INVALID || *start == UNBOUNDED_FOLLOWING {
                return Err(invalid("invalid frame start boundary".into()));
            }
            if *end == INVALID {
                return Err(invalid("invalid frame end boundary".into()));
            }
            if needs_expression(*start) && start_expr.is_none() {
                return Err(invalid("missing frame start expression".into()));
            }
            if needs_expression(*end) && end_expr.is_none() {
                return Err(invalid("missing frame end expression".into()));
            }
        }
        K::TypeExpression(e) => children.extend(&e.children),
        K::ConstantExpression(constant) => {
            if constant
                .literal
                .as_ref()
                .is_some_and(|literal| literal.kind == enums::literal_kind::POINTER)
            {
                return Err(serialization_error("Deserialization of POINTER is not allowed"));
            }
        }
        K::DefaultExpression(_) | K::ParameterExpression(_) | K::PositionalReferenceExpression(_) => {}
    }
    check_children(&children, &invalid)
}

/// That every child is there.
fn present(children: &[&Child], invalid: &dyn Fn(String) -> Error) -> Result<()> {
    if children.iter().any(|child| child.is_none()) {
        return Err(invalid("missing child expression".into()));
    }
    Ok(())
}

/// That every child is there, and is valid itself.
fn check_children(children: &[&Child], invalid: &dyn Fn(String) -> Error) -> Result<()> {
    for child in children {
        let Some(child) = child else {
            return Err(invalid("missing child expression".into()));
        };
        expression_tree(child)?;
    }
    Ok(())
}
