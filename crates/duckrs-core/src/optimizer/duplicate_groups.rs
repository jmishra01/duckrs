//! `RemoveDuplicateGroups`: a group that is the same column as an earlier one goes, and so does one computed
//! of a single column that is a group itself (`x - 1` next to `x`): a projection above the aggregate computes
//! it again, of that group.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use std::collections::{BTreeSet, HashMap};

/// What to do with a group: drop it for the group at `target`, and if it is computed, compute it of that group.
struct Removal {
    removed: usize,
    target: usize,
    derived: bool,
}

/// The rewrites of a plan: what each binding becomes above an aggregate, and the projection above it, if there is
/// one, whose expressions read the aggregate as it now is.
type Remap = (HashMap<ColumnBinding, ColumnBinding>, Option<usize>);

/// `RemoveDuplicateGroups::VisitOperator`.
pub fn remove(mut plan: LogicalOperator) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    let mut remaps = Vec::new();
    plan = rewrite(plan, &mut next, &mut remaps);
    for (map, stop) in &remaps {
        replace_bindings(&mut plan, *stop, map);
    }
    plan
}

fn rewrite(mut op: LogicalOperator, next: &mut usize, remaps: &mut Vec<Remap>) -> LogicalOperator {
    op.children = std::mem::take(&mut op.children)
        .into_iter()
        .map(|child| rewrite(child, next, remaps))
        .collect();
    if matches!(op.kind, K::Aggregate { .. }) {
        return aggregate(op, next, remaps);
    }
    op
}

/// `BaseTypeAllowsCollapse`: of floats and intervals, values that are equal can still differ (`0e0` and `-0e0`
/// by `signbit`, `1 day` and `24 hours` by `datepart`).
fn allows_collapse(ty: &LogicalType) -> bool {
    match ty.id() {
        LogicalTypeId::Float | LogicalTypeId::Double | LogicalTypeId::Interval => false,
        _ => {
            ty.struct_children().iter().all(|(_, child)| allows_collapse(child))
                && ty.child_type().is_none_or(allows_collapse)
        }
    }
}

/// `CollectColumnRefBindings`: the single column `expr` reads, if it reads one.
fn single_column(expr: &BoundExpr) -> Option<ColumnBinding> {
    let mut columns = BTreeSet::new();
    let mut other = false;
    expr.clone().walk_mut(&mut |e| match &e.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => {
            columns.insert((binding.table_index, binding.column_index));
        }
        BoundExprKind::ColumnRef { .. } | BoundExprKind::Subquery { .. } => other = true,
        _ => {}
    });
    let (table_index, column_index) = (columns.len() == 1 && !other).then(|| *columns.iter().next().expect("one"))?;
    Some(ColumnBinding::new(table_index, column_index))
}

/// `RemoveDuplicateGroups::VisitAggregate`.
fn aggregate(mut op: LogicalOperator, next: &mut usize, remaps: &mut Vec<Remap>) -> LogicalOperator {
    let K::Aggregate {
        group_index,
        aggregate_index,
        groups,
        aggregates,
        grouping_sets,
        grouping_functions,
        ..
    } = &mut op.kind
    else {
        unreachable!()
    };
    // with several grouping sets the place of a group matters, even of one that is another's column
    if !grouping_functions.is_empty() || grouping_sets.len() > 1 {
        return op;
    }
    let (group_index, aggregate_index) = (*group_index, *aggregate_index);
    // `CollectGroupRemovals`
    let mut removals = Vec::new();
    let mut first: HashMap<ColumnBinding, usize> = HashMap::new();
    for (at, group) in groups.iter().enumerate() {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &group.kind {
            match first.get(binding) {
                Some(&target) => removals.push(Removal {
                    removed: at,
                    target,
                    derived: false,
                }),
                None => {
                    first.insert(*binding, at);
                }
            }
        }
    }
    for (at, group) in groups.iter().enumerate() {
        if matches!(group.kind, BoundExprKind::ColumnRef { .. }) || group.is_foldable() || group.is_volatile() {
            continue;
        }
        let Some(&target) = single_column(group).and_then(|binding| first.get(&binding)) else {
            continue;
        };
        if !allows_collapse(&groups[target].return_type) {
            continue;
        }
        removals.push(Removal {
            removed: at,
            target,
            derived: true,
        });
    }
    if removals.is_empty() {
        return op;
    }
    // `ApplyGroupRemovals`
    let original: Vec<BoundExpr> = groups.clone();
    let removed: BTreeSet<usize> = removals.iter().map(|r| r.removed).collect();
    let mut to_post = vec![0; original.len()];
    let mut kept = Vec::new();
    for (at, group) in original.iter().enumerate() {
        if !removed.contains(&at) {
            to_post[at] = kept.len();
            kept.push(group.clone());
        }
    }
    for removal in &removals {
        to_post[removal.removed] = to_post[removal.target];
    }
    *groups = kept;
    for set in grouping_sets.iter_mut() {
        *set = set.iter().map(|&at| to_post[at]).collect();
    }
    let aggregate_types: Vec<LogicalType> = aggregates.iter().map(|a| a.return_type.clone()).collect();
    let group_map: HashMap<ColumnBinding, ColumnBinding> = (0..original.len())
        .map(|at| (ColumnBinding::new(group_index, at), ColumnBinding::new(group_index, to_post[at])))
        .collect();
    if !removals.iter().any(|r| r.derived) {
        remaps.push((group_map, None));
        return op;
    }
    // `BuildDerivedProjection`: the aggregate's columns as they were, the computed groups of their base group
    let projection = *next;
    *next += 1;
    let mut expressions = Vec::with_capacity(original.len() + aggregate_types.len());
    let mut remap = HashMap::new();
    for (at, group) in original.iter().enumerate() {
        let post = ColumnBinding::new(group_index, to_post[at]);
        expressions.push(BoundExpr::column_ref(post, group.return_type.clone(), group.alias.clone()));
        remap.insert(ColumnBinding::new(group_index, at), ColumnBinding::new(projection, at));
    }
    for (k, ty) in aggregate_types.iter().enumerate() {
        expressions.push(BoundExpr::column_ref(ColumnBinding::new(aggregate_index, k), ty.clone(), String::new()));
        remap.insert(
            ColumnBinding::new(aggregate_index, k),
            ColumnBinding::new(projection, original.len() + k),
        );
    }
    for removal in removals.iter().filter(|r| r.derived) {
        let BoundExprKind::ColumnRef { binding: source, .. } = original[removal.target].kind else {
            unreachable!("the base of a computed group is a column")
        };
        let base = ColumnBinding::new(group_index, to_post[removal.target]);
        let mut derived = original[removal.removed].clone();
        derived.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                && *binding == source
            {
                *binding = base;
            }
        });
        expressions[removal.removed] = derived;
    }
    remaps.push((remap, Some(projection)));
    LogicalOperator::new(
        K::Projection {
            table_index: projection,
            expressions,
        },
        vec![op],
    )
}

/// `ColumnBindingReplacer`: `map`'s bindings in `op` and below, but in the projection `stop` and below it.
fn replace_bindings(op: &mut LogicalOperator, stop: Option<usize>, map: &HashMap<ColumnBinding, ColumnBinding>) {
    if let K::Projection { table_index, .. } = &op.kind
        && Some(*table_index) == stop
    {
        return;
    }
    for child in &mut op.children {
        replace_bindings(child, stop, map);
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}
