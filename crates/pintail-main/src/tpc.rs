// Ported from duckdb/extension/tpch/tpch_extension.cpp (DbgenBind, DbgenFunction, PragmaTpchQuery),
// DBGenWrapper::CreateTPCHSchema of duckdb/extension/tpch/dbgen/dbgen.cpp,
// duckdb/extension/tpcds/tpcds_extension.cpp (DsdgenBind, DsdgenFunction, PragmaTpcdsQuery) and
// DSDGenWrapper::CreateTPCDSSchema of duckdb/extension/tpcds/dsdgen/dsdgen.cpp @ rust-port-baseline
//
//! The statements of the TPC extensions that change the database or run a query: `CALL dbgen(...)`
//! and `CALL dsdgen(...)`, which create the TPC-H and TPC-DS tables and fill them, and
//! `PRAGMA tpch(n)` and `PRAGMA tpcds(n)`, which run a query. Their text-only table functions
//! (`tpch_queries`, `tpch_answers`, `tpcds_queries`, `tpcds_answers`) are in the binder.

use super::{ClientContext, MaterializedQueryResult};
use pintail_common::types::LogicalType;
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::DataChunk;
use pintail_common::{Error, ErrorKind, Result};
use pintail_core::execution::expression_executor::evaluate_scalar;
use pintail_core::planner::binder::bind_constant_expression;
use pintail_parser::ast::{Expr, ExprKind, FunctionArgument, Statement, sql_quoted_identifier};
use std::collections::HashMap;
use std::sync::Arc;

/// The chunks of a table that one INSERT appends (`DBGEN_ROW_BATCH_SIZE` rows and more).
const APPEND_CHUNKS: usize = 60;

/// The benchmark of a TPC extension.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Benchmark {
    Tpch,
    Tpcds,
}

impl Benchmark {
    /// The table function that generates the data.
    fn generator(self) -> &'static str {
        match self {
            Benchmark::Tpch => "dbgen",
            Benchmark::Tpcds => "dsdgen",
        }
    }

    /// The pragma that runs a query.
    fn pragma(self) -> &'static str {
        match self {
            Benchmark::Tpch => "tpch",
            Benchmark::Tpcds => "tpcds",
        }
    }

    /// How its error messages name it.
    fn title(self) -> &'static str {
        match self {
            Benchmark::Tpch => "TPC-H",
            Benchmark::Tpcds => "TPC-DS",
        }
    }

    /// The type of a named option of the generator (its `TypedKwargs`).
    fn option_type(self, name: &str) -> Option<LogicalType> {
        Some(match (self, name) {
            (_, "sf") => LogicalType::DOUBLE,
            (_, "overwrite") | (Benchmark::Tpcds, "keys") => LogicalType::BOOLEAN,
            (_, "catalog" | "schema" | "suffix") => LogicalType::VARCHAR,
            (Benchmark::Tpch, "children" | "step") => LogicalType::UINTEGER,
            _ => return None,
        })
    }
}

/// The benchmark whose generator `function` calls, if it is `dbgen(...)` or `dsdgen(...)`.
pub(super) fn generator_call(function: &Expr) -> Option<Benchmark> {
    let ExprKind::Function(call) = &function.kind else {
        return None;
    };
    if !call.name.qualification.is_empty() {
        return None;
    }
    [Benchmark::Tpch, Benchmark::Tpcds]
        .into_iter()
        .find(|benchmark| call.name.name.eq_ignore_ascii_case(benchmark.generator()))
}

/// The benchmark whose pragma `statement` is, if it is `PRAGMA tpch(...)` or `PRAGMA tpcds(...)`.
pub(super) fn query_pragma(statement: &Statement) -> Option<Benchmark> {
    let Statement::Pragma(info) = statement else {
        return None;
    };
    [Benchmark::Tpch, Benchmark::Tpcds]
        .into_iter()
        .find(|benchmark| info.name.eq_ignore_ascii_case(benchmark.pragma()))
}

/// A table's column names, types, and the chunks not yet inserted.
type PendingTable = (Vec<String>, Vec<LogicalType>, Vec<DataChunk>);

/// `DBGenFunctionData` and `DSDGenFunctionData`.
struct GeneratorOptions {
    sf: f64,
    catalog: Option<String>,
    schema: Option<String>,
    suffix: String,
    overwrite: bool,
    keys: bool,
    children: u32,
    step: Option<u32>,
}

/// A table to create: its name without the suffix, its columns and its primary key.
struct TableSpec {
    name: &'static str,
    columns: Vec<(String, LogicalType)>,
    not_null: bool,
    primary_key: Option<Vec<String>>,
}

impl ClientContext {
    /// `CALL dbgen(...)` / `CALL dsdgen(...)`: the bind, then the function, which gives no rows.
    pub(super) fn run_generator(&mut self, benchmark: Benchmark, function: &Expr) -> Result<MaterializedQueryResult> {
        let options = self.bind_generator(benchmark, function)?;
        // `RegisterDBModify`: the database it writes takes writes
        let database = match &options.catalog {
            Some(name) => self
                .find_database(name)
                .ok_or_else(|| Error::new(ErrorKind::Catalog, format!("Catalog \"{name}\" does not exist!")))?,
            None => self.db.clone(),
        };
        database.check_writable(&Statement::Call(Box::new(function.clone())))?;
        let target = |table: &str| -> String {
            let table = sql_quoted_identifier(&format!("{table}{}", options.suffix));
            match (&options.catalog, &options.schema) {
                (Some(catalog), schema) => format!(
                    "{}.{}.{table}",
                    sql_quoted_identifier(catalog),
                    sql_quoted_identifier(schema.as_deref().unwrap_or(pintail_core::catalog::DEFAULT_SCHEMA))
                ),
                (None, Some(schema)) => format!("{}.{table}", sql_quoted_identifier(schema)),
                (None, None) => table,
            }
        };
        match benchmark {
            Benchmark::Tpch => {
                // `DbgenBind`: children without a step are an error before anything is created
                let mut generator = pintail_tpch::DBGenGenerator::new(options.sf, options.children, options.step)?;
                // `DBGenWrapper::CreateTPCHSchema`: the tables, if they are not there, with NOT NULL columns
                for table in pintail_tpch::TpchTable::ALL {
                    let schema = table.schema();
                    let spec = TableSpec {
                        name: schema.name,
                        columns: schema
                            .columns
                            .iter()
                            .map(|c| (c.name.to_string(), c.ty.clone()))
                            .collect(),
                        not_null: true,
                        primary_key: None,
                    };
                    let name = target(spec.name);
                    self.create_table(&name, &spec, "CREATE TABLE IF NOT EXISTS")?;
                    // `ValidateTPCHTableSchema`: a table that was there must have the columns dbgen fills
                    let existing = self.run_sql(&format!("SELECT * FROM {name} LIMIT 0"))?;
                    let found: Vec<(&str, &LogicalType)> =
                        existing.names.iter().map(String::as_str).zip(&existing.types).collect();
                    pintail_tpch::validate_tpch_table_schema(
                        table,
                        &format!("{}{}", schema.name, options.suffix),
                        &found,
                    )?;
                }
                let chunks = std::iter::from_fn(|| generator.next_chunk())
                    .map(|(table, chunk)| (table.name(), table.column_names(), table.types(), chunk));
                self.append_generated(&target, chunks)?;
            }
            Benchmark::Tpcds => {
                let generator = pintail_tpcds::dsdgen(options.sf)?;
                // `DSDGenWrapper::CreateTPCDSSchema`: an existing table is an error, or replaced with `overwrite`
                let create = if options.overwrite {
                    "CREATE OR REPLACE TABLE"
                } else {
                    "CREATE TABLE"
                };
                for definition in pintail_tpcds::tpcds_schema("", options.keys) {
                    let spec = TableSpec {
                        name: definition.table.name(),
                        columns: definition.columns,
                        not_null: false,
                        primary_key: definition.primary_key,
                    };
                    self.create_table(&target(spec.name), &spec, create)?;
                }
                let chunks = generator.map(|(table, chunk)| {
                    let names = table.info().columns.iter().map(|(name, _)| *name).collect();
                    (table.name(), names, table.types(), chunk)
                });
                self.append_generated(&target, chunks)?;
            }
        }
        Ok(Self::success())
    }

    fn create_table(&mut self, name: &str, spec: &TableSpec, create: &str) -> Result<()> {
        let mut columns: Vec<String> = spec
            .columns
            .iter()
            .map(|(column, ty)| {
                let not_null = if spec.not_null { " NOT NULL" } else { "" };
                format!("{} {ty}{not_null}", sql_quoted_identifier(column))
            })
            .collect();
        if let Some(key) = &spec.primary_key {
            let key: Vec<String> = key.iter().map(|c| sql_quoted_identifier(c)).collect();
            columns.push(format!("PRIMARY KEY ({})", key.join(", ")));
        }
        self.run_sql(&format!("{create} {name} ({})", columns.join(", ")))
            .map(drop)
    }

    /// The generated chunks into their tables, a batch of each table's chunks to an INSERT
    /// (`TPCHDataAppender`'s appender, `DSDGenGenerator`'s).
    fn append_generated<'n>(
        &mut self,
        target: &dyn Fn(&str) -> String,
        chunks: impl Iterator<Item = (&'static str, Vec<&'n str>, Vec<LogicalType>, DataChunk)>,
    ) -> Result<()> {
        // a table's batch, in the order its chunks came
        let mut pending: HashMap<&'static str, PendingTable> = HashMap::new();
        let mut order: Vec<&'static str> = Vec::new();
        for (table, names, types, chunk) in chunks {
            pintail_core::execution::interrupt::check()?;
            let entry = pending.entry(table).or_insert_with(|| {
                order.push(table);
                (names.iter().map(|n| n.to_string()).collect(), types, Vec::new())
            });
            entry.2.push(chunk);
            if entry.2.len() >= APPEND_CHUNKS {
                let (names, types, chunks) = pending.remove(table).expect("just added");
                order.retain(|t| *t != table);
                self.insert_chunks(&target(table), names, types, chunks)?;
            }
        }
        for table in order {
            let (names, types, chunks) = pending.remove(table).expect("pending");
            self.insert_chunks(&target(table), names, types, chunks)?;
        }
        Ok(())
    }

    fn insert_chunks(
        &mut self,
        name: &str,
        names: Vec<String>,
        types: Vec<LogicalType>,
        chunks: Vec<DataChunk>,
    ) -> Result<()> {
        self.appended = Some((names, types, Arc::new(chunks)));
        let result = self.run_sql(&format!(
            "INSERT INTO {name} SELECT * FROM __internal_appender_chunks()"
        ));
        self.appended = None;
        result.map(drop)
    }

    /// One statement, run as part of the statement that runs it.
    fn run_sql(&mut self, sql: &str) -> Result<MaterializedQueryResult> {
        let statement = pintail_parser::parse_ast(sql)?.remove(0);
        // a table of an attached database: the statement runs there
        if let Some((database, routed)) = self.route_to_database(&statement)? {
            let previous = std::mem::replace(&mut self.db, database);
            let result = self.execute_statement(&routed);
            self.db = previous;
            return result;
        }
        self.execute_statement(&statement)
    }

    /// `DbgenBind` / `DsdgenBind`, of the named arguments of the generator.
    fn bind_generator(&self, benchmark: Benchmark, function: &Expr) -> Result<GeneratorOptions> {
        let ExprKind::Function(call) = &function.kind else {
            unreachable!("checked by generator_call");
        };
        let function_name = benchmark.generator();
        let arguments: Vec<FunctionArgument> = call
            .args
            .iter()
            .map(pintail_core::planner::binder::named_argument)
            .collect();
        let mut options = GeneratorOptions {
            sf: 0.0,
            catalog: None,
            schema: None,
            suffix: String::new(),
            overwrite: false,
            keys: false,
            children: 1,
            step: None,
        };
        if arguments.iter().any(|a| a.name.is_empty()) {
            let types = arguments.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            return Err(Error::new(
                ErrorKind::Binder,
                format!("No function matches the given name and argument types '{function_name}({types})'"),
            ));
        }
        let catalog = self.catalog();
        for argument in &arguments {
            let name = argument.name.to_lowercase();
            let Some(ty) = benchmark.option_type(&name) else {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!(
                        "Invalid named parameter \"{}\" for function {function_name}",
                        argument.name
                    ),
                ));
            };
            let value = evaluate_scalar(&bind_constant_expression(&catalog, &argument.expr)?)?;
            if value.is_null() {
                return Err(Error::new(ErrorKind::Binder, "Cannot use NULL as function argument"));
            }
            let value = place_option(&value, &name, &ty)?;
            match (name.as_str(), value.data()) {
                ("sf", ValueData::Double(sf)) => options.sf = *sf,
                ("catalog", _) => options.catalog = Some(value.try_to_string()?),
                ("schema", _) => options.schema = Some(value.try_to_string()?),
                ("suffix", _) => options.suffix = value.try_to_string()?,
                ("overwrite", ValueData::Bool(b)) => options.overwrite = *b,
                ("keys", ValueData::Bool(b)) => options.keys = *b,
                ("children", _) => options.children = unsigned(&value)?,
                ("step", _) => options.step = Some(unsigned(&value)?),
                _ => {}
            }
        }
        Ok(options)
    }

    /// `PragmaTpchQuery` / `PragmaTpcdsQuery`: the text of query `n`, which then runs.
    pub(super) fn run_query_pragma(
        &mut self,
        benchmark: Benchmark,
        parameters: &[Expr],
        named_parameters: &[(String, Expr)],
    ) -> Result<MaterializedQueryResult> {
        let catalog = self.catalog();
        let pragma = benchmark.pragma();
        let title = benchmark.title();
        let [number] = parameters else {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("No function matches the given name and argument types '{pragma}'"),
            ));
        };
        let number = evaluate_scalar(&bind_constant_expression(&catalog, number)?)?;
        // `BindPragmaFunction`: `query_nr` is a BIGINT, which a DECIMAL or a VARCHAR is not implicitly
        if pintail_core::function::cast_rules::implicit_cast(number.logical_type(), &LogicalType::BIGINT) < 0 {
            return Err(Error::new(
                ErrorKind::Binder,
                format!(
                    "No function matches the given name and argument types '{pragma}({})'",
                    number.logical_type()
                ),
            ));
        }
        if number.is_null() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Cannot use NULL as argument for the {title} query number"),
            ));
        }
        let number = pintail_core::function::cast::cast_value(&number, &LogicalType::INTEGER)?;
        let ValueData::Int32(number) = number.data() else {
            unreachable!("cast to INTEGER");
        };
        let mut sf = None;
        for (name, expr) in named_parameters {
            if !name.eq_ignore_ascii_case("sf") {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Invalid named parameter \"{name}\" for function {pragma}"),
                ));
            }
            let value = evaluate_scalar(&bind_constant_expression(&catalog, expr)?)?;
            if value.is_null() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("Cannot use NULL as scale factor for the {title} query"),
                ));
            }
            let value = place_option(&value, "sf", &LogicalType::DOUBLE)?;
            if let ValueData::Double(value) = value.data() {
                sf = Some(*value);
            }
        }
        let query = match (benchmark, sf) {
            (Benchmark::Tpch, Some(sf)) => pintail_tpch::get_query_for_sf(*number, sf)?,
            (Benchmark::Tpch, None) => pintail_tpch::get_query(*number)?.to_string(),
            (Benchmark::Tpcds, Some(sf)) => pintail_tpcds::query_for_scale(*number, sf)?,
            (Benchmark::Tpcds, None) => pintail_tpcds::query(*number)?.to_string(),
        };
        let mut result = Self::success();
        for statement in pintail_parser::parse_ast(&query)? {
            result = self.execute_statement(&statement)?;
        }
        Ok(result)
    }
}

/// `PlaceOption`: the value of a named option, cast to its type.
fn place_option(value: &Value, name: &str, ty: &LogicalType) -> Result<Value> {
    pintail_core::function::cast::cast_value(value, ty).map_err(|error| {
        let literal = match value.logical_type().id() {
            pintail_common::types::LogicalTypeId::Varchar => {
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
    })
}

/// A UINTEGER argument.
fn unsigned(value: &Value) -> Result<u32> {
    match value.data() {
        ValueData::UInt32(v) => Ok(*v),
        _ => Err(Error::new(ErrorKind::Conversion, "expected a UINTEGER")),
    }
}
