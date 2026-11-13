// Ported from duckdb/src/planner/{binder,bind_context,table_binding,planner}.cpp @ rust-port-baseline
//
//! The binder: resolves the names in a parsed statement against the catalog, types its
//! expressions and plans it as a tree of logical operators.

mod alter;
mod copy;
mod create_index;
mod expression;
mod generated;
mod join;
mod json;
mod lambda;
mod macros;
mod match_recognize;
mod merge;
mod on_conflict;
mod parquet;
mod pivot;
pub(crate) use pivot::has_subquery;
pub mod read_duckdb;
mod show;
mod star;
mod statements;
mod table_constraints;
mod table_functions;
pub(crate) use table_functions::named_argument;
mod window;

use crate::optimizer::OptimizerOptions;
pub use alter::{BoundAlter, BoundTableAlter};
pub use expression::{enum_type_from, is_builtin_type};
pub(crate) use statements::range_parameters;
pub use statements::schema_of;

use super::expression::{BoundAggregate, BoundExpr, ColumnBinding};
use super::logical::LogicalOperator;
use crate::catalog::{Catalog, MacroCatalogEntry, ViewCatalogEntry};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType};
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;
use duckrs_parser::ast::Statement;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// What a statement returns to the client (`StatementReturnType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementReturnType {
    /// Rows of a query.
    QueryResult,
    /// The number of rows changed, as a single `Count` column.
    ChangedRows,
    /// Nothing.
    Nothing,
}

/// A planned statement (`BoundStatement` + `Planner` output).
#[derive(Debug)]
pub struct BoundStatement {
    pub plan: LogicalOperator,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    /// what ORDER BY of an enclosing set operation can call the columns
    pub column_aliases: Vec<(String, usize)>,
    pub return_type: StatementReturnType,
}

/// The columns a FROM clause entry makes visible (`TableBinding`).
#[derive(Debug, Clone)]
pub struct Binding {
    pub alias: String,
    /// the schema of a base table that kept its name, which can tell it from another's
    pub schema: Option<String>,
    pub table_index: usize,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    /// where a base table scan puts the `rowid` pseudo-column
    pub row_id: Option<usize>,
    /// with `names`, the expressions of the generated columns; empty without any
    pub generated: Vec<Option<ast::Expr>>,
    /// columns a scan offers that `*` leaves out (`GetVirtualColumns`), as name, type and position
    pub virtual_columns: Vec<(String, LogicalType, usize)>,
}

impl Binding {
    fn column_index(&self, name: &str) -> Option<usize> {
        if let Some(position) = name.strip_prefix(generated::POSITIONAL_COLUMN) {
            return position.parse().ok();
        }
        self.names.iter().position(|n| n.eq_ignore_ascii_case(name))
    }

    /// `Binding::HasMatchingBinding`: a column, or a column alias such as the original name of a
    /// set-returning function's column.
    fn matches_column(&self, name: &str) -> bool {
        self.column_index(name).is_some() || self.column_alias(name).is_some()
    }

    /// `Binding::AddColumnAlias`: another name of one of the columns (kept as a hidden column at the
    /// column's own position, which a scan's virtual columns are never at).
    fn column_alias(&self, name: &str) -> Option<usize> {
        self.virtual_column(name)
            .map(|(index, _, _)| index)
            .filter(|index| *index < self.names.len())
    }

    /// `rowid`, for a table that has no column of that name (`TableBinding::Bind`).
    fn row_id_column(&self, name: &str) -> Option<usize> {
        self.row_id.filter(|_| name.eq_ignore_ascii_case("rowid"))
    }

    /// A column by name: its index, type and name as spelled here.
    fn column(&self, name: &str) -> Option<(usize, LogicalType, String)> {
        match self.column_index(name) {
            Some(index) => {
                let stored = self.generated.iter().take(index).filter(|g| g.is_none()).count();
                let position = if self.generated.is_empty() { index } else { stored };
                Some((position, self.types[index].clone(), self.names[index].clone()))
            }
            None => self
                .row_id_column(name)
                .map(|index| (index, LogicalType::BIGINT, "rowid".to_string()))
                .or_else(|| self.virtual_column(name)),
        }
    }

    /// A virtual column of a scan, such as `filename`, by name.
    fn virtual_column(&self, name: &str) -> Option<(usize, LogicalType, String)> {
        self.virtual_columns
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
            .map(|(n, ty, index)| (*index, ty.clone(), n.clone()))
    }

    /// Whether `name` is a hidden column here: `rowid` or a virtual column of a scan.
    fn hidden_column(&self, name: &str) -> bool {
        self.row_id_column(name).is_some() || self.virtual_column(name).is_some()
    }
}

/// A column shared by the sides of USING or NATURAL joins (`UsingColumnSet`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsingColumnSet {
    pub column: String,
    /// the aliases of the bindings that have the column
    pub bindings: Vec<String>,
    /// the binding that always has the value; without one, the column is their COALESCE
    pub primary: Option<String>,
}

impl UsingColumnSet {
    /// The column as an expression, spelled `column` where the query spelled it that way.
    fn expression(&self, column: &str) -> ast::Expr {
        let qualified =
            |alias: &String| ast::Expr::from(ast::ExprKind::ColumnRef(vec![alias.clone(), column.to_string()]));
        match &self.primary {
            Some(primary) => qualified(primary),
            None => ast::Expr::from(ast::ExprKind::Operator {
                op: ast::OperatorType::Coalesce,
                children: self.bindings.iter().map(qualified).collect(),
            }),
        }
    }
}

/// The bindings of the current query level (`BindContext`), in FROM clause order.
#[derive(Debug, Default, Clone)]
pub struct BindContext {
    pub bindings: Vec<Binding>,
    pub using_columns: Vec<UsingColumnSet>,
}

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

impl BindContext {
    /// `BindContext::AddBinding`. Two bindings can have one alias: only a reference that could
    /// mean either of them is an error.
    pub fn add_binding(&mut self, binding: Binding) {
        self.bindings.push(binding);
    }

    /// `BindContext::AddContext`.
    fn add_context(&mut self, other: BindContext) {
        self.bindings.extend(other.bindings);
        self.using_columns.extend(other.using_columns);
    }

    /// `BindContext::RemoveContext`: a USING column goes too when nothing refers to it anymore.
    fn remove_context(&mut self, aliases: &[String]) {
        for alias in aliases {
            let mut removed: Vec<String> = Vec::new();
            for set in &mut self.using_columns {
                set.bindings.retain(|b| b != alias);
                if set.bindings.is_empty() || set.primary.as_ref() == Some(alias) {
                    removed.push(set.column.clone());
                }
            }
            self.using_columns
                .retain(|s| !removed.iter().any(|c| c.eq_ignore_ascii_case(&s.column)));
            self.bindings.retain(|b| b.alias != *alias);
        }
    }

    fn aliases(&self) -> Vec<String> {
        self.bindings.iter().map(|b| b.alias.clone()).collect()
    }

    /// `BindContext::GetMatchingBindings`.
    fn matching_bindings(&self, column: &str) -> Vec<&Binding> {
        self.bindings
            .iter()
            .filter(|b| b.matches_column(column))
            .collect()
    }

    /// `BindContext::GetActualColumnName`: the column as its binding spells it.
    fn actual_column_name(&self, alias: &str, column: &str) -> String {
        self.bindings
            .iter()
            .find(|b| b.alias == alias)
            .and_then(|b| b.column_index(column).map(|i| b.names[i].clone()))
            .unwrap_or_else(|| column.to_string())
    }

    /// The expression of the generated column `names` refers to, if it does.
    fn generated_expression(&self, names: &[String]) -> Option<ast::Expr> {
        let (table, column) = match names {
            [column] => (None, column),
            [table, column] => (Some(table), column),
            _ => return None,
        };
        let binding = self
            .bindings
            .iter()
            .filter(|b| !b.generated.is_empty())
            .filter(|b| table.is_none_or(|t| b.alias.eq_ignore_ascii_case(t)))
            .find(|b| b.column_index(column).is_some())?;
        binding.generated[binding.column_index(column)?].clone()
    }

    /// `BindContext::GetUsingBinding(column)`: the USING set of a column, which has to be unique.
    fn using_binding(&self, column: &str) -> Result<Option<&UsingColumnSet>> {
        let sets: Vec<&UsingColumnSet> = self
            .using_columns
            .iter()
            .filter(|s| s.column.eq_ignore_ascii_case(column))
            .collect();
        if sets.len() > 1 {
            let candidates: String = sets
                .iter()
                .map(|set| {
                    let columns: Vec<String> = set
                        .bindings
                        .iter()
                        .map(|alias| format!("{alias}.{}", self.actual_column_name(alias, column)))
                        .collect();
                    format!("[{}]", columns.join(", "))
                })
                .collect();
            return Err(binder_error(format!(
                "Ambiguous column reference: column \"{column}\" can refer to either:\n{candidates}"
            )));
        }
        Ok(sets.first().copied())
    }

    /// `BindContext::GetUsingBinding(column, binding)`: the USING set this binding's column is in.
    fn using_binding_of(&self, column: &str, alias: &str) -> Option<usize> {
        self.using_columns
            .iter()
            .position(|s| s.column.eq_ignore_ascii_case(column) && s.bindings.iter().any(|b| b == alias))
    }

    fn remove_using_binding(&mut self, set: Option<&UsingColumnSet>) {
        if let Some(set) = set {
            self.using_columns.retain(|s| s != set);
        }
    }

    /// `ColumnQualifier::QualifyColumnName` for USING columns: what an unqualified column means
    /// if it is one.
    fn using_expression(&self, column: &str) -> Result<Option<ast::Expr>> {
        Ok(self.using_binding(column)?.map(|set| set.expression(column)))
    }

    /// `BindContext::GetSimilarBindings`: the columns most like `name`, qualified when
    /// ambiguous.
    fn similar_bindings(&self, name: &str) -> Vec<String> {
        let mut scores = Vec::new();
        for binding in &self.bindings {
            for column in &binding.names {
                let score = similarity_rating(column, name);
                let matches = self
                    .bindings
                    .iter()
                    .filter(|b| b.column_index(column).is_some())
                    .count();
                if matches > 1 {
                    scores.push((format!("{}.{column}", binding.alias), score));
                } else {
                    scores.push((column.clone(), score));
                }
            }
        }
        top_n_strings(scores)
    }

    /// `BindContext::GetMatchingBinding`: the one binding with this column, if any.
    fn matching_binding(&self, name: &str) -> Result<Option<(&Binding, usize)>> {
        let mut found: Option<(&Binding, usize)> = None;
        for binding in &self.bindings {
            if let Some(index) = binding.column_index(name) {
                if let Some((previous, _)) = found {
                    let quoted = ast::sql_identifier(name);
                    return Err(binder_error(format!(
                        "Ambiguous reference to column name {} (use: '{}.{quoted}' or '{}.{quoted}')",
                        ast::sql_quoted_identifier(name),
                        previous.alias,
                        binding.alias
                    )));
                }
                found = Some((binding, index));
            }
        }
        Ok(found)
    }

    /// Binds an unqualified column (`BindContext::GetMatchingBinding` + `TableBinding::Bind`).
    fn bind_column(&self, name: &str) -> Result<(ColumnBinding, LogicalType, String)> {
        let found = match self.matching_binding(name)? {
            Some((binding, _)) => Some(binding),
            None => {
                let mut with_row_id = self.bindings.iter().filter(|b| b.hidden_column(name));
                let first = with_row_id.next();
                if let (Some(first), Some(second)) = (first, with_row_id.next()) {
                    return Err(binder_error(format!(
                        "Ambiguous reference to column name {} (use: '{}.{name}' or '{}.{name}')",
                        ast::sql_quoted_identifier(name),
                        first.alias,
                        second.alias
                    )));
                }
                first
            }
        };
        match found {
            Some(binding) => {
                let (index, ty, name) = binding.column(name).expect("the binding has this column");
                Ok((ColumnBinding::new(binding.table_index, index), ty, name))
            }
            None if self.bindings.is_empty() => Err(binder_error(format!(
                "Referenced column \"{name}\" was not found because the FROM clause is missing"
            ))),
            None => Err(binder_error(format!(
                "Referenced column \"{name}\" not found in FROM clause!{}",
                candidates_message(&self.similar_bindings(name), "Candidate bindings")
            ))),
        }
    }

    /// `BindContext::GetBindings`: the bindings with this alias, at least one.
    fn bindings_of(&self, schema: Option<&str>, table: &str) -> Result<Vec<&Binding>> {
        let matching: Vec<&Binding> = self
            .bindings
            .iter()
            .filter(|b| b.alias.eq_ignore_ascii_case(table))
            .filter(|b| match schema {
                Some(schema) => b.schema.as_ref().is_some_and(|s| Self::schema_matches(s, schema)),
                None => true,
            })
            .collect();
        if matching.is_empty() {
            let scores = self
                .bindings
                .iter()
                .map(|b| (b.alias.clone(), similarity_rating(&b.alias, table)))
                .collect();
            let name = match schema {
                Some(schema) => format!("{schema}.{table}"),
                None => table.to_string(),
            };
            return Err(binder_error(format!(
                "Referenced table \"{name}\" not found!{}",
                candidates_message(&top_n_strings(scores), "Candidate tables")
            )));
        }
        Ok(matching)
    }

    /// `BindingAlias::Matches`: whether a qualification names the schema a binding is of. It is the
    /// end of its path: the innermost schema, the path, or the catalog and the path.
    fn schema_matches(binding: &str, qualification: &str) -> bool {
        let sep = crate::catalog::SCHEMA_PATH_SEPARATOR;
        let mut full = vec![crate::catalog::DEFAULT_CATALOG];
        full.extend(binding.split(sep));
        let given: Vec<&str> = qualification.split(sep).collect();
        given.len() <= full.len()
            && full[full.len() - given.len()..]
                .iter()
                .zip(&given)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// `Binder::QualifyColumnName`: where the struct fields of a dotted name start, if it has
    /// any. What comes before is the column.
    pub(super) fn struct_field_start(&self, names: &[String]) -> Option<usize> {
        let has_table_column = |table: &str, column: &str| {
            self.bindings
                .iter()
                .any(|b| b.alias.eq_ignore_ascii_case(table) && b.matches_column(column))
        };
        let schema_table_column = |schema: &str, table: &str, column: &str| {
            self.bindings.iter().any(|b| {
                b.alias.eq_ignore_ascii_case(table)
                    && b.schema.as_ref().is_some_and(|s| Self::schema_matches(s, schema))
                    && b.matches_column(column)
            })
        };
        let has_column = |column: &str| self.bindings.iter().any(|b| b.column_index(column).is_some());
        // `[catalog.]schema path.table.column` of a table in a nested schema, the longest first
        let sep = crate::catalog::SCHEMA_PATH_SEPARATOR.to_string();
        let nested = (2..names.len().saturating_sub(1))
            .rev()
            .find(|&table| schema_table_column(&names[..table].join(&sep), &names[table], &names[table + 1]));
        if let Some(table) = nested {
            return (table + 2 < names.len()).then_some(table + 2);
        }
        let column_parts = match names {
            [schema, table, column, ..] if schema_table_column(schema, table, column) => 3,
            [table, column, ..] if has_table_column(table, column) => 2,
            [column, _, ..] if has_column(column) => 1,
            _ => return None,
        };
        (column_parts < names.len()).then_some(column_parts)
    }

    /// `AmbiguityException`, for bindings with one alias.
    fn ambiguous_table(table: &str) -> Error {
        binder_error(format!(
            "Ambiguous reference to table \"{table}\" (duplicate alias \"{table}\", explicitly alias one of the \
             tables using \"AS my_alias\")"
        ))
    }

    /// Binds `[schema.]table.column` (`BindContext::GetBinding(alias, column)`): of the bindings
    /// with this alias, the one with the column.
    fn bind_qualified_column(
        &self,
        schema: Option<&str>,
        table: &str,
        name: &str,
    ) -> Result<(ColumnBinding, LogicalType, String)> {
        let matching = self.bindings_of(schema, table)?;
        let mut with_column = matching.iter().filter(|b| b.column(name).is_some());
        let Some(binding) = with_column.next() else {
            return Err(binder_error(format!(
                "Table {} does not have a column named {}",
                ast::sql_quoted_identifier(&matching[0].alias),
                ast::sql_quoted_identifier(name)
            )));
        };
        if with_column.next().is_some() {
            return Err(Self::ambiguous_table(table));
        }
        let (index, ty, name) = binding.column(name).expect("filtered above");
        Ok((ColumnBinding::new(binding.table_index, index), ty, name))
    }

    /// Binds a column reference of any qualification.
    fn bind_column_ref(&self, names: &[String]) -> Result<(ColumnBinding, LogicalType, String)> {
        match names {
            [column] => self.bind_column(column),
            [table, column] => self.bind_qualified_column(None, table, column),
            [schema, table, column] => {
                // `table.column.field` needs STRUCT, which `schema.table.column` is not
                let is_field = self.bindings_of(Some(schema), table).is_err()
                    && self
                        .bindings
                        .iter()
                        .any(|b| b.alias.eq_ignore_ascii_case(schema) && b.column_index(table).is_some());
                if is_field {
                    return Err(crate::unported("struct field references"));
                }
                self.bind_qualified_column(Some(schema), table, column)
            }
            [catalog, schema, table, column] if catalog.eq_ignore_ascii_case(crate::catalog::DEFAULT_CATALOG) => {
                self.bind_qualified_column(Some(schema), table, column)
            }
            // `[catalog.]schema path.table.column` of a table in a nested schema
            [prefix @ .., table, column] if prefix.len() >= 2 => {
                let nested = prefix.join(&crate::catalog::SCHEMA_PATH_SEPARATOR.to_string());
                self.bind_qualified_column(Some(&nested), table, column)
            }
            _ => Err(crate::unported("column references with this qualification")),
        }
    }

    /// `BindContext::BindColumn(PositionalReferenceExpression)`: the column at a position of the
    /// FROM clause; `#0` is the first table's rowid.
    fn position_to_column(&self, position: u64) -> Result<Vec<String>> {
        let mut total = 0;
        let mut current = position.wrapping_sub(1) as usize;
        for binding in &self.bindings {
            if position == 0 {
                return Ok(vec![binding.alias.clone(), "rowid".to_string()]);
            }
            if let Some(name) = binding.names.get(current) {
                return Ok(self.qualified_names(binding, name));
            }
            total += binding.names.len();
            current -= binding.names.len();
        }
        Err(binder_error(format!(
            "Positional reference {position} out of range (total {total} columns)"
        )))
    }

    /// The names that refer to a column of `binding`: with its schema if other bindings share
    /// its alias (`BindContext::CreateColumnReference`).
    fn qualified_names(&self, binding: &Binding, column: &str) -> Vec<String> {
        let shared = self
            .bindings
            .iter()
            .filter(|b| b.alias.eq_ignore_ascii_case(&binding.alias))
            .count()
            > 1;
        match &binding.schema {
            Some(schema) if shared => vec![schema.clone(), binding.alias.clone(), column.to_string()],
            _ => vec![binding.alias.clone(), column.to_string()],
        }
    }
}

/// The `default_null_order` setting (`DefaultOrderByNullType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultNullOrder {
    NullsFirst,
    NullsLast,
    NullsFirstOnAscLastOnDesc,
    NullsLastOnAscFirstOnDesc,
}

/// What the session knows that binding can ask for: `current_setting` and `getvariable` are
/// constants by the time they run.
pub struct SessionValues {
    /// the name of the database statements run in, which its catalog does not know
    pub database: String,
    pub variables: HashMap<String, Value>,
    pub setting: SettingLookup,
    /// `duckdb_logs()`, and the connection's logger for `write_log`
    pub log_manager: crate::logging::SharedLogManager,
    pub log_target: u64,
    /// `current_connection_id()` and `current_query_id()`
    pub connection_id: u64,
    pub query_id: u64,
    /// `default_collation`: the collation of a VARCHAR that has none
    pub default_collation: String,
    /// `duckdb_settings()`
    pub settings: SettingList,
    /// `CatalogSearchPath::GetSetPaths`: the schemas of the search path that was set
    pub search_path: Vec<String>,
    /// `current_query()`
    pub current_query: String,
    /// `read_duckdb`: a database file opened read-only for the statement (`AttachDatabase`, hidden)
    pub open_database: OpenDatabase,
    /// the rows an `Appender` flushes: the names and types of its columns, and its chunks
    pub appended: Option<AppendedRows>,
    /// the client's replacement scans: rows for a table name the catalog does not have
    pub replacement_scans: Vec<ReplacementScan>,
    /// `DatabaseManager::IsAttachedDatabaseFile`: the files of the attached databases
    pub database_files: Box<dyn Fn() -> Vec<std::path::PathBuf> + Send + Sync>,
}

/// The rows of an `Appender`, for `__internal_appender_chunks`.
pub type AppendedRows = (
    Vec<String>,
    Vec<LogicalType>,
    Arc<Vec<duckrs_common::vector::DataChunk>>,
);

/// `ReplacementScan`: for a table name, the rows to read in its place (names, types and chunks), if
/// the scan has any.
pub type ReplacementScan = Arc<dyn Fn(&str) -> Option<Result<AppendedRows>> + Send + Sync>;

/// The replacement scans of a connection.
#[derive(Clone, Default)]
pub struct ReplacementScans(pub Vec<ReplacementScan>);

impl std::fmt::Debug for ReplacementScans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReplacementScans({})", self.0.len())
    }
}

/// Opens a database file read-only: its catalog, and what keeps it open.
pub type OpenDatabase = Arc<dyn Fn(&str) -> Result<(Arc<Catalog>, Arc<dyn std::any::Any + Send + Sync>)> + Send + Sync>;

/// `current_setting`.
pub type SettingLookup = Box<dyn Fn(&str) -> Result<Value> + Send + Sync>;

/// `duckdb_settings()`.
pub type SettingList = Box<dyn Fn() -> Vec<crate::client::settings::SettingRow> + Send + Sync>;

impl std::fmt::Debug for SessionValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionValues")
            .field("variables", &self.variables)
            .finish()
    }
}

/// The settings that change how statements bind.
#[derive(Debug, Clone)]
pub struct BindOptions {
    /// `txid_current`
    pub transaction_id: u64,
    /// `default_order`
    pub default_order: OrderType,
    /// `default_null_order`
    pub default_null_order: DefaultNullOrder,
    /// `lambda_syntax = ENABLE_SINGLE_ARROW`
    pub single_arrow_lambdas: bool,
    /// `pivot_limit`
    pub pivot_limit: u64,
    /// `scalar_subquery_error_on_multiple_rows`
    pub scalar_subquery_error: bool,
    /// `show_behavior`
    pub show_behavior: ShowBehavior,
    /// The optimizer passes for the plans of subqueries.
    pub optimizer: OptimizerOptions,
    /// what files can be read and written, with `enable_external_access` off
    pub file_access: Option<crate::execution::file_system::FileAccess>,
    /// `error_on_division_by_zero`
    pub error_on_division_by_zero: bool,
    /// `ieee_floating_point_ops`
    pub ieee_floating_point_ops: bool,
}

/// What `SHOW name` looks for (`ShowBehaviorType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowBehavior {
    Auto,
    Setting,
    Table,
}

impl Default for BindOptions {
    fn default() -> Self {
        Self {
            transaction_id: 0,
            default_order: OrderType::Ascending,
            default_null_order: DefaultNullOrder::NullsLast,
            single_arrow_lambdas: false,
            pivot_limit: 100_000,
            scalar_subquery_error: true,
            show_behavior: ShowBehavior::Auto,
            optimizer: OptimizerOptions::default(),
            file_access: None,
            error_on_division_by_zero: true,
            ieee_floating_point_ops: true,
        }
    }
}

impl BindOptions {
    /// `DBConfig::ResolveOrder` + `ResolveNullOrder`: an ORDER BY term's order and NULL order,
    /// filling in the defaults.
    pub fn resolve_order(&self, order: ast::OrderType, nulls: ast::NullOrder) -> OrderModifiers {
        let order = match order {
            ast::OrderType::Ascending => OrderType::Ascending,
            ast::OrderType::Descending => OrderType::Descending,
            ast::OrderType::Default => self.default_order,
        };
        let nulls = match nulls {
            ast::NullOrder::NullsFirst => NullOrder::NullsFirst,
            ast::NullOrder::NullsLast => NullOrder::NullsLast,
            ast::NullOrder::Default => match (self.default_null_order, order) {
                (DefaultNullOrder::NullsFirst, _) => NullOrder::NullsFirst,
                (DefaultNullOrder::NullsLast, _) => NullOrder::NullsLast,
                (DefaultNullOrder::NullsFirstOnAscLastOnDesc, OrderType::Ascending)
                | (DefaultNullOrder::NullsLastOnAscFirstOnDesc, OrderType::Descending) => NullOrder::NullsFirst,
                _ => NullOrder::NullsLast,
            },
        };
        OrderModifiers { order, nulls }
    }
}

/// The parameters of the prepared statement being bound (`BoundParameterMap`).
#[derive(Debug, Default)]
pub struct ParameterValues {
    /// lowercase name -> value; without values, parameters bind as NULLs, to check a statement
    /// when it is prepared
    pub values: Option<HashMap<String, Value>>,
    /// the parameters the statement refers to (`named_param_map`), in order of appearance
    pub seen: Mutex<Vec<String>>,
}

/// How column references bind while a select list or HAVING clause is being bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnMode {
    /// Columns bind to the FROM clause, and are noted: with a grouping they are an error.
    SelectList,
    /// Columns that are not groups name aliases or are an error (`HavingBinder`).
    Having,
}

/// The grouping side of `BaseSelectBinder` and `BoundSelectNode`.
#[derive(Debug)]
struct AggregateBindState {
    group_index: usize,
    aggregate_index: usize,
    groupings_index: usize,
    /// the GROUPING calls, as the groups each one reports on (`grouping_functions`)
    grouping_functions: Vec<Vec<usize>>,
    /// (qualified, alias-free) group expression -> group (`group_map`)
    group_map: Vec<(ast::Expr, usize)>,
    /// alias or select list position -> group (`group_alias_map`)
    group_alias_map: Vec<(String, usize)>,
    groups: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    /// identifies each entry of `aggregates`, so that equal aggregates are computed once
    aggregate_keys: Vec<String>,
    inside_aggregate: bool,
    mode: ColumnMode,
    /// the columns bound outside of aggregates (`bound_columns`)
    bound_columns: Vec<String>,
    bound_aggregate: bool,
    /// `GROUP BY ALL`
    force_aggregates: bool,
    /// the groups that are collated, with the `first(x)` aggregate that gives their value as it
    /// was (`collated_groups`)
    collated_groups: Vec<(usize, usize)>,
    /// how many grouping sets there are (`grouping_sets.size()`)
    grouping_set_count: usize,
}

/// Binds statements (`Binder`).
/// The value of a constant, as the binder makes it.
pub(crate) fn literal_value(literal: &duckrs_parser::ast::Literal) -> Result<Value> {
    expression::literal_to_value(literal)
}

pub struct Binder<'a> {
    catalog: &'a Catalog,
    pub options: BindOptions,
    bind_context: BindContext,
    next_table_index: usize,
    /// The COPY being bound is about JSON.
    copy_json: bool,
    /// Binding what a table function was given: `TableFunctionBinder` reads a name as its text.
    table_function_argument: bool,
    /// Numbers the subqueries in FROM without an alias.
    unnamed_subquery_index: usize,
    /// The grouping of the select node being bound; `None` where aggregates are not allowed.
    aggregates: Option<AggregateBindState>,
    /// The error for an aggregate where it is not allowed (`UnsupportedAggregateMessage`).
    aggregate_error: &'static str,
    pub session: Option<Arc<SessionValues>>,
    lambda_scopes: Vec<lambda::LambdaScope>,
    /// The windows of the SELECT clause being bound, when it may have any here.
    windows: Option<window::WindowBindState>,
    window_error: &'static str,
    inside_window: bool,
    /// The select list aliases the expression being bound may refer to: all of them in WHERE,
    /// the earlier ones in the select list (`ColumnAliasBinder`, `SelectBinder`).
    alias_fallback: Vec<(String, duckrs_parser::ast::Expr)>,
    /// While a select list entry binds: the aliases of the entries from it on, which it can't
    /// refer to yet (`SelectBinder::TryResolveAliasReference`).
    later_select_aliases: Option<Vec<String>>,
    /// The select list aliases of the queries around this subquery, innermost last, with their
    /// columns qualified, and whether they come from a select list (`ExpressionBinder::BindInEnclosingScope`).
    outer_aliases: Vec<(String, duckrs_parser::ast::Expr, bool)>,
    /// The left sides of the joins whose right side is being bound. Their columns are out of
    /// reach until LATERAL exists, which is a missing feature and not a bad column.
    left_of_join: Vec<BindContext>,
    /// the table indexes of the relations an outer join NULL-extends (`Binding::IsNullExtended`)
    pub(super) null_extended: std::collections::HashSet<usize>,
    /// binding the argument of a TRY or HAS_ERROR, where aggregates and windows are refused
    pub(super) inside_try: bool,
    /// binding the correlated columns of a subquery (`depth > 0` in C++'s binders)
    pub(super) binding_correlated: bool,
    /// The views being bound, innermost last (`bound_views`).
    bound_views: Vec<Arc<ViewCatalogEntry>>,
    /// The schema unqualified tables are looked up in before the default one: the schema of the
    /// view being bound (`GetSearchPath`).
    search_schema: Option<String>,
    /// the database files `read_duckdb` opened, by the names its query gives them
    foreign_databases: Vec<read_duckdb::ForeignDatabase>,
    /// the tables and views (schema and name) that what is bound reads, for the dependencies of a view
    read_entries: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// the subquery being bound is the columns of another, as a row for `(a, b) IN (...)`
    row_subquery: bool,
    /// The table macros being bound, innermost last (`bound_macros`).
    bound_macros: Vec<Arc<MacroCatalogEntry>>,
    /// `Binder::macro_binding`: the parameters of a macro being created, and what they bind as.
    macro_arguments: Vec<(String, ast::Expr)>,
    /// How many scalar macros the expression being bound is nested in.
    macro_depth: usize,
    /// how many UNNESTs a user wrote the expression being bound is inside (`UnnestBinder::unnest_level`)
    unnest_level: usize,
    /// The parameters of the prepared statement being bound, if it is one.
    pub parameters: Option<Arc<ParameterValues>>,
    /// The CTEs in scope, innermost last, with how many outer contexts they can see.
    ctes: Vec<(String, Arc<ast::CteInfo>, usize)>,
    /// The CTEs whose definition is being bound, which cannot refer to themselves.
    ctes_being_bound: Vec<String>,
    /// What the queries around a subquery can see, innermost last.
    outer_contexts: Vec<BindContext>,
    /// The columns of those queries this subquery uses. Binders of the queries inside of it that
    /// are not subqueries themselves share the list.
    correlated: Arc<Mutex<Vec<CorrelatedColumn>>>,
    /// The recursive CTEs whose recursive part is being bound, innermost last.
    working_tables: Vec<WorkingTable>,
    /// RETURNING of a MERGE can read `merge_action`
    merge_action_column: bool,
    /// the aggregate of an outer query's columns that stopped this binder
    pending_outer_aggregate: Option<ast::FunctionExpr>,
}

/// What a recursive CTE is inside of its recursive part.
#[derive(Debug, Clone)]
struct WorkingTable {
    name: String,
    names: Vec<String>,
    types: Vec<LogicalType>,
    rows: Arc<Mutex<ColumnDataCollection>>,
    /// what `recurring.name` reads
    recurring: Arc<Mutex<ColumnDataCollection>>,
    /// whether the recursive part reads it
    used: Arc<AtomicBool>,
    /// a transition table of a trigger body, which a CTE of the body can shadow
    transition: bool,
}

/// A column of a query around a subquery (`CorrelatedColumnInfo`).
#[derive(Debug, Clone)]
pub struct CorrelatedColumn {
    pub binding: ColumnBinding,
    /// what the query around it calls the column
    pub names: Vec<String>,
}

impl<'a> Binder<'a> {
    /// `OpenerFileSystem::VerifyCanAccessFile`: with `enable_external_access` off, only the
    /// allowed paths and directories can be read or written.
    pub(crate) fn check_file_access(&self, path: &str) -> Result<()> {
        match &self.options.file_access {
            Some(access) => access.check(path, false),
            None => Ok(()),
        }
    }

    pub fn new(catalog: &'a Catalog) -> Self {
        Self::with_options(catalog, BindOptions::default())
    }

    pub fn with_options(catalog: &'a Catalog, options: BindOptions) -> Self {
        let mut binder = Self {
            catalog,
            options,
            bind_context: BindContext::default(),
            next_table_index: 0,
            copy_json: false,
            table_function_argument: false,
            unnamed_subquery_index: 1,
            aggregates: None,
            aggregate_error: "Aggregate functions are not supported here",
            session: None,
            lambda_scopes: Vec::new(),
            windows: None,
            window_error: "Window functions are not supported here",
            inside_window: false,
            alias_fallback: Vec::new(),
            later_select_aliases: None,
            outer_aliases: Vec::new(),
            left_of_join: Vec::new(),
            null_extended: std::collections::HashSet::new(),
            inside_try: false,
            binding_correlated: false,
            bound_views: Vec::new(),
            search_schema: None,
            foreign_databases: Vec::new(),
            read_entries: Arc::default(),
            row_subquery: false,
            bound_macros: Vec::new(),
            macro_arguments: Vec::new(),
            macro_depth: 0,
            unnest_level: 0,
            parameters: None,
            ctes: Vec::new(),
            ctes_being_bound: Vec::new(),
            outer_contexts: Vec::new(),
            correlated: Arc::default(),
            working_tables: Vec::new(),
            merge_action_column: false,
            pending_outer_aggregate: None,
        };
        // the transition tables of the trigger whose body this binds
        if let Some(scope) = crate::planner::trigger_scope::current() {
            for table in &scope.tables {
                binder.working_tables.push(WorkingTable {
                    name: table.name.clone(),
                    names: table.names.clone(),
                    types: table.types.clone(),
                    rows: table.rows.clone(),
                    recurring: table.rows.clone(),
                    used: Arc::new(AtomicBool::new(false)),
                    transition: true,
                });
            }
        }
        binder
    }

    /// `Binder::BindView` + `BindCreateViewInfo`: checks the query of a view being created.
    pub fn bind_create_view(
        &mut self,
        query: &ast::SelectStatement,
        aliases: &[String],
        schema: &str,
    ) -> Result<Vec<(String, String)>> {
        // a DML or COPY body would run each time the view is read
        for (_, cte) in &query.node.cte_map.0 {
            match cte.query.kind {
                ast::QueryNodeKind::Insert(_) | ast::QueryNodeKind::Update(_) | ast::QueryNodeKind::Delete(_) => {
                    return Err(binder_error(
                        "DML statements (INSERT/UPDATE/DELETE) are not allowed as CTE bodies inside a VIEW",
                    ));
                }
                ast::QueryNodeKind::Copy(_) => {
                    return Err(binder_error(
                        "COPY statements are not allowed as CTE bodies inside a VIEW",
                    ));
                }
                _ => {}
            }
        }
        self.search_schema = Some(schema.to_string());
        let bound = self.bind_select_statement(query)?;
        if aliases.len() > bound.names.len() {
            return Err(binder_error("More VIEW aliases than columns in query result"));
        }
        let mut read = self.read_entries.lock().expect("not poisoned").clone();
        read.dedup();
        Ok(read)
    }

    /// That what is bound reads the table or view `name` of `schema`.
    fn note_read(&self, schema: &str, name: &str) {
        let mut read = self.read_entries.lock().expect("not poisoned");
        if !read.iter().any(|(s, n)| s == schema && n == name) {
            read.push((schema.to_string(), name.to_string()));
        }
    }

    /// `error`, unless the column it did not find is one a LATERAL join could read.
    fn lateral_error(&self, names: &[String], error: Error) -> Error {
        let names_column = |context: &BindContext| match names {
            [column] => !context.matching_bindings(column).is_empty(),
            [table, column] => context
                .bindings
                .iter()
                .any(|b| b.alias.eq_ignore_ascii_case(table) && b.column_index(column).is_some()),
            _ => false,
        };
        if self.outer_contexts.iter().any(names_column) {
            crate::unported("correlated subqueries")
        } else if self.left_of_join.iter().any(names_column) {
            crate::unported("LATERAL joins")
        } else {
            error
        }
    }

    /// `Binder::CreateBinder` for a query inside of this one: it numbers tables on from here and
    /// knows what is being bound around it, but none of the columns.
    fn child_binder(&self) -> Binder<'a> {
        let mut binder = Binder::with_options(self.catalog, self.options.clone());
        binder.next_table_index = self.next_table_index;
        binder.bound_views = self.bound_views.clone();
        binder.bound_macros = self.bound_macros.clone();
        binder.macro_arguments = self.macro_arguments.clone();
        binder.search_schema = self.search_schema.clone();
        binder.foreign_databases = self.foreign_databases.clone();
        binder.read_entries = self.read_entries.clone();
        binder.parameters = self.parameters.clone();
        binder.ctes = self.ctes.clone();
        binder.ctes_being_bound = self.ctes_being_bound.clone();
        binder.outer_contexts = self.outer_contexts.clone();
        binder.outer_aliases = self.outer_aliases.clone();
        binder.session = self.session.clone();
        binder.correlated = self.correlated.clone();
        binder.working_tables = self.working_tables.clone();
        binder
    }

    /// `Binder::GenerateTableIndex`.
    fn generate_table_index(&mut self) -> usize {
        let index = self.next_table_index;
        self.next_table_index += 1;
        index
    }

    /// `Planner::CreatePlan`: binds and plans one statement.
    pub fn bind(&mut self, statement: &Statement) -> Result<BoundStatement> {
        let mut bound = self.bind_statement(statement)?;
        crate::planner::subquery::flatten_subqueries(&mut bound.plan, &mut self.next_table_index);
        Ok(bound)
    }

    fn bind_statement(&mut self, statement: &Statement) -> Result<BoundStatement> {
        match statement {
            Statement::Select(select) => self.bind_select_statement(select),
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::Delete(delete) => self.bind_delete(delete),
            Statement::Update(update) => self.bind_update(update),
            Statement::Merge(merge) => self.bind_merge(merge),
            Statement::CreateTable(info) => self.bind_create_table(info),
            Statement::Copy(info) => self.bind_copy(info),
            // `Binder::Bind(CallStatement)`
            Statement::Call(function) => {
                let select = duckrs_parser::parse_ast(&format!("SELECT * FROM {function}"))?.remove(0);
                let Statement::Select(select) = select else {
                    unreachable!("this text is a SELECT");
                };
                self.bind_select_statement(&select)
            }
            other => Err(crate::unported(format!("{} statements", statement_kind(other)))),
        }
    }
}

/// A short name for a statement kind, for "not supported" errors.
fn statement_kind(statement: &Statement) -> String {
    let debug = format!("{statement:?}");
    let name: String = debug.chars().take_while(|c| c.is_alphanumeric()).collect();
    name.to_uppercase()
}

/// Binds and resolves an expression that has no columns, e.g. a VALUES entry.
pub fn bind_constant_expression(catalog: &Catalog, expr: &duckrs_parser::ast::Expr) -> Result<BoundExpr> {
    let mut binder = Binder::new(catalog);
    binder.bind_expression(expr)
}

impl Binder<'_> {
    /// An expression without a FROM clause, as the value of a SET.
    pub fn bind_value_expression(&mut self, expr: &duckrs_parser::ast::Expr) -> Result<BoundExpr> {
        self.bind_expression(expr)
    }
}

// Ported from `ExpressionBinder::PushCollation` and the collation handling of
// `BoundComparisonExpression::TryBindComparison` @ rust-port-baseline
impl Binder<'_> {
    /// The `default_collation` of the session.
    pub(crate) fn default_collation(&self) -> String {
        self.session
            .as_ref()
            .map(|s| s.default_collation.clone())
            .unwrap_or_default()
    }

    /// `expr` with the collation of `ty` applied (`PushCollation`).
    pub(crate) fn collate(&self, expr: BoundExpr, ty: &LogicalType) -> Result<BoundExpr> {
        let default = self.default_collation();
        Ok(crate::planner::collation::push_collation(
            expr,
            ty,
            &default,
            crate::planner::collation::CollationType::AllCollations,
        )?
        .0)
    }

    /// The VARCHARs `exprs`, compared with each other, with the collation of their types
    /// (`originals`, before the casts) applied; an error if two of them disagree.
    pub(crate) fn collate_compared(&self, originals: &[LogicalType], exprs: Vec<BoundExpr>) -> Result<Vec<BoundExpr>> {
        // `ComparisonBind`: what is not a VARCHAR compares with the collation of its type (a TIMETZ
        // by its instant, an INTERVAL normalized, the fields and elements of a nested value)
        if exprs
            .iter()
            .any(|e| e.return_type.id() != duckrs_common::types::LogicalTypeId::Varchar)
        {
            // the collations of the VARCHARs nested in either go to both (the comparison type)
            use crate::planner::collation::{FunctionCollations, apply_collations, extract_collations};
            let mut collations = FunctionCollations::default();
            for ty in originals.iter().chain(exprs.iter().map(|e| &e.return_type)) {
                extract_collations(ty, &mut collations)?;
            }
            return exprs
                .into_iter()
                .map(|e| {
                    let ty = apply_collations(&e.return_type, &collations);
                    self.collate(e, &ty)
                })
                .collect();
        }
        let mut collation: Option<String> = None;
        for ty in originals {
            let combined = crate::planner::collation::combined_collation(
                &LogicalType::VARCHAR.with_collation(collation.as_deref()),
                ty,
            )?;
            collation = combined;
        }
        let ty = LogicalType::VARCHAR.with_collation(collation.as_deref());
        exprs.into_iter().map(|e| self.collate(e, &ty)).collect()
    }
}

impl Binder<'_> {
    /// `storage_compatibility_version`: what serialized objects are written for.
    pub(super) fn storage_compatibility(&self) -> duckrs_common::serializer::StorageVersion {
        use duckrs_common::serializer::StorageVersion;
        self.session
            .as_ref()
            .and_then(|session| (session.setting)("storage_compatibility_version").ok())
            .and_then(|value| value.try_to_string().ok())
            .and_then(|name| StorageVersion::from_name(&name))
            .unwrap_or(StorageVersion::DEFAULT)
    }
}
