// Ported from duckdb/src/optimizer/row_number_rewriter.cpp @ rust-port-baseline
//
//! `RowNumberRewriter`: `row_number() OVER ()` straight over a scan of a table is the scan's
//! `row_number` column, the place of each row among the rows of the table. The window goes, and
//! what read its result reads that column. A scan with filters numbers its rows differently, so
//! it keeps the window.

use super::common_subplan::visit_bindings;
use crate::planner::expression::{ColumnBinding, WindowFunction};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_NUMBER, LogicalOperator, LogicalOperatorKind as K};
use std::collections::HashMap;

pub fn rewrite(mut plan: LogicalOperator) -> LogicalOperator {
    let mut replacements = HashMap::new();
    visit(&mut plan, &mut replacements);
    plan
}

/// `CanOptimize`.
fn can_optimize(op: &LogicalOperator) -> bool {
    let K::Window { windows, .. } = &op.kind else {
        return false;
    };
    let [window] = windows.as_slice() else {
        return false;
    };
    if window.function != WindowFunction::RowNumber
        || !window.partitions.is_empty()
        || !window.orders.is_empty()
        || !window.arg_orders.is_empty()
    {
        return false;
    }
    // only a scan of a table numbers the rows the window would, and only without filters
    matches!(
        &op.children[0].kind,
        K::Get {
            filters,
            zone_filters,
            index_rows: None,
            sample: None,
            ..
        } if filters.is_empty() && zone_filters.is_empty()
    )
}

/// `RewritePlan`, children first, with the operators above a rewritten window reading the
/// column of the scan instead (`ColumnBindingReplacer`).
fn visit(op: &mut LogicalOperator, replacements: &mut HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        visit(child, replacements);
    }
    if !replacements.is_empty() {
        visit_bindings(&mut op.kind, &mut |binding, depth| {
            if depth == 0
                && let Some(new) = replacements.get(binding)
            {
                *binding = *new;
            }
        });
    }
    if !can_optimize(op) {
        return;
    }
    let K::Window { table_index: window_index, .. } = op.kind else {
        unreachable!("checked");
    };
    let mut get = op.children.pop().expect("a window has a child");
    let K::Get {
        table_index,
        column_ids,
        projection,
        ..
    } = &mut get.kind
    else {
        unreachable!("checked");
    };
    // `RewriteGet`: the column goes at the end of the scan's columns
    let position = column_ids.len();
    column_ids.push(COLUMN_IDENTIFIER_ROW_NUMBER);
    if let Some(positions) = projection {
        positions.push(position);
    }
    replacements.insert(ColumnBinding::new(window_index, 0), ColumnBinding::new(*table_index, position));
    *op = get;
}
