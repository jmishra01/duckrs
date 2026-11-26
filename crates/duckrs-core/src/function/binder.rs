// Ported from duckdb/src/function/{function_binder,function_binder_result}.cpp and
// ExpressionBinder::GetExpressionReturnType @ rust-port-baseline
//
//! Overload resolution (`FunctionBinder`): every overload whose parameters all accept the
//! arguments by implicit cast is a match, and the cheapest match wins. Only positional
//! arguments are supported so far; named arguments, varargs and templates come later.

use super::aggregates::AggregateKind;
use super::aggregates::{AggregateFunction, AggregateFunctionSet};
use super::cast_rules::implicit_cast;
use super::holistic::check_quantile_parameter;
use super::nested::bind_struct_pack;
use super::{ScalarFunction, ScalarFunctionSet};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

/// `ExpressionBinder::GetExpressionReturnType`: constants bind as literal types, which cast
/// more cheaply than their own type.
pub fn expression_return_type(expr: &BoundExpr) -> LogicalType {
    if let BoundExprKind::Constant(value) = &expr.kind {
        // a collated string is not a literal: its collation counts (`GetExpressionReturnType`)
        if expr.return_type.id() == LogicalTypeId::Varchar && expr.return_type.collation().is_none() {
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
    let mut has_unknown_argument = false;
    for (argument, parameter) in arguments.iter().zip(parameters) {
        if argument.id() == LogicalTypeId::Unknown {
            // a parameter of a statement being prepared takes any type
            has_unknown_argument = true;
            continue;
        }
        let cast_cost = implicit_cast(argument, parameter);
        if cast_cost < 0 {
            return None;
        }
        cost += cast_cost;
    }
    // with a parameter of an unknown type, the cost is 0
    Some(if has_unknown_argument { 0 } else { cost })
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
        // a macro in C++
        0 if name == "split_part" => Err(binder_error(format!(
            "Macro {name}() does not support the supplied arguments."
        ))),
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
    // `IntegerNegateBind`: the negation of a type's minimum is of the next wider type
    if set.name == "-"
        && let [child] = children.as_mut_slice()
        && let Some(value) = match &child.kind {
            BoundExprKind::Constant(value) => Some(value.clone()),
            // `TryGetConstant`: a foldable argument counts as the constant it folds to
            _ if child.is_foldable() => fold(child),
            _ => None,
        }
        && !value.is_null()
    {
        let (minimum, wider) = match child.return_type.id() {
            LogicalTypeId::TinyInt => (i128::from(i8::MIN), LogicalType::SMALLINT),
            LogicalTypeId::SmallInt => (i128::from(i16::MIN), LogicalType::INTEGER),
            LogicalTypeId::Integer => (i128::from(i32::MIN), LogicalType::BIGINT),
            LogicalTypeId::BigInt => (i128::from(i64::MIN), LogicalType::HUGEINT),
            _ => (0, LogicalType::SQLNULL),
        };
        if wider.id() != LogicalTypeId::SqlNull && value.as_i128() == Some(minimum) {
            *child = child.clone().cast_to(&wider, false);
        }
    }
    let arguments: Vec<LogicalType> = children.iter().map(expression_return_type).collect();
    let function = select_function(set, &arguments)?;
    check_scalar_constant_parameters(&function.name, &children)?;
    // `FunctionNullHandling::SPECIAL_HANDLING`: the function decides what a NULL argument means
    let special_nulls = matches!(
        function.name.as_str(),
        "string_split"
            | "str_split"
            | "string_to_array"
            | "split"
            | "stats"
            | "__internal_barrier"
            | "vector_type"
            | "regexp_extract"
            | "parse_filename"
            | "parse_path"
            | "parse_dirname"
            | "parse_dirpath"
    );
    if special_nulls {
        let function = match function.resolve_types {
            Some(resolve_types) => {
                let types: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
                Arc::new(resolve_types(&function, &types)?)
            }
            None => function,
        };
        let children = children
            .into_iter()
            .zip(&function.arguments)
            .map(|(child, parameter)| child.cast_to(parameter, false))
            .collect();
        let return_type = function.return_type.clone();
        return Ok(BoundExpr::new(
            BoundExprKind::Function {
                function,
                children,
                is_operator,
            },
            return_type,
        ));
    }
    // if any of the arguments are NULL, the function is replaced with a NULL constant
    if children.iter().any(|c| c.return_type.id() == LogicalTypeId::SqlNull) {
        // untyped NULLs never go through type resolution
        // (`||` and `constant_or_null` give ANY until their types are resolved)
        let incomplete = function.return_type.id() == LogicalTypeId::Decimal && !function.return_type.has_params()
            || matches!(function.name.as_str(), "||" | "constant_or_null");
        return Ok(BoundExpr::constant(if incomplete {
            Value::sql_null()
        } else {
            Value::null(function.return_type.clone())
        }));
    }
    let function = match function.resolve_types {
        Some(resolve_types) => {
            let types: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
            Arc::new(resolve_types(&function, &types)?)
        }
        None => function,
    };
    for child in &mut children {
        if let BoundExprKind::Constant(value) = &child.kind {
            if value.is_null() {
                return Ok(BoundExpr::constant(Value::null(folded_type(
                    &function.return_type,
                    &children,
                ))));
            }
            continue;
        }
        if !child.is_foldable() {
            continue;
        }
        // an argument that cannot be evaluated stays, so that its error surfaces at execution
        if let Some(value) = fold(child) {
            if value.is_null() {
                return Ok(BoundExpr::constant(Value::null(folded_type(
                    &function.return_type,
                    &children,
                ))));
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

/// `BindFunctionInput::GetConstant`: the value of a parameter of `function` that has to be a
/// constant. `None` while the parameter is an unresolved prepared statement parameter.
pub(super) fn constant_parameter(
    function: &str,
    child: &BoundExpr,
    at: usize,
    accept_null: bool,
) -> Result<Option<Value>> {
    if child.return_type.id() == LogicalTypeId::Unknown {
        return Ok(None);
    }
    if !child.is_foldable() {
        return Err(binder_error(format!(
            "Argument #{} in function {function} must be a constant expression",
            at + 1
        )));
    }
    let value = evaluate_scalar(child)?;
    if !accept_null && value.is_null() {
        return Err(binder_error(format!(
            "Argument #{} in function {function} must not be NULL",
            at + 1
        )));
    }
    Ok(Some(value))
}

thread_local! {
    /// How many TRY expressions are being bound around the current one.
    static TRY_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Binds `f` inside of a TRY: the errors that C++ raises when the expression's state is
/// initialized, which a TRY catches, are left to the execution.
pub fn inside_try<T>(f: impl FnOnce() -> T) -> T {
    TRY_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let result = f();
    TRY_DEPTH.with(|depth| depth.set(depth.get() - 1));
    result
}

/// The type a call folded to NULL has: a VARCHAR keeps the collation of its arguments, as
/// `PropagateReturnCollation` gives the call itself.
fn folded_type(return_type: &LogicalType, children: &[BoundExpr]) -> LogicalType {
    if return_type.id() != LogicalTypeId::Varchar || return_type.alias().is_some() || return_type.collation().is_some()
    {
        return return_type.clone();
    }
    children
        .iter()
        .find_map(|c| {
            (c.return_type.id() == LogicalTypeId::Varchar)
                .then(|| c.return_type.collation())
                .flatten()
        })
        .map_or_else(|| return_type.clone(), LogicalType::varchar_collation)
}

/// The bind callbacks of the scalar functions with a parameter that has to be constant.
fn check_scalar_constant_parameters(name: &str, children: &[BoundExpr]) -> Result<()> {
    // `ParseRegexOptions` on the options of the regex functions
    let options = match (name, children.len()) {
        ("regexp_matches" | "regexp_full_match", 3) => Some(2),
        ("regexp_replace" | "regexp_extract" | "regexp_extract_all", 4) => Some(3),
        ("regexp_matches" | "regexp_full_match" | "regexp_replace" | "regexp_extract" | "regexp_extract_all", _) => {
            None
        }
        _ => return Ok(()),
    };
    let mut option_text = None;
    if let Some(options) = options
        && let Some(value) = constant_parameter(name, &children[options], options, true)?
    {
        if value.is_null() {
            return Err(duckrs_common::Error::new(
                duckrs_common::ErrorKind::InvalidInput,
                "Regex options field must not be NULL",
            ));
        }
        option_text = value.try_to_string().ok();
    }
    // a constant pattern compiles at bind time
    if let Some(pattern) = children.get(1)
        && TRY_DEPTH.with(|depth| depth.get() == 0)
        && pattern.is_foldable()
        && pattern.return_type.id() != LogicalTypeId::Unknown
        && let Ok(value) = evaluate_scalar(pattern)
        && !value.is_null()
        && let Ok(text) = value.try_to_string()
    {
        crate::function::regexp::check_constant_pattern(&text, option_text.as_deref())?;
    }
    Ok(())
}

/// The bind callbacks of the aggregates with a parameter that has to be constant.
fn check_constant_parameters(name: &str, children: &mut [BoundExpr]) -> Result<()> {
    match (name, children) {
        // `StringAggBind`: of VARCHARs, to which anything else is cast
        ("string_agg" | "listagg" | "group_concat", [input]) => {
            *input = input.clone().cast_to(&LogicalType::VARCHAR, false);
            Ok(())
        }
        ("string_agg" | "listagg" | "group_concat", [input, separator]) => {
            *input = input.clone().cast_to(&LogicalType::VARCHAR, false);
            if constant_parameter(name, separator, 1, true)?.is_some_and(|value| value.is_null()) {
                *input = BoundExpr::constant(Value::null(LogicalType::VARCHAR));
            }
            Ok(())
        }
        ("approx_top_k", [_, k]) => {
            if k.return_type.id() != LogicalTypeId::Unknown && !k.is_foldable() {
                return Err(binder_error(
                    "The \"k\" argument in function \"approx_top_k\" must be a constant expression".to_string(),
                ));
            }
            Ok(())
        }
        ("bitstring_agg", [_, min, max]) => {
            constant_parameter(name, min, 1, true)?;
            constant_parameter(name, max, 2, true)?;
            Ok(())
        }
        // `BindApproxQuantile` + `CheckApproxQuantile`: fractions in [0, 1], kept as FLOAT
        ("approx_quantile", [input, parameter]) => {
            // `GetNonNullConstant`
            let Some(value) = constant_parameter(name, parameter, 1, false)? else {
                return Ok(());
            };
            let as_float = |v: &Value| -> Result<f32> {
                if v.is_null() {
                    return Err(binder_error(
                        "APPROXIMATE QUANTILE parameter cannot be NULL".to_string(),
                    ));
                }
                let cast = crate::execution::parquet::cast_value(v, &LogicalType::FLOAT)?;
                let ValueData::Float(q) = cast.data() else {
                    return Err(binder_error(
                        "APPROXIMATE QUANTILE parameter cannot be NULL".to_string(),
                    ));
                };
                if q.is_nan() {
                    return Err(binder_error("APPROXIMATE QUANTILE parameter cannot be NaN".to_string()));
                }
                if !(0.0..=1.0).contains(q) {
                    return Err(binder_error(
                        "APPROXIMATE QUANTILE can only take parameters in range [0, 1]".to_string(),
                    ));
                }
                Ok(*q)
            };
            let list = matches!(parameter.return_type.id(), LogicalTypeId::List | LogicalTypeId::Array);
            let fractions = match value.data() {
                ValueData::List(items) if list => {
                    let floats = items
                        .iter()
                        .map(|i| as_float(i).map(Value::float))
                        .collect::<Result<Vec<_>>>()?;
                    Value::list(LogicalType::FLOAT, floats)?
                }
                _ => Value::float(as_float(&value)?),
            };
            *parameter = BoundExpr::constant(fractions);
            let target = super::holistic::approx_quantile_type(&expression_return_type(input), list)?;
            *input = input.clone().cast_to(&target, false);
            Ok(())
        }
        // quantile_cont takes its fraction, quantile_disc has an overload without one (for deserializing)
        (_, arguments @ [_]) | (_, arguments @ [_, _, _, ..])
            if name == "quantile_cont" || (arguments.len() > 2 && matches!(name, "quantile_disc" | "quantile")) =>
        {
            let types: Vec<String> = arguments
                .iter()
                .map(|c| expression_return_type(c).to_string())
                .collect();
            Err(binder_error(format!(
                "No function matches the given name and argument types '{name}({})'. You might need to add \
                 explicit type casts.",
                types.join(", ")
            )))
        }
        ("quantile_disc" | "quantile", [_]) => Err(binder_error(
            "QUANTILE requires a range argument between [0, 1]".to_string(),
        )),
        // the fraction is a number or a list of them: no overload takes a STRUCT; and quantile_cont has
        // no INTERVAL overload (`GetContinuousQuantileTypes`)
        ("quantile_cont" | "quantile_disc" | "quantile", [input, parameter])
            if matches!(parameter.return_type.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple)
                || (name == "quantile_cont"
                    && (input.return_type.is_nested()
                        || matches!(
                            input.return_type.id(),
                            LogicalTypeId::Interval | LogicalTypeId::Varchar | LogicalTypeId::Blob
                        ))) =>
        {
            Err(binder_error(format!(
                "No function matches the given name and argument types '{name}({}, {})'. You might need to add \
                 explicit type casts.",
                expression_return_type(input),
                expression_return_type(parameter)
            )))
        }
        ("quantile_cont" | "quantile_disc" | "quantile", [_, parameter]) => {
            match constant_parameter(name, parameter, 1, false)? {
                Some(value) => check_quantile_parameter(&value),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// `BindReservoirQuantile`: the quantile (0.5 by default) and the sample size (8192 by default) are
/// constants. duckrs keeps every row, so the quantile is exact and the sample size only checked.
fn reservoir_quantile_parameters(children: &mut Vec<BoundExpr>) -> Result<()> {
    const NAME: &str = "reservoir_quantile";
    if children.len() == 1 {
        children.push(BoundExpr::constant(Value::double(0.5)));
    }
    if children.len() == 3 {
        let size = children.pop().expect("three arguments");
        if let Some(value) = constant_parameter(NAME, &size, 2, false)? {
            let size = crate::execution::parquet::cast_value(&value, &LogicalType::INTEGER)?
                .as_i128()
                .unwrap_or(0);
            if size <= 0 {
                return Err(binder_error(
                    "Size of the RESERVOIR_QUANTILE sample must be bigger than 0".to_string(),
                ));
            }
        }
    }
    let [_, quantile] = children.as_slice() else {
        return Ok(());
    };
    // `CheckReservoirQuantile`
    let Some(value) = constant_parameter(NAME, quantile, 1, true)? else {
        return Ok(());
    };
    let check = |value: &Value| -> Result<()> {
        if value.is_null() {
            return Err(binder_error(
                "RESERVOIR_QUANTILE QUANTILE parameter cannot be NULL".to_string(),
            ));
        }
        let cast = crate::execution::parquet::cast_value(value, &LogicalType::DOUBLE)?;
        let ValueData::Double(quantile) = *cast.data() else {
            return Ok(());
        };
        if quantile.is_nan() {
            return Err(binder_error("RESERVOIR_QUANTILE parameter cannot be NaN".to_string()));
        }
        if !(0.0..=1.0).contains(&quantile) {
            return Err(binder_error(
                "RESERVOIR_QUANTILE can only take parameters in the range [0, 1]".to_string(),
            ));
        }
        Ok(())
    };
    match value.data() {
        ValueData::List(items) => items.iter().try_for_each(check),
        _ => check(&value),
    }
}

/// The `T` and `LIST(T)` of `histogram(arg T, bins LIST(T))`: the type both have in common, and DOUBLE for a
/// DECIMAL (`GetHistogramBinFunction`).
fn histogram_bin_arguments(name: &str, mut children: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
    let arg = expression_return_type(&children[0]);
    let bins = expression_return_type(&children[1]);
    let element = match bins.id() {
        LogicalTypeId::List | LogicalTypeId::Array => bins.child_type().cloned(),
        LogicalTypeId::SqlNull => Some(LogicalType::SQLNULL),
        _ => None,
    };
    // a VARCHAR next to a type that is not a string is no type both have, as a template deduces it
    let varchar = |t: &LogicalType| t.id() == LogicalTypeId::Varchar;
    let stringy = |t: &LogicalType| {
        matches!(
            t.id(),
            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral | LogicalTypeId::SqlNull | LogicalTypeId::Enum
        )
    };
    let common = element
        .filter(|e| (stringy(e) || !varchar(&arg)) && (stringy(&arg) || !varchar(e)))
        .and_then(|e| super::combine_types::try_max_logical_type(&arg, &e));
    let Some(mut common) = common.map(|t| super::combine_types::normalize_type(&t)) else {
        let deduction = match bins.child_type() {
            Some(element) => format!(
                "\n\tCannot deduce template type 'T', it was inferred to be:\n  - '{arg}', from first occurrence\n  - \
                 '{element}', which is incompatible with the previously inferred type"
            ),
            None => String::new(),
        };
        return Err(binder_error(format!(
            "No function matches the given name and argument types '{name}({arg}, {bins})'. You might need to add \
             explicit type casts.{deduction}"
        )));
    };
    if common.id() == LogicalTypeId::Decimal {
        common = LogicalType::DOUBLE;
    }
    if common.id() == LogicalTypeId::SqlNull {
        common = LogicalType::INTEGER;
    }
    let list = LogicalType::list(common.clone());
    let bins = children.pop().expect("two arguments");
    let arg = children.pop().expect("two arguments");
    Ok(vec![arg.cast_to(&common, false), bins.cast_to(&list, false)])
}

/// `FunctionBinder::BindAggregateFunction`: selects the overload and casts the arguments. ANY
/// and DECIMAL parameters take the argument as it is, and may decide the return type.
pub fn bind_aggregate_function(
    set: &AggregateFunctionSet,
    children: Vec<BoundExpr>,
    filter: Option<BoundExpr>,
    distinct: bool,
) -> Result<BoundAggregate> {
    let mut children = children;
    if set.name == "reservoir_quantile" {
        reservoir_quantile_parameters(&mut children)?;
    }
    check_constant_parameters(&set.name, &mut children)?;
    // `GetAverageAggregate`'s stub of TIMESTAMP_S, TIMESTAMP_MS and DATE: averaged as a TIMESTAMP,
    // which holds every value they have, rather than widened to TIMESTAMP_NS
    if matches!(set.name.as_str(), "avg" | "mean")
        && let [input] = children.as_mut_slice()
        && matches!(
            input.return_type.id(),
            LogicalTypeId::TimestampSec | LogicalTypeId::TimestampMs | LogicalTypeId::Date
        )
    {
        *input = input.clone().cast_to(&LogicalType::TIMESTAMP, false);
    }
    if matches!(set.name.as_str(), "histogram" | "histogram_exact") && children.len() == 2 {
        children = histogram_bin_arguments(&set.name, children)?;
    }
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
    let mut return_type = function.resolve_return_type(children.first().map(|c| &c.return_type));
    let mut children = children;
    if let AggregateKind::Holistic(kind) = function.kind {
        let types: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
        return_type = kind.return_type(&return_type, &types);
        // the state keeps one value per row
        if children.len() > 1 {
            let names = (0..children.len()).map(|i| format!("a{i}")).collect();
            children = vec![bind_struct_pack(names, children)?];
        }
    }
    Ok(BoundAggregate {
        function,
        return_type,
        children,
        filter,
        distinct,
        order_by: Vec::new(),
        distinct_key: None,
        distinct_order: Vec::new(),
        proven_no_overflow: false,
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
