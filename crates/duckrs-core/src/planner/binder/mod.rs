// Ported from duckdb/src/planner/{binder,bind_context,table_binding,planner}.cpp @ rust-port-baseline
//
//! The binder: resolves the names in a parsed statement against the catalog, types its
//! expressions and plans it as a tree of logical operators.

mod expression;
mod join;
mod statements;

pub use statements::schema_of;

use super::expression::{BoundAggregate, BoundExpr, ColumnBinding};
use super::logical::LogicalOperator;
use crate::catalog::{Catalog, ViewCatalogEntry};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType};
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;
use duckrs_parser::ast::Statement;
use std::sync::Arc;

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
    pub return_type: StatementReturnType,
}

/// The columns a FROM clause entry makes visible (`TableBinding`).
#[derive(Debug, Clone)]
pub struct Binding {
    pub alias: String,
    pub table_index: usize,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
}

impl Binding {
    fn column_index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n.eq_ignore_ascii_case(name))
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
#[derive(Debug, Default)]
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
            .filter(|b| b.column_index(column).is_some())
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
                    let quoted = duckrs_parser::ast::sql_identifier(name);
                    return Err(binder_error(format!(
                        "Ambiguous reference to column name {name} (use: '{}.{quoted}' or '{}.{quoted}')",
                        previous.alias, binding.alias
                    )));
                }
                found = Some((binding, index));
            }
        }
        Ok(found)
    }

    /// Binds an unqualified column (`BindContext::GetMatchingBinding` + `TableBinding::Bind`).
    fn bind_column(&self, name: &str) -> Result<(ColumnBinding, LogicalType, String)> {
        match self.matching_binding(name)? {
            Some((binding, index)) => Ok((
                ColumnBinding::new(binding.table_index, index),
                binding.types[index].clone(),
                binding.names[index].clone(),
            )),
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
    fn bindings_of(&self, table: &str) -> Result<Vec<&Binding>> {
        let matching: Vec<&Binding> = self
            .bindings
            .iter()
            .filter(|b| b.alias.eq_ignore_ascii_case(table))
            .collect();
        if matching.is_empty() {
            let scores = self
                .bindings
                .iter()
                .map(|b| (b.alias.clone(), similarity_rating(&b.alias, table)))
                .collect();
            return Err(binder_error(format!(
                "Referenced table \"{table}\" not found!{}",
                candidates_message(&top_n_strings(scores), "Candidate tables")
            )));
        }
        Ok(matching)
    }

    /// `AmbiguityException`, for bindings with one alias.
    fn ambiguous_table(table: &str) -> Error {
        binder_error(format!(
            "Ambiguous reference to table \"{table}\" (duplicate alias \"{table}\", explicitly alias one of the \
             tables using \"AS my_alias\")"
        ))
    }

    /// Binds `table.column` (`BindContext::GetBinding(alias, column)`): of the bindings with this
    /// alias, the one with the column.
    fn bind_qualified_column(&self, table: &str, name: &str) -> Result<(ColumnBinding, LogicalType, String)> {
        let matching = self.bindings_of(table)?;
        let mut with_column = matching.iter().filter(|b| b.column_index(name).is_some());
        let Some(binding) = with_column.next() else {
            return Err(binder_error(format!(
                "Table {} does not have a column named {name}",
                matching[0].alias
            )));
        };
        if with_column.next().is_some() {
            return Err(Self::ambiguous_table(table));
        }
        let index = binding.column_index(name).expect("filtered above");
        Ok((
            ColumnBinding::new(binding.table_index, index),
            binding.types[index].clone(),
            binding.names[index].clone(),
        ))
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

/// The settings that change how statements bind.
#[derive(Debug, Clone, Copy)]
pub struct BindOptions {
    /// `default_order`
    pub default_order: OrderType,
    /// `default_null_order`
    pub default_null_order: DefaultNullOrder,
}

impl Default for BindOptions {
    fn default() -> Self {
        Self {
            default_order: OrderType::Ascending,
            default_null_order: DefaultNullOrder::NullsLast,
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
}

/// Binds statements (`Binder`).
pub struct Binder<'a> {
    catalog: &'a Catalog,
    pub options: BindOptions,
    bind_context: BindContext,
    next_table_index: usize,
    /// Numbers the subqueries in FROM without an alias.
    unnamed_subquery_index: usize,
    /// The grouping of the select node being bound; `None` where aggregates are not allowed.
    aggregates: Option<AggregateBindState>,
    /// The error for an aggregate where it is not allowed (`UnsupportedAggregateMessage`).
    aggregate_error: &'static str,
    /// The select list aliases the expression being bound may refer to: all of them in WHERE,
    /// the earlier ones in the select list (`ColumnAliasBinder`, `SelectBinder`).
    alias_fallback: Vec<(String, duckrs_parser::ast::Expr)>,
    /// The left sides of the joins whose right side is being bound. Their columns are out of
    /// reach until LATERAL exists, which is a missing feature and not a bad column.
    left_of_join: Vec<BindContext>,
    /// The views being bound, innermost last (`bound_views`).
    bound_views: Vec<Arc<ViewCatalogEntry>>,
    /// The schema unqualified tables are looked up in before the default one: the schema of the
    /// view being bound (`GetSearchPath`).
    search_schema: Option<String>,
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self::with_options(catalog, BindOptions::default())
    }

    pub fn with_options(catalog: &'a Catalog, options: BindOptions) -> Self {
        Self {
            catalog,
            options,
            bind_context: BindContext::default(),
            next_table_index: 0,
            unnamed_subquery_index: 1,
            aggregates: None,
            aggregate_error: "Aggregate functions are not supported here",
            alias_fallback: Vec::new(),
            left_of_join: Vec::new(),
            bound_views: Vec::new(),
            search_schema: None,
        }
    }

    /// `Binder::BindView` + `BindCreateViewInfo`: checks the query of a view being created.
    pub fn bind_create_view(&mut self, query: &ast::SelectStatement, aliases: &[String], schema: &str) -> Result<()> {
        self.search_schema = Some(schema.to_string());
        let bound = self.bind_select_statement(query)?;
        if aliases.len() > bound.names.len() {
            return Err(binder_error("More VIEW aliases than columns in query result"));
        }
        Ok(())
    }

    /// `error`, unless the column it did not find is one a LATERAL join could read.
    fn lateral_error(&self, names: &[String], error: Error) -> Error {
        let lateral = self.left_of_join.iter().any(|context| match names {
            [column] => !context.matching_bindings(column).is_empty(),
            [table, column] => context
                .bindings
                .iter()
                .any(|b| b.alias.eq_ignore_ascii_case(table) && b.column_index(column).is_some()),
            _ => false,
        });
        if lateral {
            crate::unported("LATERAL joins")
        } else {
            error
        }
    }

    /// `Binder::GenerateTableIndex`.
    fn generate_table_index(&mut self) -> usize {
        let index = self.next_table_index;
        self.next_table_index += 1;
        index
    }

    /// `Planner::CreatePlan`: binds and plans one statement.
    pub fn bind(&mut self, statement: &Statement) -> Result<BoundStatement> {
        match statement {
            Statement::Select(select) => self.bind_select_statement(select),
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::CreateTable(info) => self.bind_create_table(info),
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
