// Ported from duckdb/src/optimizer/pushdown/pushdown_get.cpp and the table filter part of
// duckdb/src/optimizer/filter_combiner.cpp @ rust-port-baseline
//
//! Moves filters on one column into the scan below them (`TableFilterSet`), where they run on each
//! chunk as it is read. In-memory scans have no zone maps yet, so all this gains is that no
//! operator is left to pass the rows on, and that the columns only they read stay in the scan. It
//! runs late, so that the passes before it see filters as operators, and only takes what can run
//! ahead of the filters that stay: nothing that can fail, unless it is the only filter.

use super::rewriter::{can_throw, contains_barrier, split_conjuncts};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};

/// The columns of the expression, in the query around the plan too, with `outer` for those.
fn columns_of(expr: &BoundExpr, columns: &mut Vec<ColumnBinding>, outer: &mut bool) {
    if let BoundExprKind::ColumnRef { binding, depth } = &expr.kind {
        *outer |= *depth != 0;
        columns.push(*binding);
    }
    for child in expr.children() {
        columns_of(child, columns, outer);
    }
}

/// `TryPushdownGenericExpression`, for a filter on the output of a scan.
fn can_move_into_scan(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile() && !can_throw(expr)
}

/// [`can_move_into_scan`], for a filter that may fail.
fn can_move_ignoring_errors(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile()
}

/// Moves the filters directly on scans into them.
pub fn push_into_scans(op: &mut LogicalOperator) {
    for child in &mut op.children {
        push_into_scans(child);
    }
    if let K::LateralJoin { right, .. } = &mut op.kind {
        push_into_scans(right);
    }
    let K::Filter { expressions } = &mut op.kind else {
        return;
    };
    if !matches!(op.children[0].kind, K::Get { .. } | K::ParquetScan { .. }) {
        return;
    }
    let scan = op.children[0].column_bindings();
    let mut conjuncts = Vec::new();
    for expr in std::mem::take(expressions) {
        split_conjuncts(expr, &mut conjuncts);
    }
    let filters = match &mut op.children[0].kind {
        K::Get { filters, .. } => filters,
        // `MultiFileFunction` has `filter_pushdown`; these run as the file is read
        K::ParquetScan { scan: data, .. } => {
            let data = std::sync::Arc::make_mut(data);
            prune_files(data, &scan, &mut conjuncts);
            &mut data.filters
        }
        _ => unreachable!("checked above"),
    };
    let (barred, conjuncts): (Vec<BoundExpr>, Vec<BoundExpr>) = conjuncts.into_iter().partition(contains_barrier);
    for conjunct in conjuncts {
        if can_move_into_scan(&conjunct, &scan) {
            filters.push(conjunct);
        } else {
            expressions.push(conjunct);
        }
    }
    // `PushdownGet`: a filter that can fail goes into the scan too when it is the only one left, as then there is
    // no other filter it would run ahead of
    if let [only] = expressions.as_slice()
        && barred.is_empty()
        && can_move_ignoring_errors(only, &scan)
    {
        filters.push(expressions.pop().expect("one filter"));
    }
    // `PushdownBarrierFilters`: into the scan, after the filters there, only if no other filter stays on top
    for conjunct in barred {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(&conjunct, &mut columns, &mut outer);
        let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]) && scan.contains(&columns[0]);
        if expressions.is_empty() && one_column && !outer && !conjunct.is_volatile() {
            filters.push(conjunct);
        } else {
            expressions.push(conjunct);
        }
    }
    // `TryCreateMultiColumnExpressionFilter`: a filter on several columns of a table stays, and a copy tells the
    // scan which row groups none of its rows pass in
    if let K::Get { zone_filters, .. } = &mut op.children[0].kind {
        for expr in expressions.iter() {
            let mut columns = Vec::new();
            let mut outer = false;
            columns_of(expr, &mut columns, &mut outer);
            let several = columns.iter().any(|c| *c != columns[0]);
            if several && !outer && columns.iter().all(|c| scan.contains(c)) && !expr.is_volatile() {
                zone_filters.push(expr.clone());
            }
        }
    }
    if expressions.is_empty() {
        *op = op.children.remove(0);
    }
}

/// `HivePartitioning::ApplyFiltersToFileList`: the filters that only read the hive partitions and the file name
/// are worked out for each file, those of the files they are false for are left out, and the filters that could
/// be worked out for every file are no longer needed.
fn prune_files(
    scan: &mut crate::execution::parquet::ParquetScanData,
    bindings: &[ColumnBinding],
    conjuncts: &mut Vec<BoundExpr>,
) {
    let known: Vec<usize> = (0..bindings.len()).filter(|&at| scan.is_known_per_file(at)).collect();
    if known.is_empty() || conjuncts.is_empty() {
        return;
    }
    let mut keep = vec![true; scan.files.len()];
    let mut file_filters = String::new();
    conjuncts.retain(|conjunct| {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(conjunct, &mut columns, &mut outer);
        let on_known = |binding: &ColumnBinding| {
            bindings
                .iter()
                .position(|b| b == binding)
                .is_some_and(|at| known.contains(&at))
        };
        // a subquery runs with the query (in a recursive CTE, once per round): not before it
        fn has_subquery(expr: &BoundExpr) -> bool {
            matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
        }
        if columns.is_empty() || outer || conjunct.is_volatile() || has_subquery(conjunct) || !columns.iter().all(on_known) {
            return true;
        }
        let mut preserved = false;
        let mut applied = false;
        for (file, keep) in scan.files.iter().zip(keep.iter_mut()) {
            let mut copy = conjunct.clone();
            let mut known_values = true;
            copy.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
                    let at = bindings
                        .iter()
                        .position(|b| b == binding)
                        .expect("a column of the scan");
                    match scan.known_value(at, file) {
                        Some(value) => *e = BoundExpr::constant(value),
                        None => known_values = false,
                    }
                }
            });
            let result = known_values
                .then(|| crate::execution::expression_executor::try_evaluate_scalar(&copy))
                .flatten();
            match result {
                None => preserved = true,
                Some(value) if value.is_null() || value.data() == &duckrs_common::value::ValueData::Bool(false) => {
                    *keep = false;
                    applied = true;
                }
                Some(_) => {}
            }
        }
        if applied {
            file_filters.push_str(&conjunct.to_string());
        }
        preserved
    });
    let total = scan.files.len();
    let mut keep = keep.into_iter();
    scan.files.retain(|_| keep.next().unwrap_or(true));
    scan.file_filters.push_str(&file_filters);
    scan.total_files = Some(total);
}
