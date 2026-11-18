// Ported from duckdb/src/optimizer/cte_inlining.cpp @ rust-port-baseline
//
//! `CTEInlining`: a materialized CTE that is read once, or that is cheap to run again, becomes
//! its definition in place of each scan of it.
//!
//! The binder here plans each reference of a CTE by itself, so the CTEs this sees are those of the
//! optimizer (`DisjunctiveJoinRewriter`). `LogicalOperatorDeepCopy` is [`deep_copy`], which gives
//! up, as C++'s does for what it cannot copy, on plans with subqueries, DELIM joins, recursive and
//! materialized CTEs.

use super::common_subplan::{defined_indexes_mut, ends_in_aggregate_or_distinct, max_table_index, visit_bindings};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use std::collections::HashMap;

/// Runs the pass over a plan.
pub fn inline(mut plan: LogicalOperator) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    try_inlining(&mut plan, &mut next);
    plan
}

fn count_base_table_references(op: &LogicalOperator) -> usize {
    let own = matches!(
        op.kind,
        K::Get { .. } | K::Range { .. } | K::CsvScan { .. } | K::JsonScan { .. } | K::ParquetScan { .. }
    );
    usize::from(own) + op.children.iter().map(count_base_table_references).sum::<usize>()
}

fn count_cte_references(op: &LogicalOperator, cte: usize) -> usize {
    let own = matches!(op.kind, K::CteRef { cte_index, .. } if cte_index == cte);
    usize::from(own) + op.children.iter().map(|c| count_cte_references(c, cte)).sum::<usize>()
}

/// Whether a scan of the CTE is in a plan of its own: a subquery that stays an expression, a
/// recursive CTE or a lateral join, which C++ has flattened. Those the inlining does not reach.
pub(super) fn has_nested_reference(op: &LogicalOperator, cte: usize) -> bool {
    let nested = match &op.kind {
        K::RecursiveCte { recursive, .. } => count_cte_references(recursive, cte) > 0 || has_nested_reference(recursive, cte),
        K::LateralJoin { right, .. } => count_cte_references(right, cte) > 0 || has_nested_reference(right, cte),
        _ => false,
    };
    let mut in_subquery = false;
    let mut shallow = LogicalOperator::new(op.kind.clone(), Vec::new());
    shallow.visit_expressions(&mut |e| {
        if let BoundExprKind::Subquery { subquery, .. } = &e.kind {
            for plan in std::iter::once(&subquery.plan).chain(subquery.raw.as_ref()) {
                in_subquery |= count_cte_references(plan, cte) > 0 || has_nested_reference(plan, cte);
            }
        }
    });
    nested || in_subquery || op.children.iter().any(|c| has_nested_reference(c, cte))
}

/// `ContainsLimit`: along the operators with one child.
fn contains_limit(op: &LogicalOperator) -> bool {
    if matches!(op.kind, K::Limit { .. } | K::TopN { .. }) {
        return true;
    }
    op.children.len() == 1 && contains_limit(&op.children[0])
}

fn ends_in_dummy_scan(op: &LogicalOperator) -> bool {
    if matches!(op.kind, K::DummyScan { .. } | K::EmptyResult { .. } | K::CteRef { .. }) {
        return true;
    }
    // C++'s `LogicalExpressionGet` of VALUES reads a `LogicalDummyScan`, which this one leaves out
    if matches!(op.kind, K::ExpressionGet { .. }) && op.children.is_empty() {
        return true;
    }
    op.children.len() == 1 && ends_in_dummy_scan(&op.children[0])
}

fn contains_delim_get(op: &LogicalOperator) -> bool {
    matches!(op.kind, K::DelimGet { .. }) || op.children.iter().any(contains_delim_get)
}

fn has_reference_below_delim_join(op: &LogicalOperator, cte: usize, below: bool) -> bool {
    if matches!(op.kind, K::CteRef { cte_index, .. } if cte_index == cte) {
        return below;
    }
    let below = below || matches!(op.kind, K::DelimJoin { .. });
    op.children
        .iter()
        .any(|child| has_reference_below_delim_join(child, cte, below))
}

/// `LogicalOperator::HasSideEffects`.
fn has_side_effects(op: &LogicalOperator) -> bool {
    matches!(
        op.kind,
        K::Insert { .. }
            | K::Update { .. }
            | K::Delete { .. }
            | K::MergeInto { .. }
            | K::CopyToFile { .. }
            | K::CopyToParquet { .. }
            | K::CreateTableAs { .. }
            | K::CreateTable { .. }
    ) || op.children.iter().any(has_side_effects)
}

/// `PreventInlining`: a volatile function other than `error`.
fn prevents_inlining(op: &LogicalOperator) -> bool {
    let mut prevent = false;
    let mut shallow = LogicalOperator::new(op.kind.clone(), Vec::new());
    shallow.visit_expressions(&mut |e| {
        if let BoundExprKind::Function { function, .. } = &e.kind
            && function.name != "error"
            && e.is_volatile()
        {
            prevent = true;
        }
    });
    prevent || op.children.iter().any(prevents_inlining)
}

fn try_inlining(op: &mut LogicalOperator, next: &mut usize) {
    // the deepest CTEs first
    for child in &mut op.children {
        try_inlining(child, next);
    }
    let K::MaterializedCte {
        table_index, always, ..
    } = op.kind
    else {
        return;
    };
    if has_nested_reference(&op.children[1], table_index) {
        return;
    }
    let ref_count = count_cte_references(op, table_index);
    if ref_count == 0 {
        if !has_side_effects(&op.children[0]) {
            *op = op.children.pop().expect("a CTE has two children");
        }
        return;
    }
    if has_side_effects(&op.children[0]) {
        return;
    }
    if contains_delim_get(&op.children[0]) && has_reference_below_delim_join(&op.children[1], table_index, false) {
        return;
    }
    if always {
        return;
    }
    if ref_count == 1 {
        inline_references(op, table_index, false, next);
        return;
    }
    if prevents_inlining(&op.children[0]) || ends_in_aggregate_or_distinct(&op.children[0]) {
        return;
    }
    let definition = &op.children[0];
    let cheap = matches!(definition.kind, K::EmptyResult { .. } | K::CteRef { .. }) || ends_in_dummy_scan(definition);
    let base_tables = count_base_table_references(definition);
    if !cheap && base_tables > 2 && base_tables * ref_count > 10 {
        return;
    }
    if cheap || contains_limit(&op.children[1]) {
        inline_references(op, table_index, true, next);
    }
}

/// `Inline`: the scans of the CTE `op` become its definition, copied if `copy`. Leaves the CTE as
/// it is if the definition cannot be copied.
fn inline_references(op: &mut LogicalOperator, cte: usize, copy: bool, next: &mut usize) {
    if copy && !can_copy(&op.children[0]) {
        return;
    }
    let mut definition = Some(op.children[0].clone());
    let mut rest = op.children.pop().expect("a CTE has two children");
    replace_references(&mut rest, cte, copy, &mut definition, next);
    *op = rest;
}

fn replace_references(
    op: &mut LogicalOperator,
    cte: usize,
    copy: bool,
    definition: &mut Option<LogicalOperator>,
    next: &mut usize,
) {
    if let K::CteRef {
        table_index,
        cte_index,
        ref types,
        ..
    } = op.kind
        && cte_index == cte
    {
        let body = if copy {
            deep_copy(definition.as_ref().expect("the definition"), next)
        } else {
            definition.take().expect("one reference")
        };
        // `LogicalProjection::CreateIdentity`
        let expressions = body
            .column_bindings()
            .into_iter()
            .zip(types.iter())
            .map(|(binding, ty)| BoundExpr::column_ref(binding, ty.clone(), ""))
            .collect();
        *op = LogicalOperator::new(
            K::Projection {
                table_index,
                expressions,
            },
            vec![body],
        );
        return;
    }
    for child in &mut op.children {
        replace_references(child, cte, copy, definition, next);
    }
}

/// Whether [`deep_copy`] can copy the plan.
fn can_copy(op: &LogicalOperator) -> bool {
    if matches!(
        op.kind,
        K::DelimJoin { .. }
            | K::DelimGet { .. }
            | K::RecursiveCte { .. }
            | K::WorkingTable { .. }
            | K::MaterializedCte { .. }
            | K::LateralJoin { .. }
    ) {
        return false;
    }
    let mut subquery = false;
    let mut shallow = LogicalOperator::new(op.kind.clone(), Vec::new());
    shallow.visit_expressions(&mut |e| subquery |= matches!(e.kind, BoundExprKind::Subquery { .. }));
    !subquery && op.children.iter().all(can_copy)
}

/// `LogicalOperatorDeepCopy`: a copy of the plan whose operators have new table indexes.
pub(super) fn deep_copy(op: &LogicalOperator, next: &mut usize) -> LogicalOperator {
    let mut copy = op.clone();
    let mut map = HashMap::new();
    renumber(&mut copy, &mut map, next);
    rebind(&mut copy, &map);
    copy
}

fn renumber(op: &mut LogicalOperator, map: &mut HashMap<usize, usize>, next: &mut usize) {
    for child in &mut op.children {
        renumber(child, map, next);
    }
    for index in defined_indexes_mut(&mut op.kind) {
        let new = *map.entry(*index).or_insert_with(|| {
            let fresh = *next;
            *next += 1;
            fresh
        });
        *index = new;
    }
}

fn rebind(op: &mut LogicalOperator, map: &HashMap<usize, usize>) {
    for child in &mut op.children {
        rebind(child, map);
    }
    visit_bindings(&mut op.kind, &mut |binding: &mut ColumnBinding, depth| {
        if depth == 0
            && let Some(new) = map.get(&binding.table_index)
        {
            binding.table_index = *new;
        }
    });
}
