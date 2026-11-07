// Ported from duckdb/src/main/{database,client_context,materialized_query_result}.cpp
// @ rust-port-baseline
//
//! Databases and client contexts (DuckDB's `src/main`): running SQL text statement by statement through the
//! parser, binder, planner and executor.

mod explain;
pub mod settings;
mod settings_table;

use crate::catalog::{
    Catalog, DEFAULT_CATALOG, Fork, IndexCatalogEntry, OnCreateConflict, SchemaCatalogEntry, SequenceData, SessionTemp,
    TEMP_SCHEMA, TableOrView, macro_type_name, missing_entry, tick,
};
use crate::catalog::{DatabaseList, DatabaseListing};
use crate::execution::alter::{execute_alter, execute_create_index};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::physical::PhysicalResultCollector;
use crate::execution::{create_plan, execute_plan};
use crate::optimizer::join_order::estimate_plan;
use crate::optimizer::{OptimizerOptions, optimize};
use crate::planner::binder::{
    BindOptions, Binder, DefaultNullOrder, ParameterValues, bind_constant_expression, schema_of,
};
use crate::planner::binder::{SessionValues, ShowBehavior, enum_type_from, is_builtin_type};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::storage;
use crate::storage::buffer_manager::BufferManager;
use crate::storage::log_writer::WriteAheadLog;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderType;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ParserOptions;
use duckrs_parser::ast::{
    AlterInfo, AlterKind, CreateInfo, CreateKind, CreateTypeKind, DropInfo, ExplainType, Expr, ExprKind, LiteralKind,
    OnCreateConflict as AstConflict, PragmaInfo, QualifiedName, Statement, sql_quoted_identifier,
};
use duckrs_parser::ast::{TransactionModifier, TransactionType};
use parking_lot::{Mutex, RwLock};
use settings::SettingValues;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::atomic::{AtomicU64, Ordering};

/// A database (`DatabaseInstance`): its catalog, its global settings and, with Phase 9, its
/// storage.
#[derive(Debug)]
pub struct DatabaseInstance {
    pub catalog: Arc<Catalog>,
    /// Shared with the databases that are attached to this one.
    settings: Arc<RwLock<SettingValues>>,
    transactions: Mutex<OpenTransactions>,
    /// held to read while a SELECT runs, and to write while anything else does
    statements: RwLock<()>,
    /// Set when this database came from a file that takes no writes.
    file_access: Option<FileAccess>,
    /// The file of this database, if it has one that checkpoints go to.
    file: Option<DatabaseFile>,
    /// Whether anything changed since the last checkpoint.
    changed: AtomicBool,
    /// `PRAGMA disable_checkpoint_on_shutdown` leaves it to the log.
    checkpoint_on_shutdown: AtomicBool,
    /// `ValidChecker`: why this database takes nothing more until it is opened again.
    invalid: Mutex<Option<String>>,
    /// `WriteAheadLog::sync_failed`: pages may be gone, so the log can't be trusted until reopened.
    log_sync_failed: AtomicBool,
    /// How `USE` and qualified names call this database.
    name: String,
    /// What `ATTACH` added to this database, which connections go to with `USE` or by name. A
    /// query runs in one database, so one that reads two isn't ported.
    attached: RwLock<BTreeMap<String, Arc<DatabaseInstance>>>,
}

/// What `ATTACH` can say of a file.
#[derive(Debug, Default)]
pub struct AttachOptions {
    pub read_only: bool,
    pub block_size: Option<u64>,
    pub row_group_size: Option<u64>,
    /// Those of the database that it is attached to, as a log is replayed with them.
    settings: Option<Arc<RwLock<SettingValues>>>,
}

/// Where a checkpoint goes, and the log of what happened since the last one.
#[derive(Debug)]
struct DatabaseFile {
    path: PathBuf,
    name: String,
    buffer: Arc<BufferManager>,
    log: Mutex<WriteAheadLog>,
    /// What this attachment asked for: a file doesn't say.
    row_group_size: u64,
}

/// Why a database that came from a file takes no writes.
#[derive(Debug)]
enum FileAccess {
    /// It was opened read-only: this is its name.
    ReadOnly(String),
    /// Storing changes is not ported, and they must not get lost silently.
    WritesNotPorted,
}

/// Which connections have a transaction open.
#[derive(Debug, Default)]
struct OpenTransactions {
    open: HashSet<u64>,
}

impl DatabaseInstance {
    fn with_file_access(file_access: Option<FileAccess>) -> Self {
        Self {
            catalog: Arc::new(Catalog::new(DEFAULT_CATALOG)),
            settings: Arc::default(),
            transactions: Mutex::new(OpenTransactions::default()),
            statements: RwLock::new(()),
            file_access,
            file: None,
            changed: AtomicBool::new(false),
            checkpoint_on_shutdown: AtomicBool::new(true),
            invalid: Mutex::new(None),
            log_sync_failed: AtomicBool::new(false),
            name: DEFAULT_CATALOG.to_string(),
            attached: RwLock::default(),
        }
    }

    pub fn new() -> Arc<Self> {
        Arc::new(Self::with_file_access(None))
    }

    /// A database with what is in the file at `path`. Without a database there it stays in memory
    /// and goes away at the end, as all of them did before storage: a new file is not written yet.
    pub fn open_file(path: &Path, read_only: bool) -> Result<Arc<Self>> {
        let options = AttachOptions {
            read_only,
            block_size: None,
            row_group_size: None,
            settings: None,
        };
        Self::open_with(path, &options)
    }

    /// `open_file` with what `ATTACH` can ask for.
    pub fn open_with(path: &Path, options: &AttachOptions) -> Result<Arc<Self>> {
        let read_only = options.read_only;
        if read_only && !storage::is_database_file(path) {
            return Ok(Self::new());
        }
        if !storage::is_database_file(path) {
            storage::create_database(path, options.block_size)?;
        }
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut db = Self::with_file_access(None);
        db.name.clone_from(&name);
        if let Some(settings) = &options.settings {
            db.settings = settings.clone();
        }
        let buffer = storage::load_database(path, &db.catalog, read_only)?;
        db.file_access = if read_only {
            Some(FileAccess::ReadOnly(name.clone()))
        } else if !storage::takes_checkpoints(&buffer) || db.has_foreign_indexes() {
            Some(FileAccess::WritesNotPorted)
        } else {
            None
        };
        let log = Mutex::new(WriteAheadLog::new(&db.catalog));
        db.file = Some(DatabaseFile {
            path: path.to_path_buf(),
            name,
            buffer,
            log,
            row_group_size: options
                .row_group_size
                .unwrap_or(crate::execution::zone_map::ROW_GROUP_SIZE as u64),
        });
        // a log that was replayed can end in anything a crash leaves, so it isn't added to: its
        // records go to the file, and the next commit starts a new one
        if db.file_access.is_none() && storage::has_log(path) {
            db.changed.store(true, AtomicOrdering::SeqCst);
            if db.write_checkpoint().is_err() {
                db.changed.store(false, AtomicOrdering::SeqCst);
                db.file_access = Some(FileAccess::WritesNotPorted);
            }
        }
        Ok(Arc::new(db))
    }

    /// Makes a commit durable, in the log of the file, and checkpoints if the log has grown past
    /// `threshold` bytes and nobody is in the middle of a transaction.
    fn log_commit(&self, threshold: u64, abort: &str) -> Result<()> {
        let Some(file) = self.file.as_ref().filter(|_| self.file_access.is_none()) else {
            return Ok(());
        };
        // what a statement is in the middle of isn't committed, nor are its rows all there
        let reading = self.statements.read();
        let blocks = file.buffer.block_manager();
        let mut log = file.log.lock();
        let identifier = blocks.main_header().db_identifier;
        if !log.commit(
            &file.path,
            &self.catalog,
            &file.name,
            &identifier,
            blocks.header().iteration,
            blocks.block_size(),
        )? {
            return Ok(());
        }
        self.changed.store(true, AtomicOrdering::SeqCst);
        let full = log.size() >= threshold;
        drop(log);
        drop(reading);
        if full && self.transactions.lock().open.is_empty() {
            let _writing = self.statements.write();
            self.write_checkpoint_with(abort)?;
        }
        Ok(())
    }

    /// `SingleFileStorageManager::GetDatabaseSize`: all zeros in memory.
    fn listing(&self) -> DatabaseListing {
        let mut listing = DatabaseListing {
            name: self.name.clone(),
            path: None,
            read_only: self.file_access.is_some(),
            block_size: 0,
            total_blocks: 0,
            free_blocks: 0,
            wal_size: 0,
        };
        if let Some(file) = &self.file {
            let blocks = file.buffer.block_manager();
            listing.path = Some(file.path.display().to_string());
            listing.block_size = blocks.block_alloc_size() as u64;
            listing.total_blocks = blocks.header().block_count;
            listing.free_blocks = blocks.free_list().len() as u64;
            listing.wal_size = storage::log_writer::log_size(&file.path);
        }
        listing
    }

    /// Whether an index came with an extension, as only that can build it again.
    fn has_foreign_indexes(&self) -> bool {
        self.catalog.schemas().iter().any(|schema| {
            schema.tables_and_views().iter().any(|entry| match entry {
                TableOrView::Table(table) => table.indexes.read().iter().any(|index| index.index_type != "ART"),
                TableOrView::View(_) => false,
            })
        })
    }

    /// Writes all of the database to its file, if it has one that takes it.
    fn write_checkpoint(&self) -> Result<()> {
        self.write_checkpoint_with("")
    }

    /// What is a crash in a test is one here too: nothing is undone, nor does anything run again.
    fn invalidate(&self, reason: &str) {
        self.invalid.lock().get_or_insert_with(|| reason.to_string());
        for database in self.attached.read().values() {
            database.invalidate(reason);
        }
    }

    /// `abort` is `debug_checkpoint_abort`, which belongs to the session that asked.
    fn write_checkpoint_with(&self, abort: &str) -> Result<()> {
        let Some(file) = self.file.as_ref().filter(|_| self.file_access.is_none()) else {
            return Ok(());
        };
        if !self.changed.swap(false, AtomicOrdering::SeqCst) {
            return Ok(());
        }
        let mut log = file.log.lock();
        let mut marked = None;
        let log_ref = &mut *log;
        let mut mark = |root: storage::metadata::MetaBlockPointer| {
            marked = Some(log_ref.mark_checkpoint(&file.path, root)?);
            Ok(())
        };
        let force = settings::current_value("force_compression", &self.settings.read(), &SettingValues::default());
        let force = force.and_then(|value| value.try_to_string().ok()).unwrap_or_default();
        let vacuum_tasks =
            settings::current_value("max_vacuum_tasks", &self.settings.read(), &SettingValues::default());
        let vacuum_tasks = vacuum_tasks
            .and_then(|value| value.as_i128())
            .map_or(100, |tasks| tasks as usize);
        let options = storage::writer::CheckpointOptions {
            keep_free_blocks: self.catalog.has_forks(),
            force_compression: &force,
            row_group_size: file.row_group_size,
            vacuum_tasks: if self.catalog.has_forks() { 0 } else { vacuum_tasks },
            abort,
        };
        let result =
            storage::writer::write_checkpoint(&file.path, &file.buffer, &self.catalog, &file.name, options, &mut mark);
        match result {
            Ok(()) => {
                log.checkpointed(&self.catalog);
                Ok(())
            }
            Err(error) if error.kind == ErrorKind::Fatal => {
                drop(log);
                self.invalidate(&error.message);
                Err(error)
            }
            Err(error) => {
                if let Some(size) = marked {
                    log.unmark_checkpoint(&file.path, size)?;
                }
                self.changed.store(true, AtomicOrdering::SeqCst);
                Err(error)
            }
        }
    }

    /// Fails if this database takes no writes.
    fn check_writable(&self, statement: &Statement) -> Result<()> {
        match &self.file_access {
            None => {
                if self.file.is_some() {
                    self.changed.store(true, AtomicOrdering::SeqCst);
                }
                Ok(())
            }
            Some(FileAccess::ReadOnly(name)) => Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Cannot execute statement of type \"{}\" on database \"{name}\" which is attached in read-only mode!",
                    statement_type_name(statement)
                ),
            )),
            Some(FileAccess::WritesNotPorted) => Err(Error::not_implemented(
                "writing to this database file is not yet ported to duckrs: it is from a version before \
                 v2.0.0, or it has an index of an extension",
            )),
        }
    }
}

/// Pragmas that only switch DuckDB's debug verification, profiling, progress output or
/// checkpointing, none of which changes a result; duckrs runs them as no-ops.
const NO_OP_PRAGMAS: &[&str] = &[
    "force_checkpoint",
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

fn storage_version_at_least_v2(version: &str) -> bool {
    version == "latest"
        || version
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok())
            >= Some(2)
}

/// Fails if an index depends on the macro `name`.
fn macro_is_free(schema: &SchemaCatalogEntry, name: &str) -> Result<()> {
    match schema.index_using_macro(name) {
        Some(index) => Err(Error::new(
            ErrorKind::Dependency,
            format!(
                "Cannot drop entry \"{name}\" because there are entries that depend on it.\nindex \"{index}\" depends on macro function \"{name}\"."
            ),
        )),
        None => Ok(()),
    }
}

/// The names of the tables and other entries that a statement refers to directly, for routing it
/// to the database they are in. Subqueries in expressions aren't looked at.
fn for_each_table_name(statement: &mut Statement, f: &mut impl FnMut(&mut QualifiedName)) {
    fn in_table(table: &mut duckrs_parser::ast::TableRef, f: &mut impl FnMut(&mut QualifiedName)) {
        use duckrs_parser::ast::TableRefKind as K;
        match &mut table.kind {
            K::BaseTable { name, .. } => f(name),
            K::Join(join) => {
                in_table(&mut join.left, f);
                in_table(&mut join.right, f);
            }
            K::Subquery(query) => in_query(&mut query.node, f),
            _ => {}
        }
    }
    fn in_query(node: &mut duckrs_parser::ast::QueryNode, f: &mut impl FnMut(&mut QualifiedName)) {
        use duckrs_parser::ast::QueryNodeKind as K;
        for (_, cte) in &mut node.cte_map.0 {
            in_query(&mut cte.query, f);
        }
        match &mut node.kind {
            K::Select(select) => in_table(&mut select.from_table, f),
            K::SetOperation(set) => set.children.iter_mut().for_each(|child| in_query(child, f)),
            _ => {}
        }
        duckrs_parser::transformer::visit_query_node_exprs_mut(node, &mut |e: &mut Expr| in_expression(e, f));
    }
    /// The subquery of `e`, whose parts go by anyway.
    fn in_expression(e: &mut Expr, f: &mut impl FnMut(&mut QualifiedName)) {
        if let ExprKind::Subquery { subquery, .. } = &mut e.kind {
            in_query(&mut subquery.node, f);
        }
    }
    use duckrs_parser::ast::InsertValues;
    match statement {
        Statement::Select(select) => in_query(&mut select.node, f),
        Statement::Insert(insert) => {
            f(&mut insert.table);
            if let InsertValues::Select(query) = &mut insert.values {
                in_query(&mut query.node, f);
            }
        }
        Statement::Update(update) => in_table(&mut update.table, f),
        Statement::Delete(delete) => in_table(&mut delete.table, f),
        Statement::CreateTable(info) => {
            f(&mut info.name);
            if let Some(query) = &mut info.query {
                in_query(&mut query.node, f);
            }
        }
        Statement::Create(info) => f(&mut info.name),
        Statement::Drop(info) => f(&mut info.name),
        Statement::Alter(info) => f(&mut info.name),
        Statement::Explain { statement, .. } => for_each_table_name(statement, f),
        Statement::Vacuum { table: Some(table), .. } => in_table(table, f),
        _ => {}
    }
}

/// A column named with its database, which a statement that runs there doesn't have any more.
fn strip_database(statement: &mut Statement, database: &str) {
    let mut strip = |e: &mut Expr| {
        if let ExprKind::ColumnRef(parts) = &mut e.kind
            && parts.len() > 2
            && parts[0].eq_ignore_ascii_case(database)
        {
            parts.remove(0);
        }
    };
    let mut visit = |e: &mut Expr| duckrs_parser::transformer::visit_expr_mut(e, &mut strip);
    match statement {
        Statement::Select(select) => {
            duckrs_parser::transformer::visit_query_node_exprs_mut(&mut select.node, &mut |e: &mut Expr| {
                visit(e);
            });
        }
        Statement::Insert(insert) => {
            if let Some(info) = insert
                .on_conflict
                .as_mut()
                .and_then(|conflict| conflict.set_info.as_mut())
            {
                info.expressions.iter_mut().for_each(&mut visit);
                info.condition.iter_mut().for_each(|e| visit(e));
            }
        }
        Statement::Update(update) => update.set_info.expressions.iter_mut().for_each(&mut visit),
        Statement::Delete(delete) => delete.condition.iter_mut().for_each(|e| visit(e)),
        _ => {}
    }
}

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
    /// What the connection was made to, which holds what is attached.
    home: Arc<DatabaseInstance>,
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
    /// its temporary objects, which the `temp` catalog holds in C++
    temp: Arc<SchemaCatalogEntry>,
}

/// A transaction started with BEGIN.
#[derive(Debug)]
struct ActiveTransaction {
    /// What `txid_current` says.
    id: u64,
    read_only: bool,
    /// a statement failed in a way that leaves only ROLLBACK (`ValidChecker`)
    invalidated: bool,
    /// the transaction's `TransactionInvalidationPolicy`
    invalidation_policy: String,
    /// What it sees of each database it touched. Going away is what rolls it back.
    forks: Vec<(Arc<DatabaseInstance>, Fork)>,
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
        | Statement::Merge(_)
        | Statement::Drop(_)
        | Statement::Alter(_) => true,
        Statement::Copy(info) => info.is_from,
        // temporary objects are private to the connection
        Statement::CreateTable(info) => !info.temporary,
        Statement::Create(info) => !info.temporary,
        _ => false,
    }
}

/// `StatementTypeToString`, of a statement that `is_write`.
fn statement_type_name(statement: &Statement) -> &'static str {
    match statement {
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Merge(_) => "MERGE_INTO",
        Statement::Drop(_) => "DROP",
        Statement::Alter(_) => "ALTER",
        Statement::Copy(_) => "COPY",
        _ => "CREATE",
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
        let list: Arc<dyn DatabaseList> = db.clone();
        db.catalog.set_database_list(Arc::downgrade(&list));
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self {
            home: db.clone(),
            db,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            transaction: None,
            settings: SettingValues::default(),
            prepared_statements: HashMap::new(),
            parameters: None,
            variables: HashMap::new(),
            temp: SessionTemp::new_schema(),
        }
    }

    /// The current value of a setting (`Settings::Get` / `current_setting`).
    pub fn setting(&self, name: &str) -> Option<Value> {
        settings::current_value(name, &self.home.settings.read(), &self.settings)
    }

    fn setting_text(&self, name: &str) -> String {
        self.setting(name)
            .and_then(|v| v.try_to_string().ok())
            .unwrap_or_default()
    }

    /// What `current_setting` and `getvariable` read.
    fn session_values(&self) -> Arc<SessionValues> {
        let db = self.home.clone();
        let session = self.settings.clone();
        Arc::new(SessionValues {
            variables: self.variables.clone(),
            setting: Box::new(move |name| settings::get_setting(name, &db.settings.read(), &session)),
        })
    }

    /// The settings the binder needs.
    /// `enable_optimizer` and `disabled_optimizers`.
    fn optimizer_options(&self) -> OptimizerOptions {
        let mut options = if self.setting_text("enable_optimizer") == "false" {
            OptimizerOptions::none()
        } else {
            OptimizerOptions::from_disabled(&self.setting_text("disabled_optimizers"))
        };
        if let Ok(percentage) = self.setting_text("index_scan_percentage").parse() {
            options.index_scan_percentage = percentage;
        }
        if let Ok(count) = self.setting_text("index_scan_max_count").parse() {
            options.index_scan_max_count = count;
        }
        if let Ok(threshold) = self.setting_text("dynamic_or_filter_threshold").parse() {
            options.dynamic_or_filter_threshold = threshold;
        }
        options
    }

    fn bind_options(&self) -> BindOptions {
        BindOptions {
            transaction_id: self
                .transaction
                .as_ref()
                .map_or_else(tick, |transaction| transaction.id),
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
            optimizer: self.optimizer_options(),
            single_arrow_lambdas: self.setting_text("lambda_syntax") == "ENABLE_SINGLE_ARROW",
            pivot_limit: self.setting_text("pivot_limit").parse().unwrap_or(100_000),
            scalar_subquery_error: self.setting_text("scalar_subquery_error_on_multiple_rows") != "false",
            show_behavior: match self.setting_text("show_behavior").to_uppercase().as_str() {
                "SETTING" => ShowBehavior::Setting,
                "TABLE" => ShowBehavior::Table,
                _ => ShowBehavior::Auto,
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
        let _temp = SessionTemp::enter(&self.temp);
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
        let _temp = SessionTemp::enter(&self.temp);
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
                | Statement::Vacuum { .. }
        );
        if !client_statement {
            let catalog = self.catalog();
            let mut binder = Binder::with_options(&catalog, self.bind_options());
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
        let _temp = SessionTemp::enter(&self.temp);
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
                    id: tick(),
                    read_only: modifier == TransactionModifier::ReadOnly,
                    invalidated: false,
                    invalidation_policy: self.setting_text("default_transaction_invalidation_policy"),
                    forks: Vec::new(),
                });
                if self
                    .setting_text("immediate_transaction_mode")
                    .eq_ignore_ascii_case("true")
                {
                    self.start_on_database();
                }
            }
            TransactionType::Commit => {
                let Some(transaction) = self.transaction.take() else {
                    return Err(error("cannot commit - no transaction is active"));
                };
                // COMMIT of an invalidated transaction rolls it back
                let rollback = transaction.invalidated;
                if !rollback
                    && transaction.forks.iter().any(|(_, fork)| fork.dirty)
                    && let Some(error) = self.forced_commit_failure()
                {
                    self.end_transaction(transaction);
                    return Err(error);
                }
                if !rollback {
                    for (database, fork) in &transaction.forks {
                        let _writing = database.statements.write();
                        if let Err(error) = fork.commit() {
                            drop(_writing);
                            self.end_transaction(transaction);
                            return Err(error);
                        }
                    }
                }
                let databases: Vec<_> = transaction.forks.iter().map(|(database, _)| database.clone()).collect();
                self.end_transaction(transaction);
                if !rollback {
                    let current = self.db.clone();
                    let mut result = Ok(());
                    for database in databases {
                        self.db = database;
                        result = result.and(self.log_commit());
                    }
                    self.db = current;
                    result?;
                }
            }
            TransactionType::Rollback => {
                let Some(transaction) = self.transaction.take() else {
                    return Err(error("cannot rollback - no transaction is active"));
                };
                self.end_transaction(transaction);
            }
        }
        Ok(Self::success())
    }

    /// A commit, as far as this connection can fail one.
    fn log_commit(&self) -> Result<()> {
        let abort = self.setting_text("debug_checkpoint_abort").to_lowercase();
        self.db.log_commit(self.checkpoint_threshold(), &abort)
    }

    /// What `debug_force_commit_failure` and `debug_force_wal_fsync_failure` turn a commit into,
    /// before anything of it reached the log.
    fn forced_commit_failure(&self) -> Option<Error> {
        if self
            .setting_text("debug_force_commit_failure")
            .eq_ignore_ascii_case("true")
        {
            // what can't be reverted leaves nothing to go on with
            if self
                .setting_text("debug_force_commit_revert_failure")
                .eq_ignore_ascii_case("true")
            {
                self.db
                    .invalidate("Forced RevertCommit failure (debug_force_commit_revert_failure)");
            }
            return Some(Error::new(
                ErrorKind::InvalidInput,
                "Forced commit failure (debug_force_commit_failure)",
            ));
        }
        let file = self.db.file.as_ref()?;
        if self.db.log_sync_failed.load(AtomicOrdering::SeqCst) {
            return Some(Error::new(
                ErrorKind::Io,
                format!(
                    "Cannot sync WAL \"{}.wal\": a previous sync of this WAL has failed",
                    file.path.display()
                ),
            ));
        }
        if self
            .setting_text("debug_force_wal_fsync_failure")
            .eq_ignore_ascii_case("true")
        {
            self.db.log_sync_failed.store(true, AtomicOrdering::SeqCst);
            return Some(Error::new(
                ErrorKind::Io,
                "debug_force_wal_fsync_failure: injected WAL fsync failure",
            ));
        }
        if self
            .setting_text("debug_checkpoint_abort")
            .eq_ignore_ascii_case("before_wal_flush")
        {
            const CRASH: &str = "Simulated crash before WAL_FLUSH write";
            self.db.invalidate(CRASH);
            return Some(Error::new(ErrorKind::Fatal, CRASH));
        }
        None
    }

    fn commit_may_fail(&self) -> bool {
        self.db.log_sync_failed.load(AtomicOrdering::SeqCst)
            || self
                .setting_text("debug_force_commit_failure")
                .eq_ignore_ascii_case("true")
            || self
                .setting_text("debug_force_wal_fsync_failure")
                .eq_ignore_ascii_case("true")
            || self
                .setting_text("debug_checkpoint_abort")
                .eq_ignore_ascii_case("before_wal_flush")
    }

    /// `ValidChecker`: a database that had a fatal error takes nothing more, and neither does what
    /// else is attached if it was the one this connection was made to.
    fn check_valid(&self, database: &DatabaseInstance) -> Result<()> {
        if let Some(reason) = &*self.home.invalid.lock() {
            return Err(Error::new(
                ErrorKind::Fatal,
                format!(
                    "Failed: database has been invalidated because of a previous fatal error. The database must be \
                     restarted prior to being used again.\nOriginal error: \"{reason}\""
                ),
            ));
        }
        if let Some(reason) = &*database.invalid.lock() {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "Database \"{}\" has been invalidated because checkpointing failed. Detach and reattach it before \
                     using it again. Original error: {reason}",
                    database.name
                ),
            ));
        }
        Ok(())
    }

    fn end_transaction(&self, transaction: ActiveTransaction) {
        for (database, _) in &transaction.forks {
            database.transactions.lock().open.remove(&self.id);
        }
        self.db.transactions.lock().open.remove(&self.id);
    }

    /// The catalog that statements of this connection use: the fork of the transaction, or the
    /// database's own.
    fn catalog(&self) -> Arc<Catalog> {
        self.fork()
            .map_or_else(|| self.db.catalog.clone(), |fork| fork.catalog.clone())
    }

    fn fork(&self) -> Option<&Fork> {
        let forks = &self.transaction.as_ref()?.forks;
        forks
            .iter()
            .map(|(_, fork)| fork)
            .find(|fork| fork.is_of(&self.db.catalog))
    }

    /// Whether the table that `statement` writes to is one that is private to the connection.
    fn writes_temporary_table(&self, statement: &Statement) -> bool {
        let table_of = |table: &duckrs_parser::ast::TableRef| match &table.kind {
            duckrs_parser::ast::TableRefKind::BaseTable { name, .. } => Some(name.clone()),
            _ => None,
        };
        let name = match statement {
            Statement::Insert(insert) => Some(insert.table.clone()),
            Statement::Update(update) => table_of(&update.table),
            Statement::Delete(delete) => table_of(&delete.table),
            Statement::Alter(info) => Some(info.name.clone()),
            Statement::Create(info) => match &info.kind {
                CreateKind::Index { table, .. } => Some(duckrs_parser::ast::QualifiedName {
                    qualification: info.name.qualification.clone(),
                    name: table.clone(),
                }),
                _ => None,
            },
            _ => None,
        };
        name.is_some_and(|name| {
            self.catalog()
                .get_table(schema_of(&name).ok().flatten(), &name.name)
                .is_ok_and(|table| table.schema.eq_ignore_ascii_case(TEMP_SCHEMA))
        })
    }

    /// `MetaTransaction::GetTransaction`: a transaction starts on a database when it first uses it.
    fn start_on_database(&mut self) {
        if self.fork().is_some() {
            return;
        }
        let db = self.db.clone();
        if let Some(transaction) = &mut self.transaction {
            let fork = {
                let _reading = db.statements.read();
                db.catalog.fork()
            };
            db.transactions.lock().open.insert(self.id);
            transaction.forks.push((db, fork));
        }
    }

    /// Runs one statement within the current transaction, or as a transaction of its own
    /// (`ClientContext::PendingStatementOrPreparedStatementInternal` + `CleanupInternal`).
    fn run_statement(&mut self, statement: &Statement) -> Result<MaterializedQueryResult> {
        if let Some((database, routed)) = self.route_to_database(statement) {
            let previous = std::mem::replace(&mut self.db, database);
            let result = self.run_statement(&routed);
            self.db = previous;
            return result;
        }
        match statement {
            // the statements a PIVOT was rewritten into: the last one has the result
            Statement::Multi(statements) => {
                // they are one statement, so they go together or not at all
                let implicit = self.transaction.is_none();
                if implicit {
                    self.run_transaction_statement(TransactionType::Begin, TransactionModifier::Default)?;
                }
                let mut result = Ok(Self::success());
                for statement in statements {
                    result = self.run_statement(statement);
                    if result.is_err() {
                        break;
                    }
                }
                if implicit {
                    let end = if result.is_ok() {
                        TransactionType::Commit
                    } else {
                        TransactionType::Rollback
                    };
                    self.run_transaction_statement(end, TransactionModifier::Default)?;
                }
                return result;
            }
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
            // `CheckIfPreparedStatementIsExecutable` comes after binding
            if matches!(
                statement,
                Statement::Select(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
            ) {
                self.prepare_statement(statement.clone())?;
            }
            return Err(Error::new(ErrorKind::Transaction, INVALIDATED_TRANSACTION));
        }
        self.check_valid(&self.db)?;
        let write = is_write(statement);
        if write {
            self.db.check_writable(statement)?;
        }
        self.start_on_database();
        let writes_temporary_table = self.writes_temporary_table(statement);
        // what a failed statement outside of a transaction undoes: INSERT is the one that can
        // fail halfway through
        let mut appending = None;
        let stops_checkpoints = match statement {
            Statement::Delete(_) => false,
            Statement::Insert(insert) => insert.on_conflict.as_ref().is_some_and(|c| c.set_info.is_some()),
            _ => true,
        };
        if write {
            match &mut self.transaction {
                Some(transaction) if transaction.read_only && !writes_temporary_table => {
                    return Err(Error::new(
                        ErrorKind::Transaction,
                        format!(
                            "Cannot write to database {} - transaction is launched in read-only mode",
                            sql_quoted_identifier(DEFAULT_CATALOG)
                        ),
                    ));
                }
                Some(transaction) => {
                    let forks = transaction.forks.iter_mut().map(|(_, fork)| fork);
                    if let Some(fork) = forks.into_iter().find(|fork| fork.is_of(&self.db.catalog)) {
                        fork.mark_dirty(stops_checkpoints);
                    }
                }
                None => {}
            }
            if let Statement::Insert(insert) = statement
                && let Ok(table) = self
                    .catalog()
                    .get_table(schema_of(&insert.table).ok().flatten(), &insert.table.name)
            {
                appending = Some(table);
            }
        }
        // outside of a transaction only a commit that is made to fail needs one of these
        let undo = (write && self.transaction.is_none() && self.commit_may_fail()).then(|| self.catalog().snapshot());
        // what isn't a change to rows can conflict with what transactions did to the catalog
        let changes_entries = write
            && !matches!(
                statement,
                Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) | Statement::Merge(_)
            );
        let db = self.db.clone();
        let result = if self.transaction.is_some() {
            // on a fork of its own, which nobody else writes to
            let before = changes_entries.then(|| self.catalog().snapshot());
            let rows = appending.as_ref().map(|table| table.row_count());
            let result = self.execute_statement(statement);
            if result.is_err()
                && let (Some(table), Some(rows)) = (&appending, rows)
            {
                table.truncate(rows);
            }
            match (result, before) {
                (Ok(result), Some(before)) => match self.fork().expect("started above").claim_changes() {
                    Ok(()) => Ok(result),
                    Err(error) => {
                        self.catalog().restore(before);
                        Err(error)
                    }
                },
                (result, _) => result,
            }
        } else if matches!(statement, Statement::Select(_)) {
            let _reading = db.statements.read();
            self.execute_statement(statement)
        } else if matches!(statement, Statement::Call(function) if checkpoint_call(function).is_some()) {
            // which takes the lock of the database that it is about
            self.execute_statement(statement)
        } else {
            // UPDATE and DELETE rewrite tables, which would pull the rows from under a scan
            let _writing = db.statements.write();
            let before = (changes_entries && db.catalog.has_forks()).then(|| db.catalog.snapshot());
            // what others added stays: this is only known for sure while nobody else can write
            let rows = appending.as_ref().map(|table| table.row_count());
            let result = self.execute_statement(statement);
            if result.is_err()
                && let (Some(table), Some(rows)) = (&appending, rows)
            {
                table.truncate(rows);
            }
            match (result, before) {
                (Ok(result), Some(before)) => match db.catalog.claim_changes(&before) {
                    Ok(()) => Ok(result),
                    Err(error) => {
                        db.catalog.restore(before);
                        Err(error)
                    }
                },
                (result, _) => result,
            }
        };
        if let Err(error) = &result {
            if let Some(transaction) = &mut self.transaction {
                transaction.apply_invalidation_policy(error.kind);
            }
        } else if self.transaction.is_none() {
            if let Some(snapshot) = undo
                && let Some(error) = self.forced_commit_failure()
            {
                self.catalog().restore(snapshot);
                return Err(error);
            }
            // also for a SELECT, as `nextval` is one
            self.log_commit()?;
        }
        result
    }

    /// The database of this name, among those this connection can reach.
    fn find_database(&self, name: &str) -> Option<Arc<DatabaseInstance>> {
        if name.eq_ignore_ascii_case(&self.home.name) {
            return Some(self.home.clone());
        }
        if name.eq_ignore_ascii_case(&self.db.name) {
            return Some(self.db.clone());
        }
        self.home.attached.read().get(&name.to_lowercase()).cloned()
    }

    /// A statement that names another database runs there, without that name. The first part of
    /// `a.b` is a schema if this database has one, as in C++.
    fn route_to_database(&self, statement: &Statement) -> Option<(Arc<DatabaseInstance>, Statement)> {
        let mut names = Vec::new();
        let mut routed = statement.clone();
        for_each_table_name(&mut routed, &mut |name| names.push(name.clone()));
        let database = names.iter().find_map(|name| {
            let first = name.qualification.first()?;
            let is_schema = name.qualification.len() == 1
                && (self.catalog().get_schema(first).is_some() || first.eq_ignore_ascii_case(TEMP_SCHEMA));
            if is_schema {
                return None;
            }
            self.find_database(first)
        })?;
        let prefix = database.name.clone();
        for_each_table_name(&mut routed, &mut |name| {
            if name
                .qualification
                .first()
                .is_some_and(|first| first.eq_ignore_ascii_case(&prefix))
            {
                name.qualification.remove(0);
            }
        });
        strip_database(&mut routed, &prefix);
        Some((database, routed))
    }

    /// `USE`: the database, and a schema in it if it isn't the default one.
    fn use_database(&mut self, target: &str) -> Result<bool> {
        let parts: Vec<&str> = target.split('.').map(|part| part.trim_matches('"')).collect();
        let Some(database) = self.find_database(parts[0]) else {
            return Ok(false);
        };
        if parts.get(1).is_some_and(|schema| !schema.eq_ignore_ascii_case("main")) {
            return Ok(false);
        }
        self.db = database;
        Ok(true)
    }

    /// `PhysicalAttach`.
    fn attach(&mut self, info: &duckrs_parser::ast::AttachInfo) -> Result<()> {
        use duckrs_parser::ast::OnCreateConflict as Conflict;
        if info.external_resource.is_some() {
            return Err(crate::unported("ATTACH TO EXTERNAL RESOURCE"));
        }
        let path = evaluate_scalar(&bind_constant_expression(&self.catalog(), &info.path)?)?.try_to_string()?;
        let mut options = AttachOptions::default();
        for (name, value) in &info.options {
            let text = value.text.to_lowercase();
            match name.to_lowercase().as_str() {
                "readonly" | "read_only" => options.read_only = text != "false",
                "readwrite" | "read_write" => options.read_only = text == "false",
                "type" if text == "duckdb" => {}
                // files are written as v2.0.0, and nothing else is: an older one has to be refused
                "storage_version" if storage_version_at_least_v2(&text) => {}
                "block_size" => {
                    let size = text
                        .parse()
                        .map_err(|_| Error::new(ErrorKind::InvalidInput, "BLOCK_SIZE is not a number"))?;
                    options.block_size = Some(size);
                }
                "row_group_size" => {
                    let size: u64 = text
                        .parse()
                        .map_err(|_| Error::new(ErrorKind::InvalidInput, "ROW_GROUP_SIZE is not a number"))?;
                    let wrong = |why: &str| {
                        Error::new(
                            ErrorKind::NotImplemented,
                            format!("Invalid row group size: {size} - row group size must be {why}"),
                        )
                    };
                    if size == 0 {
                        return Err(wrong("bigger than 0"));
                    }
                    if !size.is_multiple_of(duckrs_common::STANDARD_VECTOR_SIZE as u64) {
                        return Err(wrong(&format!(
                            "divisible by the vector size ({})",
                            duckrs_common::STANDARD_VECTOR_SIZE
                        )));
                    }
                    options.row_group_size = Some(size);
                }
                other => return Err(crate::unported(format!("ATTACH with {other}"))),
            }
        }
        if !info.parsed_options.is_empty() {
            let names: Vec<&str> = info.parsed_options.iter().map(|(name, _)| name.as_str()).collect();
            return Err(crate::unported(format!("ATTACH with {}", names.join(", "))));
        }
        let in_memory = path.is_empty() || path == ":memory:";
        let stem = Path::new(&path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let name = if !info.name.is_empty() {
            info.name.clone()
        } else if in_memory {
            DEFAULT_CATALOG.to_string()
        } else {
            stem
        };
        if self.find_database(&name).is_some() {
            if info.on_conflict == Conflict::Ignore {
                return Ok(());
            }
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Failed to attach database: database with name \"{name}\" already exists"),
            ));
        }
        let database = if in_memory {
            let mut database = DatabaseInstance::with_file_access(None);
            database.name.clone_from(&name);
            database.settings = self.home.settings.clone();
            Arc::new(database)
        } else {
            options.settings = Some(self.home.settings.clone());
            let mut opened = DatabaseInstance::open_with(Path::new(&path), &options)?;
            let Some(database) = Arc::get_mut(&mut opened) else {
                return Err(Error::new(ErrorKind::Internal, "a new database is in use"));
            };
            database.name.clone_from(&name);
            opened
        };
        database.catalog.set_database_list(self.database_list());
        self.home.attached.write().insert(name.to_lowercase(), database);
        Ok(())
    }

    fn database_list(&self) -> std::sync::Weak<dyn DatabaseList> {
        let list: Arc<dyn DatabaseList> = self.home.clone();
        Arc::downgrade(&list)
    }

    /// `PhysicalDetach`.
    fn detach(&mut self, name: &str, if_exists: bool) -> Result<()> {
        if name.eq_ignore_ascii_case(&self.db.name) {
            return Err(Error::new(
                ErrorKind::Binder,
                format!(
                    "Cannot detach database \"{name}\" because it is the default database. Select a different database using `USE` to allow detaching."
                ),
            ));
        }
        if self.home.attached.write().remove(&name.to_lowercase()).is_none() && !if_exists {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Failed to detach database with name \"{name}\": database not found"),
            ));
        }
        Ok(())
    }

    /// `checkpoint_threshold` in bytes: how large the log can get.
    fn checkpoint_threshold(&self) -> u64 {
        const DEFAULT: u64 = 16 << 20;
        let text = self.setting_text("checkpoint_threshold");
        let text = text.trim();
        let digits = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(text.len());
        let Ok(number) = text[..digits].parse::<f64>() else {
            return DEFAULT;
        };
        let unit: u64 = match text[digits..].trim().to_lowercase().as_str() {
            "" | "b" | "bytes" => 1,
            "kb" | "kilobytes" => 1000,
            "mb" | "megabytes" => 1000 * 1000,
            "gb" | "gigabytes" => 1000 * 1000 * 1000,
            "tb" | "terabytes" => 1000 * 1000 * 1000 * 1000,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            "tib" => 1 << 40,
            _ => return DEFAULT,
        };
        (number * unit as f64) as u64
    }

    /// `PhysicalExplain`: the plan in text form, as a key and a value per plan.
    /// What `EXECUTE` names, and the values it gives.
    fn bound_execute(
        &self,
        name: &str,
        values: &[(String, Expr)],
    ) -> Result<(Arc<PreparedStatementData>, HashMap<String, Value>)> {
        let Some(prepared) = self.prepared_statements.get(&name.to_lowercase()).cloned() else {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Prepared statement {} does not exist", sql_quoted_identifier(name)),
            ));
        };
        let mut bound = HashMap::with_capacity(values.len());
        for (parameter, value) in values {
            let value = evaluate_scalar(&bind_constant_expression(&self.catalog(), value)?)?;
            bound.insert(parameter.to_lowercase(), value);
        }
        Ok((prepared, bound))
    }

    /// `COMMENT ON INDEX`: a table of a transaction gets an entry of its own.
    fn comment_on_index(&self, info: &AlterInfo) -> Result<MaterializedQueryResult> {
        let AlterKind::SetComment { value, .. } = &info.kind else {
            unreachable!("checked by the caller");
        };
        let catalog = self.catalog();
        let schema = catalog.schema_for(schema_of(&info.name)?)?;
        let Some((table, index)) = schema.get_index(&info.name.name) else {
            return Err(missing_entry("Index", &info.name.name));
        };
        let comment = (!matches!(value.kind, LiteralKind::Null)).then(|| value.text.clone());
        let changed = Arc::new(IndexCatalogEntry {
            comment,
            ..(*index).clone()
        });
        for entry in table
            .indexes
            .write()
            .iter_mut()
            .filter(|entry| Arc::ptr_eq(entry, &index))
        {
            *entry = changed.clone();
        }
        Ok(Self::success())
    }

    fn explain(
        &mut self,
        statement: &Statement,
        explain_type: ExplainType,
        format: &str,
    ) -> Result<MaterializedQueryResult> {
        if explain_type == ExplainType::Sql {
            return Err(crate::unported("EXPLAIN (SQL)"));
        }
        let catalog = self.catalog();
        let mut binder = Binder::with_options(&catalog, self.bind_options());
        binder.parameters = self.parameters.clone();
        binder.session = Some(self.session_values());
        let bound = binder.bind(statement)?;
        let upper = self
            .setting_text("profiling_renderer_settings")
            .to_lowercase()
            .replace(['\'', ' '], "")
            .contains("operator_casing=upper");
        let logical = explain::render(&explain::logical_tree(&bound.plan, None), format, upper);
        let mut plan = optimize(bound.plan, self.optimizer_options());
        let optimized = plan.clone();
        let estimate = estimate_plan(&plan);
        let logical_optimized = explain::render(&explain::logical_tree(&plan, Some(&estimate)), format, upper);
        resolve_column_bindings(&mut plan)?;
        let physical_plan = create_plan(plan);
        let analyze = explain_type == ExplainType::Analyze;
        let physical = explain::physical_tree(physical_plan.as_ref(), Some(&optimized), Some(&estimate), analyze);
        let physical = explain::render(&physical, format, upper);
        if explain_type == ExplainType::Analyze {
            self.execute_statement(statement)?;
        }
        let rows: Vec<(&str, String)> = if explain_type == ExplainType::Analyze {
            vec![("analyzed_plan", physical)]
        } else {
            match self.setting_text("explain_output").to_uppercase().as_str() {
                "ALL" => vec![
                    ("logical_plan", logical.clone()),
                    ("logical_opt", logical_optimized),
                    ("physical_plan", physical),
                ],
                "OPTIMIZED_ONLY" => vec![("logical_opt", logical_optimized)],
                _ => vec![("physical_plan", physical)],
            }
        };
        let names = vec!["explain_key".to_string(), "explain_value".to_string()];
        let types = vec![LogicalType::VARCHAR, LogicalType::VARCHAR];
        let mut collection = ColumnDataCollection::new(types.clone());
        let mut chunk = duckrs_common::vector::DataChunk::new(&types);
        for (key, value) in rows {
            chunk.append_row(&[Value::varchar(key), Value::varchar(value)])?;
        }
        collection.append(&chunk);
        Ok(MaterializedQueryResult {
            names,
            types,
            collection,
        })
    }

    /// `CheckpointFunction`. In memory there is nothing to write, so only its errors are left.
    fn checkpoint(&mut self, function: &Expr) -> Result<MaterializedQueryResult> {
        let (force, argument) = checkpoint_call(function).expect("checked by the caller");
        let mut target = None;
        if let Some(argument) = argument {
            let database = evaluate_scalar(&bind_constant_expression(&self.catalog(), argument)?)?;
            if database.is_null() {
                return Err(Error::new(ErrorKind::Binder, "Database cannot be NULL"));
            }
            let database = database.try_to_string()?;
            if database.eq_ignore_ascii_case("system") || database.eq_ignore_ascii_case("temp") {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("Cannot checkpoint database {database}: it does not have storage"),
                ));
            }
            target = self.find_database(&database);
            if target.is_none() {
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
            if transaction.forks.iter().any(|(_, fork)| fork.dirty) {
                return Err(Error::new(
                    ErrorKind::Transaction,
                    "Cannot CHECKPOINT: the current transaction has transaction local changes",
                ));
            }
        }
        let database = target.unwrap_or_else(|| self.db.clone());
        self.check_valid(&database)?;
        let abort = self.setting_text("debug_checkpoint_abort").to_lowercase();
        let mut writing = database.statements.write();
        while database.catalog.has_writers() {
            if !force {
                return Err(Error::new(
                    ErrorKind::Transaction,
                    "Cannot CHECKPOINT: there are other write transactions active. Try using FORCE CHECKPOINT to \
                     wait until all active transactions are finished",
                ));
            }
            // they need this lock to finish
            drop(writing);
            std::thread::sleep(std::time::Duration::from_millis(1));
            writing = database.statements.write();
        }
        let result = database.write_checkpoint_with(&abort);
        drop(writing);
        // whatever went wrong, what is on disk can't be vouched for now; only the database that a
        // connection was made to takes everything down with it
        let Err(error) = result else {
            return Ok(Self::success());
        };
        database.invalidate(&error.message);
        if Arc::ptr_eq(&database, &self.home) {
            return Err(Error::new(
                ErrorKind::Fatal,
                format!("Failed to create checkpoint because of error: {}", error.message),
            ));
        }
        Err(Error::new(
            ErrorKind::Io,
            format!(
                "Checkpoint failed for database \"{}\". The database has been invalidated. Original error: {}",
                database.name, error.message
            ),
        ))
    }

    /// `ClientContext::PendingStatementOrPreparedStatement` + `ExecutePendingQueryInternal`.
    fn execute_statement(&mut self, statement: &Statement) -> Result<MaterializedQueryResult> {
        match statement {
            // statistics here are made when they are needed, and in-memory tables have nothing to
            // compact, so this only says whether there is such a table
            Statement::Vacuum { table, .. } => {
                if let Some(table) = table
                    && let duckrs_parser::ast::TableRefKind::BaseTable { name, .. } = &table.kind
                {
                    let schema = crate::planner::binder::schema_of(name)?;
                    if let crate::catalog::TableOrView::View(_) = self.catalog().get_entry(schema, None, &name.name)? {
                        return Err(Error::new(ErrorKind::Binder, "Can only vacuum or analyze base tables"));
                    }
                }
                return Ok(Self::success());
            }
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
            Statement::SetVariable { name, value, .. }
                if name.eq_ignore_ascii_case("schema")
                    && let Ok(target) = evaluate_scalar(&bind_constant_expression(&self.catalog(), value)?)
                        .and_then(|value| value.try_to_string())
                    && self.use_database(&target)? =>
            {
                return Ok(Self::success());
            }
            Statement::Attach(info) => {
                self.attach(info)?;
                return Ok(Self::success());
            }
            Statement::Detach { name, if_exists } => {
                self.detach(name, *if_exists)?;
                return Ok(Self::success());
            }
            Statement::SetVariable { name, value, scope } => {
                // `Binder::Bind(SetVariableStatement)` + `PhysicalSet`
                let value = evaluate_scalar(&bind_constant_expression(&self.catalog(), value)?)?;
                if name.eq_ignore_ascii_case("current_transaction_invalidation_policy")
                    && let Some(transaction) = &mut self.transaction
                {
                    transaction.invalidation_policy = value.try_to_string()?.to_uppercase();
                }
                let mut global = self.home.settings.write();
                settings::set_variable(name, *scope, &value, &mut global, &mut self.settings)?;
                return Ok(Self::success());
            }
            Statement::ResetVariable { name, scope } => {
                let mut global = self.home.settings.write();
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
                let lower = name.to_lowercase();
                if parameters.is_empty() && lower.ends_with("_checkpoint_on_shutdown") {
                    let enable = lower == "enable_checkpoint_on_shutdown";
                    self.db.checkpoint_on_shutdown.store(enable, AtomicOrdering::SeqCst);
                    return Ok(Self::success());
                }
                let sql = match (lower.as_str(), parameters.as_slice()) {
                    ("enable_optimizer", []) => Some("SET enable_optimizer = true".to_string()),
                    ("disable_optimizer", []) => Some("SET enable_optimizer = false".to_string()),
                    ("show_tables", []) => Some("SHOW TABLES".to_string()),
                    ("show_tables_expanded", []) => Some("SHOW ALL TABLES".to_string()),
                    ("show_databases" | "database_list", []) => Some("SELECT * FROM duckdb_databases()".to_string()),
                    ("table_info" | "show", [table]) => {
                        Some(format!("SELECT * FROM pragma_{lower}({})", pragma_argument(table)))
                    }
                    _ => None,
                };
                if let Some(sql) = sql {
                    let query = duckrs_parser::parse_ast(&sql)?.remove(0);
                    return self.execute_statement(&query);
                }
                return Err(crate::unported(format!("PRAGMA {name}")));
            }
            Statement::Explain {
                statement,
                explain_type,
                format,
            } => {
                let Statement::Execute { name, values } = &**statement else {
                    return self.explain(statement, *explain_type, format);
                };
                let (prepared, values) = self.bound_execute(name, values)?;
                let parameters = ParameterValues {
                    values: Some(values),
                    ..ParameterValues::default()
                };
                let saved = self.parameters.replace(Arc::new(parameters));
                let result = self.explain(&prepared.statement, *explain_type, format);
                self.parameters = saved;
                return result;
            }
            Statement::Alter(info) if matches!(&info.kind, AlterKind::SetComment { entry_type, .. } if *entry_type == "INDEX") =>
            {
                return self.comment_on_index(info);
            }
            Statement::Alter(info) => {
                let catalog = self.catalog();
                let mut binder = Binder::new(&catalog);
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
                let (prepared, bound) = self.bound_execute(name, values)?;
                return self.execute_with(&prepared, bound);
            }
            // what is compiled in needs no loading
            Statement::Load(info)
                if matches!(
                    info.load_type,
                    duckrs_parser::ast::LoadType::Load | duckrs_parser::ast::LoadType::Install
                ) && ["parquet", "json"]
                    .iter()
                    .any(|name| info.filename.eq_ignore_ascii_case(name)) =>
            {
                return Ok(Self::success());
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
                let catalog = self.catalog();
                let mut binder = Binder::new(&catalog);
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
                if info.temporary {
                    return Err(Error::new(ErrorKind::Binder, "Temporary schemas are not supported"));
                }
                if !options.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        "WITH clause is not supported for schemas",
                    ));
                }
                if !catalog_qualifier_only(&info.name.qualification) {
                    return Err(crate::unported("this form of CREATE SCHEMA"));
                }
                self.catalog()
                    .create_schema(&info.name.name, on_create_conflict(info.on_conflict))?;
                return Ok(Self::success());
            }
            Statement::Drop(info) => {
                self.drop_entry(info)?;
                return Ok(Self::success());
            }
            _ => {}
        }
        let catalog = self.catalog();
        let mut binder = Binder::with_options(&catalog, self.bind_options());
        binder.parameters = self.parameters.clone();
        binder.session = Some(self.session_values());
        let bound = binder.bind(statement)?;
        let mut plan = optimize(bound.plan, self.optimizer_options());
        resolve_column_bindings(&mut plan)?;
        let plan = create_plan(plan);
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

/// The schema a CREATE puts its object in: temporary objects have one of their own.
fn create_schema_of(info: &CreateInfo) -> Result<Option<&str>> {
    if info.temporary {
        return Ok(Some(TEMP_SCHEMA));
    }
    schema_of(&info.name)
}

/// The argument of a PRAGMA as a string: a bare name counts as one.
fn pragma_argument(argument: &Expr) -> String {
    let text = match &argument.kind {
        ExprKind::ColumnRef(names) => names.join("."),
        ExprKind::Constant(literal) => literal.text.clone(),
        _ => argument.to_string(),
    };
    format!("'{}'", text.replace('\'', "''"))
}

fn on_create_conflict(conflict: AstConflict) -> OnCreateConflict {
    match conflict {
        AstConflict::Error => OnCreateConflict::Error,
        AstConflict::Ignore => OnCreateConflict::Ignore,
        AstConflict::Replace => OnCreateConflict::Replace,
    }
}

impl DatabaseList for DatabaseInstance {
    fn with_catalog(&self, name: &str, use_it: &mut dyn FnMut(&crate::catalog::Catalog)) {
        if name.eq_ignore_ascii_case(&self.name) {
            use_it(&self.catalog);
        } else if let Some(database) = self.attached.read().get(&name.to_lowercase()) {
            use_it(&database.catalog);
        }
    }

    fn databases(&self) -> Vec<DatabaseListing> {
        let attached = self.attached.read();
        std::iter::once(self)
            .chain(attached.values().map(Arc::as_ref))
            .map(DatabaseInstance::listing)
            .collect()
    }
}

impl Drop for DatabaseInstance {
    /// `CheckpointOnShutdown`: leaves nothing for the next open to replay.
    fn drop(&mut self) {
        if !self.checkpoint_on_shutdown.load(AtomicOrdering::SeqCst) || self.invalid.lock().is_some() {
            return;
        }
        if let Err(error) = self.write_checkpoint() {
            eprintln!("duckrs: failed to write to the database file: {error}");
        }
    }
}

impl Drop for ClientContext {
    /// A connection that goes away rolls back what it did not commit.
    fn drop(&mut self) {
        if let Some(transaction) = self.transaction.take() {
            self.end_transaction(transaction);
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
        if *secure {
            return Err(crate::unported("CREATE SECURE VIEW"));
        }
        // without them a view does not hold on to what it reads, which is the setting's whole point
        if self.setting_text("enable_view_dependencies") == "true" {
            return Err(crate::unported("view dependencies"));
        }
        let catalog = self.catalog();
        let schema = catalog.schema_for(create_schema_of(info)?)?;
        let on_conflict = on_create_conflict(info.on_conflict);
        // IF NOT EXISTS for a view that exists does not look at the query
        let exists = matches!(schema.get_entry(&info.name.name), Some(TableOrView::View(_)));
        let skip_binding = *defer_binding || (on_conflict == OnCreateConflict::Ignore && exists);
        if !skip_binding {
            let mut binder = Binder::with_options(&catalog, self.bind_options());
            binder.bind_create_view(query, aliases, &schema.name)?;
        }
        schema.create_view(&info.name.name, (**query).clone(), aliases.clone(), on_conflict)
    }

    /// `CREATE MACRO` (`Binder::BindCreateFunctionInfo` + `DuckSchemaEntry::CreateFunction`).
    fn create_macro(&mut self, info: &CreateInfo) -> Result<()> {
        let CreateKind::Macro { table_macro, macros } = &info.kind else {
            unreachable!("checked by the caller");
        };
        if self.setting_text("enable_macro_dependencies") == "true" {
            return Err(crate::unported("macro dependencies"));
        }
        let catalog = self.catalog();
        let schema = catalog.schema_for(create_schema_of(info)?)?;
        let mut binder = Binder::with_options(&catalog, self.bind_options());
        binder.bind_create_macro(&info.name.name, macros, *table_macro, &schema.name)?;
        if on_create_conflict(info.on_conflict) == OnCreateConflict::Replace {
            macro_is_free(&schema, &info.name.name)?;
        }
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
                let catalog = self.catalog();
                let binder = Binder::with_options(&catalog, self.bind_options());
                binder.bind_type(target)?
            }
        };
        let schema = self.catalog().schema_for(create_schema_of(info)?)?;
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
        let data = SequenceData {
            usage_count: 0,
            counter: start_value,
            last_value: None,
            increment,
            min_value,
            max_value,
            cycle,
        };
        let schema = self.catalog().schema_for(create_schema_of(info)?)?;
        schema.create_sequence(&info.name.name, data, on_create_conflict(info.on_conflict))
    }

    /// `DROP TABLE`, `VIEW`, `SEQUENCE`, `MACRO` and `SCHEMA` (`Catalog::DropEntry`).
    fn drop_entry(&mut self, info: &DropInfo) -> Result<()> {
        let catalog = self.catalog();
        let temporary = catalog.schema_for(Some(TEMP_SCHEMA))?;
        let in_temp = info.name.qualification.is_empty()
            && match info.entry_type {
                "TABLE" | "VIEW" => temporary.get_entry(&info.name.name).is_some(),
                "SEQUENCE" => temporary.get_sequence(&info.name.name).is_some(),
                "MACRO" | "MACRO TABLE" => {
                    temporary.get_macro(&info.name.name, false).is_some()
                        || temporary.get_macro(&info.name.name, true).is_some()
                }
                "TYPE" => temporary.get_type(&info.name.name).is_some(),
                _ => false,
            };
        let qualified;
        let info = if in_temp {
            qualified = DropInfo {
                name: QualifiedName {
                    qualification: vec![TEMP_SCHEMA.to_string()],
                    ..info.name.clone()
                },
                ..info.clone()
            };
            &qualified
        } else {
            info
        };
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
                        macro_is_free(&schema, &info.name.name)?;
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
    fn a_subquery_in_create_table_as_keeps_its_own_table_index() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE c AS SELECT TIMESTAMP '2023-01-01' s, 1::BIGINT y");
        run(
            &mut c,
            "CREATE TABLE d AS (SELECT * FROM c WHERE y IN (SELECT y FROM c))",
        );
        assert_eq!(run(&mut c, "SELECT count(*) FROM d"), vec![vec!["1".to_string()]]);
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
    fn list_operators_and_temporal_averages() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut c,
                "SELECT [1, 2] || [3], [1] || NULL, list_append([1], 2), list_prepend(0, [1])"
            ),
            [["[1, 2, 3]", "NULL", "[1, 2]", "[0, 1]"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT avg(i) FROM (VALUES (INTERVAL 1 DAY), (INTERVAL 0 DAY)) t(i)"
            ),
            [["12:00:00"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT avg(t) FROM (VALUES (TIMESTAMP '2000-01-01'), (TIMESTAMP '2000-01-03')) v(t)"
            ),
            [["2000-01-02 00:00:00"]]
        );
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
        // a transaction sees what was there when it started, and others don't see what it did
        let mut other = ClientContext::new(db.clone());
        run(&mut c, "BEGIN");
        assert_eq!(run(&mut other, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        run(&mut c, "INSERT INTO t VALUES (10)");
        assert_eq!(run(&mut other, "SELECT count(*) FROM t"), rows(&[&["3"]]));
        run(&mut other, "INSERT INTO t VALUES (9)");
        assert_eq!(run(&mut c, "SELECT count(*) FROM t"), rows(&[&["4"]]));
        run(&mut c, "COMMIT");
        assert_eq!(run(&mut other, "SELECT count(*) FROM t"), rows(&[&["5"]]));
        run(&mut other, "DELETE FROM t WHERE i = 9");
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
    fn grouping_sets() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (a INTEGER, b VARCHAR, v INTEGER)");
        run(
            &mut context,
            "INSERT INTO t VALUES (1, 'x', 10), (1, 'y', 20), (2, 'x', 5), (2, 'x', 5)",
        );
        let rows = |context: &mut ClientContext, sql: &str| {
            run(context, sql).into_iter().map(|r| r.join(",")).collect::<Vec<_>>()
        };
        // groups outside of a set are NULL, and aggregates still see their values
        assert_eq!(
            rows(
                &mut context,
                "SELECT a, b, sum(a), count(*) FROM t GROUP BY GROUPING SETS ((a), (b), ()) \
                 ORDER BY a NULLS LAST, b NULLS LAST"
            ),
            ["1,NULL,2,2", "2,NULL,4,2", "NULL,x,5,3", "NULL,y,1,1", "NULL,NULL,6,4"]
        );
        assert_eq!(
            rows(
                &mut context,
                "SELECT a, b, sum(v) FROM t GROUP BY ROLLUP (a, b) ORDER BY a NULLS LAST, b NULLS LAST"
            ),
            ["1,x,10", "1,y,20", "1,NULL,30", "2,x,10", "2,NULL,10", "NULL,NULL,40"]
        );
        assert_eq!(
            run(&mut context, "SELECT a, b, count(*) FROM t GROUP BY CUBE (a, b)").len(),
            8
        );
        // GROUPING has a bit per argument, the first one highest, set outside of the grouping set
        assert_eq!(
            rows(
                &mut context,
                "SELECT GROUPING(a), GROUPING(b), GROUPING(a, b), GROUPING_ID(b, a), GROUPING(), count(*) \
                 FROM t GROUP BY CUBE (a, b) HAVING a IS NULL OR b IS NULL"
            )
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            ["0,1,1,2,1,2", "1,0,2,1,2,1", "1,0,2,1,2,3", "1,1,3,3,3,4"]
        );
        // DISTINCT, FILTER and ORDER BY aggregate per set
        assert_eq!(
            rows(
                &mut context,
                "SELECT a, count(DISTINCT v), count(*) FILTER (v > 5), first(v ORDER BY v DESC) \
                 FROM t GROUP BY ROLLUP (a) ORDER BY a NULLS LAST"
            ),
            ["1,2,2,20", "2,1,0,5", "NULL,3,2,20"]
        );
        // equal sets each give their rows
        assert_eq!(
            rows(&mut context, "SELECT count(*) FROM t GROUP BY GROUPING SETS ((), ())"),
            ["4", "4"]
        );
        // the set without groups has a row for no input, the others have none
        assert_eq!(
            rows(
                &mut context,
                "SELECT a, count(*) FROM t WHERE a > 5 GROUP BY ROLLUP (a)"
            ),
            ["NULL,0"]
        );
        assert!(run(&mut context, "SELECT a, count(*) FROM t WHERE a > 5 GROUP BY a").is_empty());
        // a grouping set that is all the groups is a plain GROUP BY
        assert_eq!(
            rows(
                &mut context,
                "SELECT a, GROUPING(a) FROM t GROUP BY GROUPING SETS ((a)) ORDER BY a"
            ),
            ["1,0", "2,0"]
        );
    }

    #[test]
    fn grouping_function_errors() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (a INTEGER, b INTEGER)");
        let err = |context: &mut ClientContext, sql: &str| context.query(sql).unwrap_err().to_string();
        assert_eq!(
            err(&mut context, "SELECT GROUPING(a) FROM t"),
            "Binder Error: GROUPING statement cannot be used without groups"
        );
        assert!(err(&mut context, "SELECT GROUPING(b) FROM t GROUP BY a").contains("must be a grouping column"));
        assert!(
            err(&mut context, "SELECT a FROM t WHERE GROUPING(a) = 0 GROUP BY a")
                .contains("GROUPING function is not supported here")
        );
        assert!(
            err(&mut context, "SELECT sum(GROUPING(a)) FROM t GROUP BY a")
                .contains("GROUPING function is not supported here")
        );
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
    fn explain_matches_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE a AS SELECT i AS x FROM range(1000) t(i)");
        run(&mut context, "CREATE TABLE b AS SELECT i AS y FROM range(1000) t(i)");
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let query = "SELECT x FROM a, b WHERE x = y ORDER BY x LIMIT 10";
        // operators are in title case, with their details and estimates
        let plan = explain(&mut context, query);
        for expected in [
            "Hash Join",
            "Join Type: INNER",
            "Conditions: x = y",
            "Order By: x ASC",
            "Top N",
            "Top: 10",
            "Seq Scan",
            "Table: a",
            "Type: Sequential Scan",
            "~1,000 rows",
        ] {
            assert!(plan.contains(expected), "{expected} in {plan}");
        }
        assert!(!plan.contains("HASH_JOIN"), "{plan}");
        // one row is no plural
        assert!(explain(&mut context, "SELECT count(*) FROM a").contains("~1 row\n"));
        // the setting asks for the names as they are
        run(
            &mut context,
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
        );
        let plan = explain(&mut context, query);
        assert!(plan.contains("HASH_JOIN") && plan.contains("SEQ_SCAN"), "{plan}");
        // JSON has the names as they are, in any case, and a list for what has several lines
        run(&mut context, "RESET profiling_renderer_settings");
        let plan = explain(&mut context, &format!("(FORMAT JSON) {query}"));
        for expected in [
            "\"name\": \"HASH_JOIN\"",
            "\"Join Type\": \"INNER\"",
            "\"Estimated Cardinality\": \"1000\"",
            "\"children\": []",
        ] {
            assert!(plan.contains(expected), "{expected} in {plan}");
        }
        assert!(plan.starts_with('[') && plan.ends_with(']'), "{plan}");
        let plan = explain(&mut context, "(FORMAT JSON) SELECT x, x + 1 FROM a");
        assert!(plan.contains("\"Projections\": [\"x\", \"(x + 1)\"]"), "{plan}");
        // the estimates in the logical plan
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        let plan = explain(&mut context, query);
        assert!(
            plan.contains("Comparison Join") && plan.contains("~1,000 rows"),
            "{plan}"
        );
        // what fails in a statement is not the renderer's: DML runs through it as well
        run(&mut context, "EXPLAIN DELETE FROM a WHERE x = 1");
        assert_eq!(run(&mut context, "SELECT count(*) FROM a"), [["1000"]]);
    }

    #[test]
    fn scan_filters_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE t (i INTEGER, j INTEGER, s VARCHAR)");
        run(
            &mut context,
            "INSERT INTO t SELECT r, r % 10, r::VARCHAR FROM range(5000) x(r)",
        );
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        // one column goes into the scan, and no operator is left
        let plan = explain(&mut context, "SELECT i FROM t WHERE j = 5");
        assert!(plan.contains("Filters: j = 5") && !plan.contains("Filter\n"), "{plan}");
        // several columns stay above it, and each conjunct goes its own way
        let plan = explain(&mut context, "SELECT i FROM t WHERE j = 5 AND i > j AND s <> '55'");
        assert!(
            plan.contains("Filters: j = 5\n") && plan.contains("s != '55'"),
            "{plan}"
        );
        assert!(plan.contains("Filter\n") && plan.contains("i > j"), "{plan}");
        // and an OR on one column is one filter, without parentheses around all of it
        let plan = explain(&mut context, "SELECT i FROM t WHERE j = 5 OR j = 6");
        assert!(!plan.contains("Filter\n"), "{plan}");
        // the answers stay, across the chunks a filter empties
        for (sql, count) in [
            ("SELECT count(*) FROM t WHERE j = 5", "500"),
            ("SELECT count(*) FROM t WHERE i = 4999", "1"),
            ("SELECT count(*) FROM t WHERE i >= 2048 AND i < 2050", "2"),
            ("SELECT count(*) FROM t WHERE i < 0", "0"),
            ("SELECT count(*) FROM t WHERE j = 5 AND i > j", "499"),
            ("SELECT count(*) FROM t WHERE j IS NULL", "0"),
        ] {
            assert_eq!(run(&mut context, sql), [[count]], "{sql}");
        }
        // what the filter does not output still counts, and rowids are those of the table
        assert_eq!(run(&mut context, "SELECT rowid FROM t WHERE i = 3000"), [["3000"]]);
        run(&mut context, "UPDATE t SET j = 100 WHERE i = 4000");
        assert_eq!(run(&mut context, "SELECT i FROM t WHERE j = 100"), [["4000"]]);
        run(&mut context, "DELETE FROM t WHERE i >= 10");
        assert_eq!(run(&mut context, "SELECT count(*) FROM t"), [["10"]]);
        // what can fail, or differ per call, or reads the query around it, stays an operator
        let plan = explain(&mut context, "SELECT i FROM t WHERE s::INTEGER = 3");
        assert!(plan.contains("Filter\n"), "{plan}");
        let plan = explain(&mut context, "SELECT i FROM t WHERE i < random() * 100");
        assert!(plan.contains("Filter\n"), "{plan}");
        let rows = run(
            &mut context,
            "SELECT i FROM t t1 WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.i = t1.i AND t2.j > 8) ORDER BY i",
        );
        assert_eq!(rows, [["9"]]);
        // a filter that keeps all rows leaves the estimate alone
        run(&mut context, "SET disabled_optimizers = 'statistics_propagation'");
        let plan = explain(&mut context, "SELECT i FROM t WHERE i IS NOT NULL");
        assert!(plan.contains("~10 rows"), "{plan}");
    }

    #[test]
    fn date_part_and_equal_or_null_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE d (t TIMESTAMP)");
        run(&mut context, "INSERT INTO d VALUES ('2024-03-05 06:07:08.5'), (NULL)");
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        // the plan is that of the function, and the value that of date_part
        let plan = explain(&mut context, "SELECT EXTRACT(year FROM t) FROM d");
        assert!(plan.contains("year(t)") && !plan.contains("date_part"), "{plan}");
        for (part, value) in [
            ("year", "2024"),
            ("yr", "2024"),
            ("month", "3"),
            ("day", "5"),
            ("decade", "202"),
            ("century", "21"),
            ("millennium", "3"),
            ("quarter", "1"),
            ("week", "10"),
            ("dow", "2"),
            ("isodow", "2"),
            ("doy", "65"),
            ("hour", "6"),
            ("minute", "7"),
            ("second", "8"),
            ("millisecond", "8500"),
            ("microsecond", "8500000"),
        ] {
            let sql = format!("SELECT date_part('{part}', t) FROM d WHERE t IS NOT NULL");
            assert_eq!(run(&mut context, &sql), [[value]], "{sql}");
        }
        // a type the function has no overload for keeps date_part, as a cast would round it
        let sql = "SELECT date_part('microsecond', TIMESTAMP_NS '2020-01-01 01:23:45.123456789')";
        assert_eq!(run(&mut context, sql), [["45123456"]]);
        run(&mut context, "CREATE TABLE ns (t TIMESTAMP_NS)");
        run(&mut context, "INSERT INTO ns VALUES ('2020-01-01 01:23:45.123456789')");
        let sql = "SELECT EXTRACT(microseconds FROM t) FROM ns";
        assert!(explain(&mut context, sql).contains("date_part"), "{sql}");
        assert_eq!(run(&mut context, sql), [["45123456"]]);
        // NULL stays, and what has no function of its own stays date_part
        assert_eq!(run(&mut context, "SELECT year(t) FROM d WHERE t IS NULL"), [["NULL"]]);
        let plan = explain(&mut context, "SELECT date_part('epoch', t) FROM d");
        assert!(plan.contains("date_part"), "{plan}");
        // a specifier nobody knows fails as it did
        let error = context.query("SELECT date_part('nonsense', t) FROM d").unwrap_err();
        assert!(error.to_string().contains("not recognized"), "{error}");
        run(&mut context, "CREATE TABLE n (a INTEGER, b INTEGER)");
        run(
            &mut context,
            "INSERT INTO n VALUES (1, 1), (1, 2), (0, NULL), (NULL, NULL)",
        );
        // both orders, and on the conjunct of a filter too
        for sql in [
            "SELECT count(*) FROM n WHERE a = b OR (a IS NULL AND b IS NULL)",
            "SELECT count(*) FROM n WHERE (b IS NULL AND a IS NULL) OR a = b",
        ] {
            assert!(explain(&mut context, sql).contains("IS NOT DISTINCT FROM"), "{sql}");
            assert_eq!(run(&mut context, sql), [["2"]], "{sql}");
        }
        let sql = "SELECT count(*) FROM n WHERE (a = b OR (a IS NULL AND b IS NULL)) AND a IS NULL";
        assert!(explain(&mut context, sql).contains("IS NOT DISTINCT FROM"), "{sql}");
        assert_eq!(run(&mut context, sql), [["1"]]);
        // as a value the two differ, for a value and a NULL
        let sql = "SELECT a = b OR (a IS NULL AND b IS NULL) FROM n";
        assert!(!explain(&mut context, sql).contains("DISTINCT"), "{sql}");
        assert_eq!(run(&mut context, sql), [["true"], ["false"], ["NULL"], ["true"]]);
        // other columns under IS NULL are something else
        let sql = "SELECT count(*) FROM n WHERE a = b OR (a IS NULL AND a IS NULL)";
        assert!(!explain(&mut context, sql).contains("DISTINCT"), "{sql}");
        assert_eq!(run(&mut context, sql), [["2"]]);
        // and between tables it makes a hash join with NULLs as equal
        let sql = "SELECT count(*) FROM n x, n y WHERE x.b = y.b OR (x.b IS NULL AND y.b IS NULL)";
        let plan = explain(&mut context, sql);
        assert!(
            plan.contains("IS NOT DISTINCT FROM") && plan.contains("Comparison Join"),
            "{plan}"
        );
        assert_eq!(run(&mut context, sql), [["6"]]);
    }

    #[test]
    fn zone_maps_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        // five row groups, with values that rise with the row
        run(
            &mut context,
            "CREATE TABLE t AS SELECT i AS x, CASE WHEN i < 122880 THEN NULL ELSE i END AS n, \
             'k' || lpad(i::VARCHAR, 7, '0') AS s, DATE '2000-01-01' + (i // 1000)::INTEGER AS d \
             FROM range(500000) r(i)",
        );
        let scanned = |context: &mut ClientContext, filter: &str| {
            let sql = format!("EXPLAIN ANALYZE SELECT sum(x) FROM t WHERE {filter}");
            let plan = run(context, &sql)[0][1].clone();
            let line = plan.lines().find(|line| line.contains("Row Groups Scanned"));
            let line = line.unwrap_or_else(|| panic!("{sql}: {plan}"));
            let count = |c: &mut ClientContext| run(c, &format!("SELECT count(*) FROM t WHERE {filter}"))[0][0].clone();
            (line.trim().replace("Row Groups Scanned: ", ""), count(context))
        };
        // the rows are counted by hand; the groups by which of 122880 they fall in
        for (filter, groups, rows) in [
            ("x < 100", "1 / 5", "100"),
            ("x >= 491520", "1 / 5", "8480"),
            ("x = 250000", "1 / 5", "1"),
            ("x BETWEEN 122880 AND 245759", "1 / 5", "122880"),
            ("x BETWEEN 122879 AND 122880", "2 / 5", "2"),
            ("x = 10 OR x = 400000", "2 / 5", "2"),
            ("x IN (10, 11, 400000)", "2 / 5", "3"),
            ("x > 100 AND x < 200", "1 / 5", "99"),
            ("x <> 5", "5 / 5", "499999"),
            ("x % 7 = 5 OR x = 6", "5 / 5", "71429"),
            // strings, dates, NULLs and functions that keep the order
            ("s = 'k0250000'", "1 / 5", "1"),
            ("s < 'k0000100'", "1 / 5", "100"),
            ("d = DATE '2000-01-02'", "1 / 5", "1000"),
            ("n IS NULL", "1 / 5", "122880"),
            ("n IS NOT NULL", "4 / 5", "377120"),
            ("x::BIGINT > 499990", "1 / 5", "9"),
            ("floor(x::DOUBLE) > 495000", "1 / 5", "4999"),
            ("floor(x::DOUBLE) >= 245760", "3 / 5", "254240"),
            ("year(d) > 2000", "3 / 5", "134000"),
            ("year(d) = 2000", "3 / 5", "366000"),
            ("rowid > 491520", "1 / 5", "8479"),
        ] {
            assert_eq!(
                scanned(&mut context, filter),
                (groups.to_string(), rows.to_string()),
                "{filter}"
            );
        }
        // a change shows at once, in the group it lands in and in the ones it leaves
        run(&mut context, "UPDATE t SET x = -5 WHERE x = 300000");
        assert_eq!(scanned(&mut context, "x < 0"), ("1 / 5".to_string(), "1".to_string()));
        assert_eq!(
            scanned(&mut context, "x = 300000"),
            ("1 / 5".to_string(), "0".to_string())
        );
        run(&mut context, "INSERT INTO t VALUES (9999999, NULL, 'z', NULL)");
        assert_eq!(
            scanned(&mut context, "x = 9999999"),
            ("1 / 5".to_string(), "1".to_string())
        );
        run(&mut context, "BEGIN");
        run(&mut context, "UPDATE t SET x = 7777777 WHERE x = 5");
        // the last group goes up to 9999999 now, so it may hold this value too
        assert_eq!(
            scanned(&mut context, "x = 7777777"),
            ("2 / 5".to_string(), "1".to_string())
        );
        run(&mut context, "ROLLBACK");
        assert_eq!(scanned(&mut context, "x = 7777777").1, "0");
        assert_eq!(scanned(&mut context, "x = 5").1, "1");
        run(&mut context, "DELETE FROM t WHERE x < 491520");
        assert_eq!(
            run(&mut context, "SELECT count(*) FROM t WHERE x >= 491520"),
            [["8481"]]
        );
    }

    #[test]
    fn running_min_and_max_take_time_of_the_order_of_their_rows() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        // encoding all of the input for each frame would never get through this many rows
        let rows = run(
            &mut context,
            "SELECT count(*), min(lo), max(hi), count(s) FROM (
                SELECT min(n) OVER w AS lo, max(n) OVER w AS hi, max(s) OVER w AS s FROM (
                    SELECT i, CASE WHEN i % 5 = 0 THEN NULL ELSE i END AS n,
                           CASE WHEN i < 3 THEN NULL ELSE i::VARCHAR END AS s
                    FROM range(200000) t(i)
                ) WINDOW w AS (ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW))",
        );
        assert_eq!(rows, [["200000", "1", "199999", "199997"]]);
        // nor could each of many partitions encode all of the input
        let rows = run(
            &mut context,
            "SELECT count(*), sum(hi), count(DISTINCT n) FROM (
                SELECT max(i) OVER w AS hi, count(DISTINCT i % 3) OVER w AS n FROM range(200000) t(i)
                WINDOW w AS (PARTITION BY i // 2 ORDER BY i ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW))",
        );
        assert_eq!(rows, [["200000", "19999900000", "2"]]);
        // a frame that moves still gets the extreme of its own rows, NULLs left out
        let rows = run(
            &mut context,
            "SELECT i, min(n) OVER w, max(n) OVER w, max(n) FILTER (WHERE n < 3) OVER w
             FROM (VALUES (0, 5), (1, NULL), (2, 1), (3, 9), (4, NULL)) t(i, n)
             WINDOW w AS (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) ORDER BY i",
        );
        assert_eq!(
            rows,
            [
                ["0", "5", "5", "NULL"],
                ["1", "5", "5", "NULL"],
                ["2", "1", "1", "1"],
                ["3", "1", "9", "1"],
                ["4", "9", "9", "NULL"],
            ]
        );
    }

    #[test]
    fn top_n_gives_the_rows_of_a_sort() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(
            &mut context,
            "CREATE TABLE t AS SELECT i, i % 7 AS g, CASE WHEN i % 5 = 0 THEN NULL ELSE i END AS n FROM range(200000) t(i)",
        );
        let plan = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let sorted = |context: &mut ClientContext, order: &str, limit: usize, offset: usize| {
            let rows = run(context, &format!("SELECT i FROM t ORDER BY {order}"));
            rows[offset.min(rows.len())..(offset + limit).min(rows.len())].to_vec()
        };
        // what is limited comes out as from a sort, ties in input order included
        for (order, limit, offset) in [
            ("g, i DESC", 5, 0),
            ("g DESC", 5, 3),
            ("n DESC NULLS FIRST, i", 4, 2),
            ("n NULLS LAST, i", 3, 39990),
            ("g", 3, 199998),
            ("g", 3, 300000),
        ] {
            let sql = format!("SELECT i FROM t ORDER BY {order} LIMIT {limit} OFFSET {offset}");
            assert_eq!(plan(&mut context, &sql).contains("Top N"), offset < 5000, "{sql}");
            assert_eq!(
                run(&mut context, &sql),
                sorted(&mut context, order, limit, offset),
                "{sql}"
            );
        }
        // more rows than it buffers at once, with the first ones arriving last
        let sql = "SELECT i FROM t ORDER BY i DESC LIMIT 2 OFFSET 1";
        assert_eq!(run(&mut context, sql), [["199998"], ["199997"]]);
        // it goes through projections, also of a subquery
        let sql = "SELECT i + 1 FROM (SELECT i, g FROM t ORDER BY i) LIMIT 2";
        assert!(plan(&mut context, sql).contains("Top N"));
        assert_eq!(run(&mut context, sql), [["1"], ["2"]]);
        // these are no Top-N: without a limit, without an order, and a limit too large to gain
        let sql = "SELECT i FROM t ORDER BY i OFFSET 3";
        assert!(!plan(&mut context, sql).contains("Top N"));
        assert!(!plan(&mut context, "SELECT i FROM t LIMIT 3").contains("Top N"));
        assert!(!plan(&mut context, "SELECT i FROM t ORDER BY i LIMIT 100000").contains("Top N"));
        assert_eq!(
            run(&mut context, "SELECT i FROM t ORDER BY i LIMIT 0"),
            Vec::<Vec<String>>::new()
        );
        // a small limit goes below a projection, unless an offset would skip what is volatile
        let below = plan(&mut context, "SELECT i + 1 FROM t LIMIT 2");
        assert!(below.find("Projection") < below.find("Limit"), "{below}");
        let above = plan(&mut context, "SELECT random() FROM t LIMIT 2 OFFSET 1");
        assert!(above.find("Limit") < above.find("Projection"), "{above}");
        assert_eq!(
            run(&mut context, "SELECT i + 1 FROM t LIMIT 2 OFFSET 1"),
            [["2"], ["3"]]
        );
        // count of a column without NULLs reads no column, and with NULLs it still counts them
        run(
            &mut context,
            "CREATE TABLE c (a INTEGER NOT NULL, b INTEGER, s VARCHAR)",
        );
        run(
            &mut context,
            "INSERT INTO c VALUES (1, NULL, 'x'), (2, 2, 'x'), (3, 3, 'y')",
        );
        let sql = "SELECT s, count(a), count(b) FROM c GROUP BY s ORDER BY s";
        assert_eq!(run(&mut context, sql), [["x", "2", "1"], ["y", "1", "1"]]);
        assert!(!plan(&mut context, "SELECT count(a) FROM c").contains("count(a)"));
        let sql = "SELECT count(DISTINCT a), count(a) FILTER (WHERE a > 1) FROM c";
        assert_eq!(run(&mut context, sql), [["3", "2"]]);
        assert_eq!(run(&mut context, "SELECT count(b) FROM c WHERE b > 0"), [["2"]]);
        let sql = "SELECT count(c2.a) FROM c c1 LEFT JOIN c c2 ON c1.a = c2.b";
        assert_eq!(run(&mut context, sql), [["2"]]);
    }

    #[test]
    fn analyze_checks_its_table() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "CREATE TABLE t (i INTEGER)");
        run(&mut context, "CREATE VIEW v AS SELECT * FROM t");
        for sql in ["ANALYZE", "VACUUM", "VACUUM ANALYZE", "ANALYZE t", "VACUUM t"] {
            run(&mut context, sql);
        }
        assert!(context.query("ANALYZE missing").is_err());
        let error = context.query("ANALYZE v").unwrap_err().to_string();
        assert!(error.contains("Can only vacuum or analyze base tables"), "{error}");
    }

    #[test]
    fn scans_output_only_what_is_read() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE a (k INTEGER, x INTEGER, unused VARCHAR)");
        run(
            &mut context,
            "INSERT INTO a VALUES (1, 10, 'u'), (2, 20, 'v'), (3, 30, 'w')",
        );
        run(&mut context, "CREATE TABLE b (k INTEGER, y INTEGER, spare VARCHAR)");
        run(
            &mut context,
            "INSERT INTO b VALUES (1, 5, 's'), (1, 6, 't'), (2, 7, 'r')",
        );
        let plan = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();

        let sql = "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k ORDER BY a.x, b.y";
        let joined = plan(&mut context, sql);
        assert!(!joined.contains("unused") && !joined.contains("spare"), "{joined}");
        assert_eq!(run(&mut context, sql), [["10", "5"], ["10", "6"], ["20", "7"]]);
        // a column that only a filter of the scan reads stays in the scan
        let sql = "SELECT a.x FROM a JOIN b ON a.k = b.k WHERE b.spare = 'r' AND a.unused = 'v'";
        let filtered = plan(&mut context, sql);
        assert!(!filtered.contains("Projections: spare"), "{filtered}");
        assert!(filtered.contains("Filters: spare = 'r'"), "{filtered}");
        assert_eq!(run(&mut context, sql), [["20"]]);
        // no column is read at all, and the rows still count
        assert_eq!(run(&mut context, "SELECT count(*) FROM a"), [["3"]]);
        assert_eq!(run(&mut context, "SELECT 1 FROM a WHERE x > 15"), [["1"], ["1"]]);
        assert_eq!(run(&mut context, "SELECT count(*) FROM a, b"), [["9"]]);
        assert_eq!(run(&mut context, "SELECT count(*) FROM a WHERE x > 15"), [["2"]]);
        // read above in other ways than a plain column: by position, for order, in other operators
        assert_eq!(run(&mut context, "SELECT * FROM a WHERE k = 2"), [["2", "20", "v"]]);
        assert_eq!(
            run(&mut context, "SELECT x FROM a ORDER BY unused DESC LIMIT 1"),
            [["30"]]
        );
        assert_eq!(run(&mut context, "SELECT DISTINCT k FROM b ORDER BY k"), [["1"], ["2"]]);
        let sql = "SELECT sum(x) OVER (ORDER BY k) FROM a ORDER BY 1";
        assert_eq!(run(&mut context, sql), [["10"], ["30"], ["60"]]);
        assert_eq!(run(&mut context, "SELECT rowid FROM a WHERE x = 20"), [["1"]]);
        let sql = "SELECT k FROM a UNION ALL SELECT y FROM b ORDER BY 1";
        assert_eq!(run(&mut context, sql), [["1"], ["2"], ["3"], ["5"], ["6"], ["7"]]);
        let sql = "WITH c AS MATERIALIZED (SELECT x, unused FROM a) SELECT x FROM c WHERE unused = 'w'";
        assert_eq!(run(&mut context, sql), [["30"]]);
        // read by a subquery of the query around them
        let sql = "SELECT x FROM a WHERE (SELECT sum(y) FROM b WHERE b.k = a.k) > 10 ORDER BY x";
        assert_eq!(run(&mut context, sql), [["10"]]);
        let sql = "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k) ORDER BY x";
        assert_eq!(run(&mut context, sql), [["10"], ["20"]]);
        let sql = "SELECT x FROM a WHERE x > 15 AND NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)";
        assert_eq!(run(&mut context, sql), [["30"]]);
        let sql = "SELECT x, (SELECT max(y) FROM b WHERE b.k = a.k AND random() >= 0) FROM a WHERE x = 20";
        assert_eq!(run(&mut context, sql), [["20", "7"]]);
        let sql = "SELECT x, y FROM a, LATERAL (SELECT y FROM b WHERE b.k = a.k) ORDER BY y";
        assert_eq!(run(&mut context, sql), [["10", "5"], ["10", "6"], ["20", "7"]]);
        // writes read their rows by position
        run(
            &mut context,
            "CREATE TABLE c AS SELECT a.x FROM a JOIN b ON a.k = b.k WHERE b.y = 7",
        );
        assert_eq!(run(&mut context, "SELECT * FROM c"), [["20"]]);
        run(&mut context, "INSERT INTO c SELECT y FROM b WHERE k = 2");
        run(&mut context, "UPDATE a SET x = 11 WHERE unused = 'u'");
        run(&mut context, "DELETE FROM a WHERE unused = 'v'");
        assert_eq!(
            run(&mut context, "SELECT k, x FROM a ORDER BY k"),
            [["1", "11"], ["3", "30"]]
        );
        assert_eq!(run(&mut context, "SELECT count(*) FROM c"), [["2"]]);
        // columns that a join or a filter read for themselves go before what copies rows next
        run(&mut context, "CREATE TABLE d (y INTEGER, z INTEGER)");
        run(&mut context, "INSERT INTO d VALUES (5, 50), (7, 70), (8, 80)");
        let sql = "SELECT a.x, d.z FROM a JOIN b ON a.k = b.k JOIN d ON b.y = d.y ORDER BY d.z";
        assert_eq!(run(&mut context, sql), [["11", "50"]]);
        let sql = "SELECT b.k FROM b JOIN d ON b.y = d.y WHERE b.y + d.z > 60 AND b.k < d.z ORDER BY 1";
        assert_eq!(run(&mut context, sql), [["2"]]);
        let sql = "SELECT b.k FROM b WHERE b.y + 3 > b.k * 7 AND EXISTS (SELECT * FROM d WHERE d.y = b.y) ORDER BY 1";
        assert_eq!(run(&mut context, sql), [["1"]]);
        let sql = "SELECT k FROM b WHERE y <> k ORDER BY spare";
        assert_eq!(run(&mut context, sql), [["2"], ["1"], ["1"]]);
        let sql = "SELECT b.k, (SELECT max(z) FROM d WHERE d.y = b.y) FROM b WHERE y > k ORDER BY b.y";
        assert_eq!(run(&mut context, sql), [["1", "50"], ["1", "NULL"], ["2", "70"]]);
        let sql = "SELECT count(*) FROM (SELECT y + 1 AS v, z FROM d) WHERE z > 60";
        assert_eq!(run(&mut context, sql), [["2"]]);
        let sql = "SELECT count(*) FROM (SELECT 1 / 0 AS v, z FROM d)";
        assert_eq!(run(&mut context, sql), [["3"]]);
        // the operator that does this stays out of plans, and it shows in the result only
        let sql = "EXPLAIN ANALYZE SELECT a.x FROM a JOIN b ON a.k = b.k JOIN d ON b.y = d.y";
        let analyzed = run(&mut context, sql)[0][1].clone();
        assert!(
            !analyzed.contains("COLUMN_SELECT") && !analyzed.contains("Column Select"),
            "{analyzed}"
        );
        // off, a scan outputs everything
        run(&mut context, "SET disabled_optimizers = 'unused_columns'");
        let sql = "SELECT a.x FROM a JOIN b ON a.k = b.k";
        assert!(plan(&mut context, sql).contains("unused"));
    }

    #[test]
    fn left_join_on_conditions_of_the_right_side_go_below_it() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE a (k INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1), (2), (3), (NULL)");
        run(&mut context, "CREATE TABLE b (k INTEGER, s VARCHAR)");
        run(
            &mut context,
            "INSERT INTO b VALUES (1, 'keep'), (1, 'drop'), (2, 'drop'), (NULL, 'keep'), (9, NULL)",
        );
        let plan = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        // the left rows stay, with NULLs where the condition left no partner
        let sql = "SELECT a.k, b.s FROM a LEFT JOIN b ON a.k = b.k AND b.s LIKE 'k%' ORDER BY 1 NULLS LAST, 2";
        let below = plan(&mut context, sql);
        assert!(below.contains("Filters: "), "{below}");
        assert!(!below.contains("Join Type: INNER"), "{below}");
        let rows = [["1", "keep"], ["2", "NULL"], ["3", "NULL"], ["NULL", "NULL"]];
        assert_eq!(run(&mut context, sql), rows);
        // both sides or the left side alone stay on the join, next to one that moves
        let sql =
            "SELECT a.k, b.s FROM a LEFT JOIN b ON a.k = b.k AND b.s LIKE 'k%' AND a.k <> 1 ORDER BY 1 NULLS LAST";
        let rows = [["1", "NULL"], ["2", "NULL"], ["3", "NULL"], ["NULL", "NULL"]];
        assert_eq!(run(&mut context, sql), rows);
        let sql = "SELECT a.k, b.s FROM a LEFT JOIN b ON a.k = b.k AND a.k + length(b.s) = 5 ORDER BY 1 NULLS LAST, 2";
        let rows = [
            ["1", "drop"],
            ["1", "keep"],
            ["2", "NULL"],
            ["3", "NULL"],
            ["NULL", "NULL"],
        ];
        assert_eq!(run(&mut context, sql), rows);
        // NULL is not true either
        let sql = "SELECT count(b.k) FROM a LEFT JOIN b ON a.k = b.k AND b.s NOT LIKE 'k%'";
        assert_eq!(run(&mut context, sql), [["2"]]);
        // what can fail stays on the join, where only rows with a partner reach it
        let sql = "SELECT count(*) FROM (SELECT * FROM a WHERE k = 3) a LEFT JOIN b ON a.k = b.k AND b.s::INTEGER = 1";
        assert_eq!(run(&mut context, sql), [["1"]]);
        let sql = "SELECT count(*) FROM a LEFT JOIN b ON a.k = b.k AND b.s::INTEGER = 1";
        assert!(context.query(sql).is_err(), "a partner reaches the cast");
    }

    #[test]
    fn deliminator_matches_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE a (k INTEGER, v INTEGER)");
        run(
            &mut context,
            "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30), (NULL, 40)",
        );
        run(&mut context, "CREATE TABLE b (k INTEGER, w INTEGER)");
        run(
            &mut context,
            "INSERT INTO b VALUES (1, 1), (1, 2), (2, 3), (9, 4), (NULL, 5)",
        );
        let plan = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let delim_gets = |plan: &str| plan.lines().filter(|line| line.contains("Delim Get")).count();

        // nothing filters the outer rows, so nothing of the delim join is left, and a key that the
        // outer query does not have (9) goes nowhere
        let sql = "SELECT k, (SELECT sum(w) FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        let unfiltered = plan(&mut context, sql);
        assert!(!unfiltered.contains("Delim"), "{unfiltered}");
        assert!(unfiltered.contains("LEFT"), "{unfiltered}");
        let rows = [["1", "3"], ["2", "3"], ["3", "NULL"], ["NULL", "NULL"]];
        assert_eq!(run(&mut context, sql), rows);
        // a filter keeps the join that is deepest, and the one above the aggregate goes
        let sql = "SELECT k, (SELECT sum(w) FROM b WHERE b.k = a.k) FROM a WHERE v > 15 ORDER BY v";
        let filtered = plan(&mut context, sql);
        assert_eq!(delim_gets(&filtered), 1, "{filtered}");
        assert!(filtered.contains("Join Type: LEFT"), "{filtered}");
        assert!(!filtered.contains("SINGLE"), "{filtered}");
        assert_eq!(run(&mut context, sql), [["2", "3"], ["3", "NULL"], ["NULL", "NULL"]]);
        // what an empty subquery gives has to survive: no NULL here, or NULL turned into something
        let sql = "SELECT k, (SELECT count(*) FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        assert_eq!(
            run(&mut context, sql),
            [["1", "2"], ["2", "1"], ["3", "0"], ["NULL", "0"]]
        );
        let sql = "SELECT k, (SELECT sum(w) IS NULL FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        let rows = [["1", "false"], ["2", "false"], ["3", "true"], ["NULL", "true"]];
        assert_eq!(run(&mut context, sql), rows);
        let sql = "SELECT k, (SELECT coalesce(sum(w), 7) FROM b WHERE b.k = a.k) + 1 FROM a ORDER BY v";
        assert_eq!(
            run(&mut context, sql),
            [["1", "4"], ["2", "4"], ["3", "8"], ["NULL", "8"]]
        );
        // HAVING can see the row of NULLs that only exists without the delim join
        let sql = "SELECT k, (SELECT 5 FROM b WHERE b.k = a.k HAVING sum(w) IS NULL) FROM a ORDER BY v";
        let rows = [["1", "NULL"], ["2", "NULL"], ["3", "5"], ["NULL", "5"]];
        assert_eq!(run(&mut context, sql), rows);
        // grouped by more than the keys there can be two rows, and that remains an error
        let sql = "SELECT (SELECT w FROM b WHERE b.k = a.k GROUP BY w) FROM a";
        assert!(plan(&mut context, sql).contains("SINGLE"));
        assert!(context.query(sql).is_err());
        // the IS NOT NULL that the join stood for: `=` does not find the NULL of `a`
        let sql = "SELECT k, EXISTS (SELECT 1 FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        let exists = plan(&mut context, sql);
        assert!(!exists.contains("Delim"), "{exists}");
        let rows = [["1", "true"], ["2", "true"], ["3", "false"], ["NULL", "false"]];
        assert_eq!(run(&mut context, sql), rows);
        let sql = "SELECT k, NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        let rows = [["1", "false"], ["2", "false"], ["3", "true"], ["NULL", "true"]];
        assert_eq!(run(&mut context, sql), rows);
        // while IS NOT DISTINCT FROM finds it
        let sql = "SELECT k, EXISTS (SELECT 1 FROM b WHERE b.k IS NOT DISTINCT FROM a.k) FROM a ORDER BY v";
        let rows = [["1", "true"], ["2", "true"], ["3", "false"], ["NULL", "true"]];
        assert_eq!(run(&mut context, sql), rows);
        // a marker that stays as a filter and a join that leaves no duplicates of the outer rows
        let sql = "SELECT v FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k) ORDER BY v";
        assert_eq!(run(&mut context, sql), [["10"], ["20"]]);
        // an ANY keeps its NULL: 30 is unknown next to the NULL of key 3, and no row is false
        run(&mut context, "INSERT INTO b VALUES (3, NULL)");
        let sql = "SELECT k, v = ANY (SELECT w FROM b WHERE b.k = a.k) FROM a ORDER BY v";
        let rows = [["1", "false"], ["2", "false"], ["3", "NULL"], ["NULL", "false"]];
        assert_eq!(run(&mut context, sql), rows);
    }

    #[test]
    fn join_dependent_filters_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(
            &mut context,
            "CREATE TABLE a AS SELECT i AS x, i AS y FROM range(10) t(i)",
        );
        run(
            &mut context,
            "CREATE TABLE b AS SELECT i AS x, i::VARCHAR AS s FROM range(10) t(i)",
        );
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let filters = |plan: &str| plan.lines().filter(|line| line.contains("Filters: ")).count();
        // both tables get what they alone can tell, and the OR stays for the pairs
        let sql = "SELECT count(*) FROM a, b WHERE (a.x = 2 AND b.x = 4) OR (a.x = 0 AND b.x = 2)";
        assert_eq!(filters(&explain(&mut context, sql)), 2, "{sql}");
        assert_eq!(run(&mut context, sql), [["2"]]);
        // with the filters on their own this would be four pairs
        let sql = "SELECT a.x, b.x FROM a, b WHERE (a.x = 2 AND b.x = 4) OR (a.x = 0 AND b.x = 2) ORDER BY 1";
        assert_eq!(run(&mut context, sql), [["0", "2"], ["2", "4"]]);
        // a branch that does not read a table says nothing about it
        let sql = "SELECT count(*) FROM a, b WHERE (a.x = 2 AND b.x = 4) OR b.x = 7";
        assert_eq!(filters(&explain(&mut context, sql)), 1, "{sql}");
        assert_eq!(run(&mut context, sql), [["11"]]);
        // several conditions on a table in one branch stay together
        let sql = "SELECT count(*) FROM a, b WHERE (a.x = 2 AND a.y = 2 AND b.x = 4) OR (a.x = 3 AND b.x = 5)";
        assert_eq!(run(&mut context, sql), [["2"]]);
        // as a value it is not a filter, and NULL has to stay
        let sql = "SELECT count(*) FROM (SELECT (a.x = 2 AND b.x = 4) OR (a.x = 0 AND b.x = 2) AS v FROM a, b) WHERE v";
        assert_eq!(run(&mut context, sql), [["2"]]);
        // nothing that can fail moves: only pairs that reach it may run the cast
        let sql = "SELECT count(*) FROM a, b WHERE (b.s::INTEGER = 4 AND a.x = 2) OR (b.s::INTEGER = 2 AND a.x = 0)";
        let plan = explain(&mut context, sql);
        assert_eq!(filters(&plan), 1, "{plan}");
        assert_eq!(run(&mut context, sql), [["2"]]);
        // and what differs per call neither
        let sql = "SELECT count(*) FROM a, b WHERE (a.x < random() * 0 AND b.x = 1) OR (a.x = 1 AND b.x = 2)";
        assert_eq!(run(&mut context, sql), [["1"]]);
    }

    #[test]
    fn in_subqueries_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE l (i INTEGER)");
        run(&mut context, "INSERT INTO l VALUES (1), (2), (NULL)");
        run(&mut context, "CREATE TABLE r (j INTEGER)");
        run(&mut context, "INSERT INTO r VALUES (1), (3)");
        run(&mut context, "CREATE TABLE n (j INTEGER)");
        run(&mut context, "INSERT INTO n VALUES (1), (NULL)");
        run(&mut context, "CREATE TABLE e (j INTEGER)");
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        // three-valued: every combination of a NULL on the left, a NULL on the right and no rows
        let marks = |context: &mut ClientContext, right: &str| {
            let sql = format!("SELECT i IN (SELECT j FROM {right}), i NOT IN (SELECT j FROM {right}) FROM l");
            run(context, &sql)
        };
        let rows = marks(&mut context, "r");
        assert_eq!(rows, [["true", "false"], ["false", "true"], ["NULL", "NULL"]]);
        let rows = marks(&mut context, "n");
        assert_eq!(rows, [["true", "false"], ["NULL", "NULL"], ["NULL", "NULL"]]);
        let rows = marks(&mut context, "e");
        assert_eq!(rows, [["false", "true"], ["false", "true"], ["false", "true"]]);
        // a filter on the marker only is a SEMI join, which drops false and NULL alike
        let sql = "SELECT i FROM l WHERE i IN (SELECT j FROM n)";
        let plan = explain(&mut context, sql);
        assert!(plan.contains("Join Type: SEMI"), "{plan}");
        assert_eq!(run(&mut context, sql), [["1"]]);
        // NOT IN needs the NULLs, so stays MARK
        let sql = "SELECT i FROM l WHERE i NOT IN (SELECT j FROM n)";
        let plan = explain(&mut context, sql);
        assert!(plan.contains("MARK"), "{plan}");
        assert_eq!(run(&mut context, sql), Vec::<Vec<String>>::new());
        assert_eq!(
            run(&mut context, "SELECT i FROM l WHERE i NOT IN (SELECT j FROM r)"),
            [["2"]]
        );
        // something else reads the marker: it has to stay
        let sql = "SELECT i, m FROM (SELECT i, i IN (SELECT j FROM r) AS m FROM l) WHERE m";
        let plan = explain(&mut context, sql);
        assert!(!plan.contains("SEMI"), "{plan}");
        assert_eq!(run(&mut context, sql), [["1", "true"]]);
        // OR, CASE and DML go through the marker; positions stay for what reads them
        let rows = run(
            &mut context,
            "SELECT i FROM l WHERE i IN (SELECT j FROM r) OR i IS NULL ORDER BY i NULLS LAST",
        );
        assert_eq!(rows, [["1"], ["NULL"]]);
        let rows = run(
            &mut context,
            "SELECT * FROM l WHERE i IN (SELECT j FROM r) UNION ALL SELECT 7",
        );
        assert_eq!(rows, [["1"], ["7"]]);
        run(&mut context, "DELETE FROM l WHERE i IN (SELECT j FROM r)");
        assert_eq!(run(&mut context, "SELECT count(*) FROM l"), [["2"]]);
        run(&mut context, "UPDATE l SET i = 10 WHERE i IN (SELECT 2)");
        assert_eq!(
            run(&mut context, "SELECT i FROM l ORDER BY i NULLS LAST"),
            [["10"], ["NULL"]]
        );
        // nested, and the same value twice on the right does not double a row
        let rows = run(
            &mut context,
            "SELECT count(*) FROM range(5) t(a) WHERE a IN (SELECT a2 FROM (VALUES (1), (1), (2)) v(a2) \
             WHERE a2 IN (SELECT 1 UNION ALL SELECT 2))",
        );
        assert_eq!(rows, [["2"]]);
    }

    #[test]
    fn join_order_matches_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(
            &mut context,
            "CREATE TABLE big AS SELECT i AS id, i % 500 AS mid_id, i % 20 AS small_id FROM range(20000) t(i)",
        );
        run(&mut context, "CREATE TABLE mid AS SELECT i AS id FROM range(500) t(i)");
        run(&mut context, "CREATE TABLE small AS SELECT i AS id FROM range(20) t(i)");
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let at = |plan: &str, table: &str| {
            let line = format!("Table: {table}");
            let line = plan.lines().position(|text| text.trim() == line);
            line.unwrap_or_else(|| panic!("{table} in {plan}"))
        };
        // written with the cross product first; the smaller tables are built, with no cross product
        let query = "FROM small, mid, big WHERE big.mid_id = mid.id AND big.small_id = small.id";
        let plan = explain(&mut context, &format!("SELECT count(*) {query}"));
        assert!(!plan.contains("Cross Product"), "{plan}");
        assert!(
            at(&plan, "big") < at(&plan, "mid") && at(&plan, "big") < at(&plan, "small"),
            "{plan}"
        );
        assert_eq!(run(&mut context, &format!("SELECT count(*) {query}")), [["20000"]]);
        // the smaller side of a join is on the right, however the query has it
        for query in [
            "big, mid WHERE big.mid_id = mid.id",
            "mid, big WHERE big.mid_id = mid.id",
        ] {
            let plan = explain(&mut context, &format!("SELECT count(*) FROM {query}"));
            assert!(at(&plan, "big") < at(&plan, "mid"), "{plan}");
        }
        // columns keep their order, also through a set operation, which reads them by position
        run(&mut context, "CREATE TABLE l (a INTEGER)");
        run(&mut context, "CREATE TABLE r AS SELECT i AS b FROM range(100) t(i)");
        run(&mut context, "INSERT INTO l VALUES (1)");
        let rows = run(&mut context, "SELECT * FROM l, r WHERE a = b UNION ALL SELECT 5, 6");
        assert_eq!(rows, [["1", "1"], ["5", "6"]]);
        assert_eq!(run(&mut context, "SELECT * FROM r, l WHERE a = b"), [["1", "1"]]);
        // a filter that reads three relations is on the join that has all of them
        let rows = run(
            &mut context,
            "SELECT count(*) FROM small s1, small s2, small s3 WHERE s1.id = s2.id AND s1.id + s2.id > s3.id",
        );
        assert_eq!(rows, [["290"]]);
        // what cannot move: volatile filters, the sides of outer joins
        let rows = run(
            &mut context,
            "SELECT count(*) FROM (SELECT * FROM small s1, small s2 WHERE s1.id = s2.id AND random() < 2) x",
        );
        assert_eq!(rows, [["20"]]);
        let rows = run(
            &mut context,
            "SELECT count(*), count(m.id) FROM big b LEFT JOIN mid m ON b.mid_id = m.id AND m.id < 10, small s \
             WHERE b.small_id = s.id",
        );
        assert_eq!(rows, [["20000", "400"]]);
        // a SEMI join goes down to what it reads; on both sides of the join it stays on top
        let sql = "SELECT count(*) FROM big, mid WHERE big.mid_id = mid.id AND mid.id IN (SELECT id FROM small)";
        let plan = explain(&mut context, sql);
        let joins: Vec<&str> = plan.lines().filter(|l| l.contains("Join Type")).collect();
        assert!(joins[0].contains("INNER") && joins[1].contains("SEMI"), "{plan}");
        assert_eq!(run(&mut context, sql), [["800"]]);
        let sql =
            "SELECT count(*) FROM big b1, mid WHERE b1.mid_id = mid.id AND b1.id + mid.id IN (SELECT id FROM small)";
        let plan = explain(&mut context, sql);
        let joins: Vec<&str> = plan.lines().filter(|l| l.contains("Join Type")).collect();
        assert!(joins[0].contains("SEMI") && joins[1].contains("INNER"), "{plan}");
        // a join on two keys divides by the rows of the smaller side, not by both of its domains:
        // with the domains multiplied, this one would look like 0 rows and go first
        run(
            &mut context,
            "CREATE TABLE fact AS SELECT i % 400 AS p, i % 50 AS q FROM range(20000) t(i)",
        );
        run(
            &mut context,
            "CREATE TABLE pairs AS SELECT i % 400 AS p, i % 50 AS q, i AS id FROM range(2000) t(i)",
        );
        let plan = explain(
            &mut context,
            "SELECT count(*) FROM fact f, pairs x WHERE f.p = x.p AND f.q = x.q",
        );
        assert!(plan.contains("~20,000 rows\n      Seq Scan"), "{plan}");
        // the answer does not depend on any of that
        let rows = run(
            &mut context,
            "SELECT count(*) FROM fact f, pairs x WHERE f.p = x.p AND f.q = x.q AND x.id < 400",
        );
        assert_eq!(rows, [["20000"]]);
        // over the limit of the exact search, and without anything to connect two of them
        let many = (0..14).map(|i| format!("small s{i}")).collect::<Vec<_>>().join(", ");
        let joins = (1..14)
            .map(|i| format!("s{}.id = s{i}.id", i - 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        assert_eq!(
            run(&mut context, &format!("SELECT count(*) FROM {many} WHERE {joins}")),
            [["20"]]
        );
        let plan = explain(&mut context, "SELECT * FROM l, r, mid WHERE a = b");
        assert!(plan.contains("Cross Product"), "{plan}");
        // off under the name DuckDB has for it: the query stays as it is written
        run(&mut context, "SET disabled_optimizers = 'join_order'");
        let plan = explain(&mut context, "SELECT count(*) FROM mid, big WHERE big.mid_id = mid.id");
        assert!(at(&plan, "mid") < at(&plan, "big"), "{plan}");
    }

    #[test]
    fn statistics_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(
            &mut context,
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
        );
        run(&mut context, "SET explain_output = 'OPTIMIZED_ONLY'");
        run(&mut context, "CREATE TABLE s (i INTEGER)");
        run(&mut context, "INSERT INTO s VALUES (1), (2), (3)");
        let explain = |context: &mut ClientContext, sql: &str| run(context, &format!("EXPLAIN {sql}"))[0][1].clone();
        let plan = explain(&mut context, "SELECT * FROM s WHERE i > 10");
        assert!(plan.contains("EMPTY_RESULT"), "{plan}");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i > 10"), [["0"]]);
        let plan = explain(&mut context, "SELECT * FROM s WHERE i <= 3");
        assert!(!plan.contains("FILTER"), "{plan}");
        // what a statement writes shows in the next plan
        run(&mut context, "INSERT INTO s VALUES (20)");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i > 10"), [["1"]]);
        run(&mut context, "UPDATE s SET i = 100 WHERE i = 1");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i >= 100"), [["1"]]);
        run(&mut context, "DELETE FROM s WHERE i = 100");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i >= 100"), [["0"]]);
        run(&mut context, "BEGIN");
        run(&mut context, "INSERT INTO s VALUES (500)");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i >= 500"), [["1"]]);
        run(&mut context, "ROLLBACK");
        assert_eq!(run(&mut context, "SELECT count(*) FROM s WHERE i >= 500"), [["0"]]);
        // a NULL stays NULL where the comparison is decided, but does not pass a filter
        run(&mut context, "CREATE TABLE n (i INTEGER, l INTEGER, u INTEGER)");
        run(
            &mut context,
            "INSERT INTO n VALUES (1, 5, NULL), (2, NULL, 9), (NULL, 5, 9)",
        );
        assert_eq!(run(&mut context, "SELECT i > 0 FROM n"), [["true"], ["true"], ["NULL"]]);
        assert_eq!(run(&mut context, "SELECT count(*) FROM n WHERE i > 0"), [["2"]]);
        assert_eq!(run(&mut context, "SELECT count(*) FROM n WHERE i < 0"), [["0"]]);
        assert_eq!(run(&mut context, "SELECT count(*) FROM n WHERE i IS NULL"), [["1"]]);
        // FALSE AND NULL is FALSE
        assert_eq!(
            run(&mut context, "SELECT i BETWEEN l AND u FROM n"),
            [["false"], ["NULL"], ["NULL"]]
        );
        // an outer join puts NULLs in a column that has none, and TRY_CAST too
        run(&mut context, "CREATE TABLE t (i INTEGER)");
        run(&mut context, "INSERT INTO t VALUES (1), (2)");
        run(&mut context, "CREATE TABLE r (i INTEGER)");
        run(&mut context, "INSERT INTO r VALUES (2)");
        assert_eq!(
            run(
                &mut context,
                "SELECT t.i, r.i IS NULL FROM t LEFT JOIN r ON t.i = r.i ORDER BY 1"
            ),
            [["1", "true"], ["2", "false"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT t.i, r.i IS NULL FROM r RIGHT JOIN t ON t.i = r.i ORDER BY 1"
            ),
            [["1", "true"], ["2", "false"]]
        );
        run(&mut context, "CREATE TABLE big (i INTEGER)");
        run(&mut context, "INSERT INTO big VALUES (1000)");
        assert_eq!(
            run(&mut context, "SELECT TRY_CAST(i AS TINYINT) IS NULL FROM big"),
            [["true"]]
        );
        // a failing expression keeps failing
        assert!(context.query("SELECT 1 FROM big WHERE (i::TINYINT) IS NULL").is_err());
    }

    #[test]
    fn flattened_subqueries_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
        run(
            &mut context,
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
        );
        run(&mut context, "CREATE TABLE a (x INTEGER, k INTEGER)");
        run(&mut context, "CREATE TABLE b (y INTEGER, k INTEGER)");
        run(&mut context, "INSERT INTO a VALUES (1, 1), (2, 1), (3, 2), (NULL, 3)");
        run(&mut context, "INSERT INTO b VALUES (10, 1), (20, 1), (30, 2)");
        let explained = run(
            &mut context,
            "EXPLAIN SELECT x FROM a WHERE x > 1 AND EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        );
        assert!(explained[0][1].contains("LEFT_DELIM_JOIN"), "{explained:?}");
        // the left rows keep their order
        assert_eq!(
            run(&mut context, "SELECT x, (SELECT sum(y) FROM b WHERE b.k = a.k) FROM a"),
            [["1", "30"], ["2", "30"], ["3", "30"], ["NULL", "NULL"]]
        );
        // a value without rows still gets what the aggregate gives for no rows
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT count(*) FROM b WHERE b.k = a.k) FROM a"
            ),
            [["1", "2"], ["2", "2"], ["3", "1"], ["NULL", "0"]]
        );
        // LIMIT and OFFSET hold per value
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT y FROM b WHERE b.k = a.k ORDER BY y DESC LIMIT 1) FROM a"
            ),
            [["1", "20"], ["2", "20"], ["3", "30"], ["NULL", "NULL"]]
        );
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT y FROM b WHERE b.k = a.k ORDER BY y DESC LIMIT 1 OFFSET 1) FROM a"
            ),
            [["1", "10"], ["2", "10"], ["3", "NULL"], ["NULL", "NULL"]]
        );
        // set operations carry the values along
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT sum(v) FROM (SELECT y AS v FROM b WHERE b.k = a.k UNION ALL SELECT a.x) s) FROM a"
            ),
            [["1", "31"], ["2", "32"], ["3", "33"], ["NULL", "NULL"]]
        );
        // nested: the inner one reads the middle query, or the outer one, which stays row by row
        for inner in ["b2.k = b.k", "b2.k = a.k"] {
            let sql = format!(
                "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND b.y = \
                 (SELECT max(b2.y) FROM b b2 WHERE {inner}))"
            );
            assert_eq!(run(&mut context, &sql), [["1"], ["2"], ["3"]]);
        }
        // NULLs are equal in the domain
        run(&mut context, "CREATE TABLE n (k INTEGER)");
        run(&mut context, "INSERT INTO n VALUES (1), (NULL)");
        assert_eq!(
            run(&mut context, "SELECT k, (SELECT 42 WHERE n.k IS NULL) FROM n"),
            [["1", "NULL"], ["NULL", "42"]]
        );
        // ANY: TRUE, NULL for a NULL among the rows or as the value, and FALSE, also for no rows
        run(&mut context, "CREATE TABLE c (k INTEGER, v INTEGER)");
        run(&mut context, "INSERT INTO c VALUES (1, NULL), (1, 5), (2, 6)");
        run(&mut context, "CREATE TABLE l (x INTEGER, k INTEGER)");
        run(
            &mut context,
            "INSERT INTO l VALUES (5, 1), (7, 1), (7, 2), (NULL, 2), (1, 3)",
        );
        assert_eq!(
            run(&mut context, "SELECT x IN (SELECT v FROM c WHERE c.k = l.k) FROM l"),
            [["true"], ["NULL"], ["false"], ["NULL"], ["false"]]
        );
        // more than one row
        let error = context
            .query("SELECT (SELECT y FROM b WHERE b.k = a.k) FROM a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("More than one row returned by a subquery"), "{error}");
        run(&mut context, "SET scalar_subquery_error_on_multiple_rows = false");
        assert_eq!(
            run(
                &mut context,
                "SELECT x, (SELECT y FROM b WHERE b.k = a.k) FROM a WHERE x = 3"
            ),
            [["3", "30"]]
        );
        // an operator that reads its child by position does not see the columns of the join
        run(
            &mut context,
            "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND b.y > 20)",
        );
        assert_eq!(run(&mut context, "SELECT x FROM a"), [["1"], ["2"], ["NULL"]]);
    }

    #[test]
    fn correlated_subqueries_match_duckdb() {
        let mut context = ClientContext::new(DatabaseInstance::new());
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
        assert_eq!(run(&mut context, "SELECT (SELECT sum(a.x)) FROM a").len(), 1);
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

    fn error(context: &mut ClientContext, sql: &str) -> String {
        context.query(sql).expect_err("the statement fails").to_string()
    }

    #[test]
    fn pivot_and_unpivot() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE s(y INT, q VARCHAR, v INT)");
        run(&mut c, "INSERT INTO s VALUES (1, 'a', 10), (1, 'b', 20), (2, 'a', 30)");
        assert_eq!(
            run(&mut c, "SELECT * FROM s PIVOT (sum(v) FOR q IN ('a', 'b')) ORDER BY y"),
            [["1", "10", "20"], ["2", "30", "NULL"]]
        );
        assert_eq!(
            run(&mut c, "PIVOT s ON q USING sum(v) GROUP BY y ORDER BY y"),
            [["1", "10", "20"], ["2", "30", "NULL"]]
        );
        run(&mut c, "CREATE TABLE w(id INT, a INT, b INT)");
        run(&mut c, "INSERT INTO w VALUES (1, 5, NULL)");
        assert_eq!(run(&mut c, "UNPIVOT w ON a, b INTO NAME n VALUE x"), [["1", "a", "5"]]);
    }

    #[test]
    fn columns_expressions() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t(a1 INT, a2 INT, b INT)");
        run(&mut c, "INSERT INTO t VALUES (1, 2, 3), (4, 5, 6)");
        assert_eq!(run(&mut c, "SELECT max(COLUMNS('a.')) FROM t"), [["4", "5"]]);
        assert_eq!(run(&mut c, "SELECT COLUMNS(['b']) FROM t WHERE a1 = 1"), [["3"]]);
        assert_eq!(run(&mut c, "SELECT * LIKE 'a%' FROM t WHERE a1 = 1"), [["1", "2"]]);
        assert_eq!(run(&mut c, "SELECT count(*) FROM t WHERE COLUMNS('a.') > 1"), [["1"]]);
    }

    #[test]
    fn temporary_objects() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t AS SELECT 1 AS x");
        run(&mut c, "CREATE TEMPORARY TABLE t AS SELECT 2 AS x");
        assert_eq!(run(&mut c, "SELECT x FROM t"), [["2"]]);
        run(&mut c, "DROP TABLE t");
        assert_eq!(run(&mut c, "SELECT x FROM t"), [["1"]]);
    }

    #[test]
    fn generated_columns() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t(a INT, b AS (a + 1), c INT, d AS (b * c))");
        run(&mut c, "INSERT INTO t VALUES (1, 2)");
        assert_eq!(run(&mut c, "SELECT * FROM t"), [["1", "2", "2", "4"]]);
        assert_eq!(run(&mut c, "SELECT s FROM t x(p, q, r, s)"), [["4"]]);
        assert!(error(&mut c, "INSERT INTO t (b) VALUES (1)").contains("generated"));
        assert!(error(&mut c, "ALTER TABLE t DROP COLUMN a").contains("dependency"));
        run(&mut c, "ALTER TABLE t RENAME a TO z");
        assert_eq!(run(&mut c, "SELECT b FROM t"), [["2"]]);
        assert!(error(&mut c, "CREATE TABLE u(x AS (y), y AS (x), z INT)").contains("Circular"));
    }

    #[test]
    fn describe_and_summarize() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t(a INT PRIMARY KEY, b VARCHAR DEFAULT 'x')");
        assert_eq!(
            run(&mut c, "DESCRIBE t"),
            [
                ["a", "INTEGER", "NO", "PRI", "NULL", "NULL"],
                ["b", "VARCHAR", "YES", "NULL", "'x'", "NULL"]
            ]
        );
        assert_eq!(run(&mut c, "SHOW TABLES"), [["t"]]);
        assert_eq!(run(&mut c, "SELECT table_name FROM duckdb_tables"), [["t"]]);
        assert_eq!(
            run(&mut c, "SELECT column_name, count FROM (SUMMARIZE t)"),
            [["a", "0"], ["b", "0"]]
        );
    }

    #[test]
    fn merge_into() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t(k INT, v INT)");
        run(&mut c, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
        let merge = "MERGE INTO t USING (VALUES (1, 11), (4, 44)) s(k, v) USING (k) \
                     WHEN MATCHED THEN UPDATE SET v = s.v \
                     WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v) \
                     WHEN NOT MATCHED BY SOURCE AND t.k = 3 THEN DELETE";
        assert_eq!(run(&mut c, merge), [["3"]]);
        assert_eq!(
            run(&mut c, "SELECT * FROM t ORDER BY k"),
            [["1", "11"], ["2", "20"], ["4", "44"]]
        );
        assert!(
            error(
                &mut c,
                "MERGE INTO t USING (VALUES (1), (1)) s(k) USING (k) WHEN MATCHED THEN DELETE"
            )
            .contains("more than once")
        );
    }

    #[test]
    fn asof_positional_and_mark_joins() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE p(t INT, v INT)");
        run(&mut c, "INSERT INTO p VALUES (1, 10), (5, 50)");
        run(&mut c, "CREATE TABLE q(t INT)");
        run(&mut c, "INSERT INTO q VALUES (0), (3), (9)");
        assert_eq!(
            run(
                &mut c,
                "SELECT q.t, v FROM q ASOF LEFT JOIN p ON q.t >= p.t ORDER BY q.t"
            ),
            [["0", "NULL"], ["3", "10"], ["9", "50"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT * FROM (VALUES (1), (2)) a(x) POSITIONAL JOIN (VALUES (3)) b(y)"
            ),
            [["1", "3"], ["2", "NULL"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT t, __mark_join_marker FROM q JOIN BY (TYPE MARK) p ON q.t = p.t ORDER BY t"
            ),
            [["0", "false"], ["3", "false"], ["9", "false"]]
        );
    }

    #[test]
    fn recursive_cte_using_key() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        let sql = "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT 1, 0 UNION ALL \
                   SELECT k, v + 1 FROM t WHERE v < 3) SELECT * FROM t";
        assert_eq!(run(&mut c, sql), [["1", "3"]]);
    }

    #[test]
    fn samples_and_percent_limits() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        assert_eq!(
            run(
                &mut c,
                "SELECT count(*) FROM range(100) USING SAMPLE 7 ROWS (reservoir, 1)"
            ),
            [["7"]]
        );
        assert_eq!(
            run(&mut c, "SELECT count(*) FROM (SELECT * FROM range(100) LIMIT 10%)"),
            [["10"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT DISTINCT ON (x) x, y FROM (VALUES (1, 2), (1, 1), (2, 3)) t(x, y) ORDER BY x, y"
            ),
            [["1", "1"], ["2", "3"]]
        );
    }

    #[test]
    fn row_subqueries_and_outer_aggregates() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(&mut c, "CREATE TABLE t(a INT, b INT)");
        run(&mut c, "INSERT INTO t VALUES (1, 2), (3, 4)");
        assert_eq!(run(&mut c, "SELECT a FROM t WHERE (a, b) IN (SELECT 3, 4)"), [["3"]]);
        assert_eq!(
            run(
                &mut c,
                "SELECT max(a) > ANY(SELECT a FROM t WHERE a > min(t2.a)) FROM t t2"
            ),
            [["false"]]
        );
    }

    #[test]
    fn explain_names_the_operators() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(
            &mut c,
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
        );
        run(&mut c, "CREATE TABLE t(a INT)");
        let plan = run(&mut c, "EXPLAIN SELECT a FROM t ORDER BY a");
        assert_eq!(plan[0][0], "physical_plan");
        assert!(plan[0][1].contains("ORDER_BY"));
    }

    #[test]
    fn a_finished_limit_lets_the_operators_above_it_drain() {
        let mut c = ClientContext::new(DatabaseInstance::new());
        run(
            &mut c,
            "CREATE TABLE a AS SELECT i AS id, i % 100 AS k FROM range(10000) t(i)",
        );
        run(&mut c, "CREATE TABLE b AS SELECT 0 AS z, id, k FROM a");
        // the LIMIT is done after its first chunk, with the cross product still on its first output
        assert_eq!(
            run(&mut c, "SELECT count(*) FROM (SELECT id FROM a LIMIT 2) a CROSS JOIN b"),
            [["20000"]]
        );
        assert_eq!(
            run(
                &mut c,
                "SELECT p.k, n FROM (SELECT b.k, count(*) AS n FROM (SELECT id FROM a LIMIT 2) a \
                 CROSS JOIN b GROUP BY b.k) p WHERE p.k = 42"
            ),
            [["42", "200"]]
        );
        // the same for a hash join with more matches than fit in one output chunk
        assert_eq!(
            run(
                &mut c,
                "SELECT count(*) FROM (SELECT 0 AS z FROM a LIMIT 2) a JOIN b ON a.z = b.z"
            ),
            [["20000"]]
        );
        // a LIMIT above the join still ends the pipeline
        assert_eq!(
            run(
                &mut c,
                "SELECT count(*) FROM (SELECT a.id FROM a CROSS JOIN b LIMIT 5000) t"
            ),
            [["5000"]]
        );
    }
}
