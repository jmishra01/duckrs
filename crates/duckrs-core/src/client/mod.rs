// Ported from duckdb/src/main/{database,client_context,materialized_query_result}.cpp
// @ rust-port-baseline
//
//! Databases and client contexts (DuckDB's `src/main`): running SQL text statement by statement through the
//! parser, binder, planner and executor.

pub mod settings;
mod settings_table;

use crate::catalog::{
    Catalog, DEFAULT_CATALOG, OnCreateConflict, SequenceData, TableOrView, macro_type_name, missing_entry,
};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::physical::PhysicalResultCollector;
use crate::execution::{create_plan, execute_plan};
use crate::planner::binder::{
    BindOptions, Binder, DefaultNullOrder, ParameterValues, bind_constant_expression, schema_of,
};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderType;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ParserOptions;
use duckrs_parser::ast::{
    CreateInfo, CreateKind, DropInfo, OnCreateConflict as AstConflict, PragmaInfo, Statement, sql_quoted_identifier,
};
use parking_lot::RwLock;
use settings::SettingValues;
use std::collections::HashMap;
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

/// Pragmas that only switch DuckDB's debug verification, profiling, progress output or
/// checkpointing, none of which changes a result; duckrs runs them as no-ops.
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
    "enable_checkpoint_on_shutdown",
    "disable_checkpoint_on_shutdown",
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
    /// the statements of PREPARE (`ClientData::prepared_statements`)
    prepared_statements: HashMap<String, Arc<PreparedStatementData>>,
    /// the parameters of the prepared statement running now
    parameters: Option<Arc<ParameterValues>>,
}

/// A prepared statement (`PreparedStatementData`). It binds anew each time it runs, as C++ does
/// when the catalog changed, so that it always sees the catalog as it is then.
#[derive(Debug)]
pub struct PreparedStatementData {
    statement: Statement,
    /// the parameters, lowercase, in order of appearance
    parameters: Vec<String>,
}

impl ClientContext {
    pub fn new(db: Arc<DatabaseInstance>) -> Self {
        Self {
            db,
            settings: SettingValues::default(),
            prepared_statements: HashMap::new(),
            parameters: None,
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

    /// `ClientContext::Prepare`: checks one statement and keeps it to run later.
    pub fn prepare(&mut self, sql: &str) -> Result<Arc<PreparedStatementData>> {
        let mut statements = duckrs_parser::parse_ast_with(sql, &self.parser_options())?;
        if statements.len() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cannot prepare multiple statements at once!",
            ));
        }
        self.prepare_statement(statements.remove(0))
    }

    fn prepare_statement(&mut self, statement: Statement) -> Result<Arc<PreparedStatementData>> {
        let parameters = Arc::new(ParameterValues::default());
        // these run without the binder, and have no parameters
        let client_statement = matches!(
            statement,
            Statement::SetVariable { .. }
                | Statement::ResetVariable { .. }
                | Statement::Pragma(_)
                | Statement::Create(_)
                | Statement::Drop(_)
        );
        if !client_statement {
            let mut binder = Binder::with_options(&self.db.catalog, self.bind_options());
            binder.parameters = Some(parameters.clone());
            binder.bind(&statement)?;
        }
        let seen = parameters.seen.lock().clone();
        Ok(Arc::new(PreparedStatementData {
            statement,
            parameters: seen,
        }))
    }

    /// `PreparedStatement::Execute` with values for parameters `1`, `2`, ...
    pub fn execute_prepared(
        &mut self,
        prepared: &PreparedStatementData,
        values: &[Value],
    ) -> Result<MaterializedQueryResult> {
        let named = values
            .iter()
            .enumerate()
            .map(|(i, value)| ((i + 1).to_string(), value.clone()))
            .collect();
        self.execute_with(prepared, named)
    }

    /// `PreparedStatement::VerifyParameters`, then the statement with these values.
    fn execute_with(
        &mut self,
        prepared: &PreparedStatementData,
        values: HashMap<String, Value>,
    ) -> Result<MaterializedQueryResult> {
        let mut excess: Vec<&str> = values
            .keys()
            .filter(|k| !prepared.parameters.contains(*k))
            .map(String::as_str)
            .collect();
        if !excess.is_empty() {
            excess.sort_unstable();
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Parameter argument/count mismatch, identifiers of the excess parameters: {}",
                    excess.join(", ")
                ),
            ));
        }
        let missing: Vec<&str> = prepared
            .parameters
            .iter()
            .filter(|p| !values.contains_key(*p))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Values were not provided for the following parameters: {}",
                    missing.join(", ")
                ),
            ));
        }
        self.parameters = Some(Arc::new(ParameterValues {
            values: Some(values),
            seen: Default::default(),
        }));
        let result = self.run_statement(&prepared.statement);
        self.parameters = None;
        result
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
            Statement::Prepare { name, statement } => {
                let prepared = self.prepare_statement((**statement).clone())?;
                self.prepared_statements.insert(name.to_lowercase(), prepared);
                return Ok(Self::success());
            }
            Statement::Execute { name, values } => {
                let Some(prepared) = self.prepared_statements.get(&name.to_lowercase()).cloned() else {
                    return Err(Error::new(
                        ErrorKind::Binder,
                        format!("Prepared statement {} does not exist", sql_quoted_identifier(name)),
                    ));
                };
                let mut bound = HashMap::with_capacity(values.len());
                for (parameter, value) in values {
                    let value = evaluate_scalar(&bind_constant_expression(&self.db.catalog, value)?)?;
                    bound.insert(parameter.to_lowercase(), value);
                }
                return self.execute_with(&prepared, bound);
            }
            Statement::Deallocate(name) => {
                self.prepared_statements.remove(&name.to_lowercase());
                return Ok(Self::success());
            }
            Statement::Create(info) if matches!(info.kind, CreateKind::Macro { .. }) => {
                self.create_macro(info)?;
                return Ok(Self::success());
            }
            Statement::Create(info) if matches!(info.kind, CreateKind::Sequence { .. }) => {
                self.create_sequence(info)?;
                return Ok(Self::success());
            }
            Statement::Create(info) if matches!(info.kind, CreateKind::View { .. }) => {
                self.create_view(info)?;
                return Ok(Self::success());
            }
            Statement::Create(info) => {
                let CreateKind::Schema { options } = &info.kind else {
                    return Err(crate::unported(create_kind_name(&info.kind)));
                };
                if !options.is_empty() || info.temporary || !catalog_qualifier_only(&info.name.qualification) {
                    return Err(crate::unported("this form of CREATE SCHEMA"));
                }
                self.db
                    .catalog
                    .create_schema(&info.name.name, on_create_conflict(info.on_conflict))?;
                return Ok(Self::success());
            }
            Statement::Drop(info) => {
                self.drop_entry(info)?;
                return Ok(Self::success());
            }
            _ => {}
        }
        let mut binder = Binder::with_options(&self.db.catalog, self.bind_options());
        binder.parameters = self.parameters.clone();
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

fn on_create_conflict(conflict: AstConflict) -> OnCreateConflict {
    match conflict {
        AstConflict::Error => OnCreateConflict::Error,
        AstConflict::Ignore => OnCreateConflict::Ignore,
        AstConflict::Replace => OnCreateConflict::Replace,
    }
}

/// The name of a CREATE statement kind, for the not-ported error.
fn create_kind_name(kind: &CreateKind) -> &'static str {
    match kind {
        CreateKind::View { .. } => "CREATE VIEW",
        CreateKind::Schema { .. } => "CREATE SCHEMA",
        CreateKind::Sequence { .. } => "CREATE SEQUENCE",
        CreateKind::Index { .. } => "CREATE INDEX",
        CreateKind::Macro { .. } => "CREATE MACRO",
        CreateKind::Type(_) => "CREATE TYPE",
        CreateKind::Trigger { .. } => "CREATE TRIGGER",
        CreateKind::Secret { .. } => "CREATE SECRET",
    }
}

/// Whether a schema's qualification names no more than this database's catalog.
fn catalog_qualifier_only(qualification: &[String]) -> bool {
    match qualification {
        [] => true,
        [catalog] => catalog.eq_ignore_ascii_case(DEFAULT_CATALOG),
        _ => false,
    }
}

impl ClientContext {
    /// `CREATE VIEW` (`Binder::BindCreateViewInfo` + `DuckSchemaEntry::CreateView`).
    fn create_view(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::View {
            secure,
            aliases,
            defer_binding,
            query,
        } = &info.kind
        else {
            unreachable!("checked by the caller");
        };
        if info.temporary {
            return Err(crate::unported("CREATE TEMPORARY VIEW"));
        }
        if *secure {
            return Err(crate::unported("CREATE SECURE VIEW"));
        }
        // without them a view does not hold on to what it reads, which is the setting's whole point
        if self.setting_text("enable_view_dependencies") == "true" {
            return Err(crate::unported("view dependencies"));
        }
        let catalog = &self.db.catalog;
        let schema = catalog.schema_for(schema_of(&info.name)?)?;
        let on_conflict = on_create_conflict(info.on_conflict);
        // IF NOT EXISTS for a view that exists does not look at the query
        let exists = matches!(schema.get_entry(&info.name.name), Some(TableOrView::View(_)));
        let skip_binding = *defer_binding || (on_conflict == OnCreateConflict::Ignore && exists);
        if !skip_binding {
            let mut binder = Binder::with_options(catalog, self.bind_options());
            binder.bind_create_view(query, aliases, &schema.name)?;
        }
        schema.create_view(&info.name.name, (**query).clone(), aliases.clone(), on_conflict)
    }

    /// `CREATE MACRO` (`Binder::BindCreateFunctionInfo` + `DuckSchemaEntry::CreateFunction`).
    fn create_macro(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::Macro { table_macro, macros } = &info.kind else {
            unreachable!("checked by the caller");
        };
        if info.temporary {
            return Err(crate::unported("CREATE TEMPORARY MACRO"));
        }
        if self.setting_text("enable_macro_dependencies") == "true" {
            return Err(crate::unported("macro dependencies"));
        }
        let catalog = &self.db.catalog;
        let schema = catalog.schema_for(schema_of(&info.name)?)?;
        let mut binder = Binder::with_options(catalog, self.bind_options());
        binder.bind_create_macro(&info.name.name, macros, *table_macro, &schema.name)?;
        schema.create_macro(
            &info.name.name,
            *table_macro,
            macros.clone(),
            on_create_conflict(info.on_conflict),
        )
    }

    /// `CREATE SEQUENCE`.
    fn create_sequence(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::Sequence {
            increment,
            min_value,
            max_value,
            start_value,
            cycle,
        } = info.kind
        else {
            unreachable!("checked by the caller");
        };
        if info.temporary {
            return Err(crate::unported("CREATE TEMPORARY SEQUENCE"));
        }
        let data = SequenceData {
            counter: start_value,
            last_value: None,
            increment,
            min_value,
            max_value,
            cycle,
        };
        let schema = self.db.catalog.schema_for(schema_of(&info.name)?)?;
        schema.create_sequence(&info.name.name, data, on_create_conflict(info.on_conflict))
    }

    /// `DROP TABLE`, `VIEW`, `SEQUENCE`, `MACRO` and `SCHEMA` (`Catalog::DropEntry`).
    fn drop_entry(&mut self, info: &DropInfo) -> Result<()> {
        let catalog = &self.db.catalog;
        match info.entry_type {
            kind @ ("TABLE" | "VIEW") => {
                let type_name = if kind == "TABLE" { "Table" } else { "View" };
                let schema = schema_of(&info.name).map_err(|_| crate::unported(format!("this form of DROP {kind}")))?;
                let dropped = match catalog.schema_for(schema) {
                    Ok(schema) => schema.drop_entry(&info.name.name, type_name)?,
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry(type_name, &info.name.name));
                }
                Ok(())
            }
            kind @ ("MACRO" | "MACRO TABLE") => {
                let table_macro = kind == "MACRO TABLE";
                let schema = schema_of(&info.name).map_err(|_| crate::unported("this form of DROP MACRO"))?;
                let dropped = match catalog.schema_for(schema) {
                    // DROP MACRO also drops a table macro, if there is no scalar one
                    Ok(schema) => {
                        schema.drop_macro(&info.name.name, table_macro)
                            || (!table_macro && schema.drop_macro(&info.name.name, true))
                    }
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry(macro_type_name(table_macro), &info.name.name));
                }
                Ok(())
            }
            "SEQUENCE" => {
                let schema = schema_of(&info.name).map_err(|_| crate::unported("this form of DROP SEQUENCE"))?;
                let dropped = match catalog.schema_for(schema) {
                    Ok(schema) => schema.drop_sequence(&info.name.name),
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry("Sequence", &info.name.name));
                }
                Ok(())
            }
            "SCHEMA" if catalog_qualifier_only(&info.name.qualification) => {
                catalog.drop_schema(&info.name.name, info.if_exists, info.cascade)
            }
            other => Err(crate::unported(format!("DROP {other}"))),
        }
    }

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

    /// Two small tables with a duplicate key, a NULL key and keys only one side has.
    fn join_tables() -> ClientContext {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE l (k INTEGER, a VARCHAR)");
        run(
            &mut context,
            "INSERT INTO l VALUES (1, 'x'), (2, 'y'), (2, 'z'), (NULL, 'n')",
        );
        run(&mut context, "CREATE TABLE r (k INTEGER, b INTEGER)");
        run(
            &mut context,
            "INSERT INTO r VALUES (2, 20), (2, 21), (3, 30), (NULL, 40)",
        );
        context
    }

    fn rows(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|v| v.to_string()).collect())
            .collect()
    }

    #[test]
    fn join_types() {
        let mut c = join_tables();
        let inner = rows(&[&["y", "20"], &["y", "21"], &["z", "20"], &["z", "21"]]);
        for from in [
            "l JOIN r ON l.k = r.k",
            "l, r WHERE l.k = r.k",
            "l JOIN r ON r.k = l.k AND true",
        ] {
            assert_eq!(
                run(&mut c, &format!("SELECT a, b FROM {from} ORDER BY 1, 2")),
                inner,
                "{from}"
            );
        }
        assert_eq!(
            run(&mut c, "SELECT a, b FROM l LEFT JOIN r ON l.k = r.k ORDER BY 1, 2"),
            rows(&[
                &["n", "NULL"],
                &["x", "NULL"],
                &["y", "20"],
                &["y", "21"],
                &["z", "20"],
                &["z", "21"]
            ])
        );
        assert_eq!(
            run(&mut c, "SELECT a, b FROM l RIGHT JOIN r ON l.k = r.k ORDER BY 2, 1"),
            rows(&[
                &["y", "20"],
                &["z", "20"],
                &["y", "21"],
                &["z", "21"],
                &["NULL", "30"],
                &["NULL", "40"]
            ])
        );
        assert_eq!(
            run(&mut c, "SELECT a, b FROM l FULL JOIN r ON l.k = r.k ORDER BY 1, 2").len(),
            8
        );
        assert_eq!(
            run(&mut c, "SELECT a FROM l SEMI JOIN r ON l.k = r.k ORDER BY 1"),
            rows(&[&["y"], &["z"]])
        );
        assert_eq!(
            run(&mut c, "SELECT * FROM l ANTI JOIN r ON l.k = r.k ORDER BY 2"),
            rows(&[&["NULL", "n"], &["1", "x"]])
        );
        // a NULL only finds a NULL with IS NOT DISTINCT FROM
        assert_eq!(
            run(
                &mut c,
                "SELECT a, b FROM l JOIN r ON l.k IS NOT DISTINCT FROM r.k AND a = 'n'"
            ),
            rows(&[&["n", "40"]])
        );
    }

    #[test]
    fn join_predicates() {
        let mut c = join_tables();
        // the predicate decides what counts as a match, not what the join keeps
        assert_eq!(
            run(
                &mut c,
                "SELECT a, b FROM l LEFT JOIN r ON l.k = r.k AND b > 20 ORDER BY 1, 2"
            ),
            rows(&[&["n", "NULL"], &["x", "NULL"], &["y", "21"], &["z", "21"]])
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT a, b FROM l JOIN r ON l.k < r.k AND b < 30 ORDER BY 1, 2"
            ),
            rows(&[&["x", "20"], &["x", "21"]])
        );
        assert_eq!(
            run(&mut c, "SELECT a, b FROM l LEFT JOIN r ON false ORDER BY 1"),
            rows(&[&["n", "NULL"], &["x", "NULL"], &["y", "NULL"], &["z", "NULL"]])
        );
        assert_eq!(
            run(&mut c, "SELECT b FROM l RIGHT JOIN r ON l.k = 1 AND b = 30 ORDER BY 1"),
            rows(&[&["20"], &["21"], &["30"], &["40"]])
        );
    }

    #[test]
    fn using_and_natural() {
        let mut c = join_tables();
        assert_eq!(
            run(&mut c, "SELECT * FROM l JOIN r USING (k) ORDER BY a, b LIMIT 1"),
            rows(&[&["2", "y", "20"]])
        );
        assert_eq!(
            run(&mut c, "SELECT * FROM l NATURAL JOIN r ORDER BY a, b LIMIT 1"),
            rows(&[&["2", "y", "20"]])
        );
        // k is whichever side has it
        assert_eq!(
            run(
                &mut c,
                "SELECT k, a, b FROM l FULL JOIN r USING (k) WHERE b = 30 OR a = 'x' ORDER BY k"
            ),
            rows(&[&["1", "x", "NULL"], &["3", "NULL", "30"]])
        );
        assert_eq!(
            run(&mut c, "SELECT k FROM l RIGHT JOIN r USING (k) WHERE b = 30"),
            rows(&[&["3"]])
        );
        assert_eq!(
            run(&mut c, "SELECT k, count(*) FROM l JOIN r USING (k) GROUP BY k"),
            rows(&[&["2", "4"]])
        );
        run(&mut c, "CREATE TABLE t (k INTEGER, c INTEGER)");
        run(&mut c, "INSERT INTO t VALUES (2, 5)");
        assert_eq!(
            run(
                &mut c,
                "SELECT DISTINCT * FROM l JOIN r USING (k) JOIN t USING (k) WHERE a = 'y' AND b = 20"
            ),
            rows(&[&["2", "y", "20", "5"]])
        );
        // ON only sees the two sides of its join
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().message;
        let message = error(&mut c, "SELECT * FROM (l JOIN r ON t.k = l.k) JOIN t ON true");
        assert!(message.contains("Referenced table \"t\" not found"), "{message}");
        let message = error(&mut c, "SELECT * FROM l JOIN r USING (x)");
        assert_eq!(message, "Column \"x\" does not exist on left side of join!");
        let message = error(&mut c, "SELECT * FROM l NATURAL JOIN (SELECT 1 AS z) u");
        assert!(
            message.starts_with("No columns found to join on in NATURAL JOIN."),
            "{message}"
        );
        let message = error(&mut c, "SELECT * FROM l JOIN r ON sum(l.k) = 1");
        assert_eq!(message, "WHERE clause cannot contain aggregates!");
        // a column of the left side is a LATERAL reference, not a mistake
        for sql in [
            "SELECT * FROM l, r JOIN t ON a = 'x'",
            "SELECT * FROM l, (SELECT l.k) s",
            "SELECT * FROM l, range(k)",
            "SELECT * FROM l, (SELECT 1 FROM r JOIN t ON a = 'x') s",
        ] {
            assert_eq!(c.query(sql).unwrap_err().kind, ErrorKind::NotImplemented, "{sql}");
        }
        // a duplicate alias only matters to a reference that could mean either table
        assert_eq!(
            run(&mut c, "SELECT t.x, t.y FROM (SELECT 1 x) t, (SELECT 2 y) t"),
            rows(&[&["1", "2"]])
        );
        assert_eq!(run(&mut c, "SELECT count(*) FROM t, t"), rows(&[&["1"]]));
        let message = error(&mut c, "SELECT t.k FROM t, t");
        assert!(message.contains("duplicate alias \"t\""), "{message}");
        // nothing refers to the USING column of a side the join drops
        assert_eq!(
            run(&mut c, "SELECT k FROM t SEMI JOIN (l JOIN r USING (k)) ON true"),
            rows(&[&["2"]])
        );
        // nested loops run in the order of DuckDB's cross product
        assert_eq!(
            run(
                &mut c,
                "SELECT x.k, y.k FROM (VALUES (1), (2)) x(k) JOIN (VALUES (1), (2)) y(k) ON x.k <> y.k"
            ),
            rows(&[&["2", "1"], &["1", "2"]])
        );
        let message = error(&mut c, "SELECT k FROM l, r");
        assert!(message.contains("Ambiguous reference to column name k"), "{message}");
    }

    #[test]
    fn joins_larger_than_a_vector() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        let count = "SELECT count(*), sum(a + b) FROM";
        assert_eq!(
            run(&mut c, &format!("{count} range(3000) x(a), range(5000) y(b)")),
            rows(&[&["15000000", "59985000000"]])
        );
        // one key matching more rows than fit a chunk, and a build side of many chunks
        assert_eq!(
            run(
                &mut c,
                &format!(
                    "{count} (SELECT a, 1 AS k FROM range(3000) t(a)) x JOIN (SELECT b, 1 AS k FROM range(5000) t(b)) y USING (k)"
                )
            ),
            rows(&[&["15000000", "59985000000"]])
        );
        // every row of both sides is unmatched in a chunk of its own
        assert_eq!(
            run(
                &mut c,
                "SELECT count(a), count(b) FROM range(5000) x(a) FULL JOIN range(5000, 12000) y(b) ON a = b"
            ),
            rows(&[&["5000", "7000"]])
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT count(*) FROM range(3000) x(a) JOIN range(3000) y(b) ON a < b"
            ),
            rows(&[&["4498500"]])
        );
        // LIMIT stops a cross product that would never finish
        assert_eq!(
            run(&mut c, "SELECT * FROM range(100000000) x, range(100000000) y LIMIT 2").len(),
            2
        );
    }

    #[test]
    fn views() {
        let mut c = join_tables();
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE VIEW v AS SELECT k + 1 AS j, a FROM l WHERE k = 1");
        assert_eq!(run(&mut c, "SELECT * FROM v"), rows(&[&["2", "x"]]));
        assert_eq!(run(&mut c, "SELECT x FROM v t(x)"), rows(&[&["2"]]));
        // a view sees later changes, and joins like a table
        run(&mut c, "INSERT INTO l VALUES (1, 'w')");
        assert_eq!(run(&mut c, "SELECT a FROM v ORDER BY a"), rows(&[&["w"], &["x"]]));
        assert_eq!(
            run(
                &mut c,
                "SELECT v.a, r.b FROM v JOIN r ON v.j = r.k AND r.b = 20 ORDER BY 1"
            ),
            rows(&[&["w", "20"], &["x", "20"]])
        );
        // views on views, and the column names of CREATE VIEW v (...)
        run(&mut c, "CREATE VIEW w (p) AS SELECT j, a FROM v");
        assert_eq!(
            run(&mut c, "SELECT p, a FROM w ORDER BY a LIMIT 1"),
            rows(&[&["2", "w"]])
        );
        assert_eq!(run(&mut c, "SELECT q FROM w z(q) LIMIT 1"), rows(&[&["2"]]));
        // tables and views share their names
        assert_eq!(
            error(&mut c, "CREATE TABLE v (i INTEGER)"),
            "Catalog Error: View with name \"v\" already exists!"
        );
        assert_eq!(
            error(&mut c, "CREATE VIEW l AS SELECT 1"),
            "Catalog Error: Table with name \"l\" already exists!"
        );
        assert_eq!(
            error(&mut c, "CREATE TABLE v AS SELECT 1"),
            "Catalog Error: View with name \"v\" already exists!"
        );
        assert_eq!(
            error(&mut c, "CREATE OR REPLACE VIEW l AS SELECT 1"),
            "Catalog Error: Existing object \"l\" is of type Table, trying to replace with type View"
        );
        assert_eq!(
            error(&mut c, "DROP TABLE v"),
            "Catalog Error: Existing object \"v\" is of type View, trying to drop type Table"
        );
        assert_eq!(
            error(&mut c, "DROP VIEW l"),
            "Catalog Error: Existing object \"l\" is of type Table, trying to drop type View"
        );
        assert_eq!(
            error(&mut c, "INSERT INTO v VALUES (1, 'a')"),
            "Catalog Error: v is not an table"
        );
        assert_eq!(
            error(&mut c, "CREATE VIEW b (x, y) AS SELECT 1"),
            "Binder Error: More VIEW aliases than columns in query result"
        );
        // IF NOT EXISTS never looks at the query of a view that exists; a new one must bind
        run(&mut c, "CREATE VIEW IF NOT EXISTS v AS SELECT * FROM nope");
        assert!(error(&mut c, "CREATE VIEW n AS SELECT * FROM nope").contains("does not exist"));
        // replacing a view with one reading itself only fails on use
        run(&mut c, "CREATE OR REPLACE VIEW w AS SELECT * FROM w");
        assert!(error(&mut c, "SELECT * FROM w").contains("infinite recursion detected"));
        run(&mut c, "DROP VIEW w");
        assert_eq!(
            error(&mut c, "DROP VIEW w"),
            "Catalog Error: View with name w does not exist!"
        );
        run(&mut c, "DROP VIEW IF EXISTS w");
        run(&mut c, "SET enable_view_dependencies = true");
        assert!(error(&mut c, "CREATE VIEW d AS SELECT 1").contains("view dependencies is not yet ported"));
        run(&mut c, "SET enable_view_dependencies = false");
        // a view looks in its own schema first
        run(&mut c, "CREATE SCHEMA s");
        run(&mut c, "CREATE TABLE s.l (a VARCHAR)");
        run(&mut c, "INSERT INTO s.l VALUES ('in s')");
        run(&mut c, "CREATE VIEW s.sv AS SELECT a FROM l");
        assert_eq!(run(&mut c, "SELECT * FROM s.sv"), rows(&[&["in s"]]));
        assert!(error(&mut c, "DROP SCHEMA s").contains("view \"sv\" depends on schema \"s\""));
        run(&mut c, "DROP SCHEMA s CASCADE");
    }

    #[test]
    fn sequences() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE SEQUENCE s");
        assert_eq!(
            error(&mut c, "SELECT currval('s')"),
            "Sequence Error: currval: sequence is not yet defined in this session"
        );
        assert_eq!(
            run(&mut c, "SELECT nextval('s'), nextval('\"s\"')"),
            rows(&[&["1", "2"]])
        );
        assert_eq!(run(&mut c, "SELECT currval('main.s')"), rows(&[&["2"]]));
        // one value per row, and never folded away
        assert_eq!(
            run(&mut c, "SELECT nextval('s') FROM range(3) ORDER BY 1"),
            rows(&[&["3"], &["4"], &["5"]])
        );
        assert_eq!(run(&mut c, "SELECT nextval(NULL)"), rows(&[&["NULL"]]));
        assert_eq!(run(&mut c, "SELECT setval('s', 10)"), rows(&[&["10"]]));
        assert_eq!(run(&mut c, "SELECT nextval('s')"), rows(&[&["11"]]));
        assert_eq!(run(&mut c, "SELECT setval('s', 20, false)"), rows(&[&["20"]]));
        assert_eq!(run(&mut c, "SELECT nextval('s')"), rows(&[&["20"]]));
        // as a column default
        run(&mut c, "CREATE TABLE t (i BIGINT DEFAULT nextval('s'), j INTEGER)");
        run(&mut c, "INSERT INTO t (j) VALUES (1), (2)");
        assert_eq!(run(&mut c, "SELECT i FROM t ORDER BY 1"), rows(&[&["21"], &["22"]]));
        // bounds, cycles and negative increments
        run(&mut c, "CREATE SEQUENCE b MAXVALUE 2");
        run(&mut c, "SELECT nextval('b'), nextval('b')");
        assert_eq!(
            error(&mut c, "SELECT nextval('b')"),
            "Sequence Error: nextval: reached maximum value of sequence \"b\" (2)"
        );
        run(&mut c, "CREATE SEQUENCE r INCREMENT BY -1 MINVALUE 1 MAXVALUE 2 CYCLE");
        assert_eq!(
            run(&mut c, "SELECT nextval('r'), nextval('r'), nextval('r')"),
            rows(&[&["2", "1", "2"]])
        );
        assert_eq!(
            error(&mut c, "SELECT setval('b', 9)"),
            "Sequence Error: setval: value 9 is out of bounds for sequence \"b\" (1..2)"
        );
        // names
        assert_eq!(
            error(&mut c, "SELECT nextval('nope')"),
            "Catalog Error: Sequence with name nope does not exist!"
        );
        assert!(error(&mut c, "SELECT nextval('main..s')").contains("Zero-length"));
        assert!(error(&mut c, "SELECT nextval(a) FROM (VALUES ('s')) t(a)").contains("must be a constant expression"));
        assert_eq!(
            error(&mut c, "CREATE SEQUENCE s"),
            "Catalog Error: Sequence with name \"s\" already exists!"
        );
        run(&mut c, "CREATE SEQUENCE IF NOT EXISTS s");
        run(&mut c, "CREATE OR REPLACE SEQUENCE s");
        assert_eq!(run(&mut c, "SELECT nextval('s')"), rows(&[&["1"]]));
        run(&mut c, "DROP SEQUENCE s");
        assert_eq!(
            error(&mut c, "DROP SEQUENCE s"),
            "Catalog Error: Sequence with name s does not exist!"
        );
        run(&mut c, "DROP SEQUENCE IF EXISTS s");
    }

    #[test]
    fn macros() {
        let mut c = join_tables();
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE MACRO add1(a) AS a + 1");
        assert_eq!(run(&mut c, "SELECT add1(41)"), rows(&[&["42"]]));
        assert_eq!(
            run(&mut c, "SELECT add1(k), a FROM l WHERE add1(k) = 2"),
            rows(&[&["2", "x"]])
        );
        // arguments keep their own meaning: `b` here is the caller's, not the parameter
        run(&mut c, "CREATE MACRO sub(a, b) AS a - b");
        assert_eq!(
            run(&mut c, "SELECT sub(b + 1, 2) FROM r WHERE b = 20"),
            rows(&[&["19"]])
        );
        assert_eq!(run(&mut c, "SELECT sub(b, b) FROM (SELECT 7 AS b) t"), rows(&[&["0"]]));
        // the result is named after the call
        assert_eq!(c.query("SELECT add1(1)").unwrap().names, vec!["add1(1)"]);
        // aggregates and groups see through macros
        run(&mut c, "CREATE MACRO total(x) AS sum(x) + 1");
        assert_eq!(run(&mut c, "SELECT total(b) FROM r"), rows(&[&["112"]]));
        assert_eq!(
            run(
                &mut c,
                "SELECT add1(k), count(*) FROM r GROUP BY add1(k) ORDER BY 1 NULLS LAST"
            ),
            rows(&[&["3", "2"], &["4", "1"], &["NULL", "1"]])
        );
        // defaults, named arguments and overloads
        run(&mut c, "CREATE MACRO d(a, b := 10) AS a + b");
        assert_eq!(
            run(&mut c, "SELECT d(1), d(1, 2), d(1, b := 3)"),
            rows(&[&["11", "3", "4"]])
        );
        assert!(error(&mut c, "SELECT d(b := 6, 3)").contains("positional argument following named argument"));
        run(&mut c, "CREATE MACRO o(a) AS 1, (a, b) AS 2");
        assert_eq!(run(&mut c, "SELECT o(0), o(0, 0)"), rows(&[&["1", "2"]]));
        assert!(error(&mut c, "SELECT o(1, 2, 3)").contains("does not support the supplied arguments"));
        // typed parameters and return types
        run(&mut c, "CREATE MACRO t1(a INTEGER) AS a + 1, (a VARCHAR) AS 0");
        assert_eq!(run(&mut c, "SELECT t1(1), t1('x')"), rows(&[&["2", "0"]]));
        // a macro shadows a built-in function, which stays reachable through its schema
        run(&mut c, "CREATE MACRO \"sum\"(x) AS system.main.sum(x) + 100");
        assert_eq!(run(&mut c, "SELECT sum(b) FROM r"), rows(&[&["211"]]));
        run(&mut c, "DROP MACRO sum");
        // a macro calling itself ends
        run(&mut c, "CREATE MACRO m1(a) AS a");
        run(&mut c, "CREATE OR REPLACE MACRO m1(a) AS m1(a) + 1");
        assert!(error(&mut c, "SELECT m1(1)").contains("Max expression depth limit"));
        // table macros
        run(
            &mut c,
            "CREATE MACRO big(n) AS TABLE SELECT k, b FROM r WHERE b > n ORDER BY b",
        );
        assert_eq!(run(&mut c, "SELECT b FROM big(25)"), rows(&[&["30"], &["40"]]));
        assert_eq!(
            run(&mut c, "SELECT z.x FROM big(35) z(x, y) JOIN l ON true WHERE a = 'x'"),
            rows(&[&["NULL"]])
        );
        assert!(error(&mut c, "SELECT big(1)").contains("is a table function but it was used as a scalar function"));
        assert!(error(&mut c, "SELECT * FROM add1(1)").contains("with name add1 does not exist"));
        run(&mut c, "CREATE OR REPLACE MACRO big(n) AS TABLE SELECT * FROM big(n)");
        assert!(error(&mut c, "SELECT * FROM big(1)").contains("infinite recursion detected"));
        // checked when they are created
        assert!(error(&mut c, "CREATE MACRO bad(a) AS a + z").contains("\"z\" was not found because the FROM clause"));
        assert!(error(&mut c, "CREATE MACRO add1(a) AS a").contains("already exists"));
        assert!(error(&mut c, "CREATE MACRO dup(a) AS 1, (b) AS 2").contains("Ambiguity in macro overloads"));
        assert!(error(&mut c, "CREATE MACRO nc(a := nextval('s')) AS a").contains("does not exist"));
        // dropping
        assert_eq!(
            error(&mut c, "DROP MACRO TABLE add1"),
            "Catalog Error: Table Macro Function with name add1 does not exist!"
        );
        // a scalar and a table macro can share a name, and DROP MACRO takes the scalar one first
        run(&mut c, "CREATE MACRO twin() AS 1");
        run(&mut c, "CREATE MACRO twin() AS TABLE SELECT 2 AS i");
        assert_eq!(run(&mut c, "SELECT twin(), i FROM twin()"), rows(&[&["1", "2"]]));
        run(&mut c, "DROP MACRO twin");
        assert!(error(&mut c, "SELECT twin()").contains("is a table function"));
        assert_eq!(run(&mut c, "SELECT i FROM twin()"), rows(&[&["2"]]));
        run(&mut c, "DROP MACRO twin");
        run(&mut c, "DROP MACRO add1");
        assert_eq!(
            error(&mut c, "DROP MACRO add1"),
            "Catalog Error: Macro Function with name add1 does not exist!"
        );
        run(&mut c, "DROP MACRO IF EXISTS add1");
    }

    #[test]
    fn prepared_statements() {
        let mut c = join_tables();
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "PREPARE p AS SELECT a, $1 + k FROM l WHERE k = $2");
        assert_eq!(run(&mut c, "EXECUTE p(10, 1)"), rows(&[&["x", "11"]]));
        assert_eq!(run(&mut c, "EXECUTE p(20, 1 + 1) ").len(), 2);
        // it sees later changes
        run(&mut c, "INSERT INTO l VALUES (9, 'nine')");
        assert_eq!(run(&mut c, "EXECUTE p(0, 9)"), rows(&[&["nine", "9"]]));
        // through subqueries, and into INSERT
        run(&mut c, "PREPARE q AS SELECT * FROM (SELECT $1 AS x) t");
        assert_eq!(run(&mut c, "EXECUTE q('hi')"), rows(&[&["hi"]]));
        run(&mut c, "PREPARE i AS INSERT INTO l VALUES ($1, $2)");
        run(&mut c, "EXECUTE i(100, 'hundred')");
        assert_eq!(run(&mut c, "SELECT a FROM l WHERE k = 100"), rows(&[&["hundred"]]));
        // a repeated and a named parameter
        run(&mut c, "PREPARE n AS SELECT $v + $v");
        assert_eq!(run(&mut c, "EXECUTE n(v := 21)"), rows(&[&["42"]]));
        // `?` numbers in the order of the text, whichever way around FROM is
        run(
            &mut c,
            "PREPARE t AS SELECT ?::VARCHAR FROM (SELECT ?::INTEGER) x(i) WHERE i > ?::INTEGER",
        );
        assert_eq!(run(&mut c, "EXECUTE t('hello', 2, 1)"), rows(&[&["hello"]]));
        run(
            &mut c,
            "PREPARE f AS FROM (SELECT ?::INTEGER AS i) SELECT i, ?::VARCHAR",
        );
        assert_eq!(run(&mut c, "EXECUTE f(1, 'one')"), rows(&[&["1", "one"]]));
        // in LIMIT and OFFSET
        run(&mut c, "PREPARE o AS SELECT x FROM range(10) t(x) OFFSET ? LIMIT ?");
        assert_eq!(run(&mut c, "EXECUTE o(3, 1)"), rows(&[&["3"]]));
        assert!(error(&mut c, "PREPARE m AS SELECT $1, $name").contains("Mixing named and positional"));
        // without PREPARE there are no values
        assert!(error(&mut c, "SELECT ?").contains("Values were not provided"));
        // what cannot run says so when prepared, not with a confusing error when executed
        assert!(error(&mut c, "PREPARE u AS UPDATE l SET k = ?").contains("not yet ported"));
        // wrong arguments
        assert!(error(&mut c, "EXECUTE p(1)").contains("Values were not provided"));
        assert!(error(&mut c, "EXECUTE p(1, 2, 3)").contains("excess parameters"));
        // checked when prepared
        assert!(error(&mut c, "PREPARE bad AS SELECT * FROM nope").contains("does not exist"));
        // and gone once deallocated
        run(&mut c, "DEALLOCATE p");
        assert_eq!(
            error(&mut c, "EXECUTE p(1, 2)"),
            "Binder Error: Prepared statement \"p\" does not exist"
        );
        // from the API
        let prepared = c.prepare("SELECT b FROM r WHERE b = ?").unwrap();
        let result = c.execute_prepared(&prepared, &[Value::integer(30)]).unwrap();
        assert_eq!(result.value(0, 0), Value::integer(30));
        assert_eq!(
            c.execute_prepared(&prepared, &[Value::integer(1)]).unwrap().row_count(),
            0
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
    fn aggregates() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(
            &mut context,
            "CREATE TABLE t (g INTEGER, v INTEGER, s VARCHAR, d DOUBLE, m DECIMAL(4,1))",
        );
        run(
            &mut context,
            "INSERT INTO t VALUES (1, 10, 'a', 1.5, 1.5), (1, 20, 'b', 2.5, 2.5), (2, NULL, NULL, NULL, NULL), \
             (2, 5, 'c', 0.5, 0.5), (1, 10, 'a', 1.5, 1.5)",
        );
        let rows = |context: &mut ClientContext, sql: &str| {
            run(context, sql).into_iter().map(|r| r.join(",")).collect::<Vec<_>>()
        };
        assert_eq!(
            rows(
                &mut context,
                "SELECT count(*), count(v), sum(v), min(v), max(s), avg(v), sum(d), sum(m), avg(m) FROM t"
            ),
            ["5,4,45,5,c,11.25,6.0,6.0,1.5"]
        );
        assert_eq!(
            rows(
                &mut context,
                "SELECT g, sum(v), count(DISTINCT v) FROM t GROUP BY g ORDER BY g"
            ),
            ["1,40,2", "2,5,1"]
        );
        // groups by position and alias, HAVING, an expression over groups and aggregates
        assert_eq!(
            rows(
                &mut context,
                "SELECT g + 1 AS x, sum(v) * 2 FROM t GROUP BY x HAVING sum(v) > 10"
            ),
            ["2,80"]
        );
        assert_eq!(
            rows(&mut context, "SELECT g + 1, count(*) FROM t GROUP BY 1 ORDER BY 1"),
            ["2,3", "3,2"]
        );
        assert_eq!(
            rows(&mut context, "SELECT g, count(*) AS c FROM t GROUP BY ALL HAVING c > 2"),
            ["1,3"]
        );
        // ORDER BY an aggregate that is not selected
        assert_eq!(
            rows(&mut context, "SELECT g FROM t GROUP BY g ORDER BY count(*)"),
            ["2", "1"]
        );
        // FILTER, NULL groups, and aggregates over nothing
        assert_eq!(
            rows(&mut context, "SELECT count(*) FILTER (WHERE v > 5), first(s) FROM t"),
            ["3,a"]
        );
        assert_eq!(
            rows(&mut context, "SELECT s, count(*) FROM t GROUP BY s ORDER BY s"),
            ["a,2", "b,1", "c,1", "NULL,1"]
        );
        assert_eq!(
            rows(&mut context, "SELECT count(*), sum(v) FROM t WHERE g > 5"),
            ["0,NULL"]
        );
        assert!(run(&mut context, "SELECT g, sum(v) FROM t WHERE g > 5 GROUP BY g").is_empty());
        assert_eq!(rows(&mut context, "SELECT 1 FROM t GROUP BY ()"), ["1"]);
        // names compare case-insensitively, however they are qualified
        assert_eq!(
            rows(
                &mut context,
                "SELECT t.g AS g, count(*) FROM t GROUP BY G ORDER BY \"T\".\"G\""
            ),
            ["1,3", "2,2"]
        );
        assert_eq!(rows(&mut context, "SELECT '1e308'::FLOAT"), ["inf"]);
        let err = context
            .query("CREATE TABLE c (s VARCHAR COLLATE nocase)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not yet ported"), "{err}");
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        assert_eq!(
            err(&mut context, "SELECT v, sum(v) FROM t"),
            "Binder Error: column v must appear in the GROUP BY clause or must be part of an aggregate function.\n\
             Either add it to the GROUP BY list, or use ANY_VALUE(v) if the exact value of v is not important."
        );
        assert_eq!(
            err(&mut context, "SELECT g FROM t WHERE sum(v) > 1"),
            "Binder Error: WHERE clause cannot contain aggregates!"
        );
        assert_eq!(
            err(&mut context, "SELECT sum(sum(v)) FROM t"),
            "Binder Error: aggregate function calls cannot be nested"
        );
        assert_eq!(
            err(&mut context, "SELECT g FROM t GROUP BY g HAVING v > 1"),
            "Binder Error: column v must appear in the GROUP BY clause or be used in an aggregate function"
        );
    }

    #[test]
    fn expressions_and_sources() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let rows = |context: &mut ClientContext, sql: &str| {
            run(context, sql).into_iter().map(|r| r.join(",")).collect::<Vec<_>>()
        };
        assert_eq!(
            rows(&mut context, "SELECT 7 / 2, 7 // 2, 7 % 2, -7 % 2, 1.0 / 0"),
            ["3.5,3,1,-1,inf"]
        );
        assert_eq!(
            rows(
                &mut context,
                "SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' END, COALESCE(NULL, 2, 3), 2 BETWEEN 1 AND 3"
            ),
            ["b,2,true"]
        );
        assert_eq!(
            rows(&mut context, "SELECT 1 IN (1, 2), 3 NOT IN (1, 2), 1 IN (2, NULL)"),
            ["true,true,NULL"]
        );
        // range, subqueries, and early exit from an endless-looking range
        assert_eq!(
            rows(&mut context, "SELECT count(*), sum(range) FROM range(5)"),
            ["5,10"]
        );
        assert_eq!(
            rows(&mut context, "SELECT * FROM generate_series(1, 7, 3) t(x)"),
            ["1", "4", "7"]
        );
        assert_eq!(
            rows(&mut context, "SELECT i FROM range(100000000000) t(i) LIMIT 2"),
            ["0", "1"]
        );
        assert!(run(&mut context, "SELECT * FROM range(5, 1)").is_empty());
        assert_eq!(
            rows(
                &mut context,
                "SELECT x + 1 FROM (SELECT i * 2 AS x FROM range(3) t(i) ORDER BY i DESC LIMIT 2) s"
            ),
            ["5", "3"]
        );
        assert_eq!(
            rows(&mut context, "SELECT a, a_1 FROM (SELECT 1 AS a, 2 AS a)"),
            ["1,2"]
        );
        // CREATE TABLE AS, including reading the table it replaces
        assert_eq!(
            rows(
                &mut context,
                "CREATE TABLE t AS SELECT i, i % 2 AS odd FROM range(4) r(i)"
            ),
            ["4"]
        );
        assert_eq!(
            rows(&mut context, "SELECT odd, count(*) FROM t GROUP BY odd ORDER BY odd"),
            ["0,2", "1,2"]
        );
        run(
            &mut context,
            "CREATE OR REPLACE TABLE t AS SELECT i FROM t WHERE odd = 1",
        );
        assert_eq!(rows(&mut context, "SELECT i FROM t ORDER BY i"), ["1", "3"]);
        // an existing table is noticed before the query runs, as is LIMIT 0
        run(
            &mut context,
            "CREATE TABLE IF NOT EXISTS t AS SELECT i FROM range(10000000000000000) r(i)",
        );
        assert_eq!(
            context
                .query("CREATE TABLE t AS SELECT i FROM range(10000000000000000) r(i)")
                .unwrap_err()
                .to_string(),
            "Catalog Error: Table with name \"t\" already exists!"
        );
        assert!(
            run(
                &mut context,
                "SELECT * FROM (SELECT sum(i) FROM range(100000000000) r(i)) LIMIT 0"
            )
            .is_empty()
        );
        // an alias wins in HAVING, and a group alias may be used in an aggregate
        assert_eq!(
            rows(
                &mut context,
                "SELECT b, sum(a) AS a FROM (VALUES (1, 0), (1, 1)) v(a, b) GROUP BY b HAVING a > 0 ORDER BY ALL"
            ),
            ["0,1", "1,1"]
        );
        assert_eq!(
            rows(&mut context, "SELECT i % 2 AS k, sum(k) FROM t GROUP BY k"),
            ["1,2"]
        );
        assert_eq!(
            rows(&mut context, "SELECT i AS x FROM t ORDER BY alias.x DESC"),
            ["3", "1"]
        );
        let err = context
            .query("SET debug_window_mode='unknown'")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrecognized value"), "{err}");
        // defaults, NOT NULL, schemas and DROP
        run(&mut context, "CREATE SCHEMA s");
        run(
            &mut context,
            "CREATE TABLE s.d (a INTEGER NOT NULL, b INTEGER DEFAULT 41 + 1, c INTEGER)",
        );
        run(&mut context, "INSERT INTO s.d (a) VALUES (1)");
        run(&mut context, "INSERT INTO s.d VALUES (2, DEFAULT, 3)");
        assert_eq!(
            rows(&mut context, "SELECT * FROM s.d ORDER BY a"),
            ["1,42,NULL", "2,42,3"]
        );
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        assert_eq!(
            err(&mut context, "INSERT INTO s.d (b) VALUES (1)"),
            "Constraint Error: NOT NULL constraint failed: d.a"
        );
        assert_eq!(
            err(&mut context, "SELECT 1 // 0"),
            "Invalid Input Error: Division by zero in expression (1 // 0). Use TRY(...) to return NULL for this \
             expression, or SET error_on_division_by_zero=false to return NULL for all divisions by zero."
        );
        assert_eq!(
            err(&mut context, "SELECT * FROM range(1, 2, 0)"),
            "Binder Error: interval cannot be 0!"
        );
        assert!(err(&mut context, "DROP SCHEMA s").starts_with("Dependency Error: Cannot drop entry \"s\""));
        run(&mut context, "DROP TABLE s.d");
        run(&mut context, "DROP SCHEMA s");
        run(&mut context, "DROP TABLE IF EXISTS nope");
        assert_eq!(
            err(&mut context, "DROP TABLE nope"),
            "Catalog Error: Table with name nope does not exist!"
        );
    }

    #[test]
    fn filtered_strings_read_the_right_rows() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (i INTEGER, s VARCHAR)");
        run(&mut context, "INSERT INTO t VALUES (1, '10'), (2, '20'), (3, '30')");
        // the filter leaves a dictionary vector, whose rows the cast has to follow
        assert_eq!(
            run(&mut context, "SELECT CAST(s AS INTEGER) FROM t WHERE i > 1"),
            vec![vec!["20"], vec!["30"]]
        );
        assert_eq!(
            run(&mut context, "SELECT s IS DISTINCT FROM '30' FROM t WHERE i > 1"),
            vec![vec!["true"], vec!["false"]]
        );
    }

    #[test]
    fn insert_casts_below_order_and_limit() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (i INTEGER)");
        // `range` is a BIGINT, and the root of the plan is no projection to put the cast in
        run(
            &mut context,
            "INSERT INTO t SELECT range FROM range(3) ORDER BY range DESC",
        );
        run(&mut context, "INSERT INTO t SELECT range FROM range(5) LIMIT 1");
        assert_eq!(
            run(&mut context, "SELECT i FROM t ORDER BY i"),
            [["0"], ["0"], ["1"], ["2"]]
        );
    }

    #[test]
    fn negative_nan_keeps_its_sign() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut context,
                "SELECT '-NaN'::DOUBLE::VARCHAR, '-NaN'::FLOAT::VARCHAR, 'NaN'::DOUBLE::VARCHAR, \
                 'NaN'::FLOAT::VARCHAR"
            ),
            [["-nan", "-nan", "nan", "nan"]]
        );
    }

    #[test]
    fn decimal_arithmetic_is_exact() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut context,
                "SELECT '0.543215432154321'::DECIMAL(15,15) + 10000::DECIMAL(20,15), \
                 '0.3'::DECIMAL(2,1) - '0.1'::DECIMAL(2,1), '0.1'::DECIMAL(2,1) * '0.2'::DECIMAL(2,1), \
                 -'0.5'::DECIMAL(2,1), +'0.5'::DECIMAL(2,1)"
            ),
            [["10000.543215432154321", "0.2", "0.02", "-0.5", "0.5"]]
        );
        assert_eq!(
            run(&mut context, "SELECT 12345678901111111 % 2.0, 10 % 2.4, -10.0 % 2.4"),
            [["1.0", "0.4", "-0.4"]]
        );
        // the required width exceeds the maximum decimal width: modulo falls back to DOUBLE
        let result = context
            .query(
                "SELECT 10000000000000000000000000000000000001::DECIMAL(38,0) % 0.00000000000000000000000000000000004",
            )
            .unwrap();
        assert_eq!(result.types[0], LogicalType::DOUBLE);
        let result = context.query("SELECT 1.5 + 1, 1.5 * 2, NULL + 1.5").unwrap();
        assert_eq!(result.types[0], LogicalType::decimal(12, 1));
        assert_eq!(result.types[1], LogicalType::decimal(12, 1));
    }

    #[test]
    fn decimal_overflow_matches_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let big = "99999999999999999999999999999999999999::DECIMAL(38,0)";
        let err = context.query(&format!("SELECT {big} + {big}")).unwrap_err().to_string();
        assert!(err.contains("Overflow in addition of DECIMAL(38) ("), "{err}");
        let err = context
            .query(&format!("SELECT -{big} - {big}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Overflow in subtract of DECIMAL(38) ("), "{err}");
        let err = context.query(&format!("SELECT {big} * {big}")).unwrap_err().to_string();
        assert!(err.contains("Overflow in multiplication of DECIMAL(38) ("), "{err}");
    }

    #[test]
    fn having_prefers_a_group_to_an_alias() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t1 (a INTEGER)");
        run(&mut context, "INSERT INTO t1 VALUES (42), (84)");
        assert_eq!(
            run(&mut context, "SELECT a + 1 AS a FROM t1 GROUP BY a HAVING a = 42"),
            [["43"]]
        );
        run(&mut context, "CREATE TABLE t2 (a INTEGER)");
        run(&mut context, "INSERT INTO t2 VALUES (42), (84), (42)");
        assert_eq!(
            run(
                &mut context,
                "SELECT a AS b, sum(a) AS a FROM t2 GROUP BY b HAVING a = 42"
            ),
            [["42", "84"]]
        );
        // a name that is no group stays an alias
        assert_eq!(
            run(
                &mut context,
                "SELECT a, sum(a) AS s FROM t2 GROUP BY a HAVING s = 84 ORDER BY a"
            ),
            [["42", "84"], ["84", "84"]]
        );
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
