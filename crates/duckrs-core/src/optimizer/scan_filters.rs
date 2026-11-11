// Ported from duckdb/src/optimizer/pushdown/pushdown_get.cpp and the table filter part of
// duckdb/src/optimizer/filter_combiner.cpp @ rust-port-baseline
//
//! Moves filters on one column into the scan below them (`TableFilterSet`), where they run on each
//! chunk as it is read. In-memory scans have no zone maps yet, so all this gains is that no
//! operator is left to pass the rows on, and that the columns only they read stay in the scan. It
//! runs late, so that the passes before it see filters as operators, and only takes what can run
//! ahead of the filters that stay: nothing that can fail.

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
    let scan = op.children[0].column_bindings();
    let K::Get { filters, .. } = &mut op.children[0].kind else {
        return;
    };
    let mut conjuncts = Vec::new();
    for expr in std::mem::take(expressions) {
        split_conjuncts(expr, &mut conjuncts);
    }
    let (barred, conjuncts): (Vec<BoundExpr>, Vec<BoundExpr>) = conjuncts.into_iter().partition(contains_barrier);
    for conjunct in conjuncts {
        if can_move_into_scan(&conjunct, &scan) {
            filters.push(conjunct);
        } else {
            expressions.push(conjunct);
        }
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
    if expressions.is_empty() {
        *op = op.children.remove(0);
    }
}
