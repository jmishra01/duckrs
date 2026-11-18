// Ported from duckdb/src/optimizer/cte_filter_pusher.cpp @ rust-port-baseline
//
//! `CTEFilterPusher`: where every scan of a materialized CTE has a filter right above it, the CTE
//! need not compute the rows none of them keep. The OR of their filters goes on its definition,
//! and the filter pushdown takes it as far down as it can, leaving those of computed columns above
//! the projection that computes them. The filters of the scans stay.
//!
//! Not ported: the `filter_dependency` of a CTE that one unfiltered scan reads all of, and
//! `CTEJoinFilterPusher`. A scan of the CTE in a subquery expression, a recursive CTE or a lateral
//! join, which C++ has flattened, counts as one without a filter.

use super::cte_inlining::has_nested_reference;
use super::rewriter::{can_throw, contains_barrier};
use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::LogicalType;

/// Runs the pass over a plan.
pub fn push(mut plan: LogicalOperator) -> LogicalOperator {
    let mut ctes = Vec::new();
    collect_ctes(&plan, &mut ctes);
    // the innermost first (`rbegin`)
    for cte in ctes.into_iter().rev() {
        push_into_cte(&mut plan, cte);
    }
    plan
}

fn collect_ctes(op: &LogicalOperator, out: &mut Vec<usize>) {
    if let K::MaterializedCte { table_index, .. } = op.kind {
        out.push(table_index);
    }
    for child in &op.children {
        collect_ctes(child, out);
    }
}

/// `MaterializedCTEInfo`: the filters right above the scans of the CTE, as conjuncts and the scan's
/// table, and whether every scan has one.
struct Info {
    filters: Vec<(Vec<BoundExpr>, usize)>,
    all_filtered: bool,
}

/// `FindCandidates`.
fn find_candidates(op: &LogicalOperator, cte: usize, info: &mut Info) {
    match &op.kind {
        K::Filter { expressions } => {
            if let [child] = op.children.as_slice()
                && let K::CteRef {
                    table_index,
                    cte_index,
                    ..
                } = child.kind
            {
                if cte_index == cte {
                    info.filters.push((expressions.clone(), table_index));
                }
                return;
            }
        }
        K::CteRef { cte_index, .. } => {
            if *cte_index == cte {
                info.all_filtered = false;
            }
            return;
        }
        _ => {}
    }
    for child in &op.children {
        find_candidates(child, cte, info);
    }
}

fn find_cte(op: &mut LogicalOperator, cte: usize) -> Option<&mut LogicalOperator> {
    if matches!(op.kind, K::MaterializedCte { table_index, .. } if table_index == cte) {
        return Some(op);
    }
    op.children.iter_mut().find_map(|child| find_cte(child, cte))
}

/// `PushFilterIntoCTE`.
fn push_into_cte(plan: &mut LogicalOperator, cte: usize) {
    let Some(op) = find_cte(plan, cte) else {
        return;
    };
    let mut info = Info {
        filters: Vec::new(),
        all_filtered: true,
    };
    find_candidates(&op.children[1], cte, &mut info);
    if info.filters.is_empty() || !info.all_filtered || has_nested_reference(&op.children[1], cte) {
        return;
    }
    let definition_bindings = op.children[0].column_bindings();
    let mut disjuncts = Vec::new();
    for (conjuncts, ref_index) in &info.filters {
        let repeatable = conjuncts.iter().all(|e| !e.is_volatile() && !contains_barrier(e));
        let mut kept = Vec::new();
        for conjunct in conjuncts {
            // dropping a conjunct must not expose errors it short-circuited
            if conjunct.is_volatile() || contains_barrier(conjunct) || (!repeatable && can_throw(conjunct)) {
                continue;
            }
            let mut copy = conjunct.clone();
            let mut ok = true;
            copy.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                    && binding.table_index == *ref_index
                {
                    match definition_bindings.get(binding.column_index) {
                        Some(new) => *binding = *new,
                        None => ok = false,
                    }
                }
            });
            if !ok {
                return;
            }
            kept.push(copy);
        }
        // an unrestricted consumer makes the disjunction true
        if kept.is_empty() {
            return;
        }
        disjuncts.push(super::conjunction_of(kept));
    }
    let mut combined = disjuncts.remove(0);
    for disjunct in disjuncts {
        combined = BoundExpr::new(
            BoundExprKind::Conjunction {
                and: false,
                children: vec![combined, disjunct],
            },
            LogicalType::BOOLEAN,
        );
    }
    // `SplitPredicates`
    let mut expressions = Vec::new();
    super::rewriter::split_conjuncts(combined, &mut expressions);
    let mut filter = LogicalOperator::new(K::Filter { expressions }, Vec::new());
    super::rewriter::rewrite_plan(&mut filter);
    let definition = std::mem::replace(&mut op.children[0], LogicalOperator::new(K::CrossProduct, Vec::new()));
    filter.children.push(definition);
    op.children[0] = super::filter_pushdown::push_down_preserving_computed(filter);
}
