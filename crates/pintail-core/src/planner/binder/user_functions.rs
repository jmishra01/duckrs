// Ported from duckdb/src/function/function_binder.cpp (`BindScalarFunction`, `VerifyNamedArgumentsAccepted`,
// `PlaceOption`, `BindTableFunction`) and duckdb/src/planner/binder/tableref/bind_table_function.cpp, as they bind
// the functions of duckdb/src/main/capi/v1/{scalar_function,table_function}-c.cpp @ rust-port-baseline
//
//! Binding calls of the functions a client registered ([`crate::function::udf`]).

use super::{Binder, BoundStatement, StatementReturnType};
use crate::function::aggregates::AggregateFunctionSet;
use crate::function::binder::{expression_return_type, select_function};
use crate::function::udf::{
    CopyToBindInput, ScalarUdfBindInput, ScalarUdfSet, TableUdfBindInput, TableUdfSet, UserCast, UserCopyFunction,
    UserCopyTo, UserFunctions,
};
use crate::function::{ScalarFunction, ScalarFunctionSet};
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::Value;
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};
use pintail_parser::ast::{CopyInfo, FunctionExpr, InsertStatement, InsertValues, TableRef};
use std::sync::Arc;

/// `MapCastFunction`: `child` cast to `target` by the registered `cast`, which runs on the vectors of the rows.
pub(crate) fn user_cast_expression(
    child: BoundExpr,
    cast: UserCast,
    target: &LogicalType,
    try_cast: bool,
) -> BoundExpr {
    fn unbound(_: &DataChunk, _: &mut Vector) -> Result<()> {
        Err(Error::new(
            ErrorKind::Internal,
            "a user-defined cast runs once it is bound",
        ))
    }
    let mut scalar = ScalarFunction::new("CAST", vec![child.return_type.clone()], target.clone(), unbound);
    let function = cast.function;
    scalar.bound = Some(Arc::new(move |arguments, result| {
        function.cast(arguments.column(0), result, arguments.len(), try_cast)
    }));
    BoundExpr::new(
        BoundExprKind::Function {
            function: Arc::new(scalar),
            children: vec![child],
            is_operator: false,
        },
        target.clone(),
    )
}

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

impl Binder<'_> {
    fn user_functions(&self) -> Option<&Arc<UserFunctions>> {
        self.session
            .as_ref()
            .and_then(|session| session.user_functions.as_ref())
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

    /// The type a client registered under `name` (`duckdb_register_logical_type`), which the system catalog has.
    pub(super) fn user_type(&self, name: &str) -> Option<LogicalType> {
        self.user_functions()?.user_type(name)
    }

    /// The types a client registered, with their names.
    pub(super) fn user_types(&self) -> Vec<(String, LogicalType)> {
        self.user_functions().map(|f| f.user_types()).unwrap_or_default()
    }

    /// `CastFunctionSet::GetCastFunction` when a client registered a cast from the type of `child` to `target`
    /// (`MapCastFunction`): the cast, as a call of the registered function. `Err(child)` when there is none.
    pub(super) fn bind_user_cast(
        &self,
        child: BoundExpr,
        target: &LogicalType,
        try_cast: bool,
    ) -> std::result::Result<BoundExpr, BoundExpr> {
        if child.return_type == *target {
            return Err(child);
        }
        let Some(cast) = self.user_functions().and_then(|f| f.cast(&child.return_type, target)) else {
            return Err(child);
        };
        Ok(user_cast_expression(child, cast, target, try_cast))
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
            Err(Error::new(
                ErrorKind::Internal,
                "a user-defined function runs once it is bound",
            ))
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
            let argument = &super::table_functions::named_argument(argument);
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
                    .map(|(option, _)| {
                        (
                            option.clone(),
                            pintail_common::string_util::similarity_rating(option, &lower),
                        )
                    })
                    .filter(|(_, score)| *score >= 0.8)
                    .collect();
                scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let similar: Vec<String> = scores.into_iter().take(3).map(|(option, _)| option).collect();
                return Err(binder_error(format!(
                    "Invalid named parameter \"{argument}\" for function {name}{}",
                    pintail_common::string_util::candidates_message(&similar, "Did you mean")
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
            expected_names: Vec::new(),
            expected_types: Vec::new(),
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

    /// `Binder::BindCopyTo` and `Binder::BindCopyFrom` with a copy function a client registered under the FORMAT
    /// of the COPY (`Catalog::GetEntry<CopyFunctionCatalogEntry>`); `None` when there is none.
    pub(super) fn bind_user_copy(&mut self, info: &CopyInfo) -> Option<Result<BoundStatement>> {
        if info.format.is_empty() {
            return None;
        }
        let function = self.user_functions()?.copy(&info.format)?;
        Some(if info.is_from {
            self.bind_user_copy_from(info, &function)
        } else {
            self.bind_user_copy_to(info, &function)
        })
    }

    /// The file of a COPY: its expression's value, or the path it names, expanded (`FileSystem::ExpandPath`).
    fn user_copy_path(&mut self, info: &CopyInfo) -> Result<String> {
        let path = match &info.file_path_expression {
            Some(expression) => {
                let value = self.constant_argument(expression)?;
                if value.logical_type().id() != LogicalTypeId::Varchar || value.is_null() {
                    return Err(binder_error(
                        "Unsupported parameter type for filename: expected e.g. TARGET 'file.parquet'",
                    ));
                }
                super::copy::text(&value)?
            }
            None => info.file_path.clone(),
        };
        let path = crate::execution::file_system::expand_path(&path);
        self.check_file_access(&path)?;
        Ok(path)
    }

    /// The options of a COPY by name (`CopyInfo::options`): each with its values, none for an option without one.
    fn user_copy_options(&mut self, info: &CopyInfo) -> Result<Vec<(String, Vec<Value>)>> {
        let bare: Vec<String> = info
            .parsed_options
            .iter()
            .filter(|(_, e)| e.is_none())
            .map(|(n, _)| n.to_lowercase())
            .chain(
                info.options
                    .iter()
                    .filter(|(_, l)| l.is_empty())
                    .map(|(n, _)| n.to_lowercase()),
            )
            .collect();
        Ok(self
            .copy_options(info)?
            .into_iter()
            .filter(|(name, _)| name != "format")
            .map(|(name, value)| {
                let values = if bare.contains(&name) { Vec::new() } else { vec![value] };
                (name, values)
            })
            .collect())
    }

    /// `Binder::BindCopyTo` with `CCopyToBind`: the function's bind sees the columns of the query and the options.
    fn bind_user_copy_to(&mut self, info: &CopyInfo, function: &UserCopyFunction) -> Result<BoundStatement> {
        let Some(copy_to) = &function.copy_to else {
            return Err(Error::not_implemented(format!(
                "COPY TO is not supported for FORMAT \"{}\"",
                info.format
            )));
        };
        let path = self.user_copy_path(info)?;
        let select = self.copy_to_select(info)?;
        let bound = self.bind_select_statement(&select)?;
        let names = super::copy::unique_names(&bound.names);
        let options = self.user_copy_options(info)?;
        let copy = copy_to.bind(CopyToBindInput {
            names: names.clone(),
            types: bound.types.clone(),
            options,
            connection_id: self.connection_id(),
        })?;
        let plan = LogicalOperator::new(
            LogicalOperatorKind::UserCopyTo {
                names,
                copy: UserCopyTo {
                    name: function.name.clone(),
                    path,
                    bound: copy,
                },
            },
            vec![bound.plan],
        );
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::ChangedRows,
        })
    }

    /// `Binder::BindCopyFrom` with `CCopyFromBind`: an INSERT into the table of the rows of the function's table
    /// function, which takes the file as its parameter and the options as its named parameters.
    fn bind_user_copy_from(&mut self, info: &CopyInfo, function: &UserCopyFunction) -> Result<BoundStatement> {
        if info.table.name.is_empty() {
            return Err(Error::new(
                ErrorKind::Parser,
                "COPY FROM requires a table name to be specified",
            ));
        }
        let Some(overload) = &function.copy_from else {
            return Err(Error::not_implemented(format!(
                "COPY FROM is not supported for FORMAT \"{}\"",
                info.format
            )));
        };
        let path = self.user_copy_path(info)?;
        let table = self
            .catalog
            .get_table(self.schema_of(&info.table)?.as_deref(), &info.table.name)?;
        // `BindInsertColumnList`
        let names: Vec<String> = if info.select_list.is_empty() {
            table.columns.iter().map(|c| c.name.clone()).collect()
        } else {
            info.select_list.clone()
        };
        let mut types = Vec::with_capacity(names.len());
        for name in &names {
            let Some(index) = table.column_index(name) else {
                return Err(binder_error(format!(
                    "Table \"{}\" does not have a column with name \"{name}\"",
                    table.name
                )));
            };
            types.push(table.columns[index].ty.clone());
        }
        // the INSERT, of rows that stand in for those of the function (`ColumnDataRef` `__copy_from`)
        let row: Vec<String> = types.iter().map(|ty| format!("NULL::{ty}")).collect();
        let select = super::table_functions::parse_select(
            &format!("SELECT * FROM (VALUES ({})) __copy_from", row.join(", ")),
            "Expected a single SELECT statement",
        )?;
        let mut insert = self.bind_insert(&InsertStatement {
            cte_map: Default::default(),
            table: info.table.clone(),
            table_alias: None,
            by_name: false,
            columns: names.clone(),
            values: InsertValues::Select(Box::new(select)),
            on_conflict: None,
            returning: Vec::new(),
        })?;
        // `CCopyFromBind`: the options are named parameters of the table function, and the file its parameter
        let mut named_parameters = Vec::new();
        for (option, values) in self.user_copy_options(info)? {
            let Some((parameter, _)) = overload
                .named_parameters
                .iter()
                .find(|(parameter, _)| parameter.eq_ignore_ascii_case(&option))
            else {
                return Err(binder_error(format!(
                    "'{option}' is not a supported option for copy function '{}'",
                    overload.name
                )));
            };
            let value = match values.as_slice() {
                [] => continue,
                [value] => value.clone(),
                [first, ..] => {
                    let ty = first.logical_type();
                    if values.iter().any(|v| v.logical_type() != ty) {
                        return Err(binder_error(format!(
                            "Cannot pass multiple values of different types for copy option '{option}'"
                        )));
                    }
                    Value::list(ty.clone(), values)?
                }
            };
            named_parameters.push((parameter.clone(), value));
        }
        let binding = overload.function.bind(TableUdfBindInput {
            parameters: vec![Value::varchar(path)],
            named_parameters,
            connection_id: self.connection_id(),
            expected_names: names,
            expected_types: types.clone(),
        })?;
        // `FindCopySource`: the stand-in rows give way to the function's scan
        fn replace(
            op: &mut LogicalOperator,
            scan: &mut Option<crate::function::udf::UserTableScan>,
            types: &[LogicalType],
        ) {
            if let LogicalOperatorKind::ExpressionGet {
                table_index,
                types: get_types,
                ..
            } = &op.kind
                && get_types == types
                && let Some(scan) = scan.take()
            {
                *op = LogicalOperator::new(
                    LogicalOperatorKind::UserTableScan {
                        table_index: *table_index,
                        types: types.to_vec(),
                        scan,
                    },
                    vec![],
                );
                return;
            }
            for child in &mut op.children {
                replace(child, scan, types);
            }
        }
        let mut scan = Some(crate::function::udf::UserTableScan {
            name: overload.name.clone(),
            scan: binding.scan,
            cardinality: binding.cardinality,
        });
        replace(&mut insert.plan, &mut scan, &types);
        if scan.is_some() {
            return Err(Error::new(
                ErrorKind::Internal,
                "Failed to find the source operator for COPY FROM",
            ));
        }
        Ok(insert)
    }
}
