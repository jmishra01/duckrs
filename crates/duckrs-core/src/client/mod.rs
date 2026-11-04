// Ported from duckdb/src/main/{database,client_context,materialized_query_result}.cpp
// @ rust-port-baseline
//
//! Databases and client contexts (DuckDB's `src/main`): running SQL text statement by statement through the
//! parser, binder, planner and executor.

pub mod settings;
mod settings_table;

use crate::catalog::{
    Catalog, CatalogSnapshot, DEFAULT_CATALOG, OnCreateConflict, SequenceData, TableOrView, macro_type_name,
    missing_entry,
};
use crate::execution::alter::{execute_alter, execute_create_index};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::physical::PhysicalResultCollector;
use crate::execution::{create_plan, execute_plan};
use crate::planner::binder::{
    BindOptions, Binder, DefaultNullOrder, ParameterValues, bind_constant_expression, schema_of,
};
use crate::planner::binder::{SessionValues, enum_type_from, is_builtin_type};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderType;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ParserOptions;
use duckrs_parser::ast::{
    CreateInfo, CreateKind, CreateTypeKind, DropInfo, Expr, ExprKind, OnCreateConflict as AstConflict, PragmaInfo,
    Statement, sql_quoted_identifier,
};
use duckrs_parser::ast::{TransactionModifier, TransactionType};
use parking_lot::{Mutex, RwLock};
use settings::SettingValues;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A database (`DatabaseInstance`): its catalog, its global settings and, with Phase 9, its
/// storage.
#[derive(Debug)]
pub struct DatabaseInstance {
    pub catalog: Catalog,
    settings: RwLock<SettingValues>,
    transactions: Mutex<OpenTransactions>,
    /// held to read while a SELECT runs, and to write while anything else does
    statements: RwLock<()>,
}

/// Which connections have a transaction open. There is no MVCC until Phase 10, so transactions
/// do not isolate connections from each other: what would need that fails as not yet ported
/// rather than show one connection's uncommitted changes to another.
#[derive(Debug, Default)]
struct OpenTransactions {
    open: HashSet<u64>,
    /// the connection whose transaction has uncommitted changes
    writer: Option<u64>,
}

impl DatabaseInstance {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog: Catalog::new(DEFAULT_CATALOG),
            settings: RwLock::new(SettingValues::default()),
            transactions: Mutex::new(OpenTransactions::default()),
            statements: RwLock::new(()),
        })
    }
}

/// Pragmas that only switch DuckDB's debug verification, profiling, progress output or
/// checkpointing, none of which changes a result; duckrs runs them as no-ops.
const NO_OP_PRAGMAS: &[&str] = &[
    "enable_verification",
    "disable_verification",
    "enable_optimizer",
    "disable_optimizer",
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
    /// identifies the connection among the database's connections
    id: u64,
    /// the transaction BEGIN started (`TransactionContext`); without one, each statement is its
    /// own (auto-commit)
    transaction: Option<ActiveTransaction>,
    /// session settings (`ClientConfig`)
    settings: SettingValues,
    /// the statements of PREPARE (`ClientData::prepared_statements`)
    prepared_statements: HashMap<String, Arc<PreparedStatementData>>,
    /// the parameters of the prepared statement running now
    parameters: Option<Arc<ParameterValues>>,
    /// what SET VARIABLE set (`ClientConfig::user_variables`), by lower-case name
    variables: HashMap<String, Value>,
}

/// A transaction started with BEGIN.
#[derive(Debug)]
struct ActiveTransaction {
    read_only: bool,
    /// a statement failed in a way that leaves only ROLLBACK (`ValidChecker`)
    invalidated: bool,
    /// the transaction's `TransactionInvalidationPolicy`
    invalidation_policy: String,
    /// what ROLLBACK goes back to, from before the transaction's first write
    snapshot: Option<CatalogSnapshot>,
}

impl ActiveTransaction {
    /// `ClientContext::ApplyInvalidationPolicy`: any error ends a transaction, unless its policy
    /// spares the errors that cannot have changed anything (`Exception::InvalidatesTransaction`).
    fn apply_invalidation_policy(&mut self, kind: ErrorKind) {
        let syntactic = matches!(
            kind,
            ErrorKind::Binder
                | ErrorKind::Catalog
                | ErrorKind::Connection
                | ErrorKind::ParameterNotAllowed
                | ErrorKind::Parser
                | ErrorKind::Permission
                | ErrorKind::ResourceInUse
        );
        if !(syntactic && self.invalidation_policy == "SYNTACTIC_ERRORS_DO_NOT_INVALIDATE") {
            self.invalidated = true;
        }
    }
}

/// Whether `function` is `checkpoint` or `force_checkpoint`, and if so whether it forces, and of
/// which database.
fn checkpoint_call(function: &Expr) -> Option<(bool, Option<&Expr>)> {
    let ExprKind::Function(call) = &function.kind else {
        return None;
    };
    let name = call.name.name.to_lowercase();
    if !matches!(name.as_str(), "checkpoint" | "force_checkpoint") || call.args.len() > 1 {
        return None;
    }
    Some((name == "force_checkpoint", call.args.first().map(|a| &a.expr)))
}

/// Whether a statement changes the database (`StatementProperties::IsReadOnly`).
fn is_write(statement: &Statement) -> bool {
    match statement {
        Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Drop(_)
        | Statement::Alter(_) => true,
        // temporary objects are private to the connection
        Statement::CreateTable(info) => !info.temporary,
        Statement::Create(info) => !info.temporary,
        _ => false,
    }
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
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self {
            db,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            transaction: None,
            settings: SettingValues::default(),
            prepared_statements: HashMap::new(),
            parameters: None,
            variables: HashMap::new(),
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

    /// What `current_setting` and `getvariable` read.
    fn session_values(&self) -> Arc<SessionValues> {
        let db = self.db.clone();
        let session = self.settings.clone();
        Arc::new(SessionValues {
            variables: self.variables.clone(),
            setting: Box::new(move |name| settings::get_setting(name, &db.settings.read(), &session)),
        })
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
            single_arrow_lambdas: self.setting_text("lambda_syntax") == "ENABLE_SINGLE_ARROW",
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
        let statements = match duckrs_parser::parse_ast_with(sql, &self.parser_options()) {
            Ok(statements) => statements,
            Err(error) => {
                if let Some(transaction) = &mut self.transaction {
                    transaction.apply_invalidation_policy(error.kind);
                }
                return Err(error);
            }
        };
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

    /// `ClientContext::RunTransactionStatementInternal`.
    fn run_transaction_statement(
        &mut self,
        transaction_type: TransactionType,
        modifier: TransactionModifier,
    ) -> Result<MaterializedQueryResult> {
        let error = |message: &str| Error::new(ErrorKind::Transaction, message);
        match transaction_type {
            TransactionType::Begin => {
                match &self.transaction {
                    Some(transaction) if transaction.invalidated => return Err(error(INVALIDATED_TRANSACTION)),
                    Some(_) => return Err(error("cannot start a transaction within a transaction")),
                    None => {}
                }
                self.db.transactions.lock().open.insert(self.id);
                self.transaction = Some(ActiveTransaction {
                    read_only: modifier == TransactionModifier::ReadOnly,
                    invalidated: false,
                    invalidation_policy: self.setting_text("default_transaction_invalidation_policy"),
                    snapshot: None,
                });
            }
            TransactionType::Commit => {
                let Some(transaction) = self.transaction.take() else {
                    return Err(error("cannot commit - no transaction is active"));
                };
                // COMMIT of an invalidated transaction rolls it back
                let rollback = transaction.invalidated;
                self.end_transaction(transaction, rollback);
            }
            TransactionType::Rollback => {
                let Some(transaction) = self.transaction.take() else {
                    return Err(error("cannot rollback - no transaction is active"));
                };
                self.end_transaction(transaction, true);
            }
        }
        Ok(Self::success())
    }

    fn end_transaction(&self, transaction: ActiveTransaction, rollback: bool) {
        if rollback && let Some(snapshot) = transaction.snapshot {
            self.db.catalog.restore(snapshot);
        }
        let mut transactions = self.db.transactions.lock();
        transactions.open.remove(&self.id);
        if transactions.writer == Some(self.id) {
            transactions.writer = None;
        }
    }

    /// Fails for what would need other connections to be isolated from this one, or the other
    /// way around.
    fn check_other_connections(&self, write: bool) -> Result<()> {
        let transactions = self.db.transactions.lock();
        if transactions.writer.is_some_and(|writer| writer != self.id) {
            return Err(crate::unported(
                "running a statement while another connection has uncommitted changes",
            ));
        }
        if write && transactions.open.iter().any(|id| *id != self.id) {
            return Err(crate::unported(
                "changing the database while another connection has a transaction open",
            ));
        }
        Ok(())
    }

    /// Runs one statement within the current transaction, or as a transaction of its own
    /// (`ClientContext::PendingStatementOrPreparedStatementInternal` + `CleanupInternal`).
    fn run_statement(&mut self, statement: &Statement) -> Result<MaterializedQueryResult> {
        match statement {
            Statement::Transaction {
                transaction_type,
                modifier,
            } => return self.run_transaction_statement(*transaction_type, *modifier),
            // EXECUTE comes back here with the statement it runs
            Statement::Prepare { .. } | Statement::Execute { .. } | Statement::Deallocate(_) => {
                return self.execute_statement(statement);
            }
            _ => {}
        }
        if self.transaction.as_ref().is_some_and(|t| t.invalidated) {
            return Err(Error::new(ErrorKind::Transaction, INVALIDATED_TRANSACTION));
        }
        let write = is_write(statement);
        self.check_other_connections(write)?;
        // what a failed statement outside of a transaction undoes: INSERT is the one that can
        // fail halfway through
        let mut appending = None;
        if write {
            match &mut self.transaction {
                Some(transaction) if transaction.read_only => {
                    return Err(Error::new(
                        ErrorKind::Transaction,
                        format!(
                            "Cannot write to database {} - transaction is launched in read-only mode",
                            sql_quoted_identifier(DEFAULT_CATALOG)
                        ),
                    ));
                }
                Some(transaction) if transaction.snapshot.is_none() => {
                    transaction.snapshot = Some(self.db.catalog.snapshot());
                    self.db.transactions.lock().writer = Some(self.id);
                }
                _ => {}
            }
            if let Statement::Insert(insert) = statement
                && let Ok(table) = self
                    .db
                    .catalog
                    .get_table(schema_of(&insert.table).ok().flatten(), &insert.table.name)
            {
                let rows = table.storage.read().len();
                appending = Some((table, rows));
            }
        }
        // until Phase 10 isolates connections, a statement that writes runs alone: UPDATE and
        // DELETE rewrite tables, which would pull the rows from under a scan
        let db = self.db.clone();
        let result = if matches!(statement, Statement::Select(_)) {
            let _reading = db.statements.read();
            self.execute_statement(statement)
        } else {
            let _writing = db.statements.write();
            self.execute_statement(statement)
        };
        if let Err(error) = &result {
            if let Some(transaction) = &mut self.transaction {
                transaction.apply_invalidation_policy(error.kind);
            }
            if let Some((table, rows)) = appending {
                table.truncate(rows);
            }
        }
        result
    }

    /// `CheckpointFunction`. In memory there is nothing to write, so only its errors are left;
    /// Phase 12 brings the rest with the storage.
    fn checkpoint(&mut self, function: &Expr) -> Result<MaterializedQueryResult> {
        let (force, argument) = checkpoint_call(function).expect("checked by the caller");
        if let Some(argument) = argument {
            let database = evaluate_scalar(&bind_constant_expression(&self.db.catalog, argument)?)?;
            if database.is_null() {
                return Err(Error::new(ErrorKind::Binder, "Database cannot be NULL"));
            }
            let database = database.try_to_string()?;
            if database != DEFAULT_CATALOG {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Database \"{database}\" not found"),
                ));
            }
        }
        if let Some(transaction) = &self.transaction {
            if force {
                return Err(Error::new(
                    ErrorKind::Transaction,
                    "Cannot FORCE CHECKPOINT: the current transaction has been started for this database",
                ));
            }
            if transaction.snapshot.is_some() {
                return Err(Error::new(
                    ErrorKind::Transaction,
                    "Cannot CHECKPOINT: the current transaction has transaction local changes",
                ));
            }
        }
        Ok(Self::success())
    }

    /// `ClientContext::PendingStatementOrPreparedStatement` + `ExecutePendingQueryInternal`.
    fn execute_statement(&mut self, statement: &Statement) -> Result<MaterializedQueryResult> {
        match statement {
            Statement::SetVariable { name, value, scope } if *scope == duckrs_parser::ast::SetScope::Variable => {
                // a query, because the value can be a subquery
                let query = duckrs_parser::parse_ast(&format!("SELECT {value}"))?.remove(0);
                let result = self.execute_statement(&query)?;
                let value = if result.row_count() == 0 {
                    Value::sql_null()
                } else {
                    result.value(0, 0)
                };
                self.variables.insert(name.to_lowercase(), value);
                return Ok(Self::success());
            }
            Statement::ResetVariable { name, scope } if *scope == duckrs_parser::ast::SetScope::Variable => {
                self.variables.remove(&name.to_lowercase());
                return Ok(Self::success());
            }
            Statement::SetVariable { name, value, scope } => {
                // `Binder::Bind(SetVariableStatement)` + `PhysicalSet`
                let value = evaluate_scalar(&bind_constant_expression(&self.db.catalog, value)?)?;
                if name.eq_ignore_ascii_case("current_transaction_invalidation_policy")
                    && let Some(transaction) = &mut self.transaction
                {
                    transaction.invalidation_policy = value.try_to_string()?.to_uppercase();
                }
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
            Statement::Alter(info) => {
                let mut binder = Binder::new(&self.db.catalog);
                let alter = binder.bind_alter(info)?;
                execute_alter(alter)?;
                return Ok(Self::success());
            }
            Statement::Call(function) if checkpoint_call(function).is_some() => {
                return self.checkpoint(function);
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
            Statement::Create(info) if matches!(info.kind, CreateKind::Index { .. }) => {
                let mut binder = Binder::new(&self.db.catalog);
                if let Some((table, index)) = binder.bind_create_index(info)? {
                    execute_create_index(&table, index)?;
                }
                return Ok(Self::success());
            }
            Statement::Create(info) if matches!(info.kind, CreateKind::Type(_)) => {
                self.create_type(info)?;
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
        binder.session = Some(self.session_values());
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

impl Drop for ClientContext {
    /// A connection that goes away rolls back what it did not commit.
    fn drop(&mut self) {
        if let Some(transaction) = self.transaction.take() {
            self.end_transaction(transaction, true);
        }
    }
}

/// `ErrorType::INVALIDATED_TRANSACTION`.
const INVALIDATED_TRANSACTION: &str = "Current transaction is aborted (please ROLLBACK)";

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

    /// `CREATE TYPE` (`Binder::Bind(CreateStatement)` + `PhysicalCreateType`).
    fn create_type(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::Type(kind) = &info.kind else {
            unreachable!("checked by the caller");
        };
        if info.temporary {
            return Err(crate::unported("CREATE TEMPORARY TYPE"));
        }
        let ty = match kind {
            CreateTypeKind::Enum(values) => enum_type_from(values.clone())?,
            CreateTypeKind::EnumQuery(query) => {
                let result = self.execute_statement(&Statement::Select((**query).clone()))?;
                if result.column_count() != 1 {
                    return Err(Error::new(ErrorKind::Binder, "The query must return a single column"));
                }
                let mut values = Vec::with_capacity(result.row_count());
                for row in 0..result.row_count() {
                    let value = result.value(0, row);
                    if value.is_null() {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "Attempted to create ENUM type with NULL value!",
                        ));
                    }
                    values.push(value.try_to_string()?);
                }
                enum_type_from(values)?
            }
            CreateTypeKind::Alias(target) => {
                let binder = Binder::with_options(&self.db.catalog, self.bind_options());
                binder.bind_type(target)?
            }
        };
        let schema = self.db.catalog.schema_for(schema_of(&info.name)?)?;
        if is_builtin_type(&info.name.name) {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Type with name \"{}\" already exists!", info.name.name),
            ));
        }
        schema.create_type(&info.name.name, ty, on_create_conflict(info.on_conflict))
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
            "INDEX" => {
                let schema = schema_of(&info.name).map_err(|_| crate::unported("this form of DROP INDEX"))?;
                let dropped = match catalog.schema_for(schema) {
                    Ok(schema) => schema.drop_index(&info.name.name),
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry("Index", &info.name.name));
                }
                Ok(())
            }
            "SEQUENCE" => {
                let schema = schema_of(&info.name).map_err(|_| crate::unported("this form of DROP SEQUENCE"))?;
                let dropped = match catalog.schema_for(schema) {
                    Ok(schema) => schema.drop_sequence_checked(&info.name.name, info.cascade)?,
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry("Sequence", &info.name.name));
                }
                Ok(())
            }
            "TYPE" => {
                let schema = schema_of(&info.name).map_err(|_| crate::unported("this form of DROP TYPE"))?;
                let dropped = match catalog.schema_for(schema) {
                    Ok(schema) => schema.drop_type_checked(&info.name.name, info.cascade)?,
                    Err(_) if info.if_exists => false,
                    Err(error) => return Err(error),
                };
                if !dropped && !info.if_exists {
                    return Err(missing_entry("Type", &info.name.name));
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
    fn single_arrow_lambdas_need_the_setting() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "SET lambda_syntax='DISABLE_SINGLE_ARROW'");
        assert!(error(&mut c, "SELECT list_transform([1], x -> x + 1)").contains("Deprecated lambda arrow"));
        run(&mut c, "SELECT list_transform([1], lambda x: x + 1)");
        run(&mut c, "SET lambda_syntax='ENABLE_SINGLE_ARROW'");
        run(&mut c, "SELECT list_transform([1], x -> x + 1)");
    }

    #[test]
    fn tables_keep_the_sequences_and_types_they_use() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE SEQUENCE seq");
        run(&mut c, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
        run(&mut c, "CREATE TABLE t(i INTEGER DEFAULT nextval('seq'), m mood)");
        assert!(error(&mut c, "DROP SEQUENCE seq").contains("table \"t\" depends on sequence \"seq\""));
        assert!(error(&mut c, "DROP TYPE mood").contains("table \"t\" depends on type \"mood\""));
        run(&mut c, "DROP SEQUENCE seq CASCADE");
        assert!(error(&mut c, "SELECT * FROM t").contains("does not exist"));
    }

    #[test]
    fn a_missing_table_suggests_the_closest_one() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE hello(i INTEGER)");
        let message = c.query("SELECT * FROM helloo").unwrap_err().to_string();
        assert!(message.contains("Did you mean \"hello\"?"), "{message}");
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
        let sorted = |c: &mut ClientContext, sql: &str| {
            let mut rows = run(c, sql);
            rows.sort();
            rows
        };
        assert_eq!(
            sorted(&mut c, "SELECT * FROM l, r JOIN t ON a = 'x'"),
            sorted(&mut c, "SELECT * FROM l, r, t WHERE a = 'x'")
        );
        assert_eq!(
            sorted(&mut c, "SELECT * FROM l, (SELECT l.k) s"),
            sorted(&mut c, "SELECT *, k FROM l")
        );
        assert_eq!(
            sorted(&mut c, "SELECT * FROM l, (SELECT 1 FROM r JOIN t ON a = 'x') s"),
            sorted(&mut c, "SELECT l.*, 1 FROM l, r, t WHERE a = 'x'")
        );
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
        assert!(
            message.contains("Ambiguous reference to column name \"k\""),
            "{message}"
        );
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
        run(&mut c, "PREPARE u AS UPDATE l SET k = k + ? WHERE k = 1");
        assert_eq!(run(&mut c, "EXECUTE u(100)"), rows(&[&["1"]]));
        assert_eq!(run(&mut c, "SELECT count(*) FROM l WHERE k = 101"), rows(&[&["1"]]));
        run(&mut c, "UPDATE l SET k = 1 WHERE k = 101");
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
    fn transactions() {
        let db = DatabaseInstance::new();
        let mut c = ClientContext::new(db.clone());
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE TABLE t (i INTEGER NOT NULL)");
        run(&mut c, "INSERT INTO t VALUES (1)");
        // ROLLBACK takes back rows and catalog changes, and brings back what was dropped
        run(&mut c, "BEGIN TRANSACTION");
        run(&mut c, "INSERT INTO t VALUES (2)");
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["2"]]));
        run(&mut c, "CREATE TABLE gone (i INTEGER)");
        run(&mut c, "CREATE VIEW v AS SELECT 1");
        run(&mut c, "ROLLBACK");
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["1"]]));
        assert!(error(&mut c, "SELECT * FROM gone").contains("does not exist"));
        assert!(error(&mut c, "SELECT * FROM v").contains("does not exist"));
        run(&mut c, "BEGIN");
        run(&mut c, "DROP TABLE t");
        assert!(error(&mut c, "SELECT * FROM t").contains("does not exist"));
        run(&mut c, "ROLLBACK");
        assert_eq!(run(&mut c, "SELECT i FROM t"), rows(&[&["1"]]));
        // rows beyond a chunk go too, and appending goes on from there
        run(&mut c, "BEGIN");
        run(&mut c, "INSERT INTO t SELECT 5 FROM range(5000)");
        run(&mut c, "ROLLBACK");
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["1"]]));
        run(&mut c, "INSERT INTO t VALUES (2)");
        assert_eq!(run(&mut c, "SELECT i FROM t ORDER BY 1"), rows(&[&["1"], &["2"]]));
        // COMMIT keeps them
        run(&mut c, "BEGIN");
        run(&mut c, "INSERT INTO t VALUES (3)");
        run(&mut c, "COMMIT");
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        // a failed statement outside of a transaction leaves nothing behind
        assert!(
            error(
                &mut c,
                "INSERT INTO t SELECT CASE WHEN x = 3000 THEN NULL ELSE 1 END FROM range(4000) r(x)"
            )
            .contains("NOT NULL")
        );
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        // any error leaves only ROLLBACK, and COMMIT then rolls back
        for bad in [
            "SELECT * FROM nope",
            "INSER INTO t VALUES (1)",
            "INSERT INTO t VALUES (NULL)",
        ] {
            run(&mut c, "BEGIN");
            run(&mut c, "INSERT INTO t VALUES (4)");
            c.query(bad).unwrap_err();
            assert_eq!(
                error(&mut c, "SELECT 1"),
                "TransactionContext Error: Current transaction is aborted (please ROLLBACK)",
                "{bad}"
            );
            run(&mut c, "COMMIT");
            assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        }
        // unless the policy spares what cannot have changed anything
        run(
            &mut c,
            "SET default_transaction_invalidation_policy = 'SYNTACTIC_ERRORS_DO_NOT_INVALIDATE'",
        );
        run(&mut c, "BEGIN");
        c.query("SELECT * FROM nope").unwrap_err();
        c.query("INSER INTO t").unwrap_err();
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        c.query("INSERT INTO t VALUES (NULL)").unwrap_err();
        assert!(error(&mut c, "SELECT 1").contains("Current transaction is aborted"));
        run(&mut c, "ROLLBACK");
        run(&mut c, "RESET default_transaction_invalidation_policy");
        // misuse
        assert_eq!(
            error(&mut c, "COMMIT"),
            "TransactionContext Error: cannot commit - no transaction is active"
        );
        assert_eq!(
            error(&mut c, "ROLLBACK"),
            "TransactionContext Error: cannot rollback - no transaction is active"
        );
        run(&mut c, "BEGIN");
        assert_eq!(
            error(&mut c, "BEGIN"),
            "TransactionContext Error: cannot start a transaction within a transaction"
        );
        run(&mut c, "ROLLBACK");
        run(&mut c, "BEGIN TRANSACTION READ ONLY");
        assert!(error(&mut c, "INSERT INTO t VALUES (9)").contains("transaction is launched in read-only mode"));
        run(&mut c, "COMMIT");
        // without isolation, what would need it says so instead of showing uncommitted rows
        let mut other = ClientContext::new(db.clone());
        run(&mut c, "BEGIN");
        assert_eq!(run(&mut other, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        assert!(error(&mut other, "INSERT INTO t VALUES (9)").contains("not yet ported"));
        run(&mut c, "INSERT INTO t VALUES (10)");
        assert!(error(&mut other, "SELECT * FROM t").contains("not yet ported"));
        run(&mut c, "COMMIT");
        assert_eq!(run(&mut other, "SELECT count(*) FROM t"), rows(&[&["4"]]));
        // a connection that goes away rolls back
        run(&mut other, "BEGIN");
        run(&mut other, "INSERT INTO t VALUES (11)");
        drop(other);
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["4"]]));
        run(&mut c, "INSERT INTO t VALUES (12)");
    }

    #[test]
    fn column_references() {
        let mut c = join_tables();
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE SCHEMA s");
        run(&mut c, "CREATE TABLE s.l (k INTEGER)");
        run(&mut c, "INSERT INTO s.l VALUES (7)");
        // schema and catalog qualified
        assert_eq!(run(&mut c, "SELECT s.l.k FROM s.l"), rows(&[&["7"]]));
        assert_eq!(run(&mut c, "SELECT memory.s.l.k FROM s.l"), rows(&[&["7"]]));
        assert!(error(&mut c, "SELECT s.l.k FROM s.l t").contains("not found"));
        assert!(error(&mut c, "SELECT x.l.k FROM s.l").contains("not found"));
        // two tables of one name, told apart by their schema
        assert_eq!(
            run(&mut c, "SELECT main.l.a, s.l.k FROM main.l, s.l WHERE main.l.k = 1"),
            rows(&[&["x", "7"]])
        );
        assert_eq!(run(&mut c, "SELECT * FROM s.l, main.l WHERE main.l.k = 1").len(), 1);
        // rowid
        assert_eq!(
            run(&mut c, "SELECT rowid, a FROM l ORDER BY rowid"),
            rows(&[&["0", "x"], &["1", "y"], &["2", "z"], &["3", "n"]])
        );
        assert_eq!(run(&mut c, "SELECT a FROM l WHERE rowid = 2"), rows(&[&["z"]]));
        assert_eq!(run(&mut c, "SELECT l.rowid + 1 FROM l WHERE a = 'y'"), rows(&[&["2"]]));
        assert_eq!(c.query("SELECT * FROM l").unwrap().column_count(), 2);
        assert!(error(&mut c, "SELECT rowid FROM (SELECT 1) t").contains("not found"));
        assert!(error(&mut c, "SELECT rowid FROM l, r").contains("Ambiguous reference"));
        run(&mut c, "CREATE TABLE own (rowid VARCHAR)");
        run(&mut c, "INSERT INTO own VALUES ('mine')");
        assert_eq!(run(&mut c, "SELECT rowid FROM own"), rows(&[&["mine"]]));
        // rows of a later chunk
        run(&mut c, "CREATE TABLE big AS SELECT range AS i FROM range(5000)");
        assert_eq!(
            run(&mut c, "SELECT rowid, i FROM big WHERE i = 4097"),
            rows(&[&["4097", "4097"]])
        );
        // positions
        assert_eq!(run(&mut c, "SELECT #2 FROM l WHERE #1 = 1"), rows(&[&["x"]]));
        assert_eq!(
            run(&mut c, "SELECT #3 FROM (SELECT 1 a, 2 b) x, (SELECT 3 c) y"),
            rows(&[&["3"]])
        );
        assert_eq!(
            run(&mut c, "SELECT #1, count(*) FROM r WHERE #1 = 2 GROUP BY #1"),
            rows(&[&["2", "2"]])
        );
        assert!(error(&mut c, "SELECT #5 FROM l").contains("Positional reference 5 out of range (total 2 columns)"));
        // star modifiers
        assert_eq!(run(&mut c, "SELECT * EXCLUDE (k) FROM l WHERE k = 1"), rows(&[&["x"]]));
        assert_eq!(run(&mut c, "SELECT * EXCLUDE k FROM r WHERE b = 30"), rows(&[&["30"]]));
        assert_eq!(
            run(&mut c, "SELECT * REPLACE (k + 1 AS k) FROM l WHERE a = 'x'"),
            rows(&[&["2", "x"]])
        );
        let renamed = c.query("SELECT * RENAME (k AS key) FROM l").unwrap();
        assert_eq!(renamed.names, vec!["key", "a"]);
        assert_eq!(
            run(
                &mut c,
                "SELECT l.* EXCLUDE (a), r.b FROM l, r WHERE l.k = 1 AND r.b = 30"
            ),
            rows(&[&["1", "30"]])
        );
        assert!(error(&mut c, "SELECT * EXCLUDE (nope) FROM l").contains("in EXCLUDE list not found in FROM clause"));
        assert!(error(&mut c, "SELECT * REPLACE (1 AS nope) FROM l").contains("in REPLACE list not found"));
        assert!(error(&mut c, "SELECT * EXCLUDE (k, a) FROM l").contains("SELECT list is empty"));
        // names in messages
        assert!(error(&mut c, "SELECT k, sum(b) FROM r").contains("column \"k\" must appear in the GROUP BY clause"));
        assert!(error(&mut c, "INSERT INTO r VALUES (1)").contains("table \"r\" has 2 columns but 1 values"));
    }

    #[test]
    fn ordered_aggregates() {
        let mut c = join_tables();
        // the order is what makes this sum exact
        run(&mut c, "CREATE TABLE d (n DOUBLE)");
        run(&mut c, "INSERT INTO d VALUES (9007199254740992), (1), (1), (0)");
        assert_eq!(
            run(&mut c, "SELECT sum(n ORDER BY abs(n))::BIGINT FROM d"),
            rows(&[&["9007199254740994"]])
        );
        assert_eq!(run(&mut c, "SELECT first(b ORDER BY b DESC) FROM r"), rows(&[&["40"]]));
        assert_eq!(run(&mut c, "SELECT last(b ORDER BY b DESC) FROM r"), rows(&[&["20"]]));
        // per group, on another column, with NULLs where they are asked for
        assert_eq!(
            run(
                &mut c,
                "SELECT k, first(b ORDER BY b DESC) FROM r WHERE k = 2 GROUP BY k"
            ),
            rows(&[&["2", "21"]])
        );
        assert_eq!(
            run(&mut c, "SELECT first(b ORDER BY k NULLS FIRST) FROM r"),
            rows(&[&["40"]])
        );
        // with FILTER
        assert_eq!(
            run(
                &mut c,
                "SELECT first(b ORDER BY b) FILTER (WHERE b > 25), count(b ORDER BY b) FROM r"
            ),
            rows(&[&["30", "4"]])
        );
        assert_eq!(run(&mut c, "SELECT count(1 ORDER BY 1)"), rows(&[&["1"]]));
        // two aggregates that differ only in their order are not one
        assert_eq!(
            run(&mut c, "SELECT first(b ORDER BY b), first(b ORDER BY b DESC) FROM r"),
            rows(&[&["20", "40"]])
        );
        // more rows than a chunk
        assert_eq!(
            run(&mut c, "SELECT first(i ORDER BY i DESC) FROM range(5000) t(i)"),
            rows(&[&["4999"]])
        );
    }

    #[test]
    fn math_functions() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut c,
                "SELECT abs(1), abs(-1), abs(NULL), abs(-1.5), abs(-2.5::DOUBLE)"
            ),
            rows(&[&["1", "1", "NULL", "1.5", "2.5"]])
        );
        assert_eq!(run(&mut c, "SELECT abs(-0.0::DOUBLE)"), rows(&[&["0.0"]]));
        assert_eq!(
            c.query("SELECT abs(-2147483647 - 1)").unwrap_err().to_string(),
            "Out of Range Error: Overflow on abs(-2147483648)"
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT isnan('nan'::DOUBLE), isinf('inf'::DOUBLE), isfinite('inf'::FLOAT), isfinite(1.0::DOUBLE)"
            ),
            rows(&[&["true", "true", "false", "true"]])
        );
    }

    #[test]
    fn concat_operator() {
        let mut c = join_tables();
        assert_eq!(
            run(&mut c, "SELECT 'a' || 'b', 'n' || 1, 1 || 2, 'a' || NULL, NULL || NULL"),
            rows(&[&["ab", "n1", "12", "NULL", "NULL"]])
        );
        assert_eq!(
            run(&mut c, "SELECT a || ':' || k FROM l WHERE k = 1"),
            rows(&[&["x:1"]])
        );
        // a NULL in a column gives NULL for its row alone
        assert_eq!(
            run(&mut c, "SELECT k || a FROM l WHERE a IN ('x', 'n') ORDER BY a"),
            rows(&[&["NULL"], &["1x"]])
        );
        let result = c
            .query("SELECT 'long string past the inline limit ' || i FROM range(3000) t(i)")
            .unwrap();
        assert_eq!(
            result.value(0, 2999).try_to_string().unwrap(),
            "long string past the inline limit 2999"
        );
        assert_eq!(run(&mut c, "SELECT CAST(X'61' || X'62' AS VARCHAR)"), rows(&[&["ab"]]));
        assert_eq!(c.query("SELECT X'61' || X'62'").unwrap().types, vec![LogicalType::BLOB]);
        // OPERATOR() names the same function
        assert_eq!(run(&mut c, "SELECT 1 OPERATOR(main.+) 2"), rows(&[&["3"]]));
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
            "Binder Error: column \"v\" must appear in the GROUP BY clause or must be part of an aggregate function.\n\
             Either add it to the GROUP BY list, or use ANY_VALUE(\"v\") if the exact value of \"v\" is not important."
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
            "Binder Error: column \"v\" must appear in the GROUP BY clause or be used in an aggregate function"
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
    fn scalar_casts_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        for (sql, expected) in [
            ("SELECT '1992-01-01 12:00:00'::DATE", "1992-01-01"),
            ("SELECT DATE '1992-01-01'::TIMESTAMP", "1992-01-01 00:00:00"),
            ("SELECT DATE 'infinity'::TIMESTAMP_MS", "infinity"),
            (
                "SELECT '2000-01-01 00:00:00.5'::TIMESTAMP::TIMESTAMP_S",
                "2000-01-01 00:00:01",
            ),
            ("SELECT '12:30:00+02'::TIMETZ::TIME", "12:30:00"),
            ("SELECT '12:30:00.123456789'::TIME_NS::TIME", "12:30:00.123457"),
            ("SELECT '1 day 2 hours'::INTERVAL", "1 day 02:00:00"),
            ("SELECT 'a\\x00b'::BLOB", "a\\x00b"),
            (
                "SELECT '6ba7b810-9dad-11d1-80b4-00c04fd430c8'::UUID",
                "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            ),
            (
                "SELECT '00000000-0000-0000-0000-000000000001'::UUID::BLOB::UUID",
                "00000000-0000-0000-0000-000000000001",
            ),
            ("SELECT typeof(1::TINYINT)", "TINYINT"),
            ("SELECT typeof(DATE '1992-01-01')", "DATE"),
            ("SELECT TRY_CAST(DATE '1992-01-01' AS INTEGER)", "NULL"),
            ("SELECT CAST(NULL::DATE AS INTEGER)", "NULL"),
            ("SELECT TRY_CAST('nonsense' AS TIMESTAMP)", "NULL"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        assert_eq!(
            err(&mut context, "SELECT 'nonsense'::DATE"),
            "Conversion Error: invalid date field format: \"nonsense\", expected format is (YYYY-MM-DD)"
        );
        assert_eq!(
            err(&mut context, "SELECT DATE '1992-01-01'::INTEGER"),
            "Conversion Error: Unimplemented type for cast (DATE -> INTEGER)"
        );
        assert_eq!(
            err(&mut context, "SELECT 'nonsense'::UUID"),
            "Conversion Error: Could not convert string 'nonsense' to UUID"
        );
        assert_eq!(
            err(&mut context, "SELECT 'infinity'::TIMESTAMP::TIME"),
            "Conversion Error: Can't get TIME of infinite TIMESTAMP"
        );
    }

    #[test]
    fn nested_types_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        for (sql, expected) in [
            ("SELECT [1, 2, NULL]", "[1, 2, NULL]"),
            ("SELECT ['a b', 'c, d', '']", "[a b, 'c, d', '']"),
            ("SELECT {'a': 1, 'b': 'x'}", "{'a': 1, 'b': x}"),
            ("SELECT row(1, 'x')", "(1, x)"),
            ("SELECT row(1)", "(1,)"),
            ("SELECT MAP {'a': 1}", "{a=1}"),
            ("SELECT array_value(1, 2)", "[1, 2]"),
            ("SELECT [1, 2]::BIGINT[2]", "[1, 2]"),
            ("SELECT '[1, NULL]'::INTEGER[]", "[1, NULL]"),
            ("SELECT '{a: 1}'::STRUCT(a INTEGER, b INTEGER)", "{'a': 1, 'b': NULL}"),
            ("SELECT '{a=1}'::MAP(VARCHAR, INTEGER)", "{a=1}"),
            ("SELECT TRY_CAST(['1', 'x'] AS INTEGER[])", "[1, NULL]"),
            ("SELECT {'a': 1, 'b': 2}::STRUCT(b VARCHAR)", "{'b': 2}"),
            ("SELECT [1, 2, 3][-1]", "3"),
            ("SELECT {'a': 1}['a']", "1"),
            ("SELECT MAP {'a': 1}['a']", "1"),
            ("SELECT typeof([1]::BIGINT[])", "BIGINT[]"),
            ("SELECT typeof({'a': 1})", "STRUCT(a INTEGER)"),
            ("SELECT 1::UNION(a INTEGER, b VARCHAR)", "1"),
            ("SELECT [[1, NULL]] = [[1, NULL]]", "true"),
            ("SELECT [1, 2] < [1, 3]", "true"),
            ("SELECT {'a': 1} = {'a': 2}", "false"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        run(
            &mut context,
            "CREATE TABLE t (s STRUCT(a INTEGER, b VARCHAR), l INTEGER[])",
        );
        run(&mut context, "INSERT INTO t VALUES ({'a': 1, 'b': 'x'}, [1, 2])");
        assert_eq!(run(&mut context, "SELECT s.a, t.s.b, l[2] FROM t"), [["1", "x", "2"]]);
        assert_eq!(
            err(&mut context, "SELECT [1, 2]::INTEGER[3]"),
            "Conversion Error: Cannot cast list with length 2 to array with length 3"
        );
        assert_eq!(
            err(&mut context, "SELECT map([1, 1], [2, 3])"),
            "Invalid Input Error: Map keys must be unique."
        );
    }

    #[test]
    fn types_and_enums_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        run(&mut context, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')");
        run(&mut context, "CREATE TYPE name_t AS VARCHAR");
        run(&mut context, "CREATE TABLE t (m mood, n name_t)");
        run(
            &mut context,
            "INSERT INTO t VALUES ('happy', 'a'), ('sad', 'b'), (NULL, 'c')",
        );
        assert_eq!(
            run(&mut context, "SELECT m, typeof(m), typeof(n) FROM t WHERE m = 'happy'"),
            [["happy", "ENUM('sad', 'ok', 'happy')", "VARCHAR"]]
        );
        assert_eq!(
            run(&mut context, "SELECT m FROM t ORDER BY m DESC NULLS LAST"),
            [["happy"], ["sad"], ["NULL"]]
        );
        for (sql, expected) in [
            ("SELECT enum_first(NULL::mood)", "sad"),
            ("SELECT enum_last(NULL::mood)", "happy"),
            ("SELECT enum_range(NULL::mood)", "[sad, ok, happy]"),
            ("SELECT enum_code('ok'::mood)", "1"),
            ("SELECT enum_range_boundary('ok'::mood, NULL)", "[ok, happy]"),
            ("SELECT 'ok'::mood::VARCHAR", "ok"),
            ("SELECT TRY_CAST('x' AS mood)", "NULL"),
            ("SELECT 'a'::ENUM('a', 'b')", "a"),
            ("SELECT typeof('a'::ENUM('a', 'b'))", "ENUM('a', 'b')"),
            ("SELECT union_tag(1::UNION(a INTEGER, b VARCHAR))", "a"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        assert_eq!(
            err(&mut context, "SELECT 'x'::mood"),
            "Conversion Error: Could not convert string 'x' to ENUM('sad', 'ok', 'happy')"
        );
        assert_eq!(
            err(&mut context, "CREATE TYPE mood AS INTEGER"),
            "Catalog Error: Type with name \"mood\" already exists!"
        );
        assert_eq!(
            err(&mut context, "CREATE TYPE dup AS ENUM ('a', 'a')"),
            "Invalid Input Error: Attempted to create ENUM type with duplicate value a"
        );
        run(&mut context, "DROP TYPE name_t");
        assert_eq!(
            err(&mut context, "DROP TYPE name_t"),
            "Catalog Error: Type with name name_t does not exist!"
        );
    }

    #[test]
    fn bit_and_bignum_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        let huge = "340282366920938463463374607431768211456000";
        for (sql, expected) in [
            ("SELECT '0101'::BIT", "0101"),
            ("SELECT 'xFA'::BIT", "11111010"),
            ("SELECT '101'::BIT::INTEGER", "5"),
            ("SELECT 5::TINYINT::BIT", "00000101"),
            ("SELECT '0'::BIT < '10101010'::BIT", "true"),
            ("SELECT '101'::BIT & '110'::BIT", "100"),
            ("SELECT ~'101'::BIT", "010"),
            ("SELECT '1001'::BIT << 1", "0010"),
            ("SELECT bit_count('1101'::BIT)", "3"),
            ("SELECT get_bit('0110'::BIT, 2)", "1"),
            ("SELECT set_bit('0110'::BIT, 0, 1)", "1110"),
            ("SELECT bit_position('10'::BIT, '0010'::BIT)", "3"),
            ("SELECT bitstring('101', 5)", "00101"),
            ("SELECT '-0010'::BIGNUM", "-10"),
            ("SELECT '0.5'::BIGNUM", "1"),
            ("SELECT 1e3::BIGNUM", "1000"),
            (
                "SELECT 170141183460469231731687303715884105728000::BIGNUM // 1000",
                "170141183460469231731687303715884105728",
            ),
            (
                "SELECT 9223372036854775807::BIGNUM + 9223372036854775807::BIGNUM",
                "18446744073709551614",
            ),
            ("SELECT (-7)::BIGNUM % 2::BIGNUM", "-1"),
            ("SELECT 5::BIGNUM * 3", "15"),
            ("SELECT typeof(5::BIGNUM * 3)", "BIGNUM"),
            ("SELECT (-1)::BIGNUM < 1::BIGNUM", "true"),
            ("SELECT 300::BIGNUM::SMALLINT", "300"),
            ("SELECT TRY_CAST(300::BIGNUM AS TINYINT)", "NULL"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        assert_eq!(run(&mut context, &format!("SELECT {huge}")), [[huge]]);
        run(&mut context, "CREATE TABLE b (x BIGNUM)");
        run(&mut context, "INSERT INTO b VALUES (9223372036854775807), (2), (NULL)");
        assert_eq!(run(&mut context, "SELECT sum(x) FROM b"), [["9223372036854775809"]]);
        assert_eq!(
            err(&mut context, "SELECT '012'::BIT"),
            "Conversion Error: Invalid character encountered in string -> bit conversion: '2'"
        );
        assert_eq!(
            err(&mut context, "SELECT '101'::BIT & '11'::BIT"),
            "Invalid Input Error: Cannot AND bit strings of different sizes"
        );
        assert_eq!(
            err(&mut context, "SELECT 300::BIGNUM::TINYINT"),
            "Conversion Error: BIGNUM value 300 is out of range for type INT8"
        );
        assert_eq!(
            err(&mut context, "SELECT 'x'::BIGNUM"),
            "Conversion Error: Could not convert string 'x' to BIGNUM"
        );
    }

    #[test]
    fn variants_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        for (sql, expected) in [
            ("SELECT VARIANT 'a'", "a"),
            ("SELECT 42::VARIANT::BIGINT", "42"),
            ("SELECT [1, 2]::VARIANT", "[1, 2]"),
            ("SELECT {'b': 1, 'a': 'x, y'}::VARIANT", "{'a': 'x, y', 'b': 1}"),
            ("SELECT ({'a': 42}::VARIANT).a::BIGINT", "42"),
            ("SELECT ([10, 20]::VARIANT)[2]::INTEGER", "20"),
            ("SELECT {'a': 'true'}::VARIANT::STRUCT(a BOOLEAN)", "{'a': true}"),
            ("SELECT '[1, 2]'::VARIANT::INTEGER[]", "[1, 2]"),
            ("SELECT MAP {'k': 1}::VARIANT::MAP(VARCHAR, INTEGER)", "{k=1}"),
            ("SELECT variant_typeof({'o': 1, 'l': 2}::VARIANT)", "OBJECT(l, o)"),
            ("SELECT variant_typeof([1, 2]::VARIANT)", "ARRAY(2)"),
            ("SELECT variant_typeof(1::VARIANT)", "INT32"),
            ("SELECT variant_typeof(1.5::VARIANT)", "DECIMAL(2, 1)"),
            ("SELECT variant_typeof(NULL::VARIANT)", "VARIANT_NULL"),
            ("SELECT variant_keys({'b': 1, 'a': 2}::VARIANT)", "[a, b]"),
            ("SELECT ({'a': NULL}::VARIANT).a IS NULL", "true"),
            ("SELECT TRY_CAST('hello'::VARIANT AS INTEGER)", "NULL"),
            ("SELECT typeof([1::VARIANT, 'a'])", "VARIANT[]"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        assert_eq!(
            err(&mut context, "SELECT {'a': 'x'}::VARIANT::STRUCT(a BOOLEAN)"),
            "Conversion Error: Can't convert VARIANT(VARCHAR) value 'x' to 'STRUCT(a BOOLEAN)'"
        );
        assert_eq!(
            err(&mut context, "SELECT {'a': 1}::VARIANT::STRUCT(b INTEGER)"),
            "Conversion Error: VARIANT(OBJECT(a)) is missing key 'b' to 'STRUCT(b INTEGER)'"
        );
    }

    #[test]
    fn helpers_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        for (sql, expected) in [
            ("SELECT repeat('ab', 2)", "abab"),
            ("SELECT repeat([1], 2)", "[1, 1]"),
            ("SELECT len([1, 2])", "2"),
            ("SELECT length('héllo')", "5"),
            ("SELECT strlen('héllo')", "6"),
            ("SELECT concat('a', NULL, 1)", "a1"),
            ("SELECT concat_ws(',', 'a', NULL, 'b')", "a,b"),
            ("SELECT list_concat([1], [2, 3])", "[1, 2, 3]"),
            ("SELECT array_to_string([1, NULL, 2], '-')", "1-2"),
            ("SELECT list_sort([3, NULL, 1])", "[NULL, 1, 3]"),
            ("SELECT array_sort([3, 1], 'DESC', 'NULLS LAST')", "[3, 1]"),
            ("SELECT map_keys(MAP {'a': 1})", "[a]"),
            ("SELECT map_values(MAP {'a': 1})", "[1]"),
            ("SELECT cardinality(MAP {'a': 1})", "1"),
            ("SELECT map_from_entries([{'k': 'a', 'v': 1}])", "{a=1}"),
            ("SELECT if(1 = 1, 'y', 'n')", "y"),
            ("SELECT upper('ab'), lower('AB')", "AB"),
            ("SELECT 'abc' LIKE 'a%'", "true"),
            ("SELECT 'abc' NOT LIKE 'a_'", "true"),
            ("SELECT 'ABC' ILIKE 'a%'", "true"),
            ("SELECT sign(-5)", "-1"),
            ("SELECT to_days(2)", "2 days"),
            ("SELECT -INTERVAL 1 DAY", "-1 day"),
            ("SELECT [1, 2, 3, 4][2:3]", "[2, 3]"),
            ("SELECT [1, 2, 3][:-1]", "[1, 2, 3]"),
            ("SELECT [1, 2, 3, 4, 5][:-:-2]", "[5, 3, 1]"),
            ("SELECT 'hello'[2:3]", "el"),
            ("SELECT TRY(1 // 0)", "NULL"),
            ("SELECT TRY('x'::INTEGER)", "NULL"),
            ("SELECT TRY(CAST('3' AS INTEGER))", "3"),
        ] {
            assert_eq!(run(&mut context, sql)[0][0], expected, "{sql}");
        }
        run(&mut context, "CREATE TABLE t (i INTEGER, l INTEGER[])");
        run(&mut context, "INSERT INTO t VALUES (1, [10, 20]), (2, NULL), (3, [])");
        assert_eq!(
            run(&mut context, "SELECT i, unnest(l) FROM t ORDER BY 1, 2"),
            [["1", "10"], ["1", "20"]]
        );
        assert_eq!(
            run(&mut context, "SELECT unnest([1, 2]), unnest(['a'])"),
            [["1", "a"], ["2", "NULL"]]
        );
        assert_eq!(run(&mut context, "SELECT unnest(unnest([[1], [2]]))"), [["1"], ["2"]]);
        assert_eq!(
            run(&mut context, "SELECT list(i ORDER BY i DESC) FROM t"),
            [["[3, 2, 1]"]]
        );
        assert_eq!(
            err(&mut context, "SELECT 85070591730234615865843651857942052864 * 2"),
            "Out of Range Error: Overflow in multiplication of INT128 (85070591730234615865843651857942052864 * 2)!"
        );
        assert_eq!(
            err(&mut context, "SELECT 1 FROM t WHERE unnest(l) = 1"),
            "Binder Error: UNNEST not supported here"
        );
    }

    #[test]
    fn set_operations_and_ctes_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        run(&mut context, "CREATE TABLE a (x INTEGER)");
        run(&mut context, "CREATE TABLE b (x INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1), (1), (2), (NULL)");
        run(&mut context, "INSERT INTO b VALUES (1), (3), (NULL)");
        let rows = |context: &mut ClientContext, sql: &str| run(context, &format!("{sql} ORDER BY 1 NULLS LAST"));
        assert_eq!(
            rows(&mut context, "SELECT x FROM a UNION ALL SELECT x FROM b"),
            [["1"], ["1"], ["1"], ["2"], ["3"], ["NULL"], ["NULL"]]
        );
        assert_eq!(
            rows(&mut context, "SELECT x FROM a UNION SELECT x FROM b"),
            [["1"], ["2"], ["3"], ["NULL"]]
        );
        assert_eq!(rows(&mut context, "SELECT x FROM a EXCEPT SELECT x FROM b"), [["2"]]);
        assert_eq!(
            rows(&mut context, "SELECT x FROM a EXCEPT ALL SELECT x FROM b"),
            [["1"], ["2"]]
        );
        assert_eq!(
            rows(&mut context, "SELECT x FROM a INTERSECT SELECT x FROM b"),
            [["1"], ["NULL"]]
        );
        assert_eq!(
            rows(&mut context, "SELECT x FROM a INTERSECT ALL SELECT x FROM a"),
            [["1"], ["1"], ["2"], ["NULL"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT 1 AS p, 'a' AS q UNION ALL BY NAME SELECT 'b' AS q, 2 AS r ORDER BY q"
            ),
            [["1", "a", "NULL"], ["NULL", "b", "2"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT 1 AS v UNION ALL SELECT 2.5 ORDER BY v DESC LIMIT 1"
            ),
            [["2.5"]]
        );
        assert_eq!(
            run(
                &mut context,
                "WITH t(y) AS (SELECT 1), u AS (SELECT y + 1 AS z FROM t) SELECT y, z FROM t, u"
            ),
            [["1", "2"]]
        );
        // a CTE shadows a table, and does not see itself
        assert_eq!(
            run(&mut context, "WITH a AS (SELECT count(*) AS c FROM a) SELECT * FROM a"),
            [["4"]]
        );
        assert_eq!(
            run(
                &mut context,
                "WITH t AS (SELECT 1 AS v) SELECT v FROM (WITH t AS (SELECT 2 AS v) SELECT v FROM t) s"
            ),
            [["2"]]
        );
        assert_eq!(
            err(&mut context, "SELECT 1, 2 UNION ALL SELECT 1"),
            "Binder Error: Set operations can only apply to expressions with the same number of result columns"
        );
        assert_eq!(
            err(&mut context, "SELECT 1 AS a UNION ALL SELECT 2 ORDER BY nope"),
            "Binder Error: Could not ORDER BY column \"nope\": add the expression/function to every SELECT, or move \
             the UNION into a FROM clause."
        );
    }

    #[test]
    fn uncorrelated_subqueries_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        run(&mut context, "CREATE TABLE a (x INTEGER)");
        run(&mut context, "CREATE TABLE b (y INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1), (2), (3), (NULL)");
        run(&mut context, "INSERT INTO b VALUES (2), (4)");
        for (sql, expected) in [
            ("SELECT (SELECT max(y) FROM b)", "4"),
            ("SELECT (SELECT y FROM b WHERE y > 100)", "NULL"),
            ("SELECT EXISTS (SELECT 1 FROM b)", "true"),
            ("SELECT NOT EXISTS (SELECT 1 FROM b)", "false"),
            ("SELECT EXISTS (SELECT 1 FROM b WHERE y > 100)", "false"),
            ("SELECT 2 IN (SELECT y FROM b)", "true"),
            ("SELECT 3 IN (SELECT y FROM b)", "false"),
            ("SELECT 3 NOT IN (SELECT y FROM b)", "true"),
            ("SELECT NULL IN (SELECT y FROM b)", "NULL"),
            ("SELECT NULL IN (SELECT y FROM b WHERE y > 100)", "false"),
            ("SELECT 7 IN (SELECT x FROM a)", "NULL"),
            ("SELECT 1 IN (SELECT x FROM a)", "true"),
            ("SELECT 5 > ALL (SELECT y FROM b)", "true"),
            ("SELECT 3 > ALL (SELECT y FROM b)", "false"),
            ("SELECT 3 > ANY (SELECT y FROM b)", "true"),
            ("SELECT 5 > ALL (SELECT x FROM a)", "NULL"),
            ("SELECT 1 <> ANY (SELECT y FROM b)", "true"),
            ("SELECT TRY((SELECT y FROM b))", "NULL"),
        ] {
            assert_eq!(run(&mut context, sql), [[expected]], "{sql}");
        }
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE x IN (SELECT y FROM b) OR x = (SELECT min(x) FROM a)"
            ),
            [["1"], ["2"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE x NOT IN (SELECT y FROM b) ORDER BY x"
            ),
            [["1"], ["3"]]
        );
        assert_eq!(
            err(&mut context, "SELECT (SELECT y FROM b)"),
            "Invalid Input Error: More than one row returned by a subquery used as an expression - scalar subqueries \
             can only return a single row.\n\nUse \"SET scalar_subquery_error_on_multiple_rows=false\" to revert to \
             previous behavior of returning a random row."
        );
        assert_eq!(
            err(&mut context, "SELECT (SELECT 1, 2)"),
            "Binder Error: Subquery returns 2 columns - expected 1"
        );
        assert_eq!(
            err(&mut context, "SELECT (SELECT nope FROM b) FROM a"),
            "Binder Error: Referenced column \"nope\" not found in FROM clause!\nCandidate bindings: \"y\""
        );
    }

    #[test]
    fn correlated_subqueries_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        run(&mut context, "CREATE TABLE a (x INTEGER, k INTEGER)");
        run(&mut context, "CREATE TABLE b (y INTEGER, k INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1, 1), (2, 1), (3, 2), (NULL, 3)");
        run(&mut context, "INSERT INTO b VALUES (10, 1), (20, 1), (30, 2)");
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT sum(y) FROM b WHERE b.k = a.k) FROM a ORDER BY x NULLS LAST"
            ),
            [["1", "30"], ["2", "30"], ["3", "30"], ["NULL", "NULL"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k) ORDER BY x"
            ),
            [["1"], ["2"], ["3"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)"
            ),
            [["NULL"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE x * 10 IN (SELECT y FROM b WHERE b.k = a.k) ORDER BY x"
            ),
            [["1"], ["2"], ["3"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x FROM a WHERE x * 10 < ALL (SELECT y FROM b WHERE b.k = a.k) ORDER BY x"
            ),
            // ALL over no rows holds, even for NULL
            [["NULL"]]
        );
        // two levels: the innermost query reads the outermost one
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT (SELECT max(y) FROM b WHERE b.k = a.k AND b.y > c.y) FROM b AS c WHERE c.y = 10) \
                 FROM a WHERE x = 1"
            ),
            [["1", "20"]]
        );
        // a correlated column in a query in FROM, and in a set operation
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT count(*) FROM (SELECT y FROM b WHERE b.k = a.k UNION ALL SELECT a.x)) FROM a \
                 WHERE x = 3"
            ),
            [["3", "2"]]
        );
        // grouped: the correlated column must be a group
        assert_eq!(
            run(
                &mut context,
                "SELECT k, (SELECT count(*) FROM b WHERE b.k = a.k) FROM a GROUP BY k ORDER BY k"
            ),
            [["1", "2"], ["2", "1"], ["3", "0"]]
        );
        assert_eq!(
            err(&mut context, "SELECT (SELECT sum(a.x)) FROM a"),
            "Not implemented Error: aggregates of the columns of an outer query is not yet ported to duckrs"
        );
    }

    #[test]
    fn recursive_ctes_and_lateral_joins_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        assert_eq!(
            run(
                &mut context,
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t ORDER BY n"
            ),
            [["1"], ["2"], ["3"]]
        );
        // UNION stops at what it has seen
        assert_eq!(
            run(
                &mut context,
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT (n + 1) % 3 FROM t) SELECT n FROM t ORDER BY n"
            ),
            [["0"], ["1"], ["2"]]
        );
        // LIMIT ends recursion that would not end
        assert_eq!(
            run(
                &mut context,
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t LIMIT 2"
            ),
            [["1"], ["2"]]
        );
        // RECURSIVE without a reference to itself is a set operation
        assert_eq!(
            run(
                &mut context,
                "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT 2) SELECT n FROM t ORDER BY n"
            ),
            [["1"], ["2"]]
        );
        run(&mut context, "CREATE TABLE a (x INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1), (2)");
        assert_eq!(
            run(
                &mut context,
                "SELECT x, y FROM a, LATERAL (SELECT x + 10 AS y UNION ALL SELECT x + 20) ORDER BY x, y"
            ),
            [["1", "11"], ["1", "21"], ["2", "12"], ["2", "22"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x, y FROM a LEFT JOIN LATERAL (SELECT x AS y WHERE x > 1) ON true ORDER BY x"
            ),
            [["1", "NULL"], ["2", "2"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x, y FROM a JOIN LATERAL (SELECT x * 2 AS y) ON y > 2"
            ),
            [["2", "4"]]
        );
        assert_eq!(
            err(
                &mut context,
                "SELECT * FROM a RIGHT JOIN LATERAL (SELECT x AS y) ON true"
            ),
            "Binder Error: The combining JOIN type must be INNER or LEFT for a LATERAL reference"
        );
        // a volatile subquery runs for every row, not for every value
        run(&mut context, "CREATE SEQUENCE s");
        run(&mut context, "CREATE TABLE same (k INTEGER)");
        run(&mut context, "INSERT INTO same VALUES (1), (1), (1)");
        assert_eq!(
            run(&mut context, "SELECT max((SELECT nextval('s') + k - k)) FROM same"),
            [["3"]]
        );
    }

    #[test]
    fn updates_and_deletes_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        run(
            &mut context,
            "CREATE TABLE t (i INTEGER, s VARCHAR DEFAULT 'd', n INTEGER NOT NULL DEFAULT 0)",
        );
        run(
            &mut context,
            "INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 3), (NULL, NULL, 4)",
        );
        assert_eq!(
            run(&mut context, "UPDATE t SET i = i + 10, s = upper(s) WHERE i >= 2"),
            [["2"]]
        );
        assert_eq!(
            run(&mut context, "SELECT i, s FROM t ORDER BY n"),
            [["1", "a"], ["12", "B"], ["13", "C"], ["NULL", "NULL"]]
        );
        // the right side sees the table as it was
        run(&mut context, "UPDATE t SET i = (SELECT max(n) FROM t) WHERE n = 1");
        assert_eq!(run(&mut context, "SELECT i FROM t WHERE n = 1"), [["4"]]);
        run(&mut context, "UPDATE t SET s = DEFAULT WHERE n = 2");
        assert_eq!(run(&mut context, "SELECT s FROM t WHERE n = 2"), [["d"]]);
        assert_eq!(
            run(
                &mut context,
                "UPDATE t AS o SET i = 0 WHERE n = 3 RETURNING o.n, i + 1 AS next"
            ),
            [["3", "1"]]
        );
        assert_eq!(
            run(&mut context, "DELETE FROM t WHERE i IS NULL RETURNING *"),
            [["NULL", "NULL", "4"]]
        );
        run(&mut context, "CREATE TABLE d (k INTEGER)");
        run(&mut context, "INSERT INTO d VALUES (1), (1), (2)");
        assert_eq!(run(&mut context, "DELETE FROM t USING d WHERE t.n = d.k"), [["2"]]);
        assert_eq!(run(&mut context, "SELECT n FROM t"), [["3"]]);
        assert_eq!(
            run(
                &mut context,
                "WITH c AS (SELECT 3 AS v) DELETE FROM t WHERE n IN (SELECT v FROM c)"
            ),
            [["1"]]
        );
        run(&mut context, "INSERT INTO t VALUES (1, 'x', 1), (2, 'y', 2)");
        // ROLLBACK undoes both, and a chunk's worth of rows keeps row ids right
        run(&mut context, "BEGIN");
        run(&mut context, "DELETE FROM t WHERE n = 1");
        run(&mut context, "UPDATE t SET s = 'z'");
        run(&mut context, "ROLLBACK");
        assert_eq!(
            run(&mut context, "SELECT n, s FROM t ORDER BY n"),
            [["1", "x"], ["2", "y"]]
        );
        run(&mut context, "CREATE TABLE big AS SELECT range AS i FROM range(5000)");
        run(&mut context, "DELETE FROM big WHERE i % 2 = 0");
        run(&mut context, "UPDATE big SET i = -i WHERE i = 4999");
        assert_eq!(
            run(&mut context, "SELECT count(*), min(i), max(i) FROM big"),
            [["2500", "-4999", "4997"]]
        );
        assert_eq!(
            err(&mut context, "UPDATE t SET n = NULL"),
            "Constraint Error: NOT NULL constraint failed: t.n"
        );
        assert_eq!(
            err(&mut context, "UPDATE t SET i = 1, i = 2"),
            "Binder Error: Multiple assignments to same column \"i\""
        );
        run(&mut context, "CREATE VIEW v AS SELECT 1 AS a");
        assert_eq!(
            err(&mut context, "DELETE FROM v"),
            "Binder Error: Can only delete from base table"
        );
        assert_eq!(
            err(&mut context, "UPDATE v SET a = 2"),
            "Binder Error: Can only update base table"
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
            "Binder Error: table \"t\" has 1 columns but 2 values were supplied"
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

    #[test]
    fn table_functions() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        let single = |rows: Vec<Vec<String>>| rows.into_iter().map(|mut r| r.remove(0)).collect::<Vec<_>>();
        assert_eq!(single(run(&mut context, "SELECT * FROM unnest([1, 2])")), ["1", "2"]);
        assert_eq!(
            single(run(&mut context, "SELECT repeat FROM repeat('a', 2)")),
            ["a", "a"]
        );
        assert_eq!(
            run(&mut context, "FROM repeat_row(1, 'b', num_rows = 2)"),
            [["1", "b"], ["1", "b"]]
        );
        run(&mut context, "CREATE TABLE t AS SELECT range AS i FROM range(1, 3)");
        assert_eq!(
            single(run(&mut context, "FROM query('SELECT i * 2 FROM t')")),
            ["2", "4"]
        );
        assert_eq!(single(run(&mut context, "FROM query_table('t')")), ["1", "2"]);
        assert_eq!(
            run(&mut context, "SELECT i, r FROM t, range(i) r(r) ORDER BY ALL"),
            [["1", "0"], ["2", "0"], ["2", "1"]]
        );
        assert_eq!(single(run(&mut context, "SELECT range(3)")), ["[0, 1, 2]"]);
        assert_eq!(single(run(&mut context, "SELECT generate_series(1, 2)")), ["[1, 2]"]);
        assert_eq!(single(run(&mut context, "CALL range(2)")), ["0", "1"]);
        assert_eq!(
            context
                .query("FROM query('INSERT INTO t VALUES (1)')")
                .unwrap_err()
                .to_string(),
            "Parser Error: Expected a single SELECT statement"
        );
        assert_eq!(
            single(run(&mut context, "SELECT count(*) FROM test_all_types()")),
            ["3"]
        );
    }

    #[test]
    fn checkpoints() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        for sql in [
            "CHECKPOINT",
            "FORCE CHECKPOINT",
            "CHECKPOINT memory",
            "CALL checkpoint()",
        ] {
            run(&mut context, sql);
        }
        assert_eq!(
            context.query("CHECKPOINT other").unwrap_err().to_string(),
            "Binder Error: Database \"other\" not found"
        );
        run(&mut context, "CREATE TABLE t (i INTEGER)");
        run(&mut context, "BEGIN");
        run(&mut context, "CHECKPOINT");
        assert_eq!(
            context.query("FORCE CHECKPOINT").unwrap_err().to_string(),
            "TransactionContext Error: Cannot FORCE CHECKPOINT: the current transaction has been started for this database"
        );
    }

    #[test]
    fn table_constraints() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(
            &mut c,
            "CREATE TABLE p (a INTEGER PRIMARY KEY, b VARCHAR UNIQUE, CHECK (a < 10))",
        );
        run(&mut c, "INSERT INTO p VALUES (1, 'x'), (2, NULL), (3, NULL)");
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        assert_eq!(
            error(&mut c, "INSERT INTO p VALUES (1, 'y')"),
            "Constraint Error: Duplicate key \"a: 1\" violates primary key constraint."
        );
        assert_eq!(
            error(&mut c, "INSERT INTO p VALUES (4, 'x')"),
            "Constraint Error: Duplicate key \"b: x\" violates unique constraint."
        );
        assert_eq!(
            error(&mut c, "INSERT INTO p VALUES (NULL, 'z')"),
            "Constraint Error: NOT NULL constraint failed: p.a"
        );
        assert_eq!(
            error(&mut c, "INSERT INTO p VALUES (11, 'z')"),
            "Constraint Error: CHECK constraint failed on table \"p\" with expression CHECK((a < 10))"
        );
        // a failed INSERT leaves nothing behind, and its keys are free
        assert_eq!(
            error(&mut c, "INSERT INTO p VALUES (5, 'a'), (5, 'b')"),
            "Constraint Error: PRIMARY KEY or UNIQUE constraint violation: duplicate key \"5\""
        );
        run(&mut c, "INSERT INTO p VALUES (5, 'a')");
        run(&mut c, "UPDATE p SET a = a + 1");
        assert_eq!(run(&mut c, "SELECT a FROM p ORDER BY a"), [["2"], ["3"], ["4"], ["6"]]);
        assert_eq!(
            error(&mut c, "UPDATE p SET a = 3 WHERE a = 2"),
            "Constraint Error: Duplicate key \"a: 3\" violates primary key constraint."
        );
        run(&mut c, "DELETE FROM p WHERE a = 2");
        run(&mut c, "INSERT INTO p VALUES (2, 'x')");

        run(&mut c, "CREATE TABLE f (i INTEGER REFERENCES p)");
        run(&mut c, "INSERT INTO f VALUES (2), (NULL)");
        assert_eq!(
            error(&mut c, "INSERT INTO f VALUES (7)"),
            "Constraint Error: Violates foreign key constraint because key \"a: 7\" does not exist in the referenced table"
        );
        assert!(error(&mut c, "DELETE FROM p WHERE a = 2").contains("key \"i: 2\" is still referenced"));
        assert!(error(&mut c, "UPDATE p SET a = 8 WHERE a = 2").contains("is still referenced"));
        assert!(error(&mut c, "DROP TABLE p").contains("main key table of the table \"f\""));
        run(&mut c, "DROP TABLE f");
        run(&mut c, "DROP TABLE p");

        // a rollback brings back the keys it took away
        run(&mut c, "CREATE TABLE u (i INTEGER UNIQUE)");
        run(&mut c, "INSERT INTO u VALUES (1)");
        run(&mut c, "BEGIN");
        run(&mut c, "DELETE FROM u");
        run(&mut c, "INSERT INTO u VALUES (2)");
        run(&mut c, "ROLLBACK");
        run(&mut c, "INSERT INTO u VALUES (2)");
        assert!(c.query("INSERT INTO u VALUES (1)").is_err());
    }

    #[test]
    fn upserts() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(
            &mut c,
            "CREATE TABLE t (i INTEGER PRIMARY KEY, j INTEGER DEFAULT 7, k VARCHAR)",
        );
        run(&mut c, "INSERT INTO t VALUES (1, 1, 'a')");
        assert_eq!(
            run(&mut c, "INSERT INTO t (i, k) VALUES (2, 'b') RETURNING i, j"),
            [["2", "7"]]
        );
        assert_eq!(
            run(
                &mut c,
                "INSERT INTO t VALUES (1, 9, 'x'), (3, 3, 'c') ON CONFLICT DO NOTHING"
            ),
            [["1"]]
        );
        assert_eq!(
            run(
                &mut c,
                "INSERT INTO t VALUES (1, 5, 'y'), (4, 4, 'd') ON CONFLICT (i) DO UPDATE SET j = j + excluded.j RETURNING *"
            ),
            [["1", "6", "a"], ["4", "4", "d"]]
        );
        run(
            &mut c,
            "INSERT INTO t VALUES (2, 0, 'z') ON CONFLICT (i) DO UPDATE SET j = 1 WHERE excluded.j > 0",
        );
        run(&mut c, "INSERT OR REPLACE INTO t VALUES (3, 30, 'cc')");
        run(&mut c, "INSERT OR IGNORE INTO t VALUES (3, 31, 'ccc')");
        assert_eq!(
            run(&mut c, "SELECT i, j, k FROM t ORDER BY i"),
            [["1", "6", "a"], ["2", "7", "b"], ["3", "30", "cc"], ["4", "4", "d"]]
        );
        run(&mut c, "CREATE TABLE u (a INTEGER, b INTEGER)");
        run(&mut c, "INSERT INTO u BY NAME SELECT 1 AS b, 2 AS a");
        run(&mut c, "WITH x AS (SELECT 5 AS a) INSERT INTO u SELECT a, a FROM x");
        assert_eq!(run(&mut c, "FROM u ORDER BY a"), [["2", "1"], ["5", "5"]]);
    }

    #[test]
    fn window_functions() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t (g INTEGER, v INTEGER)");
        run(
            &mut c,
            "INSERT INTO t VALUES (1, 10), (1, 20), (1, 20), (2, 5), (2, NULL)",
        );
        let window = |c: &mut ClientContext, over: &str| {
            let sql = format!("SELECT {over} FROM t ORDER BY g, v NULLS LAST, 1");
            run(c, &sql)
                .into_iter()
                .map(|mut r| r.remove(0))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            window(&mut c, "row_number() OVER (PARTITION BY g ORDER BY v)"),
            "1 2 3 1 2"
        );
        assert_eq!(window(&mut c, "rank() OVER (PARTITION BY g ORDER BY v)"), "1 2 2 1 2");
        assert_eq!(window(&mut c, "dense_rank() OVER (ORDER BY g)"), "1 1 1 2 2");
        assert_eq!(
            window(&mut c, "sum(v) OVER (PARTITION BY g ORDER BY v)"),
            "10 50 50 5 5"
        );
        assert_eq!(
            window(
                &mut c,
                "sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"
            ),
            "10 30 40 5 5"
        );
        assert_eq!(
            window(
                &mut c,
                "count(*) OVER (ORDER BY v RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING)"
            ),
            "2 2 2 2 1"
        );
        assert_eq!(
            window(&mut c, "lag(v, 1, -1) OVER (PARTITION BY g ORDER BY v)"),
            "-1 10 20 -1 5"
        );
        assert_eq!(
            window(
                &mut c,
                "first_value(v) OVER (PARTITION BY g ORDER BY v DESC NULLS FIRST)"
            ),
            "20 20 20 NULL NULL"
        );
        assert_eq!(window(&mut c, "ntile(2) OVER (ORDER BY g, v)"), "1 1 1 2 2");
        assert_eq!(
            run(
                &mut c,
                "SELECT g FROM t QUALIFY row_number() OVER (ORDER BY v DESC NULLS LAST) = 1 ORDER BY g"
            ),
            [["1"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT g, sum(sum(v)) OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g"
            ),
            [["1", "50"], ["2", "55"]]
        );
        assert_eq!(
            c.query("SELECT 1 FROM t WHERE rank() OVER () = 1")
                .unwrap_err()
                .to_string(),
            "Binder Error: WHERE clause cannot contain window functions!"
        );
    }

    #[test]
    fn alter_table() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t (i INTEGER, j INTEGER, CHECK (i < 10))");
        run(&mut c, "INSERT INTO t VALUES (1, 2), (3, NULL)");
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "ALTER TABLE t ADD COLUMN k VARCHAR DEFAULT 'x'");
        run(&mut c, "ALTER TABLE t RENAME COLUMN i TO a");
        assert_eq!(run(&mut c, "SELECT a, k FROM t ORDER BY a"), [["1", "x"], ["3", "x"]]);
        assert_eq!(
            error(&mut c, "INSERT INTO t (a) VALUES (11)"),
            "Constraint Error: CHECK constraint failed on table \"t\" with expression CHECK((a < 10))"
        );
        run(
            &mut c,
            "ALTER TABLE t ALTER COLUMN j TYPE VARCHAR USING a::VARCHAR || 'z'",
        );
        assert_eq!(run(&mut c, "SELECT j FROM t ORDER BY a"), [["1z"], ["3z"]]);
        run(&mut c, "ALTER TABLE t DROP COLUMN k");
        run(&mut c, "ALTER TABLE t ADD PRIMARY KEY (a)");
        assert!(c.query("INSERT INTO t VALUES (1, 'dup')").is_err());
        run(&mut c, "ALTER TABLE t RENAME TO u");
        assert_eq!(run(&mut c, "SELECT count(*) FROM u"), [["2"]]);
        run(&mut c, "ALTER TABLE IF EXISTS missing RENAME TO other");

        // a rollback brings the old entry back
        run(&mut c, "BEGIN");
        run(&mut c, "ALTER TABLE u ADD COLUMN extra INTEGER");
        run(&mut c, "ALTER TABLE u RENAME TO v");
        run(&mut c, "ROLLBACK");
        assert_eq!(run(&mut c, "SELECT * FROM u ORDER BY a"), [["1", "1z"], ["3", "3z"]]);

        run(&mut c, "CREATE TABLE f (i INTEGER REFERENCES u)");
        assert_eq!(
            error(&mut c, "ALTER TABLE u RENAME TO w"),
            "Dependency Error: Cannot alter entry \"u\" because there are entries that depend on it."
        );
        run(&mut c, "ALTER TABLE u ADD COLUMN extra INTEGER");
        run(&mut c, "INSERT INTO f VALUES (3)");
        assert!(c.query("INSERT INTO f VALUES (5)").is_err());
        run(&mut c, "CREATE VIEW w AS SELECT 1 AS x");
        run(&mut c, "ALTER VIEW w RENAME TO w2");
        assert_eq!(run(&mut c, "FROM w2"), [["1"]]);
    }

    #[test]
    fn indexes() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t (i INTEGER, j VARCHAR)");
        run(
            &mut c,
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (NULL, 'c'), (NULL, 'd')",
        );
        let error = |c: &mut ClientContext, sql: &str| c.query(sql).unwrap_err().to_string();
        run(&mut c, "CREATE INDEX plain ON t (j)");
        run(&mut c, "CREATE UNIQUE INDEX u ON t (i)");
        assert_eq!(
            error(&mut c, "INSERT INTO t VALUES (1, 'x')"),
            "Constraint Error: Duplicate key \"i: 1\" violates unique constraint."
        );
        run(&mut c, "INSERT INTO t VALUES (NULL, 'x')");
        assert_eq!(
            error(&mut c, "CREATE UNIQUE INDEX v ON t ((i % 1))"),
            "Constraint Error: Data contains duplicates on indexed column(s)"
        );
        assert_eq!(
            error(&mut c, "CREATE INDEX u ON t (j)"),
            "Catalog Error: Index with name \"u\" already exists!"
        );
        run(&mut c, "CREATE INDEX IF NOT EXISTS u ON t (j)");
        assert!(error(&mut c, "ALTER TABLE t RENAME TO w").contains("Cannot alter entry \"t\""));
        run(
            &mut c,
            "INSERT INTO t VALUES (1, 'y') ON CONFLICT (i) DO UPDATE SET j = excluded.j",
        );
        assert_eq!(run(&mut c, "SELECT j FROM t WHERE i = 1"), [["y"]]);

        run(&mut c, "BEGIN");
        run(&mut c, "DROP INDEX u");
        run(&mut c, "INSERT INTO t VALUES (1, 'z')");
        run(&mut c, "ROLLBACK");
        assert!(c.query("INSERT INTO t VALUES (1, 'z')").is_err());
        run(&mut c, "DROP INDEX u");
        run(&mut c, "INSERT INTO t VALUES (1, 'z')");
        assert_eq!(
            error(&mut c, "DROP INDEX u"),
            "Catalog Error: Index with name u does not exist!"
        );
        run(&mut c, "DROP INDEX IF EXISTS u");
    }

    #[test]
    fn session_values() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(
            &mut c,
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
        );
        run(&mut c, "SET VARIABLE v = 40 + 2");
        run(&mut c, "SET VARIABLE q = (SELECT 'SELECT 7')");
        assert_eq!(
            run(&mut c, "SELECT getvariable('v'), getvariable('missing')"),
            [["42", "NULL"]]
        );
        assert_eq!(run(&mut c, "FROM query(getvariable('q'))"), [["7"]]);
        run(&mut c, "RESET VARIABLE v");
        assert_eq!(run(&mut c, "SELECT getvariable('v')"), [["NULL"]]);
        assert_eq!(run(&mut c, "SELECT current_setting('default_order')"), [["ASC"]]);
        assert!(c.query("SELECT current_setting('nonsense')").is_err());
        assert_eq!(
            run(
                &mut c,
                "SELECT chr(65), contains('abc', 'b'), lpad('a', 3, 'xy'), rpad('abcd', 2, 'x'), nullif(1, 1), nullif(1, 2)"
            ),
            [["A", "true", "xya", "ab", "NULL", "1"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT greatest(1, NULL, 3), least(2.5, 1), greatest('a', 'b'), signbit(-1.0)"
            ),
            [["3", "1.0", "b", "true"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT count(DISTINCT random()), count(DISTINCT uuid()) FROM range(100)"
            ),
            [["100", "100"]]
        );
    }

    #[test]
    fn function_library() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut c,
                "SELECT substring('hello', 2, 3), replace('aXb', 'X', '-'), trim('  a '), left('abc', 2), \
                 printf('%05.1f|%-3s|%d', 3.14159::DOUBLE, 'a', 7), format('{}-{:03d}', 'x', 5), \
                 md5('abc'), split_part('a,b', ',', 2)"
            ),
            [[
                "ell",
                "a-b",
                "a",
                "ab",
                "003.1|a  |7",
                "x-005",
                "900150983cd24fb0d6963f7d28e17f72",
                "b"
            ]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT regexp_matches('abc', 'b'), regexp_replace('aaa', 'a', 'b', 'g'), \
                 regexp_extract('a1', '([a-z])(\\d)', 2), sqrt(16), round(2.675::DOUBLE, 2), pow(2, 10)"
            ),
            [["true", "bbb", "1", "4.0", "2.67", "1024.0"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT year(DATE '2024-03-15'), date_part('dow', DATE '2024-03-15'), \
                 DATE '2024-01-31' + INTERVAL 1 MONTH, DATE '2024-03-15' + 1, DATE '2024-03-15' - DATE '2024-03-01', \
                 date_trunc('month', TIMESTAMP '2024-03-15 10:00:00'), date_diff('month', DATE '2024-01-31', DATE '2024-03-01'), \
                 strftime(DATE '2024-03-05', '%d/%m/%Y %a'), strptime('05/03/2024', '%d/%m/%Y')"
            ),
            [[
                "2024",
                "5",
                "2024-02-29 00:00:00",
                "2024-03-16",
                "14",
                "2024-03-01 00:00:00",
                "2",
                "05/03/2024 Tue",
                "2024-03-05 00:00:00"
            ]]
        );
    }

    #[test]
    fn aggregates_and_lambdas() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "SET lambda_syntax='ENABLE_SINGLE_ARROW'");
        run(&mut c, "CREATE TABLE t (g INTEGER, v INTEGER, s VARCHAR)");
        run(
            &mut c,
            "INSERT INTO t VALUES (1, 1, 'a'), (1, 3, 'b'), (2, 5, NULL), (2, 7, 'c')",
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT string_agg(s, '-'), median(v), quantile_disc(v, 0.5), quantile_cont(v, [0.0, 1.0]), \
                 mode(g), arg_max(s, v), bool_or(v > 6), stddev_pop(v) > 2, bit_or(v), count_if(v > 2) FROM t"
            ),
            [["a-b-c", "4.0", "3", "[1.0, 7.0]", "1", "c", "true", "true", "7", "3"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT string_agg(s, ',' ORDER BY v DESC) FROM t GROUP BY g ORDER BY g"
            ),
            [["b,a"], ["c"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT list_transform([1, 2], x -> x + 1), list_filter([1, 2, 3], x -> x > 1), \
                 list_reduce([1, 2, 3], (a, b) -> a + b), list_transform([10], (x, i) -> x + i)"
            ),
            [["[2, 3]", "[2, 3]", "6", "[11]"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT g, list_transform([1], x -> x + g) FROM t GROUP BY g ORDER BY g"
            ),
            [["1", "[2]"], ["2", "[3]"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT list_contains([1, 2], 2), list_position([1, 2], 2), list_sum([1, 2]), list_aggr([1, 3], 'max'), \
                 list_reverse([1, 2]), flatten([[1], [2, 3]]), list_has_any([1], [1, 2]), list_resize([1], 2)"
            ),
            [["true", "2", "3", "3", "[2, 1]", "[1, 2, 3]", "true", "[1, NULL]"]]
        );
    }
}
