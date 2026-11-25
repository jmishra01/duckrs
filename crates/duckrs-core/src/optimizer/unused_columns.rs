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
    /// The new places of the outputs of windows that lost some (`ClearUnusedExpressions`), and of recursive CTEs
    /// that did, when the plan has no subquery plan that could refer to them without this pass seeing it.
    static WINDOW_RENUMBERING: RefCell<Option<HashMap<ColumnBinding, ColumnBinding>>> = const { RefCell::new(None) };
    /// The projections whose columns are read by position, by the scans of the CTE they define: an unread column
    /// becomes a NULL and keeps its place.
    static POSITIONAL_PROJECTIONS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    /// Whether the projections of the plan keep their columns in place: with a recursive CTE, whose recursive
    /// part is resolved to positions already, or with projections of the same table index in two places (late
    /// materialization's copies), which a renumbering of the bindings of one would move in the other.
    static KEEP_PROJECTION_COLUMNS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The next table index of the plan, for the projections a pruned recursive CTE gets.
    static NEXT_TABLE_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Returns where the columns that moved went.
pub fn remove_unused_columns(plan: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
    remove_unused_columns_with(plan, true)
}

/// [`remove_unused_columns`]; without `trim_projections`, the unread columns of projections become NULLs in place,
/// for the runs after the rewrites (late materialization, TOP_N window elimination) that copy parts of the plan.
pub fn remove_unused_columns_with(
    plan: &mut LogicalOperator,
    trim_projections: bool,
) -> HashMap<ColumnBinding, ColumnBinding> {
    let nested = has_nested_plan(plan);
    WINDOW_RENUMBERING.with(|r| *r.borrow_mut() = (!nested).then(HashMap::new));
    POSITIONAL_PROJECTIONS.with(|p| p.borrow_mut().clear());
    KEEP_PROJECTION_COLUMNS
        .with(|k| k.set(!trim_projections || has_recursive_cte(plan) || has_repeated_projection(plan)));
    NEXT_TABLE_INDEX.with(|next| next.set(super::common_subplan::max_table_index(plan) + 1));
    // the columns of the result are all read, by position
    visit(plan, true, &mut HashSet::new());
    let renumbering = WINDOW_RENUMBERING.with(|r| r.borrow_mut().take()).unwrap_or_default();
    if !renumbering.is_empty() {
        renumber(plan, &renumbering);
    }
    renumbering
}

fn has_recursive_cte(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::RecursiveCte { .. }) || op.children.iter().any(has_recursive_cte)
}

fn has_repeated_projection(plan: &LogicalOperator) -> bool {
    fn visit(op: &LogicalOperator, seen: &mut HashSet<usize>) -> bool {
        if let K::Projection { table_index, .. } = &op.kind
            && !seen.insert(*table_index)
        {
            return true;
        }
        op.children.iter().any(|child| visit(child, seen))
    }
    visit(plan, &mut HashSet::new())
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
    if !by_position && matches!(op.kind, K::RecursiveCte { .. }) {
        prune_recursive_cte(op, used);
    }
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
            if let K::Projection { table_index, .. } = &op.children[0].kind {
                POSITIONAL_PROJECTIONS.with(|p| p.borrow_mut().insert(*table_index));
            }
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
        let table_index = *table_index;
        // `ClearUnusedExpressions`: with no subquery plan to refer to them, the unread columns go and the others
        // move up; a projection none of whose columns are read projects a single constant
        let positional = POSITIONAL_PROJECTIONS.with(|p| p.borrow().contains(&table_index))
            || KEEP_PROJECTION_COLUMNS.with(std::cell::Cell::get);
        let removed = WINDOW_RENUMBERING.with(|r| {
            if positional {
                return None;
            }
            let mut r = r.borrow_mut();
            let renumbering = r.as_mut()?;
            if (0..expressions.len()).all(|index| used.contains(&ColumnBinding::new(table_index, index))) {
                return Some(());
            }
            let mut kept = Vec::new();
            for (index, expr) in std::mem::take(expressions).into_iter().enumerate() {
                if !used.contains(&ColumnBinding::new(table_index, index)) {
                    continue;
                }
                if kept.len() != index {
                    renumbering.insert(
                        ColumnBinding::new(table_index, index),
                        ColumnBinding::new(table_index, kept.len()),
                    );
                }
                kept.push(expr);
            }
            if kept.is_empty() {
                kept.push(BoundExpr::constant(Value::integer(42)));
            }
            *expressions = kept;
            Some(())
        });
        for (index, expr) in expressions.iter_mut().enumerate() {
            if removed.is_some() {
                break;
            }
            // volatile or not, what nothing reads is not computed
            if !used.contains(&ColumnBinding::new(table_index, index)) {
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
                        renumbering.insert(
                            ColumnBinding::new(table_index, index),
                            ColumnBinding::new(table_index, kept.len()),
                        );
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
    aggregate
        .children
        .iter()
        .chain(aggregate.filter.iter())
        .any(BoundExpr::is_volatile)
        || aggregate.order_by.iter().any(|(e, _)| e.is_volatile())
}

/// The `count_star()` an ungrouped aggregate computes when none of its own aggregates are read.
fn count_star() -> BoundAggregate {
    let function = crate::function::BuiltinFunctions::get()
        .aggregate("count_star")
        .expect("count_star is a builtin aggregate");
    crate::function::binder::bind_aggregate_function(function, Vec::new(), None, false).expect("count_star() binds")
}

fn next_table_index() -> usize {
    NEXT_TABLE_INDEX.with(|next| {
        let index = next.get();
        next.set(index + 1);
        index
    })
}

/// Whether anything in the plan is volatile, or has a plan of its own (`LogicalSubtreeIsRepeatable`, and no
/// subquery that could read the working table without this pass seeing it).
fn volatile_or_nested(op: &LogicalOperator) -> bool {
    let mut copy = op.kind.clone();
    let mut found = matches!(op.kind, K::LateralJoin { .. } | K::RecursiveCte { .. });
    let mut probe = LogicalOperator::new(std::mem::replace(&mut copy, K::CrossProduct), Vec::new());
    for expr in probe.expressions_mut() {
        expr.walk_mut(&mut |e| found |= e.is_volatile() || matches!(e.kind, BoundExprKind::Subquery { .. }));
    }
    found || op.children.iter().any(volatile_or_nested)
}

/// The table indexes of the reads of the working (or recurring) table `tables` below `op`.
fn working_reads(op: &LogicalOperator, tables: &[usize], found: &mut Vec<usize>) {
    if let K::WorkingTable {
        table_index, working, ..
    } = &op.kind
        && tables.contains(&(std::sync::Arc::as_ptr(working) as *const () as usize))
    {
        found.push(*table_index);
    }
    for child in &op.children {
        working_reads(child, tables, found);
    }
}

/// `TryPruneRecursiveCTE`: of a UNION ALL recursive CTE, the columns nothing above reads, and that the recursive part
/// needs only to compute themselves, go: from the anchor, the recursive part and the working table. The columns that
/// stay are the fixed point of those read above and those the recursive part reads to compute them
/// (`ComputeRecursiveRequiredColumns`).
fn prune_recursive_cte(op: &mut LogicalOperator, used: &mut HashSet<ColumnBinding>) {
    // the readers above are renumbered at the end, which a subquery plan could escape
    if WINDOW_RENUMBERING.with(|r| r.borrow().is_none()) {
        return;
    }
    let K::RecursiveCte {
        table_index,
        types,
        union_all,
        working,
        recurring,
        key,
        unresolved,
        ..
    } = &op.kind
    else {
        return;
    };
    if !*union_all || !key.is_empty() || volatile_or_nested(&op.children[0]) || volatile_or_nested(unresolved) {
        return;
    }
    let (cte_index, width) = (*table_index, types.len());
    let tables = [
        std::sync::Arc::as_ptr(working) as *const () as usize,
        std::sync::Arc::as_ptr(recurring) as *const () as usize,
    ];
    let mut reads = Vec::new();
    working_reads(unresolved, &tables, &mut reads);
    let outputs = unresolved.column_bindings();
    if outputs.len() != width {
        return;
    }
    let mut required: std::collections::BTreeSet<usize> = (0..width)
        .filter(|&i| used.contains(&ColumnBinding::new(cte_index, i)))
        .collect();
    if required.is_empty() {
        required.insert(0);
    }
    // the fixed point: what the recursive part reads of the working table to compute the columns kept
    loop {
        let mut live: HashSet<ColumnBinding> = required.iter().map(|&i| outputs[i]).collect();
        let mut copy = (**unresolved).clone();
        visit(&mut copy, false, &mut live);
        let before = required.len();
        for binding in &live {
            if reads.contains(&binding.table_index) && binding.column_index < width {
                required.insert(binding.column_index);
            }
        }
        if required.len() >= width {
            return;
        }
        if required.len() == before {
            break;
        }
    }
    let kept: Vec<usize> = required.into_iter().collect();
    let position: HashMap<usize, usize> = kept.iter().enumerate().map(|(new, &old)| (old, new)).collect();
    let K::RecursiveCte {
        types,
        working,
        recurring,
        recursive,
        unresolved,
        ..
    } = &mut op.kind
    else {
        unreachable!()
    };
    let new_types: Vec<_> = kept.iter().map(|&i| types[i].clone()).collect();
    let fresh = |old: &std::sync::Arc<parking_lot::Mutex<duckrs_common::column_data::ColumnDataCollection>>| {
        let _ = old;
        std::sync::Arc::new(parking_lot::Mutex::new(
            duckrs_common::column_data::ColumnDataCollection::new(new_types.clone()),
        ))
    };
    let (new_working, new_recurring) = (fresh(working), fresh(recurring));
    // `ApplyRecursiveProjections`: each side gives the columns kept, in order
    let narrow = |child: LogicalOperator| {
        let bindings = child.column_bindings();
        let child_types = child.types();
        let expressions = kept
            .iter()
            .map(|&i| BoundExpr::column_ref(bindings[i], child_types[i].clone(), String::new()))
            .collect();
        LogicalOperator::new(
            K::Projection {
                table_index: next_table_index(),
                expressions,
            },
            vec![child],
        )
    };
    // `CTERefPruner`: the reads of the working table have the columns kept, at their new places
    let mut plan = narrow((**unresolved).clone());
    fn rewrite_reads(
        op: &mut LogicalOperator,
        tables: &[usize],
        reads: &[usize],
        position: &HashMap<usize, usize>,
        new_types: &[duckrs_common::types::LogicalType],
        new_working: &std::sync::Arc<parking_lot::Mutex<duckrs_common::column_data::ColumnDataCollection>>,
        new_recurring: &std::sync::Arc<parking_lot::Mutex<duckrs_common::column_data::ColumnDataCollection>>,
    ) {
        for child in &mut op.children {
            rewrite_reads(child, tables, reads, position, new_types, new_working, new_recurring);
        }
        if let K::WorkingTable { types, working, .. } = &mut op.kind {
            let address = std::sync::Arc::as_ptr(working) as *const () as usize;
            if address == tables[0] {
                *working = new_working.clone();
                *types = new_types.to_vec();
            } else if address == tables[1] {
                *working = new_recurring.clone();
                *types = new_types.to_vec();
            }
        }
        super::common_subplan::visit_bindings(&mut op.kind, &mut |binding: &mut ColumnBinding, depth| {
            if depth == 0
                && reads.contains(&binding.table_index)
                && let Some(&new) = position.get(&binding.column_index)
            {
                binding.column_index = new;
            }
        });
    }
    rewrite_reads(
        &mut plan,
        &tables,
        &reads,
        &position,
        &new_types,
        &new_working,
        &new_recurring,
    );
    // what the recursive part computes for the columns that went is not computed
    visit(&mut plan, true, &mut HashSet::new());
    let mut resolved = plan.clone();
    if crate::planner::column_binding_resolver::resolve_column_bindings(&mut resolved).is_err() {
        return;
    }
    **unresolved = plan;
    **recursive = resolved;
    *types = new_types;
    *working = new_working;
    *recurring = new_recurring;
    let anchor = op.children.remove(0);
    op.children.insert(0, narrow(anchor));
    // the readers above read the columns kept at their new places
    WINDOW_RENUMBERING.with(|r| {
        if let Some(renumbering) = r.borrow_mut().as_mut() {
            for (new, &old) in kept.iter().enumerate() {
                renumbering.insert(ColumnBinding::new(cte_index, old), ColumnBinding::new(cte_index, new));
            }
        }
    });
}
