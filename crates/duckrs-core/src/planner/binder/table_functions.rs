// Ported from duckdb/src/function/table/query_function.cpp,
// duckdb/src/function/table/repeat.cpp, duckdb/src/function/table/repeat_row.cpp,
// duckdb/src/function/table/unnest.cpp and duckdb/src/function/table/checkpoint.cpp @ rust-port-baseline
//
//! The table functions that are a query in disguise. C++ gives several of them a `bind_replace`
//! that turns the call into a subquery; the rest are the same here, because a subquery brings
//! LATERAL with it, which the scans of C++ handle through `in_out_function`.

use super::{Binder, binder_error};
use crate::execution::expression_executor::evaluate_scalar;
use crate::function::test_all_types::test_all_types;
use crate::planner::expression::BoundExpr;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    ComparisonOp, Expr, ExprKind, FunctionArgument, FunctionExpr, SelectStatement, Statement, TableRef, sql_identifier,
};
use duckrs_parser::parse_ast;

/// The "hack to make named parameters work" of `Binder::BindTableFunctionParameters`: `a = 1`
/// names a parameter as `a := 1` does.
pub(crate) fn named_argument(argument: &FunctionArgument) -> FunctionArgument {
    if argument.name.is_empty()
        && let ExprKind::Comparison {
            op: ComparisonOp::Equal,
            left,
            right,
        } = &argument.expr.kind
        && let ExprKind::ColumnRef(names) = &left.kind
        && let [name] = names.as_slice()
    {
        return FunctionArgument {
            name: name.clone(),
            expr: (**right).clone(),
        };
    }
    argument.clone()
}

pub(super) fn parse_select(sql: &str, message: &str) -> Result<SelectStatement> {
    let mut statements = parse_ast(sql)?;
    match (statements.pop(), statements.is_empty()) {
        (Some(Statement::Select(select)), true) => Ok(select),
        _ => Err(Error::new(ErrorKind::Parser, message)),
    }
}

/// `TableReference` of `query_function.cpp`, without the file paths, which come with Phase 11.
fn table_reference(name: &str) -> String {
    if name.contains(['"', '.']) {
        name.to_string()
    } else {
        sql_identifier(name)
    }
}

/// `UnionTablesQuery`: `FROM` the tables that `tables` names, one after the other.
fn union_tables_query(tables: &Value, by_name: bool) -> Result<String> {
    if tables.logical_type().id() == LogicalTypeId::Varchar {
        return Ok(format!("FROM {}", table_reference(&tables.try_to_string()?)));
    }
    let ValueData::List(items) = tables.data() else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Expected a table or a list with tables as input",
        ));
    };
    if items.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "Input list is empty"));
    }
    let mut froms = Vec::with_capacity(items.len());
    for item in items {
        if item.is_null() {
            return Err(binder_error("Cannot use NULL as function argument"));
        }
        froms.push(format!("FROM {}", table_reference(&item.try_to_string()?)));
    }
    let separator = if by_name { " UNION ALL BY NAME " } else { " UNION ALL " };
    Ok(froms.join(separator))
}

impl Binder<'_> {
    /// A table function's argument, which C++ evaluates when binding.
    pub(super) fn constant_argument(&mut self, expr: &Expr) -> Result<Value> {
        // `TableFunctionBinder::BindExpression`
        let mut subquery = false;
        duckrs_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e: &mut Expr| {
            subquery |= matches!(e.kind, duckrs_parser::ast::ExprKind::Subquery { .. });
        });
        if subquery {
            return Err(binder_error("Table function cannot contain subqueries"));
        }
        // a parameter of a table macro being created: its value is known when the macro is used
        let mut macro_parameter = false;
        duckrs_parser::transformer::visit_expr_mut(&mut expr.clone(), &mut |e: &mut Expr| {
            macro_parameter |= matches!(
                &e.kind,
                duckrs_parser::ast::ExprKind::Constant(literal) if literal.text == super::macros::CREATE_PARAMETER_MARK
            );
        });
        if macro_parameter {
            return Err(Error::new(
                ErrorKind::ParameterNotResolved,
                "Parameter types could not be resolved",
            ));
        }
        let mut binder = self.child_binder();
        binder.table_function_argument = true;
        binder.aggregate_error = "Table function cannot contain aggregates!";
        binder.window_error = "Table function cannot contain window functions!";
        let bound = binder.bind_expression(expr);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        // `ParameterNotResolvedException`: the statement binds again with the values when it is executed
        if self.parameters.as_ref().is_some_and(|p| p.values.is_none()) && super::macros::has_parameter(expr) {
            return Err(Error::new(
                ErrorKind::ParameterNotResolved,
                "Parameter types could not be resolved",
            ));
        }
        if !bound.is_foldable() {
            return Err(binder_error("Table function requires a constant parameter"));
        }
        evaluate_scalar(&bound)
    }

    /// Binds `select` in the place of the table function `name`.
    pub(super) fn bind_rewritten(
        &mut self,
        select: &SelectStatement,
        table_ref: &TableRef,
        name: &str,
    ) -> Result<LogicalOperator> {
        let mut binder = self.child_binder();
        binder.left_of_join = std::mem::take(&mut self.left_of_join);
        let bound = binder.bind_select_statement(select);
        self.left_of_join = std::mem::take(&mut binder.left_of_join);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let alias = if table_ref.alias.is_empty() {
            name.to_string()
        } else {
            table_ref.alias.clone()
        };
        // `BindTableFunctionInternal`: aliases past the function's columns are ignored
        let aliases = &table_ref.column_name_alias;
        let aliases = &aliases[..aliases.len().min(bound.names.len())];
        self.add_query_binding(alias, bound, aliases)
    }

    /// The table functions this module knows. `None` leaves `call` to the caller.
    pub(super) fn bind_rewritten_function(
        &mut self,
        function: &Expr,
        call: &FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<Option<LogicalOperator>> {
        if !call.name.qualification.is_empty() {
            return Ok(None);
        }
        let name = call.name.name.to_lowercase();
        let args: Vec<FunctionArgument> = call.args.iter().map(named_argument).collect();
        let sql = match name.as_str() {
            "unnest" => format!("SELECT {function} AS unnest"),
            // `SummaryTableFunction`: a row's values as text, then the row
            "summary" => {
                let [input] = args.as_slice() else {
                    return Ok(None);
                };
                let duckrs_parser::ast::ExprKind::Subquery { subquery, .. } = &input.expr.kind else {
                    return Ok(None);
                };
                let mut binder = self.child_binder();
                let bound = binder.bind_select_statement(subquery)?;
                let names: Vec<String> = (0..bound.names.len()).map(|i| format!("\"__summary_{i}\"")).collect();
                let text = names
                    .iter()
                    .map(|n| format!("coalesce({n}::VARCHAR, 'NULL')"))
                    .collect::<Vec<_>>()
                    .join(" || ', ' || ");
                let text = if text.is_empty() { "''".to_string() } else { text };
                let originals: Vec<String> = bound
                    .names
                    .iter()
                    .zip(&names)
                    .map(|(original, n)| format!("{n} AS \"{}\"", original.replace('"', "\"\"")))
                    .collect();
                let mut select = format!("SELECT '[' || {text} || ']' AS summary");
                for column in originals {
                    select += &format!(", {column}");
                }
                format!("{select} FROM ({subquery}) __summary({})", names.join(", "))
            }
            // the table macro of `default_table_functions.cpp`
            "duckdb_logs_parsed" => {
                let [log_type] = args.as_slice() else {
                    return Err(binder_error("duckdb_logs_parsed takes a log type"));
                };
                let t = &log_type.expr;
                format!(
                    "SELECT * EXCLUDE (message), UNNEST(CASE WHEN type ILIKE {t} THEN parse_duckdb_log_message({t}, \
                     message) END) FROM duckdb_logs(denormalized_table=1) WHERE type ILIKE {t}"
                )
            }
            "repeat" => {
                let [value, count] = args.as_slice() else {
                    return Err(binder_error("repeat requires a value and a number of repetitions"));
                };
                let repetitions = self.constant_argument(&count.expr)?;
                if repetitions.is_null() {
                    return Err(binder_error("Repeat second parameter cannot be NULL"));
                }
                if repetitions.as_i128().is_some_and(|n| n < 0) {
                    return Err(binder_error("Repeat second parameter cannot be be less than 0"));
                }
                format!("SELECT {} AS repeat FROM range({})", value.expr, count.expr)
            }
            "repeat_row" => {
                let mut columns = Vec::new();
                let mut rows = None;
                for argument in &args {
                    if argument.name.eq_ignore_ascii_case("num_rows") {
                        rows = Some(&argument.expr);
                    } else if argument.name.is_empty() {
                        // `TableFunctionBinder`: a name is read as its text
                        let mut expr = argument.expr.clone();
                        duckrs_parser::transformer::visit_expr_mut(&mut expr, &mut |e: &mut Expr| {
                            if let duckrs_parser::ast::ExprKind::ColumnRef(names) = &e.kind {
                                let text = names.join(".");
                                *e = Expr::constant(duckrs_parser::ast::Literal::string(text));
                            }
                        });
                        columns.push(format!("{expr} AS column{}", columns.len()));
                    } else {
                        return Err(binder_error(format!(
                            "Invalid named parameter \"{}\" for function repeat_row",
                            argument.name
                        )));
                    }
                }
                // num_rows is a required named parameter: without it no overload matches
                let Some(rows) = rows else {
                    let mut types = Vec::new();
                    for argument in &args {
                        types.push(
                            crate::function::binder::expression_return_type(&BoundExpr::constant(
                                self.constant_argument(&argument.expr)?,
                            ))
                            .to_string(),
                        );
                    }
                    return Err(binder_error(format!(
                        "No function matches the given name and argument types 'repeat_row({})'. You might need \
                         to add explicit type casts.",
                        types.join(", ")
                    )));
                };
                // `RepeatRowFunctionBind`
                let count = self.constant_argument(rows)?;
                if count.is_null() {
                    return Err(binder_error("num_rows should be an integer value >= 0"));
                }
                if count.as_i128().is_some_and(|n| n < 0) {
                    return Err(binder_error("num_rows cannot be less than zero"));
                }
                if columns.is_empty() {
                    return Err(binder_error("repeat_rows requires at least one column to be specified"));
                }
                format!("SELECT {} FROM range({rows})", columns.join(", "))
            }
            "query" => {
                let [argument] = args.as_slice() else {
                    return Err(binder_error("query requires a query string"));
                };
                let query = self.constant_argument(&argument.expr)?;
                if query.is_null() {
                    return Err(binder_error("Cannot use NULL as function argument"));
                }
                let select = parse_select(&query.try_to_string()?, "Expected a single SELECT statement")?;
                return self.bind_rewritten(&select, table_ref, &name).map(Some);
            }
            "query_table" => {
                let (tables, by_name) = match args.as_slice() {
                    [tables] => (self.constant_argument(&tables.expr)?, Value::boolean(false)),
                    [tables, by_name] => (
                        self.constant_argument(&tables.expr)?,
                        self.constant_argument(&by_name.expr)?,
                    ),
                    _ => return Err(binder_error("query_table requires a table or a list of tables")),
                };
                if tables.is_null() || by_name.is_null() {
                    return Err(binder_error("Cannot use NULL as function argument"));
                }
                let by_name = matches!(by_name.data(), ValueData::Bool(true));
                let query = union_tables_query(&tables, by_name)?;
                let select = parse_select(&query, "Expected a table or a list with tables as input")?;
                return self.bind_rewritten(&select, table_ref, &name).map(Some);
            }
            // `GlobFunction`
            "glob" => {
                let [pattern] = args.as_slice() else {
                    return Err(binder_error("glob requires a pattern"));
                };
                let value = self.constant_argument(&pattern.expr)?;
                // a list of patterns: the files of each, in turn
                let patterns = match value.data() {
                    ValueData::List(items) => items.iter().map(Value::try_to_string).collect::<Result<Vec<_>>>()?,
                    _ => vec![value.try_to_string()?],
                };
                let mut files = Vec::new();
                for pattern in &patterns {
                    self.check_file_access(pattern)?;
                    files.extend(
                        crate::execution::file_system::glob(pattern)
                            .iter()
                            .map(|file| format!("'{}'", file.replace('\'', "''"))),
                    );
                }
                format!("SELECT unnest([{}]::VARCHAR[]) AS file", files.join(", "))
            }
            // `ExecuteSqlTableFunction::Bind`: the statement of the document, run as a subquery
            "json_execute_serialized_sql" => {
                let [serialized] = args.as_slice() else {
                    return Err(binder_error(format!(
                        "No function matches the given name and argument types 'json_execute_serialized_sql({})'",
                        args.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
                    )));
                };
                let value = self.constant_argument(&serialized.expr)?;
                let select = crate::function::json_serialize::execute_serialized_statement(&value, false)?;
                return self.bind_rewritten(&select, table_ref, &name).map(Some);
            }
            "test_all_types" => return self.bind_test_all_types(&args, table_ref).map(Some),
            "test_vector_types" => return self.bind_test_vector_types(&args, table_ref).map(Some),
            // the rows of an `Appender` that is flushing (`ClientContext::Append`)
            "__internal_appender_chunks" => {
                let Some((names, types, chunks)) = self.session.as_ref().and_then(|session| session.appended.clone())
                else {
                    return Ok(None);
                };
                return self.bind_chunks(table_ref, &name, names, types, chunks).map(Some);
            }
            _ => return Ok(None),
        };
        let select = parse_select(&sql, "Expected a single SELECT statement")?;
        self.bind_rewritten(&select, table_ref, &name).map(Some)
    }

    /// `ColumnDataRef`: rows the client holds, read as a table called `name` (or the alias).
    pub(super) fn bind_chunks(
        &mut self,
        table_ref: &TableRef,
        name: &str,
        names: Vec<String>,
        types: Vec<LogicalType>,
        chunks: std::sync::Arc<Vec<duckrs_common::vector::DataChunk>>,
    ) -> Result<LogicalOperator> {
        let alias = if table_ref.alias.is_empty() {
            name.to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ChunkGet {
                table_index,
                types,
                chunks,
            },
            vec![],
        ))
    }

    /// `TestVectorTypesBind` and `TestVectorTypesInit`: a column `test_vector`, `test_vector2`, ...
    /// of the type of each argument, in chunks of each kind of vector.
    fn bind_test_vector_types(&mut self, args: &[FunctionArgument], table_ref: &TableRef) -> Result<LogicalOperator> {
        let mut types = Vec::new();
        let mut all_flat = false;
        for argument in args {
            if argument.name.is_empty() {
                let ty = self.constant_argument(&argument.expr)?.logical_type().clone();
                crate::function::test_vector_types::check_type(&ty)?;
                types.push(ty);
            } else if argument.name.eq_ignore_ascii_case("all_flat") {
                let value = self.constant_argument(&argument.expr)?;
                if value.is_null() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Cannot use NULL as argument for all_flat",
                    ));
                }
                all_flat = matches!(value.data(), ValueData::Bool(true));
            } else {
                return Err(binder_error(format!(
                    "Invalid named parameter \"{}\" for function test_vector_types",
                    argument.name
                )));
            }
        }
        if types.is_empty() {
            return Err(binder_error("test_vector_types requires at least one column"));
        }
        let chunks = crate::function::test_vector_types::test_vector_chunks(&types, all_flat)?;
        let alias = if table_ref.alias.is_empty() {
            "test_vector_types".to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = (0..types.len())
            .map(|i| {
                if i == 0 {
                    "test_vector".to_string()
                } else {
                    format!("test_vector{}", i + 1)
                }
            })
            .collect();
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ChunkGet {
                table_index,
                types,
                chunks: std::sync::Arc::new(chunks),
            },
            vec![],
        ))
    }

    /// `TestAllTypesBind`: a row of minimums, one of maximums and one of NULLs.
    fn bind_test_all_types(&mut self, args: &[FunctionArgument], table_ref: &TableRef) -> Result<LogicalOperator> {
        let (mut large_enum, mut large_bignum) = (false, false);
        for argument in args {
            let target = match argument.name.to_lowercase().as_str() {
                "use_large_enum" => &mut large_enum,
                "use_large_bignum" => &mut large_bignum,
                _ => return Err(binder_error("test_all_types only takes named parameters")),
            };
            let value = self.constant_argument(&argument.expr)?;
            if value.is_null() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Cannot use NULL as argument for {}", argument.name.to_lowercase()),
                ));
            }
            *target = matches!(value.data(), ValueData::Bool(true));
        }
        let columns = test_all_types(large_enum, large_bignum)?;
        let alias = if table_ref.alias.is_empty() {
            "test_all_types".to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = columns.iter().map(|c| c.name.clone()).collect();
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let types: Vec<LogicalType> = columns.iter().map(|c| c.min.logical_type().clone()).collect();
        let row = |value: &dyn Fn(usize) -> Value| (0..columns.len()).map(|i| BoundExpr::constant(value(i))).collect();
        let expressions: Vec<Vec<BoundExpr>> = vec![
            row(&|i| columns[i].min.clone()),
            row(&|i| columns[i].max.clone()),
            row(&|i| Value::null(types[i].clone())),
        ];
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ExpressionGet {
                table_index,
                types,
                expressions,
            },
            vec![],
        ))
    }
}
