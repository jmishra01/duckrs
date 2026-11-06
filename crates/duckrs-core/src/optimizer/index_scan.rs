// Ported from duckdb/src/function/table/table_scan.cpp (`TableScanInitGlobal`, `TryScanIndex`) and
// duckdb/src/execution/index/art/art.cpp (`TryInitializeScan`) @ rust-port-baseline
//
//! Whether an ART can answer the filters of a scan. C++ decides this when the scan starts, by trying:
//! it finds a key on the one column with filters, and gives up if more rows match than are worth
//! fetching. Planning and running a statement are one step here, so this pass tries the same way
//! and leaves the plan with the rows. The filters stay in the scan, whose result they keep correct
//! whatever this finds, as long as it finds all rows that match.

use super::OptimizerOptions;
use super::rewriter::split_conjuncts;
use crate::catalog::{IndexTree, RowIds, TableCatalogEntry, TableKey};
use crate::execution::art::key::{ArtKey, chunk_keys};
use crate::execution::constraints::index_tree;
use crate::execution::run_plan;
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::sync::Arc;

/// What a key is asked.
#[derive(Debug, Default)]
struct Probe {
    points: Vec<Value>,
    low: Option<(Value, bool)>,
    high: Option<(Value, bool)>,
}

/// Whether they compute the same, whatever they are called.
fn same_expression(a: &BoundExpr, b: &BoundExpr) -> bool {
    let unnamed = |expression: &BoundExpr| {
        let mut expression = expression.clone();
        expression.walk_mut(&mut |e| e.alias.clear());
        expression.to_string()
    };
    a.return_type == b.return_type && unnamed(a) == unnamed(b)
}

/// The constant of a comparison of `key` with one, and the operator as if `key` came first.
fn compared(key: &BoundExpr, expr: &BoundExpr) -> Option<(ComparisonOp, Value)> {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return None;
    };
    let same = |side: &BoundExpr| same_expression(side, key);
    let flipped = match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => *other,
    };
    match (&left.kind, &right.kind) {
        (_, BoundExprKind::Constant(value)) if same(left) && !value.is_null() => Some((*op, value.clone())),
        (BoundExprKind::Constant(value), _) if same(right) && !value.is_null() => Some((flipped, value.clone())),
        _ => None,
    }
}

/// `TryInitializeScan`: values to look up, or at most one bound on each side. The filters still run
/// on what is found, so with values those that say more are only left to do that.
fn probe_of(key: &BoundExpr, conjuncts: &[BoundExpr]) -> Option<Probe> {
    let mut probe = Probe::default();
    let mut unused = false;
    for conjunct in conjuncts {
        if let BoundExprKind::Between { input, lower, upper } = &conjunct.kind {
            match (&lower.kind, &upper.kind) {
                (BoundExprKind::Constant(low), BoundExprKind::Constant(high))
                    if same_expression(input, key) && !low.is_null() && !high.is_null() =>
                {
                    probe.low = Some((low.clone(), true));
                    probe.high = Some((high.clone(), true));
                }
                _ => unused = true,
            }
            continue;
        }
        // the rewriter leaves a list of values as ORs of equalities
        if let BoundExprKind::Conjunction { and: false, children } = &conjunct.kind {
            let values: Option<Vec<Value>> = children
                .iter()
                .map(|child| match compared(key, child)? {
                    (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom, value) => Some(value),
                    _ => None,
                })
                .collect();
            match values {
                Some(values) => probe.points.extend(values),
                None => unused = true,
            }
            continue;
        }
        match compared(key, conjunct) {
            // of a constant that isn't NULL, which is what `compared` gives
            Some((ComparisonOp::Equal | ComparisonOp::NotDistinctFrom, value)) => probe.points.push(value),
            Some((op @ (ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual), value))
                if probe.low.is_none() =>
            {
                probe.low = Some((value, op == ComparisonOp::GreaterThanOrEqual));
            }
            Some((op @ (ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual), value)) if probe.high.is_none() => {
                probe.high = Some((value, op == ComparisonOp::LessThanOrEqual));
            }
            _ => unused = true,
        }
    }
    if probe.points.is_empty() && unused {
        return None;
    }
    (!probe.points.is_empty() || probe.low.is_some() || probe.high.is_some()).then_some(probe)
}

fn key_of(value: &Value, ty: &LogicalType) -> Result<Option<ArtKey>> {
    let mut vector = Vector::new(ty.clone(), 1);
    vector.set_value(0, value)?;
    Ok(chunk_keys(&[&vector], 1)?.pop().flatten())
}

/// The rows that `probe` finds in `tree`, or `None` if they are more than `limit`.
fn rows_of(
    index: &IndexTree,
    ids: &RowIds,
    probe: &Probe,
    ty: &LogicalType,
    limit: usize,
) -> Result<Option<Vec<usize>>> {
    let tree = &index.tree;
    let same_type = |value: &Value| value.logical_type() == ty;
    if !probe.points.iter().all(same_type) || !probe.low.iter().chain(&probe.high).all(|(value, _)| same_type(value)) {
        return Ok(None);
    }
    let mut rows = Vec::new();
    if probe.points.is_empty() {
        let bound = |bound: &Option<(Value, bool)>| -> Result<Option<(ArtKey, bool)>> {
            bound
                .as_ref()
                .and_then(|(value, inclusive)| key_of(value, ty).transpose().map(|key| Ok((key?, *inclusive))))
                .transpose()
        };
        let (low, high) = (bound(&probe.low)?, bound(&probe.high)?);
        let low = low.as_ref().map(|(key, inclusive)| (key.as_bytes(), *inclusive));
        let high = high.as_ref().map(|(key, inclusive)| (key.as_bytes(), *inclusive));
        match tree.range(low, high, limit)? {
            Some(found) => rows = found,
            None => return Ok(None),
        }
    } else {
        for value in &probe.points {
            if let Some(key) = key_of(value, ty)? {
                rows.extend(tree.lookup(&key)?);
            }
            if rows.len() > limit {
                return Ok(None);
            }
        }
    }
    let mut rows: Vec<usize> = rows.into_iter().filter_map(|id| ids.position(id as u64)).collect();
    rows.sort_unstable();
    rows.dedup();
    Ok(Some(rows))
}

/// `TryScanIndex` for one key: only one that reads a single column, with a single expression.
fn try_key(
    table: &TableCatalogEntry,
    key: &TableKey,
    column: usize,
    conjuncts: &[BoundExpr],
    limit: usize,
) -> Result<Option<Vec<usize>>> {
    if key.columns.as_slice() != [column] {
        return Ok(None);
    }
    let expression = match &key.expressions {
        None => BoundExpr::new(BoundExprKind::Reference(column), table.columns[column].ty.clone()),
        Some(expressions) if expressions.len() == 1 => expressions[0].clone(),
        Some(_) => return Ok(None),
    };
    let Some(probe) = probe_of(&expression, conjuncts) else {
        return Ok(None);
    };
    let tree = index_tree(table, key)?;
    let ids = table.row_ids(table.row_count());
    rows_of(&tree, &ids, &probe, &expression.return_type, limit)
}

/// The filters in terms of columns of the table, if they all are on one column of it.
fn filters_on_one_column(column_ids: &[usize], filters: &[BoundExpr]) -> Option<(usize, Vec<BoundExpr>)> {
    let mut column = None;
    let mut conjuncts = Vec::new();
    for filter in filters {
        split_conjuncts(filter.clone(), &mut conjuncts);
    }
    let mut same_column = true;
    for conjunct in &mut conjuncts {
        conjunct.walk_mut(&mut |expr| {
            if let BoundExprKind::ColumnRef { binding, .. } = &expr.kind {
                let table_column = column_ids[binding.column_index];
                same_column &=
                    table_column != COLUMN_IDENTIFIER_ROW_ID && *column.get_or_insert(table_column) == table_column;
                expr.kind = BoundExprKind::Reference(table_column);
            }
        });
    }
    Some((column.filter(|_| same_column)?, conjuncts))
}

fn index_rows(
    table: &TableCatalogEntry,
    column_ids: &[usize],
    filters: &[BoundExpr],
    options: &OptimizerOptions,
) -> Option<Vec<usize>> {
    let (column, conjuncts) = filters_on_one_column(column_ids, filters)?;
    let total = table.row_count();
    let limit = (options.index_scan_max_count as usize).max((total as f64 * options.index_scan_percentage) as usize);
    table.scan_keys().iter().find_map(|key| {
        // what can't be looked up is for the scan to find out
        try_key(table, key, column, &conjuncts, limit).ok().flatten()
    })
}

/// Gives the scans that an ART can answer their rows.
pub fn plan_index_scans(op: &mut LogicalOperator, options: &OptimizerOptions) {
    for child in &mut op.children {
        plan_index_scans(child, options);
    }
    if let K::LateralJoin { right, .. } = &mut op.kind {
        plan_index_scans(right, options);
    }
    if let K::Get {
        table,
        column_ids,
        filters,
        index_rows: rows,
        ..
    } = &mut op.kind
        && !filters.is_empty()
    {
        *rows = index_rows(table, column_ids, filters, options).map(Arc::new);
    }
    if let K::Join {
        join_type: JoinType::Inner | JoinType::Semi,
        conditions,
        ..
    } = &op.kind
    {
        let conditions = conditions.clone();
        let (left, right) = op.children.split_at_mut(1);
        for condition in &conditions {
            scan_join_keys(&mut left[0], &right[0], &condition.left, &condition.right, options);
            scan_join_keys(&mut right[0], &left[0], &condition.right, &condition.left, options);
        }
    }
}

/// How much a plan can read and be run for its keys ahead of the statement.
const MOST_ROWS_READ_AHEAD: usize = 100_000;

/// Whether `op` gives the same rows when it runs again, and soon.
fn can_run_ahead(op: &LogicalOperator, rows: &mut usize) -> bool {
    match &op.kind {
        K::Get { table, .. } => *rows += table.row_count(),
        K::Sample { options } if options.seed.is_none() => return false,
        K::Sample { .. }
        | K::Range { .. }
        | K::DummyScan { .. }
        | K::ExpressionGet { .. }
        | K::Filter { .. }
        | K::Projection { .. }
        | K::ColumnSelect { .. }
        | K::Aggregate { .. }
        | K::Distinct { .. }
        | K::Limit { .. } => {}
        _ => return false,
    }
    *rows <= MOST_ROWS_READ_AHEAD && op.children.iter().all(|child| can_run_ahead(child, rows))
}

/// C++ turns the keys of a small build side into a filter on the scan of the probe side when the
/// join runs (`dynamic_or_filter_threshold`), which an ART can answer. Planning and running are one
/// step here, so the other side runs now if that costs little. The join still does the matching:
/// all this has to do is leave out rows that have no key on the other side.
fn scan_join_keys(
    scan: &mut LogicalOperator,
    other: &LogicalOperator,
    column: &BoundExpr,
    keys: &BoundExpr,
    options: &OptimizerOptions,
) {
    let mut scan = scan;
    while matches!(scan.kind, K::ColumnSelect { .. }) {
        scan = &mut scan.children[0];
    }
    let K::Get {
        table,
        table_index,
        column_ids,
        index_rows: rows,
        ..
    } = &mut scan.kind
    else {
        return;
    };
    let BoundExprKind::ColumnRef { binding, .. } = &column.kind else {
        return;
    };
    if rows.is_some() || binding.table_index != *table_index {
        return;
    }
    let Some(table_column) = column_ids.get(binding.column_index).copied() else {
        return;
    };
    let keyed = table.scan_keys().into_iter().find(|key| {
        let plain = key.expressions.as_ref().is_none_or(|expressions| {
            matches!(expressions.as_slice(), [expression] if matches!(expression.kind, BoundExprKind::Reference(_)))
        });
        plain && key.columns.as_slice() == [table_column]
    });
    let Some(key) = keyed else { return };
    if column.return_type != table.columns[table_column].ty
        || keys.return_type != column.return_type
        || !can_run_ahead(other, &mut 0)
        || other.is_volatile()
    {
        return;
    }
    let total = table.row_count();
    if total <= options.dynamic_or_filter_threshold {
        return;
    }
    let mut plan = LogicalOperator::new(
        K::Projection {
            table_index: usize::MAX,
            expressions: vec![keys.clone()],
        },
        vec![other.clone()],
    );
    let found = (|| -> Result<Option<Vec<usize>>> {
        resolve_column_bindings(&mut plan)?;
        let values = run_plan(plan)?;
        if values.len() > options.dynamic_or_filter_threshold {
            return Ok(None);
        }
        let probe = Probe {
            points: (0..values.len())
                .map(|row| values.get_value(0, row))
                .filter(|value| !value.is_null())
                .collect(),
            ..Probe::default()
        };
        let limit =
            (options.index_scan_max_count as usize).max((total as f64 * options.index_scan_percentage) as usize);
        let tree = index_tree(table, &key)?;
        let ids = table.row_ids(total);
        rows_of(&tree, &ids, &probe, &column.return_type, limit)
    })();
    // what doesn't work out is for the scan to find out
    if let Ok(Some(found)) = found {
        *rows = Some(Arc::new(found));
    }
}
