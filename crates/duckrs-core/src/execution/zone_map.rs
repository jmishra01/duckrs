// Ported from duckdb/src/storage/table/row_group.cpp (CheckZonemapSegments) and
// duckdb/src/planner/filter/expression_filter.cpp (CheckStatistics) @ rust-port-baseline
//
//! Zone maps: the smallest and largest value of a column per row group, which tell a scan with
//! filters which groups it need not read. A table in a database file has them in its metadata.
//! Rows in memory change and only the catalog knows when, so for those they are worked out per
//! column when a filter first needs them, and thrown away by `TableCatalogEntry::write_storage`.
//!
//! A group is skipped only if no row of it can pass, so anything not understood keeps it. C++ also
//! prunes through many functions with statistics of their own (`contains`, `substr`, ...); here it
//! is comparisons, `BETWEEN`, NULL checks, AND, OR and functions that keep the order of values.

use super::physical::group_keys;
use crate::catalog::{ColumnZone, TableCatalogEntry, TableCursor};
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

/// The row groups of `rows` rows of `table`.
pub fn row_group_count(table: &TableCatalogEntry, rows: usize) -> usize {
    rows.div_ceil(table.row_group_size().max(1))
}

/// Functions whose result does not go down when their argument goes up.
const MONOTONE_FUNCTIONS: &[&str] = &[
    "floor",
    "ceil",
    "ceiling",
    "year",
    "decade",
    "century",
    "millennium",
    "sign",
];

/// The zones of a column, by the keys in which its values sort as they do in a comparison.
fn compute_zones(table: &TableCatalogEntry, column: usize, known: &[ColumnZone]) -> Vec<ColumnZone> {
    if !has_ordered_statistics(&table.columns[column].ty) {
        return Vec::new();
    }
    if let Some(source) = table
        .source()
        .filter(|source| TableCursor::is_aligned(&**source, table.row_group_size()))
    {
        // what the file knows of its row groups: a query need not read those that it doesn't want
        return source.zones(column).unwrap_or_default();
    }
    let mut zones = known.to_vec();
    let chunks_per_group = table.chunks_per_row_group();
    let first_chunk = known.len() * chunks_per_group;
    let read = table.for_each_chunk(column, |index, vector, count| {
        if index < first_chunk {
            return;
        }
        if index % chunks_per_group == 0 {
            zones.push(ColumnZone::default());
        }
        let zone = zones.last_mut().expect("just pushed one");
        let Ok(keys) = group_keys(&[vector], count) else {
            zone.unknown = true;
            return;
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
    });
    // with what can't be read there is nothing to go by, which means reading every group
    if read.is_err() {
        return Vec::new();
    }
    zones
}

/// `compute_zones`, once per state of the rows.
fn zones_of(table: &TableCatalogEntry, column: usize) -> Arc<Vec<ColumnZone>> {
    let groups = row_group_count(table, table.row_count());
    let (version, known) = {
        let mut cached = table.statistics.lock();
        if cached.unreported {
            cached.forget();
        }
        let known = cached.zones.get(&column).cloned().unwrap_or_default();
        if known.len() == groups {
            return known;
        }
        (cached.version, known)
    };
    let zones = Arc::new(compute_zones(table, column, &known));
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
            let size = self.table.row_group_size();
            let first = self.group * size;
            let last = (first + size).min(self.rows).checked_sub(1)?;
            let ids = self.table.row_ids(self.rows);
            return Some(ColumnZone {
                min: Some(Value::bigint(ids.id(first) as i64)),
                max: Some(Value::bigint(ids.id(last) as i64)),
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
            BoundExprKind::Constant(value) if value.is_null() => Range::Null,
            BoundExprKind::Constant(value) => Range::Bounds(value.clone(), value.clone()),
            // `PropagateCaseExpression`: what any branch gives
            BoundExprKind::Case { checks, else_expr } => {
                let branches = checks.iter().map(|(_, then)| then).chain(std::iter::once(&**else_expr));
                self.union(branches)
            }
            BoundExprKind::Function { function, children, .. } => self.function_range(expr, &function.name, children),
            BoundExprKind::Operator {
                op: BoundOperator::Coalesce,
                children,
            } => self.union(children.iter()),
            _ => Range::Unknown,
        }
    }

    /// The range of what is any of `exprs`.
    fn union<'e>(&self, exprs: impl Iterator<Item = &'e BoundExpr>) -> Range {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        let mut result = Range::Null;
        for expr in exprs {
            result = match (result, self.range(expr)) {
                (Range::Unknown, _) | (_, Range::Unknown) => return Range::Unknown,
                (Range::Null, other) | (other, Range::Null) => other,
                (Range::Bounds(a_min, a_max), Range::Bounds(b_min, b_max)) => Range::Bounds(
                    if is(ComparisonOp::LessThan, &b_min, &a_min) {
                        b_min
                    } else {
                        a_min
                    },
                    if is(ComparisonOp::GreaterThan, &b_max, &a_max) {
                        b_max
                    } else {
                        a_max
                    },
                ),
            };
        }
        result
    }

    /// The statistics of a function of the columns (`FunctionStatistics` of `coalesce`, `greatest`, `least`, the
    /// arithmetic operators and the date parts).
    fn function_range(&self, expr: &BoundExpr, name: &str, children: &[BoundExpr]) -> Range {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        let constant = |e: &BoundExpr| match &e.kind {
            BoundExprKind::Constant(value) if !value.is_null() => Some(value.clone()),
            _ => None,
        };
        let sign = |value: &Value| -> Option<Ordering> {
            let zero = crate::execution::parquet::try_cast_value(&Value::integer(0), value.logical_type())?;
            if is(ComparisonOp::GreaterThan, value, &zero) {
                Some(Ordering::Greater)
            } else if is(ComparisonOp::LessThan, value, &zero) {
                Some(Ordering::Less)
            } else {
                Some(Ordering::Equal)
            }
        };
        match (name, children) {
            ("coalesce", _) => self.union(children.iter()),
            ("__internal_barrier", [child]) => self.range(child),
            // a prefix of a string sorts as the string does
            ("left" | "substring" | "substr", [_, ..])
                if children[1..].iter().all(|c| constant(c).is_some())
                    && (name == "left"
                        || constant(&children[1]).and_then(|v| v.try_to_string().ok()).as_deref() == Some("1")) =>
            {
                self.monotone(expr, 0, true)
            }
            ("greatest" | "least", [_, ..]) => {
                let greatest = name == "greatest";
                let mut bounds: Option<(Value, Value)> = None;
                for child in children {
                    let Range::Bounds(min, max) = self.range(child) else {
                        return Range::Unknown;
                    };
                    bounds = Some(match bounds {
                        None => (min, max),
                        Some((a, b)) => {
                            let pick = |x: Value, y: Value| {
                                let op = if greatest {
                                    ComparisonOp::GreaterThan
                                } else {
                                    ComparisonOp::LessThan
                                };
                                if is(op, &y, &x) { y } else { x }
                            };
                            (pick(a, min), pick(b, max))
                        }
                    });
                }
                bounds.map_or(Range::Unknown, |(min, max)| Range::Bounds(min, max))
            }
            // a function of one column and a constant that keeps or reverses the order of values
            ("+" | "-" | "*" | "/" | "//", [left, right]) => {
                let (varying, increasing) = match (constant(left), constant(right)) {
                    (None, Some(c)) => match (name, sign(&c)) {
                        ("+" | "-", _) => (0, true),
                        ("*" | "/" | "//", Some(Ordering::Greater)) => (0, true),
                        ("*" | "/" | "//", Some(Ordering::Less)) => (0, false),
                        _ => return Range::Unknown,
                    },
                    (Some(c), None) => match (name, sign(&c)) {
                        ("+", _) => (1, true),
                        ("-", _) => (1, false),
                        ("*", Some(Ordering::Greater)) => (1, true),
                        ("*", Some(Ordering::Less)) => (1, false),
                        // a constant that isn't negative over divisors that are all positive: the larger the divisor,
                        // the smaller the quotient
                        ("/" | "//", Some(Ordering::Greater | Ordering::Equal)) => {
                            let Range::Bounds(min, _) = self.range(right) else {
                                return Range::Unknown;
                            };
                            if sign(&min) != Some(Ordering::Greater) {
                                return Range::Unknown;
                            }
                            (1, false)
                        }
                        _ => return Range::Unknown,
                    },
                    _ => return Range::Unknown,
                };
                self.monotone(expr, varying, increasing)
            }
            // `DatePart::MonthOperator` and the like: within one year, the parts of the bounds
            ("month" | "quarter", [child]) => match self.range(child) {
                Range::Bounds(min, max) => {
                    // the year of a DATE or TIMESTAMP is the start of its text
                    let year = |value: &Value| {
                        value
                            .try_to_string()
                            .ok()
                            .map(|text| text.chars().take(4).collect::<String>())
                    };
                    if year(&min).is_some() && year(&min) == year(&max) {
                        self.monotone(expr, 0, true)
                    } else {
                        Range::Unknown
                    }
                }
                other => other,
            },
            _ => Range::Unknown,
        }
    }

    /// The range of `expr`, whose child `varying` is the one that changes, in or against the order of its values.
    fn monotone(&self, expr: &BoundExpr, varying: usize, increasing: bool) -> Range {
        let BoundExprKind::Function { children, .. } = &expr.kind else {
            return Range::Unknown;
        };
        let at = |value: &Value| {
            let mut call = expr.clone();
            if let BoundExprKind::Function { children, .. } = &mut call.kind {
                children[varying] = BoundExpr::constant(value.clone());
            }
            try_evaluate_scalar(&call).filter(|result| !result.is_null())
        };
        match self.range(&children[varying]) {
            Range::Bounds(min, max) => match (at(&min), at(&max)) {
                (Some(low), Some(high)) if increasing => Range::Bounds(low, high),
                (Some(low), Some(high)) => Range::Bounds(high, low),
                _ => Range::Unknown,
            },
            other => other,
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
            // `BarrierStats`: the statistics of what is in it
            BoundExprKind::Function { function, children, .. }
                if function.name == "__internal_barrier" && children.len() == 1 =>
            {
                self.never_true(&children[0])
            }
            BoundExprKind::Conjunction { and: true, children } => children.iter().any(|c| self.never_true(c)),
            BoundExprKind::Conjunction { and: false, children } => children.iter().all(|c| self.never_true(c)),
            // `(x > 5) = true`: the comparison itself
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal | ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if left.return_type.id() == LogicalTypeId::Boolean
                && matches!(
                    left.kind,
                    BoundExprKind::Comparison { .. }
                        | BoundExprKind::Between { .. }
                        | BoundExprKind::Conjunction { .. }
                )
                && matches!(&right.kind, BoundExprKind::Constant(value) if value.data() == &duckrs_common::value::ValueData::Bool(true)) =>
            {
                self.never_true(left)
            }
            BoundExprKind::Comparison { op, left, right } => match (&left.kind, &right.kind) {
                // `IS NOT DISTINCT FROM` a value is `=` it, but for NULLs
                (_, BoundExprKind::Constant(constant))
                    if *op == ComparisonOp::NotDistinctFrom && !constant.is_null() =>
                {
                    self.comparison_fails(ComparisonOp::Equal, left, constant)
                }
                (_, BoundExprKind::Constant(constant)) => self.comparison_fails(*op, left, constant),
                (BoundExprKind::Constant(constant), _) => self.comparison_fails(flipped(*op), right, constant),
                // two expressions of the columns: their ranges (`PropagateComparison`)
                _ => self.ranges_fail(*op, left, right),
            },
            // `prefix(s, 'ab')` / `starts_with`: s from 'ab' up to what follows all strings that start with it
            BoundExprKind::Function { function, children, .. }
                if matches!(function.name.as_str(), "prefix" | "starts_with") && children.len() == 2 =>
            {
                let BoundExprKind::Constant(prefix) = &children[1].kind else {
                    return false;
                };
                let Ok(prefix) = prefix.try_to_string() else {
                    return false;
                };
                if prefix.is_empty() {
                    return false;
                }
                let Range::Bounds(min, max) = self.range(&children[0]) else {
                    return matches!(self.range(&children[0]), Range::Null);
                };
                let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                    return false;
                };
                let (min, max, prefix) = (min.as_bytes(), max.as_bytes(), prefix.as_bytes());
                // every string of the group sorts after the prefix's range, or before it
                let after = min > prefix && !min.starts_with(prefix);
                let before = max < prefix;
                after || before
            }
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

    /// Whether `left op right` can not hold for any two values of their ranges.
    fn ranges_fail(&self, op: ComparisonOp, left: &BoundExpr, right: &BoundExpr) -> bool {
        let is = |op, a: &Value, b: &Value| compare_values(op, a, b) == Some(true);
        let (Range::Bounds(l_min, l_max), Range::Bounds(r_min, r_max)) = (self.range(left), self.range(right)) else {
            return false;
        };
        match op {
            ComparisonOp::Equal => {
                is(ComparisonOp::LessThan, &l_max, &r_min) || is(ComparisonOp::GreaterThan, &l_min, &r_max)
            }
            ComparisonOp::NotEqual => {
                is(ComparisonOp::Equal, &l_min, &l_max)
                    && is(ComparisonOp::Equal, &r_min, &r_max)
                    && is(ComparisonOp::Equal, &l_min, &r_min)
            }
            ComparisonOp::LessThan => is(ComparisonOp::GreaterThanOrEqual, &l_min, &r_max),
            ComparisonOp::LessThanOrEqual => is(ComparisonOp::GreaterThan, &l_min, &r_max),
            ComparisonOp::GreaterThan => is(ComparisonOp::LessThanOrEqual, &l_max, &r_min),
            ComparisonOp::GreaterThanOrEqual => is(ComparisonOp::LessThan, &l_max, &r_min),
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
    let groups = row_group_count(table, rows);
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
