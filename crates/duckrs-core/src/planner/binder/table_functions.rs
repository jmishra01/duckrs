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
fn named_argument(argument: &FunctionArgument) -> FunctionArgument {
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
        let mut binder = self.child_binder();
        let bound = binder.bind_expression(expr);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
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
        self.add_query_binding(alias, bound, &table_ref.column_name_alias)
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
                        columns.push(format!("{} AS column{}", argument.expr, columns.len()));
                    } else {
                        return Err(binder_error(format!(
                            "Invalid named parameter \"{}\" for function repeat_row",
                            argument.name
                        )));
                    }
                }
                let Some(rows) = rows else {
                    return Err(binder_error("repeat_rows requires num_rows to be specified"));
                };
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
            "test_all_types" => return self.bind_test_all_types(&args, table_ref).map(Some),
            _ => return Ok(None),
        };
        let select = parse_select(&sql, "Expected a single SELECT statement")?;
        self.bind_rewritten(&select, table_ref, &name).map(Some)
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
