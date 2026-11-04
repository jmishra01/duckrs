// Ported from duckdb/src/planner/operator/*.cpp and duckdb/src/planner/logical_operator.cpp
// @ rust-port-baseline
//
//! Logical operators (`LogicalOperator` and its subclasses), as an enum. Each operator knows
//! its output types and the column bindings its parents refer to.

use super::expression::{BoundAggregate, BoundExpr, ColumnBinding};
use crate::catalog::{ColumnDefinition, OnCreateConflict, SchemaCatalogEntry, TableCatalogEntry};
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use std::sync::Arc;

#[derive(Debug)]
pub enum LogicalOperatorKind {
    /// `LogicalGet` of a table: scans `column_ids` (indexes into the table's columns).
    Get {
        table: Arc<TableCatalogEntry>,
        table_index: usize,
        column_ids: Vec<usize>,
    },
    /// The `range` and `generate_series` table functions: `count` integers from `start`.
    Range {
        table_index: usize,
        start: i128,
        increment: i128,
        count: u64,
    },
    /// `LogicalEmptyResult`: no rows, with the columns of the plan it replaced.
    EmptyResult {
        types: Vec<LogicalType>,
        bindings: Vec<ColumnBinding>,
    },
    /// `LogicalDummyScan`: a single row with no columns, for a SELECT without FROM.
    DummyScan { table_index: usize },
    /// `LogicalExpressionGet`: rows of expressions (VALUES), evaluated once per row.
    ExpressionGet {
        table_index: usize,
        types: Vec<LogicalType>,
        expressions: Vec<Vec<BoundExpr>>,
    },
    /// `LogicalFilter`: keeps the rows for which all expressions are true.
    Filter { expressions: Vec<BoundExpr> },
    /// `LogicalAggregate`: one row per distinct `groups`, with the `aggregates` of its rows.
    Aggregate {
        group_index: usize,
        aggregate_index: usize,
        groups: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
    },
    /// `LogicalOrder`: sorts by the expressions, in order.
    Order { orders: Vec<BoundOrderByNode> },
    /// `LogicalLimit` with constant values: skips `offset` rows, then keeps `limit` rows (all
    /// of them with `None`).
    Limit { limit: Option<u64>, offset: u64 },
    /// `LogicalDistinct` (`DISTINCT`, not `DISTINCT ON`): one row per distinct `targets`.
    Distinct { targets: Vec<BoundExpr> },
    /// `LogicalProjection`.
    Projection {
        table_index: usize,
        expressions: Vec<BoundExpr>,
    },
    /// `LogicalInsert`: appends its child's rows to `table`. `column_index_map[i]` is the child
    /// column that fills table column `i`, or `None` for the column's default (NULL).
    Insert {
        table: Arc<TableCatalogEntry>,
        column_index_map: Vec<Option<usize>>,
    },
    /// `LogicalCreateTable` with a query: creates the table with the child's rows.
    CreateTableAs {
        schema: Arc<SchemaCatalogEntry>,
        name: String,
        columns: Vec<ColumnDefinition>,
        on_conflict: OnCreateConflict,
    },
    /// `LogicalCreateTable`.
    CreateTable {
        schema: Arc<SchemaCatalogEntry>,
        name: String,
        columns: Vec<ColumnDefinition>,
        on_conflict: OnCreateConflict,
    },
}

/// An ORDER BY term with its resolved order and NULL order (`BoundOrderByNode`).
#[derive(Debug, Clone)]
pub struct BoundOrderByNode {
    pub modifiers: OrderModifiers,
    pub expr: BoundExpr,
}

#[derive(Debug)]
pub struct LogicalOperator {
    pub kind: LogicalOperatorKind,
    pub children: Vec<LogicalOperator>,
}

impl LogicalOperator {
    pub fn new(kind: LogicalOperatorKind, children: Vec<LogicalOperator>) -> Self {
        Self { kind, children }
    }

    /// `LogicalOperator::types` after `ResolveOperatorTypes`.
    pub fn types(&self) -> Vec<LogicalType> {
        match &self.kind {
            LogicalOperatorKind::Get { table, column_ids, .. } => {
                column_ids.iter().map(|&i| table.columns[i].ty.clone()).collect()
            }
            LogicalOperatorKind::Range { .. } => vec![LogicalType::BIGINT],
            LogicalOperatorKind::EmptyResult { types, .. } => types.clone(),
            // the dummy scan's single column is never read
            LogicalOperatorKind::DummyScan { .. } => vec![LogicalType::INTEGER],
            LogicalOperatorKind::ExpressionGet { types, .. } => types.clone(),
            LogicalOperatorKind::Aggregate { groups, aggregates, .. } => groups
                .iter()
                .map(|g| g.return_type.clone())
                .chain(aggregates.iter().map(|a| a.return_type.clone()))
                .collect(),
            LogicalOperatorKind::Filter { .. }
            | LogicalOperatorKind::Order { .. }
            | LogicalOperatorKind::Limit { .. }
            | LogicalOperatorKind::Distinct { .. } => self.children[0].types(),
            LogicalOperatorKind::Projection { expressions, .. } => {
                expressions.iter().map(|e| e.return_type.clone()).collect()
            }
            LogicalOperatorKind::Insert { .. }
            | LogicalOperatorKind::CreateTable { .. }
            | LogicalOperatorKind::CreateTableAs { .. } => vec![LogicalType::BIGINT],
        }
    }

    /// `LogicalOperator::GetColumnBindings`.
    pub fn column_bindings(&self) -> Vec<ColumnBinding> {
        let numbered = |table_index: usize, count: usize| -> Vec<ColumnBinding> {
            (0..count).map(|i| ColumnBinding::new(table_index, i)).collect()
        };
        match &self.kind {
            LogicalOperatorKind::Get {
                table_index,
                column_ids,
                ..
            } => numbered(*table_index, column_ids.len()),
            LogicalOperatorKind::EmptyResult { bindings, .. } => bindings.clone(),
            LogicalOperatorKind::DummyScan { table_index } | LogicalOperatorKind::Range { table_index, .. } => {
                numbered(*table_index, 1)
            }
            LogicalOperatorKind::ExpressionGet { table_index, types, .. } => numbered(*table_index, types.len()),
            LogicalOperatorKind::Aggregate {
                group_index,
                aggregate_index,
                groups,
                aggregates,
            } => {
                let mut bindings = numbered(*group_index, groups.len());
                bindings.extend(numbered(*aggregate_index, aggregates.len()));
                bindings
            }
            LogicalOperatorKind::Filter { .. }
            | LogicalOperatorKind::Order { .. }
            | LogicalOperatorKind::Limit { .. }
            | LogicalOperatorKind::Distinct { .. } => self.children[0].column_bindings(),
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            } => numbered(*table_index, expressions.len()),
            LogicalOperatorKind::Insert { .. }
            | LogicalOperatorKind::CreateTable { .. }
            | LogicalOperatorKind::CreateTableAs { .. } => numbered(0, 1),
        }
    }

    /// The expressions of this operator, for passes over the plan.
    pub fn expressions_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            LogicalOperatorKind::Filter { expressions }
            | LogicalOperatorKind::Projection { expressions, .. }
            | LogicalOperatorKind::Distinct { targets: expressions } => expressions.iter_mut().collect(),
            LogicalOperatorKind::Order { orders } => orders.iter_mut().map(|o| &mut o.expr).collect(),
            LogicalOperatorKind::Aggregate { groups, aggregates, .. } => groups
                .iter_mut()
                .chain(
                    aggregates
                        .iter_mut()
                        .flat_map(|a| a.children.iter_mut().chain(a.filter.iter_mut())),
                )
                .collect(),
            LogicalOperatorKind::ExpressionGet { expressions, .. } => expressions.iter_mut().flatten().collect(),
            _ => Vec::new(),
        }
    }
}
