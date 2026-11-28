// Ported from duckdb/src/optimizer/remove_redundant_order_keys.cpp and
// apply_functional_dependencies.cpp @ rust-port-baseline
//
//! `RemoveRedundantOrderKeys` (`ApplyFunctionalDependencies`): sort keys that the keys before them
//! decide are left out, and so are the keys after a prefix that holds a unique key of the input
//! (a primary key, a UNIQUE NOT NULL column, the groups of an aggregate, the targets of a
//! DISTINCT). Window partitions are cut down the same way.

use crate::catalog::TableConstraint;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundWindow, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use pintail_parser::ast::ConstraintCheckMode;
use std::collections::{HashMap, HashSet};

type Key = HashSet<ColumnBinding>;

/// Runs the pass over a plan.
pub fn remove(mut plan: LogicalOperator) -> LogicalOperator {
    visit(&mut plan);
    plan
}

/// Whether the expressions compute the same.
fn equals(a: &BoundExpr, b: &BoundExpr) -> bool {
    let unnamed = |expression: &BoundExpr| {
        let mut expression = expression.clone();
        expression.walk_mut(&mut |e| e.alias.clear());
        expression.to_string()
    };
    a.return_type == b.return_type && unnamed(a) == unnamed(b)
}

/// `BaseSelectBinder::IsFunctionallyDependent`: whether `expr` is decided by `dependencies`.
fn is_functionally_dependent(expr: &BoundExpr, dependencies: &[&BoundExpr]) -> bool {
    if expr.is_volatile() {
        return false;
    }
    if expr.is_foldable() {
        return true;
    }
    if dependencies.iter().any(|dependency| equals(expr, dependency)) {
        return true;
    }
    let children = expr.children();
    !children.is_empty()
        && children
            .into_iter()
            .all(|child| is_functionally_dependent(child, dependencies))
}

/// The unique keys of `op`'s rows, after simplifying the sorts at and below it.
fn visit(op: &mut LogicalOperator) -> Vec<Key> {
    match &op.kind {
        K::Order { .. } => {
            let keys = visit(&mut op.children[0]);
            if let K::Order { orders } = &mut op.kind {
                visit_order_bys(orders, &keys, |node| &node.expr);
            }
            keys
        }
        K::Filter { .. } | K::Limit { .. } | K::ColumnSelect { .. } => visit(&mut op.children[0]),
        // windowing only adds bindings
        K::Window { .. } => {
            let keys = visit(&mut op.children[0]);
            if let K::Window { windows, .. } = &mut op.kind {
                for window in windows {
                    visit_window(window, &keys);
                }
            }
            keys
        }
        K::Projection { .. } => {
            let keys = visit(&mut op.children[0]);
            let K::Projection {
                table_index,
                expressions,
            } = &op.kind
            else {
                unreachable!()
            };
            let mut forwarded: HashMap<ColumnBinding, ColumnBinding> = HashMap::new();
            for (at, expr) in expressions.iter().enumerate() {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
                    forwarded
                        .entry(*binding)
                        .or_insert(ColumnBinding::new(*table_index, at));
                }
            }
            keys.iter()
                .filter_map(|key| key.iter().map(|b| forwarded.get(b).copied()).collect::<Option<Key>>())
                .collect()
        }
        K::Get { .. } => get_keys(op),
        K::Aggregate { .. } => {
            visit(&mut op.children[0]);
            let K::Aggregate {
                group_index,
                groups,
                grouping_sets,
                grouping_functions,
                ..
            } = &op.kind
            else {
                unreachable!()
            };
            if groups.is_empty() || grouping_sets.len() > 1 || !grouping_functions.is_empty() {
                return Vec::new();
            }
            let key: Key = match grouping_sets.first() {
                None => (0..groups.len()).map(|i| ColumnBinding::new(*group_index, i)).collect(),
                Some(set) => {
                    if set.iter().any(|&i| i >= groups.len()) {
                        return Vec::new();
                    }
                    set.iter().map(|&i| ColumnBinding::new(*group_index, i)).collect()
                }
            };
            if key.is_empty() { Vec::new() } else { vec![key] }
        }
        K::Distinct { .. } => {
            visit(&mut op.children[0]);
            let K::Distinct { targets, .. } = &op.kind else {
                unreachable!()
            };
            let key: Option<Key> = targets
                .iter()
                .map(|target| match &target.kind {
                    BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
                    _ => None,
                })
                .collect();
            match key {
                Some(key) if !key.is_empty() => vec![key],
                _ => Vec::new(),
            }
        }
        // what the operator does with the keys is not known
        _ => {
            for child in &mut op.children {
                visit(child);
            }
            Vec::new()
        }
    }
}

/// `VisitLogicalGet`: the primary keys, and the UNIQUE keys of NOT NULL columns, that the scan
/// outputs all columns of.
fn get_keys(op: &LogicalOperator) -> Vec<Key> {
    let K::Get { table, column_ids, .. } = &op.kind else {
        return Vec::new();
    };
    if column_ids.is_empty() {
        return Vec::new();
    }
    let mut bindings: HashMap<usize, ColumnBinding> = HashMap::new();
    for binding in op.column_bindings() {
        let column = column_ids[binding.column_index];
        if column != COLUMN_IDENTIFIER_ROW_ID {
            bindings.entry(column).or_insert(binding);
        }
    }
    let not_null: HashSet<usize> = table
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            TableConstraint::NotNull(column) => Some(*column),
            _ => None,
        })
        .chain(
            table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.not_null)
                .map(|(i, _)| i),
        )
        .collect();
    let mut keys = Vec::new();
    for constraint in &table.constraints {
        let TableConstraint::Unique {
            columns,
            primary_key,
            check_mode,
            ..
        } = constraint
        else {
            continue;
        };
        if *check_mode == ConstraintCheckMode::Deferred {
            continue;
        }
        let key: Option<Key> = columns
            .iter()
            .map(|column| {
                let binding = bindings.get(column)?;
                (*primary_key || not_null.contains(column)).then_some(*binding)
            })
            .collect();
        if let Some(key) = key
            && !key.is_empty()
        {
            keys.push(key);
        }
    }
    keys
}

/// `VisitOrderBys`: leaves out the orders decided by those before them, and those after a prefix
/// that holds a unique key.
fn visit_order_bys<T>(orders: &mut Vec<T>, keys: &[Key], expr: impl Fn(&T) -> &BoundExpr) {
    if orders.len() < 2 {
        return;
    }
    let mut at = 1;
    while at < orders.len() {
        let before: Vec<&BoundExpr> = orders[..at].iter().map(&expr).collect();
        if is_functionally_dependent(expr(&orders[at]), &before) {
            orders.remove(at);
        } else {
            at += 1;
        }
    }
    if keys.is_empty() {
        return;
    }
    let mut prefix = Key::new();
    for i in 0..orders.len().saturating_sub(1) {
        let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr(&orders[i]).kind else {
            continue;
        };
        prefix.insert(*binding);
        if keys.iter().any(|key| key.iter().all(|b| prefix.contains(b))) {
            orders.truncate(i + 1);
            return;
        }
    }
}

/// `VisitPartitioning`: leaves out the partitions decided by the others, and cuts them down to
/// the smallest unique key they hold.
fn visit_partitioning(partitions: &mut Vec<BoundExpr>, keys: &[Key]) {
    let mut at = 0;
    while at < partitions.len() {
        let others: Vec<&BoundExpr> = partitions
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != at)
            .map(|(_, p)| p)
            .collect();
        if is_functionally_dependent(&partitions[at], &others) {
            partitions.remove(at);
        } else {
            at += 1;
        }
    }
    let bindings: Key = partitions
        .iter()
        .filter_map(|p| match &p.kind {
            BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
            _ => None,
        })
        .collect();
    let mut smallest: Option<&Key> = None;
    for key in keys {
        if key.iter().all(|b| bindings.contains(b)) && smallest.is_none_or(|s| key.len() < s.len()) {
            smallest = Some(key);
        }
    }
    if let Some(key) = smallest {
        partitions
            .retain(|p| matches!(&p.kind, BoundExprKind::ColumnRef { binding, depth: 0 } if key.contains(binding)));
    }
}

/// `VisitWindowExpression`.
fn visit_window(window: &mut BoundWindow, keys: &[Key]) {
    visit_partitioning(&mut window.partitions, keys);
    visit_order_bys(&mut window.orders, keys, |(expr, _)| expr);
    // a single order decided by the partitions is one peer group, as without one
    if !window.partitions.is_empty() && window.orders.len() == 1 {
        let partitions: Vec<&BoundExpr> = window.partitions.iter().collect();
        if is_functionally_dependent(&window.orders[0].0, &partitions) {
            window.orders.clear();
        }
    }
}
