// Ported from duckdb/src/optimizer/join_elimination.cpp @ rust-port-baseline
//
//! `JoinElimination`: a LEFT (or RIGHT, or SINGLE) join whose inner side nothing reads goes away
//! when it gives each outer row exactly once: when the join keys cover a set of columns that is
//! unique on the inner side (it ends in a DISTINCT or a GROUP BY), or when a DISTINCT above
//! removes the duplicates anyway.
//!
//! C++ walks the plan in "pipes" that carry the distinct groups found above them down to their
//! children, and those found below them up. Here the groups from above go down as an argument
//! and the unique columns of the inner side are worked out from it.

use super::common_subplan::{node_mut, visit_bindings};
use super::projection_pullup::child_layout_free;
use crate::planner::expression::{BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_parser::ast::JoinType;
use std::collections::HashSet;

type Path = Vec<usize>;
type Group = HashSet<ColumnBinding>;

/// `JoinElimination::Optimize`.
pub fn eliminate(mut plan: LogicalOperator) -> LogicalOperator {
    for _ in 0..10_000 {
        let Some((path, outer)) = find(&plan, &plan, &mut Vec::new(), &[], false) else {
            break;
        };
        let slot = node_mut(&mut plan, &path);
        let kept = slot.children.swap_remove(outer);
        *slot = kept;
    }
    plan
}

/// The bindings of a column reference, if `expr` is one.
fn column_ref(expr: &crate::planner::expression::BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        _ => None,
    }
}

/// The distinct groups of an operator's parent that hold for the operator's rows too, mapped to
/// its columns.
fn groups_for_child(op: &LogicalOperator, above: &[Group]) -> Vec<Group> {
    match &op.kind {
        // these pick rows by count, or compute values that depend on how often rows occur
        K::Limit { .. } | K::LimitPercent { .. } | K::TopN { .. } | K::Sample { .. } | K::Window { .. } => Vec::new(),
        K::Distinct { targets, .. } => {
            let mut groups = above.to_vec();
            if let Some(group) = targets.iter().map(column_ref).collect::<Option<Group>>()
                && !group.is_empty()
            {
                groups.push(group);
            }
            groups
        }
        K::Projection {
            table_index,
            expressions,
        } => {
            let mut groups = above.to_vec();
            for group in above {
                // a group of this projection's columns that are all columns of one table below
                let mapped: Option<Group> = group
                    .iter()
                    .map(|binding| {
                        (binding.table_index == *table_index)
                            .then(|| expressions.get(binding.column_index).and_then(column_ref))
                            .flatten()
                    })
                    .collect();
                if let Some(mapped) = mapped {
                    let tables: HashSet<usize> = mapped.iter().map(|b| b.table_index).collect();
                    if tables.len() == 1 {
                        groups.push(mapped);
                    }
                }
            }
            groups
        }
        _ => above.to_vec(),
    }
}

/// The sets of columns of `op` whose values no two of its rows share.
fn unique_keys(op: &LogicalOperator) -> Vec<Group> {
    match &op.kind {
        K::Distinct { targets, .. } => {
            let mut keys = unique_keys(&op.children[0]);
            if let Some(key) = targets.iter().map(column_ref).collect::<Option<Group>>()
                && !key.is_empty()
            {
                keys.push(key);
            }
            keys
        }
        K::Aggregate {
            group_index,
            groups,
            grouping_sets,
            ..
        } if grouping_sets.len() <= 1 && !groups.is_empty() => {
            vec![(0..groups.len()).map(|i| ColumnBinding::new(*group_index, i)).collect()]
        }
        K::Projection {
            table_index,
            expressions,
        } => unique_keys(&op.children[0])
            .into_iter()
            .filter_map(|key| {
                key.iter()
                    .map(|binding| {
                        expressions
                            .iter()
                            .position(|e| column_ref(e) == Some(*binding))
                            .map(|at| ColumnBinding::new(*table_index, at))
                    })
                    .collect::<Option<Group>>()
            })
            .collect(),
        K::Filter { .. } | K::Order { .. } | K::Limit { .. } | K::TopN { .. } => unique_keys(&op.children[0]),
        _ => Vec::new(),
    }
}

/// Whether the operators of `op` down to its first operator with several children filter a scan
/// (`has_filter`).
fn has_filter(op: &LogicalOperator) -> bool {
    let mut current = op;
    loop {
        // duckrs moves most filters into the scans only after this pass
        match &current.kind {
            K::Get { filters, .. } if !filters.is_empty() => return true,
            K::Filter { .. } => return true,
            _ => {}
        }
        if current.children.len() != 1 {
            return false;
        }
        current = &current.children[0];
    }
}

/// The tables whose columns the plan reads outside of the operator at `skip`.
fn tables_read_outside(op: &LogicalOperator, path: &mut Path, skip: &[usize], tables: &mut HashSet<usize>) {
    if path.as_slice() == skip {
        return;
    }
    let mut kind = op.kind.clone();
    visit_bindings(&mut kind, &mut |binding, _| {
        tables.insert(binding.table_index);
    });
    for (at, child) in op.children.iter().enumerate() {
        path.push(at);
        tables_read_outside(child, path, skip, tables);
        path.pop();
    }
}

/// The first join that can go, children first, with which of its children stays. With `free`,
/// what reads the columns of `op` goes by binding, so they may change.
fn find(
    root: &LogicalOperator,
    op: &LogicalOperator,
    path: &mut Path,
    above: &[Group],
    free: bool,
) -> Option<(Path, usize)> {
    let groups = groups_for_child(op, above);
    for (at, child) in op.children.iter().enumerate() {
        path.push(at);
        let found = find(root, child, path, &groups, child_layout_free(op, at, free));
        path.pop();
        if found.is_some() {
            return found;
        }
    }
    // the root join stays (C++ needs a parent to put the outer side in), as does one whose
    // columns something reads by position
    if path.is_empty() || !free {
        return None;
    }
    let K::Join {
        join_type,
        conditions,
        predicate,
        ..
    } = &op.kind
    else {
        return None;
    };
    let (outer, inner, mut unique) = match join_type {
        JoinType::Left => (0, 1, false),
        JoinType::Single => (0, 1, true),
        JoinType::Right => (1, 0, false),
        _ => return None,
    };
    if has_filter(&op.children[inner]) {
        return None;
    }
    // nothing outside of the join may read the inner side
    let inner_tables: HashSet<usize> = op.children[inner].column_bindings().iter().map(|b| b.table_index).collect();
    let mut read = HashSet::new();
    tables_read_outside(root, &mut Vec::new(), path, &mut read);
    if read.iter().any(|t| inner_tables.contains(t)) {
        return None;
    }
    if !unique {
        // the inner sides of the join keys, all equalities of columns, cover a unique key
        let keys: Option<Group> = predicate
            .is_none()
            .then(|| {
                conditions
                    .iter()
                    .map(|c| {
                        let (left, right) = (column_ref(&c.left)?, column_ref(&c.right)?);
                        (!c.null_safe()).then_some(if inner == 0 { left } else { right })
                    })
                    .collect::<Option<Group>>()
            })
            .flatten();
        if let Some(keys) = keys
            && !keys.is_empty()
        {
            unique = unique_keys(&op.children[inner]).iter().any(|key| key.is_subset(&keys));
        }
    }
    if !unique {
        // a DISTINCT above on columns of the outer side removes what the join repeats
        let outer_bindings: Group = op.children[outer].column_bindings().into_iter().collect();
        unique = groups.iter().any(|group| group.is_subset(&outer_bindings));
    }
    unique.then(|| (path.clone(), outer))
}
