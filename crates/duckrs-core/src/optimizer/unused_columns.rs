// Ported from duckdb/src/optimizer/remove_unused_columns.cpp @ rust-port-baseline
//
//! `RemoveUnusedColumns`: what nothing reads is not computed, also not when computing it would
//! fail, and a scan does not output what nothing above it reads, so that joins do not copy it. C++
//! removes the columns and renumbers the column bindings. Here a projection keeps its columns and
//! an unused one becomes a NULL, and a scan keeps its `column_ids` and says which it outputs, since
//! subquery plans refer to column bindings of the plans around them. A column that nothing above
//! reads goes before the next operator that copies rows, through `ColumnSelect`.

use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundWindow, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

thread_local! {
    /// The new places of the outputs of windows that lost some (`ClearUnusedExpressions`), when the
    /// plan has no subquery plan that could refer to them without this pass seeing it.
    static WINDOW_RENUMBERING: RefCell<Option<HashMap<ColumnBinding, ColumnBinding>>> = const { RefCell::new(None) };
}

pub fn remove_unused_columns(plan: &mut LogicalOperator) {
    let nested = has_nested_plan(plan);
    WINDOW_RENUMBERING.with(|r| *r.borrow_mut() = (!nested).then(HashMap::new));
    // the columns of the result are all read, by position
    visit(plan, true, &mut HashSet::new());
    let renumbering = WINDOW_RENUMBERING.with(|r| r.borrow_mut().take()).unwrap_or_default();
    if !renumbering.is_empty() {
        renumber(plan, &renumbering);
    }
}

/// Whether a subquery or lateral join below `op` has a plan of its own.
fn has_nested_plan(op: &mut LogicalOperator) -> bool {
    if matches!(op.kind, K::LateralJoin { .. }) {
        return true;
    }
    let mut nested = false;
    for expr in op.expressions_mut() {
        expr.walk_mut(&mut |e| nested |= matches!(e.kind, BoundExprKind::Subquery { .. }));
    }
    nested || op.children.iter_mut().any(has_nested_plan)
}

fn renumber(op: &mut LogicalOperator, renumbering: &HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        renumber(child, renumbering);
    }
    super::common_subplan::visit_bindings(&mut op.kind, &mut |binding: &mut ColumnBinding, depth| {
        if depth == 0
            && let Some(new) = renumbering.get(binding)
        {
            *binding = *new;
        }
    });
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
        // `GetAnyColumn`: a chunk without columns would have no rows to count, and the row id costs nothing to read
        if read.is_empty() {
            let row_id = crate::planner::logical::COLUMN_IDENTIFIER_ROW_ID;
            let position = match column_ids.iter().position(|&c| c == row_id) {
                Some(position) => position,
                None => {
                    column_ids.push(row_id);
                    column_ids.len() - 1
                }
            };
            read.push(position);
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
            // `ClearUnusedExpressions`: volatile or not, what nothing reads is not computed
            if !used.contains(&ColumnBinding::new(*table_index, index)) {
                let ty = expr.return_type.clone();
                expr.kind = BoundExprKind::Constant(Value::null(ty));
                // the column is gone, as C++ removes it: so is its name
                expr.alias.clear();
            }
        }
    }
    // `RemoveUnusedColumns` of a window none of whose outputs are read: it passes its child's rows and
    // columns through, so it is its child (volatile windows run all the same)
    if let K::Window { table_index, windows } = &op.kind
        && !by_position
        && (0..windows.len()).all(|index| !used.contains(&ColumnBinding::new(*table_index, index)))
        && !op.expressions_mut().into_iter().any(|e| e.is_volatile())
    {
        *op = op.children.remove(0);
        visit(op, by_position, used);
        return;
    }
    // the windows whose outputs nothing reads go, and the others move up
    if let K::Window { table_index, windows } = &mut op.kind
        && !by_position
        && windows.len() > 1
    {
        let table_index = *table_index;
        let unused = |index: usize, window: &BoundWindow| {
            !used.contains(&ColumnBinding::new(table_index, index)) && !window_is_volatile(window)
        };
        let removed = windows.iter().enumerate().filter(|(i, w)| unused(*i, w)).count();
        if removed > 0 && removed < windows.len() {
            WINDOW_RENUMBERING.with(|r| {
                let mut r = r.borrow_mut();
                let Some(renumbering) = r.as_mut() else {
                    return;
                };
                let mut kept = Vec::new();
                for (index, window) in std::mem::take(windows).into_iter().enumerate() {
                    if unused(index, &window) {
                        continue;
                    }
                    if kept.len() != index {
                        renumbering.insert(ColumnBinding::new(table_index, index), ColumnBinding::new(table_index, kept.len()));
                    }
                    kept.push(window);
                }
                *windows = kept;
            });
        }
    }
    // `ClearUnusedExpressions` of an aggregate: what nothing reads is not computed; an ungrouped
    // aggregate none of whose aggregates are read computes a COUNT(*) of its own, for its one row
    if let K::Aggregate {
        aggregate_index,
        aggregates,
        groups,
        ..
    } = &mut op.kind
        && !by_position
    {
        let aggregate_index = *aggregate_index;
        let unused = |index: usize, aggregate: &BoundAggregate| {
            !used.contains(&ColumnBinding::new(aggregate_index, index)) && !aggregate_is_volatile(aggregate)
        };
        let removable = aggregates.iter().enumerate().filter(|(i, a)| unused(*i, a)).count();
        let ungrouped = groups.is_empty();
        // a lone unread COUNT(*) is the one the ungrouped aggregate would compute in its place
        let lone_count_star = ungrouped
            && aggregates.len() == 1
            && aggregates[0].function.name == "count_star"
            && aggregates[0].filter.is_none();
        if removable > 0 && !lone_count_star {
            WINDOW_RENUMBERING.with(|r| {
                let mut r = r.borrow_mut();
                let Some(renumbering) = r.as_mut() else {
                    return;
                };
                let mut kept = Vec::new();
                for (index, aggregate) in std::mem::take(aggregates).into_iter().enumerate() {
                    if unused(index, &aggregate) {
                        continue;
                    }
                    if kept.len() != index {
                        renumbering.insert(
                            ColumnBinding::new(aggregate_index, index),
                            ColumnBinding::new(aggregate_index, kept.len()),
                        );
                    }
                    kept.push(aggregate);
                }
                if kept.is_empty() && ungrouped {
                    kept.push(count_star());
                }
                *aggregates = kept;
            });
        }
    }
    // a column select hides columns of its child (the markers of the IN clause rewriter): it keeps no more than is
    // read of it, and its child gives what it keeps
    if let K::ColumnSelect { keep } = &mut op.kind {
        if !by_position {
            let read: Vec<ColumnBinding> = keep.iter().copied().filter(|binding| used.contains(binding)).collect();
            // a chunk without columns would have no rows to count
            if !read.is_empty() {
                *keep = read;
            } else {
                keep.truncate(1);
            }
        }
        used.extend(keep.iter().copied());
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
        | K::SecureView { .. }
        | K::ColumnSelect { .. } => by_position,
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

/// A window that changes state as it runs, which runs whether or not its output is read.
fn window_is_volatile(window: &BoundWindow) -> bool {
    let mut op = LogicalOperator::new(
        K::Window {
            table_index: 0,
            windows: vec![window.clone()],
        },
        Vec::new(),
    );
    op.expressions_mut().into_iter().any(|e| e.is_volatile())
}

/// An aggregate that changes state as it runs.
fn aggregate_is_volatile(aggregate: &BoundAggregate) -> bool {
    aggregate.children.iter().chain(aggregate.filter.iter()).any(BoundExpr::is_volatile)
        || aggregate.order_by.iter().any(|(e, _)| e.is_volatile())
}

/// The `count_star()` an ungrouped aggregate computes when none of its own aggregates are read.
fn count_star() -> BoundAggregate {
    let function = crate::function::BuiltinFunctions::get()
        .aggregate("count_star")
        .expect("count_star is a builtin aggregate");
    crate::function::binder::bind_aggregate_function(function, Vec::new(), None, false).expect("count_star() binds")
}
