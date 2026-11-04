// Ported from duckdb/src/function/{function_binder,function_binder_result}.cpp and
// ExpressionBinder::GetExpressionReturnType @ rust-port-baseline
//
//! Overload resolution (`FunctionBinder`): every overload whose parameters all accept the
//! arguments by implicit cast is a match, and the cheapest match wins. Only positional
//! arguments are supported so far; named arguments, varargs and templates come later.

use super::aggregates::{AggregateFunction, AggregateFunctionSet};
use super::cast_rules::implicit_cast;
use super::{ScalarFunction, ScalarFunctionSet};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

/// `ExpressionBinder::GetExpressionReturnType`: constants bind as literal types, which cast
/// more cheaply than their own type.
pub fn expression_return_type(expr: &BoundExpr) -> LogicalType {
    if let BoundExprKind::Constant(value) = &expr.kind {
        if expr.return_type.id() == LogicalTypeId::Varchar {
            return LogicalType::new(LogicalTypeId::StringLiteral);
        }
        if expr.return_type.is_integral()
            && !value.is_null()
            && let Some(literal) = integer_literal_type(value)
        {
            return literal;
        }
    }
    expr.return_type.clone()
}

/// `LogicalType::INTEGER_LITERAL(value)`.
fn integer_literal_type(value: &Value) -> Option<LogicalType> {
    let id = value.logical_type().id();
    Some(match value.data() {
        ValueData::UInt128(v) => match i128::try_from(*v) {
            Ok(v) => LogicalType::integer_literal(id, v, false),
            Err(_) => LogicalType::integer_literal(id, i128::MAX, true),
        },
        _ => LogicalType::integer_literal(id, value.as_i128()?, false),
    })
}

/// `Function::CallToString`: `name(T1, T2)`.
fn call_to_string(name: &str, arguments: &[LogicalType]) -> String {
    let args: Vec<String> = arguments.iter().map(|a| a.to_string()).collect();
    format!("{name}({})", args.join(", "))
}

fn binder_error(message: String) -> Error {
    Error::new(ErrorKind::Binder, message)
}

/// `FunctionBinder::MatchOverload` for positional arguments: the total implicit cast cost, or
/// `None` if the overload cannot take the arguments.
fn match_overload(parameters: &[LogicalType], arguments: &[LogicalType]) -> Option<i64> {
    if parameters.len() != arguments.len() {
        return None;
    }
    let mut cost = 0;
    for (argument, parameter) in arguments.iter().zip(parameters) {
        if argument.id() == LogicalTypeId::Unknown {
            // with a parameter of an unknown type, the cost is 0
            return Some(0);
        }
        let cast_cost = implicit_cast(argument, parameter);
        if cast_cost < 0 {
            return None;
        }
        cost += cast_cost;
    }
    Some(cost)
}

/// `FunctionBinder::SelectFunction`: the cheapest overload, or the error DuckDB reports.
pub fn select_function(set: &ScalarFunctionSet, arguments: &[LogicalType]) -> Result<Arc<ScalarFunction>> {
    select_overload(&set.name, &set.functions, |f| &f.arguments, arguments)
}

/// [`select_function`] for any kind of function.
fn select_overload<F: std::fmt::Display>(
    name: &str,
    functions: &[Arc<F>],
    parameters: impl Fn(&F) -> &[LogicalType],
    arguments: &[LogicalType],
) -> Result<Arc<F>> {
    let mut cheapest: Vec<(&Arc<F>, i64)> = Vec::new();
    for function in functions {
        let Some(cost) = match_overload(parameters(function), arguments) else {
            continue;
        };
        match cheapest.first() {
            Some((_, best)) if *best < cost => continue,
            Some((_, best)) if cost < *best => cheapest.clear(),
            _ => {}
        }
        cheapest.push((function, cost));
    }
    match cheapest.len() {
        0 => Err(binder_error(format!(
            "No function matches the given name and argument types '{}'. You might need to add explicit type casts.",
            call_to_string(name, arguments)
        ))),
        1 => Ok(cheapest[0].0.clone()),
        _ => {
            if arguments.iter().any(|a| a.id() == LogicalTypeId::Unknown) {
                return Err(Error::new(
                    ErrorKind::ParameterNotResolved,
                    "Could not determine the types of the prepared statement parameters",
                ));
            }
            let mut message = format!(
                "Could not choose a best candidate function for the function call \"{}\". In order to select one, \
                 please add explicit type casts.\nCandidates:",
                call_to_string(name, arguments)
            );
            for (function, _) in &cheapest {
                message += &format!("\n    {function}");
            }
            message += "\n        All match the given arguments equally well";
            Err(binder_error(message))
        }
    }
}

/// `FunctionBinder::BindScalarFunction`: selects the overload, folds calls with a NULL
/// argument into a NULL constant, and casts the arguments to the parameter types. `fold`
/// evaluates a foldable argument (`ExpressionExecutor::TryEvaluateScalar`).
pub fn bind_scalar_function(
    set: &ScalarFunctionSet,
    mut children: Vec<BoundExpr>,
    is_operator: bool,
    fold: &dyn Fn(&BoundExpr) -> Option<Value>,
) -> Result<BoundExpr> {
    let arguments: Vec<LogicalType> = children.iter().map(expression_return_type).collect();
    let function = select_function(set, &arguments)?;
    // if any of the arguments are NULL, the function is replaced with a NULL constant
    if children.iter().any(|c| c.return_type.id() == LogicalTypeId::SqlNull) {
        return Ok(BoundExpr::constant(Value::null(function.return_type.clone())));
    }
    for child in &mut children {
        if let BoundExprKind::Constant(value) = &child.kind {
            if value.is_null() {
                return Ok(BoundExpr::constant(Value::null(function.return_type.clone())));
            }
            continue;
        }
        if !child.is_foldable() {
            continue;
        }
        // an argument that cannot be evaluated stays, so that its error surfaces at execution
        if let Some(value) = fold(child) {
            if value.is_null() {
                return Ok(BoundExpr::constant(Value::null(function.return_type.clone())));
            }
            let alias = std::mem::take(&mut child.alias);
            *child = BoundExpr::constant(value).with_alias(alias);
        }
    }
    let children = children
        .into_iter()
        .zip(&function.arguments)
        .map(|(child, parameter)| child.cast_to(parameter, false))
        .collect();
    let return_type = function.return_type.clone();
    Ok(BoundExpr::new(
        BoundExprKind::Function {
            function,
            children,
            is_operator,
        },
        return_type,
    ))
}

/// `FunctionBinder::BindAggregateFunction`: selects the overload and casts the arguments. ANY
/// and DECIMAL parameters take the argument as it is, and may decide the return type.
pub fn bind_aggregate_function(
    set: &AggregateFunctionSet,
    children: Vec<BoundExpr>,
    filter: Option<BoundExpr>,
    distinct: bool,
) -> Result<BoundAggregate> {
    let arguments: Vec<LogicalType> = children.iter().map(expression_return_type).collect();
    let function: Arc<AggregateFunction> = select_overload(&set.name, &set.functions, |f| &f.arguments, &arguments)?;
    let children: Vec<BoundExpr> = children
        .into_iter()
        .zip(&function.arguments)
        .map(|(child, parameter)| {
            let takes_input_type = parameter.id() == LogicalTypeId::Any
                || (parameter.id() == LogicalTypeId::Decimal && !parameter.has_params());
            if takes_input_type {
                child
            } else {
                child.cast_to(parameter, false)
            }
        })
        .collect();
    let return_type = function.resolve_return_type(children.first().map(|c| &c.return_type));
    Ok(BoundAggregate {
        function,
        return_type,
        children,
        filter,
        distinct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::BuiltinFunctions;

    #[test]
    fn picks_the_cheapest_overload() {
        let add = BuiltinFunctions::get().scalar("+").unwrap();
        let literal = LogicalType::integer_literal(LogicalTypeId::Integer, 2, false);
        let f = select_function(add, &[LogicalType::INTEGER, literal.clone()]).unwrap();
        assert_eq!(f.return_type, LogicalType::INTEGER);
        let f = select_function(add, &[LogicalType::TINYINT, literal]).unwrap();
        assert_eq!(f.return_type, LogicalType::TINYINT);
        let f = select_function(add, &[LogicalType::INTEGER, LogicalType::BIGINT]).unwrap();
        assert_eq!(f.return_type, LogicalType::BIGINT);
        let err = select_function(add, &[LogicalType::VARCHAR, LogicalType::INTEGER]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Binder Error: No function matches the given name and argument types '+(VARCHAR, INTEGER)'. You might \
             need to add explicit type casts."
        );
    }
}
