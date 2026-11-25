//! Turns the objects of a database file into the AST. C++ deserializes into the classes of its
//! parser, so what the transformer makes of a statement and what this makes of its stored form
//! have to be the same tree; `crates/duckrs/tests/read_database_file.rs` checks that.

use super::custom::{CastExpression, FunctionExpression, QualifiedName, WindowExpression};
use super::generated::{self as g, enums};
use super::not_ported;
use crate::storage::block_manager::corrupt;
use duckrs_common::Result;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{self, CastType, ComparisonOp, Expr, ExprKind, Literal, LiteralKind, OperatorType};

type Child = Option<Box<g::ParsedExpression>>;

fn required<T>(value: Option<Box<T>>, what: &str) -> Result<T> {
    value.map(|v| *v).ok_or_else(|| corrupt(format!("{what} is missing")))
}

fn required_expr(value: Child, what: &str) -> Result<Expr> {
    expression(required(value, what)?)
}

fn boxed_expr(value: Child, what: &str) -> Result<Box<Expr>> {
    required_expr(value, what).map(Box::new)
}

fn optional(value: Child) -> Result<Option<Box<Expr>>> {
    value.map(|e| expression(*e).map(Box::new)).transpose()
}

fn expressions(list: Vec<Child>) -> Result<Vec<Expr>> {
    list.into_iter().map(|e| required_expr(e, "an expression")).collect()
}

/// `QualifiedName`: the path, or the separate fields of the files that don't have one.
pub fn qualified_name(
    catalog: String,
    schema: String,
    name: String,
    path: Option<QualifiedName>,
) -> ast::QualifiedName {
    if let Some(mut path) = path.map(|p| p.path).filter(|p| !p.is_empty()) {
        let name = path.pop().expect("not empty");
        path.retain(|component| !component.is_empty());
        return ast::QualifiedName {
            qualification: path,
            name,
        };
    }
    let qualification = [catalog, schema].into_iter().filter(|c| !c.is_empty()).collect();
    ast::QualifiedName { qualification, name }
}

fn comparison(expression_type: u64) -> Result<ComparisonOp> {
    use enums::expression_type::*;
    Ok(match expression_type {
        COMPARE_EQUAL => ComparisonOp::Equal,
        COMPARE_NOTEQUAL => ComparisonOp::NotEqual,
        COMPARE_LESSTHAN => ComparisonOp::LessThan,
        COMPARE_GREATERTHAN => ComparisonOp::GreaterThan,
        COMPARE_LESSTHANOREQUALTO => ComparisonOp::LessThanOrEqual,
        COMPARE_GREATERTHANOREQUALTO => ComparisonOp::GreaterThanOrEqual,
        COMPARE_DISTINCT_FROM => ComparisonOp::DistinctFrom,
        COMPARE_NOT_DISTINCT_FROM => ComparisonOp::NotDistinctFrom,
        other => return Err(corrupt(format!("comparison of type {other}"))),
    })
}

fn operator(expression_type: u64) -> Result<OperatorType> {
    use enums::expression_type::*;
    Ok(match expression_type {
        OPERATOR_NOT => OperatorType::Not,
        OPERATOR_IS_NULL => OperatorType::IsNull,
        OPERATOR_IS_NOT_NULL => OperatorType::IsNotNull,
        COMPARE_IN => OperatorType::In,
        COMPARE_NOT_IN => OperatorType::NotIn,
        OPERATOR_COALESCE => OperatorType::Coalesce,
        OPERATOR_UNPACK => OperatorType::Unpack,
        OPERATOR_TRY => OperatorType::Try,
        ARRAY_EXTRACT => OperatorType::ArrayExtract,
        ARRAY_SLICE => OperatorType::ArraySlice,
        STRUCT_EXTRACT => OperatorType::StructExtract,
        ARRAY_CONSTRUCTOR => OperatorType::ArrayConstructor,
        GROUPING_FUNCTION => OperatorType::Grouping,
        other => return Err(corrupt(format!("operator of type {other}"))),
    })
}

fn literal(stored: g::Literal) -> Result<Literal> {
    use enums::literal_kind::*;
    let kind = match stored.kind {
        NULL_LITERAL => LiteralKind::Null,
        BOOLEAN => LiteralKind::Boolean,
        INTEGER => LiteralKind::Integer,
        NUMERIC => LiteralKind::Numeric,
        STRING => LiteralKind::String,
        HEX => LiteralKind::Hex,
        BIT => LiteralKind::Bit,
        other => return Err(not_ported(&format!("a literal of kind {other}"))),
    };
    Ok(Literal {
        kind,
        text: stored.text,
    })
}

/// The literal that reads back as this value with the type it has, if there is one.
fn value_literal(value: &Value) -> Result<Option<Literal>> {
    let id = value.logical_type().id();
    if value.is_null() {
        return Ok((id == LogicalTypeId::SqlNull).then(Literal::null));
    }
    let text = value.try_to_string()?;
    Ok(match id {
        LogicalTypeId::Boolean => Some(Literal::boolean(text == "true")),
        LogicalTypeId::Integer | LogicalTypeId::Decimal => Some(Literal::number(text)),
        LogicalTypeId::Varchar => Some(Literal::string(text)),
        _ => None,
    })
}

/// `ConstantExpression::FromValue`: files from before v2.0.0 have the value and not the text. A
/// value that no literal stands for is a cast of its text.
fn constant(value: Value) -> Result<Expr> {
    if let Some(literal) = value_literal(&value)? {
        return Ok(Expr::constant(literal));
    }
    let ty = value.logical_type().clone();
    if value.is_null() {
        return Ok(Expr::cast(Expr::constant(Literal::null()), CastType::Resolved(ty)));
    }
    // `ConstantExpression::FromValue`: a number is a literal, cast when it would not read back as
    // the value (`LiteralOrCast`); an infinite one is a number too big for any
    let number = match value.data() {
        duckrs_common::value::ValueData::Double(x) if x.is_infinite() => {
            Some(if *x < 0.0 { "-1e1000" } else { "1e1000" }.to_string())
        }
        duckrs_common::value::ValueData::Float(x) if x.is_infinite() => {
            Some(if *x < 0.0 { "-1e1000" } else { "1e1000" }.to_string())
        }
        duckrs_common::value::ValueData::Double(x) if x.is_nan() || (*x == 0.0 && x.is_sign_negative()) => None,
        duckrs_common::value::ValueData::Float(x) if x.is_nan() || (*x == 0.0 && x.is_sign_negative()) => None,
        _ if matches!(
            ty.id(),
            LogicalTypeId::TinyInt
                | LogicalTypeId::SmallInt
                | LogicalTypeId::BigInt
                | LogicalTypeId::HugeInt
                | LogicalTypeId::UTinyInt
                | LogicalTypeId::USmallInt
                | LogicalTypeId::UInteger
                | LogicalTypeId::UBigInt
                | LogicalTypeId::UHugeInt
                | LogicalTypeId::Float
                | LogicalTypeId::Double
        ) =>
        {
            Some(value.try_to_string()?)
        }
        _ => None,
    };
    if let Some(text) = number {
        let literal = Literal::number(text);
        // `RoundTrips`: the literal binds to this very value
        let round_trips = crate::planner::binder::literal_value(&literal)
            .is_ok_and(|bound| bound.logical_type() == &ty && bound == value);
        if round_trips {
            return Ok(Expr::constant(literal));
        }
        return Ok(Expr::cast(Expr::constant(literal), CastType::Resolved(ty)));
    }
    // `StringCast`: the text of the value, cast
    Ok(Expr::cast(
        Expr::constant(Literal::string(value.try_to_string()?)),
        CastType::Resolved(ty),
    ))
}

/// `Identifier`s of a STAR's EXCLUDE and RENAME lists.
fn column_name(stored: g::QualifiedColumnName) -> ast::QualifiedColumnName {
    ast::QualifiedColumnName {
        qualification: [stored.catalog, stored.schema, stored.table]
            .into_iter()
            .filter(|c| !c.is_empty())
            .collect(),
        column: stored.column,
    }
}

fn star(stored: g::StarExpression) -> Result<Expr> {
    let mut exclude_list: Vec<_> = stored
        .exclude_list
        .into_iter()
        .map(|column| ast::QualifiedColumnName {
            qualification: Vec::new(),
            column,
        })
        .collect();
    exclude_list.extend(stored.qualified_exclude_list.into_iter().flatten().map(column_name));
    let replace_list = stored
        .replace_list
        .into_iter()
        .map(|(name, e)| Ok((name, required_expr(e, "a REPLACE expression")?)))
        .collect::<Result<_>>()?;
    let rename_list = stored
        .rename_list
        .into_iter()
        .map(|(column, name)| (column_name(column), name))
        .collect();
    let result: Expr = ExprKind::Star(ast::StarExpr {
        relation_name: stored.relation_name,
        exclude_list,
        replace_list,
        rename_list,
        expr: optional(stored.expr)?,
        columns: stored.columns,
    })
    .into();
    Ok(if stored.unpacked.unwrap_or(false) {
        Expr::operator(OperatorType::Unpack, vec![result])
    } else {
        result
    })
}

fn order_by(stored: g::OrderByNode) -> Result<ast::OrderByNode> {
    use enums::{order_by_null_type, order_type};
    Ok(ast::OrderByNode {
        order: match stored.type_ {
            order_type::ASCENDING => ast::OrderType::Ascending,
            order_type::DESCENDING => ast::OrderType::Descending,
            _ => ast::OrderType::Default,
        },
        nulls: match stored.null_order {
            order_by_null_type::NULLS_FIRST => ast::NullOrder::NullsFirst,
            order_by_null_type::NULLS_LAST => ast::NullOrder::NullsLast,
            _ => ast::NullOrder::Default,
        },
        expr: required_expr(stored.expression, "an ORDER BY expression")?,
    })
}

fn order_bys(list: Vec<g::OrderByNode>) -> Result<Vec<ast::OrderByNode>> {
    list.into_iter().map(order_by).collect()
}

/// Arguments: those of v2.0.0, or the children before it, whose aliases are their names.
fn arguments(children: Vec<Child>, stored: Vec<g::FunctionArgument>) -> Result<Vec<ast::FunctionArgument>> {
    if !children.is_empty() {
        return expressions(children)?
            .into_iter()
            .map(|expr| {
                Ok(ast::FunctionArgument {
                    name: expr.alias.clone(),
                    expr,
                })
            })
            .collect();
    }
    stored
        .into_iter()
        .map(|argument| {
            Ok(ast::FunctionArgument {
                name: argument.name,
                expr: required_expr(argument.expression, "a function argument")?,
            })
        })
        .collect()
}

fn function(stored: FunctionExpression) -> Result<Expr> {
    let order_by = match stored.order_bys.map(|m| m.kind) {
        None => Vec::new(),
        Some(g::ResultModifierKind::OrderModifier(modifier)) => order_bys(modifier.orders)?,
        Some(_) => return Err(corrupt("the ORDER BY of a function is another kind of modifier")),
    };
    Ok(ast::FunctionExpr {
        name: qualified_name(
            stored.catalog,
            stored.schema,
            stored.function_name,
            stored.qualified_name,
        ),
        args: arguments(stored.children, stored.arguments)?,
        filter: optional(stored.filter)?,
        order_by,
        distinct: stored.distinct,
        is_operator: stored.is_operator,
        export_state: stored.export_state,
    }
    .into())
}

fn window_boundary(value: u64) -> Result<ast::WindowBoundary> {
    use ast::WindowBoundary as B;
    use enums::window_boundary::*;
    Ok(match value {
        INVALID => B::Invalid,
        UNBOUNDED_PRECEDING => B::UnboundedPreceding,
        UNBOUNDED_FOLLOWING => B::UnboundedFollowing,
        CURRENT_ROW_RANGE => B::CurrentRowRange,
        CURRENT_ROW_ROWS => B::CurrentRowRows,
        CURRENT_ROW_GROUPS => B::CurrentRowGroups,
        EXPR_PRECEDING_ROWS => B::ExprPrecedingRows,
        EXPR_PRECEDING_RANGE => B::ExprPrecedingRange,
        EXPR_PRECEDING_GROUPS => B::ExprPrecedingGroups,
        EXPR_FOLLOWING_ROWS => B::ExprFollowingRows,
        EXPR_FOLLOWING_RANGE => B::ExprFollowingRange,
        EXPR_FOLLOWING_GROUPS => B::ExprFollowingGroups,
        other => return Err(corrupt(format!("window boundary {other}"))),
    })
}

fn window(stored: WindowExpression) -> Result<Expr> {
    use enums::window_exclude_mode::*;
    let mut args = arguments(stored.children, stored.arguments)?;
    for extra in [stored.offset_expr, stored.default_expr].into_iter().flatten() {
        args.push(ast::FunctionArgument::positional(expression(*extra)?));
    }
    let exclude = match stored.exclude_clause.unwrap_or(NO_OTHER) {
        NO_OTHER => ast::WindowExclude::NoOther,
        CURRENT_ROW => ast::WindowExclude::CurrentRow,
        GROUP => ast::WindowExclude::Group,
        TIES => ast::WindowExclude::Ties,
        other => return Err(corrupt(format!("window exclusion {other}"))),
    };
    Ok(ExprKind::Window(Box::new(ast::WindowExpr {
        name: qualified_name(
            stored.catalog,
            stored.schema,
            stored.function_name,
            stored.qualified_name,
        ),
        args,
        partitions: expressions(stored.partitions)?,
        orders: order_bys(stored.orders)?,
        arg_orders: order_bys(stored.arg_orders)?,
        start: window_boundary(stored.start)?,
        end: window_boundary(stored.end)?,
        start_expr: optional(stored.start_expr)?,
        end_expr: optional(stored.end_expr)?,
        exclude,
        filter: optional(stored.filter_expr)?,
        distinct: stored.distinct,
        ignore_nulls: stored.ignore_nulls,
        has_ignore_nulls: stored.has_ignore_nulls,
    }))
    .into())
}

fn cast(stored: CastExpression) -> Result<Expr> {
    let target = match (stored.type_expr, stored.cast_type) {
        (Some(ty), _) => CastType::Parsed(Box::new(expression(*ty)?)),
        (None, Some(ty)) => CastType::Resolved(ty),
        (None, None) => return Err(corrupt("a CAST has no target type")),
    };
    Ok(ExprKind::Cast {
        child: boxed_expr(stored.child, "the child of a CAST")?,
        target,
        try_cast: stored.try_cast,
    }
    .into())
}

fn subquery(stored: g::SubqueryExpression) -> Result<Expr> {
    use enums::subquery_type::*;
    let subquery_type = match stored.subquery_type {
        SCALAR => ast::SubqueryType::Scalar,
        EXISTS => ast::SubqueryType::Exists,
        NOT_EXISTS => ast::SubqueryType::NotExists,
        ANY => ast::SubqueryType::Any,
        other => return Err(corrupt(format!("subquery of type {other}"))),
    };
    let comparison = if subquery_type == ast::SubqueryType::Any {
        comparison(stored.comparison_type)?
    } else {
        ComparisonOp::Equal
    };
    Ok(ExprKind::Subquery {
        subquery: Box::new(select_statement(stored.subquery)?),
        subquery_type,
        child: optional(stored.child)?,
        comparison,
    }
    .into())
}

/// `OperatorExpression::Deserialize`: an open end of a slice used to be an empty list.
fn operator_expression(expression_type: u64, children: Vec<Child>) -> Result<Expr> {
    let slice = expression_type == enums::expression_type::ARRAY_SLICE;
    let children = children
        .into_iter()
        .enumerate()
        .map(|(i, child)| {
            let child = required(child, "an operator child")?;
            if slice
                && matches!(i, 1 | 2)
                && let g::ParsedExpressionKind::ConstantExpression(g::ConstantExpression { value: Some(value), .. }) =
                    &child.kind
                && value.logical_type().id() == LogicalTypeId::List
                && value.try_to_string()? == "[]"
            {
                return Ok(ast::FunctionExpr::new("list_value", Vec::new()).into());
            }
            expression(child)
        })
        .collect::<Result<_>>()?;
    Ok(Expr::operator(operator(expression_type)?, children))
}

/// `ParsedExpression::Deserialize`.
pub fn expression(stored: g::ParsedExpression) -> Result<Expr> {
    use g::ParsedExpressionKind as K;
    let expression_type = stored.type_;
    let mut result: Expr = match stored.kind {
        K::BetweenExpression(e) => ExprKind::Between {
            input: boxed_expr(e.input, "BETWEEN input")?,
            lower: boxed_expr(e.lower, "BETWEEN lower bound")?,
            upper: boxed_expr(e.upper, "BETWEEN upper bound")?,
        }
        .into(),
        K::CaseExpression(e) => ExprKind::Case {
            checks: e
                .case_checks
                .into_iter()
                .map(|check| {
                    Ok((
                        required_expr(check.when_expr, "a WHEN")?,
                        required_expr(check.then_expr, "a THEN")?,
                    ))
                })
                .collect::<Result<_>>()?,
            else_expr: boxed_expr(e.else_expr, "an ELSE")?,
        }
        .into(),
        K::CastExpression(e) => cast(e)?,
        K::CollateExpression(e) => ExprKind::Collate {
            child: boxed_expr(e.child, "the child of a COLLATE")?,
            collation: e.collation,
        }
        .into(),
        K::ColumnRefExpression(e) => Expr::column(e.column_names),
        K::LambdaRefExpression(_) => return Err(not_ported("a bound lambda parameter")),
        K::ComparisonExpression(e) => Expr::comparison(
            comparison(expression_type)?,
            required_expr(e.left, "the left of a comparison")?,
            required_expr(e.right, "the right of a comparison")?,
        ),
        K::ConjunctionExpression(e) => ExprKind::Conjunction {
            and: expression_type == enums::expression_type::CONJUNCTION_AND,
            children: expressions(e.children)?,
        }
        .into(),
        K::ConstantExpression(e) => match (e.literal, e.value) {
            (Some(stored), _) if stored.kind != enums::literal_kind::INVALID => Expr::constant(literal(stored)?),
            (_, Some(value)) => constant(value)?,
            _ => return Err(corrupt("a constant has no value")),
        },
        K::DefaultExpression(_) => ExprKind::Default.into(),
        K::FunctionExpression(e) => function(e)?,
        K::LambdaExpression(e) => ExprKind::Lambda {
            lhs: boxed_expr(e.lhs, "the parameters of a lambda")?,
            expr: boxed_expr(e.expr, "the body of a lambda")?,
            syntax: match e.syntax_type {
                enums::lambda_syntax_type::LAMBDA_KEYWORD => ast::LambdaSyntax::Keyword,
                enums::lambda_syntax_type::SINGLE_ARROW => ast::LambdaSyntax::Arrow,
                _ => ast::LambdaSyntax::StoredArrow,
            },
        }
        .into(),
        K::OperatorExpression(e) => operator_expression(expression_type, e.children)?,
        K::ParameterExpression(e) => ExprKind::Parameter(e.identifier).into(),
        K::PositionalReferenceExpression(e) => ExprKind::Positional(e.index).into(),
        K::StarExpression(e) => star(e)?,
        K::SubqueryExpression(e) => subquery(e)?,
        K::WindowExpression(e) => window(e)?,
        K::TypeExpression(e) => ExprKind::Type(ast::TypeExpr {
            name: qualified_name(e.catalog, e.schema, e.type_name, e.qualified_name),
            children: expressions(e.children)?,
        })
        .into(),
    };
    result.alias = stored.alias;
    Ok(result)
}

fn sample(stored: g::SampleOptions) -> Result<ast::SampleOptions> {
    use enums::sample_method::*;
    Ok(ast::SampleOptions {
        sample_size: stored.sample_size.try_to_string()?,
        is_percentage: stored.is_percentage,
        method: match stored.method {
            SYSTEM_SAMPLE => ast::SampleMethod::System,
            BERNOULLI_SAMPLE => ast::SampleMethod::Bernoulli,
            RESERVOIR_SAMPLE => ast::SampleMethod::Reservoir,
            other => return Err(corrupt(format!("sample method {other}"))),
        },
        // `SampleOptions::GetSeed`: -1 is no seed
        seed: u64::try_from(stored.seed).ok(),
        repeatable: stored.repeatable,
    })
}

fn result_modifier(stored: g::ResultModifier) -> Result<ast::ResultModifier> {
    use g::ResultModifierKind as K;
    Ok(match stored.kind {
        K::LimitModifier(m) => ast::ResultModifier::Limit {
            limit: optional(m.limit)?,
            offset: optional(m.offset)?,
            percent: m.limit_type == Some(enums::limit_value_type::PERCENTAGE),
        },
        K::LegacyLimitPercentModifier(m) => ast::ResultModifier::Limit {
            limit: optional(m.limit)?,
            offset: optional(m.offset)?,
            percent: true,
        },
        K::DistinctModifier(m) => ast::ResultModifier::Distinct(expressions(m.distinct_on_targets)?),
        K::OrderModifier(m) => ast::ResultModifier::Order(order_bys(m.orders)?),
    })
}

fn cte_map(stored: g::CommonTableExpressionMap) -> Result<ast::CteMap> {
    use enums::cte_materialize::*;
    let mut result = Vec::with_capacity(stored.map.len());
    for (name, info) in stored.map {
        let info = required(info, "a CTE")?;
        if !info.payload_aggregates.is_empty() {
            return Err(not_ported("USING KEY payload aggregates"));
        }
        let query = match (info.query_node, info.query) {
            (Some(node), _) => query_node(*node)?,
            (None, query) => select_statement(query)?.node,
        };
        let materialized = match info.materialized {
            CTE_MATERIALIZE_ALWAYS => ast::CteMaterialize::Always,
            CTE_MATERIALIZE_NEVER => ast::CteMaterialize::Never,
            _ => ast::CteMaterialize::Default,
        };
        result.push((
            name,
            ast::CteInfo {
                aliases: info.aliases,
                query: Box::new(query),
                key_targets: expressions(info.key_targets)?,
                materialized,
            },
        ));
    }
    Ok(ast::CteMap(result))
}

fn select_node(stored: g::SelectNode) -> Result<ast::SelectNode> {
    let groups = ast::GroupByNode {
        group_expressions: expressions(stored.group_expressions)?,
        grouping_sets: stored
            .group_sets
            .into_iter()
            .map(|set| set.into_iter().map(|index| index as usize).collect())
            .collect(),
    };
    let count = groups.group_expressions.len();
    if groups.grouping_sets.iter().flatten().any(|&index| index >= count) {
        return Err(duckrs_common::Error::new(
            duckrs_common::ErrorKind::Serialization,
            "Grouping set refers to a group that does not exist",
        ));
    }
    Ok(ast::SelectNode {
        select_list: expressions(stored.select_list)?,
        from_table: match stored.from_table {
            Some(table) => table_ref(*table)?,
            None => ast::TableRef::empty(),
        },
        where_clause: optional(stored.where_clause)?,
        groups,
        having: optional(stored.having)?,
        qualify: optional(stored.qualify)?,
        force_aggregates: stored.aggregate_handling == enums::aggregate_handling::FORCE_AGGREGATES,
        sample: stored.sample.map(|s| sample(*s)).transpose()?,
    })
}

fn set_operation(stored: g::SetOperationNode) -> Result<ast::SetOperationNode> {
    use enums::set_operation_type::*;
    let setop_type = match stored.setop_type {
        UNION => ast::SetOpType::Union,
        UNION_BY_NAME => ast::SetOpType::UnionByName,
        EXCEPT => ast::SetOpType::Except,
        INTERSECT => ast::SetOpType::Intersect,
        other => return Err(corrupt(format!("set operation {other}"))),
    };
    // before v1.5.0 there are just two sides
    let children = if stored.children.is_empty() {
        vec![stored.left, stored.right]
    } else {
        stored.children
    };
    Ok(ast::SetOperationNode {
        setop_type,
        setop_all: stored.setop_all.unwrap_or(true),
        children: children
            .into_iter()
            .map(|child| query_node(required(child, "a side of a set operation")?))
            .collect::<Result<_>>()?,
    })
}

/// `QueryNode::Deserialize`.
pub fn query_node(stored: g::QueryNode) -> Result<ast::QueryNode> {
    use g::QueryNodeKind as K;
    let kind = match stored.kind {
        // older versions wrap the query in the CTEs that its own map lists as well
        K::CTENode(node) => return query_node(required(node.child, "the query of a CTE")?),
        K::SelectNode(node) => ast::QueryNodeKind::Select(Box::new(select_node(node)?)),
        K::SetOperationNode(node) => ast::QueryNodeKind::SetOperation(set_operation(node)?),
        K::InsertQueryNode(node) => ast::QueryNodeKind::Insert(Box::new(insert_node(node)?)),
        K::UpdateQueryNode(node) => ast::QueryNodeKind::Update(Box::new(ast::UpdateStatement {
            cte_map: ast::CteMap::default(),
            table: table_ref(required(node.table, "the table of an UPDATE")?)?,
            set_info: update_set_info(required(node.set_info, "the SET of an UPDATE")?)?,
            from_table: node.from_table.map(|t| table_ref(*t)).transpose()?,
            returning: expressions(node.returning_list)?,
        })),
        K::DeleteQueryNode(node) => ast::QueryNodeKind::Delete(Box::new(ast::DeleteStatement {
            cte_map: ast::CteMap::default(),
            table: table_ref(required(node.table, "the table of a DELETE")?)?,
            using: node
                .using_clauses
                .into_iter()
                .map(|t| table_ref(required(t, "a USING clause")?))
                .collect::<Result<_>>()?,
            condition: optional(node.condition)?,
            returning: expressions(node.returning_list)?,
        })),
        K::RecursiveCTENode(node) => ast::QueryNodeKind::RecursiveCte(ast::RecursiveCteNode {
            ctename: node.cte_name,
            union_all: node.union_all.unwrap_or(false),
            left: Box::new(query_node(required(node.left, "a recursive CTE")?)?),
            right: Box::new(query_node(required(node.right, "a recursive CTE")?)?),
            aliases: node.aliases,
            key_targets: expressions(node.key_targets)?,
        }),
    };
    Ok(ast::QueryNode {
        kind,
        modifiers: stored
            .modifiers
            .into_iter()
            .map(|m| result_modifier(required(m, "a result modifier")?))
            .collect::<Result<_>>()?,
        cte_map: cte_map(stored.cte_map)?,
    })
}

/// `UpdateSetInfo::Deserialize`.
fn update_set_info(stored: g::UpdateSetInfo) -> Result<ast::UpdateSetInfo> {
    Ok(ast::UpdateSetInfo {
        columns: stored.columns,
        expressions: expressions(stored.expressions)?,
        condition: optional(stored.condition)?,
    })
}

/// `InsertQueryNode::Deserialize`.
fn insert_node(stored: g::InsertQueryNode) -> Result<ast::InsertStatement> {
    let on_conflict = match stored.on_conflict_info {
        Some(conflict) => Some(ast::OnConflictInfo {
            action: match conflict.action_type {
                1 => ast::OnConflictAction::Nothing,
                2 => ast::OnConflictAction::Update,
                3 => ast::OnConflictAction::Replace,
                other => return Err(corrupt(format!("ON CONFLICT action {other}"))),
            },
            indexed_columns: conflict.indexed_columns,
            condition: optional(conflict.condition)?,
            set_info: conflict.set_info.map(|info| update_set_info(*info)).transpose()?,
        }),
        None => None,
    };
    let table_alias = stored
        .table_ref
        .map(|t| t.alias.clone())
        .filter(|alias| !alias.is_empty());
    Ok(ast::InsertStatement {
        cte_map: ast::CteMap::default(),
        table: qualified_name(stored.catalog, stored.schema, stored.table, stored.qualified_name),
        table_alias,
        by_name: stored.column_order == Some(1),
        columns: stored.columns,
        values: match stored.select_statement {
            Some(select) => ast::InsertValues::Select(Box::new(select_statement(Some(select))?)),
            None => ast::InsertValues::DefaultValues,
        },
        on_conflict,
        returning: expressions(stored.returning_list)?,
    })
}

pub fn select_statement(stored: Option<Box<g::SelectStatement>>) -> Result<ast::SelectStatement> {
    let stored = required(stored, "a query")?;
    Ok(ast::SelectStatement {
        node: query_node(required(stored.node, "a query")?)?,
    })
}

fn join_type(value: u64) -> Result<ast::JoinType> {
    use enums::join_type::*;
    Ok(match value {
        INNER => ast::JoinType::Inner,
        LEFT => ast::JoinType::Left,
        RIGHT => ast::JoinType::Right,
        OUTER => ast::JoinType::Outer,
        SEMI => ast::JoinType::Semi,
        ANTI => ast::JoinType::Anti,
        MARK => ast::JoinType::Mark,
        SINGLE => ast::JoinType::Single,
        RIGHT_SEMI => ast::JoinType::RightSemi,
        RIGHT_ANTI => ast::JoinType::RightAnti,
        other => return Err(corrupt(format!("join of type {other}"))),
    })
}

fn join_ref(stored: g::JoinRef) -> Result<ast::JoinRef> {
    use enums::join_ref_type::*;
    let ref_type = match stored.ref_type {
        REGULAR => ast::JoinRefType::Regular,
        NATURAL => ast::JoinRefType::Natural,
        CROSS => ast::JoinRefType::Cross,
        POSITIONAL => ast::JoinRefType::Positional,
        ASOF => ast::JoinRefType::Asof,
        NEAREST => ast::JoinRefType::Nearest,
        other => return Err(corrupt(format!("join reference of type {other}"))),
    };
    Ok(ast::JoinRef {
        left: Box::new(table_ref(required(stored.left, "a side of a join")?)?),
        right: Box::new(table_ref(required(stored.right, "a side of a join")?)?),
        condition: optional(stored.condition)?,
        using_columns: stored.using_columns,
        join_type: join_type(stored.join_type)?,
        ref_type,
        is_implicit: stored.is_implicit.unwrap_or(true),
        nearest_count: stored.nearest_count.unwrap_or(1),
        nearest_descending: stored.nearest_order_type == Some(enums::order_type::DESCENDING),
        nearest_approx: stored.nearest_approx.unwrap_or(false),
        ranking_expression: optional(stored.ranking_expression)?,
    })
}

fn pivot_column(stored: g::PivotColumn) -> Result<ast::PivotColumn> {
    let mut entries = Vec::with_capacity(stored.entries.len());
    for entry in stored.entries {
        let values = entry
            .values
            .iter()
            .map(|value| value_literal(value)?.ok_or_else(|| not_ported("this kind of PIVOT value")))
            .collect::<Result<_>>()?;
        entries.push(ast::PivotColumnEntry {
            values,
            expr: entry.star_expr.map(|e| expression(*e)).transpose()?,
            alias: entry.alias,
        });
    }
    Ok(ast::PivotColumn {
        pivot_expressions: expressions(stored.pivot_expressions)?,
        unpivot_names: stored.unpivot_names,
        entries,
        pivot_enum: stored.pivot_enum,
        subquery: None,
    })
}

/// `TableRef::Deserialize`.
pub fn table_ref(stored: g::TableRef) -> Result<ast::TableRef> {
    use g::TableRefKind as K;
    let mut column_name_alias = Vec::new();
    let kind = match stored.kind {
        K::BaseTableRef(t) => {
            column_name_alias = t.column_name_alias;
            ast::TableRefKind::BaseTable {
                name: qualified_name(t.catalog_name, t.schema_name, t.table_name, t.qualified_name),
                at_clause: t
                    .at_clause
                    .map(|clause| {
                        Ok::<_, duckrs_common::Error>(ast::AtClause {
                            unit: clause.unit,
                            expr: boxed_expr(clause.expr, "an AT clause")?,
                        })
                    })
                    .transpose()?,
            }
        }
        K::JoinRef(t) => ast::TableRefKind::Join(join_ref(t)?),
        K::SubqueryRef(t) => {
            column_name_alias = t.column_name_alias;
            ast::TableRefKind::Subquery(Box::new(select_statement(t.subquery)?))
        }
        K::TableFunctionRef(t) => {
            column_name_alias = t.column_name_alias;
            ast::TableRefKind::TableFunction {
                function: boxed_expr(t.function, "a table function")?,
                with_ordinality: t.with_ordinality == Some(enums::ordinality_type::WITH_ORDINALITY),
            }
        }
        K::EmptyTableRef(_) => ast::TableRefKind::Empty,
        K::ExpressionListRef(t) => {
            column_name_alias = t.expected_names;
            ast::TableRefKind::ExpressionList(t.values.into_iter().map(expressions).collect::<Result<_>>()?)
        }
        K::PivotRef(t) => {
            column_name_alias = t.column_name_alias;
            ast::TableRefKind::Pivot(ast::PivotRef {
                source: Box::new(table_ref(required(t.source, "the source of a PIVOT")?)?),
                aggregates: expressions(t.aggregates)?,
                unpivot_names: t.unpivot_names,
                pivots: t.pivots.into_iter().map(pivot_column).collect::<Result<_>>()?,
                groups: t.groups,
                include_nulls: t.include_nulls,
            })
        }
    };
    Ok(ast::TableRef {
        kind,
        alias: stored.alias,
        column_name_alias,
        sample: stored.sample.map(|s| sample(*s)).transpose()?,
        query_location: ast::QueryLocation(stored.query_location.and_then(|l| usize::try_from(l).ok())),
    })
}

/// `MacroFunction::Deserialize` and `FinalizeDeserialization`.
pub fn macro_function(stored: g::MacroFunction) -> Result<ast::MacroFunction> {
    let mut names = Vec::with_capacity(stored.parameters.len());
    for parameter in expressions(stored.parameters)? {
        let ExprKind::ColumnRef(mut parts) = parameter.kind else {
            return Err(corrupt("a macro parameter is not a name"));
        };
        names.push(parts.pop().ok_or_else(|| corrupt("a macro parameter has no name"))?);
    }
    let mut defaults = Vec::with_capacity(stored.default_parameters.len());
    for (name, value) in stored.default_parameters {
        // older versions don't list the parameters that have a default
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name.clone());
        }
        defaults.push((name, required_expr(value, "a macro default")?));
    }
    let unknown_to_none = |ty: LogicalType| (ty.id() != LogicalTypeId::Unknown).then_some(CastType::Resolved(ty));
    let mut types = stored.types.into_iter();
    let parameters = names
        .into_iter()
        .map(|name| {
            let position = defaults.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name));
            ast::MacroParameter {
                parameter_type: types.next().and_then(unknown_to_none),
                default: position.map(|i| defaults.swap_remove(i).1),
                name,
            }
        })
        .collect();
    let body = match stored.kind {
        g::MacroFunctionKind::ScalarMacroFunction(m) => {
            ast::MacroBody::Scalar(boxed_expr(m.expression, "the body of a macro")?)
        }
        g::MacroFunctionKind::TableMacroFunction(m) => {
            ast::MacroBody::Table(Box::new(query_node(required(m.query_node, "the body of a macro")?)?))
        }
    };
    Ok(ast::MacroFunction {
        parameters,
        return_type: stored.return_type.and_then(unknown_to_none),
        body,
    })
}

/// A UNIQUE, PRIMARY KEY, CHECK, NOT NULL or FOREIGN KEY. The side of a foreign key that is
/// referenced has one too in C++, which isn't part of the table's own definition: `None`.
pub fn constraint(stored: g::Constraint) -> Result<Option<ast::Constraint>> {
    use enums::{constraint_check_mode, foreign_key_type};
    use g::ConstraintKind as K;
    Ok(Some(match stored.kind {
        K::NotNullConstraint(c) => ast::Constraint::NotNull(c.index as usize),
        K::CheckConstraint(c) => ast::Constraint::Check(boxed_expr(c.expression, "a CHECK")?),
        K::UniqueConstraint(c) => ast::Constraint::Unique {
            // `DConstants::INVALID_INDEX` for a constraint on the table
            index: (c.index != u64::MAX).then_some(c.index as usize),
            columns: c.columns,
            primary_key: c.is_primary_key,
            check_mode: match c.check_mode {
                Some(constraint_check_mode::IMMEDIATE) => ast::ConstraintCheckMode::Immediate,
                Some(constraint_check_mode::DEFERRED) => ast::ConstraintCheckMode::Deferred,
                _ => ast::ConstraintCheckMode::Default,
            },
        },
        K::ForeignKeyConstraint(c) if c.fk_type == foreign_key_type::FK_TYPE_PRIMARY_KEY_TABLE => return Ok(None),
        K::ForeignKeyConstraint(c) => ast::Constraint::ForeignKey {
            fk_columns: c.fk_columns,
            pk_columns: c.pk_columns,
            schema: c.schema,
            table: c.table,
        },
    }))
}

/// `CREATE INDEX`, for the binder. An index of a type that isn't ART came with an extension, which C++
/// doesn't need to read the table either, so here it can be read like any other. Only rewriting it
/// takes the extension (see `DatabaseInstance::has_foreign_indexes`).
pub fn create_index(schema: &str, stored: g::CreateIndexInfo) -> Result<ast::CreateInfo> {
    use enums::index_constraint_type::*;
    Ok(ast::CreateInfo {
        name: ast::QualifiedName {
            qualification: vec![schema.to_string()],
            name: stored.name,
        },
        on_conflict: ast::OnCreateConflict::Error,
        temporary: false,
        kind: ast::CreateKind::Index {
            table: stored.table,
            unique: matches!(stored.constraint_type, UNIQUE | PRIMARY),
            index_type: if stored.index_type_name.is_empty() {
                "ART".to_string()
            } else {
                stored.index_type_name
            },
            expressions: expressions(stored.parsed_expressions)?,
            options: Vec::new(),
        },
    })
}

/// A column of a table: its definition and, if it is stored, its name and type.
pub fn column(stored: g::ColumnDefinition) -> Result<(ast::ColumnDefinition, Option<(String, LogicalType)>)> {
    let expression = optional(stored.expression)?;
    let is_generated = stored.category == enums::table_column_type::GENERATED;
    let physical = (!is_generated).then(|| (stored.name.clone(), stored.type_.clone()));
    let (generated, default_value) = if is_generated {
        (expression, None)
    } else {
        (None, expression)
    };
    Ok((
        ast::ColumnDefinition {
            name: stored.name,
            column_type: Some(CastType::Resolved(stored.type_)),
            generated,
            default_value,
            compression: u8::try_from(stored.compression_type)
                .ok()
                .filter(|compression| *compression != 0)
                .map(crate::storage::compression::storage_name),
        },
        physical,
    ))
}

/// `AlterInfo` of a write-ahead log (`ReplayAlter`): an ALTER TABLE as the statement says it. The other kinds of
/// ALTER aren't logged by duckrs, nor read.
pub fn alter_info(stored: g::AlterInfo) -> Result<ast::AlterInfo> {
    let g::AlterInfoKind::AlterTableInfo(table) = stored.kind else {
        return Err(not_ported("an ALTER of other than a table in a write-ahead log"));
    };
    // a column, or a field of one when the path says so
    let path = |name: String, path: Vec<String>| if path.is_empty() { vec![name] } else { path };
    let kind = match table.kind {
        g::AlterTableInfoKind::RenameColumnInfo(info) => ast::AlterKind::RenameColumn {
            path: vec![info.old_name],
            new_name: info.new_name,
        },
        g::AlterTableInfoKind::RenameTableInfo(info) => ast::AlterKind::RenameTable(info.new_table_name),
        g::AlterTableInfoKind::AddColumnInfo(info) => ast::AlterKind::AddColumn {
            column: column(info.new_column)?.0,
            if_not_exists: info.if_column_not_exists,
            not_null: info.add_not_null,
            unique: info.add_unique,
        },
        g::AlterTableInfoKind::RemoveColumnInfo(info) => ast::AlterKind::RemoveColumn {
            path: vec![info.removed_column],
            if_exists: info.if_column_exists,
            cascade: info.cascade,
        },
        g::AlterTableInfoKind::ChangeColumnTypeInfo(info) => ast::AlterKind::ChangeColumnType {
            path: path(info.column_name, info.column_path),
            target: Some(CastType::Resolved(info.target_type)),
            using: optional(info.expression)?,
        },
        g::AlterTableInfoKind::SetDefaultInfo(info) => ast::AlterKind::SetDefault {
            path: path(info.column_name, info.column_path),
            expression: optional(info.expression)?,
        },
        g::AlterTableInfoKind::SetNotNullInfo(info) => {
            ast::AlterKind::SetNotNull(path(info.column_name, info.column_path))
        }
        g::AlterTableInfoKind::DropNotNullInfo(info) => {
            ast::AlterKind::DropNotNull(path(info.column_name, info.column_path))
        }
        _ => return Err(not_ported("this kind of ALTER TABLE in a write-ahead log")),
    };
    Ok(ast::AlterInfo {
        name: qualified_name(String::new(), stored.schema, stored.name, stored.qualified_name),
        if_exists: stored.if_not_found == enums::on_entry_not_found::RETURN_NULL,
        kind,
    })
}
