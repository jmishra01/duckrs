// Ported from duckdb/src/execution/column_binding_resolver.cpp @ rust-port-baseline
//
//! Turns the column references of a plan (table index, column index) into positions in each
//! operator's input chunk (`BoundReferenceExpression`), which is what execution reads.

use super::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use super::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::{Error, ErrorKind, Result};

/// `ColumnBindingResolver::VisitOperator`: resolves the children first, then this operator's
/// expressions against the bindings its children produce.
pub fn resolve_column_bindings(op: &mut LogicalOperator) -> Result<()> {
    for child in &mut op.children {
        resolve_column_bindings(child)?;
    }
    if let LogicalOperatorKind::Join { conditions, .. }
    | LogicalOperatorKind::DelimJoin { conditions, .. }
    | LogicalOperatorKind::MarkJoin { conditions, .. } = &mut op.kind
    {
        let left = op.children[0].column_bindings();
        let right = op.children[1].column_bindings();
        for condition in conditions {
            resolve_expression(&mut condition.left, &left)?;
            resolve_expression(&mut condition.right, &right)?;
        }
    }
    if let LogicalOperatorKind::AsOfJoin {
        conditions,
        left,
        right,
        predicate,
        ..
    } = &mut op.kind
    {
        let left_bindings = op.children[0].column_bindings();
        let right_bindings = op.children[1].column_bindings();
        for condition in conditions {
            resolve_expression(&mut condition.left, &left_bindings)?;
            resolve_expression(&mut condition.right, &right_bindings)?;
        }
        resolve_expression(left, &left_bindings)?;
        resolve_expression(right, &right_bindings)?;
        if let Some(predicate) = predicate {
            let mut bindings = left_bindings;
            bindings.extend(right_bindings);
            resolve_expression(predicate, &bindings)?;
        }
        return Ok(());
    }
    if let LogicalOperatorKind::LateralJoin {
        right,
        correlated,
        predicate,
        ..
    } = &mut op.kind
    {
        resolve_column_bindings(right)?;
        let mut bindings = op.children[0].column_bindings();
        for expr in correlated {
            resolve_expression(expr, &bindings)?;
        }
        bindings.extend(right.column_bindings());
        if let Some(expr) = predicate {
            resolve_expression(expr, &bindings)?;
        }
        return Ok(());
    }
    // a scan's filters read what it scans; everything else reads its children
    let bindings: Vec<ColumnBinding> = if let LogicalOperatorKind::Get {
        table_index,
        column_ids,
        ..
    } = &op.kind
    {
        (0..column_ids.len())
            .map(|column| ColumnBinding::new(*table_index, column))
            .collect()
    } else if let LogicalOperatorKind::ParquetScan { .. } = &op.kind {
        op.column_bindings()
    } else {
        op.children.iter().flat_map(|c| c.column_bindings()).collect()
    };
    for expr in op.expressions_mut() {
        resolve_expression(expr, &bindings)?;
    }
    Ok(())
}

pub(crate) fn resolve_expression(expr: &mut BoundExpr, bindings: &[ColumnBinding]) -> Result<()> {
    let mut result = Ok(());
    expr.walk_mut(&mut |e| {
        // a correlated column gets its value when the subquery runs
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            match bindings.iter().position(|b| b == binding) {
                Some(index) => e.kind = BoundExprKind::Reference(index),
                None if result.is_ok() => {
                    result = Err(Error::new(
                        ErrorKind::Internal,
                        format!("Failed to bind column reference \"{}\" {binding}", e.alias),
                    ));
                }
                None => {}
            }
        }
    });
    result
}
