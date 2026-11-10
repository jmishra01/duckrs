//! The way back of `convert.rs`: turns the AST into the objects of a database file, as v2.0.0 has
//! them. There constants are literals and casts name parsed types, so that the text of a query goes
//! into the file as it is, and a file reads back as the tree that was written.

use super::custom::{CastExpression, FunctionExpression, LambdaExpression, QualifiedName, WindowExpression};
use super::generated::{self as g, enums};
use super::not_ported;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{self, CastType, ComparisonOp, Expr, ExprKind, Literal, LiteralKind, OperatorType};

type Child = Option<Box<g::ParsedExpression>>;

fn boxed(expr: &Expr) -> Result<Child> {
    expression(expr).map(|e| Some(Box::new(e)))
}

fn optional(expr: &Option<Box<Expr>>) -> Result<Child> {
    expr.as_ref().map_or(Ok(None), |e| boxed(e))
}

fn expressions(list: &[Expr]) -> Result<Vec<Child>> {
    list.iter().map(boxed).collect()
}

fn comparison(op: ComparisonOp) -> u64 {
    use enums::expression_type::*;
    match op {
        ComparisonOp::Equal => COMPARE_EQUAL,
        ComparisonOp::NotEqual => COMPARE_NOTEQUAL,
        ComparisonOp::LessThan => COMPARE_LESSTHAN,
        ComparisonOp::GreaterThan => COMPARE_GREATERTHAN,
        ComparisonOp::LessThanOrEqual => COMPARE_LESSTHANOREQUALTO,
        ComparisonOp::GreaterThanOrEqual => COMPARE_GREATERTHANOREQUALTO,
        ComparisonOp::DistinctFrom => COMPARE_DISTINCT_FROM,
        ComparisonOp::NotDistinctFrom => COMPARE_NOT_DISTINCT_FROM,
    }
}

fn operator(op: OperatorType) -> u64 {
    use enums::expression_type::*;
    match op {
        OperatorType::Not => OPERATOR_NOT,
        OperatorType::IsNull => OPERATOR_IS_NULL,
        OperatorType::IsNotNull => OPERATOR_IS_NOT_NULL,
        OperatorType::In => COMPARE_IN,
        OperatorType::NotIn => COMPARE_NOT_IN,
        OperatorType::Coalesce => OPERATOR_COALESCE,
        OperatorType::Unpack => OPERATOR_UNPACK,
        OperatorType::Try => OPERATOR_TRY,
        OperatorType::ArrayExtract => ARRAY_EXTRACT,
        OperatorType::ArraySlice => ARRAY_SLICE,
        OperatorType::StructExtract => STRUCT_EXTRACT,
        OperatorType::ArrayConstructor => ARRAY_CONSTRUCTOR,
        OperatorType::Grouping => GROUPING_FUNCTION,
    }
}

fn literal(stored: &Literal) -> g::Literal {
    use enums::literal_kind::*;
    let kind = match stored.kind {
        LiteralKind::Null => NULL_LITERAL,
        LiteralKind::Boolean => BOOLEAN,
        LiteralKind::Integer => INTEGER,
        LiteralKind::Numeric => NUMERIC,
        LiteralKind::String => STRING,
        LiteralKind::Hex => HEX,
        LiteralKind::Bit => BIT,
    };
    g::Literal {
        kind,
        text: stored.text.clone(),
    }
}

/// Splits a name into the fields that older readers look at.
fn name_parts(name: &ast::QualifiedName) -> (String, String, String) {
    let (catalog, schema) = match name.qualification.as_slice() {
        [] => (String::new(), String::new()),
        [schema] => (String::new(), schema.clone()),
        [catalog, schema, ..] => (catalog.clone(), schema.clone()),
    };
    (catalog, schema, name.name.clone())
}

fn order_by(node: &ast::OrderByNode) -> Result<g::OrderByNode> {
    use enums::{order_by_null_type, order_type};
    Ok(g::OrderByNode {
        type_: match node.order {
            ast::OrderType::Default => order_type::ORDER_DEFAULT,
            ast::OrderType::Ascending => order_type::ASCENDING,
            ast::OrderType::Descending => order_type::DESCENDING,
        },
        null_order: match node.nulls {
            ast::NullOrder::Default => order_by_null_type::ORDER_DEFAULT,
            ast::NullOrder::NullsFirst => order_by_null_type::NULLS_FIRST,
            ast::NullOrder::NullsLast => order_by_null_type::NULLS_LAST,
        },
        expression: boxed(&node.expr)?,
    })
}

fn order_bys(list: &[ast::OrderByNode]) -> Result<Vec<g::OrderByNode>> {
    list.iter().map(order_by).collect()
}

fn arguments(list: &[ast::FunctionArgument]) -> Result<Vec<g::FunctionArgument>> {
    list.iter()
        .map(|argument| {
            Ok(g::FunctionArgument {
                name: argument.name.clone(),
                expression: boxed(&argument.expr)?,
            })
        })
        .collect()
}

fn function(stored: &ast::FunctionExpr) -> Result<FunctionExpression> {
    let (catalog, schema, function_name) = name_parts(&stored.name);
    // there always is one, if empty: v2.0.0 doesn't read a function without it
    let order_bys = Some(Box::new(g::ResultModifier {
        type_: enums::result_modifier_type::ORDER_MODIFIER,
        kind: g::ResultModifierKind::OrderModifier(g::OrderModifier {
            orders: order_bys(&stored.order_by)?,
        }),
    }));
    Ok(FunctionExpression {
        function_name,
        schema,
        children: Vec::new(),
        filter: optional(&stored.filter)?,
        order_bys,
        distinct: stored.distinct,
        is_operator: stored.is_operator,
        export_state: stored.export_state,
        catalog,
        arguments: arguments(&stored.args)?,
        qualified_name: None,
    })
}

fn window_boundary(boundary: ast::WindowBoundary) -> u64 {
    use ast::WindowBoundary as B;
    use enums::window_boundary::*;
    match boundary {
        B::Invalid => INVALID,
        B::UnboundedPreceding => UNBOUNDED_PRECEDING,
        B::UnboundedFollowing => UNBOUNDED_FOLLOWING,
        B::CurrentRowRange => CURRENT_ROW_RANGE,
        B::CurrentRowRows => CURRENT_ROW_ROWS,
        B::CurrentRowGroups => CURRENT_ROW_GROUPS,
        B::ExprPrecedingRows => EXPR_PRECEDING_ROWS,
        B::ExprPrecedingRange => EXPR_PRECEDING_RANGE,
        B::ExprPrecedingGroups => EXPR_PRECEDING_GROUPS,
        B::ExprFollowingRows => EXPR_FOLLOWING_ROWS,
        B::ExprFollowingRange => EXPR_FOLLOWING_RANGE,
        B::ExprFollowingGroups => EXPR_FOLLOWING_GROUPS,
    }
}

/// `WindowToExpressionType`.
fn window_type(name: &str) -> u64 {
    use enums::expression_type::*;
    match name {
        "rank" => WINDOW_RANK,
        "rank_dense" | "dense_rank" => WINDOW_RANK_DENSE,
        "percent_rank" => WINDOW_PERCENT_RANK,
        "row_number" => WINDOW_ROW_NUMBER,
        "first_value" => WINDOW_FIRST_VALUE,
        "last_value" => WINDOW_LAST_VALUE,
        "nth_value" => WINDOW_NTH_VALUE,
        "cume_dist" => WINDOW_CUME_DIST,
        "lead" => WINDOW_LEAD,
        "lag" => WINDOW_LAG,
        "ntile" => WINDOW_NTILE,
        "fill" => WINDOW_FILL,
        _ => WINDOW_AGGREGATE,
    }
}

fn window(stored: &ast::WindowExpr) -> Result<WindowExpression> {
    use enums::window_exclude_mode::*;
    let (catalog, schema, function_name) = name_parts(&stored.name);
    Ok(WindowExpression {
        function_name,
        schema,
        catalog,
        children: Vec::new(),
        partitions: expressions(&stored.partitions)?,
        orders: order_bys(&stored.orders)?,
        start: window_boundary(stored.start),
        end: window_boundary(stored.end),
        start_expr: optional(&stored.start_expr)?,
        end_expr: optional(&stored.end_expr)?,
        offset_expr: None,
        default_expr: None,
        ignore_nulls: stored.ignore_nulls,
        filter_expr: optional(&stored.filter)?,
        exclude_clause: Some(match stored.exclude {
            ast::WindowExclude::NoOther => NO_OTHER,
            ast::WindowExclude::CurrentRow => CURRENT_ROW,
            ast::WindowExclude::Group => GROUP,
            ast::WindowExclude::Ties => TIES,
        }),
        distinct: stored.distinct,
        arg_orders: order_bys(&stored.arg_orders)?,
        has_ignore_nulls: stored.has_ignore_nulls,
        arguments: arguments(&stored.args)?,
        qualified_name: None,
    })
}

/// The expression that names a type, from what a file or a query has of it.
fn type_expression(target: &CastType) -> Result<Expr> {
    match target {
        CastType::Parsed(expr) => Ok((**expr).clone()),
        CastType::Builtin(name) => Ok(ExprKind::Type(ast::TypeExpr {
            name: ast::QualifiedName::new(*name),
            children: Vec::new(),
        })
        .into()),
        CastType::Resolved(ty) => {
            let sql = format!("SELECT CAST(NULL AS {ty})");
            let statements = duckrs_parser::parse_ast(&sql)?;
            if let [ast::Statement::Select(select)] = statements.as_slice()
                && let ast::QueryNodeKind::Select(node) = &select.node.kind
                && let [
                    Expr {
                        kind: ExprKind::Cast { target, .. },
                        ..
                    },
                ] = node.select_list.as_slice()
                && !matches!(target, CastType::Resolved(_))
            {
                return type_expression(target);
            }
            Err(not_ported(&format!("the type {ty}")))
        }
    }
}

fn subquery(
    stored: &ast::SelectStatement,
    subquery_type: ast::SubqueryType,
    child: &Option<Box<Expr>>,
    comparison_op: ComparisonOp,
) -> Result<g::SubqueryExpression> {
    use enums::subquery_type::*;
    let any = subquery_type == ast::SubqueryType::Any;
    Ok(g::SubqueryExpression {
        subquery_type: match subquery_type {
            ast::SubqueryType::Scalar => SCALAR,
            ast::SubqueryType::Exists => EXISTS,
            ast::SubqueryType::NotExists => NOT_EXISTS,
            ast::SubqueryType::Any => ANY,
        },
        subquery: Some(Box::new(select_statement(stored)?)),
        child: optional(child)?,
        comparison_type: if any {
            comparison(comparison_op)
        } else {
            enums::expression_type::INVALID
        },
    })
}

fn column_name(name: &ast::QualifiedColumnName) -> g::QualifiedColumnName {
    let mut parts = name.qualification.iter().rev().cloned();
    let table = parts.next().unwrap_or_default();
    let schema = parts.next().unwrap_or_default();
    let catalog = parts.next().unwrap_or_default();
    g::QualifiedColumnName {
        catalog,
        schema,
        table,
        column: name.column.clone(),
    }
}

fn star(stored: &ast::StarExpr) -> Result<g::StarExpression> {
    let (plain, qualified): (Vec<_>, Vec<_>) = stored.exclude_list.iter().partition(|c| c.qualification.is_empty());
    Ok(g::StarExpression {
        relation_name: stored.relation_name.clone(),
        exclude_list: plain.into_iter().map(|c| c.column.clone()).collect(),
        replace_list: stored
            .replace_list
            .iter()
            .map(|(name, e)| Ok((name.clone(), boxed(e)?)))
            .collect::<Result<_>>()?,
        columns: stored.columns,
        expr: optional(&stored.expr)?,
        unpacked: None,
        qualified_exclude_list: Some(qualified.into_iter().map(column_name).collect()),
        rename_list: stored
            .rename_list
            .iter()
            .map(|(column, name)| (column_name(column), name.clone()))
            .collect(),
    })
}

/// `ParsedExpression::Serialize`.
pub fn expression(stored: &Expr) -> Result<g::ParsedExpression> {
    use enums::{expression_class as class, expression_type as ty};
    use g::ParsedExpressionKind as K;
    let (class, type_, kind) = match &stored.kind {
        ExprKind::Constant(value) => (
            class::CONSTANT,
            ty::VALUE_CONSTANT,
            K::ConstantExpression(g::ConstantExpression {
                // what files before v2.0.0 have of a constant: its value
                value: crate::planner::binder::literal_value(value).ok(),
                literal: Some(literal(value)),
            }),
        ),
        ExprKind::ColumnRef(names) => (
            class::COLUMN_REF,
            ty::COLUMN_REF,
            K::ColumnRefExpression(g::ColumnRefExpression {
                column_names: names.clone(),
            }),
        ),
        ExprKind::Star(value) => (class::STAR, ty::STAR, K::StarExpression(star(value)?)),
        ExprKind::Function(value) => (class::FUNCTION, ty::FUNCTION, K::FunctionExpression(function(value)?)),
        ExprKind::Window(value) => (
            class::WINDOW,
            window_type(&value.name.name),
            K::WindowExpression(window(value)?),
        ),
        ExprKind::Comparison { op, left, right } => (
            class::COMPARISON,
            comparison(*op),
            K::ComparisonExpression(g::ComparisonExpression {
                left: boxed(left)?,
                right: boxed(right)?,
            }),
        ),
        ExprKind::Conjunction { and, children } => (
            class::CONJUNCTION,
            if *and { ty::CONJUNCTION_AND } else { ty::CONJUNCTION_OR },
            K::ConjunctionExpression(g::ConjunctionExpression {
                children: expressions(children)?,
            }),
        ),
        ExprKind::Operator { op, children } => (
            class::OPERATOR,
            operator(*op),
            K::OperatorExpression(g::OperatorExpression {
                children: expressions(children)?,
            }),
        ),
        ExprKind::Between { input, lower, upper } => (
            class::BETWEEN,
            ty::COMPARE_BETWEEN,
            K::BetweenExpression(g::BetweenExpression {
                input: boxed(input)?,
                lower: boxed(lower)?,
                upper: boxed(upper)?,
            }),
        ),
        ExprKind::Case { checks, else_expr } => (
            class::CASE,
            ty::CASE_EXPR,
            K::CaseExpression(g::CaseExpression {
                case_checks: checks
                    .iter()
                    .map(|(when, then)| {
                        Ok(g::CaseCheck {
                            when_expr: boxed(when)?,
                            then_expr: boxed(then)?,
                        })
                    })
                    .collect::<Result<_>>()?,
                else_expr: boxed(else_expr)?,
            }),
        ),
        ExprKind::Cast {
            child,
            target,
            try_cast,
        } => (
            class::CAST,
            ty::OPERATOR_CAST,
            K::CastExpression(CastExpression {
                child: boxed(child)?,
                cast_type: None,
                try_cast: *try_cast,
                type_expr: boxed(&type_expression(target)?)?,
            }),
        ),
        ExprKind::Collate { child, collation } => (
            class::COLLATE,
            ty::COLLATE,
            K::CollateExpression(g::CollateExpression {
                child: boxed(child)?,
                collation: collation.clone(),
            }),
        ),
        ExprKind::Subquery {
            subquery: query,
            subquery_type,
            child,
            comparison: op,
        } => (
            class::SUBQUERY,
            ty::SUBQUERY,
            K::SubqueryExpression(subquery(query, *subquery_type, child, *op)?),
        ),
        ExprKind::Lambda { lhs, expr, syntax } => (
            class::LAMBDA,
            ty::LAMBDA,
            K::LambdaExpression(LambdaExpression {
                lhs: boxed(lhs)?,
                expr: boxed(expr)?,
                syntax_type: match syntax {
                    ast::LambdaSyntax::Arrow => enums::lambda_syntax_type::SINGLE_ARROW,
                    ast::LambdaSyntax::Keyword => enums::lambda_syntax_type::LAMBDA_KEYWORD,
                    ast::LambdaSyntax::StoredArrow => enums::lambda_syntax_type::SINGLE_ARROW_STORAGE,
                },
            }),
        ),
        ExprKind::Parameter(identifier) => (
            class::PARAMETER,
            ty::VALUE_PARAMETER,
            K::ParameterExpression(g::ParameterExpression {
                identifier: identifier.clone(),
            }),
        ),
        ExprKind::Positional(index) => (
            class::POSITIONAL_REFERENCE,
            ty::POSITIONAL_REFERENCE,
            K::PositionalReferenceExpression(g::PositionalReferenceExpression { index: *index }),
        ),
        ExprKind::Default => (
            class::DEFAULT,
            ty::VALUE_DEFAULT,
            K::DefaultExpression(g::DefaultExpression {}),
        ),
        ExprKind::Type(value) => {
            let (catalog, schema, type_name) = name_parts(&value.name);
            (
                class::TYPE,
                ty::TYPE,
                K::TypeExpression(g::TypeExpression {
                    catalog,
                    schema,
                    type_name,
                    children: expressions(&value.children)?,
                    qualified_name: None,
                }),
            )
        }
        ExprKind::Concatenation(_) | ExprKind::Quantified { .. } | ExprKind::Alternation(..) | ExprKind::Anchor(_) => {
            return Err(write_not_ported("MATCH_RECOGNIZE patterns"));
        }
    };
    Ok(g::ParsedExpression {
        class,
        type_,
        alias: stored.alias.clone(),
        query_location: None,
        query_location_length: None,
        kind,
    })
}

pub(crate) fn write_not_ported(what: &str) -> Error {
    Error::not_implemented(format!("writing {what} to a database file is not yet ported to duckrs"))
}

fn sample(stored: &ast::SampleOptions) -> Result<g::SampleOptions> {
    use enums::sample_method::*;
    let bad_size = || Error::new(ErrorKind::Internal, "the size of a sample is not a number");
    let sample_size = if stored.is_percentage {
        Value::double(stored.sample_size.parse().map_err(|_| bad_size())?)
    } else {
        Value::bigint(stored.sample_size.parse().map_err(|_| bad_size())?)
    };
    Ok(g::SampleOptions {
        sample_size,
        is_percentage: stored.is_percentage,
        method: match stored.method {
            ast::SampleMethod::System => SYSTEM_SAMPLE,
            ast::SampleMethod::Bernoulli => BERNOULLI_SAMPLE,
            ast::SampleMethod::Reservoir => RESERVOIR_SAMPLE,
        },
        seed: stored.seed.map_or(-1, |seed| seed as i64),
        repeatable: stored.repeatable,
        sample_rate: None,
    })
}

fn result_modifier(stored: &ast::ResultModifier) -> Result<g::ResultModifier> {
    use enums::result_modifier_type::*;
    use g::ResultModifierKind as K;
    Ok(match stored {
        ast::ResultModifier::Order(orders) => g::ResultModifier {
            type_: ORDER_MODIFIER,
            kind: K::OrderModifier(g::OrderModifier {
                orders: order_bys(orders)?,
            }),
        },
        ast::ResultModifier::Limit { limit, offset, percent } => g::ResultModifier {
            type_: LIMIT_MODIFIER,
            kind: K::LimitModifier(g::LimitModifier {
                limit: optional(limit)?,
                offset: optional(offset)?,
                limit_type: percent.then_some(enums::limit_value_type::PERCENTAGE),
            }),
        },
        ast::ResultModifier::Distinct(targets) => g::ResultModifier {
            type_: DISTINCT_MODIFIER,
            kind: K::DistinctModifier(g::DistinctModifier {
                distinct_on_targets: expressions(targets)?,
            }),
        },
    })
}

fn cte_map(stored: &ast::CteMap) -> Result<g::CommonTableExpressionMap> {
    use enums::cte_materialize::*;
    let mut map = Vec::with_capacity(stored.0.len());
    for (name, info) in &stored.0 {
        let node = query_node(&info.query)?;
        let info = g::CommonTableExpressionInfo {
            aliases: info.aliases.clone(),
            // what files before v2.0.0 have of the query
            query: Some(Box::new(g::SelectStatement {
                node: Some(Box::new(node.clone())),
                named_param_map: Vec::new(),
            })),
            materialized: match info.materialized {
                ast::CteMaterialize::Default => CTE_MATERIALIZE_DEFAULT,
                ast::CteMaterialize::Always => CTE_MATERIALIZE_ALWAYS,
                ast::CteMaterialize::Never => CTE_MATERIALIZE_NEVER,
            },
            key_targets: expressions(&info.key_targets)?,
            payload_aggregates: Vec::new(),
            query_node: Some(Box::new(node)),
        };
        map.push((name.clone(), Some(Box::new(info))));
    }
    Ok(g::CommonTableExpressionMap { map })
}

fn select_node(stored: &ast::SelectNode) -> Result<g::SelectNode> {
    use enums::aggregate_handling::*;
    Ok(g::SelectNode {
        select_list: expressions(&stored.select_list)?,
        from_table: Some(Box::new(table_ref(&stored.from_table)?)),
        where_clause: optional(&stored.where_clause)?,
        group_expressions: expressions(&stored.groups.group_expressions)?,
        group_sets: stored
            .groups
            .grouping_sets
            .iter()
            .map(|set| set.iter().map(|&index| index as u64).collect())
            .collect(),
        aggregate_handling: if stored.force_aggregates {
            FORCE_AGGREGATES
        } else {
            STANDARD_HANDLING
        },
        having: optional(&stored.having)?,
        sample: stored.sample.as_ref().map(sample).transpose()?.map(Box::new),
        qualify: optional(&stored.qualify)?,
    })
}

/// `QueryNode::Serialize`.
pub fn query_node(stored: &ast::QueryNode) -> Result<g::QueryNode> {
    use enums::query_node_type::*;
    use g::QueryNodeKind as K;
    let (type_, kind) = match &stored.kind {
        ast::QueryNodeKind::Select(node) => (SELECT_NODE, K::SelectNode(select_node(node)?)),
        ast::QueryNodeKind::SetOperation(node) => {
            use enums::set_operation_type::*;
            let children = node
                .children
                .iter()
                .map(|child| query_node(child).map(|c| Some(Box::new(c))))
                .collect::<Result<_>>()?;
            (
                SET_OPERATION_NODE,
                K::SetOperationNode(g::SetOperationNode {
                    setop_type: match node.setop_type {
                        ast::SetOpType::Union => UNION,
                        ast::SetOpType::UnionByName => UNION_BY_NAME,
                        ast::SetOpType::Except => EXCEPT,
                        ast::SetOpType::Intersect => INTERSECT,
                    },
                    left: None,
                    right: None,
                    setop_all: Some(node.setop_all),
                    children,
                }),
            )
        }
        ast::QueryNodeKind::RecursiveCte(node) => (
            RECURSIVE_CTE_NODE,
            K::RecursiveCTENode(g::RecursiveCTENode {
                cte_name: node.ctename.clone(),
                union_all: Some(node.union_all),
                left: Some(Box::new(query_node(&node.left)?)),
                right: Some(Box::new(query_node(&node.right)?)),
                aliases: node.aliases.clone(),
                key_targets: expressions(&node.key_targets)?,
            }),
        ),
        ast::QueryNodeKind::Insert(insert) => (INSERT_QUERY_NODE, K::InsertQueryNode(insert_node(insert)?)),
        ast::QueryNodeKind::Update(update) => (
            UPDATE_QUERY_NODE,
            K::UpdateQueryNode(g::UpdateQueryNode {
                table: Some(Box::new(table_ref(&update.table)?)),
                from_table: update.from_table.as_ref().map(table_ref).transpose()?.map(Box::new),
                returning_list: expressions(&update.returning)?,
                set_info: Some(Box::new(update_set_info(&update.set_info)?)),
                prioritize_table_when_binding: Some(false),
            }),
        ),
        ast::QueryNodeKind::Delete(delete) => (
            DELETE_QUERY_NODE,
            K::DeleteQueryNode(g::DeleteQueryNode {
                condition: optional(&delete.condition)?,
                table: Some(Box::new(table_ref(&delete.table)?)),
                using_clauses: delete
                    .using
                    .iter()
                    .map(|t| table_ref(t).map(|t| Some(Box::new(t))))
                    .collect::<Result<_>>()?,
                returning_list: expressions(&delete.returning)?,
            }),
        ),
        ast::QueryNodeKind::Merge(_) | ast::QueryNodeKind::Copy(_) => {
            return Err(write_not_ported("a statement that writes in a stored query"));
        }
    };
    // a statement keeps its WITH in itself as well
    let statement_ctes = match &stored.kind {
        ast::QueryNodeKind::Insert(insert) => Some(&insert.cte_map),
        ast::QueryNodeKind::Update(update) => Some(&update.cte_map),
        ast::QueryNodeKind::Delete(delete) => Some(&delete.cte_map),
        _ => None,
    };
    let ctes = match statement_ctes {
        Some(ctes) if stored.cte_map.0.is_empty() => ctes,
        _ => &stored.cte_map,
    };
    Ok(g::QueryNode {
        type_,
        modifiers: stored
            .modifiers
            .iter()
            .map(|m| result_modifier(m).map(|m| Some(Box::new(m))))
            .collect::<Result<_>>()?,
        cte_map: cte_map(ctes)?,
        kind,
    })
}

/// `UpdateSetInfo::Serialize`.
fn update_set_info(info: &ast::UpdateSetInfo) -> Result<g::UpdateSetInfo> {
    Ok(g::UpdateSetInfo {
        condition: optional(&info.condition)?,
        columns: info.columns.clone(),
        expressions: expressions(&info.expressions)?,
    })
}

/// `InsertQueryNode::Serialize`.
fn insert_node(insert: &ast::InsertStatement) -> Result<g::InsertQueryNode> {
    let (catalog, schema, table) = name_parts(&insert.table);
    let mut path = insert.table.qualification.clone();
    path.push(insert.table.name.clone());
    let on_conflict_info = match &insert.on_conflict {
        Some(conflict) => Some(Box::new(g::OnConflictInfo {
            action_type: match conflict.action {
                ast::OnConflictAction::Nothing => 1,
                ast::OnConflictAction::Update => 2,
                ast::OnConflictAction::Replace => 3,
            },
            indexed_columns: conflict.indexed_columns.clone(),
            set_info: conflict
                .set_info
                .as_ref()
                .map(update_set_info)
                .transpose()?
                .map(Box::new),
            condition: optional(&conflict.condition)?,
        })),
        None => None,
    };
    let table_ref = match &insert.table_alias {
        Some(alias) if !alias.is_empty() => {
            let mut target = ast::TableRef::from(ast::TableRefKind::BaseTable {
                name: insert.table.clone(),
                at_clause: None,
            });
            target.alias = alias.clone();
            Some(Box::new(table_ref(&target)?))
        }
        _ => None,
    };
    Ok(g::InsertQueryNode {
        select_statement: match &insert.values {
            ast::InsertValues::Select(select) => Some(Box::new(select_statement(select)?)),
            ast::InsertValues::DefaultValues => None,
        },
        columns: insert.columns.clone(),
        table,
        schema,
        catalog,
        returning_list: expressions(&insert.returning)?,
        on_conflict_info,
        table_ref,
        default_values: Some(matches!(insert.values, ast::InsertValues::DefaultValues)),
        column_order: Some(u64::from(insert.by_name)),
        qualified_name: Some(QualifiedName { path }),
    })
}

pub fn select_statement(stored: &ast::SelectStatement) -> Result<g::SelectStatement> {
    Ok(g::SelectStatement {
        node: Some(Box::new(query_node(&stored.node)?)),
        named_param_map: Vec::new(),
    })
}

fn join_ref(stored: &ast::JoinRef) -> Result<g::JoinRef> {
    use ast::JoinType as J;
    use enums::{join_ref_type, join_type};
    Ok(g::JoinRef {
        left: Some(Box::new(table_ref(&stored.left)?)),
        right: Some(Box::new(table_ref(&stored.right)?)),
        condition: optional(&stored.condition)?,
        join_type: match stored.join_type {
            J::Inner => join_type::INNER,
            J::Left => join_type::LEFT,
            J::Right => join_type::RIGHT,
            J::Outer => join_type::OUTER,
            J::Semi => join_type::SEMI,
            J::Anti => join_type::ANTI,
            J::Mark => join_type::MARK,
            J::Single => join_type::SINGLE,
            J::RightSemi => join_type::RIGHT_SEMI,
            J::RightAnti => join_type::RIGHT_ANTI,
        },
        ref_type: match stored.ref_type {
            ast::JoinRefType::Regular => join_ref_type::REGULAR,
            ast::JoinRefType::Natural => join_ref_type::NATURAL,
            ast::JoinRefType::Cross => join_ref_type::CROSS,
            ast::JoinRefType::Positional => join_ref_type::POSITIONAL,
            ast::JoinRefType::Asof => join_ref_type::ASOF,
            ast::JoinRefType::Nearest => join_ref_type::NEAREST,
        },
        using_columns: stored.using_columns.clone(),
        delim_flipped: false,
        duplicate_eliminated_columns: Vec::new(),
        is_implicit: Some(stored.is_implicit),
        ranking_expression: optional(&stored.ranking_expression)?,
        nearest_count: Some(stored.nearest_count),
        nearest_order_type: stored.nearest_descending.then_some(enums::order_type::DESCENDING),
        nearest_approx: Some(stored.nearest_approx),
    })
}

fn pivot_value(literal: &Literal) -> Result<Value> {
    match literal.kind {
        LiteralKind::String => Ok(Value::varchar(literal.text.clone())),
        LiteralKind::Integer => match literal.try_get_i64() {
            Some(value) => Ok(Value::bigint(value)),
            None => Err(write_not_ported("this kind of PIVOT value")),
        },
        _ => Err(write_not_ported("this kind of PIVOT value")),
    }
}

fn pivot_column(stored: &ast::PivotColumn) -> Result<g::PivotColumn> {
    if stored.subquery.is_some() {
        return Err(write_not_ported("a PIVOT over a subquery"));
    }
    let entries = stored
        .entries
        .iter()
        .map(|entry| {
            Ok(g::PivotColumnEntry {
                values: entry.values.iter().map(pivot_value).collect::<Result<_>>()?,
                star_expr: entry.expr.as_ref().map(boxed).transpose()?.flatten(),
                alias: entry.alias.clone(),
            })
        })
        .collect::<Result<_>>()?;
    Ok(g::PivotColumn {
        pivot_expressions: expressions(&stored.pivot_expressions)?,
        unpivot_names: stored.unpivot_names.clone(),
        entries,
        pivot_enum: stored.pivot_enum.clone(),
    })
}

/// `TableRef::Serialize`.
pub fn table_ref(stored: &ast::TableRef) -> Result<g::TableRef> {
    use ast::TableRefKind as A;
    use enums::table_reference_type::*;
    use g::TableRefKind as K;
    let aliases = stored.column_name_alias.clone();
    let (type_, kind) = match &stored.kind {
        A::Empty => (EMPTY_FROM, K::EmptyTableRef(g::EmptyTableRef {})),
        A::BaseTable { name, at_clause } => {
            let (catalog_name, schema_name, table_name) = name_parts(name);
            let at_clause = at_clause
                .as_ref()
                .map(|clause| {
                    Ok::<_, Error>(Box::new(g::AtClause {
                        unit: clause.unit.clone(),
                        expr: boxed(&clause.expr)?,
                    }))
                })
                .transpose()?;
            (
                BASE_TABLE,
                K::BaseTableRef(g::BaseTableRef {
                    schema_name,
                    table_name,
                    column_name_alias: aliases,
                    catalog_name,
                    at_clause,
                    qualified_name: None,
                }),
            )
        }
        A::Join(join) => (JOIN, K::JoinRef(join_ref(join)?)),
        A::Subquery(query) => (
            SUBQUERY,
            K::SubqueryRef(g::SubqueryRef {
                subquery: Some(Box::new(select_statement(query)?)),
                column_name_alias: aliases,
            }),
        ),
        A::TableFunction {
            function,
            with_ordinality,
        } => (
            TABLE_FUNCTION,
            K::TableFunctionRef(g::TableFunctionRef {
                function: boxed(function)?,
                column_name_alias: aliases,
                with_ordinality: with_ordinality.then_some(enums::ordinality_type::WITH_ORDINALITY),
            }),
        ),
        A::ExpressionList(rows) => (
            EXPRESSION_LIST,
            K::ExpressionListRef(g::ExpressionListRef {
                expected_names: aliases,
                expected_types: Vec::new(),
                values: rows.iter().map(|row| expressions(row)).collect::<Result<_>>()?,
            }),
        ),
        A::Pivot(pivot) => (
            PIVOT,
            K::PivotRef(g::PivotRef {
                source: Some(Box::new(table_ref(&pivot.source)?)),
                aggregates: expressions(&pivot.aggregates)?,
                unpivot_names: pivot.unpivot_names.clone(),
                pivots: pivot.pivots.iter().map(pivot_column).collect::<Result<_>>()?,
                groups: pivot.groups.clone(),
                column_name_alias: aliases,
                include_nulls: pivot.include_nulls,
            }),
        ),
        A::Show(_) => return Err(write_not_ported("SHOW and DESCRIBE in a stored query")),
        A::MatchRecognize(_) => return Err(write_not_ported("MATCH_RECOGNIZE")),
    };
    Ok(g::TableRef {
        type_,
        alias: stored.alias.clone(),
        sample: stored.sample.as_ref().map(sample).transpose()?.map(Box::new),
        query_location: None,
        query_location_length: None,
        kind,
    })
}

/// `MacroFunction::Serialize`. `bind` turns a declared type into the type it is.
pub fn macro_function(
    stored: &ast::MacroFunction,
    bind: &dyn Fn(&CastType) -> Result<LogicalType>,
) -> Result<g::MacroFunction> {
    use enums::macro_type::*;
    let unknown = || LogicalType::new(duckrs_common::types::LogicalTypeId::Unknown);
    let mut parameters = Vec::with_capacity(stored.parameters.len());
    let mut defaults = Vec::new();
    let mut types = Vec::with_capacity(stored.parameters.len());
    for parameter in &stored.parameters {
        parameters.push(boxed(&Expr::column(vec![parameter.name.clone()]))?);
        if let Some(default) = &parameter.default {
            defaults.push((parameter.name.clone(), boxed(default)?));
        }
        types.push(parameter.parameter_type.as_ref().map_or_else(|| Ok(unknown()), bind)?);
    }
    let (type_, kind) = match &stored.body {
        ast::MacroBody::Scalar(expr) => (
            SCALAR_MACRO,
            g::MacroFunctionKind::ScalarMacroFunction(g::ScalarMacroFunction {
                expression: boxed(expr)?,
            }),
        ),
        ast::MacroBody::Table(query) => (
            TABLE_MACRO,
            g::MacroFunctionKind::TableMacroFunction(g::TableMacroFunction {
                query_node: Some(Box::new(query_node(query)?)),
            }),
        ),
    };
    Ok(g::MacroFunction {
        type_,
        parameters,
        default_parameters: defaults,
        types,
        return_type: stored.return_type.as_ref().map(bind).transpose()?,
        kind,
    })
}

/// The `CreateInfo` around what an entry says of itself. A file names entries by a path that starts
/// with the catalog, and a schema by its name and an empty name for the entry.
pub fn create_info(type_: u64, path: Vec<String>, kind: g::CreateInfoKind) -> g::CreateInfo {
    // `QualifiedName::Catalog` and `Schema`, which files before v2.0.0 have in place of the path
    let catalog = if path.len() >= 3 {
        path[0].clone()
    } else {
        String::new()
    };
    let schema = if path.len() >= 2 {
        path[path.len() - 2].clone()
    } else {
        String::new()
    };
    g::CreateInfo {
        type_,
        catalog,
        schema,
        temporary: false,
        internal: false,
        on_conflict: enums::on_create_conflict::ERROR_ON_CONFLICT,
        sql: String::new(),
        comment: None,
        tags: Vec::new(),
        dependencies: None,
        extension_name: String::new(),
        qualified_name: Some(QualifiedName { path }),
        kind,
    }
}
