// Ported from duckdb/src/planner/{binder,bind_context,table_binding,planner}.cpp @ rust-port-baseline
//
//! The binder: resolves the names in a parsed statement against the catalog, types its
//! expressions and plans it as a tree of logical operators.

mod expression;
mod statements;

use super::expression::{BoundAggregate, BoundExpr, ColumnBinding};
use super::logical::LogicalOperator;
use crate::catalog::Catalog;
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType};
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;
use duckrs_parser::ast::Statement;

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

/// The bindings of the current query level (`BindContext`), in FROM clause order.
#[derive(Debug, Default)]
pub struct BindContext {
    pub bindings: Vec<Binding>,
}

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

impl BindContext {
    /// `BindContext::AddBinding`, rejecting a duplicate alias like DuckDB.
    pub fn add_binding(&mut self, binding: Binding) -> Result<()> {
        if self
            .bindings
            .iter()
            .any(|b| b.alias.eq_ignore_ascii_case(&binding.alias))
        {
            return Err(binder_error(format!("Duplicate alias \"{}\" in query!", binding.alias)));
        }
        self.bindings.push(binding);
        Ok(())
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

    /// Binds an unqualified column (`BindContext::GetMatchingBinding` + `TableBinding::Bind`).
    fn bind_column(&self, name: &str) -> Result<(ColumnBinding, LogicalType, String)> {
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
        match found {
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

    /// Binds `table.column`.
    fn bind_qualified_column(&self, table: &str, name: &str) -> Result<(ColumnBinding, LogicalType, String)> {
        let Some(binding) = self.bindings.iter().find(|b| b.alias.eq_ignore_ascii_case(table)) else {
            let scores = self
                .bindings
                .iter()
                .map(|b| (b.alias.clone(), similarity_rating(&b.alias, table)))
                .collect();
            return Err(binder_error(format!(
                "Referenced table \"{table}\" not found!{}",
                candidates_message(&top_n_strings(scores), "Candidate tables")
            )));
        };
        match binding.column_index(name) {
            Some(index) => Ok((
                ColumnBinding::new(binding.table_index, index),
                binding.types[index].clone(),
                binding.names[index].clone(),
            )),
            None => Err(binder_error(format!(
                "Table {} does not have a column named {name}",
                binding.alias
            ))),
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
