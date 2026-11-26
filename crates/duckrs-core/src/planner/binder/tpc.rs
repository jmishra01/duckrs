// Ported from duckdb/extension/tpch/tpch_extension.cpp @ rust-port-baseline
//
//! The table functions of the `tpch` extension that only return text: `tpch_queries` and
//! `tpch_answers`. `CALL dbgen` creates and fills tables, so the client runs it
//! (`duckrs-main`'s `tpc` module).

use super::{Binder, binder_error};
use crate::planner::expression::BoundExpr;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{FunctionArgument, TableRef};

impl Binder<'_> {
    /// The text table functions of the TPC extensions, if `name` is one.
    pub(super) fn bind_tpc_function(
        &mut self,
        name: &str,
        args: &[FunctionArgument],
        table_ref: &TableRef,
    ) -> Result<Option<LogicalOperator>> {
        let (names, types, rows) = match name {
            "tpch_queries" => {
                let sf = self.query_scale_factor(name, args)?;
                let mut rows = Vec::with_capacity(duckrs_tpch::TPCH_QUERIES_COUNT as usize);
                for query in 1..=duckrs_tpch::TPCH_QUERIES_COUNT {
                    // `TPCHQueriesBindData::GetQuery`: without a scale factor the stored queries, unchanged
                    let text = match sf {
                        Some(sf) => duckrs_tpch::get_query_for_sf(query, sf)?,
                        None => duckrs_tpch::get_query(query)?.to_string(),
                    };
                    rows.push(vec![Value::integer(query), Value::varchar(text)]);
                }
                (
                    vec!["query_nr", "query"],
                    vec![LogicalType::INTEGER, LogicalType::VARCHAR],
                    rows,
                )
            }
            "tpch_answers" => {
                self.no_arguments(name, args)?;
                let mut rows = Vec::new();
                // `TPCHQueryAnswerFunction`: every query of a scale factor, then the next one
                for sf in duckrs_tpch::TPCH_ANSWER_SCALE_FACTORS {
                    for query in 1..=duckrs_tpch::TPCH_QUERIES_COUNT {
                        let answer = duckrs_tpch::get_answer(sf, query)?;
                        rows.push(vec![Value::integer(query), Value::double(sf), Value::varchar(answer)]);
                    }
                }
                (
                    vec!["query_nr", "scale_factor", "answer"],
                    vec![LogicalType::INTEGER, LogicalType::DOUBLE, LogicalType::VARCHAR],
                    rows,
                )
            }
            _ => return Ok(None),
        };
        let names = names.into_iter().map(str::to_string).collect();
        self.bind_constant_rows(table_ref, name, names, types, rows).map(Some)
    }

    /// `BindQueryScaleFactor`: the scale factor is either the positional argument or the named `sf`.
    fn query_scale_factor(&mut self, function: &str, args: &[FunctionArgument]) -> Result<Option<f64>> {
        let mut positional = None;
        let mut named = None;
        for argument in args {
            if argument.name.is_empty() {
                if positional.is_some() {
                    return Err(no_match(function, args));
                }
                positional = Some(&argument.expr);
            } else if argument.name.eq_ignore_ascii_case("sf") {
                named = Some(&argument.expr);
            } else {
                return Err(invalid_named_parameter(function, &argument.name));
            }
        }
        let sf = match (positional, named) {
            (None, None) => return Ok(None),
            (Some(_), Some(_)) => {
                return Err(binder_error(format!(
                    "{function}: the scale factor can be given as a positional argument or as \"sf\", not both"
                )));
            }
            (Some(expr), None) | (None, Some(expr)) => self.constant_argument(expr)?,
        };
        if sf.is_null() {
            return Err(binder_error(format!("{function}: cannot use NULL as scale factor")));
        }
        Ok(Some(scale_factor(&sf)?))
    }

    fn no_arguments(&self, function: &str, args: &[FunctionArgument]) -> Result<()> {
        match args.first() {
            None => Ok(()),
            Some(argument) if argument.name.is_empty() => Err(no_match(function, args)),
            Some(argument) => Err(invalid_named_parameter(function, &argument.name)),
        }
    }

    /// The rows of a table function whose values are all known when it is bound.
    fn bind_constant_rows(
        &mut self,
        table_ref: &TableRef,
        name: &str,
        names: Vec<String>,
        types: Vec<LogicalType>,
        rows: Vec<Vec<Value>>,
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
            catalog: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let expressions = rows
            .into_iter()
            .map(|row| row.into_iter().map(BoundExpr::constant).collect())
            .collect();
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ExpressionGet {
                function: None,
                volatile: false,
                materialized: false,
                table_index,
                types,
                expressions,
            },
            vec![],
        ))
    }
}

/// `Value::GetValue<double>`, as the argument of a DOUBLE parameter is cast.
pub(crate) fn scale_factor(value: &Value) -> Result<f64> {
    let cast = crate::function::cast::cast_value(value, &LogicalType::DOUBLE)?;
    match cast.data() {
        ValueData::Double(sf) => Ok(*sf),
        _ => Err(Error::new(ErrorKind::Conversion, "the scale factor is not a number")),
    }
}

fn no_match(function: &str, args: &[FunctionArgument]) -> Error {
    let positional = args.iter().filter(|a| a.name.is_empty()).map(|_| "?").collect::<Vec<_>>();
    binder_error(format!(
        "No function matches the given name and argument types '{function}({})'",
        positional.join(", ")
    ))
}

fn invalid_named_parameter(function: &str, name: &str) -> Error {
    binder_error(format!("Invalid named parameter \"{name}\" for function {function}"))
}
