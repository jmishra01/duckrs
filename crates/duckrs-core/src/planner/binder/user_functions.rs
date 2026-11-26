// Ported from duckdb/src/function/function_binder.cpp (`BindScalarFunction`, `VerifyNamedArgumentsAccepted`,
// `PlaceOption`, `BindTableFunction`) and duckdb/src/planner/binder/tableref/bind_table_function.cpp, as they bind
// the functions of duckdb/src/main/capi/v1/{scalar_function,table_function}-c.cpp @ rust-port-baseline
//
//! Binding calls of the functions a client registered ([`crate::function::udf`]).

use super::Binder;
use crate::function::aggregates::AggregateFunctionSet;
use crate::function::binder::{expression_return_type, select_function};
use crate::function::udf::{ScalarUdfBindInput, ScalarUdfSet, TableUdfBindInput, TableUdfSet, UserFunctions};
use crate::function::{ScalarFunction, ScalarFunctionSet};
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{FunctionExpr, TableRef};
use std::sync::Arc;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

impl Binder<'_> {
    fn user_functions(&self) -> Option<&Arc<UserFunctions>> {
        self.session.as_ref().and_then(|session| session.user_functions.as_ref())
    }

    fn connection_id(&self) -> u64 {
        self.session.as_ref().map_or(0, |session| session.connection_id)
    }

    /// The user-defined scalar function `name`, if the client registered one.
    pub(super) fn user_scalar(&self, name: &str) -> Option<Arc<ScalarUdfSet>> {
        self.user_functions()?.scalar(name)
    }

    /// The user-defined aggregate function `name`, if the client registered one.
    pub(super) fn user_aggregate(&self, name: &str) -> Option<Arc<AggregateFunctionSet>> {
        self.user_functions()?.aggregate(name)
    }

    /// The user-defined table function `name`, if the client registered one.
    pub(super) fn user_table(&self, name: &str) -> Option<Arc<TableUdfSet>> {
        self.user_functions()?.table(name)
    }

    /// `FunctionBinder::BindScalarFunction` of a call of a user-defined scalar function: the overload the
    /// arguments match, NULL for a NULL argument unless the function handles NULLs itself, and then the function's
    /// own bind (`CScalarFunctionBind`).
    pub(super) fn bind_user_scalar(&mut self, function: &FunctionExpr, set: &ScalarUdfSet) -> Result<BoundExpr> {
        let children = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let arguments: Vec<LogicalType> = children.iter().map(expression_return_type).collect();
        // the parameters of a v1 function are positional-only: a name matches none of them
        if function.args.iter().any(|a| !a.name.is_empty()) {
            let shown: Vec<String> = function
                .args
                .iter()
                .zip(&arguments)
                .map(|(argument, ty)| {
                    if argument.name.is_empty() {
                        ty.to_string()
                    } else {
                        format!("{} := {ty}", argument.name)
                    }
                })
                .collect();
            return Err(binder_error(format!(
                "No function matches the given name and argument types '{}({})'. You might need to add explicit \
                 type casts.",
                set.name,
                shown.join(", ")
            )));
        }
        // each overload with as many parameters as there are arguments, its varargs repeated
        fn unbound(_: &DataChunk, _: &mut Vector) -> Result<()> {
            Err(Error::new(ErrorKind::Internal, "a user-defined function runs once it is bound"))
        }
        let mut candidates = ScalarFunctionSet::new(set.name.clone());
        let mut overloads = Vec::new();
        for overload in &set.functions {
            if let Some(parameters) = overload.parameters_for(children.len()) {
                candidates.add(ScalarFunction::new(
                    set.name.clone(),
                    parameters,
                    overload.return_type.clone(),
                    unbound,
                ));
                overloads.push(overload);
            }
        }
        let chosen = select_function(&candidates, &arguments)?;
        let index = candidates
            .functions
            .iter()
            .position(|f| Arc::ptr_eq(f, &chosen))
            .expect("the chosen overload is a candidate");
        let overload = overloads[index];
        let return_type = overload.return_type.clone();
        // `FunctionNullHandling::DEFAULT_NULL_HANDLING`: an argument that is NULL makes the call NULL
        if !overload.special_handling && children.iter().any(|c| c.return_type.id() == LogicalTypeId::SqlNull) {
            return Ok(BoundExpr::constant(Value::null(return_type)));
        }
        let children: Vec<BoundExpr> = children
            .into_iter()
            .zip(&chosen.arguments)
            .map(|(child, parameter)| {
                if parameter.id() == LogicalTypeId::Any {
                    child
                } else {
                    child.cast_to(parameter, false)
                }
            })
            .collect();
        let executor = overload.function.bind(&ScalarUdfBindInput {
            arguments: &children,
            connection_id: self.connection_id(),
        })?;
        let parameters = children.iter().map(|c| c.return_type.clone()).collect();
        let mut scalar = ScalarFunction::new(set.name.clone(), parameters, return_type.clone(), unbound);
        scalar.bound = Some(Arc::new(move |arguments, result| executor.execute(arguments, result)));
        scalar.volatile = overload.volatile;
        Ok(BoundExpr::new(
            BoundExprKind::Function {
                function: Arc::new(scalar),
                children,
                is_operator: false,
            },
            return_type,
        ))
    }

    /// `FunctionBinder::BindTableFunction` and `Binder::BindTableFunctionInternal` of a call of a user-defined
    /// table function: its positional parameters choose the overload and are cast to its parameters, its named ones
    /// are cast to their types, and the function's own bind gives the columns.
    pub(super) fn bind_user_table(
        &mut self,
        call: &FunctionExpr,
        set: &TableUdfSet,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let name = &set.name;
        let mut positional = Vec::new();
        let mut named: Vec<(String, Value)> = Vec::new();
        for argument in &call.args {
            let value = self.constant_argument(&argument.expr)?;
            if argument.name.is_empty() {
                positional.push(value);
            } else {
                named.push((argument.name.clone(), value));
            }
        }
        // `VerifyNamedArgumentsAccepted`: an overload with named parameters takes any name until it is chosen
        if let Some((argument, ..)) = named.first()
            && set.functions.iter().all(|f| f.named_parameters.is_empty())
        {
            return Err(binder_error(format!(
                "Invalid named parameter \"{argument}\" for function {name}\nFunction does not accept any named \
                 parameters."
            )));
        }
        let types: Vec<LogicalType> = positional
            .iter()
            .map(|value| expression_return_type(&BoundExpr::constant(value.clone())))
            .collect();
        let overload = crate::function::binder::select_table_overload(name, &set.functions, &types)?;
        // `CastToParameterType`
        let mut parameters = Vec::with_capacity(positional.len());
        for (value, parameter) in positional.into_iter().zip(&overload.arguments) {
            if parameter.id() == LogicalTypeId::Any || value.logical_type() == parameter {
                parameters.push(value);
            } else {
                parameters.push(crate::function::cast::cast_value(&value, parameter)?);
            }
        }
        // `PlaceOption`: each option once, cast to its type like an explicit cast
        let mut named_parameters: Vec<(String, Value)> = Vec::new();
        for (argument, value) in named {
            let Some((option, ty)) = overload
                .named_parameters
                .iter()
                .find(|(option, _)| option.eq_ignore_ascii_case(&argument))
            else {
                let lower = argument.to_lowercase();
                let mut scores: Vec<(String, f64)> = overload
                    .named_parameters
                    .iter()
                    .map(|(option, _)| (option.clone(), duckrs_common::string_util::similarity_rating(option, &lower)))
                    .filter(|(_, score)| *score >= 0.8)
                    .collect();
                scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let similar: Vec<String> = scores.into_iter().take(3).map(|(option, _)| option).collect();
                return Err(binder_error(format!(
                    "Invalid named parameter \"{argument}\" for function {name}{}",
                    duckrs_common::string_util::candidates_message(&similar, "Did you mean")
                )));
            };
            if named_parameters.iter().any(|(n, _)| n == option) {
                return Err(binder_error(format!(
                    "Named parameter \"{option}\" was passed more than once in function call to {name}"
                )));
            }
            let value = if ty.id() == LogicalTypeId::Any || value.logical_type() == ty {
                value
            } else {
                crate::function::cast::cast_value(&value, ty).map_err(|error| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Could not cast value {} to named parameter \"{option}\" of type {ty}: {}",
                            BoundExpr::constant(value.clone()),
                            error.message
                        ),
                    )
                })?
            };
            named_parameters.push((option.clone(), value));
        }
        let binding = overload.function.bind(TableUdfBindInput {
            parameters,
            named_parameters,
            connection_id: self.connection_id(),
        })?;
        if binding.types.is_empty() {
            return Err(binder_error(format!(
                "Table function \"{name}\" did not return any columns"
            )));
        }
        let alias = if table_ref.alias.is_empty() {
            name.clone()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, binding.names)?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            catalog: None,
            table_index,
            names,
            types: binding.types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(LogicalOperator::new(
            LogicalOperatorKind::UserTableScan {
                table_index,
                types: binding.types,
                scan: crate::function::udf::UserTableScan {
                    name: name.clone(),
                    scan: binding.scan,
                    cardinality: binding.cardinality,
                },
            },
            vec![],
        ))
    }
}
