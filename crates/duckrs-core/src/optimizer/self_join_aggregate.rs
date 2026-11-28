// Ported from duckdb/src/optimizer/self_join_aggregate.cpp @ rust-port-baseline
//
//! `SelfJoinAggregate` (`SELF_JOIN_AGGREGATE`): a self-join of one table on equal keys `K` and on
//! `a.x <> b.x` for one more column, read only through `K` by a parent that ignores duplicates, is
//! `GROUP BY K HAVING min(x) <> max(x)` over one scan (TPC-DS q95's `ws_wh`).
//!
//! The join's rows are the pairs of rows with the same non-NULL keys and different non-NULL `x`;
//! the set of their keys is the set of the non-NULL key groups with two different `x`, which is
//! `min(x) <> max(x)` over the group. The NULL key group is filtered out first. Parents that
//! ignore duplicates are found from the root down: the right side of a SEMI, ANTI or MARK join,
//! a DISTINCT, and a GROUP BY of min, max or DISTINCT aggregates; below them the property passes
//! through projections and filters without volatile expressions, cross products and INNER joins.
//! A materialized CTE's definition ignores duplicates when all its references do.

use super::rewriter::split_conjuncts;
use crate::function::BuiltinFunctions;
use crate::function::binder::bind_aggregate_function;
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{
    COLUMN_IDENTIFIER_ROW_ID, COLUMN_IDENTIFIER_ROW_NUMBER, LogicalOperator, LogicalOperatorKind as K,
};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `SelfJoinAggregate::Optimize`.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    let mut walk = Walk::default();
    walk.walk(&plan, false, &mut Vec::new());
    if walk.candidates.is_empty() {
        return plan;
    }
    // the table indexes each candidate's columns are read through
    let mut readers: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut next = super::common_subplan::max_table_index(&plan) + 1;
    for candidate in &walk.candidates {
        let K::Projection { table_index, .. } = &at(&plan, &candidate.path).kind else {
            unreachable!("a candidate is a projection")
        };
        let through = match candidate.cte {
            Some(cte) => walk.cte_references.get(&cte).cloned().unwrap_or_default(),
            None => vec![*table_index],
        };
        readers.insert(*table_index, through);
    }
    let mut live = HashSet::new();
    live_bindings(&plan, &readers, &mut live);
    // deepest first: a rewrite replaces what is below its projection, never what is above it
    for candidate in walk.candidates.iter().rev() {
        let projection = at_mut(&mut plan, &candidate.path);
        let K::Projection {
            table_index,
            expressions,
        } = &projection.kind
        else {
            unreachable!()
        };
        let through = &readers[table_index];
        let read: Vec<bool> = (0..expressions.len())
            .map(|k| through.iter().any(|&r| live.contains(&ColumnBinding::new(r, k))))
            .collect();
        rewrite(projection, &read, &mut next);
    }
    plan
}

fn at<'a>(op: &'a LogicalOperator, path: &[usize]) -> &'a LogicalOperator {
    path.iter().fold(op, |op, &i| &op.children[i])
}

fn at_mut<'a>(op: &'a mut LogicalOperator, path: &[usize]) -> &'a mut LogicalOperator {
    path.iter().fold(op, |op, &i| &mut op.children[i])
}

fn any_volatile(op: &LogicalOperator) -> bool {
    let mut probe = LogicalOperator::new(op.kind.clone(), Vec::new());
    probe.expressions_mut().iter().any(|e| e.is_volatile())
}

/// `AggregateIgnoresDuplicates`: the same result over its input with or without repeated rows.
fn aggregate_ignores_duplicates(groups: &[BoundExpr], aggregates: &[BoundAggregate]) -> bool {
    !groups.iter().any(BoundExpr::is_volatile)
        && aggregates.iter().all(|aggregate| {
            let volatile = aggregate.children.iter().any(BoundExpr::is_volatile);
            !volatile
                && (aggregate.distinct
                    || (matches!(aggregate.function.name.as_str(), "min" | "max") && aggregate.children.len() == 1))
        })
}

struct Candidate {
    path: Vec<usize>,
    /// the CTE the projection defines, read through its references
    cte: Option<usize>,
}

#[derive(Default)]
struct Walk {
    candidates: Vec<Candidate>,
    /// a CTE's index -> whether each of its references seen so far ignores duplicates
    cte_ignores: HashMap<usize, bool>,
    /// a CTE's index -> the table indexes of its references
    cte_references: HashMap<usize, Vec<usize>>,
}

impl Walk {
    /// `ignores`: the parent of `op` gives the same result with or without repeated rows.
    fn child(&mut self, op: &LogicalOperator, i: usize, ignores: bool, path: &mut Vec<usize>) {
        path.push(i);
        self.walk(&op.children[i], ignores, path);
        path.pop();
    }

    fn walk(&mut self, op: &LogicalOperator, ignores: bool, path: &mut Vec<usize>) {
        match &op.kind {
            K::Projection { .. } => {
                if ignores {
                    self.candidates.push(Candidate {
                        path: path.clone(),
                        cte: None,
                    });
                }
                self.child(op, 0, ignores && !any_volatile(op), path);
            }
            K::Filter { .. } => self.child(op, 0, ignores && !any_volatile(op), path),
            K::CrossProduct => {
                self.child(op, 0, ignores, path);
                self.child(op, 1, ignores, path);
            }
            K::Join { join_type, .. } => {
                let deterministic = !any_volatile(op);
                let (left, right) = match join_type {
                    JoinType::Inner => (ignores, ignores),
                    JoinType::Semi | JoinType::Anti => (ignores, true),
                    JoinType::RightSemi | JoinType::RightAnti => (true, ignores),
                    _ => (false, false),
                };
                self.child(op, 0, left && deterministic, path);
                self.child(op, 1, right && deterministic, path);
            }
            // a MARK join's NULL depends on whether a NULL occurs, not how often
            K::MarkJoin { .. } => {
                let deterministic = !any_volatile(op);
                self.child(op, 0, ignores && deterministic, path);
                self.child(op, 1, deterministic, path);
            }
            K::Aggregate { groups, aggregates, .. } => {
                self.child(op, 0, aggregate_ignores_duplicates(groups, aggregates), path)
            }
            // its targets are expressions, so what it deduplicates on counts as read
            K::Distinct { .. } => self.child(op, 0, true, path),
            // the ORDER BY of a DISTINCT ON, which C++ keeps in the distinct: it changes no row
            K::Order { .. } => self.child(op, 0, ignores && !any_volatile(op), path),
            K::MaterializedCte { table_index, .. } => {
                // its references are all below its second child
                self.child(op, 1, ignores, path);
                let definition_ignores = self.cte_ignores.get(table_index).copied().unwrap_or(false);
                let definition = &op.children[0];
                if matches!(definition.kind, K::Projection { .. }) {
                    path.push(0);
                    if definition_ignores {
                        self.candidates.push(Candidate {
                            path: path.clone(),
                            cte: Some(*table_index),
                        });
                    }
                    path.push(0);
                    self.walk(
                        &definition.children[0],
                        definition_ignores && !any_volatile(definition),
                        path,
                    );
                    path.pop();
                    path.pop();
                } else {
                    self.child(op, 0, definition_ignores, path);
                }
            }
            K::CteRef {
                table_index, cte_index, ..
            } => {
                let entry = self.cte_ignores.entry(*cte_index).or_insert(true);
                *entry = *entry && ignores;
                self.cte_references.entry(*cte_index).or_default().push(*table_index);
            }
            _ => {
                for i in 0..op.children.len() {
                    self.child(op, i, false, path);
                }
            }
        }
    }
}

/// `LiveBindings`: the columns the plan reads, from the root down, where a candidate projection's
/// expressions count only when the column they compute is read.
fn live_bindings(op: &LogicalOperator, readers: &HashMap<usize, Vec<usize>>, read: &mut HashSet<ColumnBinding>) {
    if matches!(op.kind, K::MaterializedCte { .. }) {
        live_bindings(&op.children[1], readers, read);
        live_bindings(&op.children[0], readers, read);
        return;
    }
    let collect = |expr: &BoundExpr, read: &mut HashSet<ColumnBinding>| {
        expr.clone().walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, .. } = &e.kind {
                read.insert(*binding);
            }
        });
    };
    match &op.kind {
        K::Projection {
            table_index,
            expressions,
        } if readers.contains_key(table_index) => {
            for (k, expr) in expressions.iter().enumerate() {
                if readers[table_index]
                    .iter()
                    .any(|&r| read.contains(&ColumnBinding::new(r, k)))
                {
                    collect(expr, read);
                }
            }
        }
        _ => {
            let mut kind = op.kind.clone();
            super::common_subplan::visit_bindings(&mut kind, &mut |binding, _| {
                read.insert(*binding);
            });
        }
    }
    // the columns DISTINCT ON does not deduplicate on pass through by position
    if let K::Distinct { on: true, .. } = op.kind {
        read.extend(op.children[0].column_bindings());
    }
    for child in &op.children {
        live_bindings(child, readers, read);
    }
}

/// `PlainScan`: a scan of a whole table.
fn plain_scan(op: &LogicalOperator) -> Option<(&Arc<crate::catalog::TableCatalogEntry>, usize, &[usize])> {
    let K::Get {
        table,
        table_index,
        column_ids,
        filters,
        zone_filters,
        optional_filters,
        index_rows,
        sample,
        ..
    } = &op.kind
    else {
        return None;
    };
    (op.children.is_empty()
        && filters.is_empty()
        && zone_filters.is_empty()
        && optional_filters.is_empty()
        && index_rows.is_none()
        && sample.is_none())
    .then_some((table, *table_index, column_ids.as_slice()))
}

/// One column of the table, as each of the two scans outputs it.
#[derive(Clone)]
struct PairedColumn {
    column: usize,
    left: usize,
    right: usize,
    ty: LogicalType,
}

/// `OrderMatchesEquality`: `min <> max` says exactly "two different values".
fn order_matches_equality(ty: &LogicalType) -> bool {
    ty.is_integral() || ty.id() == LogicalTypeId::Date
}

/// `SelfJoinAggregateRewriter::Rewrite`: the self-join below `projection`, if it is one, as a
/// group-by. `read[k]`: whether column `k` is read above.
fn rewrite(projection: &mut LogicalOperator, read: &[bool], next: &mut usize) -> bool {
    let mut predicates: Vec<BoundExpr> = Vec::new();
    let mut op = &projection.children[0];
    while let K::Filter { expressions } = &op.kind {
        for expr in expressions {
            split_conjuncts(expr.clone(), &mut predicates);
        }
        op = &op.children[0];
    }
    let mut conditions: Vec<(ComparisonOp, BoundExpr, BoundExpr)> = Vec::new();
    match &op.kind {
        K::Join {
            join_type: JoinType::Inner,
            conditions: join_conditions,
            predicate,
            ..
        } => {
            for c in join_conditions {
                conditions.push((c.comparison, c.left.clone(), c.right.clone()));
            }
            if let Some(predicate) = predicate {
                split_conjuncts(predicate.clone(), &mut predicates);
            }
        }
        K::CrossProduct => {}
        _ => return false,
    }
    let (Some((left_table, left_index, left_columns)), Some((right_table, right_index, right_columns))) =
        (plain_scan(&op.children[0]), plain_scan(&op.children[1]))
    else {
        return false;
    };
    if !Arc::ptr_eq(left_table, right_table) {
        return false;
    }
    for predicate in predicates {
        let BoundExprKind::Comparison { op, left, right } = predicate.kind else {
            return false;
        };
        conditions.push((op, *left, *right));
    }
    // every condition a `=` (a key) or `<>` (the inequality) between the same column of the two scans
    let scan_column = |columns: &[usize], at: usize| {
        columns
            .get(at)
            .copied()
            .filter(|&c| c != COLUMN_IDENTIFIER_ROW_ID && c != COLUMN_IDENTIFIER_ROW_NUMBER)
    };
    let mut keys: Vec<PairedColumn> = Vec::new();
    let mut inequalities: Vec<PairedColumn> = Vec::new();
    for (comparison, lhs, rhs) in &conditions {
        if !matches!(comparison, ComparisonOp::Equal | ComparisonOp::NotEqual) {
            return false;
        }
        let (
            &BoundExprKind::ColumnRef {
                binding: mut lb,
                depth: 0,
            },
            &BoundExprKind::ColumnRef {
                binding: mut rb,
                depth: 0,
            },
        ) = (&lhs.kind, &rhs.kind)
        else {
            return false;
        };
        if lb.table_index == right_index && rb.table_index == left_index {
            std::mem::swap(&mut lb, &mut rb);
        }
        if lb.table_index != left_index || rb.table_index != right_index {
            return false;
        }
        let (Some(left_column), Some(right_column)) = (
            scan_column(left_columns, lb.column_index),
            scan_column(right_columns, rb.column_index),
        ) else {
            return false;
        };
        if left_column != right_column
            || lhs.return_type != rhs.return_type
            || !order_matches_equality(&lhs.return_type)
        {
            return false;
        }
        let into = if *comparison == ComparisonOp::Equal {
            &mut keys
        } else {
            &mut inequalities
        };
        if let Some(known) = into.iter().find(|k| k.column == left_column) {
            if known.left != lb.column_index || known.right != rb.column_index {
                return false;
            }
            continue;
        }
        into.push(PairedColumn {
            column: left_column,
            left: lb.column_index,
            right: rb.column_index,
            ty: lhs.return_type.clone(),
        });
    }
    let [inequality] = inequalities.as_slice() else {
        return false;
    };
    if keys.is_empty() || keys.iter().any(|k| k.column == inequality.column) {
        return false;
    }
    let inequality = inequality.clone();
    // each column read above is a key, of either scan: the group-by outputs it
    let K::Projection { expressions, .. } = &projection.kind else {
        unreachable!()
    };
    let mut key_of: Vec<Option<usize>> = vec![None; expressions.len()];
    for (k, expr) in expressions.iter().enumerate() {
        if !read[k] {
            continue;
        }
        let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind else {
            return false;
        };
        key_of[k] = keys.iter().position(|key| {
            (binding.table_index == left_index && binding.column_index == key.left)
                || (binding.table_index == right_index && binding.column_index == key.right)
        });
        if key_of[k].is_none() {
            return false;
        }
    }
    let functions = BuiltinFunctions::get();
    let column =
        |at: usize, ty: &LogicalType| BoundExpr::column_ref(ColumnBinding::new(left_index, at), ty.clone(), "");
    let mut bounds = Vec::new();
    for name in ["min", "max"] {
        let Some(set) = functions.aggregate(name) else {
            return false;
        };
        let Ok(bound) = bind_aggregate_function(set, vec![column(inequality.left, &inequality.ty)], None, false) else {
            return false;
        };
        bounds.push(bound);
    }

    // scan WHERE keys IS NOT NULL GROUP BY keys HAVING min(x) <> max(x)
    let mut join = std::mem::replace(
        &mut projection.children[0],
        LogicalOperator::new(K::CrossProduct, Vec::new()),
    );
    let mut below = &mut join;
    while matches!(below.kind, K::Filter { .. }) {
        below = &mut below.children[0];
    }
    let scan = below.children.swap_remove(0);
    let not_null = keys
        .iter()
        .map(|key| {
            BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::IsNotNull,
                    children: vec![column(key.left, &key.ty)],
                },
                LogicalType::BOOLEAN,
            )
        })
        .collect();
    let filter = LogicalOperator::new(K::Filter { expressions: not_null }, vec![scan]);
    let group_index = take(next);
    let aggregate_index = take(next);
    let aggregate = LogicalOperator::new(
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index: take(next),
            groups: keys.iter().map(|key| column(key.left, &key.ty)).collect(),
            aggregates: bounds,
            grouping_sets: Vec::new(),
            grouping_functions: Vec::new(),
            perfect_hash: None,
            domain_cap: None,
        },
        vec![filter],
    );
    let bound = |i| BoundExpr::column_ref(ColumnBinding::new(aggregate_index, i), inequality.ty.clone(), "");
    let having = LogicalOperator::new(
        K::Filter {
            expressions: vec![BoundExpr::new(
                BoundExprKind::Comparison {
                    op: ComparisonOp::NotEqual,
                    left: Box::new(bound(0)),
                    right: Box::new(bound(1)),
                },
                LogicalType::BOOLEAN,
            )],
        },
        vec![aggregate],
    );
    // the projection keeps its table index and its columns' types: a read key from the groups, the
    // rest a NULL
    let K::Projection { expressions, .. } = &mut projection.kind else {
        unreachable!()
    };
    for (k, expr) in expressions.iter_mut().enumerate() {
        let alias = expr.alias.clone();
        let mut replacement = match key_of[k] {
            Some(key) => BoundExpr::column_ref(ColumnBinding::new(group_index, key), expr.return_type.clone(), ""),
            None => BoundExpr::constant(Value::null(expr.return_type.clone())),
        };
        replacement.alias = alias;
        *expr = replacement;
    }
    projection.children[0] = having;
    true
}

fn take(next: &mut usize) -> usize {
    let index = *next;
    *next += 1;
    index
}
