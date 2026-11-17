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
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
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
        geometry: None,
        strings: None,
        distinct: None,
        children: Vec::new(),
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
        && let Some(mut statistics) = table.source().and_then(|source| source.column_statistics())
    {
        add_geometry_statistics(table, None, &mut statistics)?;
        return Some(statistics);
    }
    let mut statistics = compute_ordered_statistics(table, positions.clone())?;
    add_geometry_statistics(table, positions.clone(), &mut statistics)?;
    add_nested_statistics(table, positions, &mut statistics)?;
    Some(statistics)
}

/// The statistics of the LIST, ARRAY and STRUCT columns, with their children's, from their values.
fn add_nested_statistics(
    table: &Arc<TableCatalogEntry>,
    positions: Option<Vec<usize>>,
    statistics: &mut [Stats],
) -> Option<()> {
    let nested = |ty: &LogicalType| matches!(ty.id(), LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Struct);
    let columns: Vec<usize> = (0..table.columns.len())
        .filter(|&c| nested(&table.columns[c].ty) && statistics[c].children.is_empty())
        .collect();
    if columns.is_empty() {
        return Some(());
    }
    let scan = LogicalOperator::new(
        K::Get {
            table: table.clone(),
            table_index: 0,
            column_ids: columns.clone(),
            filters: Vec::new(),
            zone_filters: Vec::new(),
            projection: None,
            index_rows: positions.map(Arc::new),
        },
        Vec::new(),
    );
    let mut plan = scan;
    resolve_column_bindings(&mut plan).ok()?;
    let result = run_plan(plan).ok()?;
    for (index, &column) in columns.iter().enumerate() {
        let ty = &table.columns[column].ty;
        let merged = (0..result.len())
            .map(|row| value_stats(&result.get_value(index, row), ty))
            .reduce(union);
        if let Some(merged) = merged {
            let distinct = statistics[column].distinct;
            statistics[column] = Stats { distinct, ..merged };
        }
    }
    Some(())
}

/// `VertexExtractStats`: the bounds of a coordinate of the points, and whether it is always NULL.
fn vertex_extract_stats(
    child: &Stats,
    geometry: &duckrs_common::geometry::GeometryStats,
    coordinate: Option<&str>,
) -> (Stats, bool) {
    use duckrs_common::geometry::{HAS_EMPTY_GEOM, HAS_NON_EMPTY_GEOM};
    let always_null = Stats {
        can_be_null: true,
        can_be_valid: false,
        ..unknown()
    };
    const POINT: u8 = 1 << 1;
    // vertex types: XY, XYZ, XYM, XYZM
    let has_vertex = |index: usize| geometry.types[index] != 0;
    let has_point = geometry.types.iter().any(|set| set & POINT != 0);
    if !has_point || geometry.flags & HAS_NON_EMPTY_GEOM == 0 {
        return (always_null, true);
    }
    let mut result = Stats {
        can_be_null: child.can_be_null,
        can_be_valid: child.can_be_valid,
        ..unknown()
    };
    if geometry.types.iter().any(|set| set & !POINT != 0) || geometry.flags & HAS_EMPTY_GEOM != 0 {
        result.can_be_null = true;
    }
    let index = match coordinate.map(str::to_lowercase).as_deref() {
        Some("x") => 0,
        Some("y") => 1,
        Some("z") => 2,
        Some("m") => 3,
        _ => return (result, false),
    };
    if index == 2 {
        if has_vertex(0) || has_vertex(2) {
            result.can_be_null = true;
        }
        if !has_vertex(1) && !has_vertex(3) {
            return (always_null, true);
        }
    }
    if index == 3 {
        if has_vertex(0) || has_vertex(1) {
            result.can_be_null = true;
        }
        if !has_vertex(2) && !has_vertex(3) {
            return (always_null, true);
        }
    }
    let extent = geometry.extent;
    let finite = |a: f64, b: f64| a.is_finite() && b.is_finite();
    let has_xy = finite(extent.x_min, extent.x_max) && finite(extent.y_min, extent.y_max);
    let bounds = match index {
        0 if has_xy => Some((extent.x_min, extent.x_max)),
        1 if has_xy => Some((extent.y_min, extent.y_max)),
        2 if finite(extent.z_min, extent.z_max) => Some((extent.z_min, extent.z_max)),
        3 if finite(extent.m_min, extent.m_max) => Some((extent.m_min, extent.m_max)),
        _ => None,
    };
    if let Some((min, max)) = bounds {
        result.min = Some(Value::double(min));
        result.max = Some(Value::double(max));
    }
    (result, false)
}

/// The `GeometryStats` of a GEOMETRY value.
fn geometry_stats_of(value: &Value) -> Option<duckrs_common::geometry::GeometryStats> {
    if value.logical_type().id() != LogicalTypeId::Geometry {
        return None;
    }
    let duckrs_common::value::ValueData::Bytes(wkb) = value.data() else {
        return None;
    };
    let mut stats = duckrs_common::geometry::GeometryStats::empty();
    stats.update(wkb).ok()?;
    Some(stats)
}

/// The `GeometryStats` of the GEOMETRY columns that don't have them yet, from a scan of those
/// columns (of the rows at `positions`).
fn add_geometry_statistics(
    table: &Arc<TableCatalogEntry>,
    positions: Option<Vec<usize>>,
    statistics: &mut [Stats],
) -> Option<()> {
    let columns: Vec<usize> = (0..table.columns.len())
        .filter(|&c| table.columns[c].ty.id() == LogicalTypeId::Geometry && statistics[c].geometry.is_none())
        .collect();
    if columns.is_empty() {
        return Some(());
    }
    let scan = LogicalOperator::new(
        K::Get {
            table: table.clone(),
            table_index: 0,
            column_ids: columns.clone(),
            filters: Vec::new(),
            zone_filters: Vec::new(),
            projection: None,
            index_rows: positions.map(Arc::new),
        },
        Vec::new(),
    );
    let mut plan = scan;
    resolve_column_bindings(&mut plan).ok()?;
    let result = run_plan(plan).ok()?;
    for (index, &column) in columns.iter().enumerate() {
        let mut stats = duckrs_common::geometry::GeometryStats::empty();
        for row in 0..result.len() {
            let value = result.get_value(index, row);
            if let duckrs_common::value::ValueData::Bytes(wkb) = value.data()
                && !value.is_null()
            {
                stats.update(wkb).ok()?;
            }
        }
        statistics[column].geometry = Some(stats);
    }
    Some(())
}

/// The min, max and NULLs of the columns, from an aggregate over the rows (at `positions`).
fn compute_ordered_statistics(table: &Arc<TableCatalogEntry>, positions: Option<Vec<usize>>) -> Option<Vec<Stats>> {
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
        // `StatsWriter<string_t>`: the shortest, longest and total length, whether there is unicode, and for a
        // BLOB the bounds, which aren't ordered statistics otherwise
        let strings = matches!(column.ty.id(), LogicalTypeId::Varchar | LogicalTypeId::Blob).then(|| aggregates.len());
        if strings.is_some() {
            let length = string_length(reference.clone());
            aggregates.push(aggregate("min", length.clone())?);
            aggregates.push(aggregate("max", length.clone())?);
            aggregates.push(aggregate("sum", length)?);
            aggregates.push(aggregate("bool_or", has_unicode(reference.clone()))?);
            aggregates.push(aggregate("min", reference.clone())?);
            aggregates.push(aggregate("max", reference.clone())?);
        }
        aggregates.push(aggregate("count", reference)?);
        columns.push((ordered, aggregates.len() - 1, strings));
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
            zone_filters: Vec::new(),
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
            perfect_hash: false,
        },
        vec![scan],
    );
    resolve_column_bindings(&mut plan).ok()?;
    let result = run_plan(plan).ok()?;
    let as_count = |value: Value| value.try_to_string().ok()?.parse::<u64>().ok();
    let total = as_count(result.get_value(count_column, 0))?;
    let mut statistics = Vec::with_capacity(rows);
    for (index, (ordered, count_at, strings_at)) in columns.into_iter().enumerate() {
        let valid = as_count(result.get_value(count_at, 0))?;
        let strings = strings_at.map(|at| {
            let number = |offset: usize| as_count(result.get_value(at + offset, 0));
            let bytes = |offset: usize| match result.get_value(at + offset, 0).data() {
                duckrs_common::value::ValueData::Bytes(bytes) => {
                    Some(bytes[..bytes.len().min(crate::catalog::StringStatistics::PREFIX_SIZE)].to_vec())
                }
                _ => None,
            };
            crate::catalog::StringStatistics {
                bounds: bytes(4).zip(bytes(5)),
                has_unicode: result.get_value(at + 3, 0).data() == &duckrs_common::value::ValueData::Bool(true),
                min_length: number(0).or(Some(u64::from(u32::MAX))),
                max_length: number(1).or(Some(0)),
                total_length: number(2).or(Some(0)),
            }
        });
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
            geometry: None,
            strings,
            distinct: None,
            children: Vec::new(),
        });
    }
    Some(statistics)
}

/// What is known of the columns of `table`. Reading all rows takes long, and a table that is written
/// to would pay for that with each statement, so this goes on from what was known: new values add to
/// it, and rows that went don't take anything away, as in C++.
pub(super) fn table_statistics(table: &Arc<TableCatalogEntry>) -> Option<Arc<Vec<Stats>>> {
    let rows = table.row_count();
    let (version, known, new_rows, updated) = {
        let mut cached = table.statistics.lock();
        if cached.unreported {
            cached.forget();
        }
        match cached.columns.clone() {
            Some(columns) if cached.covered >= rows && cached.updated.is_empty() => return Some(columns),
            Some(columns) => {
                let updated = !cached.updated.is_empty();
                let mut positions = std::mem::take(&mut cached.updated);
                positions.extend(cached.covered..rows);
                positions.retain(|position| *position < rows);
                positions.sort_unstable();
                positions.dedup();
                (cached.version, Some(columns), positions, updated)
            }
            None => (cached.version, None, Vec::new(), false),
        }
    };
    let merged = match known {
        Some(known) => {
            let Some(new) = compute_statistics(table, Some(new_rows)) else {
                table.statistics.lock().forget();
                return None;
            };
            // `total_string_length` can't take the place of the old lengths of rows that were updated
            known
                .iter()
                .cloned()
                .zip(new)
                .map(|(a, b)| union(a, b))
                .map(|mut stats| {
                    if updated && let Some(strings) = &mut stats.strings {
                        strings.total_length = None;
                    }
                    stats
                })
                .collect()
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

/// The distinct values of each column of `table`, as C++'s statistics count them
/// ([`super::distinct_statistics`]).
fn compute_distinct_counts(table: &Arc<TableCatalogEntry>) -> Option<TableDistinctCounts> {
    let columns = (0..table.columns.len())
        .map(|column| super::distinct_statistics::table_column_distinct(table, column))
        .collect();
    Some(TableDistinctCounts {
        rows: table.row_count() as u64,
        columns,
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

/// Whether `column op constant` holds for all the values of a column with `column` statistics that are
/// not NULL (`Some(true)`), for none (`Some(false)`), or is not known.
pub(super) fn decide_with_constant(op: ComparisonOp, column: &Stats, constant: &Value, ty: &LogicalType) -> Option<bool> {
    decide(op, column, &value_stats(constant, ty))
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
        geometry: match (a.geometry, b.geometry) {
            (Some(mut a), Some(b)) => {
                a.merge(&b);
                Some(a)
            }
            _ => None,
        },
        strings: match (a_valid, b_valid, a.strings, b.strings) {
            (true, false, strings, _) | (false, true, _, strings) => strings,
            (_, _, Some(a), Some(b)) => Some(a.merge(&b, true)),
            _ => None,
        },
        distinct: None,
        children: if a.children.len() == b.children.len() {
            a.children.into_iter().zip(b.children).map(|(a, b)| union(a, b)).collect()
        } else {
            Vec::new()
        },
    }
}

/// `BaseStatistics::FromConstant`: the statistics of one value, its children's too.
fn value_stats(value: &Value, ty: &LogicalType) -> Stats {
    use duckrs_common::value::ValueData;
    // no value at all: what is merged into nothing changes nothing
    let empty = |ty: &LogicalType| Stats {
        can_be_null: false,
        can_be_valid: false,
        children: empty_children(ty),
        ..unknown()
    };
    fn empty_children(ty: &LogicalType) -> Vec<Stats> {
        let none = |child: &LogicalType| Stats {
            can_be_null: false,
            can_be_valid: false,
            children: empty_children(child),
            ..unknown()
        };
        match ty.id() {
            LogicalTypeId::List | LogicalTypeId::Array => ty.child_type().map(none).into_iter().collect(),
            LogicalTypeId::Struct => ty.struct_children().iter().map(|(_, child)| none(child)).collect(),
            _ => Vec::new(),
        }
    }
    if value.is_null() {
        return Stats {
            can_be_null: true,
            can_be_valid: false,
            children: empty_children(ty),
            ..unknown()
        };
    }
    match (ty.id(), value.data()) {
        (LogicalTypeId::List | LogicalTypeId::Array, ValueData::List(items)) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let merged = items
                .iter()
                .map(|item| value_stats(item, &child))
                .reduce(union)
                .unwrap_or_else(|| empty(&child));
            Stats {
                can_be_null: false,
                can_be_valid: true,
                children: vec![merged],
                ..unknown()
            }
        }
        (LogicalTypeId::Struct, ValueData::Struct(fields)) => Stats {
            can_be_null: false,
            can_be_valid: true,
            children: ty
                .struct_children()
                .iter()
                .zip(fields)
                .map(|((_, child), field)| value_stats(field, child))
                .collect(),
            ..unknown()
        },
        _ => {
            let strings = match (ty.id(), value.data()) {
                (id @ (LogicalTypeId::Varchar | LogicalTypeId::Blob), ValueData::Bytes(bytes)) => Some(
                    crate::catalog::StringStatistics::of_constant(bytes, id == LogicalTypeId::Varchar),
                ),
                _ => None,
            };
            let bound = has_ordered_statistics(ty).then(|| value.clone());
            Stats {
                min: bound.clone(),
                max: bound,
                can_be_null: false,
                can_be_valid: true,
                geometry: geometry_stats_of(value),
                strings,
                distinct: None,
                children: Vec::new(),
            }
        }
    }
}

/// The length in bytes of a string, as `strlen` without a value per row, for what a table's statistics say.
fn string_length(input: BoundExpr) -> BoundExpr {
    fn run(
        args: &duckrs_common::vector::DataChunk,
        result: &mut duckrs_common::vector::Vector,
    ) -> duckrs_common::Result<()> {
        let count = args.len();
        let strings = args.column(0).to_unified(count);
        result.prepare_flat(count.max(1));
        for row in 0..count {
            if strings.is_valid(row) {
                result.data_mut::<i64>()[row] = strings.str_at(row).len() as i64;
            } else {
                result.set_null(row, true);
            }
        }
        Ok(())
    }
    internal_call("__string_length", input, LogicalType::BIGINT, run)
}

/// Whether a VARCHAR has anything but ASCII in it (`Utf8Proc::Analyze`); never for a BLOB.
fn has_unicode(input: BoundExpr) -> BoundExpr {
    fn run(
        args: &duckrs_common::vector::DataChunk,
        result: &mut duckrs_common::vector::Vector,
    ) -> duckrs_common::Result<()> {
        let count = args.len();
        let varchar = args.column(0).logical_type().id() == LogicalTypeId::Varchar;
        let strings = args.column(0).to_unified(count);
        result.prepare_flat(count.max(1));
        for row in 0..count {
            if strings.is_valid(row) {
                result.data_mut::<bool>()[row] = varchar && !strings.str_at(row).is_ascii();
            } else {
                result.set_null(row, true);
            }
        }
        Ok(())
    }
    internal_call("__string_has_unicode", input, LogicalType::BOOLEAN, run)
}

fn internal_call(
    name: &str,
    input: BoundExpr,
    return_type: LogicalType,
    run: fn(&duckrs_common::vector::DataChunk, &mut duckrs_common::vector::Vector) -> duckrs_common::Result<()>,
) -> BoundExpr {
    let function =
        crate::function::ScalarFunction::new(name, vec![input.return_type.clone()], return_type.clone(), run);
    BoundExpr::new(
        BoundExprKind::Function {
            function: Arc::new(function),
            children: vec![input],
            is_operator: false,
        },
        return_type,
    )
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

/// Runs the pass over a plan, and gives what it found out of the columns of the plan
/// (`GetStatisticsMap`), if it ran.
pub fn propagate_statistics(plan: &mut LogicalOperator, perfect_ht_threshold: u64) -> Option<StatisticsMap> {
    if !has_unique_tables(plan, &mut HashSet::new()) {
        return None;
    }
    let mut propagator = StatisticsPropagator {
        perfect_ht_threshold,
        ..Default::default()
    };
    propagator.visit(plan);
    Some(StatisticsMap(propagator))
}

/// `StatisticsPropagator` with `StatisticsPropagationMode::FILTER_SIMPLIFICATION`, before the joins
/// are ordered: it also takes the branches out of filters that statistics tell cannot be true, and
/// leaves the joins' range filters and the choice of aggregates for the full pass. Gives whether a
/// filter now reads other tables than before (`FilterBindingsChanged`).
pub fn simplify_filters(plan: &mut LogicalOperator) -> bool {
    if !has_unique_tables(plan, &mut HashSet::new()) {
        return false;
    }
    let mut propagator = StatisticsPropagator {
        filter_simplification: true,
        ..Default::default()
    };
    propagator.visit(plan);
    propagator.filter_bindings_changed
}

/// The statistics of the columns of a plan after `propagate_statistics` (`statistics_map`).
pub struct StatisticsMap(StatisticsPropagator);

impl StatisticsMap {
    /// What is known of the values of `binding`.
    pub fn get(&mut self, binding: ColumnBinding) -> ColumnStatistics {
        self.0.lookup(binding)
    }

    /// `CopyStatistics`: `target` has the values of `source`.
    pub fn copy(&mut self, source: ColumnBinding, target: ColumnBinding) {
        if source != target {
            self.0.stats.remove(&target);
            self.0.aliases.insert(target, source);
        }
    }
}

#[derive(Default)]
struct StatisticsPropagator {
    /// `perfect_ht_threshold`: the most bits the groups of a perfect hash aggregate take
    perfect_ht_threshold: u64,
    /// what is known by now
    stats: HashMap<ColumnBinding, Stats>,
    /// the table columns a binding reads, whose statistics nobody asked for yet
    sources: HashMap<ColumnBinding, (Arc<TableCatalogEntry>, usize)>,
    /// bindings that are another binding under a new name
    aliases: HashMap<ColumnBinding, ColumnBinding>,
    /// bindings an outer join puts NULLs in
    nullable: HashSet<ColumnBinding>,
    /// `StatisticsPropagationMode::FILTER_SIMPLIFICATION`
    filter_simplification: bool,
    /// `filter_bindings_changed`
    filter_bindings_changed: bool,
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
                geometry: Some(duckrs_common::geometry::GeometryStats::empty())
                    .filter(|_| expr.return_type.id() == LogicalTypeId::Geometry),
                strings: None,
                distinct: None,
                children: Vec::new(),
            },
            BoundExprKind::Constant(value)
                if matches!(
                    expr.return_type.id(),
                    LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Struct
                ) =>
            {
                value_stats(value, &expr.return_type)
            }
            BoundExprKind::Constant(value) => {
                let geometry = geometry_stats_of(value);
                let strings = match (expr.return_type.id(), value.data()) {
                    (
                        id @ (LogicalTypeId::Varchar | LogicalTypeId::Blob),
                        duckrs_common::value::ValueData::Bytes(bytes),
                    ) => Some(crate::catalog::StringStatistics::of_constant(
                        bytes,
                        id == LogicalTypeId::Varchar,
                    )),
                    _ => None,
                };
                let value = has_ordered_statistics(&expr.return_type).then(|| value.clone());
                Stats {
                    min: value.clone(),
                    max: value,
                    can_be_null: false,
                    can_be_valid: true,
                    geometry,
                    strings,
                    distinct: None,
                    children: Vec::new(),
                }
            }
            BoundExprKind::ColumnRef { binding, depth: 0 } => {
                let binding = *binding;
                self.lookup(binding)
            }
            BoundExprKind::Comparison { .. } => self.comparison(expr, in_filter),
            BoundExprKind::Between { .. } => self.between(expr, in_filter),
            BoundExprKind::Conjunction { children, and } => {
                for child in children.iter_mut() {
                    self.expression(child, in_filter);
                }
                if *and {
                    simplify_constant_or_null(children);
                    if children.len() == 1 {
                        let only = children.remove(0);
                        *expr = only;
                    }
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
                op: BoundOperator::Try | BoundOperator::HasError,
                ..
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
        // a side that is only NULL makes NULL; one without any value (a column of an empty table)
        // prunes a filter (`FILTER_ALWAYS_FALSE`), but a projection keeps the comparison
        let only_null = |stats: &Stats| stats.can_be_null && !stats.can_be_valid;
        let empty = |stats: &Stats| !stats.can_be_null && !stats.can_be_valid;
        if only_null(&left_stats) || only_null(&right_stats) {
            let alias = expr.alias.clone();
            *expr = BoundExpr::constant(Value::null(LogicalType::BOOLEAN)).with_alias(alias);
            return unknown();
        }
        if empty(&left_stats) || empty(&right_stats) {
            if in_filter {
                let alias = expr.alias.clone();
                *expr = BoundExpr::constant(Value::boolean(false)).with_alias(alias);
            }
            return unknown();
        }
        if let Some(answer) = decide(op, &left_stats, &right_stats) {
            // `FILTER_TRUE_OR_NULL` / `FILTER_FALSE_OR_NULL`: NULL with either side, which both stay
            let sides = if left_stats.can_be_null || right_stats.can_be_null {
                vec![(**left).clone(), (**right).clone()]
            } else {
                Vec::new()
            };
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
        // `prefix` of strings that all start with it, or that all sort outside of the strings that do
        if matches!(function.name.as_str(), "prefix" | "starts_with" | "^@")
            && let [child, needle] = stats.as_slice()
            && let BoundExprKind::Constant(needle_value) = &children[1].kind
            && !needle_value.is_null()
            && needle.can_be_valid
            && let (Some(min), Some(max)) = (&child.min, &child.max)
            && let (Ok(min), Ok(max), Ok(prefix)) = (min.try_to_string(), max.try_to_string(), needle_value.try_to_string())
            && !children[0].is_volatile()
        {
            // C++'s string statistics keep 12 bytes of the bounds
            let provable = prefix.len() <= crate::execution::zone_map::STRING_STATISTICS_BYTES;
            let answer = if !provable {
                None
            } else if min.starts_with(&prefix) && max.starts_with(&prefix) {
                Some(true)
            } else if max.as_str() < prefix.as_str() || (min.as_str() > prefix.as_str() && !min.starts_with(&prefix)) {
                Some(false)
            } else {
                None
            };
            if let Some(answer) = answer {
                let alias = expr.alias.clone();
                let can_be_null = child.can_be_null;
                let constant = BoundExpr::constant(Value::boolean(answer));
                // NULL where the string is
                *expr = if can_be_null {
                    crate::function::constant_or_null(vec![constant, children[0].clone()])
                } else {
                    constant
                }
                .with_alias(alias);
                return Stats {
                    min: Some(Value::boolean(answer)),
                    max: Some(Value::boolean(answer)),
                    can_be_null,
                    ..unknown()
                };
            }
        }
        // a function that gives NULL for NULL, of an argument that is only NULL, is only NULL
        if matches!(
            function.name.as_str(),
            "array_extract" | "list_extract" | "list_element" | "map_extract_value" | "split_part" | "lower" | "upper"
                | "length" | "substring" | "substr" | "replace" | "trim" | "ltrim" | "rtrim" | "left" | "right"
        ) && !expr_is_volatile_function(&function.name)
            && stats.iter().any(|s| s.can_be_null && !s.can_be_valid)
        {
            return Stats {
                can_be_null: true,
                can_be_valid: false,
                ..unknown()
            };
        }
        // `SubstringPropagateStats`, `SubstringGraphemePropagateStats` and `RepeatStringStats`: the longest
        // string of the result
        if let Some(max_length) = string_function_max_length(&function.name, children, &stats) {
            let child = &stats[0];
            // `PropagateStrictFunctionValidity`
            let mut result = Stats {
                can_be_null: stats.iter().any(|s| s.can_be_null),
                can_be_valid: stats.iter().all(|s| s.can_be_valid),
                strings: Some(crate::catalog::StringStatistics {
                    bounds: None,
                    has_unicode: child.strings.as_ref().is_none_or(|s| s.has_unicode),
                    min_length: None,
                    max_length: Some(max_length),
                    total_length: None,
                }),
                ..unknown()
            };
            if function.name == "repeat" {
                // the result is known to fit in a string
                *function = Arc::new(function.infallible());
                result.can_be_valid = child.can_be_valid && stats[1].can_be_valid;
            }
            return result;
        }
        // `left(s, 0)` is the empty string
        if function.name == "left"
            && let [child, _] = stats.as_slice()
            && matches!(&children[1].kind, BoundExprKind::Constant(v) if v.as_i128() == Some(0))
        {
            return Stats {
                min: Some(Value::varchar("")),
                max: Some(Value::varchar("")),
                can_be_null: child.can_be_null,
                can_be_valid: child.can_be_valid,
                ..unknown()
            };
        }
        // `CaseConvertPropagateStats`: `lower` and `upper` are NULL only for a NULL, and say nothing of the
        // lengths of their strings
        if matches!(function.name.as_str(), "lower" | "upper" | "lcase" | "ucase")
            && let [child] = stats.as_slice()
        {
            return Stats {
                can_be_null: child.can_be_null,
                can_be_valid: child.can_be_valid,
                ..unknown()
            };
        }
        // `StringStats` of `length`: the longest string bounds it, and so does the shortest without unicode
        if matches!(function.name.as_str(), "length" | "len" | "char_length" | "character_length" | "strlen" | "octet_length")
            && let [child] = stats.as_slice()
            && let Some(strings) = &child.strings
            && let Some(max) = strings.max_length
        {
            let bytes = matches!(function.name.as_str(), "strlen" | "octet_length");
            let shortest = strings.min_length.unwrap_or(0);
            // a character takes at most 4 bytes
            let min = if bytes || !strings.has_unicode { shortest } else { shortest.div_ceil(4) };
            return Stats {
                min: Some(Value::bigint(min as i64)),
                max: Some(Value::bigint(max as i64)),
                can_be_null: child.can_be_null,
                ..unknown()
            };
        }
        // `isnan` of a float whose largest value is a number: NaN sorts above every number
        if function.name == "isnan"
            && let [child] = stats.as_slice()
            && !child.can_be_null
            && child.max.as_ref().is_some_and(|max| match max.data() {
                duckrs_common::value::ValueData::Float(f) => !f.is_nan(),
                duckrs_common::value::ValueData::Double(d) => !d.is_nan(),
                _ => false,
            })
            && !children[0].is_volatile()
        {
            let alias = expr.alias.clone();
            *expr = BoundExpr::constant(Value::boolean(false)).with_alias(alias);
            return Stats {
                min: Some(Value::boolean(false)),
                max: Some(Value::boolean(false)),
                can_be_null: false,
                ..unknown()
            };
        }
        // `PropagateLeastGreatestStats`: one of the inputs; only those that are never NULL anchor the far side
        if matches!(function.name.as_str(), "least" | "greatest")
            && !children.is_empty()
            && return_type.is_numeric()
            && stats.iter().all(|s| s.min.is_some() && s.max.is_some())
        {
            let least = function.name == "least";
            let pick = |a: Option<Value>, b: Option<Value>, keep_smaller: bool| {
                if keep_smaller { smaller(a, b) } else { larger(a, b) }
            };
            let (mut loose, mut anchored, mut fallback): (Option<Value>, Option<Value>, Option<Value>) =
                (None, None, None);
            let mut has_nonnull = false;
            for child in &stats {
                let (loose_value, anchored_value) = if least {
                    (child.min.clone(), child.max.clone())
                } else {
                    (child.max.clone(), child.min.clone())
                };
                loose = if loose.is_none() {
                    loose_value.clone()
                } else {
                    pick(loose, loose_value, least)
                };
                fallback = if fallback.is_none() {
                    anchored_value.clone()
                } else {
                    pick(fallback, anchored_value.clone(), !least)
                };
                if !child.can_be_null {
                    has_nonnull = true;
                    anchored = if anchored.is_none() {
                        anchored_value
                    } else {
                        pick(anchored, anchored_value, least)
                    };
                }
            }
            if !has_nonnull {
                anchored = fallback;
            }
            let (min, max) = if least { (loose, anchored) } else { (anchored, loose) };
            return Stats {
                min,
                max,
                can_be_null: !has_nonnull,
                can_be_valid: true,
                geometry: None,
                strings: None,
                distinct: None,
                children: Vec::new(),
            };
        }
        // `StructPackStats`, `ListValueStats`, `StructExtractStats`, `ListExtractStats`
        match function.name.as_str() {
            "struct_pack" | "row" if return_type.id() == LogicalTypeId::Struct => {
                return Stats {
                    can_be_null: false,
                    can_be_valid: true,
                    children: stats,
                    ..unknown()
                };
            }
            // `StructConcatStats`, `StructInsertStats`, `StructUpdateStats`: a struct, also of NULL inputs
            "struct_concat" | "struct_insert" | "struct_update" if return_type.id() == LogicalTypeId::Struct => {
                return Stats {
                    can_be_null: false,
                    can_be_valid: true,
                    ..unknown()
                };
            }
            "list_value" | "list_pack" if matches!(return_type.id(), LogicalTypeId::List) => {
                let child = stats.into_iter().reduce(union).unwrap_or_else(|| Stats {
                    can_be_null: false,
                    can_be_valid: false,
                    ..unknown()
                });
                return Stats {
                    can_be_null: false,
                    can_be_valid: true,
                    children: vec![child],
                    ..unknown()
                };
            }
            "struct_extract" if children.len() == 2 && children[0].return_type.id() == LogicalTypeId::Struct => {
                let position = match &children[1].kind {
                    BoundExprKind::Constant(key) => match key.data() {
                        duckrs_common::value::ValueData::Bytes(name) => {
                            let name = String::from_utf8_lossy(name);
                            children[0]
                                .return_type
                                .struct_children()
                                .iter()
                                .position(|(field, _)| field.eq_ignore_ascii_case(&name))
                        }
                        _ => key.as_i128().and_then(|i| usize::try_from(i).ok()),
                    },
                    _ => None,
                };
                if let Some(child) = position.and_then(|at| stats[0].children.get(at)) {
                    let mut child = child.clone();
                    child.can_be_null |= stats[0].can_be_null;
                    return child;
                }
                return unknown();
            }
            "array_extract" | "list_extract" | "list_element"
                if children.len() == 2
                    && matches!(children[0].return_type.id(), LogicalTypeId::List | LogicalTypeId::Array) =>
            {
                // an index past the end is NULL
                if let Some(child) = stats[0].children.first() {
                    let mut child = child.clone();
                    child.can_be_null = true;
                    return child;
                }
                return unknown();
            }
            _ => {}
        }
        // `StatsPropagateStats`: `stats()` gets the statistics of its argument as a constant
        if function.name == "stats" && children.len() == 1 {
            let argument = children[0].return_type.clone();
            // `DistinctStatistics` of a table's column: only what a column of it is has them
            let mut argument_stats = stats[0].clone();
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &children[0].kind
                && let Some((table, column)) = self.sources.get(&self.base_of(*binding)).cloned()
            {
                argument_stats.distinct = table_distinct_counts(&table)
                    .and_then(|counts| counts.columns.get(column).copied().flatten())
                    .filter(|count| *count > 0);
            }
            if crate::function::stats_supported(&argument)
                && let Some(value) = stats_struct(&argument_stats, &argument)
            {
                *function = Arc::new(crate::function::stats_with_value(argument));
                children.push(BoundExpr::constant(value));
            }
            return unknown();
        }
        // `DateTruncStatistics`: with a constant part, the bounds are those of the truncated bounds
        if matches!(function.name.as_str(), "date_trunc" | "datetrunc")
            && children.len() == 2
            && return_type.id() == LogicalTypeId::Timestamp
            && matches!(
                children[1].return_type.id(),
                LogicalTypeId::Date | LogicalTypeId::Timestamp
            )
        {
            let part = match &children[0].kind {
                BoundExprKind::Constant(value) if !value.is_null() => value.try_to_string().ok(),
                _ => None,
            };
            let supported = part.is_some_and(|part| {
                matches!(
                    part.to_lowercase().as_str(),
                    "millennium" | "millennia" | "mil" | "mils" | "millenia"
                        | "century" | "centuries" | "cent" | "c"
                        | "decade" | "decades" | "dec" | "decs"
                        | "year" | "years" | "y" | "yr" | "yrs"
                        | "quarter" | "quarters"
                        | "month" | "months" | "mon" | "mons"
                        | "week" | "weeks" | "w" | "weekofyear" | "yearweek"
                        | "isoyear"
                        | "day" | "days" | "d" | "dayofmonth"
                        | "dow" | "dayofweek" | "weekday" | "isodow" | "doy" | "dayofyear" | "julian" | "jd"
                        | "hour" | "hours" | "h" | "hr" | "hrs"
                        | "minute" | "minutes" | "min" | "mins" | "m"
                        | "second" | "seconds" | "sec" | "secs" | "s" | "epoch"
                        | "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "msecond" | "mseconds"
                        | "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "usecond" | "useconds"
                )
            });
            let (Some(min), Some(max)) = (stats[1].min.clone(), stats[1].max.clone()) else {
                return unknown();
            };
            if !supported || compare_values(ComparisonOp::GreaterThan, &min, &max) != Some(false) {
                return unknown();
            }
            let truncate = |bound: Value| {
                let call = BoundExpr {
                    kind: BoundExprKind::Function {
                        function: function.clone(),
                        children: vec![children[0].clone(), BoundExpr::constant(bound)],
                        is_operator: *is_operator,
                    },
                    ..BoundExpr::new(BoundExprKind::Reference(0), return_type.clone())
                };
                crate::execution::expression_executor::try_evaluate_scalar(&call)
                    .filter(|value| value.logical_type().id() == LogicalTypeId::Timestamp)
            };
            let (Some(min), Some(max)) = (truncate(min), truncate(max)) else {
                return unknown();
            };
            return Stats {
                min: Some(min),
                max: Some(max),
                can_be_null: stats[0].can_be_null || stats[1].can_be_null,
                can_be_valid: stats[0].can_be_valid && stats[1].can_be_valid,
                geometry: None,
                strings: None,
                distinct: None,
                children: Vec::new(),
            };
        }
        let part = match (function.name.as_str(), children.len()) {
            ("date_part" | "datepart", 2) => match &children[0].kind {
                BoundExprKind::Constant(value) if !value.is_null() => value
                    .try_to_string()
                    .ok()
                    .map(|part| format!("date_part:{}", part.to_lowercase())),
                _ => None,
            },
            (name, 1) => Some(name.to_string()),
            _ => None,
        };
        if let Some(part) = part {
            let at = children.len() - 1;
            if let Some(result) = date_part_stats(&part, &stats[at], &children[at].return_type, &return_type) {
                return result;
            }
        }
        if function.name == "vertex_extract" && children.len() == 2 {
            let coordinate = match &children[1].kind {
                BoundExprKind::Constant(value) => value.try_to_string().ok(),
                _ => None,
            };
            let Some(geometry) = stats[0].geometry else {
                return unknown();
            };
            let (result, always_null) = vertex_extract_stats(&stats[0], &geometry, coordinate.as_deref());
            if always_null {
                let alias = expr.alias.clone();
                *expr = BoundExpr::constant(Value::null(LogicalType::DOUBLE)).with_alias(alias);
            }
            return result;
        }
        if function.name == "constant_or_null" && stats[1..].iter().all(|s| !s.can_be_null) {
            let alias = expr.alias.clone();
            let value = children.swap_remove(0);
            *expr = value.with_alias(alias);
            return stats.swap_remove(0);
        }
        let as_int = |value: &Option<Value>| value.as_ref()?.try_to_string().ok()?.parse::<i128>().ok();
        let int_value = |n: i128| {
            let cast = BoundExpr::constant(Value::hugeint(n)).cast_to(&return_type, false);
            crate::execution::expression_executor::try_evaluate_scalar(&cast)
                .filter(|v| v.logical_type() == &return_type)
        };
        // `PropagateAbsStats`: of integers whose smallest value is not the type's minimum
        if function.name == "abs" && children.len() == 1 && return_type.is_integral() {
            let child = &stats[0];
            let (Some(low), Some(high)) = (as_int(&child.min), as_int(&child.max)) else {
                return unknown();
            };
            let type_min = int_value(i128::MIN).is_none() && int_value(low - 1).is_none();
            if type_min {
                return unknown();
            }
            // `PropagateAbsStats`: of values that are never negative, abs is the value itself
            if low >= 0 {
                let child_stats = child.clone();
                let alias = expr.alias.clone();
                let BoundExprKind::Function { children, .. } = &mut expr.kind else {
                    unreachable!()
                };
                let value = children.swap_remove(0);
                *expr = value.with_alias(alias);
                return child_stats;
            }
            let Some(negated_low) = low.checked_neg() else {
                return unknown();
            };
            let (min, max) = if low >= 0 {
                (low, high)
            } else if high <= 0 {
                (-high, negated_low)
            } else {
                (0, negated_low.max(high))
            };
            return Stats {
                min: int_value(min),
                max: int_value(max),
                can_be_null: child.can_be_null,
                can_be_valid: child.can_be_valid,
                geometry: None,
                strings: None,
                distinct: None,
                children: Vec::new(),
            };
        }
        if children.len() == 2 && return_type.is_integral() && matches!(function.name.as_str(), "%" | "//") {
            let (left, right) = (&stats[0], &stats[1]);
            // `ModuloFullRange`: a dividend without statistics spans its type
            let (lmin, lmax) = match (as_int(&left.min), as_int(&left.max)) {
                (Some(min), Some(max)) => (min, max),
                _ if function.name == "%" => match integer_range(&return_type) {
                    Some(range) => range,
                    None => return unknown(),
                },
                _ => return unknown(),
            };
            let (Some(rmin), Some(rmax)) = (as_int(&right.min), as_int(&right.max)) else {
                return unknown();
            };
            let zero_divisor = rmin <= 0 && rmax >= 0;
            let can_be_null = left.can_be_null || right.can_be_null || zero_divisor;
            let bounds = if function.name == "%" {
                // `SignedModuloBounds`; a divisor range with 0 in it raises, so nothing is derived
                if zero_divisor {
                    None
                } else {
                    let (mut min, mut max) = (lmin.min(0), lmax.max(0));
                    let (Some(abs_min), Some(abs_max)) = (rmin.checked_abs(), rmax.checked_abs()) else {
                        return unknown();
                    };
                    let max_abs = abs_min.max(abs_max);
                    let min_abs = if zero_divisor { 0 } else { abs_min.min(abs_max) };
                    if !zero_divisor && -min_abs < lmin && lmax < min_abs {
                        (min, max) = (lmin, lmax);
                    } else {
                        let bound = max_abs - 1;
                        min = min.max(-bound);
                        max = max.min(bound);
                    }
                    Some((min, max))
                }
            } else if zero_divisor {
                // `DividePropagateStatistics`: a divisor range with 0 in it says nothing
                None
            } else {
                let corners: Option<Vec<i128>> = [(lmin, rmin), (lmin, rmax), (lmax, rmin), (lmax, rmax)]
                    .iter()
                    .map(|(a, b)| a.checked_div(*b))
                    .collect();
                corners.map(|c| (*c.iter().min().expect("four"), *c.iter().max().expect("four")))
            };
            let Some((min, max)) = bounds else {
                return unknown();
            };
            return Stats {
                min: int_value(min),
                max: int_value(max),
                can_be_null,
                can_be_valid: true,
                geometry: None,
                strings: None,
                distinct: None,
                children: Vec::new(),
            };
        }
        let floating = matches!(return_type.id(), LogicalTypeId::Float | LogicalTypeId::Double);
        if !*is_operator || children.len() != 2 || !(return_type.is_integral() || floating) {
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
            self.handle_filter(&mut expr);
            // `constant_or_null(false, x)` is false or NULL, which a filter does not let through
            let never = matches!(&expr.kind, BoundExprKind::Function { function, children, .. }
                if function.name == "constant_or_null"
                    && matches!(&children[0].kind, BoundExprKind::Constant(v) if v.data() == &ValueData::Bool(false)));
            if never {
                *op = empty_result(op);
                return;
            }
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
        simplify_constant_or_null(&mut kept);
        // as a table filter (which a filter that can fail keeps it from becoming)
        if !kept.iter().any(crate::optimizer::rewriter::can_throw) {
            kept = kept.into_iter().map(|expr| true_or_null_as_not_null(expr, &op.children[0])).collect();
        }
        // `x != a AND x != b ...` that leaves no value of an integer column's range (a NOT IN list
        // covering it) is never true
        let mut excluded: HashMap<ColumnBinding, HashSet<i128>> = HashMap::new();
        for expr in &kept {
            if let BoundExprKind::Comparison {
                op: ComparisonOp::NotEqual,
                left,
                right,
            } = &expr.kind
            {
                let (column, constant) = match (&left.kind, &right.kind) {
                    (BoundExprKind::ColumnRef { binding, depth: 0 }, BoundExprKind::Constant(c))
                    | (BoundExprKind::Constant(c), BoundExprKind::ColumnRef { binding, depth: 0 }) => (*binding, c),
                    _ => continue,
                };
                if let Some(value) = constant.as_i128() {
                    excluded.entry(column).or_default().insert(value);
                }
            }
        }
        for (column, values) in excluded {
            let stats = self.lookup(column);
            let (Some(min), Some(max)) = (
                stats.min.as_ref().and_then(Value::as_i128),
                stats.max.as_ref().and_then(Value::as_i128),
            ) else {
                continue;
            };
            if stats.min.as_ref().is_some_and(|v| !v.logical_type().is_integral()) {
                continue;
            }
            if max >= min && max - min < values.len() as i128 + 1 && (min..=max).all(|v| values.contains(&v)) {
                *op = empty_result(op);
                return;
            }
        }
        if kept.is_empty() {
            *op = op.children.remove(0);
        } else {
            *expressions = kept;
        }
    }

    /// `HandleFilter`, up to classifying the result.
    fn handle_filter(&mut self, expr: &mut BoundExpr) {
        let before = self.filter_simplification.then(|| filter_bindings(expr));
        self.expression(expr, true);
        rewrite_expression(expr, true);
        if let Some(before) = before {
            simplify_filter(expr);
            self.filter_bindings_changed |= before != filter_bindings(expr);
        }
    }

    /// `PropagateStatistics(LogicalJoin)`.
    fn join(&mut self, op: &mut LogicalOperator) {
        let K::Join {
            join_type, conditions, ..
        } = &mut op.kind
        else {
            unreachable!()
        };
        let join_type = *join_type;
        let matches_only = matches!(join_type, JoinType::Inner | JoinType::Semi | JoinType::RightSemi);
        let mut no_rows = false;
        let mut right_is_empty = false;
        // the filters the ranges of the keys after the join make for each side (`CreateFilterFromJoinStats`)
        let mut range_filters: [Vec<BoundExpr>; 2] = [Vec::new(), Vec::new()];
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
                let (a, b) = (*a, *b);
                self.update_columns(ComparisonOp::Equal, a, b);
                for (side, (expr, before)) in [(&condition.left, &left), (&condition.right, &right)].into_iter().enumerate() {
                    let binding = if side == 0 { a } else { b };
                    let after = self.lookup(binding);
                    range_filters[side].extend(join_range_filters(expr, before, &after));
                }
            }
        }
        for (side, filters) in range_filters.into_iter().enumerate() {
            // C++ evaluates the filters already on a scan before those it adds: one that can fail fails
            // on the rows the range would have left out, as here it would not
            // `CreateFilterFromJoinStats` waits for the joins to be ordered
            if filters.is_empty() || no_rows || self.filter_simplification || has_failing_filter(&op.children[side]) {
                continue;
            }
            let child = std::mem::replace(&mut op.children[side], LogicalOperator::new(K::CrossProduct, Vec::new()));
            let filtered = LogicalOperator::new(K::Filter { expressions: filters }, vec![child]);
            op.children[side] = super::filter_pushdown::push_down(filtered);
        }
        let K::Join {
            conditions,
            predicate,
            ..
        } = &mut op.kind
        else {
            unreachable!()
        };
        // `PropagateStatistics(LogicalAnyJoin)`: the condition of an outer join is an expression, not a
        // filter: an OR keeps a branch that is false or NULL. What duckrs keeps as the condition of an
        // inner join, C++ has as a filter above it
        let in_filter = join_type == JoinType::Inner;
        if let Some(condition) = predicate {
            if in_filter {
                self.handle_filter(condition);
            } else {
                self.expression(condition, in_filter);
                rewrite_expression(condition, in_filter);
            }
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
                aggregate_index,
                groups,
                grouping_sets,
                aggregates,
                grouping_functions,
                perfect_hash,
                ..
            } if grouping_sets.len() <= 1 && grouping_sets.iter().all(|set| set.len() == groups.len()) => {
                *perfect_hash = !self.filter_simplification
                    && grouping_functions.is_empty()
                    && !groups.is_empty()
                    && !aggregates.iter().any(|a| a.distinct)
                    && self.perfect_hash_bits(groups).is_some_and(|bits| bits <= self.perfect_ht_threshold);
                for (index, group) in groups.iter().enumerate() {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &group.kind {
                        self.aliases.insert(ColumnBinding::new(*group_index, index), *binding);
                    }
                }
                let (aggregate_index, aggregates) = (*aggregate_index, aggregates.clone());
                let rows = max_cardinality(&op.children[0]);
                for (index, aggregate) in aggregates.iter().enumerate() {
                    if let Some(stats) = self.aggregate_stats(aggregate, rows) {
                        self.stats.insert(ColumnBinding::new(aggregate_index, index), stats);
                    }
                }
            }
            K::Union { table_index, types } => {
                let (table_index, columns) = (*table_index, types.len());
                self.union_of_children(op, table_index, columns);
            }
            // `PropagateStatistics(LogicalSetOperation)`: EXCEPT and INTERSECT have values of the left side
            K::ExceptIntersect { table_index, types, .. } => {
                let (table_index, columns) = (*table_index, types.len());
                let left = op.children[0].column_bindings();
                for (column, binding) in left.into_iter().enumerate().take(columns) {
                    let stats = self.lookup(binding);
                    self.stats.insert(ColumnBinding::new(table_index, column), stats);
                }
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

impl StatisticsPropagator {
    /// The bits the groups take in a perfect hash table (`CanUsePerfectHashAggregate`): each needs
    /// its range of values, plus one for NULL and one to count from one. Groups must be integers,
    /// with statistics unless their type is small enough to take its whole range.
    fn perfect_hash_bits(&mut self, groups: &[BoundExpr]) -> Option<u64> {
        use duckrs_common::types::PhysicalType::*;
        let mut total = 0;
        for group in groups {
            let ty = &group.return_type;
            let physical = ty.physical_type();
            if !matches!(physical, Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64) {
                return None;
            }
            let stats = self.expression(&mut group.clone(), false);
            let known = stats.min.as_ref().and_then(Value::as_i128).zip(stats.max.as_ref().and_then(Value::as_i128));
            let (min, max) = match known {
                Some(range) => range,
                None if matches!(physical, Int8 | Int16 | UInt8 | UInt16) => integer_range(ty)?,
                None => return None,
            };
            if max < min {
                return None;
            }
            let range = u64::try_from(max - min).ok()?;
            if range >= i32::MAX as u64 {
                return None;
            }
            // `RequiredBitsForValue`
            total += u64::from(64 - (range + 2).leading_zeros());
        }
        Some(total)
    }
}

/// Whether calls of the function `name` give something else each time.
/// The longest string `substring`, `substring_grapheme` or `repeat` gives for arguments with `stats`
/// (`StringStats::SetMaxStringLength` of their statistics functions).
fn string_function_max_length(name: &str, children: &[BoundExpr], stats: &[Stats]) -> Option<u64> {
    let constant = |expr: &BoundExpr| match &expr.kind {
        BoundExprKind::Constant(value) if !value.is_null() => value.try_to_string().ok()?.parse::<i128>().ok(),
        _ => None,
    };
    let input_max = stats.first()?.strings.as_ref().and_then(|s| s.max_length);
    match name {
        "substring" | "substr" if (2..=3).contains(&children.len()) => {
            // `TryGetSubstringStatsParameters`: a constant offset from 1 on, and a constant positive length
            let mut parameters = None;
            if let Some(mut offset) = constant(&children[1]) {
                let count = match children.get(2) {
                    None => Some(None),
                    Some(length) => constant(length).filter(|&l| l > 0 && l <= i128::from(u32::MAX)).map(Some),
                };
                if let Some(mut count) = count {
                    let mut valid = true;
                    if offset == 0 {
                        match count {
                            Some(n) if n > 1 => {
                                count = Some(n - 1);
                                offset = 1;
                            }
                            _ => valid = false,
                        }
                    }
                    if valid && offset >= 1 {
                        parameters = Some(count);
                    }
                }
            }
            let unicode = stats[0].strings.as_ref().is_none_or(|s| s.has_unicode);
            let mut max = parameters.flatten().and_then(|count| {
                let per_character = if unicode { 4 } else { 1 };
                (count <= i128::from(u32::MAX) / per_character).then(|| (count * per_character) as u64)
            });
            if let Some(input_max) = input_max {
                max = Some(max.map_or(input_max, |m| m.min(input_max)));
            }
            max
        }
        "substring_grapheme" if (2..=3).contains(&children.len()) => input_max,
        "repeat" if children.len() == 2 => {
            let count = constant(&children[1])?;
            let input_max = input_max?;
            let size = if count > 0 { input_max.checked_mul(u64::try_from(count).ok()?)? } else { 0 };
            (size <= u64::from(u32::MAX)).then_some(size)
        }
        _ => None,
    }
}

/// Whether the filters down from `op` to its scan include one that can fail.
/// `GetFilterBindings`: the tables a filter reads.
fn filter_bindings(expr: &BoundExpr) -> HashSet<usize> {
    let mut tables = HashSet::new();
    let mut expr = expr.clone();
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            tables.insert(binding.table_index);
        }
    });
    tables
}

/// `ExpressionIsConstantOrNull(expr, false)` and `ExpressionIsConstant(expr, value)`.
fn constant_answer(expr: &BoundExpr) -> Option<(bool, bool)> {
    match &expr.kind {
        BoundExprKind::Constant(value) => match value.data() {
            ValueData::Bool(b) => Some((*b, false)),
            _ => None,
        },
        BoundExprKind::Function { function, children, .. } if function.name == "constant_or_null" => {
            match &children.first()?.kind {
                BoundExprKind::Constant(value) => match value.data() {
                    ValueData::Bool(b) => Some((*b, true)),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// `StatisticsPropagator::SimplifyFilter`: in a filter, a branch that is false or NULL decides an
/// AND and drops out of an OR.
fn simplify_filter(condition: &mut BoundExpr) -> bool {
    let BoundExprKind::Conjunction { and, children } = &mut condition.kind else {
        return false;
    };
    let is_and = *and;
    let mut changed = false;
    let mut index = 0;
    while index < children.len() {
        changed |= simplify_filter(&mut children[index]);
        let answer = constant_answer(&children[index]);
        let is_true = answer == Some((true, false));
        let false_or_null = matches!(answer, Some((false, _)));
        if (is_and && false_or_null) || (!is_and && is_true) {
            *condition = BoundExpr::constant(Value::boolean(!is_and));
            return true;
        }
        if (is_and && is_true) || (!is_and && false_or_null) {
            children.remove(index);
            changed = true;
            continue;
        }
        index += 1;
    }
    if children.is_empty() {
        *condition = BoundExpr::constant(Value::boolean(is_and));
        return true;
    }
    if children.len() == 1 {
        let only = children.remove(0);
        *condition = only;
        return true;
    }
    changed
}

fn has_failing_filter(op: &LogicalOperator) -> bool {
    match &op.kind {
        K::Filter { expressions } => {
            expressions.iter().any(crate::optimizer::rewriter::can_throw) || op.children.iter().any(has_failing_filter)
        }
        K::Get { filters, .. } => filters.iter().any(crate::optimizer::rewriter::can_throw),
        K::Projection { .. } | K::ColumnSelect { .. } => op.children.iter().any(has_failing_filter),
        _ => false,
    }
}

/// `CreateFilterFromJoinStats`: the bounds a join left of an integer key column, as filters for its side.
fn join_range_filters(expr: &BoundExpr, before: &Stats, after: &Stats) -> Vec<BoundExpr> {
    if !matches!(expr.kind, BoundExprKind::ColumnRef { depth: 0, .. }) || !expr.return_type.is_integral() {
        return Vec::new();
    }
    let (Some(min_before), Some(max_before), Some(min_after), Some(max_after)) =
        (&before.min, &before.max, &after.min, &after.max)
    else {
        return Vec::new();
    };
    let compare = |op, value: &Value| {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(expr.clone()),
                right: Box::new(BoundExpr::constant(value.clone()).cast_to(&expr.return_type, false)),
            },
            LogicalType::BOOLEAN,
        )
    };
    let mut filters = Vec::new();
    if compare_values(ComparisonOp::GreaterThan, min_after, min_before) == Some(true) {
        filters.push(compare(ComparisonOp::GreaterThanOrEqual, min_after));
    }
    if compare_values(ComparisonOp::LessThan, max_after, max_before) == Some(true) {
        filters.push(compare(ComparisonOp::LessThanOrEqual, max_after));
    }
    filters
}

/// `FilterRejectsNull`: whether `filter` is never true where `expr` is NULL.
fn filter_rejects_null(filter: &BoundExpr, expr: &BoundExpr) -> bool {
    let same = |e: &BoundExpr| format!("{:?}", e.kind) == format!("{:?}", expr.kind);
    match &filter.kind {
        BoundExprKind::Conjunction { and: true, children } => children.iter().any(|c| filter_rejects_null(c, expr)),
        BoundExprKind::Conjunction { and: false, children } => {
            !children.is_empty() && children.iter().all(|c| filter_rejects_null(c, expr))
        }
        BoundExprKind::Operator {
            op: BoundOperator::IsNotNull,
            children,
        } => children.first().is_some_and(same),
        BoundExprKind::Comparison { op, left, right }
            if !matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) =>
        {
            same(left) || same(right)
        }
        _ => false,
    }
}

/// `SimplifyConstantOrNull`: `constant_or_null(true, x, ...)` among conjuncts that are each NULL where one of
/// `x, ...` is adds nothing, as long as removing it lets no conjunct see more rows that can fail.
fn simplify_constant_or_null(expressions: &mut Vec<BoundExpr>) {
    let is_candidate = |e: &BoundExpr| {
        matches!(&e.kind, BoundExprKind::Function { function, children, .. }
            if function.name == "constant_or_null"
                && children.len() >= 2
                && matches!(&children[0].kind, BoundExprKind::Constant(v) if v.data() == &ValueData::Bool(true)))
    };
    if expressions.len() < 2 || !expressions.iter().any(is_candidate) {
        return;
    }
    if expressions
        .iter()
        .any(|e| crate::optimizer::rewriter::can_throw(e) || e.is_volatile() || crate::optimizer::rewriter::contains_barrier(e))
    {
        return;
    }
    let mut at = 0;
    while at < expressions.len() {
        let redundant = is_candidate(&expressions[at]) && {
            let BoundExprKind::Function { children, .. } = &expressions[at].kind else {
                unreachable!("a candidate")
            };
            children[1..].iter().all(|child| match &child.kind {
                BoundExprKind::Constant(value) => !value.is_null(),
                _ => expressions
                    .iter()
                    .enumerate()
                    .any(|(other, filter)| other != at && filter_rejects_null(filter, child)),
            })
        };
        if redundant {
            expressions.remove(at);
        } else {
            at += 1;
        }
    }
}

/// `PropagateStatistics(LogicalGet)` of a `FILTER_TRUE_OR_NULL` table filter: `constant_or_null(true, ...)`
/// of one column of a scan, and constants that are not NULL, is NULL just when the column is, and
/// filters as `column IS NOT NULL` (`IsNullOnlyForNullColumn`).
fn true_or_null_as_not_null(expr: BoundExpr, child: &LogicalOperator) -> BoundExpr {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return expr;
    };
    if function.name != "constant_or_null"
        || children.len() < 2
        || !matches!(&children[0].kind, BoundExprKind::Constant(v) if v.data() == &ValueData::Bool(true))
        || !matches!(child.kind, K::Get { .. })
    {
        return expr;
    }
    let scan = child.column_bindings();
    let mut column: Option<&BoundExpr> = None;
    for argument in &children[1..] {
        match &argument.kind {
            BoundExprKind::ColumnRef { binding, depth: 0 } if scan.contains(binding) => {
                let same = |c: &BoundExpr| matches!(&c.kind, BoundExprKind::ColumnRef { binding: b, .. } if b == binding);
                if column.is_some_and(|c| !same(c)) {
                    return expr;
                }
                column = Some(argument);
            }
            BoundExprKind::Constant(value) if !value.is_null() => {}
            _ => return expr,
        }
    }
    let Some(column) = column else {
        return expr;
    };
    BoundExpr::new(
        BoundExprKind::Operator {
            op: BoundOperator::IsNotNull,
            children: vec![column.clone()],
        },
        LogicalType::BOOLEAN,
    )
}

fn expr_is_volatile_function(name: &str) -> bool {
    crate::planner::expression::VOLATILE_FUNCTIONS.contains(&name)
}

/// The smallest and largest value of an integer type that fits in an `i128`.
fn integer_range(ty: &LogicalType) -> Option<(i128, i128)> {
    use LogicalTypeId::*;
    Some(match ty.id() {
        TinyInt => (i128::from(i8::MIN), i128::from(i8::MAX)),
        SmallInt => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Integer => (i128::from(i32::MIN), i128::from(i32::MAX)),
        BigInt => (i128::from(i64::MIN), i128::from(i64::MAX)),
        UTinyInt => (0, i128::from(u8::MAX)),
        USmallInt => (0, i128::from(u16::MAX)),
        UInteger => (0, i128::from(u32::MAX)),
        UBigInt => (0, i128::from(u64::MAX)),
        _ => return None,
    })
}

/// The most rows an operator can give, where that is known (`NodeStatistics::max_cardinality`).
fn max_cardinality(op: &LogicalOperator) -> Option<u64> {
    match &op.kind {
        K::Get { table, .. } => Some(table.row_count() as u64),
        K::Filter { .. } | K::Projection { .. } => op.children.first().and_then(max_cardinality),
        _ => None,
    }
}

impl StatisticsPropagator {
    /// `SumPropagateStats`: a sum of integers lies between the smallest and largest value times the rows.
    fn aggregate_stats(&mut self, aggregate: &BoundAggregate, rows: Option<u64>) -> Option<Stats> {
        if aggregate.children.len() != 1 {
            return None;
        }
        let child = &aggregate.children[0];
        // `MinMaxPropagateStats`: the smallest or largest value is one of the values
        if matches!(aggregate.function.name.as_str(), "min" | "max") && aggregate.return_type == child.return_type {
            let mut stats = self.expression(&mut child.clone(), false);
            stats.can_be_null = true;
            return (stats.min.is_some() && stats.max.is_some()).then_some(stats);
        }
        if aggregate.function.name != "sum" || aggregate.distinct {
            return None;
        }
        let boolean = child.return_type.id() == LogicalTypeId::Boolean;
        if !(child.return_type.is_integral() || boolean) || !aggregate.return_type.is_integral() {
            return None;
        }
        let rows = i128::from(rows?);
        let stats = self.expression(&mut child.clone(), false);
        let as_int = |value: &Option<Value>| match value.as_ref()?.data() {
            duckrs_common::value::ValueData::Bool(b) => Some(i128::from(*b)),
            _ => value.as_ref()?.try_to_string().ok()?.parse::<i128>().ok(),
        };
        let (low, high) = (as_int(&stats.min)?, as_int(&stats.max)?);
        let (wide_low, wide_high) = (low.checked_mul(rows)?, high.checked_mul(rows)?);
        let min = if low <= 0 { wide_low } else { low };
        let max = if high >= 0 { wide_high } else { high };
        let cast = |n: i128| {
            let value = BoundExpr::constant(Value::hugeint(n)).cast_to(&aggregate.return_type, false);
            crate::execution::expression_executor::try_evaluate_scalar(&value)
        };
        Some(Stats {
            min: cast(min),
            max: cast(max),
            can_be_null: true,
            can_be_valid: true,
            geometry: None,
            strings: None,
            distinct: None,
            children: Vec::new(),
        })
    }
}

/// The call of `function` with these two values.
/// `BaseStatistics::ToStruct` of numeric statistics (`NumericStats::ToStruct`), as a VARIANT
/// (`StatsPropagateStats`). duckrs keeps no distinct counts, so there is no `approx_unique`.
fn stats_struct(stats: &Stats, ty: &LogicalType) -> Option<Value> {
    let fields = stats_fields(stats, ty)?;
    let variant = LogicalType::new(LogicalTypeId::Variant);
    let source = duckrs_common::vector::Vector::constant(&fields);
    let mut result = duckrs_common::vector::Vector::new(variant, 1);
    crate::function::cast::cast_vector(&source, &mut result, 1, crate::function::cast::CastMode::Strict).ok()?;
    Some(result.get_value(0))
}

/// `BaseStatistics::ToStruct`, as a STRUCT value.
fn stats_fields(stats: &Stats, ty: &LogicalType) -> Option<Value> {
    let mut fields = Vec::new();
    // `ListStats::ToStruct` / `ArrayStats::ToStruct` / `StructStats::ToStruct`: the children's
    let child = |at: usize, child_type: &LogicalType| -> Option<Value> {
        let unknown_child = unknown();
        stats_fields(stats.children.get(at).unwrap_or(&unknown_child), child_type)
    };
    match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Array => {
            fields.push(("child_stats".to_string(), child(0, ty.child_type()?)?));
        }
        LogicalTypeId::Struct => {
            let mut children = Vec::new();
            for (at, (name, child_type)) in ty.struct_children().iter().enumerate() {
                children.push((name.clone(), child(at, child_type)?));
            }
            fields.push(("child_stats".to_string(), Value::struct_value(children)));
        }
        _ => {}
    }
    // `StringStats::ToStruct`
    if matches!(ty.id(), LogicalTypeId::Varchar | LogicalTypeId::Blob) {
        let strings = stats.strings.as_ref()?;
        if let Some((min, max)) = &strings.bounds {
            // `Blob::ToString`
            let text = |bytes: &[u8]| Value::varchar(duckrs_common::string_cast::format_blob(bytes));
            fields.push(("min".to_string(), text(min)));
            fields.push(("max".to_string(), text(max)));
        }
        fields.push(("has_unicode".to_string(), Value::boolean(strings.has_unicode)));
        for (name, value) in [
            ("min_string_length", strings.min_length),
            ("max_string_length", strings.max_length),
            ("total_string_length", strings.total_length),
        ] {
            if let Some(value) = value {
                fields.push((name.to_string(), Value::ubigint(value)));
            }
        }
    } else if let (Some(min), Some(max)) = (&stats.min, &stats.max)
        && ty.id() != LogicalTypeId::SqlNull
    {
        fields.push(("min".to_string(), min.clone()));
        fields.push(("max".to_string(), max.clone()));
    }
    // `GeometryStats::ToStruct`
    if ty.id() == LogicalTypeId::Geometry {
        let geometry = stats.geometry?;
        let extent = geometry.extent;
        let mut bounds = vec![
            ("x_min", extent.x_min),
            ("x_max", extent.x_max),
            ("y_min", extent.y_min),
            ("y_max", extent.y_max),
        ];
        if extent.z_min.is_finite() || extent.z_max.is_finite() {
            bounds.extend([("z_min", extent.z_min), ("z_max", extent.z_max)]);
        }
        if extent.m_min.is_finite() || extent.m_max.is_finite() {
            bounds.extend([("m_min", extent.m_min), ("m_max", extent.m_max)]);
        }
        let bounds = bounds
            .into_iter()
            .map(|(name, value)| (name.to_string(), Value::double(value)))
            .collect();
        fields.push(("extent".to_string(), Value::struct_value(bounds)));
        use duckrs_common::geometry::{HAS_EMPTY_GEOM, HAS_EMPTY_PART, HAS_NON_EMPTY_GEOM, HAS_NON_EMPTY_PART};
        for (name, flag) in [
            ("has_empty_geom", HAS_EMPTY_GEOM),
            ("has_non_empty_geom", HAS_NON_EMPTY_GEOM),
            ("has_empty_part", HAS_EMPTY_PART),
            ("has_non_empty_part", HAS_NON_EMPTY_PART),
        ] {
            fields.push((name.to_string(), Value::boolean(geometry.flags & flag != 0)));
        }
    }
    fields.push(("has_null".to_string(), Value::boolean(stats.can_be_null)));
    fields.push(("has_no_null".to_string(), Value::boolean(stats.can_be_valid)));
    if let Some(distinct) = stats.distinct {
        fields.push(("approx_unique".to_string(), Value::ubigint(distinct)));
    }
    Some(Value::struct_value(fields))
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

/// A date part of a constant, `name` being a unary part function or `date_part:<part>`.
fn evaluate_part(name: &str, value: &Value) -> Option<Value> {
    use crate::execution::expression_executor::try_evaluate_scalar;
    let call = match name.strip_prefix("date_part:") {
        Some(part) => crate::function::dates::bind_date_function(
            "date_part",
            vec![BoundExpr::constant(Value::varchar(part)), BoundExpr::constant(value.clone())],
        )
        .ok()?,
        None => {
            let set = BuiltinFunctions::get().scalar(name)?;
            let children = vec![BoundExpr::constant(value.clone())];
            crate::function::binder::bind_scalar_function(set, children, false, &try_evaluate_scalar).ok()?
        }
    };
    try_evaluate_scalar(&call).filter(|value| !value.is_null())
}

/// `DatePart::*Operator::PropagateStatistics`: the bounds of a date part of a DATE, TIMESTAMP or
/// TIME whose bounds are known and finite. `name` is the unary part function (or
/// `date_part:<part>`).
fn date_part_stats(name: &str, child: &Stats, child_type: &LogicalType, return_type: &LogicalType) -> Option<Stats> {
    use LogicalTypeId as T;
    let time = matches!(child_type.id(), T::Time | T::TimeNs);
    if !time && !matches!(child_type.id(), T::Date | T::Timestamp | T::TimestampSec | T::TimestampMs | T::TimestampNs) {
        return None;
    }
    let (min, max) = (child.min.clone()?, child.max.clone()?);
    if compare_values(ComparisonOp::GreaterThan, &min, &max) != Some(false) {
        return None;
    }
    let finite = |value: &Value| !value.try_to_string().is_ok_and(|text| text.contains("infinity"));
    if !finite(&min) || !finite(&max) {
        return None;
    }
    let bigint = |low: i64, high: i64| (Some(Value::bigint(low)), Some(Value::bigint(high)));
    // the parts of the parent period, which must agree for the endpoints to be exact bounds
    let day: &[&str] = if time { &[] } else { &["year", "month", "day"] };
    let within = |parents: &[&str], low: i64, high: i64| -> Option<(Option<Value>, Option<Value>)> {
        let same = parents
            .iter()
            .map(|parent| Some(evaluate_part(parent, &min)? == evaluate_part(parent, &max)?))
            .collect::<Option<Vec<bool>>>()?
            .into_iter()
            .all(|same| same);
        Some(if same {
            (Some(evaluate_part(name, &min)?), Some(evaluate_part(name, &max)?))
        } else {
            bigint(low, high)
        })
    };
    let with = |extra: &[&'static str]| -> Vec<&str> { day.iter().copied().chain(extra.iter().copied()).collect() };
    let (low, high) = match name {
        // `EpochOperator::PropagateStatistics<dtime_t>`: any second of the day
        "date_part:epoch" | "epoch" if child_type.id() == T::Time => {
            (Some(Value::double(0.0)), Some(Value::double(86_400.0)))
        }
        // `YearWeekOperator::PropagateStatistics`: from ISO year 0 on down the week is subtracted,
        // and the endpoints no longer bound the values between them
        "yearweek" | "date_part:yearweek"
            if evaluate_part("date_part:isoyear", &min)?.as_i128().is_none_or(|year| year < 1) =>
        {
            return None;
        }
        "year" | "decade" | "century" | "millennium" | "yearweek" | "date_part:yearweek" | "date_part:isoyear" | "isoyear" | "epoch_ms"
        | "epoch_us" | "epoch_ns" | "date_part:epoch" | "epoch" | "date_part:julian" | "julian" => {
            (Some(evaluate_part(name, &min)?), Some(evaluate_part(name, &max)?))
        }
        "month" if !time => within(&["year"], 1, 12)?,
        "day" | "dayofmonth" if !time => within(&["year", "month"], 1, 31)?,
        "quarter" if !time => within(&["year"], 1, 4)?,
        "dayofyear" if !time => within(&["year"], 1, 366)?,
        "week" | "weekofyear" if !time => within(&["date_part:isoyear"], 1, 53)?,
        "dayofweek" | "weekday" if !time => bigint(0, 6),
        "isodow" if !time => bigint(1, 7),
        "date_part:era" | "era" if !time => bigint(0, 1),
        "hour" => within(day, 0, 24)?,
        "minute" => within(&with(&["hour"]), 0, 59)?,
        "second" => within(&with(&["hour", "minute"]), 0, 59)?,
        "millisecond" => within(&with(&["hour", "minute"]), 0, 59_999)?,
        "microsecond" => within(&with(&["hour", "minute"]), 0, 59_999_999)?,
        "nanosecond" => within(&with(&["hour", "minute"]), 0, 59_999_999_999)?,
        _ => return None,
    };
    let (low, high) = (low?, high?);
    if low.logical_type() != return_type || high.logical_type() != return_type {
        return None;
    }
    Some(Stats {
        min: Some(low),
        max: Some(high),
        can_be_null: child.can_be_null,
        can_be_valid: child.can_be_valid,
        geometry: None,
        strings: None,
        distinct: None,
        children: Vec::new(),
    })
}
