// Ported from duckdb/src/planner/expression_binder/bind_lambda.cpp and
// duckdb/src/function/lambda_functions.cpp @ rust-port-baseline
//
//! `list_transform`, `list_filter` and `list_reduce`. The lambda's body binds with its parameters
//! ahead of the columns in scope. What else it reads becomes a capture, an argument the
//! expression evaluates once per input row, so that the body only reads its own chunk.

use super::expression::checked_cast;
use super::{Binder, binder_error};
use crate::function::combine_types::try_max_logical_type;
use crate::function::nested::bind_nested_function;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding, LambdaKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::{ErrorKind, Result};
use duckrs_parser::ast::{Expr, ExprKind, FunctionExpr, LambdaSyntax};
use duckrs_parser::transformer::visit_expr_mut;

const DEPRECATED_ARROW: &str = "Deprecated lambda arrow (->) detected. Please transition to the new lambda syntax, \
                                i.e.., lambda x, i: x + i, before DuckDB's next release.\nUse SET \
                                lambda_syntax='ENABLE_SINGLE_ARROW' to revert to the deprecated behavior.\nFor more \
                                information, see https://duckdb.org/docs/current/sql/functions/lambda.html.";

/// The parameters of a lambda in scope, innermost last.
#[derive(Debug)]
pub(super) struct LambdaScope {
    table_index: usize,
    names: Vec<String>,
    types: Vec<LogicalType>,
}

/// Whether `name` is a function that takes a lambda.
pub(super) fn is_lambda_function(name: &str) -> bool {
    lambda_kind(name).is_some()
}

fn lambda_kind(name: &str) -> Option<LambdaKind> {
    Some(match name.to_ascii_lowercase().as_str() {
        "list_transform" | "array_transform" | "list_apply" | "array_apply" | "apply" => LambdaKind::Transform,
        "list_filter" | "array_filter" | "filter" => LambdaKind::Filter,
        "list_reduce" | "array_reduce" | "reduce" => LambdaKind::Reduce,
        _ => return None,
    })
}

/// `LambdaExpression::ExtractColumnRefExpressions`.
fn parameter_names(lhs: &Expr) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut qualified = false;
    visit_expr_mut(&mut lhs.clone(), &mut |e: &mut Expr| {
        if let ExprKind::ColumnRef(parts) = &e.kind {
            match parts.as_slice() {
                [name] => names.push(name.clone()),
                _ => qualified = true,
            }
        }
    });
    if qualified || names.is_empty() {
        return Err(binder_error(
            "Invalid lambda parameters! Parameters must be unqualified comma-separated names like x or (x, y).",
        ));
    }
    Ok(names)
}

impl Binder<'_> {
    pub(super) fn is_lambda_parameter(&self, names: &[String]) -> bool {
        self.lambda_scopes
            .iter()
            .any(|scope| scope.names.iter().any(|n| n.eq_ignore_ascii_case(&names[0])))
    }

    /// A column that is a parameter of a lambda in scope, with the fields of a STRUCT after it.
    pub(super) fn bind_lambda_parameter(&mut self, names: &[String]) -> Result<Option<BoundExpr>> {
        let found = self.lambda_scopes.iter().rev().find_map(|scope| {
            let index = scope.names.iter().position(|n| n.eq_ignore_ascii_case(&names[0]))?;
            Some((scope.table_index, index, scope.types[index].clone()))
        });
        let Some((table_index, index, ty)) = found else {
            return Ok(None);
        };
        let mut bound = BoundExpr::column_ref(ColumnBinding::new(table_index, index), ty, names[0].clone());
        for field in &names[1..] {
            let name = BoundExpr::constant(Value::varchar(field.clone()));
            bound = bind_nested_function("struct_extract", vec![String::new(); 2], vec![bound, name])?;
        }
        Ok(Some(bound))
    }

    /// `ExpressionBinder::TryBindLambdaOrJson`: the deprecated arrow only binds when the
    /// `lambda_syntax` setting enables it.
    pub(super) fn bind_lambda_function(&mut self, function: &FunctionExpr) -> Result<Option<BoundExpr>> {
        let arrow = matches!(
            function.args.get(1).map(|lambda| &lambda.expr.kind),
            Some(ExprKind::Lambda {
                syntax: LambdaSyntax::Arrow,
                ..
            })
        );
        let bound = self.bind_lambda(function);
        if !arrow || self.options.single_arrow_lambdas {
            return bound;
        }
        match bound {
            Ok(None) => Ok(None),
            Ok(Some(_)) => Err(binder_error(DEPRECATED_ARROW)),
            Err(error) if error.kind == ErrorKind::ParameterNotResolved => Err(binder_error(DEPRECATED_ARROW)),
            Err(error) => Err(error),
        }
    }

    /// `ExpressionBinder::BindLambdaFunction`.
    fn bind_lambda(&mut self, function: &FunctionExpr) -> Result<Option<BoundExpr>> {
        let Some(kind) = lambda_kind(&function.name.name) else {
            return Ok(None);
        };
        let Some(lambda) = function.args.get(1) else {
            return Ok(None);
        };
        let ExprKind::Lambda { lhs, expr, .. } = &lambda.expr.kind else {
            return Err(binder_error("Invalid lambda expression!"));
        };
        let names = parameter_names(lhs)?;
        if kind == LambdaKind::Reduce {
            if names.len() < 2 {
                return Err(binder_error("list_reduce expects a function with 2 or 3 arguments"));
            }
            if names.len() > 3 {
                return Err(binder_error(
                    "This lambda function only supports up to three lambda parameters!",
                ));
            }
        } else if names.len() > 2 {
            return Err(binder_error(
                "This lambda function only supports up to two lambda parameters!",
            ));
        }
        let mut list = self.bind_expression(&function.args[0].expr)?;
        if list.return_type.id() == LogicalTypeId::SqlNull {
            return Ok(Some(BoundExpr::constant(Value::sql_null())));
        }
        if list.return_type.id() == LogicalTypeId::Array {
            let element = list.return_type.child_type().expect("a child").clone();
            list = list.cast_to(&LogicalType::list(element), false);
        }
        if list.return_type.id() != LogicalTypeId::List {
            return Err(binder_error("Invalid LIST argument during lambda function binding!"));
        }
        let element = list.return_type.child_type().expect("a child").clone();
        let initial = match function.args.get(2) {
            Some(initial) if kind == LambdaKind::Reduce => Some(self.bind_expression(&initial.expr)?),
            _ => None,
        };
        // the initial value and the elements meet in one type
        let accumulator = match &initial {
            Some(initial) => try_max_logical_type(&element, &initial.return_type).ok_or_else(|| {
                binder_error(format!(
                    "Cannot combine the types {element} and {} of the list and the initial value",
                    initial.return_type
                ))
            })?,
            None => element.clone(),
        };
        let initial = initial.map(|initial| initial.cast_to(&accumulator, false));
        let mut types = if kind == LambdaKind::Reduce {
            vec![accumulator.clone(), element.clone()]
        } else {
            vec![element.clone()]
        };
        types.push(LogicalType::BIGINT);
        types.truncate(names.len());
        let table_index = self.generate_table_index();
        self.lambda_scopes.push(LambdaScope {
            table_index,
            names,
            types,
        });
        let body = self.bind_expression(expr);
        self.lambda_scopes.pop();
        let mut body = body?;
        // the chunk the body reads: its parameters, the index, then the captures
        let parameters = if kind == LambdaKind::Reduce { 3 } else { 2 };
        let mut captures: Vec<BoundExpr> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        body.walk_mut(&mut |e| {
            let BoundExprKind::ColumnRef { binding, .. } = &e.kind else {
                return;
            };
            if binding.table_index == table_index {
                e.kind = BoundExprKind::Reference(binding.column_index);
                return;
            }
            let key = format!("{:?}", e.kind);
            let at = keys.iter().position(|k| *k == key).unwrap_or_else(|| {
                keys.push(key);
                captures.push(e.clone());
                captures.len() - 1
            });
            e.kind = BoundExprKind::Reference(parameters + at);
        });
        let return_type = match kind {
            LambdaKind::Transform => LogicalType::list(body.return_type.clone()),
            LambdaKind::Filter => {
                body = checked_cast(body, &LogicalType::BOOLEAN)?;
                list.return_type.clone()
            }
            LambdaKind::Reduce => {
                body = checked_cast(body, &accumulator)?;
                accumulator
            }
        };
        Ok(Some(BoundExpr::new(
            BoundExprKind::ListLambda {
                kind,
                list: Box::new(list),
                initial: initial.map(Box::new),
                captures,
                body: Box::new(body),
            },
            return_type,
        )))
    }
}
