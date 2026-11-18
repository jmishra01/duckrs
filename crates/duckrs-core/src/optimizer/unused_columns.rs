// Ported from duckdb/src/optimizer/remove_unused_columns.cpp @ rust-port-baseline
//
//! `RemoveUnusedColumns`: what nothing reads is not computed, also not when computing it would
//! fail, and a scan does not output what nothing above it reads, so that joins do not copy it. C++
//! removes the columns and renumbers the column bindings. Here a projection keeps its columns and
//! an unused one becomes a NULL, and a scan keeps its `column_ids` and says which it outputs, since
//! subquery plans refer to column bindings of the plans around them. A column that nothing above
//! reads goes before the next operator that copies rows, through `ColumnSelect`.

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
    // a scan of a CTE read by position has all of its columns read
    if let K::CteRef { .. } = &op.kind
        && by_position
    {
        used.extend(op.column_bindings());
    }
    if let K::MaterializedCte { table_index, .. } = &op.kind {
        let cte_index = *table_index;
        // the output is what the query after the CTE gives
        visit(&mut op.children[1], true, used);
        // `CTERefPruner`: the columns of the definition that a scan of it reads
        let mut read = HashSet::new();
        let mut seen = true;
        collect_cte_reads(&mut op.children[1], cte_index, used, &mut read, &mut seen);
        let bindings = op.children[0].column_bindings();
        let narrow = seen && read.len() < bindings.len() && matches!(op.children[0].kind, K::Projection { .. });
        if narrow {
            used.extend(read.iter().map(|&column| bindings[column]));
        }
        visit(&mut op.children[0], !narrow, used);
        return;
    }
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
    if let K::ParquetScan { table_index, scan } = &mut op.kind
        && !by_position
    {
        let mut filtered = HashSet::new();
        for filter in &scan.filters {
            collect_columns(filter, &mut filtered);
        }
        let read: Vec<bool> = (0..scan.names.len())
            .map(|column| ColumnBinding::new(*table_index, column))
            .map(|column| used.contains(&column) || filtered.contains(&column))
            .collect();
        let projected = (0..scan.output_types().len())
            .map(|column| used.contains(&ColumnBinding::new(*table_index, column)))
            .collect();
        let scan = std::sync::Arc::make_mut(scan);
        scan.read = Some(read);
        scan.projected = Some(projected);
    }
    // `RemoveUnusedColumns` for a table function with projection pushdown: the CSV scan outputs what is
    // read of it
    if let K::CsvScan { table_index, scan } = &mut op.kind
        && !by_position
        && let Some(read) = pushed_projection(*table_index, &scan.projected_columns(), &scan.column_order, used)
    {
        let mut narrowed = super::type_pushdown::clone_scan(scan);
        narrowed.projection = Some(read);
        *scan = std::sync::Arc::new(narrowed);
    }
    if let K::JsonScan { table_index, scan } = &mut op.kind
        && !by_position
        && let Some(read) = pushed_projection(*table_index, &scan.projected_columns(), &scan.column_order, used)
    {
        std::sync::Arc::make_mut(scan).projection = Some(read);
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
                // the column is gone, as C++ removes it: so is its name
                expr.alias.clear();
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
        | K::TopN { .. }
        | K::Limit { .. }
        | K::Distinct { .. }
        | K::CrossProduct
        | K::Join { .. }
        | K::DelimJoin { .. }
        | K::MarkJoin { .. }
        | K::Window { .. }
        | K::Unnest { .. }
        | K::SecureView { .. } => by_position,
        _ => true,
    };
    // `ColumnLifetimeAnalyzer`: the operators with projection maps, which copy the rows of their children:
    // joins with an equality (hash joins) and ORDER BY
    let copies = match &op.kind {
        K::Join { conditions, .. } | K::MarkJoin { conditions, .. } => !conditions.is_empty(),
        K::DelimJoin { .. } | K::Order { .. } => true,
        _ => false,
    };
    for child in &mut op.children {
        let read: Vec<ColumnBinding> = child
            .column_bindings()
            .into_iter()
            .filter(|binding| used.contains(binding))
            .collect();
        visit(child, by_position, used);
        if !by_position && (copies || matches!(child.kind, K::Projection { .. })) {
            drop_unread_columns(child, read);
        }
    }
}

/// The columns of the CTE `cte_index` that its scans below `op` read. `seen` turns false where a
/// subquery could read it without the scan's columns being among `used`.
fn collect_cte_reads(
    op: &mut LogicalOperator,
    cte_index: usize,
    used: &HashSet<ColumnBinding>,
    read: &mut HashSet<usize>,
    seen: &mut bool,
) {
    if let K::CteRef {
        table_index,
        cte_index: index,
        types,
        ..
    } = &op.kind
        && *index == cte_index
    {
        read.extend((0..types.len()).filter(|&column| used.contains(&ColumnBinding::new(*table_index, column))));
    }
    if matches!(op.kind, K::RecursiveCte { .. } | K::LateralJoin { .. }) {
        *seen = false;
    }
    for expr in op.expressions_mut() {
        expr.walk_mut(&mut |e| {
            if matches!(e.kind, BoundExprKind::Subquery { .. }) {
                *seen = false;
            }
        });
    }
    for child in &mut op.children {
        collect_cte_reads(child, cte_index, used, read, seen);
    }
}

fn drop_unread_columns(op: &mut LogicalOperator, keep: Vec<ColumnBinding>) {
    let columns = op.column_bindings();
    if keep.len() == columns.len() || columns.is_empty() {
        return;
    }
    // `GenerateProjectionMap`: with none of them read, the map is empty, which keeps them all
    if keep.is_empty() {
        return;
    }
    let child = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
    *op = LogicalOperator::new(K::ColumnSelect { keep }, vec![child]);
}

/// The columns a scan with projection pushdown outputs once those nothing reads are gone, in the
/// order the binder read them (`column_ids`); `None` when that is what it outputs already.
fn pushed_projection(
    table_index: usize,
    columns: &[usize],
    order: &crate::execution::multi_file::ColumnOrder,
    used: &HashSet<ColumnBinding>,
) -> Option<Vec<usize>> {
    let read: Vec<usize> = columns
        .iter()
        .copied()
        .filter(|&column| used.contains(&ColumnBinding::new(table_index, column)))
        .collect();
    let mut read = order.arrange(&read);
    // a chunk without columns would have no rows to count
    if read.is_empty() {
        read.push(columns[0]);
    }
    (read != columns).then_some(read)
}
