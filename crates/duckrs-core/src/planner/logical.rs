// Ported from duckdb/src/planner/operator/*.cpp and duckdb/src/planner/logical_operator.cpp
// @ rust-port-baseline
//
//! Logical operators (`LogicalOperator` and its subclasses), as an enum. Each operator knows
//! its output types and the column bindings its parents refer to.

use super::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundWindow, ColumnBinding};
use crate::catalog::{ColumnDefinition, OnCreateConflict, SchemaCatalogEntry, TableCatalogEntry, TableConstraint};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::JoinType;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The `column_ids` entry of a scan for the `rowid` pseudo-column (`COLUMN_IDENTIFIER_ROW_ID`).
pub const COLUMN_IDENTIFIER_ROW_ID: usize = usize::MAX;

#[derive(Debug, Clone)]
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
    /// `LogicalWindow`: the columns of the child and those of the `windows`.
    Window {
        table_index: usize,
        windows: Vec<BoundWindow>,
    },
    /// `LogicalAggregate`: one row per distinct `groups`, with the `aggregates` of its rows. With
    /// `grouping_sets` there is such a set of rows per set, where the groups outside of the set
    /// are NULL; without any, all groups form the one set. The `grouping_functions` are the
    /// GROUPING calls, as the groups each one reports on.
    Aggregate {
        group_index: usize,
        aggregate_index: usize,
        groupings_index: usize,
        groups: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        grouping_sets: Vec<BTreeSet<usize>>,
        grouping_functions: Vec<Vec<usize>>,
    },
    /// `LogicalOrder`: sorts by the expressions, in order.
    Order { orders: Vec<BoundOrderByNode> },
    /// `LogicalLimit` with constant values: skips `offset` rows, then keeps `limit` rows (all
    /// of them with `None`).
    Limit { limit: Option<u64>, offset: u64 },
    /// `LogicalDistinct` (`DISTINCT`, not `DISTINCT ON`): one row per distinct `targets`.
    Distinct { targets: Vec<BoundExpr> },
    /// `LogicalUnnest`: a row per element of the lists of `expressions`, next to the child's
    /// columns. A list that runs out gives NULL.
    Unnest {
        table_index: usize,
        expressions: Vec<BoundExpr>,
    },
    /// `LogicalSetOperation` for UNION: the rows of all children.
    Union {
        table_index: usize,
        types: Vec<LogicalType>,
    },
    /// `LogicalSetOperation` for EXCEPT and INTERSECT of the first child with the second. With
    /// `all` they work on bags; without, on the rows of the first child (DISTINCT goes on top).
    ExceptIntersect {
        table_index: usize,
        types: Vec<LogicalType>,
        except: bool,
        all: bool,
    },
    /// `LogicalRecursiveCTE`: the rows of its child, then those of `recursive`, run over and over
    /// on the rows of the last round in `working`, until a round adds nothing.
    RecursiveCte {
        table_index: usize,
        types: Vec<LogicalType>,
        union_all: bool,
        working: Arc<Mutex<ColumnDataCollection>>,
        recursive: Box<LogicalOperator>,
    },
    /// `LogicalCTERef` of a recursive CTE, inside of its recursive part: the rows of the last round.
    WorkingTable {
        table_index: usize,
        types: Vec<LogicalType>,
        working: Arc<Mutex<ColumnDataCollection>>,
    },
    /// A join whose right side reads columns of its left side: `right` runs for each combination
    /// of values of `correlated`, as constants for the columns `bindings`. DuckDB flattens these.
    LateralJoin {
        join_type: JoinType,
        right: Box<LogicalOperator>,
        bindings: Vec<ColumnBinding>,
        /// reads the left child
        correlated: Vec<BoundExpr>,
        /// reads both sides
        predicate: Option<BoundExpr>,
    },
    /// `LogicalProjection`.
    Projection {
        table_index: usize,
        expressions: Vec<BoundExpr>,
    },
    /// `LogicalCrossProduct`: every pair of rows of its two children.
    CrossProduct,
    /// `LogicalComparisonJoin` and `LogicalAnyJoin`: the pairs of rows of its two children with
    /// equal `conditions` for which `predicate`, which sees both sides, holds.
    Join {
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
    },
    /// `LogicalInsert`: appends its child's rows to `table`. `column_index_map[i]` is the child
    /// column that fills table column `i`, or `None` for the column's default (NULL).
    Insert {
        table: Arc<TableCatalogEntry>,
        column_index_map: Vec<Option<usize>>,
        table_index: usize,
        /// Return the rows instead of their count.
        returning: bool,
        on_conflict: Option<Box<OnConflict>>,
    },
    /// `LogicalDelete`: removes the rows whose ids the child's only column holds. Returns their
    /// count, or with `returning` the rows.
    Delete {
        table: Arc<TableCatalogEntry>,
        table_index: usize,
        returning: bool,
    },
    /// `LogicalUpdate`: the child's columns are the new values of `columns`, and then the row id.
    Update {
        table: Arc<TableCatalogEntry>,
        table_index: usize,
        columns: Vec<usize>,
        returning: bool,
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
        constraints: Vec<TableConstraint>,
        on_conflict: OnCreateConflict,
    },
}

/// What an INSERT does about conflicts on keys (`OnConflictInfo`).
#[derive(Debug, Clone)]
pub struct OnConflict {
    /// DO UPDATE; otherwise DO NOTHING.
    pub do_update: bool,
    /// The columns of the UNIQUEs and PRIMARY KEYs it catches. The rest raise their errors.
    pub targets: Vec<Vec<usize>>,
    pub set_columns: Vec<usize>,
    /// These and the condition read the columns of the table, then those of `excluded`.
    pub set_expressions: Vec<BoundExpr>,
    pub condition: Option<BoundExpr>,
}

/// An equality between the two sides of a join (`JoinCondition`).
#[derive(Debug, Clone)]
pub struct JoinCondition {
    /// reads the left child
    pub left: BoundExpr,
    /// reads the right child
    pub right: BoundExpr,
    /// `IS NOT DISTINCT FROM`: NULLs are equal
    pub null_safe: bool,
}

/// An ORDER BY term with its resolved order and NULL order (`BoundOrderByNode`).
#[derive(Debug, Clone)]
pub struct BoundOrderByNode {
    pub modifiers: OrderModifiers,
    pub expr: BoundExpr,
}

#[derive(Debug, Clone)]
pub struct LogicalOperator {
    pub kind: LogicalOperatorKind,
    pub children: Vec<LogicalOperator>,
}

impl LogicalOperator {
    /// Replaces the correlated columns of `columns`, here and in the operators below.
    pub fn substitute_columns(&mut self, columns: &[(ColumnBinding, Value)]) {
        for child in &mut self.children {
            child.substitute_columns(columns);
        }
        for expr in self.expressions_mut() {
            expr.substitute_columns(columns);
        }
        match &mut self.kind {
            LogicalOperatorKind::Join { conditions, .. } => {
                for condition in conditions {
                    condition.left.substitute_columns(columns);
                    condition.right.substitute_columns(columns);
                }
            }
            LogicalOperatorKind::RecursiveCte { recursive: inner, .. }
            | LogicalOperatorKind::LateralJoin { right: inner, .. } => inner.substitute_columns(columns),
            _ => {}
        }
    }

    /// Every expression of this plan, in the operators below too.
    fn visit_expressions(&mut self, f: &mut impl FnMut(&mut BoundExpr)) {
        for child in &mut self.children {
            child.visit_expressions(f);
        }
        for expr in self.expressions_mut() {
            expr.walk_mut(f);
        }
        match &mut self.kind {
            LogicalOperatorKind::Join { conditions, .. } => {
                for condition in conditions {
                    condition.left.walk_mut(f);
                    condition.right.walk_mut(f);
                }
            }
            LogicalOperatorKind::RecursiveCte { recursive: inner, .. }
            | LogicalOperatorKind::LateralJoin { right: inner, .. } => inner.visit_expressions(f),
            _ => {}
        }
    }

    /// Forgets what the subqueries of this plan ran, as their input changed.
    pub fn clear_subquery_results(&mut self) {
        self.visit_expressions(&mut |e| {
            if let BoundExprKind::Subquery { subquery, .. } = &e.kind {
                subquery.results.lock().clear();
                subquery.plan.clone().clear_subquery_results();
            }
        });
    }

    /// `Expression::IsVolatile` for a plan: whether running it twice can differ.
    pub fn is_volatile(&self) -> bool {
        let mut volatile = false;
        self.clone().visit_expressions(&mut |e| match &e.kind {
            BoundExprKind::Subquery { subquery, .. } => volatile |= subquery.plan.is_volatile(),
            _ => volatile |= e.is_volatile(),
        });
        volatile
    }

    pub fn new(kind: LogicalOperatorKind, children: Vec<LogicalOperator>) -> Self {
        Self { kind, children }
    }

    /// `LogicalOperator::types` after `ResolveOperatorTypes`.
    pub fn types(&self) -> Vec<LogicalType> {
        match &self.kind {
            LogicalOperatorKind::Get { table, column_ids, .. } => column_ids
                .iter()
                .map(|&i| {
                    if i == COLUMN_IDENTIFIER_ROW_ID {
                        LogicalType::BIGINT
                    } else {
                        table.columns[i].ty.clone()
                    }
                })
                .collect(),
            LogicalOperatorKind::Range { .. } => vec![LogicalType::BIGINT],
            LogicalOperatorKind::EmptyResult { types, .. } => types.clone(),
            // the dummy scan's single column is never read
            LogicalOperatorKind::DummyScan { .. } => vec![LogicalType::INTEGER],
            LogicalOperatorKind::ExpressionGet { types, .. } => types.clone(),
            LogicalOperatorKind::Window { windows, .. } => {
                let mut types = self.children[0].types();
                types.extend(windows.iter().map(|w| w.return_type.clone()));
                types
            }
            LogicalOperatorKind::Aggregate {
                groups,
                aggregates,
                grouping_functions,
                ..
            } => groups
                .iter()
                .map(|g| g.return_type.clone())
                .chain(aggregates.iter().map(|a| a.return_type.clone()))
                .chain(grouping_functions.iter().map(|_| LogicalType::BIGINT))
                .collect(),
            LogicalOperatorKind::Filter { .. }
            | LogicalOperatorKind::Order { .. }
            | LogicalOperatorKind::Limit { .. }
            | LogicalOperatorKind::Distinct { .. } => self.children[0].types(),
            LogicalOperatorKind::Projection { expressions, .. } => {
                expressions.iter().map(|e| e.return_type.clone()).collect()
            }
            LogicalOperatorKind::Delete {
                table, returning: true, ..
            }
            | LogicalOperatorKind::Insert {
                table, returning: true, ..
            }
            | LogicalOperatorKind::Update {
                table, returning: true, ..
            } => table.types(),
            LogicalOperatorKind::Delete { .. } | LogicalOperatorKind::Update { .. } => vec![LogicalType::BIGINT],
            LogicalOperatorKind::Union { types, .. }
            | LogicalOperatorKind::ExceptIntersect { types, .. }
            | LogicalOperatorKind::RecursiveCte { types, .. }
            | LogicalOperatorKind::WorkingTable { types, .. } => types.clone(),
            LogicalOperatorKind::LateralJoin { right, .. } => {
                let mut types = self.children[0].types();
                types.extend(right.types());
                types
            }
            LogicalOperatorKind::Unnest { expressions, .. } => {
                let mut types = self.children[0].types();
                types.extend(
                    expressions
                        .iter()
                        .map(|e| e.return_type.child_type().cloned().unwrap_or(LogicalType::SQLNULL)),
                );
                types
            }
            LogicalOperatorKind::CrossProduct | LogicalOperatorKind::Join { .. } => {
                self.join_output(|child| child.types())
            }
            LogicalOperatorKind::Insert { .. }
            | LogicalOperatorKind::CreateTable { .. }
            | LogicalOperatorKind::CreateTableAs { .. } => vec![LogicalType::BIGINT],
        }
    }

    /// What a join outputs of its children: both, or only one for SEMI and ANTI joins.
    fn join_output<T>(&self, of_child: impl Fn(&LogicalOperator) -> Vec<T>) -> Vec<T> {
        match self.kind {
            LogicalOperatorKind::Join {
                join_type: JoinType::Semi | JoinType::Anti,
                ..
            } => of_child(&self.children[0]),
            LogicalOperatorKind::Join {
                join_type: JoinType::RightSemi | JoinType::RightAnti,
                ..
            } => of_child(&self.children[1]),
            _ => {
                let mut output = of_child(&self.children[0]);
                output.extend(of_child(&self.children[1]));
                output
            }
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
            LogicalOperatorKind::Window { table_index, windows } => {
                let mut bindings = self.children[0].column_bindings();
                bindings.extend(numbered(*table_index, windows.len()));
                bindings
            }
            LogicalOperatorKind::Aggregate {
                group_index,
                aggregate_index,
                groupings_index,
                groups,
                aggregates,
                grouping_functions,
                ..
            } => {
                let mut bindings = numbered(*group_index, groups.len());
                bindings.extend(numbered(*aggregate_index, aggregates.len()));
                bindings.extend(numbered(*groupings_index, grouping_functions.len()));
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
            LogicalOperatorKind::Delete {
                table,
                table_index,
                returning,
            }
            | LogicalOperatorKind::Insert {
                table,
                table_index,
                returning,
                ..
            }
            | LogicalOperatorKind::Update {
                table,
                table_index,
                returning,
                ..
            } => numbered(*table_index, if *returning { table.columns.len() } else { 1 }),
            LogicalOperatorKind::Union { table_index, types }
            | LogicalOperatorKind::ExceptIntersect { table_index, types, .. }
            | LogicalOperatorKind::RecursiveCte { table_index, types, .. }
            | LogicalOperatorKind::WorkingTable { table_index, types, .. } => numbered(*table_index, types.len()),
            LogicalOperatorKind::LateralJoin { right, .. } => {
                let mut bindings = self.children[0].column_bindings();
                bindings.extend(right.column_bindings());
                bindings
            }
            LogicalOperatorKind::Unnest {
                table_index,
                expressions,
            } => {
                let mut bindings = self.children[0].column_bindings();
                bindings.extend(numbered(*table_index, expressions.len()));
                bindings
            }
            LogicalOperatorKind::CrossProduct | LogicalOperatorKind::Join { .. } => {
                self.join_output(|child| child.column_bindings())
            }
            LogicalOperatorKind::CreateTable { .. } | LogicalOperatorKind::CreateTableAs { .. } => numbered(0, 1),
        }
    }

    /// The expressions of this operator, for passes over the plan.
    pub fn expressions_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            LogicalOperatorKind::Filter { expressions }
            | LogicalOperatorKind::Projection { expressions, .. }
            | LogicalOperatorKind::Unnest { expressions, .. }
            | LogicalOperatorKind::Distinct { targets: expressions } => expressions.iter_mut().collect(),
            LogicalOperatorKind::LateralJoin {
                correlated, predicate, ..
            } => correlated.iter_mut().chain(predicate.iter_mut()).collect(),
            LogicalOperatorKind::Order { orders } => orders.iter_mut().map(|o| &mut o.expr).collect(),
            LogicalOperatorKind::Aggregate { groups, aggregates, .. } => groups
                .iter_mut()
                .chain(aggregates.iter_mut().flat_map(|a| {
                    a.children
                        .iter_mut()
                        .chain(a.filter.iter_mut())
                        .chain(a.order_by.iter_mut().map(|(expr, _)| expr))
                }))
                .collect(),
            LogicalOperatorKind::Window { windows, .. } => {
                windows.iter_mut().flat_map(BoundWindow::expressions_mut).collect()
            }
            LogicalOperatorKind::ExpressionGet { expressions, .. } => expressions.iter_mut().flatten().collect(),
            // the join conditions read one child each, see `resolve_column_bindings`
            LogicalOperatorKind::Join { predicate, .. } => predicate.iter_mut().collect(),
            _ => Vec::new(),
        }
    }
}
