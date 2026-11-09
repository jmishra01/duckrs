// Ported from duckdb/src/optimizer/statistics_propagator.cpp and duckdb/src/optimizer/statistics/
// {expression,operator}/*.cpp @ rust-port-baseline
//
//! `StatisticsPropagator`: what is known about the columns of a plan, from the rows of its tables
//! up through its operators, decides comparisons ahead of time. A filter that cannot hold empties
//! the plan, one that always holds goes away.
//!
//! C++ keeps statistics per row group of a table. Here they cover a whole table, and they are not
//! kept up to date but worked out when a plan first reads them after a write to the table, with the
//! `min`, `max` and `count` aggregates. Plans do not outlive a statement, so they cannot go stale.
//!
//! `stats()` shows what is known here. Not ported: what statistics do for the other functions
//! (overflow-free arithmetic among them), row groups, distinct counts, and the statistics of aggregates and windows. Those columns
//! count as unknown.

use super::filter_pushdown::empty_result;
use super::rewriter::{fold_boolean, rewrite_expression};
use crate::catalog::{ColumnStatistics, TableCatalogEntry, TableDistinctCounts};
use crate::execution::run_plan;
use crate::function::binder::bind_aggregate_function;
use crate::function::{BuiltinFunctions, constant_or_null};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type Stats = ColumnStatistics;

fn unknown() -> Stats {
    Stats {
        min: None,
        max: None,
        can_be_null: true,
        can_be_valid: true,
    }
}

/// Whether min and max of values of this type say something about comparisons.
pub(crate) fn has_ordered_statistics(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    ty.is_numeric()
        || matches!(
            ty.id(),
            Boolean | Date | Time | TimestampSec | TimestampMs | Timestamp | TimestampNs | TimestampTz | Varchar
        )
}

/// The statistics of a table's columns, from its rows, or those at `positions`, in order.
fn compute_statistics(table: &Arc<TableCatalogEntry>, positions: Option<Vec<usize>>) -> Option<Vec<Stats>> {
    // a file knows these already, and reading its rows for them would read the whole table
    if positions.is_none()
        && let Some(statistics) = table.source().and_then(|source| source.column_statistics())
    {
        return Some(statistics);
    }
    let functions = BuiltinFunctions::get();
    let aggregate = |name: &str, column: BoundExpr| {
        bind_aggregate_function(functions.aggregate(name)?, vec![column], None, false).ok()
    };
    let mut aggregates = Vec::new();
    // per column where min and max are, if it has them, and then where the count is
    let mut columns = Vec::new();
    for (index, column) in table.columns.iter().enumerate() {
        let reference = BoundExpr::column_ref(ColumnBinding::new(0, index), column.ty.clone(), "");
        let ordered = has_ordered_statistics(&column.ty);
        if ordered {
            aggregates.push(aggregate("min", reference.clone())?);
            aggregates.push(aggregate("max", reference.clone())?);
        }
        aggregates.push(aggregate("count", reference)?);
        columns.push((ordered, aggregates.len() - 1));
    }
    let rows = columns.len();
    let count_star = functions.aggregate("count_star")?;
    aggregates.push(bind_aggregate_function(count_star, Vec::new(), None, false).ok()?);
    let scan = LogicalOperator::new(
        K::Get {
            table: table.clone(),
            table_index: 0,
            column_ids: (0..table.columns.len()).collect(),
            filters: Vec::new(),
            projection: None,
            index_rows: positions.map(Arc::new),
        },
        Vec::new(),
    );
    let count_column = aggregates.len() - 1;
    let mut plan = LogicalOperator::new(
        K::Aggregate {
            group_index: 1,
            aggregate_index: 2,
            groupings_index: 3,
            groups: Vec::new(),
            aggregates,
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
        },
        vec![scan],
    );
    resolve_column_bindings(&mut plan).ok()?;
    let result = run_plan(plan).ok()?;
    let as_count = |value: Value| value.try_to_string().ok()?.parse::<u64>().ok();
    let total = as_count(result.get_value(count_column, 0))?;
    let mut statistics = Vec::with_capacity(rows);
    for (index, (ordered, count_at)) in columns.into_iter().enumerate() {
        let valid = as_count(result.get_value(count_at, 0))?;
        let bound = |at: usize| {
            let value = result.get_value(at, 0);
            let usable = ordered && !value.is_null() && value.logical_type() == &table.columns[index].ty;
            usable.then_some(value)
        };
        statistics.push(Stats {
            min: count_at.checked_sub(2).and_then(bound),
            max: count_at.checked_sub(1).and_then(bound),
            can_be_null: valid < total,
            can_be_valid: valid > 0,
        });
    }
    Some(statistics)
}

/// What is known of the columns of `table`. Reading all rows takes long, and a table that is written
/// to would pay for that with each statement, so this goes on from what was known: new values add to
/// it, and rows that went don't take anything away, as in C++.
pub(super) fn table_statistics(table: &Arc<TableCatalogEntry>) -> Option<Arc<Vec<Stats>>> {
    let rows = table.row_count();
    let (version, known, new_rows) = {
        let mut cached = table.statistics.lock();
        if cached.unreported {
            cached.forget();
        }
        match cached.columns.clone() {
            Some(columns) if cached.covered >= rows && cached.updated.is_empty() => return Some(columns),
            Some(columns) => {
                let mut positions = std::mem::take(&mut cached.updated);
                positions.extend(cached.covered..rows);
                positions.retain(|position| *position < rows);
                positions.sort_unstable();
                positions.dedup();
                (cached.version, Some(columns), positions)
            }
            None => (cached.version, None, Vec::new()),
        }
    };
    let merged = match known {
        Some(known) => {
            let Some(new) = compute_statistics(table, Some(new_rows)) else {
                table.statistics.lock().forget();
                return None;
            };
            known.iter().cloned().zip(new).map(|(a, b)| union(a, b)).collect()
        }
        None => {
            let mut columns = compute_statistics(table, None)?;
            // C++ doesn't narrow them when rows go, so that it looks for such a key in an index, where
            // it isn't, and reports an index scan rather than an empty result
            if table.row_ids(rows).has_gaps() {
                for key in table.scan_keys() {
                    if let [column] = key.columns.as_slice() {
                        columns[*column].min = None;
                        columns[*column].max = None;
                    }
                }
            }
            columns
        }
    };
    let columns = Arc::new(merged);
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.columns = Some(columns.clone());
        cached.covered = rows;
    } else {
        // it isn't known what these saw of what came since
        cached.forget();
    }
    Some(columns)
}

/// How many rows `compute_distinct_counts` reads, which a statement can pay for after every write.
const DISTINCT_SAMPLE_ROWS: u64 = 100_000;

/// A HyperLogLog sketch of 2^14 registers, for about 1.6% of error.
struct DistinctSketch {
    registers: Vec<u8>,
}

impl DistinctSketch {
    const BITS: u32 = 14;

    fn new() -> Self {
        Self {
            registers: vec![0; 1 << Self::BITS],
        }
    }

    fn add(&mut self, value: u64) {
        // the finalizer of MurmurHash3: nearby keys have to end up far apart
        let mut hash = value;
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        hash ^= hash >> 33;
        let register = (hash >> (64 - Self::BITS)) as usize;
        let rest = hash << Self::BITS | 1 << (Self::BITS - 1);
        let slot = &mut self.registers[register];
        *slot = (*slot).max(rest.leading_zeros() as u8 + 1);
    }

    fn estimate(&self) -> u64 {
        let m = self.registers.len() as f64;
        let harmonic: f64 = self.registers.iter().map(|&r| 2f64.powi(-i32::from(r))).sum();
        let raw = 0.7213 / (1.0 + 1.079 / m) * m * m / harmonic;
        let empty = self.registers.iter().filter(|&&r| r == 0).count();
        // few values leave registers empty, and counting those is more exact
        let estimate = if raw <= 2.5 * m && empty > 0 {
            m * (m / empty as f64).ln()
        } else {
            raw
        };
        estimate.round() as u64
    }
}

/// The distinct values in all rows of an integer or date column, which is what keys are. Those are
/// cheap to read, and of keys a sample says little: the first rows of a sorted key have a part of
/// its values, and those of a random one miss some.
fn key_column_distinct(table: &TableCatalogEntry, column: usize) -> Option<u64> {
    let mut sketch = DistinctSketch::new();
    let id = table.columns[column].ty.id();
    if !matches!(id, LogicalTypeId::BigInt | LogicalTypeId::Integer | LogicalTypeId::Date) {
        return None;
    }
    table
        .for_each_chunk(column, |_, vector, count| {
            let unified = vector.to_unified(count);
            if id == LogicalTypeId::BigInt {
                let data = unified.data::<i64>();
                for row in (0..count).filter(|&row| unified.is_valid(row)) {
                    sketch.add(data[unified.sel.get(row)] as u64);
                }
            } else {
                let data = unified.data::<i32>();
                for row in (0..count).filter(|&row| unified.is_valid(row)) {
                    sketch.add(data[unified.sel.get(row)] as u64);
                }
            }
        })
        .ok()?;
    Some(sketch.estimate())
}

/// A scan that counts the distinct values of the columns of `table`, in its first rows. Nearly
/// all distinct there is taken to go on in the same way, and otherwise for the values to be all
/// there are. Key columns are counted in all rows, with [`key_column_distinct`].
fn compute_distinct_counts(table: &Arc<TableCatalogEntry>) -> Option<TableDistinctCounts> {
    let functions = BuiltinFunctions::get();
    let mut aggregates = Vec::new();
    let mut columns = Vec::with_capacity(table.columns.len());
    for (index, column) in table.columns.iter().enumerate() {
        if !has_ordered_statistics(&column.ty) {
            columns.push(None);
            continue;
        }
        let reference = BoundExpr::column_ref(ColumnBinding::new(0, index), column.ty.clone(), "");
        let distinct = functions.aggregate("approx_count_distinct")?;
        aggregates.push(bind_aggregate_function(distinct, vec![reference], None, false).ok()?);
        columns.push(Some(aggregates.len() - 1));
    }
    let count_star = functions.aggregate("count_star")?;
    aggregates.push(bind_aggregate_function(count_star, Vec::new(), None, false).ok()?);
    let rows_at = aggregates.len() - 1;
    let scan = LogicalOperator::new(
        K::Get {
            table: table.clone(),
            table_index: 0,
            column_ids: (0..table.columns.len()).collect(),
            filters: Vec::new(),
            projection: None,
            index_rows: None,
        },
        Vec::new(),
    );
    let rows = table.row_count() as u64;
    let scan = if rows > DISTINCT_SAMPLE_ROWS {
        let limit = K::Limit {
            limit: Some(DISTINCT_SAMPLE_ROWS),
            offset: 0,
            failure: None,
        };
        LogicalOperator::new(limit, vec![scan])
    } else {
        scan
    };
    let mut plan = LogicalOperator::new(
        K::Aggregate {
            group_index: 1,
            aggregate_index: 2,
            groupings_index: 3,
            groups: Vec::new(),
            aggregates,
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
        },
        vec![scan],
    );
    resolve_column_bindings(&mut plan).ok()?;
    let result = run_plan(plan).ok()?;
    let as_count = |at: usize| result.get_value(at, 0).try_to_string().ok()?.parse::<u64>().ok();
    let sampled = as_count(rows_at)?;
    let rows = rows.max(sampled);
    let scale = |distinct: u64| {
        if sampled > 0 && distinct as f64 >= 0.9 * sampled as f64 {
            (distinct as f64 * rows as f64 / sampled as f64) as u64
        } else {
            distinct
        }
    };
    let columns = columns.into_iter().enumerate().map(|(column, at)| {
        let exact = (rows > sampled).then(|| key_column_distinct(table, column)).flatten();
        exact.or_else(|| at.and_then(as_count).map(scale))
    });
    Some(TableDistinctCounts {
        rows,
        columns: columns.collect(),
    })
}

/// `compute_distinct_counts`, once per state of the rows.
pub(super) fn table_distinct_counts(table: &Arc<TableCatalogEntry>) -> Option<Arc<TableDistinctCounts>> {
    let version = {
        let cached = table.statistics.lock();
        if let Some(distinct) = &cached.distinct {
            return Some(distinct.clone());
        }
        cached.version
    };
    let distinct = Arc::new(compute_distinct_counts(table)?);
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.distinct = Some(distinct.clone());
    }
    Some(distinct)
}

/// `a op b` for two values of a type, if that can be told.
pub(crate) fn compare_values(op: ComparisonOp, a: &Value, b: &Value) -> Option<bool> {
    if a.logical_type() != b.logical_type() {
        return None;
    }
    let comparison = BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(BoundExpr::constant(a.clone())),
            right: Box::new(BoundExpr::constant(b.clone())),
        },
        LogicalType::BOOLEAN,
    );
    fold_boolean(&comparison).flatten()
}

pub(super) fn flip(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// `CheckZonemap`: what `left op right` is for all values within the statistics.
fn decide(op: ComparisonOp, left: &Stats, right: &Stats) -> Option<bool> {
    use ComparisonOp as C;
    let (left_min, left_max) = (left.min.as_ref()?, left.max.as_ref()?);
    let (right_min, right_max) = (right.min.as_ref()?, right.max.as_ref()?);
    let holds = |op, a, b| compare_values(op, a, b) == Some(true);
    match op {
        C::Equal => {
            if holds(C::LessThan, left_max, right_min) || holds(C::GreaterThan, left_min, right_max) {
                Some(false)
            } else if holds(C::Equal, left_min, left_max)
                && holds(C::Equal, right_min, right_max)
                && holds(C::Equal, left_min, right_min)
            {
                Some(true)
            } else {
                None
            }
        }
        C::NotEqual => decide(C::Equal, left, right).map(|equal| !equal),
        C::LessThan => {
            if holds(C::LessThan, left_max, right_min) {
                Some(true)
            } else if holds(C::GreaterThanOrEqual, left_min, right_max) {
                Some(false)
            } else {
                None
            }
        }
        C::LessThanOrEqual => {
            if holds(C::LessThanOrEqual, left_max, right_min) {
                Some(true)
            } else if holds(C::GreaterThan, left_min, right_max) {
                Some(false)
            } else {
                None
            }
        }
        C::GreaterThan | C::GreaterThanOrEqual => decide(flip(op), right, left),
        C::DistinctFrom | C::NotDistinctFrom => None,
    }
}

fn smaller(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    let (a, b) = (a?, b?);
    compare_values(ComparisonOp::LessThan, &b, &a).map(|less| if less { b } else { a })
}

fn larger(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    let (a, b) = (a?, b?);
    compare_values(ComparisonOp::GreaterThan, &b, &a).map(|more| if more { b } else { a })
}

/// `BaseStatistics::Merge`: of values that are one or the other.
fn union(a: Stats, b: Stats) -> Stats {
    let (a_valid, b_valid) = (a.can_be_valid, b.can_be_valid);
    let (min, max) = match (a_valid, b_valid) {
        (true, false) => (a.min, a.max),
        (false, true) => (b.min, b.max),
        _ => (smaller(a.min, b.min), larger(a.max, b.max)),
    };
    Stats {
        min,
        max,
        can_be_null: a.can_be_null || b.can_be_null,
        can_be_valid: a_valid || b_valid,
    }
}

/// A plan in which a table index appears twice, as from a copy of a subquery, cannot have what is
/// known about a binding apply to all of its uses.
fn has_unique_tables(op: &LogicalOperator, seen: &mut HashSet<usize>) -> bool {
    let own = match &op.kind {
        K::Get { table_index, .. } | K::Projection { table_index, .. } => seen.insert(*table_index),
        K::Aggregate { group_index, .. } => seen.insert(*group_index),
        _ => true,
    };
    own && op.children.iter().all(|child| has_unique_tables(child, seen))
}

/// Runs the pass over a plan.
pub fn propagate_statistics(plan: &mut LogicalOperator) {
    if has_unique_tables(plan, &mut HashSet::new()) {
        StatisticsPropagator::default().visit(plan);
    }
}

#[derive(Default)]
struct StatisticsPropagator {
    /// what is known by now
    stats: HashMap<ColumnBinding, Stats>,
    /// the table columns a binding reads, whose statistics nobody asked for yet
    sources: HashMap<ColumnBinding, (Arc<TableCatalogEntry>, usize)>,
    /// bindings that are another binding under a new name
    aliases: HashMap<ColumnBinding, ColumnBinding>,
    /// bindings an outer join puts NULLs in
    nullable: HashSet<ColumnBinding>,
}

impl StatisticsPropagator {
    fn base_of(&self, mut binding: ColumnBinding) -> ColumnBinding {
        while let Some(next) = self.aliases.get(&binding) {
            binding = *next;
        }
        binding
    }

    fn is_nullable(&self, mut binding: ColumnBinding) -> bool {
        loop {
            if self.nullable.contains(&binding) {
                return true;
            }
            match self.aliases.get(&binding) {
                Some(next) => binding = *next,
                None => return false,
            }
        }
    }

    fn lookup(&mut self, binding: ColumnBinding) -> Stats {
        let base = self.base_of(binding);
        if !self.stats.contains_key(&base)
            && let Some((table, column)) = self.sources.get(&base)
        {
            let statistics = table_statistics(table).map(|all| all[*column].clone());
            self.stats.insert(base, statistics.unwrap_or_else(unknown));
        }
        let mut stats = self.stats.get(&base).cloned().unwrap_or_else(unknown);
        stats.can_be_null |= self.is_nullable(binding);
        stats
    }

    /// What holds for `binding` from here on up.
    fn narrow(&mut self, binding: ColumnBinding, stats: Stats) {
        if !stats.can_be_null {
            let mut current = binding;
            self.nullable.remove(&current);
            while let Some(next) = self.aliases.get(&current).copied() {
                self.nullable.remove(&next);
                current = next;
            }
        }
        self.stats.insert(self.base_of(binding), stats);
    }

    fn mark_nullable(&mut self, op: &LogicalOperator) {
        self.nullable.extend(op.column_bindings());
    }

    /// `PropagateExpression`: the statistics of `expr`, which it simplifies on the way. `in_filter`
    /// says only TRUE counts for what is around it, so NULL is as good as FALSE.
    fn expression(&mut self, expr: &mut BoundExpr, in_filter: bool) -> Stats {
        match &mut expr.kind {
            BoundExprKind::Constant(value) if value.is_null() => Stats {
                min: None,
                max: None,
                can_be_null: true,
                can_be_valid: false,
            },
            BoundExprKind::Constant(value) => {
                let value = has_ordered_statistics(&expr.return_type).then(|| value.clone());
                Stats {
                    min: value.clone(),
                    max: value,
                    can_be_null: false,
                    can_be_valid: true,
                }
            }
            BoundExprKind::ColumnRef { binding, depth: 0 } => {
                let binding = *binding;
                self.lookup(binding)
            }
            BoundExprKind::Comparison { .. } => self.comparison(expr, in_filter),
            BoundExprKind::Between { .. } => self.between(expr, in_filter),
            BoundExprKind::Conjunction { children, .. } => {
                for child in children {
                    self.expression(child, in_filter);
                }
                unknown()
            }
            BoundExprKind::Operator {
                op: BoundOperator::IsNull | BoundOperator::IsNotNull,
                ..
            } => self.is_null(expr),
            BoundExprKind::Operator {
                op: BoundOperator::Coalesce,
                ..
            } => self.coalesce(expr),
            // TRY has its own idea of errors, a subquery and a lambda their own columns
            BoundExprKind::Operator {
                op: BoundOperator::Try, ..
            }
            | BoundExprKind::Subquery { .. }
            | BoundExprKind::ListLambda { .. } => unknown(),
            BoundExprKind::Case { checks, else_expr } => {
                let mut result: Option<Stats> = None;
                let mut add = |stats: Stats| {
                    result = Some(match result.take() {
                        Some(so_far) => union(so_far, stats),
                        None => stats,
                    });
                };
                let mut results = Vec::new();
                for (when, then) in checks {
                    self.expression(when, false);
                    results.push(self.expression(then, false));
                }
                results.push(self.expression(else_expr, false));
                results.into_iter().for_each(&mut add);
                result.unwrap_or_else(unknown)
            }
            BoundExprKind::Cast { .. } => self.cast(expr),
            BoundExprKind::Function { .. } => self.function(expr),
            _ => {
                for child in expr.children_mut() {
                    self.expression(child, false);
                }
                unknown()
            }
        }
    }

    /// Replaces `expr`, which is decided to be `answer` unless one of `sides` is NULL.
    fn settle(expr: &mut BoundExpr, answer: bool, sides: Vec<BoundExpr>, in_filter: bool) {
        let alias = expr.alias.clone();
        *expr = if sides.is_empty() || (in_filter && !answer) {
            BoundExpr::constant(Value::boolean(answer))
        } else {
            let mut children = vec![BoundExpr::constant(Value::boolean(answer))];
            children.extend(sides);
            constant_or_null(children)
        };
        expr.alias = alias;
    }

    /// `PropagateExpression(BoundComparisonExpression)`.
    fn comparison(&mut self, expr: &mut BoundExpr, in_filter: bool) -> Stats {
        let BoundExprKind::Comparison { op, left, right } = &mut expr.kind else {
            unreachable!()
        };
        let op = *op;
        let left_stats = self.expression(left, false);
        let right_stats = self.expression(right, false);
        if matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
            return unknown();
        }
        let can_be_null = left_stats.can_be_null || right_stats.can_be_null;
        if !left_stats.can_be_valid || !right_stats.can_be_valid {
            let alias = expr.alias.clone();
            *expr = BoundExpr::constant(Value::null(LogicalType::BOOLEAN)).with_alias(alias);
            return unknown();
        }
        if let Some(answer) = decide(op, &left_stats, &right_stats) {
            let mut sides = Vec::new();
            if left_stats.can_be_null {
                sides.push((**left).clone());
            }
            if right_stats.can_be_null {
                sides.push((**right).clone());
            }
            Self::settle(expr, answer, sides, in_filter);
        }
        Stats {
            can_be_null,
            ..unknown()
        }
    }

    /// `PropagateExpression(BoundBetweenExpression)`.
    fn between(&mut self, expr: &mut BoundExpr, in_filter: bool) -> Stats {
        let BoundExprKind::Between { input, lower, upper } = &mut expr.kind else {
            unreachable!()
        };
        let input_stats = self.expression(input, false);
        let lower_stats = self.expression(lower, false);
        let upper_stats = self.expression(upper, false);
        let above = decide(ComparisonOp::GreaterThanOrEqual, &input_stats, &lower_stats);
        let below = decide(ComparisonOp::LessThanOrEqual, &input_stats, &upper_stats);
        let bounds_cross = !lower_stats.can_be_null
            && !upper_stats.can_be_null
            && decide(ComparisonOp::LessThanOrEqual, &lower_stats, &upper_stats) == Some(false);
        let side = |(expr, stats): (&BoundExpr, &Stats)| stats.can_be_null.then(|| expr.clone());
        let input_side = side((input, &input_stats));
        let lower_side = side((lower, &lower_stats));
        let upper_side = side((upper, &upper_stats));
        // FALSE AND NULL is FALSE, so a side that is FALSE or NULL only settles it if the other
        // one cannot be FALSE
        let settled = match (above, below) {
            _ if bounds_cross => Some((false, vec![input_side])),
            _ if in_filter && (above == Some(false) || below == Some(false)) => Some((false, Vec::new())),
            (Some(true), Some(true)) => Some((true, vec![input_side, lower_side, upper_side])),
            (Some(false), Some(true)) => Some((false, vec![input_side, lower_side])),
            (Some(true), Some(false)) => Some((false, vec![input_side, upper_side])),
            (Some(false), _) if input_side.is_none() && lower_side.is_none() => Some((false, Vec::new())),
            (_, Some(false)) if input_side.is_none() && upper_side.is_none() => Some((false, Vec::new())),
            _ => None,
        };
        if let Some((answer, sides)) = settled {
            Self::settle(expr, answer, sides.into_iter().flatten().collect(), in_filter);
            return unknown();
        }
        // one side is settled: what is left is the other comparison
        if !input_stats.can_be_null {
            let remaining = match (above, below) {
                (Some(true), _) if !lower_stats.can_be_null => Some((ComparisonOp::LessThanOrEqual, upper.clone())),
                (_, Some(true)) if !upper_stats.can_be_null => Some((ComparisonOp::GreaterThanOrEqual, lower.clone())),
                _ => None,
            };
            if let Some((op, bound)) = remaining {
                let (input, alias) = (input.clone(), expr.alias.clone());
                *expr = BoundExpr::new(
                    BoundExprKind::Comparison {
                        op,
                        left: input,
                        right: bound,
                    },
                    LogicalType::BOOLEAN,
                )
                .with_alias(alias);
            }
        }
        unknown()
    }

    /// `PropagateExpression(BoundOperatorExpression)` for IS NULL and IS NOT NULL.
    fn is_null(&mut self, expr: &mut BoundExpr) -> Stats {
        let BoundExprKind::Operator { op, children } = &mut expr.kind else {
            unreachable!()
        };
        let is_null = *op == BoundOperator::IsNull;
        let stats = self.expression(&mut children[0], false);
        let is_always = if !stats.can_be_null {
            Some(!is_null)
        } else if !stats.can_be_valid {
            Some(is_null)
        } else {
            None
        };
        if let Some(answer) = is_always
            && !children[0].is_volatile()
        {
            let alias = expr.alias.clone();
            *expr = BoundExpr::constant(Value::boolean(answer)).with_alias(alias);
        }
        // not "not NULL": what is around it would fold, and drop the errors of the child
        unknown()
    }

    /// `PropagateExpression(BoundOperatorExpression)` for COALESCE: what comes after an argument
    /// that is not NULL never runs.
    fn coalesce(&mut self, expr: &mut BoundExpr) -> Stats {
        let BoundExprKind::Operator { children, .. } = &mut expr.kind else {
            unreachable!()
        };
        let mut merged: Option<Stats> = None;
        let mut end = children.len();
        for (index, child) in children.iter_mut().enumerate() {
            let stats = self.expression(child, false);
            let is_last_one_needed = !stats.can_be_null;
            let mut stats_so_far = match merged.take() {
                Some(so_far) => union(so_far, stats),
                None => stats,
            };
            if is_last_one_needed {
                stats_so_far.can_be_null = false;
                merged = Some(stats_so_far);
                end = index + 1;
                break;
            }
            merged = Some(stats_so_far);
        }
        children.truncate(end);
        if children.len() == 1 {
            let alias = expr.alias.clone();
            let ty = expr.return_type.clone();
            let child = children.remove(0);
            *expr = child.cast_to(&ty, false).with_alias(alias);
        }
        merged.unwrap_or_else(unknown)
    }

    /// `PropagateExpression(BoundCastExpression)`: a cast between integers keeps their order, as far
    /// as the bounds fit the new type.
    fn cast(&mut self, expr: &mut BoundExpr) -> Stats {
        let target = expr.return_type.clone();
        let BoundExprKind::Cast { child, try_cast } = &mut expr.kind else {
            unreachable!()
        };
        let try_cast = *try_cast;
        let stats = self.expression(child, false);
        // a value that does not fit fails, or is NULL with TRY_CAST
        if try_cast || !child.return_type.is_integral() || !target.is_integral() {
            return unknown();
        }
        let convert = |value: Option<Value>| {
            let cast = BoundExpr::constant(value?).cast_to(&target, false);
            let converted = crate::execution::expression_executor::try_evaluate_scalar(&cast)?;
            (converted.logical_type() == &target).then_some(converted)
        };
        match (convert(stats.min), convert(stats.max)) {
            (Some(min), Some(max)) => Stats {
                min: Some(min),
                max: Some(max),
                ..stats
            },
            _ => unknown(),
        }
    }

    /// `PropagateExpression(BoundFunctionExpression)`: `+`, `-` and `*` of integers, as long as the
    /// bounds do not overflow.
    fn function(&mut self, expr: &mut BoundExpr) -> Stats {
        let return_type = expr.return_type.clone();
        let BoundExprKind::Function {
            function,
            children,
            is_operator,
        } = &mut expr.kind
        else {
            unreachable!()
        };
        let mut stats: Vec<Stats> = children.iter_mut().map(|c| self.expression(c, false)).collect();
        // `StatsPropagateStats`: `stats()` gets the statistics of its argument as a constant
        if function.name == "stats" && children.len() == 1 {
            let argument = children[0].return_type.clone();
            if crate::function::stats_supported(&argument)
                && let Some(value) = stats_struct(&stats[0], &argument)
            {
                *function = Arc::new(crate::function::stats_with_value(argument));
                children.push(BoundExpr::constant(value));
            }
            return unknown();
        }
        if function.name == "constant_or_null" && stats[1..].iter().all(|s| !s.can_be_null) {
            let alias = expr.alias.clone();
            let value = children.swap_remove(0);
            *expr = value.with_alias(alias);
            return stats.swap_remove(0);
        }
        if !*is_operator || children.len() != 2 || !return_type.is_integral() {
            return unknown();
        }
        let right = stats.pop().expect("two children");
        let left = stats.pop().expect("two children");
        let can_be_null = left.can_be_null || right.can_be_null;
        let result = Stats {
            can_be_null,
            ..unknown()
        };
        let (Some(a), Some(b), Some(c), Some(d)) = (left.min, left.max, right.min, right.max) else {
            return result;
        };
        let apply = |x: &Value, y: &Value| {
            let call = expr_with_constants(function, x, y, &return_type);
            let value = crate::execution::expression_executor::try_evaluate_scalar(&call)?;
            (value.logical_type() == &return_type).then_some(value)
        };
        let corners: Option<Vec<Value>> = match function.name.as_str() {
            "+" => [apply(&a, &c), apply(&b, &d)].into_iter().collect(),
            "-" => [apply(&a, &d), apply(&b, &c)].into_iter().collect(),
            "*" => [apply(&a, &c), apply(&a, &d), apply(&b, &c), apply(&b, &d)]
                .into_iter()
                .collect(),
            _ => None,
        };
        // the bounds overflow, so a row can too
        let Some(corners) = corners else {
            return unknown();
        };
        let min = corners.iter().cloned().map(Some).reduce(smaller).flatten();
        let max = corners.into_iter().map(Some).reduce(larger).flatten();
        Stats { min, max, ..result }
    }

    /// `UpdateFilterStatistics`: what a row that passes `filter` says about its columns.
    fn update_from_filter(&mut self, filter: &BoundExpr) {
        match &filter.kind {
            BoundExprKind::Comparison { op, left, right } => {
                if matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
                    return;
                }
                let binding_of = |e: &BoundExpr| match &e.kind {
                    BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
                    _ => None,
                };
                match (binding_of(left), binding_of(right)) {
                    (Some(a), Some(b)) => self.update_columns(*op, a, b),
                    (Some(column), None) => self.update_with_value(*op, column, right),
                    (None, Some(column)) => self.update_with_value(flip(*op), column, left),
                    (None, None) => {}
                }
            }
            // only TRUE gets through, and that takes all of them not being NULL
            BoundExprKind::Function { function, children, .. } if function.name == "constant_or_null" => {
                for child in &children[1..] {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &child.kind {
                        let mut stats = self.lookup(*binding);
                        stats.can_be_null = false;
                        self.narrow(*binding, stats);
                    }
                }
            }
            BoundExprKind::Between { input, lower, upper } => {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &input.kind {
                    self.update_with_value(ComparisonOp::GreaterThanOrEqual, *binding, lower);
                    self.update_with_value(ComparisonOp::LessThanOrEqual, *binding, upper);
                }
            }
            BoundExprKind::Operator { op, children } => {
                let BoundExprKind::ColumnRef { binding, depth: 0 } =
                    &children.first().map_or(&filter.kind, |c| &c.kind)
                else {
                    return;
                };
                let binding = *binding;
                let mut stats = self.lookup(binding);
                match op {
                    BoundOperator::IsNotNull => stats.can_be_null = false,
                    BoundOperator::IsNull => {
                        stats.can_be_valid = false;
                        stats.min = None;
                        stats.max = None;
                    }
                    _ => return,
                }
                self.narrow(binding, stats);
            }
            _ => {}
        }
    }

    /// Two columns that passed a comparison are not NULL, and if equal in each other's range.
    fn update_columns(&mut self, op: ComparisonOp, a: ColumnBinding, b: ColumnBinding) {
        let mut left = self.lookup(a);
        let mut right = self.lookup(b);
        left.can_be_null = false;
        right.can_be_null = false;
        match op {
            ComparisonOp::Equal => {
                let min = larger(left.min.clone(), right.min.clone());
                let max = smaller(left.max.clone(), right.max.clone());
                left.min.clone_from(&min);
                left.max.clone_from(&max);
                right.min = min;
                right.max = max;
            }
            ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual => {
                let max = smaller(left.max.clone(), right.max.clone());
                let min = larger(left.min.clone(), right.min.clone());
                left.max = max;
                right.min = min;
            }
            ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual => {
                let max = smaller(left.max.clone(), right.max.clone());
                let min = larger(left.min.clone(), right.min.clone());
                right.max = max;
                left.min = min;
            }
            _ => {}
        }
        self.narrow(a, left);
        self.narrow(b, right);
    }

    /// `column op value` held.
    fn update_with_value(&mut self, op: ComparisonOp, column: ColumnBinding, value: &BoundExpr) {
        let BoundExprKind::Constant(value) = &value.kind else {
            return;
        };
        if value.is_null() {
            return;
        }
        let mut stats = self.lookup(column);
        stats.can_be_null = false;
        let value = Some(value.clone());
        match op {
            ComparisonOp::Equal => {
                stats.min = value
                    .clone()
                    .map(|v| larger(stats.min.clone(), Some(v.clone())).unwrap_or(v));
                stats.max = value.map(|v| smaller(stats.max.clone(), Some(v.clone())).unwrap_or(v));
            }
            ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual => {
                stats.min = value.map(|v| larger(stats.min.clone(), Some(v.clone())).unwrap_or(v));
            }
            ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual => {
                stats.max = value.map(|v| smaller(stats.max.clone(), Some(v.clone())).unwrap_or(v));
            }
            _ => {}
        }
        self.narrow(column, stats);
    }

    /// `PropagateStatistics(LogicalFilter)`.
    fn filter(&mut self, op: &mut LogicalOperator) {
        let K::Filter { expressions } = &mut op.kind else {
            unreachable!()
        };
        let mut kept = Vec::with_capacity(expressions.len());
        for mut expr in std::mem::take(expressions) {
            self.expression(&mut expr, true);
            rewrite_expression(&mut expr, true);
            match fold_boolean(&expr) {
                Some(Some(true)) => {}
                Some(_) => {
                    *op = empty_result(op);
                    return;
                }
                None => {
                    self.update_from_filter(&expr);
                    kept.push(expr);
                }
            }
        }
        if kept.is_empty() {
            *op = op.children.remove(0);
        } else {
            *expressions = kept;
        }
    }

    /// `PropagateStatistics(LogicalJoin)`.
    fn join(&mut self, op: &mut LogicalOperator) {
        let K::Join {
            join_type,
            conditions,
            predicate,
        } = &mut op.kind
        else {
            unreachable!()
        };
        let join_type = *join_type;
        let matches_only = matches!(join_type, JoinType::Inner | JoinType::Semi);
        let mut no_rows = false;
        let mut right_is_empty = false;
        for condition in conditions.iter_mut().filter(|c| !c.null_safe) {
            let left = self.expression(&mut condition.left, false);
            let right = self.expression(&mut condition.right, false);
            let never =
                decide(ComparisonOp::Equal, &left, &right) == Some(false) || !left.can_be_valid || !right.can_be_valid;
            no_rows |= matches_only && never;
            right_is_empty |= join_type == JoinType::Left && never;
            if let (
                true,
                BoundExprKind::ColumnRef { binding: a, depth: 0 },
                BoundExprKind::ColumnRef { binding: b, depth: 0 },
            ) = (matches_only, &condition.left.kind, &condition.right.kind)
            {
                self.update_columns(ComparisonOp::Equal, *a, *b);
            }
        }
        if let Some(condition) = predicate {
            self.expression(condition, true);
            rewrite_expression(condition, true);
            match fold_boolean(condition) {
                Some(Some(true)) => *predicate = None,
                Some(_) => {
                    no_rows |= matches_only;
                    right_is_empty |= join_type == JoinType::Left;
                }
                None if matches_only => self.update_from_filter(condition),
                None => {}
            }
        }
        let no_keys = conditions.is_empty() && predicate.is_none();
        if no_rows {
            *op = empty_result(op);
            return;
        }
        if join_type == JoinType::Inner && no_keys {
            op.kind = K::CrossProduct;
            return;
        }
        // the left rows stay, without a match
        if right_is_empty {
            op.children[1] = empty_result(&op.children[1]);
        }
        match join_type {
            JoinType::Left => self.mark_nullable(&op.children[1]),
            JoinType::Right => self.mark_nullable(&op.children[0]),
            JoinType::Outer => {
                self.mark_nullable(&op.children[0]);
                self.mark_nullable(&op.children[1]);
            }
            _ => {}
        }
    }

    /// `PropagateStatistics(LogicalSetOperation)`: a UNION has the rows of all its children.
    fn union_of_children(&mut self, op: &LogicalOperator, table_index: usize, columns: usize) {
        for column in 0..columns {
            let mut merged: Option<Stats> = None;
            for child in &op.children {
                let stats = self.lookup(child.column_bindings()[column]);
                merged = Some(match merged {
                    Some(so_far) => union(so_far, stats),
                    None => stats,
                });
            }
            if let Some(merged) = merged {
                self.stats.insert(ColumnBinding::new(table_index, column), merged);
            }
        }
    }

    /// `PropagateStatistics` of an operator, children first.
    /// `count(x)` of a column without NULLs counts rows, and does not have to read it.
    fn count_valid_as_count_star(&mut self, op: &mut LogicalOperator) {
        let K::Aggregate { aggregates, .. } = &mut op.kind else {
            return;
        };
        for aggregate in aggregates {
            // an exported state says which aggregate it is of
            if aggregate.function.export.is_some() {
                continue;
            }
            let [
                BoundExpr {
                    kind: BoundExprKind::ColumnRef { binding, depth: 0 },
                    ..
                },
            ] = aggregate.children.as_slice()
            else {
                continue;
            };
            if aggregate.function.name != "count" || aggregate.distinct || self.lookup(*binding).can_be_null {
                continue;
            }
            let Some(count_star) = BuiltinFunctions::get().aggregate("count_star") else {
                continue;
            };
            let Ok(rewritten) = bind_aggregate_function(count_star, Vec::new(), aggregate.filter.clone(), false) else {
                continue;
            };
            *aggregate = rewritten;
        }
    }

    fn visit(&mut self, op: &mut LogicalOperator) {
        for child in &mut op.children {
            self.visit(child);
        }
        self.count_valid_as_count_star(op);
        // `ParquetCopyToPropagateStatistics`: columns that can't be NULL are written REQUIRED
        if matches!(op.kind, K::CopyToParquet { .. }) {
            let not_null: Vec<bool> = op.children[0]
                .column_bindings()
                .into_iter()
                .map(|binding| !self.lookup(binding).can_be_null)
                .collect();
            if let K::CopyToParquet { options, .. } = &mut op.kind {
                options.not_null_columns = not_null;
            }
        }
        match &mut op.kind {
            K::Get {
                table,
                table_index,
                column_ids,
                ..
            } => {
                for (position, column) in column_ids.iter().enumerate() {
                    if *column != COLUMN_IDENTIFIER_ROW_ID {
                        let binding = ColumnBinding::new(*table_index, position);
                        self.sources.insert(binding, (table.clone(), *column));
                    }
                }
            }
            K::ParquetScan { table_index, scan } => {
                for (column, stats) in crate::execution::parquet::scan_statistics(scan).into_iter().enumerate() {
                    if let Some(stats) = stats {
                        self.stats.insert(ColumnBinding::new(*table_index, column), stats);
                    }
                }
            }
            K::Filter { .. } => self.filter(op),
            K::Join { .. } => self.join(op),
            K::Projection {
                table_index,
                expressions,
            } => {
                let table_index = *table_index;
                for (index, expr) in expressions.iter_mut().enumerate() {
                    let binding = ColumnBinding::new(table_index, index);
                    if let BoundExprKind::ColumnRef {
                        binding: source,
                        depth: 0,
                    } = &expr.kind
                    {
                        self.aliases.insert(binding, *source);
                    } else {
                        let stats = self.expression(expr, false);
                        rewrite_expression(expr, false);
                        self.stats.insert(binding, stats);
                    }
                }
            }
            K::Aggregate {
                group_index,
                groups,
                grouping_sets,
                ..
            } if grouping_sets.len() <= 1 && grouping_sets.iter().all(|set| set.len() == groups.len()) => {
                for (index, group) in groups.iter().enumerate() {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &group.kind {
                        self.aliases.insert(ColumnBinding::new(*group_index, index), *binding);
                    }
                }
            }
            K::Union { table_index, types } => {
                let (table_index, columns) = (*table_index, types.len());
                self.union_of_children(op, table_index, columns);
            }
            K::DelimJoin {
                join_type: JoinType::Single,
                ..
            } => self.mark_nullable(&op.children[1]),
            K::AsOfJoin {
                join_type: JoinType::Left,
                ..
            } => self.mark_nullable(&op.children[1]),
            K::AsOfJoin { .. } | K::PositionalJoin => {
                self.mark_nullable(&op.children[0]);
                self.mark_nullable(&op.children[1]);
            }
            _ => {}
        }
    }
}

/// The call of `function` with these two values.
/// `BaseStatistics::ToStruct` of numeric statistics (`NumericStats::ToStruct`), as a VARIANT
/// (`StatsPropagateStats`). duckrs keeps no distinct counts, so there is no `approx_unique`.
fn stats_struct(stats: &Stats, ty: &LogicalType) -> Option<Value> {
    let mut fields = Vec::new();
    if let (Some(min), Some(max)) = (&stats.min, &stats.max)
        && ty.id() != LogicalTypeId::SqlNull
    {
        fields.push(("min".to_string(), min.clone()));
        fields.push(("max".to_string(), max.clone()));
    }
    fields.push(("has_null".to_string(), Value::boolean(stats.can_be_null)));
    fields.push(("has_no_null".to_string(), Value::boolean(stats.can_be_valid)));
    let variant = LogicalType::new(LogicalTypeId::Variant);
    let source = duckrs_common::vector::Vector::constant(&Value::struct_value(fields));
    let mut result = duckrs_common::vector::Vector::new(variant, 1);
    crate::function::cast::cast_vector(&source, &mut result, 1, crate::function::cast::CastMode::Strict).ok()?;
    Some(result.get_value(0))
}

fn expr_with_constants(
    function: &Arc<crate::function::ScalarFunction>,
    left: &Value,
    right: &Value,
    return_type: &LogicalType,
) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Function {
            function: function.clone(),
            children: vec![BoundExpr::constant(left.clone()), BoundExpr::constant(right.clone())],
            is_operator: true,
        },
        return_type.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::DistinctSketch;

    #[test]
    fn distinct_sketch_counts_within_its_error() {
        for (values, repeats) in [(1u64, 100u64), (100, 5), (5_000, 3), (200_000, 30), (3_000_000, 1)] {
            let mut sketch = DistinctSketch::new();
            for _ in 0..repeats {
                for value in 0..values {
                    sketch.add(value);
                }
            }
            let estimate = sketch.estimate() as f64;
            let error = (estimate - values as f64).abs() / values as f64;
            assert!(error < 0.05, "{values} values counted as {estimate}");
        }
    }
}
