// Ported from duckdb/extension/tpch/tpch_extension.cpp (DbgenBind, DbgenFunction, PragmaTpchQuery) and
// DBGenWrapper::CreateTPCHSchema of duckdb/extension/tpch/dbgen/dbgen.cpp @ rust-port-baseline
//
//! The statements of the TPC extensions that change the database or run a query: `CALL dbgen(...)`,
//! which creates the TPC-H tables and fills them, and `PRAGMA tpch(n)`, which runs a query. Their
//! text-only table functions (`tpch_queries`, `tpch_answers`) are in the binder.

use super::{ClientContext, MaterializedQueryResult};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_core::execution::expression_executor::evaluate_scalar;
use duckrs_core::planner::binder::bind_constant_expression;
use duckrs_parser::ast::{Expr, ExprKind, FunctionArgument, Statement, sql_quoted_identifier};
use duckrs_tpch::{DBGenGenerator, TpchTable};
use std::sync::Arc;

/// The chunks of a table that one INSERT appends (`DBGEN_ROW_BATCH_SIZE` rows and more).
const APPEND_CHUNKS: usize = 60;

/// Whether `function` is `dbgen(...)`, which `CALL` runs.
pub(super) fn is_dbgen_call(function: &Expr) -> bool {
    matches!(&function.kind, ExprKind::Function(call)
        if call.name.qualification.is_empty() && call.name.name.eq_ignore_ascii_case("dbgen"))
}

/// `DBGenFunctionData`.
struct DbgenOptions {
    sf: f64,
    catalog: Option<String>,
    schema: Option<String>,
    suffix: String,
    children: u32,
    step: Option<u32>,
}

impl ClientContext {
    /// `CALL dbgen(...)`: `DbgenBind`, then `DbgenFunction`, which gives no rows.
    pub(super) fn run_dbgen(&mut self, function: &Expr) -> Result<MaterializedQueryResult> {
        let options = self.bind_dbgen(function)?;
        // `RegisterDBModify`: the database it writes takes writes
        let database = match &options.catalog {
            Some(name) => self.find_database(name).ok_or_else(|| {
                Error::new(ErrorKind::Catalog, format!("Catalog \"{name}\" does not exist!"))
            })?,
            None => self.db.clone(),
        };
        database.check_writable(&Statement::Call(Box::new(function.clone())))?;
        // `DbgenBind`: children without a step are an error before anything is created
        let mut generator = DBGenGenerator::new(options.sf, options.children, options.step)?;
        let target = |table: &str| -> String {
            let table = sql_quoted_identifier(&format!("{table}{}", options.suffix));
            match (&options.catalog, &options.schema) {
                (Some(catalog), schema) => format!(
                    "{}.{}.{table}",
                    sql_quoted_identifier(catalog),
                    sql_quoted_identifier(schema.as_deref().unwrap_or(duckrs_core::catalog::DEFAULT_SCHEMA))
                ),
                (None, Some(schema)) => format!("{}.{table}", sql_quoted_identifier(schema)),
                (None, None) => table,
            }
        };
        // `DBGenWrapper::CreateTPCHSchema`: the tables, if they are not there, with NOT NULL columns
        for table in TpchTable::ALL {
            let schema = table.schema();
            let columns: Vec<String> = schema
                .columns
                .iter()
                .map(|column| format!("{} {} NOT NULL", sql_quoted_identifier(column.name), column.ty))
                .collect();
            let name = target(schema.name);
            self.run_sql(&format!("CREATE TABLE IF NOT EXISTS {name} ({})", columns.join(", ")))?;
            // `ValidateTPCHTableSchema`: a table that was there must have the columns dbgen fills
            let existing = self.run_sql(&format!("SELECT * FROM {name} LIMIT 0"))?;
            let found: Vec<(&str, &LogicalType)> = existing
                .names
                .iter()
                .map(String::as_str)
                .zip(&existing.types)
                .collect();
            duckrs_tpch::validate_tpch_table_schema(table, &format!("{}{}", schema.name, options.suffix), &found)?;
        }
        let mut pending: Option<(TpchTable, Vec<DataChunk>)> = None;
        while let Some((table, chunk)) = generator.next_chunk() {
            duckrs_core::execution::interrupt::check()?;
            match &mut pending {
                Some((current, chunks)) if *current == table && chunks.len() < APPEND_CHUNKS => chunks.push(chunk),
                _ => {
                    if let Some((current, chunks)) = pending.replace((table, vec![chunk])) {
                        self.append_generated(&target(current.name()), current, chunks)?;
                    }
                }
            }
        }
        if let Some((current, chunks)) = pending {
            self.append_generated(&target(current.name()), current, chunks)?;
        }
        Ok(Self::success())
    }

    /// The rows of `chunks` into `name`, as an INSERT (`TPCHDataAppender`'s appender).
    fn append_generated(&mut self, name: &str, table: TpchTable, chunks: Vec<DataChunk>) -> Result<()> {
        let names = table.column_names().into_iter().map(str::to_string).collect();
        self.appended = Some((names, table.types(), Arc::new(chunks)));
        let result = self.run_sql(&format!("INSERT INTO {name} SELECT * FROM __internal_appender_chunks()"));
        self.appended = None;
        result.map(drop)
    }

    /// One statement, run as part of the statement that runs it.
    fn run_sql(&mut self, sql: &str) -> Result<MaterializedQueryResult> {
        let statement = duckrs_parser::parse_ast(sql)?.remove(0);
        // a table of an attached database: the statement runs there
        if let Some((database, routed)) = self.route_to_database(&statement)? {
            let previous = std::mem::replace(&mut self.db, database);
            let result = self.execute_statement(&routed);
            self.db = previous;
            return result;
        }
        self.execute_statement(&statement)
    }

    /// `DbgenBind`, of the named arguments of `dbgen(...)`.
    fn bind_dbgen(&self, function: &Expr) -> Result<DbgenOptions> {
        let ExprKind::Function(call) = &function.kind else {
            unreachable!("checked by is_dbgen_call");
        };
        let arguments: Vec<FunctionArgument> =
            call.args.iter().map(duckrs_core::planner::binder::named_argument).collect();
        let mut options = DbgenOptions {
            sf: 0.0,
            catalog: None,
            schema: None,
            suffix: String::new(),
            children: 1,
            step: None,
        };
        if arguments.iter().any(|a| a.name.is_empty()) {
            let types = arguments.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            return Err(Error::new(
                ErrorKind::Binder,
                format!("No function matches the given name and argument types 'dbgen({types})'"),
            ));
        }
        let catalog = self.catalog();
        for argument in &arguments {
            let name = argument.name.to_lowercase();
            let ty = match name.as_str() {
                "sf" => LogicalType::DOUBLE,
                "overwrite" => LogicalType::BOOLEAN,
                "catalog" | "schema" | "suffix" => LogicalType::VARCHAR,
                "children" | "step" => LogicalType::UINTEGER,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Binder,
                        format!("Invalid named parameter \"{}\" for function dbgen", argument.name),
                    ));
                }
            };
            let value = evaluate_scalar(&bind_constant_expression(&catalog, &argument.expr)?)?;
            if value.is_null() {
                return Err(Error::new(ErrorKind::Binder, "Cannot use NULL as function argument"));
            }
            // `PlaceOption`
            let value = duckrs_core::function::cast::cast_value(&value, &ty).map_err(|error| {
                let literal = match value.logical_type().id() {
                    duckrs_common::types::LogicalTypeId::Varchar => {
                        format!("'{}'", value.try_to_string().unwrap_or_default().replace('\'', "''"))
                    }
                    _ => value.try_to_string().unwrap_or_default(),
                };
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Could not cast value {literal} to named parameter \"{name}\" of type {ty}: {}",
                        error.message
                    ),
                )
            })?;
            match (name.as_str(), value.data()) {
                ("sf", ValueData::Double(sf)) => options.sf = *sf,
                ("catalog", _) => options.catalog = Some(value.try_to_string()?),
                ("schema", _) => options.schema = Some(value.try_to_string()?),
                ("suffix", _) => options.suffix = value.try_to_string()?,
                ("children", _) => options.children = unsigned(&value)?,
                ("step", _) => options.step = Some(unsigned(&value)?),
                // the tables are created if they are not there, whatever `overwrite` says
                _ => {}
            }
        }
        Ok(options)
    }

    /// `PragmaTpchQuery`: the text of query `n`, which then runs.
    pub(super) fn run_tpch_pragma(
        &mut self,
        parameters: &[Expr],
        named_parameters: &[(String, Expr)],
    ) -> Result<MaterializedQueryResult> {
        let catalog = self.catalog();
        let [number] = parameters else {
            return Err(Error::new(
                ErrorKind::Binder,
                "No function matches the given name and argument types 'tpch'",
            ));
        };
        let number = evaluate_scalar(&bind_constant_expression(&catalog, number)?)?;
        // `BindPragmaFunction`: `query_nr` is a BIGINT, which a DECIMAL or a VARCHAR is not implicitly
        if duckrs_core::function::cast_rules::implicit_cast(number.logical_type(), &LogicalType::BIGINT) < 0 {
            return Err(Error::new(
                ErrorKind::Binder,
                format!(
                    "No function matches the given name and argument types 'tpch({})'",
                    number.logical_type()
                ),
            ));
        }
        if number.is_null() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot use NULL as argument for the TPC-H query number",
            ));
        }
        let number = duckrs_core::function::cast::cast_value(&number, &LogicalType::INTEGER)?;
        let ValueData::Int32(number) = number.data() else {
            unreachable!("cast to INTEGER");
        };
        let mut sf = None;
        for (name, expr) in named_parameters {
            if !name.eq_ignore_ascii_case("sf") {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Invalid named parameter \"{name}\" for function tpch"),
                ));
            }
            let value = evaluate_scalar(&bind_constant_expression(&catalog, expr)?)?;
            if value.is_null() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Cannot use NULL as scale factor for the TPC-H query",
                ));
            }
            let value = duckrs_core::function::cast::cast_value(&value, &LogicalType::DOUBLE)?;
            if let ValueData::Double(value) = value.data() {
                sf = Some(*value);
            }
        }
        let query = match sf {
            Some(sf) => duckrs_tpch::get_query_for_sf(*number, sf)?,
            None => duckrs_tpch::get_query(*number)?.to_string(),
        };
        let mut result = Self::success();
        for statement in duckrs_parser::parse_ast(&query)? {
            result = self.execute_statement(&statement)?;
        }
        Ok(result)
    }
}

/// A UINTEGER argument.
fn unsigned(value: &Value) -> Result<u32> {
    match value.data() {
        ValueData::UInt32(v) => Ok(*v),
        _ => Err(Error::new(ErrorKind::Conversion, "expected a UINTEGER")),
    }
}

/// Whether `statement` is `PRAGMA tpch(...)`.
pub(super) fn is_tpch_pragma(statement: &Statement) -> bool {
    matches!(statement, Statement::Pragma(info) if info.name.eq_ignore_ascii_case("tpch"))
}
