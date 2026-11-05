// Ported from duckdb/src/storage/table/row_group.cpp (CheckZonemapSegments) and
// duckdb/src/planner/filter/expression_filter.cpp (CheckStatistics) @ rust-port-baseline
//
//! Zone maps: the smallest and largest value of a column per row group, which tell a scan with
//! filters which groups it need not read. DuckDB keeps them in its storage. Here the rows live in
//! memory and only the catalog knows when they change, so they are worked out per column when a
//! filter first needs them, and thrown away by `TableCatalogEntry::write_storage`.
//!
//! A group is skipped only if no row of it can pass, so anything not understood keeps it. C++ also
//! prunes through many functions with statistics of their own (`contains`, `substr`, ...); here it
//! is comparisons, `BETWEEN`, NULL checks, AND, OR and functions that keep the order of values.

use super::physical::group_keys;
use crate::catalog::{ColumnZone, TableCatalogEntry};
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::optimizer::statistics::{compare_values, has_ordered_statistics};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator};
use crate::planner::logical::COLUMN_IDENTIFIER_ROW_ID;
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::cmp::Ordering;
use std::sync::Arc;

/// `DEFAULT_ROW_GROUP_SIZE`.
pub const ROW_GROUP_SIZE: usize = 122_880;
/// The chunks of a row group, which all but the last chunk of a table fill.
pub const CHUNKS_PER_ROW_GROUP: usize = ROW_GROUP_SIZE / STANDARD_VECTOR_SIZE;

/// The row groups of `rows` rows.
pub fn row_group_count(rows: usize) -> usize {
    rows.div_ceil(ROW_GROUP_SIZE)
}

/// Functions whose result does not go down when their argument goes up.
const MONOTONE_FUNCTIONS: &[&str] = &["floor", "ceil", "ceiling", "year", "decade", "century", "millennium"];

/// The zones of a column, by the keys in which its values sort as they do in a comparison.
fn compute_zones(table: &TableCatalogEntry, column: usize) -> Vec<ColumnZone> {
    if !has_ordered_statistics(&table.columns[column].ty) {
        return Vec::new();
    }
    let storage = table.storage.read();
    let mut zones: Vec<ColumnZone> = Vec::new();
    for (index, chunk) in storage.chunks().enumerate() {
        if index % CHUNKS_PER_ROW_GROUP == 0 {
            zones.push(ColumnZone::default());
        }
        let zone = zones.last_mut().expect("just pushed one");
        let vector = chunk.column(column);
        let count = chunk.len();
        let Ok(keys) = group_keys(&[vector], count) else {
            zone.unknown = true;
            continue;
        };
        let validity = vector.to_unified(count);
        let (mut smallest, mut largest): (Option<usize>, Option<usize>) = (None, None);
        for (row, key) in keys.iter().enumerate() {
            if !validity.is_valid(row) {
                zone.has_null = true;
                continue;
            }
            if smallest.is_none_or(|at| key < &keys[at]) {
                smallest = Some(row);
            }
            if largest.is_none_or(|at| key > &keys[at]) {
                largest = Some(row);
            }
        }
        let merge = |current: &mut Option<Value>, row: Option<usize>, keep: Ordering| {
            let Some(value) = row.map(|row| vector.get_value(row)) else {
                return;
            };
            let replace = current.as_ref().is_none_or(|old| {
                let op = match keep {
                    Ordering::Less => ComparisonOp::LessThan,
                    _ => ComparisonOp::GreaterThan,
                };
                compare_values(op, &value, old) == Some(true)
            });
            if replace {
                *current = Some(value);
            }
        };
        merge(&mut zone.min, smallest, Ordering::Less);
        merge(&mut zone.max, largest, Ordering::Greater);
    }
    zones
}

/// `compute_zones`, once per state of the rows.
fn zones_of(table: &TableCatalogEntry, column: usize) -> Arc<Vec<ColumnZone>> {
    let version = {
        let cached = table.statistics.lock();
        if let Some(zones) = cached.zones.get(&column) {
            return zones.clone();
        }
        cached.version
    };
    let zones = Arc::new(compute_zones(table, column));
    let mut cached = table.statistics.lock();
    if cached.version == version {
        cached.zones.insert(column, zones.clone());
    }
    zones
}

/// The values an expression can have in a group.
enum Range {
    /// nothing is known
    Unknown,
    /// only NULL
    Null,
    Bounds(Value, Value),
}

/// The zones of a scan's output columns for one group.
struct Group<'a> {
    table: &'a TableCatalogEntry,
    column_ids: &'a [usize],
    group: usize,
    rows: usize,
}

impl Group<'_> {
    fn zone(&self, index: usize) -> Option<ColumnZone> {
        let column = *self.column_ids.get(index)?;
        if column == COLUMN_IDENTIFIER_ROW_ID {
            let first = self.group * ROW_GROUP_SIZE;
            let last = (first + ROW_GROUP_SIZE).min(self.rows).checked_sub(1)?;
            return Some(ColumnZone {
                min: Some(Value::bigint(first as i64)),
                max: Some(Value::bigint(last as i64)),
                ..ColumnZone::default()
            });
        }
        let zones = zones_of(self.table, column);
        let zone = zones.get(self.group)?;
        (!zone.unknown).then(|| zone.clone())
    }

    fn column_range(&self, index: usize) -> Range {
        match self.zone(index) {
            None => Range::Unknown,
            Some(ColumnZone {
                min: Some(min),
                max: Some(max),
                ..
            }) => Range::Bounds(min, max),
            Some(_) => Range::Null,
        }
    }

    fn range(&self, expr: &BoundExpr) -> Range {
        match &expr.kind {
            BoundExprKind::Reference(index) => self.column_range(*index),
            BoundExprKind::Cast { child, try_cast: false } if keeps_order(&child.return_type, &expr.return_type) => {
                self.through(expr, child)
            }
            BoundExprKind::Function { function, children, .. }
                if children.len() == 1 && MONOTONE_FUNCTIONS.contains(&function.name.as_str()) =>
            {
                self.through(expr, &children[0])
            }
            _ => Range::Unknown,
        }
    }

    /// The range of `expr`, a function of `child` that keeps the order of values.
    fn through(&self, expr: &BoundExpr, child: &BoundExpr) -> Range {
        let at = |value: &Value| {
            let mut call = expr.clone();
            for slot in call.children_mut() {
                *slot = BoundExpr::constant(value.clone());
            }
            try_evaluate_scalar(&call).filter(|result| !result.is_null())
        };
        match self.range(child) {
            Range::Bounds(min, max) => match (at(&min), at(&max)) {
                (Some(min), Some(max)) => Range::Bounds(min, max),
                _ => Range::Unknown,
            },
            other => other,
        }
    }

    /// Whether no row of the group can make `expr` true.
    fn never_true(&self, expr: &BoundExpr) -> bool {
        match &expr.kind {
            BoundExprKind::Conjunction { and: true, children } => children.iter().any(|c| self.never_true(c)),
            BoundExprKind::Conjunction { and: false, children } => children.iter().all(|c| self.never_true(c)),
            BoundExprKind::Comparison { op, left, right } => match (&left.kind, &right.kind) {
                (_, BoundExprKind::Constant(constant)) => self.comparison_fails(*op, left, constant),
                (BoundExprKind::Constant(constant), _) => self.comparison_fails(flipped(*op), right, constant),
                _ => false,
            },
            BoundExprKind::Between { input, lower, upper } => {
                let bound = |bound: &BoundExpr, op| match &bound.kind {
                    BoundExprKind::Constant(constant) => self.comparison_fails(op, input, constant),
                    _ => false,
                };
                bound(lower, ComparisonOp::GreaterThanOrEqual) || bound(upper, ComparisonOp::LessThanOrEqual)
            }
            BoundExprKind::Operator { op, children } => {
                let zone = match children.first().map(|c| &c.kind) {
                    Some(BoundExprKind::Reference(index)) => self.zone(*index),
                    _ => None,
                };
                match (op, zone) {
                    (BoundOperator::IsNull, Some(zone)) => !zone.has_null,
                    (BoundOperator::IsNotNull, Some(zone)) => zone.min.is_none(),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Whether `expr op constant` can not hold in the group.
    fn comparison_fails(&self, op: ComparisonOp, expr: &BoundExpr, constant: &Value) -> bool {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        match (op, self.range(expr)) {
            (ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom, _) => false,
            (_, _) if constant.is_null() => true,
            (_, Range::Unknown) => false,
            (_, Range::Null) => true,
            (ComparisonOp::Equal, Range::Bounds(min, max)) => {
                is(ComparisonOp::GreaterThan, &min, constant) || is(ComparisonOp::LessThan, &max, constant)
            }
            (ComparisonOp::NotEqual, Range::Bounds(min, max)) => {
                is(ComparisonOp::Equal, &min, constant) && is(ComparisonOp::Equal, &max, constant)
            }
            (ComparisonOp::LessThan, Range::Bounds(min, _)) => is(ComparisonOp::GreaterThanOrEqual, &min, constant),
            (ComparisonOp::LessThanOrEqual, Range::Bounds(min, _)) => is(ComparisonOp::GreaterThan, &min, constant),
            (ComparisonOp::GreaterThan, Range::Bounds(_, max)) => is(ComparisonOp::LessThanOrEqual, &max, constant),
            (ComparisonOp::GreaterThanOrEqual, Range::Bounds(_, max)) => is(ComparisonOp::LessThan, &max, constant),
        }
    }
}

/// `a op b` as `b op a`.
fn flipped(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// Whether a cast of `source` to `target` keeps the order of values and cannot fail on the ones
/// a filter in a scan gets to see, which `can_throw` lets through.
fn keeps_order(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalTypeId::*;
    (source.is_numeric() && target.is_numeric()) || matches!((source.id(), target.id()), (Date, Timestamp))
}

/// Per row group of the table, whether a scan with `filter` must read it.
pub fn groups_to_scan(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    filter: Option<&BoundExpr>,
    rows: usize,
) -> Vec<bool> {
    let groups = row_group_count(rows);
    let Some(filter) = filter else {
        return vec![true; groups];
    };
    if groups < 2 {
        return vec![true; groups];
    }
    (0..groups)
        .map(|group| {
            let group = Group {
                table,
                column_ids,
                group,
                rows,
            };
            !group.never_true(filter)
        })
        .collect()
}
