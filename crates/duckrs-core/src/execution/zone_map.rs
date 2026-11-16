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

/// `StringStatsData::MAX_STRING_MINMAX_SIZE`: the bytes of the bounds of strings C++ keeps.
pub const STRING_STATISTICS_BYTES: usize = 12;

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

/// `ArgProperties` of the functions that declare one: the argument whose order the result follows (`true`) or
/// reverses (`false`) while the others are constant (`PropagateMonotoneBounds`).
const MONOTONE_ARGUMENTS: &[(&str, usize, bool)] = &[
    ("atan", 0, true),
    ("asinh", 0, true),
    ("cbrt", 0, true),
    ("degrees", 0, true),
    ("radians", 0, true),
    ("ln", 0, true),
    ("log10", 0, true),
    ("log2", 0, true),
    ("sinh", 0, true),
    ("sqrt", 0, true),
    ("tanh", 0, true),
    ("trunc", 0, true),
    ("round", 0, true),
    ("round_even", 0, true),
    ("roundbankers", 0, true),
    ("epoch", 0, true),
    ("epoch_ms", 0, true),
    ("epoch_us", 0, true),
    ("epoch_ns", 0, true),
    ("era", 0, true),
    ("isoyear", 0, true),
    ("last_day", 0, true),
    ("normalized_interval", 0, true),
    ("to_timestamp", 0, true),
    ("make_date", 0, true),
    ("make_timestamp", 0, true),
    ("make_timestamp_ns", 0, true),
    ("make_timestamp_ms", 0, true),
    ("date_trunc", 1, true),
    ("datetrunc", 1, true),
    ("time_bucket", 1, true),
    ("date_diff", 1, false),
    ("date_diff", 2, true),
    ("datediff", 1, false),
    ("datediff", 2, true),
    ("date_sub", 1, false),
    ("date_sub", 2, true),
    ("datesub", 1, false),
    ("datesub", 2, true),
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

/// The argument of a call of `name` that its result follows, in order or reversed, while the others are constant.
fn monotone_argument(name: &str, children: &[BoundExpr]) -> Option<(usize, bool)> {
    MONOTONE_ARGUMENTS
        .iter()
        .find(|(function, at, _)| {
            *function == name
                && *at < children.len()
                && !children[*at].is_foldable()
                && children.iter().enumerate().all(|(i, c)| i == *at || c.is_foldable())
        })
        .map(|&(_, at, increasing)| (at, increasing))
}

/// The values an expression can have in a group.
enum Range {
    /// nothing is known
    Unknown,
    /// only NULL
    Null,
    Bounds(Value, Value),
}

/// The zones of a scan's output columns for one group: what is known of the column at each position.
struct Group<'a> {
    zone: &'a dyn Fn(usize) -> Option<ColumnZone>,
}

/// The zone of the output column `index` of a scan of `table` reading `column_ids`, in row group `group`.
fn table_zone(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    group: usize,
    rows: usize,
    index: usize,
) -> Option<ColumnZone> {
    let column = *column_ids.get(index)?;
    if column == COLUMN_IDENTIFIER_ROW_ID {
        let size = table.row_group_size();
        let first = group * size;
        let last = (first + size).min(rows).checked_sub(1)?;
        let ids = table.row_ids(rows);
        return Some(ColumnZone {
            min: Some(Value::bigint(ids.id(first) as i64)),
            max: Some(Value::bigint(ids.id(last) as i64)),
            ..ColumnZone::default()
        });
    }
    let zones = zones_of(table, column);
    let zone = zones.get(group)?;
    (!zone.unknown).then(|| zone.clone())
}

/// Whether no row can make `filter` true, where `zone` gives what is known of the column at each position of
/// the rows `filter` reads (`ExpressionFilter::CheckStatistics`).
pub fn never_true(zone: &dyn Fn(usize) -> Option<ColumnZone>, filter: &BoundExpr) -> bool {
    Group { zone }.never_true(filter)
}

impl Group<'_> {
    fn zone(&self, index: usize) -> Option<ColumnZone> {
        (self.zone)(index)
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
            BoundExprKind::Cast { child, .. } if child.return_type == expr.return_type => self.range(child),
            // a timestamp in another unit sorts the same; a value that doesn't fit leaves the range unknown
            BoundExprKind::Cast { child, try_cast: false }
                if (is_timestamp(&child.return_type) && is_timestamp(&expr.return_type))
                    || (is_timestamp_tz(&child.return_type) && is_timestamp_tz(&expr.return_type)) =>
            {
                self.through(expr, child)
            }
            BoundExprKind::Cast { child, try_cast: false } if keeps_order(&child.return_type, &expr.return_type) => {
                self.through(expr, child)
            }
            BoundExprKind::Function { function, children, .. }
                if children.len() == 1 && MONOTONE_FUNCTIONS.contains(&function.name.as_str()) =>
            {
                self.through(expr, &children[0])
            }
            BoundExprKind::Function { function, children, .. }
                if monotone_argument(&function.name, children).is_some() =>
            {
                let (varying, increasing) = monotone_argument(&function.name, children).expect("checked");
                self.monotone(expr, varying, increasing)
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
            // `s || ''` and `replace(s, a, a)` are `s`
            ("||", [input, other]) | ("||", [other, input])
                if constant(other).and_then(|v| v.try_to_string().ok()).as_deref() == Some("") =>
            {
                self.range(input)
            }
            ("replace", [input, from, to])
                if constant(from).is_some() && constant(from).map(|v| v.try_to_string().ok()) == constant(to).map(|v| v.try_to_string().ok()) =>
            {
                self.range(input)
            }
            // `x % c` of what is from 0 up to below `c` is itself, and otherwise from 0 up to below `c`
            ("%", [input, divisor]) if constant(divisor).as_ref().and_then(Value::as_i128).is_some_and(|c| c > 0) => {
                let c = constant(divisor).as_ref().and_then(Value::as_i128).expect("checked");
                let Range::Bounds(min, max) = self.range(input) else {
                    return self.range(input);
                };
                match (min.as_i128(), max.as_i128()) {
                    (Some(low), Some(high)) if low >= 0 && high < c => Range::Bounds(min, max),
                    (Some(low), Some(_)) if low >= 0 => {
                        let bound = |n: i128| crate::execution::parquet::try_cast_value(&Value::hugeint(n), &expr.return_type);
                        match (bound(0), bound(c - 1)) {
                            (Some(zero), Some(top)) => Range::Bounds(zero, top),
                            _ => Range::Unknown,
                        }
                    }
                    _ => Range::Unknown,
                }
            }
            // `isnan`: NaN sorts above every number, so the bounds tell
            ("isnan", [input]) => match self.range(input) {
                Range::Bounds(min, max) => {
                    let nan = |v: &Value| match v.data() {
                        duckrs_common::value::ValueData::Float(f) => Some(f.is_nan()),
                        duckrs_common::value::ValueData::Double(d) => Some(d.is_nan()),
                        _ => None,
                    };
                    match (nan(&min), nan(&max)) {
                        (Some(low), Some(high)) => Range::Bounds(Value::boolean(low), Value::boolean(high)),
                        _ => Range::Unknown,
                    }
                }
                other => other,
            },
            // `PowerStats`: a power above 0 of what is never negative grows with it
            ("power" | "pow", [base, exponent])
                if constant(exponent).is_some_and(|e| sign(&e) == Some(Ordering::Greater))
                    && matches!(self.range(base), Range::Bounds(min, _) if sign(&min) != Some(Ordering::Less)) =>
            {
                self.monotone(expr, 0, true)
            }
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
            ("month" | "quarter" | "dayofyear" | "doy" | "day" | "dayofmonth", [child]) => match self.range(child) {
                Range::Bounds(min, max) => {
                    // the year of a DATE or TIMESTAMP is the start of its text, the month follows it
                    let within = if matches!(name, "day" | "dayofmonth") { 7 } else { 4 };
                    let year = |value: &Value| {
                        value
                            .try_to_string()
                            .ok()
                            .map(|text| text.chars().take(within).collect::<String>())
                    };
                    if year(&min).is_some() && year(&min) == year(&max) {
                        self.monotone(expr, 0, true)
                    } else {
                        Range::Unknown
                    }
                }
                other => other,
            },
            // a deterministic function of what is one value in the group: that function of it
            _ if !expr.is_volatile() && !children.is_empty() => {
                // a column of the group with one value and no NULL, or a constant (a NULL in the group
                // would give the function of NULL too)
                let mut arguments = Vec::with_capacity(children.len());
                for child in children {
                    match &child.kind {
                        BoundExprKind::Constant(_) => arguments.push(child.clone()),
                        BoundExprKind::Reference(index) => match self.zone(*index) {
                            Some(ColumnZone {
                                min: Some(min),
                                max: Some(max),
                                has_null: false,
                                ..
                            }) if compare_values(ComparisonOp::Equal, &min, &max) == Some(true)
                                // the sign of a floating point zero is not in the statistics
                                && !matches!(min.data(), duckrs_common::value::ValueData::Float(f) if *f == 0.0)
                                && !matches!(min.data(), duckrs_common::value::ValueData::Double(d) if *d == 0.0) =>
                            {
                                arguments.push(BoundExpr::constant(min))
                            }
                            // a group of only NULLs
                            Some(ColumnZone {
                                min: None,
                                max: None,
                                has_null: true,
                                ..
                            }) => arguments.push(BoundExpr::constant(Value::null(child.return_type.clone()))),
                            _ => return Range::Unknown,
                        },
                        _ => return Range::Unknown,
                    }
                }
                let BoundExprKind::Function { function, is_operator, .. } = &expr.kind else {
                    return Range::Unknown;
                };
                let call = BoundExpr::new(
                    BoundExprKind::Function {
                        function: function.clone(),
                        children: arguments,
                        is_operator: *is_operator,
                    },
                    expr.return_type.clone(),
                );
                match crate::execution::expression_executor::try_evaluate_scalar(&call) {
                    Some(value) if value.is_null() => Range::Null,
                    Some(value) => Range::Bounds(value.clone(), value),
                    None => Range::Unknown,
                }
            }
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
            // a BOOLEAN column itself: no row of the group is true
            BoundExprKind::Reference(index) if expr.return_type.id() == LogicalTypeId::Boolean => {
                self.zone(*index).is_some_and(|zone| match &zone.max {
                    None => true,
                    Some(max) => max.data() == &duckrs_common::value::ValueData::Bool(false),
                })
            }
            // `NOT prefix(s, 'ab')`: every string of the group starts with it (a NULL gives NULL)
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1
                && matches!(&children[0].kind, BoundExprKind::Function { function, children, .. }
                    if matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") && children.len() == 2) =>
            {
                let BoundExprKind::Function { children, .. } = &children[0].kind else {
                    unreachable!("checked")
                };
                let BoundExprKind::Constant(prefix) = &children[1].kind else {
                    return false;
                };
                let (Ok(prefix), Range::Bounds(min, max)) = (prefix.try_to_string(), self.range(&children[0])) else {
                    return false;
                };
                let (Ok(min), Ok(max)) = (min.try_to_string(), max.try_to_string()) else {
                    return false;
                };
                // C++'s string statistics keep 12 bytes of the bounds, which prove no longer prefix
                prefix.len() <= STRING_STATISTICS_BYTES && min.starts_with(&prefix) && max.starts_with(&prefix)
            }
            BoundExprKind::Operator {
                op: BoundOperator::Not,
                children,
            } if children.len() == 1 && negated(without_noop_casts(&children[0])).is_some() => {
                self.never_true(&negated(without_noop_casts(&children[0])).expect("checked"))
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
                    without_noop_casts(left).kind,
                    BoundExprKind::Comparison { .. }
                        | BoundExprKind::Between { .. }
                        | BoundExprKind::Conjunction { .. }
                        | BoundExprKind::Reference(_)
                )
                && matches!(&right.kind, BoundExprKind::Constant(value) if value.data() == &duckrs_common::value::ValueData::Bool(true)) =>
            {
                self.never_true(without_noop_casts(left))
            }
            // `(x > 5) = false` and `(x > 5) IS FALSE`: the comparison never false
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal | ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if left.return_type.id() == LogicalTypeId::Boolean
                && matches!(&right.kind, BoundExprKind::Constant(value) if value.data() == &duckrs_common::value::ValueData::Bool(false))
                && !matches!(without_noop_casts(left).kind, BoundExprKind::Reference(_))
                && negated(without_noop_casts(left)).is_some() =>
            {
                self.never_true(&negated(without_noop_casts(left)).expect("checked"))
            }
            // `x IS DISTINCT FROM c`: every row of the group is `c`
            BoundExprKind::Comparison {
                op: ComparisonOp::DistinctFrom,
                left,
                right,
            } => match (&left.kind, &right.kind) {
                (_, BoundExprKind::Constant(constant)) | (BoundExprKind::Constant(constant), _)
                    if !constant.is_null() =>
                {
                    let column = if matches!(left.kind, BoundExprKind::Constant(_)) {
                        right
                    } else {
                        left
                    };
                    let zone = match &column.kind {
                        BoundExprKind::Reference(index) => self.zone(*index),
                        _ => None,
                    };
                    let is = |a: &Value| compare_values(ComparisonOp::Equal, a, constant) == Some(true);
                    zone.is_some_and(|zone| {
                        !zone.has_null && zone.min.as_ref().is_some_and(is) && zone.max.as_ref().is_some_and(is)
                    })
                }
                _ => false,
            },
            // `x IS NOT DISTINCT FROM NULL`: a group without NULLs
            BoundExprKind::Comparison {
                op: ComparisonOp::NotDistinctFrom,
                left,
                right,
            } if matches!(&right.kind, BoundExprKind::Constant(value) if value.is_null())
                && matches!(left.kind, BoundExprKind::Reference(_)) =>
            {
                let BoundExprKind::Reference(index) = left.kind else {
                    unreachable!("checked")
                };
                self.zone(index).is_some_and(|zone| !zone.has_null)
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
            // a BOOLEAN function whose range in the group is only false or NULL
            BoundExprKind::Function { function, .. }
                if expr.return_type.id() == LogicalTypeId::Boolean
                    && !matches!(function.name.as_str(), "prefix" | "starts_with" | "^@")
                    && matches!(self.range(expr), Range::Null | Range::Bounds(_, _))
                    && match self.range(expr) {
                        Range::Null => true,
                        Range::Bounds(_, max) => max.data() == &duckrs_common::value::ValueData::Bool(false),
                        Range::Unknown => false,
                    } =>
            {
                true
            }
            // `prefix(s, 'ab')` / `starts_with`: s from 'ab' up to what follows all strings that start with it
            BoundExprKind::Function { function, children, .. }
                if matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") && children.len() == 2 =>
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

/// A timestamp of any unit, without a time zone.
fn is_timestamp(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(ty.id(), TimestampSec | TimestampMs | Timestamp | TimestampNs)
}

/// A timestamp with a time zone, of any unit.
fn is_timestamp_tz(ty: &LogicalType) -> bool {
    matches!(ty.id(), LogicalTypeId::TimestampTz | LogicalTypeId::TimestampTzNs)
}

/// `expr` without the casts around it that change nothing.
fn without_noop_casts(mut expr: &BoundExpr) -> &BoundExpr {
    while let BoundExprKind::Cast { child, .. } = &expr.kind
        && child.return_type == expr.return_type
    {
        expr = child;
    }
    expr
}

/// `NOT expr` for a comparison of values (true, false or NULL as `expr` is), or a conjunction of them; `None` for
/// anything else.
fn negated(expr: &BoundExpr) -> Option<BoundExpr> {
    let kind = match &expr.kind {
        BoundExprKind::Comparison { op, left, right } => {
            let op = match op {
                ComparisonOp::Equal => ComparisonOp::NotEqual,
                ComparisonOp::NotEqual => ComparisonOp::Equal,
                ComparisonOp::LessThan => ComparisonOp::GreaterThanOrEqual,
                ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThan,
                ComparisonOp::GreaterThan => ComparisonOp::LessThanOrEqual,
                ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThan,
                _ => return None,
            };
            BoundExprKind::Comparison {
                op,
                left: left.clone(),
                right: right.clone(),
            }
        }
        BoundExprKind::Reference(_) if expr.return_type.id() == LogicalTypeId::Boolean => BoundExprKind::Comparison {
            op: ComparisonOp::Equal,
            left: Box::new(expr.clone()),
            right: Box::new(BoundExpr::constant(Value::boolean(false))),
        },
        BoundExprKind::Conjunction { and, children } => BoundExprKind::Conjunction {
            and: !and,
            children: children.iter().map(negated).collect::<Option<Vec<_>>>()?,
        },
        _ => return None,
    };
    Some(BoundExpr::new(kind, expr.return_type.clone()))
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
            let zone = |index| table_zone(table, column_ids, group, rows, index);
            !never_true(&zone, filter)
        })
        .collect()
}
