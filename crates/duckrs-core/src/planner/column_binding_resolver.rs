// Ported from duckdb/src/execution/column_binding_resolver.cpp @ rust-port-baseline
//
//! Turns the column references of a plan (table index, column index) into positions in each
//! operator's input chunk (`BoundReferenceExpression`), which is what execution reads.

use super::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use super::logical::LogicalOperator;
use duckrs_common::{Error, ErrorKind, Result};

/// `ColumnBindingResolver::VisitOperator`: resolves the children first, then this operator's
/// expressions against the bindings its children produce.
pub fn resolve_column_bindings(op: &mut LogicalOperator) -> Result<()> {
    for child in &mut op.children {
        resolve_column_bindings(child)?;
    }
    let bindings: Vec<ColumnBinding> = op.children.iter().flat_map(|c| c.column_bindings()).collect();
    for expr in op.expressions_mut() {
        resolve_expression(expr, &bindings)?;
    }
    Ok(())
}

fn resolve_expression(expr: &mut BoundExpr, bindings: &[ColumnBinding]) -> Result<()> {
    let mut result = Ok(());
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = &e.kind {
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
