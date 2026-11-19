// Ported from duckdb/src/optimizer/late_materialization.cpp and
// duckdb/src/optimizer/late_materialization_helper.cpp @ rust-port-baseline
//
//! `LateMaterialization` (`LATE_MATERIALIZATION`): a Top-N, LIMIT or SAMPLE over a scan that only
//! needs a few of the scan's columns to pick its rows reads just those and the `rowid`. A SEMI join
//! on the `rowid` with a second scan then fetches the other columns of the rows it picked:
//! `SELECT * FROM t ORDER BY a LIMIT 5` sorts `a` and `rowid`, and reads the rest for five rows.
//!
//! C++ runs this before the unused columns are removed and removes them again after. Here the
//! unused columns are gone already, so a scan's columns are the ones it outputs, and the removal
//! runs again once the plan changed.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, JoinCondition, LogicalOperator, LogicalOperatorKind as K,
};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType};
use duckrs_common::types::LogicalType;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::BTreeSet;

/// The most rows a LIMIT can have past the first `late_materialization_max_rows` (`LIMIT_MAX_VAL`).
const LIMIT_MAX_VAL: u64 = 1_000_000;

/// The settings the pass reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LateMaterializationOptions {
    /// `late_materialization_max_rows`
    pub max_rows: u64,
    /// `preserve_insertion_order`
    pub preserve_insertion_order: bool,
}

/// `LateMaterialization::Optimize`. Says whether the plan changed.
pub fn optimize(plan: &mut LogicalOperator, options: LateMaterializationOptions) -> bool {
    let mut next = max_table_index(plan) + 1;
    visit(plan, options, &mut next, true)
}

/// `root`: the columns of `op` are the result's, by position.
fn visit(op: &mut LogicalOperator, options: LateMaterializationOptions, next: &mut usize, root: bool) -> bool {
    let candidate = match &op.kind {
        K::Limit {
            limit: Some(limit),
            offset,
            failure: None,
        } => {
            let has_offset = *offset > 0;
            if *limit > options.max_rows {
                // a large limit, only where the rows are read in order and the row ids follow each other
                optimize_large_limit(op, *limit, has_offset, options)
            } else {
                // a small limit only gains with an offset
                has_offset
            }
        }
        K::TopN { limit, .. } => *limit <= options.max_rows,
        K::Sample { options: sample } => {
            !sample.is_percentage && sample.sample_size.parse::<u64>().is_ok_and(|n| n <= options.max_rows)
        }
        _ => false,
    };
    if candidate && try_late_materialization(op, next, root) {
        return true;
    }
    let mut changed = false;
    for child in &mut op.children {
        changed |= visit(child, options, next, false);
    }
    changed
}

/// `OptimizeLargeLimit`: only projections between the limit and a scan without filters.
fn optimize_large_limit(op: &LogicalOperator, limit: u64, has_offset: bool, options: LateMaterializationOptions) -> bool {
    if !has_offset && !options.preserve_insertion_order {
        return false;
    }
    if limit > LIMIT_MAX_VAL {
        return false;
    }
    let mut current = &op.children[0];
    loop {
        match &current.kind {
            K::Get { filters, .. } => return filters.is_empty(),
            K::Projection { .. } => current = &current.children[0],
            _ => return false,
        }
    }
}

fn collect_columns(expr: &BoundExpr, out: &mut BTreeSet<ColumnBinding>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        out.insert(*binding);
    }
    for child in expr.children() {
        collect_columns(child, out);
    }
}

/// The expressions of the operator the chain starts at: the orders of a Top-N.
fn root_expressions(op: &LogicalOperator) -> Vec<&BoundExpr> {
    match &op.kind {
        K::TopN { orders, .. } => orders.iter().map(|o| &o.expr).collect(),
        _ => Vec::new(),
    }
}

/// The output columns of a scan: its positions in `column_ids`, with their types.
fn scan_outputs(get: &LogicalOperator) -> Vec<(ColumnBinding, LogicalType)> {
    get.column_bindings().into_iter().zip(get.types()).collect()
}

/// `GetOrInsertRowIds`: the position of the `rowid` among the columns of `get`, which outputs it.
pub(super) fn get_or_insert_row_id(get: &mut LogicalOperator) -> usize {
    let K::Get {
        column_ids, projection, ..
    } = &mut get.kind
    else {
        unreachable!("a scan")
    };
    let position = match column_ids.iter().position(|&c| c == COLUMN_IDENTIFIER_ROW_ID) {
        Some(position) => position,
        None => {
            column_ids.push(COLUMN_IDENTIFIER_ROW_ID);
            column_ids.len() - 1
        }
    };
    if let Some(projection) = projection
        && !projection.contains(&position)
    {
        projection.push(position);
    }
    position
}

/// Every column reference to `from` in the operator's expressions refers to `to`.
fn rename_table(kind: &mut K, from: usize, to: usize) {
    visit_bindings(kind, &mut |binding, depth| {
        if depth == 0 && binding.table_index == from {
            binding.table_index = to;
        }
    });
}

fn replace_columns(expr: &mut BoundExpr, f: &impl Fn(ColumnBinding) -> Option<BoundExpr>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind
        && let Some(replacement) = f(*binding)
    {
        let alias = std::mem::take(&mut expr.alias);
        *expr = replacement;
        if expr.alias.is_empty() {
            expr.alias = alias;
        }
        return;
    }
    for child in expr.children_mut() {
        replace_columns(child, f);
    }
}

/// `TryLateMaterialization`.
fn try_late_materialization(op: &mut LogicalOperator, next: &mut usize, root: bool) -> bool {
    // the columns the chain from the root down to the scan needs
    let mut referenced = BTreeSet::new();
    for expr in root_expressions(op) {
        collect_columns(expr, &mut referenced);
    }
    // the projections of the chain, from the top
    let mut projections: Vec<(usize, Vec<BoundExpr>)> = Vec::new();
    let mut current = &op.children[0];
    loop {
        match &current.kind {
            K::Projection {
                table_index,
                expressions,
            } => {
                if expressions.iter().any(BoundExpr::is_volatile) {
                    return false;
                }
                // only the expressions that are read count
                let read: Vec<usize> = referenced
                    .iter()
                    .filter(|b| b.table_index == *table_index)
                    .map(|b| b.column_index)
                    .collect();
                referenced.clear();
                for i in read {
                    if let Some(expr) = expressions.get(i) {
                        collect_columns(expr, &mut referenced);
                    }
                }
                projections.push((*table_index, expressions.clone()));
                current = &current.children[0];
            }
            K::Filter { expressions } => {
                for expr in expressions {
                    collect_columns(expr, &mut referenced);
                }
                current = &current.children[0];
            }
            K::Get { .. } => break,
            _ => return false,
        }
    }
    // without a projection the final columns are all of the scan's, which the result would show
    if root && projections.is_empty() {
        return false;
    }
    let get = current;
    let K::Get {
        table_index: get_index,
        filters,
        index_rows,
        sample,
        ..
    } = &get.kind
    else {
        unreachable!("the loop ends at a scan")
    };
    let get_index = *get_index;
    // the scan's filters read columns too
    for filter in filters {
        collect_columns(filter, &mut referenced);
    }
    let required = referenced.iter().filter(|b| b.table_index == get_index).count();
    let outputs = scan_outputs(get);
    // a scan that has to read every column it outputs gains nothing; index scans and row samples are
    // as cheap already
    if required >= outputs.len() || index_rows.is_some() || sample.as_ref().is_some_and(|s| !s.options.is_percentage) {
        return false;
    }
    let is_top_n = matches!(op.kind, K::TopN { .. });

    // the LHS: a copy of the scan whose filters only skip row groups, as the RHS applies them
    let lhs_index = take(next);
    let mut lhs = get.clone();
    if let K::Get {
        table_index,
        filters,
        optional_filters,
        projection,
        ..
    } = &mut lhs.kind
    {
        *table_index = lhs_index;
        optional_filters.extend(std::mem::take(filters));
        // all its columns, of which the removal of unused columns keeps what is read
        *projection = None;
    }
    rename_table(&mut lhs.kind, get_index, lhs_index);
    let lhs_row_id = get_or_insert_row_id(&mut lhs);
    let lhs_row_id_binding = ColumnBinding::new(lhs_index, lhs_row_id);
    let lhs_types = lhs.types();

    // the final columns: the root projection with the chain inlined into it, or the scan's columns
    let final_columns: Vec<BoundExpr> = if projections.is_empty() {
        let K::Get { column_ids, .. } = &get.kind else {
            unreachable!()
        };
        (0..column_ids.len())
            .map(|i| BoundExpr::column_ref(ColumnBinding::new(lhs_index, i), lhs_types[i].clone(), ""))
            .collect()
    } else {
        let mut columns = projections[0].1.clone();
        for level in 1..projections.len() {
            let below = &projections[level].1;
            for expr in &mut columns {
                replace_columns(expr, &|binding| below.get(binding.column_index).cloned());
            }
        }
        for expr in &mut columns {
            replace_columns(expr, &|binding| {
                (binding.table_index == get_index).then(|| {
                    BoundExpr::column_ref(
                        ColumnBinding::new(lhs_index, binding.column_index),
                        lhs_types[binding.column_index].clone(),
                        "",
                    )
                })
            });
        }
        columns
    };
    let final_index = projections.first().map_or(get_index, |(index, _)| *index);

    // the RHS: the old chain, which outputs the rowid too
    let rhs_row_id = construct_rhs(op);
    // the RHS's top level gets a new index: the final projection takes over the old one
    let rhs_index = take(next);
    rename_top_level(op, final_index, rhs_index);
    let rhs_row_id = ColumnBinding::new(rhs_index, rhs_row_id.column_index);

    let rhs = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
    let join = LogicalOperator::new(
        K::Join {
            join_type: JoinType::Semi,
            conditions: vec![JoinCondition {
                left: BoundExpr::column_ref(lhs_row_id_binding, LogicalType::BIGINT, "rowid"),
                right: BoundExpr::column_ref(rhs_row_id, LogicalType::BIGINT, "rowid"),
                comparison: ComparisonOp::Equal,
                filter: None,
            }],
            predicate: None,
            algorithm: Default::default(),
        },
        vec![lhs, rhs.clone()],
    );
    let projection = |child: LogicalOperator| {
        LogicalOperator::new(
            K::Projection {
                table_index: final_index,
                expressions: final_columns.clone(),
            },
            vec![child],
        )
    };
    *op = if is_top_n {
        // a Top-N orders by expressions, so after the final projection
        let K::TopN { orders, .. } = &rhs.kind else {
            unreachable!()
        };
        let orders = orders
            .iter()
            .map(|order| {
                let mut order = order.clone();
                rename_expression(&mut order.expr, rhs_index, final_index);
                order
            })
            .collect();
        LogicalOperator::new(K::Order { orders }, vec![projection(join)])
    } else {
        // a limit or sample keeps the order of the row ids, before the projection drops them
        let order = BoundOrderByNode {
            modifiers: OrderModifiers::new(OrderType::Ascending, NullOrder::NullsLast),
            expr: BoundExpr::column_ref(lhs_row_id_binding, LogicalType::BIGINT, "rowid"),
        };
        projection(LogicalOperator::new(K::Order { orders: vec![order] }, vec![join]))
    };
    true
}

fn rename_expression(expr: &mut BoundExpr, from: usize, to: usize) {
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
            && binding.table_index == from
        {
            binding.table_index = to;
        }
    });
}

fn take(next: &mut usize) -> usize {
    let index = *next;
    *next += 1;
    index
}

/// `ConstructRHS`: the scan below `op` outputs its rowid, which each projection up to `op` passes
/// on. The binding it has at the top.
fn construct_rhs(op: &mut LogicalOperator) -> ColumnBinding {
    fn walk(op: &mut LogicalOperator) -> ColumnBinding {
        if let K::Get { table_index, .. } = &op.kind {
            let table_index = *table_index;
            return ColumnBinding::new(table_index, get_or_insert_row_id(op));
        }
        let below = walk(&mut op.children[0]);
        match &mut op.kind {
            K::Projection {
                table_index,
                expressions,
            } => {
                expressions.push(BoundExpr::column_ref(below, LogicalType::BIGINT, "rowid"));
                ColumnBinding::new(*table_index, expressions.len() - 1)
            }
            _ => below,
        }
    }
    walk(&mut op.children[0])
}

/// `ReplaceTopLevelTableIndex`: the operator at the top of the chain below `op` (a projection or
/// the scan) gets the index `to`, and what reads it in between refers to that.
fn rename_top_level(op: &mut LogicalOperator, from: usize, to: usize) {
    let mut current = op;
    loop {
        rename_table(&mut current.kind, from, to);
        match &mut current.kind {
            K::Projection { table_index, .. } | K::Get { table_index, .. } if *table_index == from => {
                *table_index = to;
                return;
            }
            _ => current = &mut current.children[0],
        }
    }
}
