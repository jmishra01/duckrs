// Ported from duckdb/src/main/{database,client_context,materialized_query_result}.cpp
// @ rust-port-baseline
//
//! Databases and client contexts (DuckDB's `src/main`): running SQL text statement by statement through the
//! parser, binder, planner and executor.

pub mod settings;
mod settings_table;

use crate::catalog::{Catalog, DEFAULT_CATALOG};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::physical::PhysicalResultCollector;
use crate::execution::{create_plan, execute_plan};
use crate::planner::binder::{BindOptions, Binder, DefaultNullOrder, bind_constant_expression};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use duckrs_common::Result;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderType;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ParserOptions;
use duckrs_parser::ast::{PragmaInfo, Statement};
use parking_lot::RwLock;
use settings::SettingValues;
use std::sync::Arc;

/// A database (`DatabaseInstance`): its catalog, its global settings and, with Phase 9, its
/// storage.
#[derive(Debug)]
pub struct DatabaseInstance {
    pub catalog: Catalog,
    settings: RwLock<SettingValues>,
}

impl DatabaseInstance {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog: Catalog::new(DEFAULT_CATALOG),
            settings: RwLock::new(SettingValues::default()),
        })
    }
}

/// Pragmas that only switch DuckDB's debug verification, profiling or progress output, none of
/// which changes a result; duckrs runs them as no-ops.
const NO_OP_PRAGMAS: &[&str] = &[
    "enable_verification",
    "disable_verification",
    "verify_external",
    "disable_verify_external",
    "verify_serializer",
    "disable_verify_serializer",
    "verify_parallelism",
    "disable_verify_parallelism",
    "enable_profiling",
    "enable_profile",
    "disable_profiling",
    "disable_profile",
    "enable_progress_bar",
    "disable_progress_bar",
    "enable_print_progress_bar",
    "disable_print_progress_bar",
    "enable_object_cache",
    "disable_object_cache",
];

/// A fully materialized result (`MaterializedQueryResult`).
#[derive(Debug)]
pub struct MaterializedQueryResult {
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub collection: ColumnDataCollection,
}

impl MaterializedQueryResult {
    pub fn row_count(&self) -> usize {
        self.collection.len()
    }

    pub fn column_count(&self) -> usize {
        self.types.len()
    }

    /// The value at `column`, `row` (`GetValue`).
    pub fn value(&self, column: usize, row: usize) -> Value {
        self.collection.get_value(column, row)
    }
}

/// A connection's state (`ClientContext`).
#[derive(Debug)]
pub struct ClientContext {
    db: Arc<DatabaseInstance>,
    /// session settings (`ClientConfig`)
    settings: SettingValues,
}

impl ClientContext {
    pub fn new(db: Arc<DatabaseInstance>) -> Self {
        Self {
            db,
            settings: SettingValues::default(),
        }
    }

    /// The current value of a setting (`Settings::Get` / `current_setting`).
    pub fn setting(&self, name: &str) -> Option<Value> {
        settings::current_value(name, &self.db.settings.read(), &self.settings)
    }

    fn setting_text(&self, name: &str) -> String {
        self.setting(name)
            .and_then(|v| v.try_to_string().ok())
            .unwrap_or_default()
    }

    /// The settings the binder needs.
    fn bind_options(&self) -> BindOptions {
        BindOptions {
            default_order: if self.setting_text("default_order") == "DESC" {
                OrderType::Descending
            } else {
                OrderType::Ascending
            },
            default_null_order: match self.setting_text("default_null_order").as_str() {
                "NULLS_FIRST" => DefaultNullOrder::NullsFirst,
                "NULLS_FIRST_ON_ASC_LAST_ON_DESC" => DefaultNullOrder::NullsFirstOnAscLastOnDesc,
                "NULLS_LAST_ON_ASC_FIRST_ON_DESC" => DefaultNullOrder::NullsLastOnAscFirstOnDesc,
                _ => DefaultNullOrder::NullsLast,
            },
        }
    }

    /// `ClientContext::GetParserOptions`.
    fn parser_options(&self) -> ParserOptions {
        let mut options = ParserOptions::default();
        if let Some(depth) = self.setting("max_expression_depth").and_then(|v| v.as_i128()) {
            options.max_expression_depth = depth as usize;
        }
        options
    }

    /// `ClientContext::Query`: runs every statement in `sql` and returns the last result.
    pub fn query(&mut self, sql: &str) -> Result<MaterializedQueryResult> {
        let statements = duckrs_parser::parse_ast_with(sql, &self.parser_options())?;
        let mut result = MaterializedQueryResult {
            names: Vec::new(),
            types: Vec::new(),
            collection: ColumnDataCollection::new(Vec::new()),
        };
        for statement in &statements {
            result = self.run_statement(statement)?;
        }
        Ok(result)
    }

    /// `ClientContext::PendingStatementOrPreparedStatement` + `ExecutePendingQueryInternal`.
    fn run_statement(&mut self, statement: &Statement) -> Result<MaterializedQueryResult> {
        match statement {
            Statement::SetVariable { name, value, scope } => {
                // `Binder::Bind(SetVariableStatement)` + `PhysicalSet`
                let value = evaluate_scalar(&bind_constant_expression(&self.db.catalog, value)?)?;
                let mut global = self.db.settings.write();
                settings::set_variable(name, *scope, &value, &mut global, &mut self.settings)?;
                return Ok(Self::success());
            }
            Statement::ResetVariable { name, scope } => {
                let mut global = self.db.settings.write();
                settings::reset_variable(name, *scope, &mut global, &mut self.settings)?;
                return Ok(Self::success());
            }
            Statement::Pragma(PragmaInfo {
                name,
                parameters,
                named_parameters,
            }) => {
                if parameters.is_empty()
                    && named_parameters.is_empty()
                    && NO_OP_PRAGMAS.iter().any(|p| p.eq_ignore_ascii_case(name))
                {
                    return Ok(Self::success());
                }
                return Err(crate::unported(format!("PRAGMA {name}")));
            }
            _ => {}
        }
        let mut binder = Binder::with_options(&self.db.catalog, self.bind_options());
        let mut bound = binder.bind(statement)?;
        resolve_column_bindings(&mut bound.plan)?;
        let plan = create_plan(bound.plan);
        let collector = PhysicalResultCollector::new(plan);
        execute_plan(&collector)?;
        let collection = collector.result.into_inner();
        // the result has the statement's types, which a collector of e.g. CREATE TABLE lacks
        let collection = if collection.types() == bound.types.as_slice() {
            collection
        } else {
            ColumnDataCollection::new(bound.types.clone())
        };
        Ok(MaterializedQueryResult {
            names: bound.names,
            types: bound.types,
            collection,
        })
    }
}

impl ClientContext {
    /// The empty result of a statement that returns nothing (`Success` column).
    fn success() -> MaterializedQueryResult {
        MaterializedQueryResult {
            names: vec!["Success".to_string()],
            types: vec![LogicalType::BOOLEAN],
            collection: ColumnDataCollection::new(vec![LogicalType::BOOLEAN]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(context: &mut ClientContext, sql: &str) -> Vec<Vec<String>> {
        let result = context.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        (0..result.row_count())
            .map(|row| {
                (0..result.column_count())
                    .map(|col| {
                        let v = result.value(col, row);
                        if v.is_null() {
                            "NULL".into()
                        } else {
                            v.try_to_string().unwrap()
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn simple_projection() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE a (i integer, j integer)");
        assert!(run(&mut context, "SELECT * FROM a").is_empty());
        assert_eq!(run(&mut context, "INSERT INTO a VALUES (42, 84)"), vec![vec!["1"]]);
        assert_eq!(run(&mut context, "SELECT x, y FROM a i1(x, y)"), vec![vec!["42", "84"]]);
        run(&mut context, "CREATE TABLE test (a INTEGER, b INTEGER)");
        run(&mut context, "INSERT INTO test VALUES (11, 22), (12, 21), (13, 22)");
        assert_eq!(
            run(&mut context, "SELECT a + 2, b FROM test WHERE a = 11"),
            vec![vec!["13", "22"]]
        );
        assert_eq!(
            run(&mut context, "SELECT cast(cast(a AS VARCHAR) as INTEGER) FROM test"),
            vec![vec!["11"], vec!["12"], vec!["13"]]
        );
        assert_eq!(
            run(&mut context, "SELECT 1 + 1, NULL + 1, 'a' = 'a'"),
            vec![vec!["2", "NULL", "true"]]
        );
    }

    #[test]
    fn order_limit_distinct() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (a INTEGER, b VARCHAR)");
        run(
            &mut context,
            "INSERT INTO t VALUES (3, 'c'), (1, 'a'), (NULL, 'n'), (2, 'b'), (1, 'a')",
        );
        let col = |rows: Vec<Vec<String>>| rows.into_iter().map(|r| r.join(",")).collect::<Vec<_>>();
        assert_eq!(
            col(run(&mut context, "SELECT a FROM t ORDER BY a")),
            ["1", "1", "2", "3", "NULL"]
        );
        assert_eq!(
            col(run(&mut context, "SELECT a FROM t ORDER BY 1 DESC")),
            ["3", "2", "1", "1", "NULL"]
        );
        // ORDER BY a column that is not selected, by an alias and by an expression
        assert_eq!(
            col(run(&mut context, "SELECT b FROM t ORDER BY a DESC NULLS FIRST")),
            ["n", "c", "b", "a", "a"]
        );
        assert_eq!(
            col(run(&mut context, "SELECT a * 2 AS x FROM t WHERE x > 2 ORDER BY x")),
            ["4", "6"]
        );
        assert_eq!(
            col(run(&mut context, "SELECT a FROM t ORDER BY a + 1 LIMIT 2 OFFSET 1")),
            ["1", "2"]
        );
        assert_eq!(
            col(run(&mut context, "SELECT DISTINCT a, b FROM t ORDER BY ALL")),
            ["1,a", "2,b", "3,c", "NULL,n"]
        );
        run(&mut context, "SET default_null_order='nulls_first'");
        assert_eq!(
            col(run(&mut context, "SELECT a FROM t ORDER BY a LIMIT 2")),
            ["NULL", "1"]
        );
        run(&mut context, "RESET default_null_order");
        assert_eq!(col(run(&mut context, "SELECT a FROM t ORDER BY a LIMIT 1")), ["1"]);
        // a query reads the rows its table had when it started
        run(&mut context, "CREATE TABLE d (i INTEGER)");
        run(&mut context, "INSERT INTO d VALUES (1), (2)");
        for _ in 0..12 {
            run(&mut context, "INSERT INTO d FROM d");
        }
        assert_eq!(col(run(&mut context, "SELECT i FROM d LIMIT 1 OFFSET 8191")), ["2"]);
        assert!(run(&mut context, "SELECT i FROM d LIMIT 1 OFFSET 8192").is_empty());
        let err = context.query("SELECT a FROM t ORDER BY 3").unwrap_err().to_string();
        assert_eq!(err, "Binder Error: ORDER term out of range - should be between 1 and 1");
        let err = context.query("SELECT a FROM t LIMIT -1").unwrap_err().to_string();
        assert_eq!(err, "Binder Error: LIMIT/OFFSET cannot be negative");
    }

    #[test]
    fn errors_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        assert_eq!(
            err(&mut context, "SELECT x"),
            "Binder Error: Referenced column \"x\" was not found because the FROM clause is missing"
        );
        assert_eq!(
            err(&mut context, "SELECT * FROM t"),
            "Catalog Error: Table with name t does not exist!"
        );
        run(&mut context, "CREATE TABLE t (abc INTEGER)");
        assert_eq!(
            err(&mut context, "SELECT abd FROM t"),
            "Binder Error: Referenced column \"abd\" not found in FROM clause!\nCandidate bindings: \"abc\""
        );
        assert_eq!(
            err(&mut context, "INSERT INTO t VALUES (1, 2)"),
            "Binder Error: table t has 1 columns but 2 values were supplied"
        );
        assert_eq!(
            err(&mut context, "SELECT 2147483647 + 1"),
            "Out of Range Error: Overflow in addition of INT32 (2147483647 + 1)!"
        );
        assert_eq!(
            err(&mut context, "SELECT CAST('abc' AS INTEGER)"),
            "Conversion Error: Could not convert string 'abc' to INT32"
        );
    }
}
