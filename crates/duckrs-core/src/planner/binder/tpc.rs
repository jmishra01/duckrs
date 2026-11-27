// Ported from duckdb/extension/tpch/tpch_extension.cpp and duckdb/extension/tpcds/tpcds_extension.cpp
// @ rust-port-baseline
//
//! The table functions of the `tpch` and `tpcds` extensions that only return text:
//! `tpch_queries`, `tpch_answers`, `tpcds_queries` and `tpcds_answers`. `CALL dbgen` and
//! `CALL dsdgen` create and fill tables, so the client runs them (`duckrs-main`'s `tpc` module).

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
        if let Some(benchmark) = TpcBenchmark::of_generator(name) {
            return self.bind_tpc_generator(benchmark, args, table_ref).map(Some);
        }
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
                    rows.push(constants(vec![Value::integer(query), Value::varchar(text)]));
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
                        rows.push(constants(vec![
                            Value::integer(query),
                            Value::double(sf),
                            Value::varchar(answer),
                        ]));
                    }
                }
                (
                    vec!["query_nr", "scale_factor", "answer"],
                    vec![LogicalType::INTEGER, LogicalType::DOUBLE, LogicalType::VARCHAR],
                    rows,
                )
            }
            "tpcds_queries" => {
                let sf = self.query_scale_factor(name, args)?;
                let count = duckrs_tpcds::QUERIES_COUNT as i32;
                let mut rows = Vec::with_capacity(count as usize);
                for query in 1..=count {
                    // `TPCDSQueriesBindData::GetQuery`
                    let text = match sf {
                        Some(sf) => duckrs_tpcds::query_for_scale(query, sf)?,
                        None => duckrs_tpcds::query(query)?.to_string(),
                    };
                    rows.push(constants(vec![Value::integer(query), Value::varchar(text)]));
                }
                (
                    vec!["query_nr", "query"],
                    vec![LogicalType::INTEGER, LogicalType::VARCHAR],
                    rows,
                )
            }
            "tpcds_answers" => {
                self.no_arguments(name, args)?;
                let count = duckrs_tpcds::QUERIES_COUNT as i32;
                let mut rows = Vec::new();
                // `TPCDSQueryAnswerFunction`: C++ lists scale factors it has no answers for (10), whose
                // `GetAnswer` throws when the scan reaches them, not when it is bound
                for sf in duckrs_tpcds::TPCDS_ANSWERS_SCALE_FACTORS {
                    for query in 1..=count {
                        let answer = match duckrs_tpcds::answer(sf, query) {
                            Ok(answer) => BoundExpr::constant(Value::varchar(answer)),
                            Err(error) => self.runtime_error(&error)?,
                        };
                        rows.push(vec![
                            BoundExpr::constant(Value::integer(query)),
                            BoundExpr::constant(Value::double(sf)),
                            answer,
                        ]);
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
        expressions: Vec<Vec<BoundExpr>>,
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

impl Binder<'_> {
    /// A VARCHAR expression that fails with `error` when it is evaluated.
    fn runtime_error(&mut self, error: &Error) -> Result<BoundExpr> {
        let message = error.message.replace('\'', "''");
        let expr = super::alter_fields::parse_expression(&format!("error('{message}')::VARCHAR"))?;
        self.bind_expression(&expr)
    }
}

fn constants(values: Vec<Value>) -> Vec<BoundExpr> {
    values.into_iter().map(BoundExpr::constant).collect()
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

/// The benchmark of a TPC extension's generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpcBenchmark {
    Tpch,
    Tpcds,
}

impl TpcBenchmark {
    fn of_generator(name: &str) -> Option<Self> {
        match name {
            "dbgen" => Some(TpcBenchmark::Tpch),
            "dsdgen" => Some(TpcBenchmark::Tpcds),
            _ => None,
        }
    }

    /// The name of its generator, `dbgen` or `dsdgen`.
    pub fn generator(self) -> &'static str {
        match self {
            TpcBenchmark::Tpch => "dbgen",
            TpcBenchmark::Tpcds => "dsdgen",
        }
    }

    /// The type of a named option of the generator (its `TypedKwargs`).
    fn option_type(self, name: &str) -> Option<LogicalType> {
        Some(match (self, name) {
            (_, "sf") => LogicalType::DOUBLE,
            (_, "overwrite") | (TpcBenchmark::Tpcds, "keys") => LogicalType::BOOLEAN,
            (_, "catalog" | "schema" | "suffix") => LogicalType::VARCHAR,
            (TpcBenchmark::Tpch, "children" | "step") => LogicalType::UINTEGER,
            _ => return None,
        })
    }
}

/// `DBGenFunctionData` / `DSDGenFunctionData`: what a `dbgen(...)` or `dsdgen(...)` in a FROM clause generates,
/// into the tables of `schema`.
#[derive(Debug, Clone)]
pub struct TpcGeneration {
    pub benchmark: TpcBenchmark,
    pub schema: std::sync::Arc<crate::catalog::SchemaCatalogEntry>,
    pub sf: f64,
    pub suffix: String,
    pub overwrite: bool,
    pub keys: bool,
    pub children: u32,
    pub step: Option<u32>,
}

/// The client's side of `dbgen(...)` / `dsdgen(...)` in a FROM clause: the scan that generates the data when the
/// query runs (in `execution`, which the binder can't use), and whether the database takes writes.
pub struct TpcGenerator {
    /// the name of the database if it is read-only (`RegisterDBModify` refuses it)
    pub read_only: Option<String>,
    pub scan: Box<dyn Fn(TpcGeneration) -> std::sync::Arc<dyn crate::function::udf::TableUdfScan> + Send + Sync>,
}

impl Binder<'_> {
    /// `DbgenBind` / `DsdgenBind` of a call in a FROM clause: a scan with the one column `Success` and no rows, whose
    /// run creates the tables and fills them.
    fn bind_tpc_generator(
        &mut self,
        benchmark: TpcBenchmark,
        args: &[FunctionArgument],
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let function = benchmark.generator();
        if args.iter().any(|a| a.name.is_empty()) {
            return Err(no_match(function, args));
        }
        let mut sf = 0.0;
        let (mut schema, mut suffix, mut overwrite, mut keys) = (None, String::new(), false, false);
        let (mut children, mut step) = (1, None);
        for argument in args {
            let name = argument.name.to_lowercase();
            let Some(ty) = benchmark.option_type(&name) else {
                return Err(invalid_named_parameter(function, &argument.name));
            };
            let value = self.constant_argument(&argument.expr)?;
            if value.is_null() {
                return Err(binder_error("Cannot use NULL as function argument"));
            }
            let value = crate::function::cast::cast_value(&value, &ty)?;
            match (name.as_str(), value.data()) {
                ("sf", ValueData::Double(v)) => sf = *v,
                ("schema", _) => schema = Some(value.try_to_string()?),
                ("suffix", _) => suffix = value.try_to_string()?,
                ("overwrite", ValueData::Bool(v)) => overwrite = *v,
                ("keys", ValueData::Bool(v)) => keys = *v,
                ("children", ValueData::UInt32(v)) => children = *v,
                ("step", ValueData::UInt32(v)) => step = Some(*v),
                // the tables go into the database the statement runs in
                ("catalog", _) => {
                    return Err(crate::unported(format!(
                        "the catalog option of {function} in a FROM clause (use CALL {function}(...))"
                    )));
                }
                _ => {}
            }
        }
        if children != 1 && step.is_none() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Step must be defined when children are defined",
            ));
        }
        let generator = self
            .session
            .as_ref()
            .and_then(|session| session.tpc_generator.clone())
            .ok_or_else(|| crate::unported(format!("{function} here")))?;
        // `RegisterDBModify`
        if let Some(database) = &generator.read_only {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Cannot execute statement of type \"SELECT\" on database \"{database}\" which is attached in \
                     read-only mode!"
                ),
            ));
        }
        let schema = self.catalog.schema_for(schema.as_deref())?;
        let scan = (generator.scan)(TpcGeneration {
            benchmark,
            schema,
            sf,
            suffix,
            overwrite,
            keys,
            children,
            step,
        });
        let types = vec![LogicalType::BOOLEAN];
        let alias = if table_ref.alias.is_empty() {
            function.to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, vec!["Success".to_string()])?;
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
        Ok(LogicalOperator::new(
            LogicalOperatorKind::UserTableScan {
                table_index,
                types,
                scan: crate::function::udf::UserTableScan {
                    name: function.to_uppercase(),
                    scan,
                    cardinality: Some((0, true)),
                },
            },
            vec![],
        ))
    }
}
