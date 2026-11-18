// Ported from duckdb/src/planner/operator/*.cpp and duckdb/src/planner/logical_operator.cpp
// @ rust-port-baseline
//
//! Logical operators (`LogicalOperator` and its subclasses), as an enum. Each operator knows
//! its output types and the column bindings its parents refer to.

use super::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundWindow, ColumnBinding};
use crate::catalog::{
    ColumnDefinition, GeneratedColumn, OnCreateConflict, SchemaCatalogEntry, TableCatalogEntry, TableConstraint,
};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType, MergeActionType, MergeCondition, SampleOptions};
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The `column_ids` entry of a scan for the `rowid` pseudo-column (`COLUMN_IDENTIFIER_ROW_ID`).
pub const COLUMN_IDENTIFIER_ROW_ID: usize = usize::MAX;
/// The `column_ids` entry of a scan for the `row_number` virtual column
/// (`COLUMN_IDENTIFIER_ROW_NUMBER`): the place of a row among the rows of the table, from 1.
pub const COLUMN_IDENTIFIER_ROW_NUMBER: usize = usize::MAX - 1;

#[derive(Debug, Clone)]
pub enum LogicalOperatorKind {
    /// `LogicalGet` of a table: scans `column_ids` (indexes into the table's columns).
    Get {
        table: Arc<TableCatalogEntry>,
        table_index: usize,
        column_ids: Vec<usize>,
        /// conditions on `column_ids`, each on one column, that rows must meet (`TableFilterSet`)
        filters: Vec<BoundExpr>,
        /// `multi_column_filters`: conditions on several of `column_ids`, copies of filters that stay above the
        /// scan, which only tell it which row groups it need not read
        zone_filters: Vec<BoundExpr>,
        /// `projection_ids`: the positions in `column_ids` that it outputs, without one all of them.
        /// A column keeps its binding either way.
        projection: Option<Vec<usize>>,
        /// `is_index_scan`: the rows that an ART found, by position, which is all that gets read.
        /// The filters still run on them.
        index_rows: Option<Arc<Vec<usize>>>,
        /// `extra_info.sample_options`: a SYSTEM sample the scan takes (`SamplingPushdown`)
        sample: Option<TableSample>,
    },
    /// The `range` and `generate_series` table functions: `count` integers from `start`.
    Range {
        table_index: usize,
        start: i128,
        increment: i128,
        count: u64,
    },
    /// The `read_csv` table function.
    CsvScan {
        table_index: usize,
        scan: Arc<crate::execution::csv::CsvScanData>,
    },
    /// The `read_json` table function.
    JsonScan {
        table_index: usize,
        scan: Arc<crate::execution::json_scan::JsonScanData>,
    },
    /// The `read_parquet` table function.
    ParquetScan {
        table_index: usize,
        scan: Arc<crate::execution::parquet::ParquetScanData>,
    },
    /// `LogicalCopyToFile` with FORMAT PARQUET.
    CopyToParquet {
        path: String,
        names: Vec<String>,
        options: crate::execution::parquet::ParquetWriteOptions,
    },
    /// `LogicalCopyToFile`: writes the rows of its child to a CSV file.
    CopyToFile {
        path: String,
        names: Vec<String>,
        options: crate::execution::csv::CsvWriteOptions,
        compression: crate::execution::file_system::Compression,
    },
    /// `LogicalEmptyResult`: no rows, with the columns of the plan it replaced.
    EmptyResult {
        types: Vec<LogicalType>,
        bindings: Vec<ColumnBinding>,
    },
    /// `LogicalDummyScan`: a single row with no columns, for a SELECT without FROM.
    DummyScan { table_index: usize },
    /// `LogicalColumnDataGet`: rows that are there already, as chunks whose vectors keep their
    /// kind (constant, dictionary, sequence).
    ChunkGet {
        table_index: usize,
        types: Vec<LogicalType>,
        chunks: Arc<Vec<duckrs_common::vector::DataChunk>>,
    },
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
    /// MATCH_RECOGNIZE over its child, whose expressions the operator holds already resolved.
    MatchRecognize {
        table_index: usize,
        data: Arc<crate::execution::match_recognize::MatchRecognizeData>,
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
        /// `CanUsePerfectHashAggregate`, which the statistics tell: the groups are integers of so
        /// small a range that a value's offset from the minimum can index the groups' table
        perfect_hash: bool,
        /// `CapGroupedAggregateCardinalityByKeyDomain`, which the statistics tell: at most how many
        /// combinations of groups there are, from the ranges and types of the groups
        domain_cap: Option<u64>,
    },
    /// `LogicalOrder`: sorts by the expressions, in order.
    Order { orders: Vec<BoundOrderByNode> },
    /// `LogicalTopN`: the rows `offset..offset + limit` of what `orders` sorts.
    TopN {
        orders: Vec<BoundOrderByNode>,
        limit: u64,
        offset: u64,
    },
    /// `LogicalLimit` with constant values: skips `offset` rows, then keeps `limit` rows (all
    /// of them with `None`).
    /// `failure`: a LIMIT or OFFSET that failed to fold, which is the error once a row reaches it
    /// (`BindLimitValue`, evaluated at runtime).
    Limit {
        limit: Option<u64>,
        offset: u64,
        failure: Option<duckrs_common::Error>,
    },
    /// `LogicalLimit` with a percentage of the rows.
    LimitPercent { percent: f64, offset: u64 },
    /// `LogicalSample`: a random part of the child's rows.
    Sample { options: SampleOptions },
    /// `LogicalDistinct`: one row per distinct `targets`, the first one with DISTINCT ON (`on`).
    Distinct { targets: Vec<BoundExpr>, on: bool },
    /// `LogicalSecureView`: the plan of a SECURE VIEW, which it passes on. The optimizer pushes
    /// nothing into it, so that no expression of the query sees the rows the view hides.
    SecureView { view_name: String },
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
        /// what rows are matched by, as expressions of the columns of a row by position: their
        /// collated values (the join conditions of `PlanSetOperation`); empty to match whole rows
        keys: Vec<BoundExpr>,
    },
    /// `LogicalRecursiveCTE`: the rows of its child, then those of `recursive`, run over and over
    /// on the rows of the last round in `working`, until a round adds nothing.
    RecursiveCte {
        table_index: usize,
        types: Vec<LogicalType>,
        union_all: bool,
        working: Arc<Mutex<ColumnDataCollection>>,
        /// what `recurring.name` reads: all rows so far
        recurring: Arc<Mutex<ColumnDataCollection>>,
        /// USING KEY: what each column is; empty without one
        key: Vec<CteKeyColumn>,
        recursive: Box<LogicalOperator>,
        /// `recursive` before its column bindings were resolved, for `EXPLAIN (SQL)`
        unresolved: Box<LogicalOperator>,
    },
    /// `LogicalCTERef` of a recursive CTE, inside of its recursive part: the rows of the last round.
    WorkingTable {
        table_index: usize,
        types: Vec<LogicalType>,
        working: Arc<Mutex<ColumnDataCollection>>,
    },
    /// `LogicalMaterializedCTE`: runs `children[0]` once, into `data`, and is then `children[1]`,
    /// whose `CteRef`s of `table_index` read those rows.
    MaterializedCte {
        name: String,
        table_index: usize,
        data: Arc<Mutex<ColumnDataCollection>>,
        /// `CTE_MATERIALIZE_ALWAYS`: `CTEInlining` must leave it; otherwise `CTE_MATERIALIZE_DEFAULT`
        always: bool,
    },
    /// `LogicalCTERef` of a materialized CTE: the rows of the CTE `cte_index`, from its `data`.
    CteRef {
        table_index: usize,
        cte_index: usize,
        types: Vec<LogicalType>,
        names: Vec<String>,
        data: Arc<Mutex<ColumnDataCollection>>,
    },
    /// `LogicalDelimGet`: the distinct values of the correlated columns of a `DelimJoin` around it.
    DelimGet {
        table_index: usize,
        types: Vec<LogicalType>,
        delim: Arc<Mutex<ColumnDataCollection>>,
    },
    /// `LogicalComparisonJoin` of type DELIM_JOIN: a flattened subquery. The left side is gathered,
    /// the distinct values of the left sides of `conditions` go into `delim`, and then the right
    /// side runs, which reads them through `DelimGet`. All left rows stay, in their order, each
    /// with either the one right row it matches (SINGLE) or a BOOLEAN (MARK, in `mark_index`) for
    /// whether `predicate` holds for its right rows.
    DelimJoin {
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        delim: Arc<Mutex<ColumnDataCollection>>,
        mark_index: Option<usize>,
        /// `scalar_subquery_error_on_multiple_rows`
        error_on_multiple_rows: bool,
    },
    /// A join whose right side reads columns of its left side: `right` runs for each combination
    /// of values of `correlated`, as constants for the columns `bindings`. DuckDB flattens these,
    /// which `planner::subquery` does not do yet.
    LateralJoin {
        join_type: JoinType,
        right: Box<LogicalOperator>,
        bindings: Vec<ColumnBinding>,
        /// reads the left child
        correlated: Vec<BoundExpr>,
        /// reads both sides
        predicate: Option<BoundExpr>,
    },
    /// The columns `keep` of its child, under the bindings they have there, so that an operator that
    /// keeps rows does not copy what nothing reads. C++ has projection maps in those operators.
    ColumnSelect { keep: Vec<ColumnBinding> },
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
        /// the operator its comparisons make, which the end of the optimizer picks
        algorithm: JoinAlgorithm,
    },
    /// `LogicalAsOfJoin`: for each left row, the closest right row with `left op right` among those
    /// with equal `conditions` that `predicate` lets through.
    AsOfJoin {
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        left: BoundExpr,
        op: ComparisonOp,
        right: BoundExpr,
        predicate: Option<BoundExpr>,
    },
    /// `LogicalPositionalJoin`: the rows of its children side by side.
    PositionalJoin,
    /// A MARK join: the left rows with a BOOLEAN for whether `predicate` finds a right row. With
    /// `conditions` it is whether a right row equals the left one, which is NULL where no right
    /// row does but there are right rows and a key of the comparison is NULL.
    MarkJoin {
        table_index: usize,
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
    /// `LogicalMergeInto`: its child is the join of the source and the target.
    MergeInto {
        table: Arc<TableCatalogEntry>,
        table_index: usize,
        actions: Vec<(MergeCondition, Vec<BoundMergeAction>)>,
        row_id: BoundExpr,
        /// NULL in a row without a source row; only with WHEN NOT MATCHED BY SOURCE
        marker: Option<BoundExpr>,
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
        generated: Vec<GeneratedColumn>,
        constraints: Vec<TableConstraint>,
        on_conflict: OnCreateConflict,
    },
}

/// The physical operator of a join (`PlanComparisonJoin`): a hash join with equalities among its
/// conditions, a blockwise nested loop join without conditions, and for comparisons the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinAlgorithm {
    /// `PhysicalHashJoin`, or `PhysicalBlockwiseNLJoin` without conditions it can use
    #[default]
    Hash,
    /// `PhysicalNestedLoopJoin`
    NestedLoop,
    /// `PhysicalPiecewiseMergeJoin`
    PiecewiseMerge,
    /// `PhysicalIEJoin`
    IeJoin,
}

/// What a column of a recursive CTE is to its USING KEY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CteKeyColumn {
    Key,
    /// The value of the last row with the key.
    Last,
    /// The value of the first row with the key.
    First,
    Min,
    Max,
}

/// `BoundMergeIntoAction`.
#[derive(Debug, Clone)]
pub struct BoundMergeAction {
    pub action_type: MergeActionType,
    pub condition: Option<BoundExpr>,
    /// The condition for error messages.
    pub text: String,
    /// UPDATE: the columns that `expressions` set.
    pub columns: Vec<usize>,
    /// UPDATE: the values; INSERT: every stored column; ERROR: the message.
    pub expressions: Vec<BoundExpr>,
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

/// A comparison between the two sides of a join (`JoinCondition`): `left comparison right`.
/// The optimizer only sees equalities; the comparisons of `predicate` that are conditions in C++
/// join them at the end of it ([`crate::optimizer::range_joins`]).
#[derive(Debug, Clone)]
pub struct JoinCondition {
    /// reads the left child
    pub left: BoundExpr,
    /// reads the right child
    pub right: BoundExpr,
    /// `=`, or `IS NOT DISTINCT FROM`, where NULLs are equal; past the optimizer also the others
    pub comparison: ComparisonOp,
    /// what the join tells a scan below its left side of its keys on the right
    pub filter: Option<std::sync::Arc<crate::execution::join_filter::JoinFilter>>,
}

impl JoinCondition {
    /// `IS NOT DISTINCT FROM`: NULLs are equal.
    pub fn null_safe(&self) -> bool {
        self.comparison == ComparisonOp::NotDistinctFrom
    }
    /// `=` or `IS NOT DISTINCT FROM`: a key of a hash join.
    pub fn is_equality(&self) -> bool {
        matches!(self.comparison, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom)
    }
    /// The comparison of the condition for its sides, `IS NOT DISTINCT FROM` if `null_safe`.
    pub fn equality(null_safe: bool) -> ComparisonOp {
        if null_safe { ComparisonOp::NotDistinctFrom } else { ComparisonOp::Equal }
    }
}

/// A SYSTEM sample of a table scan: whole vectors at `sample_size` percent, or about as many rows
/// as asked for, at `rate` of them (`SampleOptions` with its `sample_rate`).
#[derive(Debug, Clone)]
pub struct TableSample {
    pub options: SampleOptions,
    pub rate: f64,
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
            LogicalOperatorKind::Join { conditions, .. }
            | LogicalOperatorKind::DelimJoin { conditions, .. }
            | LogicalOperatorKind::MarkJoin { conditions, .. } => {
                for condition in conditions {
                    condition.left.substitute_columns(columns);
                    condition.right.substitute_columns(columns);
                }
            }
            LogicalOperatorKind::AsOfJoin { conditions, .. } => {
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
    pub(crate) fn visit_expressions(&mut self, f: &mut impl FnMut(&mut BoundExpr)) {
        for child in &mut self.children {
            child.visit_expressions(f);
        }
        for expr in self.expressions_mut() {
            expr.walk_mut(f);
        }
        match &mut self.kind {
            LogicalOperatorKind::Join { conditions, .. }
            | LogicalOperatorKind::DelimJoin { conditions, .. }
            | LogicalOperatorKind::MarkJoin { conditions, .. }
            | LogicalOperatorKind::AsOfJoin { conditions, .. } => {
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
            LogicalOperatorKind::Get {
                table,
                column_ids,
                projection,
                ..
            } => {
                let type_of = |&i: &usize| {
                    if i == COLUMN_IDENTIFIER_ROW_ID || i == COLUMN_IDENTIFIER_ROW_NUMBER {
                        LogicalType::BIGINT
                    } else {
                        table.columns[i].ty.clone()
                    }
                };
                match projection {
                    Some(positions) => positions.iter().map(|&p| type_of(&column_ids[p])).collect(),
                    None => column_ids.iter().map(type_of).collect(),
                }
            }
            LogicalOperatorKind::ColumnSelect { keep } => {
                let (bindings, types) = (self.children[0].column_bindings(), self.children[0].types());
                keep.iter()
                    .map(|binding| {
                        let position = bindings.iter().position(|b| b == binding);
                        types[position.expect("a column of the child")].clone()
                    })
                    .collect()
            }
            LogicalOperatorKind::CopyToParquet { options, .. } => options.output.types(),
            LogicalOperatorKind::CopyToFile { options, .. } => options.output.types(),
            LogicalOperatorKind::Range { .. } => vec![LogicalType::BIGINT],
            LogicalOperatorKind::JsonScan { scan, .. } => scan.projected_types(),
            LogicalOperatorKind::ParquetScan { scan, .. } => scan.output_types(),
            LogicalOperatorKind::CsvScan { scan, .. } => scan.projected_types(),
            LogicalOperatorKind::EmptyResult { types, .. } => types.clone(),
            // the dummy scan's single column is never read
            LogicalOperatorKind::DummyScan { .. } => vec![LogicalType::INTEGER],
            LogicalOperatorKind::ExpressionGet { types, .. } | LogicalOperatorKind::ChunkGet { types, .. } => {
                types.clone()
            }
            LogicalOperatorKind::MatchRecognize { data, .. } => data.output_types.clone(),
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
            | LogicalOperatorKind::TopN { .. }
            | LogicalOperatorKind::Limit { .. }
            | LogicalOperatorKind::Sample { .. }
            | LogicalOperatorKind::LimitPercent { .. }
            | LogicalOperatorKind::SecureView { .. }
            | LogicalOperatorKind::Distinct { .. } => self.children[0].types(),
            LogicalOperatorKind::Projection { expressions, .. } => {
                expressions.iter().map(|e| e.return_type.clone()).collect()
            }
            // and the `rowid` of each row
            LogicalOperatorKind::Delete {
                table, returning: true, ..
            } => {
                let mut types = table.types();
                types.push(LogicalType::BIGINT);
                types
            }
            LogicalOperatorKind::Insert {
                table, returning: true, ..
            }
            | LogicalOperatorKind::Update {
                table, returning: true, ..
            } => table.types(),
            LogicalOperatorKind::MergeInto {
                table, returning: true, ..
            } => {
                let mut types = table.types();
                types.push(LogicalType::VARCHAR);
                types
            }
            LogicalOperatorKind::Delete { .. }
            | LogicalOperatorKind::Update { .. }
            | LogicalOperatorKind::MergeInto { .. } => vec![LogicalType::BIGINT],
            LogicalOperatorKind::Union { types, .. }
            | LogicalOperatorKind::ExceptIntersect { types, .. }
            | LogicalOperatorKind::RecursiveCte { types, .. }
            | LogicalOperatorKind::WorkingTable { types, .. }
            | LogicalOperatorKind::CteRef { types, .. }
            | LogicalOperatorKind::DelimGet { types, .. } => types.clone(),
            LogicalOperatorKind::MaterializedCte { .. } => self.children[1].types(),
            LogicalOperatorKind::DelimJoin { mark_index, .. } => {
                let mut types = self.children[0].types();
                match mark_index {
                    Some(_) => types.push(LogicalType::BOOLEAN),
                    None => types.extend(self.children[1].types()),
                }
                types
            }
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
            LogicalOperatorKind::MarkJoin { .. } => {
                let mut types = self.children[0].types();
                types.push(LogicalType::BOOLEAN);
                types
            }
            LogicalOperatorKind::CrossProduct
            | LogicalOperatorKind::Join { .. }
            | LogicalOperatorKind::AsOfJoin { .. }
            | LogicalOperatorKind::PositionalJoin => self.join_output(|child| child.types()),
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
            }
            | LogicalOperatorKind::AsOfJoin {
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
                projection,
                ..
            } => match projection {
                Some(positions) => positions.iter().map(|&p| ColumnBinding::new(*table_index, p)).collect(),
                None => numbered(*table_index, column_ids.len()),
            },
            LogicalOperatorKind::EmptyResult { bindings, .. } => bindings.clone(),
            LogicalOperatorKind::ColumnSelect { keep } => keep.clone(),
            LogicalOperatorKind::DummyScan { table_index } | LogicalOperatorKind::Range { table_index, .. } => {
                numbered(*table_index, 1)
            }
            LogicalOperatorKind::ExpressionGet { table_index, types, .. }
            | LogicalOperatorKind::ChunkGet { table_index, types, .. } => numbered(*table_index, types.len()),
            LogicalOperatorKind::ParquetScan { table_index, scan } => numbered(*table_index, scan.output_types().len()),
            LogicalOperatorKind::JsonScan { table_index, scan } => scan
                .projected_columns()
                .into_iter()
                .map(|column| ColumnBinding::new(*table_index, column))
                .collect(),
            LogicalOperatorKind::CsvScan { table_index, scan } => scan
                .projected_columns()
                .into_iter()
                .map(|column| ColumnBinding::new(*table_index, column))
                .collect(),
            LogicalOperatorKind::MatchRecognize { table_index, data } => {
                numbered(*table_index, data.output_types.len())
            }
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
            | LogicalOperatorKind::TopN { .. }
            | LogicalOperatorKind::Limit { .. }
            | LogicalOperatorKind::Sample { .. }
            | LogicalOperatorKind::LimitPercent { .. }
            | LogicalOperatorKind::SecureView { .. }
            | LogicalOperatorKind::Distinct { .. } => self.children[0].column_bindings(),
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            } => numbered(*table_index, expressions.len()),
            LogicalOperatorKind::Delete {
                table,
                table_index,
                returning,
            } => numbered(*table_index, if *returning { table.columns.len() + 1 } else { 1 }),
            LogicalOperatorKind::Insert {
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
            LogicalOperatorKind::MergeInto {
                table,
                table_index,
                returning,
                ..
            } => numbered(*table_index, if *returning { table.columns.len() + 1 } else { 1 }),
            LogicalOperatorKind::Union { table_index, types }
            | LogicalOperatorKind::ExceptIntersect { table_index, types, .. }
            | LogicalOperatorKind::RecursiveCte { table_index, types, .. }
            | LogicalOperatorKind::WorkingTable { table_index, types, .. }
            | LogicalOperatorKind::CteRef { table_index, types, .. }
            | LogicalOperatorKind::DelimGet { table_index, types, .. } => numbered(*table_index, types.len()),
            LogicalOperatorKind::MaterializedCte { .. } => self.children[1].column_bindings(),
            LogicalOperatorKind::DelimJoin { mark_index, .. } => {
                let mut bindings = self.children[0].column_bindings();
                match mark_index {
                    Some(index) => bindings.push(ColumnBinding::new(*index, 0)),
                    None => bindings.extend(self.children[1].column_bindings()),
                }
                bindings
            }
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
            LogicalOperatorKind::MarkJoin { table_index, .. } => {
                let mut bindings = self.children[0].column_bindings();
                bindings.push(ColumnBinding::new(*table_index, 0));
                bindings
            }
            LogicalOperatorKind::CrossProduct
            | LogicalOperatorKind::Join { .. }
            | LogicalOperatorKind::AsOfJoin { .. }
            | LogicalOperatorKind::PositionalJoin => self.join_output(|child| child.column_bindings()),
            LogicalOperatorKind::CreateTable { .. } | LogicalOperatorKind::CreateTableAs { .. } => numbered(0, 1),
            LogicalOperatorKind::CopyToParquet { options, .. } => numbered(0, options.output.types().len()),
            LogicalOperatorKind::CopyToFile { options, .. } => numbered(0, options.output.types().len()),
        }
    }

    /// The expressions of this operator, for passes over the plan.
    pub fn expressions_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            LogicalOperatorKind::AsOfJoin {
                left, right, predicate, ..
            } => {
                let mut result: Vec<&mut BoundExpr> = vec![left, right];
                result.extend(predicate.iter_mut());
                result
            }
            LogicalOperatorKind::MarkJoin { predicate, .. } => predicate.iter_mut().collect(),
            LogicalOperatorKind::Get {
                filters, zone_filters, ..
            } => filters.iter_mut().chain(zone_filters.iter_mut()).collect(),
            LogicalOperatorKind::ParquetScan { scan, .. } => {
                std::sync::Arc::make_mut(scan).filters.iter_mut().collect()
            }
            LogicalOperatorKind::MergeInto {
                actions,
                row_id,
                marker,
                ..
            } => {
                let mut result: Vec<&mut BoundExpr> = vec![row_id];
                result.extend(marker.iter_mut());
                for action in actions.iter_mut().flat_map(|(_, actions)| actions) {
                    result.extend(action.condition.iter_mut());
                    result.extend(action.expressions.iter_mut());
                }
                result
            }
            LogicalOperatorKind::Filter { expressions }
            | LogicalOperatorKind::Projection { expressions, .. }
            | LogicalOperatorKind::Unnest { expressions, .. }
            | LogicalOperatorKind::Distinct { targets: expressions, .. } => expressions.iter_mut().collect(),
            LogicalOperatorKind::LateralJoin {
                correlated, predicate, ..
            } => correlated.iter_mut().chain(predicate.iter_mut()).collect(),
            LogicalOperatorKind::Order { orders } | LogicalOperatorKind::TopN { orders, .. } => {
                orders.iter_mut().map(|o| &mut o.expr).collect()
            }
            LogicalOperatorKind::Aggregate { groups, aggregates, .. } => groups
                .iter_mut()
                .chain(aggregates.iter_mut().flat_map(|a| {
                    a.children
                        .iter_mut()
                        .chain(a.filter.iter_mut())
                        .chain(a.order_by.iter_mut().map(|(expr, _)| expr))
                        .chain(a.distinct_key.iter_mut())
                }))
                .collect(),
            LogicalOperatorKind::Window { windows, .. } => {
                windows.iter_mut().flat_map(BoundWindow::expressions_mut).collect()
            }
            LogicalOperatorKind::ExpressionGet { expressions, .. } => expressions.iter_mut().flatten().collect(),
            // the join conditions read one child each, see `resolve_column_bindings`
            LogicalOperatorKind::Join { predicate, .. } | LogicalOperatorKind::DelimJoin { predicate, .. } => {
                predicate.iter_mut().collect()
            }
            _ => Vec::new(),
        }
    }
}
