// Ported from duckdb/src/optimizer/remove_unused_columns.cpp @ rust-port-baseline
//
//! `RemoveUnusedColumns`: what nothing reads is not computed, also not when computing it would
//! fail, and a scan does not output what nothing above it reads, so that joins do not copy it. C++
//! removes the columns and renumbers the column bindings. Here a projection keeps its columns and
//! an unused one becomes a NULL, and a scan keeps its `column_ids` and says which it outputs, since
//! subquery plans refer to column bindings of the plans around them. What a join needed for itself
//! still goes through it, which C++ removes with projection maps.

use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;
use std::collections::HashSet;

pub fn remove_unused_columns(plan: &mut LogicalOperator) {
    // the columns of the result are all read, by position
    visit(plan, true, &mut HashSet::new());
}

fn collect_columns(expr: &BoundExpr, used: &mut HashSet<ColumnBinding>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        used.insert(*binding);
    }
    for child in expr.children() {
        collect_columns(child, used);
    }
}

/// `used` holds the columns that the operators above read. `by_position` says something above reads
/// this operator's output by position and not through bindings.
fn visit(op: &mut LogicalOperator, by_position: bool, used: &mut HashSet<ColumnBinding>) {
    if let K::Get {
        table_index,
        column_ids,
        projection,
        ..
    } = &mut op.kind
        && !by_position
        && !column_ids.is_empty()
    {
        let read = (0..column_ids.len()).filter(|&column| used.contains(&ColumnBinding::new(*table_index, column)));
        let mut read: Vec<usize> = read.collect();
        // a chunk without columns would have no rows to count
        if read.is_empty() {
            read.push(0);
        }
        *projection = Some(read);
    }
    if let K::Projection {
        table_index,
        expressions,
    } = &mut op.kind
        && !by_position
    {
        for (index, expr) in expressions.iter_mut().enumerate() {
            if !used.contains(&ColumnBinding::new(*table_index, index)) && !expr.is_volatile() {
                let ty = expr.return_type.clone();
                expr.kind = BoundExprKind::Constant(Value::null(ty));
            }
        }
    }
    for expr in op.expressions_mut() {
        collect_columns(expr, used);
    }
    if let K::Join { conditions, .. } | K::DelimJoin { conditions, .. } | K::MarkJoin { conditions, .. } = &op.kind {
        for condition in conditions {
            collect_columns(&condition.left, used);
            collect_columns(&condition.right, used);
        }
    }
    let by_position = match op.kind {
        K::Projection { .. } | K::Aggregate { .. } => false,
        K::Filter { .. }
        | K::Order { .. }
        | K::Limit { .. }
        | K::Distinct { .. }
        | K::CrossProduct
        | K::Join { .. }
        | K::DelimJoin { .. }
        | K::MarkJoin { .. }
        | K::Window { .. }
        | K::Unnest { .. } => by_position,
        _ => true,
    };
    for child in &mut op.children {
        visit(child, by_position, used);
    }
}
