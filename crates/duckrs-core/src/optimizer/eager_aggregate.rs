// Ported from duckdb/src/optimizer/statistics/operator/propagate_aggregate.cpp (`TryExecuteAggregates`)
// @ rust-port-baseline
//
//! An ungrouped aggregate of only `count_star()`, `min` and `max` of a column of a table or a Parquet file
//! is worked out from the statistics of its row groups, where they are exact, and becomes one row of
//! constants. A filter on the scan may leave out row groups that it is never true for, and keep those it
//! is always true for; one that it is neither for has to be scanned, which C++ does not do yet
//! (partial precomputation is off at the baseline).

use super::scan_filters::{TablePushdown, table_pushdown};
use crate::catalog::{ColumnZone, TableCatalogEntry, TableCursor};
use crate::execution::zone_map;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use std::cmp::Ordering;

/// `MinMaxColumnInfo`.
#[derive(Clone)]
struct Column {
    binding: ColumnBinding,
    input_type: LogicalType,
    result_type: LogicalType,
}

enum Wanted {
    CountStar,
    Min(Column),
    Max(Column),
}

/// What a row group (`PartitionStatistics`) says.
struct Partition {
    count: u64,
    /// `CountType::COUNT_EXACT`
    count_exact: bool,
    /// `MinMaxIsExact` and not `HasPendingWrites`
    min_max_exact: bool,
    /// the zone of each column, by position of the scan
    zones: Vec<Option<ColumnZone>>,
}

/// `TryExecuteAggregates`: the expression get of the results in place of the aggregate `op`.
pub(super) fn try_execute(op: &LogicalOperator) -> Option<LogicalOperator> {
    let K::Aggregate {
        group_index: _,
        aggregate_index,
        groups,
        grouping_sets,
        aggregates,
        grouping_functions,
        ..
    } = &op.kind
    else {
        return None;
    };
    if !groups.is_empty() || aggregates.is_empty() || grouping_sets.len() > 1 || !grouping_functions.is_empty() {
        return None;
    }
    let mut wanted = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        wanted.push(wanted_of(aggregate)?);
    }
    // skip any projections
    let mut child = &op.children[0];
    while let K::Projection {
        table_index,
        expressions,
    } = &child.kind
    {
        for want in &mut wanted {
            let (Wanted::Min(column) | Wanted::Max(column)) = want else {
                continue;
            };
            if column.binding.table_index != *table_index {
                return None;
            }
            let below = column_of(expressions.get(column.binding.column_index)?)?;
            if !safe_cast(&below.result_type, &column.input_type) {
                return None;
            }
            column.binding = below.binding;
            column.input_type = below.input_type;
        }
        child = &child.children[0];
    }
    // the filters C++ has put into the scan by now
    let mut filters: Vec<&BoundExpr> = Vec::new();
    if let K::Filter { expressions } = &child.kind {
        filters.extend(expressions);
        child = &child.children[0];
    }
    let partitions = match &child.kind {
        K::Get {
            table,
            table_index,
            column_ids,
            filters: scan_filters,
            zone_filters,
            optional_filters,
            sample,
            index_rows,
            ..
        } => {
            if sample.is_some() || index_rows.is_some() || !zone_filters.is_empty() || !optional_filters.is_empty() {
                return None;
            }
            filters.extend(scan_filters);
            let columns = |binding: ColumnBinding| {
                (binding.table_index == *table_index)
                    .then(|| column_ids.get(binding.column_index).copied())
                    .flatten()
                    .filter(|&column| {
                        column != COLUMN_IDENTIFIER_ROW_ID && !table.is_generated(&table.columns[column].name)
                    })
            };
            for want in &wanted {
                if let Wanted::Min(column) | Wanted::Max(column) = want {
                    columns(column.binding)?;
                }
            }
            table_partitions(table, column_ids)?
        }
        K::ParquetScan { table_index, scan } => {
            // one file: `union_by_name` reads it with its own types
            // (several files when their footers are in the metadata cache: `partition_statistics` says)
            if !scan.extracts.is_empty() || !scan.optional_filters.is_empty() || scan.files.is_empty() {
                return None;
            }
            filters.extend(&scan.filters);
            for want in &wanted {
                if let Wanted::Min(column) | Wanted::Max(column) = want
                    && (column.binding.table_index != *table_index || column.binding.column_index >= scan.names.len())
                {
                    return None;
                }
            }
            crate::execution::parquet::partition_statistics(scan)?
                .into_iter()
                .map(|(count, zones, exact)| Partition {
                    count,
                    count_exact: true,
                    min_max_exact: exact,
                    zones,
                })
                .collect()
        }
        _ => return None,
    };
    if partitions.is_empty() {
        return None;
    }
    let scan_index = match &child.kind {
        K::Get { table_index, .. } | K::ParquetScan { table_index, .. } => *table_index,
        _ => unreachable!(),
    };
    // the filters as they read the columns of the scan by position
    let mut positional = Vec::with_capacity(filters.len());
    for filter in &filters {
        if !table_filter(filter, scan_index) {
            return None;
        }
        let mut filter = (*filter).clone();
        filter.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = e.kind {
                e.kind = BoundExprKind::Reference(binding.column_index);
            }
        });
        positional.push(filter);
    }
    // the row groups the filters are always true for; any they may be either for are scanned
    let mut kept = Vec::new();
    for partition in &partitions {
        let zone = |index: usize| partition.zones.get(index).cloned().flatten();
        let mut always = true;
        let mut never = false;
        for filter in &positional {
            if !partition.min_max_exact {
                always = false;
                break;
            }
            if zone_map::never_true(&zone, filter) {
                never = true;
                break;
            }
            always &= always_true(&zone, filter);
        }
        if never {
            continue;
        }
        if !always {
            return None;
        }
        kept.push(partition);
    }
    if kept.is_empty() {
        return None;
    }
    let mut results = Vec::with_capacity(wanted.len());
    for want in &wanted {
        let value = match want {
            Wanted::CountStar => {
                if kept.iter().any(|p| !p.count_exact) {
                    return None;
                }
                Value::bigint(i64::try_from(kept.iter().map(|p| p.count).sum::<u64>()).ok()?)
            }
            Wanted::Min(column) => extreme(&kept, column, Ordering::Less)?,
            Wanted::Max(column) => extreme(&kept, column, Ordering::Greater)?,
        };
        results.push(value);
    }
    let types: Vec<LogicalType> = results.iter().map(|v| v.logical_type().clone()).collect();
    let row = results
        .into_iter()
        .zip(aggregates)
        .map(|(value, aggregate)| BoundExpr::constant(value).with_alias(aggregate_alias(aggregate)))
        .collect();
    Some(LogicalOperator::new(
        K::ExpressionGet {
            function: None,
            volatile: false,
            materialized: false,
            table_index: *aggregate_index,
            types,
            expressions: vec![row],
        },
        Vec::new(),
    ))
}

fn aggregate_alias(aggregate: &BoundAggregate) -> String {
    aggregate.function.name.clone()
}

fn wanted_of(aggregate: &BoundAggregate) -> Option<Wanted> {
    if aggregate.filter.is_some() || aggregate.function.export.is_some() {
        return None;
    }
    match aggregate.function.name.as_str() {
        "count_star" => Some(Wanted::CountStar),
        name @ ("min" | "max") => {
            let [child] = aggregate.children.as_slice() else {
                return None;
            };
            let column = column_of(child)?;
            if !has_comparator(&column.input_type) {
                return None;
            }
            Some(if name == "min" {
                Wanted::Min(column)
            } else {
                Wanted::Max(column)
            })
        }
        _ => None,
    }
}

/// `GetComparator`: VARCHAR, numeric and temporal types have min/max statistics.
fn has_comparator(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    if ty.alias().is_some() {
        return false;
    }
    matches!(
        ty.id(),
        Varchar
            | TinyInt
            | SmallInt
            | Integer
            | BigInt
            | HugeInt
            | UTinyInt
            | USmallInt
            | UInteger
            | UBigInt
            | UHugeInt
            | Float
            | Double
            | Decimal
            | Date
            | Time
            | TimeTz
            | Timestamp
            | TimestampSec
            | TimestampMs
            | TimestampNs
            | TimestampTz
            | TimestampTzNs
    )
}

/// `TryGetMinMaxColumnInfo`: a column, or a cast of one to a wider integer.
fn column_of(expr: &BoundExpr) -> Option<Column> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(Column {
            binding: *binding,
            input_type: expr.return_type.clone(),
            result_type: expr.return_type.clone(),
        }),
        BoundExprKind::Cast { child, try_cast: false } => {
            let BoundExprKind::ColumnRef { binding, depth: 0 } = &child.kind else {
                return None;
            };
            if !safe_cast(&child.return_type, &expr.return_type) {
                return None;
            }
            Some(Column {
                binding: *binding,
                input_type: child.return_type.clone(),
                result_type: expr.return_type.clone(),
            })
        }
        _ => None,
    }
}

/// `IsSafeMinMaxCast`: the same type, or an integer widened.
fn safe_cast(source: &LogicalType, target: &LogicalType) -> bool {
    if source == target {
        return true;
    }
    if !source.is_integral() || !target.is_integral() {
        return false;
    }
    crate::function::combine_types::try_max_logical_type_unchecked(source, target).is_some_and(|max| &max == target)
}

/// A filter C++ puts into the scan as one: of one column of it, and not one it keeps above as well.
fn table_filter(filter: &BoundExpr, scan_index: usize) -> bool {
    let mut columns = std::collections::HashSet::new();
    let mut other = false;
    filter.clone().walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth } = e.kind {
            if depth == 0 && binding.table_index == scan_index {
                columns.insert(binding.column_index);
            } else {
                other = true;
            }
        }
    });
    if other || columns.len() != 1 || filter.is_volatile() {
        return false;
    }
    !matches!(
        table_pushdown(filter),
        TablePushdown::Optional(_) | TablePushdown::Partially(_)
    )
}

/// Whether C++'s statistics can tell a filter is always true (`NumericStats::CheckZonemap`): a comparison,
/// BETWEEN or IS NOT NULL of a numeric or temporal column with constants, and ANDs of those.
fn decidable(filter: &BoundExpr) -> bool {
    let numeric_column = |e: &BoundExpr| {
        matches!(e.kind, BoundExprKind::Reference(_))
            && !matches!(
                e.return_type.id(),
                LogicalTypeId::Varchar | LogicalTypeId::Blob | LogicalTypeId::Bit
            )
            && has_comparator(&e.return_type)
    };
    let constant = |e: &BoundExpr| matches!(e.kind, BoundExprKind::Constant(_));
    // `ExpressionFilter::CheckStatistics`: arithmetic of one numeric column, whose bounds the statistics propagate
    fn arithmetic(e: &BoundExpr, column: &dyn Fn(&BoundExpr) -> bool) -> bool {
        match &e.kind {
            BoundExprKind::Function { function, children, .. } => {
                matches!(function.name.as_str(), "+" | "-" | "*" | "abs")
                    && !matches!(
                        e.return_type.id(),
                        LogicalTypeId::Varchar | LogicalTypeId::Blob | LogicalTypeId::Bit
                    )
                    && has_comparator(&e.return_type)
                    && children
                        .iter()
                        .all(|c| matches!(c.kind, BoundExprKind::Constant(_)) || arithmetic(c, column))
            }
            BoundExprKind::Cast { .. } => false,
            _ => column(e),
        }
    }
    let operand = |e: &BoundExpr| numeric_column(e) || arithmetic(e, &numeric_column);
    match &filter.kind {
        BoundExprKind::Comparison { left, right, .. } => {
            (operand(left) && constant(right)) || (constant(left) && operand(right))
        }
        BoundExprKind::Between { input, lower, upper } => numeric_column(input) && constant(lower) && constant(upper),
        BoundExprKind::Operator {
            op: BoundOperator::IsNotNull,
            children,
        } => matches!(children[0].kind, BoundExprKind::Reference(_)),
        BoundExprKind::Conjunction { and: true, children } => children.iter().all(decidable),
        _ => false,
    }
}

/// `FILTER_ALWAYS_TRUE`: no row makes it false or NULL.
fn always_true(zone: &dyn Fn(usize) -> Option<ColumnZone>, filter: &BoundExpr) -> bool {
    if !decidable(filter) {
        return false;
    }
    let mut columns = Vec::new();
    filter.clone().walk_mut(&mut |e| {
        if let BoundExprKind::Reference(index) = e.kind {
            columns.push(index);
        }
    });
    if columns.iter().any(|&c| zone(c).is_none_or(|z| z.has_null)) {
        return false;
    }
    let negated = BoundExpr::new(
        BoundExprKind::Operator {
            op: BoundOperator::Not,
            children: vec![filter.clone()],
        },
        LogicalType::BOOLEAN,
    );
    zone_map::never_true(zone, &negated)
}

/// `TryGetValueFromStats` over the row groups: the smallest or largest bound, cast to the result type.
fn extreme(partitions: &[&Partition], column: &Column, keep: Ordering) -> Option<Value> {
    let mut result: Option<Value> = None;
    for partition in partitions {
        if !partition.min_max_exact {
            return None;
        }
        let zone = partition.zones.get(column.binding.column_index)?.as_ref()?;
        let bound = if keep == Ordering::Less {
            zone.min.as_ref()
        } else {
            zone.max.as_ref()
        }?;
        // string statistics are exact when the string fits in what they keep
        if let ValueData::Bytes(bytes) = bound.data()
            && bytes.len() > zone_map::STRING_STATISTICS_BYTES
        {
            return None;
        }
        let op = if keep == Ordering::Less {
            duckrs_parser::ast::ComparisonOp::LessThan
        } else {
            duckrs_parser::ast::ComparisonOp::GreaterThan
        };
        result = Some(match result {
            Some(current) if !super::statistics::compare_values(op, bound, &current)? => current,
            _ => bound.clone(),
        });
    }
    let value = result?;
    if value.logical_type() == &column.result_type {
        return Some(value);
    }
    let cast = BoundExpr::constant(value).cast_to(&column.result_type, false);
    crate::execution::expression_executor::try_evaluate_scalar(&cast)
}

/// `RowGroup::GetPartitionStats` of the row groups of a table, with a zone of each column of the scan.
fn table_partitions(table: &TableCatalogEntry, column_ids: &[usize]) -> Option<Vec<Partition>> {
    let rows = table.row_count();
    let size = table.row_group_size().max(1);
    let ids = table.row_ids(rows);
    // the rows of an open transaction are not known to be what the others see
    let pending = table.has_uncommitted_changes();
    // deletes leave the bounds of a row group's statistics as they were
    let deletes = ids.has_gaps();
    // `HasUnloadedDeletes`
    let unloaded = table.unloaded_deletes.load(std::sync::atomic::Ordering::Relaxed);
    let source = table.source();
    if let Some(source) = &source
        && !TableCursor::is_aligned(&**source, size)
    {
        // `HasUnloadedDeletes`: the deletes of the file are not known by row group
        return None;
    }
    let groups = zone_map::row_group_count(table, rows);
    let zones: Vec<Option<std::sync::Arc<Vec<ColumnZone>>>> = column_ids
        .iter()
        .map(|&column| (column != COLUMN_IDENTIFIER_ROW_ID).then(|| zone_map::zones_of(table, column)))
        .collect();
    Some(
        (0..groups)
            .map(|group| {
                let count = (rows - group * size).min(size) as u64;
                Partition {
                    count,
                    count_exact: !pending && !unloaded,
                    min_max_exact: !pending && !deletes && !unloaded,
                    zones: zones
                        .iter()
                        .map(|z| z.as_ref().and_then(|z| z.get(group)).filter(|z| !z.unknown).cloned())
                        .collect(),
                }
            })
            .collect(),
    )
}
