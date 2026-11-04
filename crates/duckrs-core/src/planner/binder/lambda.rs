// Ported from duckdb/src/planner/expression_binder/bind_lambda.cpp and
// duckdb/src/function/lambda_functions.cpp @ rust-port-baseline
//
//! `list_transform`, `list_filter` and `list_reduce`. The lambda's body binds with its parameters
//! ahead of the columns in scope. What else it reads becomes a capture, an argument the
//! expression evaluates once per input row, so that the body only reads its own chunk.

use super::expression::checked_cast;
use super::{Binder, binder_error};
use crate::function::nested::bind_nested_function;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding, LambdaKind};
use duckrs_common::Result;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{Expr, ExprKind, FunctionExpr};
use duckrs_parser::transformer::visit_expr_mut;

/// The parameters of a lambda in scope, innermost last.
#[derive(Debug)]
pub(super) struct LambdaScope {
    table_index: usize,
    names: Vec<String>,
    types: Vec<LogicalType>,
}

fn lambda_kind(name: &str) -> Option<LambdaKind> {
    Some(match name.to_ascii_lowercase().as_str() {
        "list_transform" | "array_transform" | "list_apply" | "array_apply" | "apply" => LambdaKind::Transform,
        "list_filter" | "array_filter" | "filter" => LambdaKind::Filter,
        "list_reduce" | "array_reduce" | "reduce" => LambdaKind::Reduce,
        _ => return None,
    })
}

fn parameter_names(lhs: &Expr) -> Vec<String> {
    let mut names = Vec::new();
    visit_expr_mut(&mut lhs.clone(), &mut |e: &mut Expr| {
        if let ExprKind::ColumnRef(parts) = &e.kind
            && let [name] = parts.as_slice()
        {
            names.push(name.clone());
        }
    });
    names
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

    /// `ExpressionBinder::BindLambdaFunction`.
    pub(super) fn bind_lambda_function(&mut self, function: &FunctionExpr) -> Result<Option<BoundExpr>> {
        let Some(kind) = lambda_kind(&function.name.name) else {
            return Ok(None);
        };
        let Some(lambda) = function.args.get(1) else {
            return Ok(None);
        };
        let ExprKind::Lambda { lhs, expr, .. } = &lambda.expr.kind else {
            return Err(binder_error("Invalid lambda expression!"));
        };
        let maximum = if kind == LambdaKind::Reduce { 3 } else { 2 };
        let names = parameter_names(lhs);
        if names.is_empty() || names.len() > maximum || (kind == LambdaKind::Reduce && names.len() < 2) {
            return Err(binder_error(format!(
                "The lambda function {} takes a different number of parameters than the {} given",
                function.name.name,
                names.len()
            )));
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
        let accumulator = initial
            .as_ref()
            .map_or_else(|| element.clone(), |i| i.return_type.clone());
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
