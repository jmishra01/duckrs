// Ported from duckdb/src/optimizer/materialized_aggregate_reuse.cpp @ rust-port-baseline
//
//! `AggregateReuseOptimizer::TryReuseMaterializedAggregate`: an aggregate grouped by a key, over
//! joins whose rows a SEMI join keeps to the keys of a materialized CTE, where that CTE joins the
//! same tables the same way and one table more on that key, reads the CTE instead. The extra table
//! can repeat rows, so only aggregates that repetition does not change qualify (`min`, `max`, ...).
//!
//! The filters of a table in C++ are its table filters. Here they can still be a filter right
//! above the scan, which counts as the scan's. `IsDomainCorrelationFilter` only knows a filter
//! comparing a column with itself: the others come from dependent joins planned as CTEs, which
//! pintail does not do.

use super::aggregate_reuse::{Origin, has_complete_grouping, reference_index, unary_aggregate_path};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use pintail_common::column_data::ColumnDataCollection;
use pintail_parser::ast::{ComparisonOp, JoinType};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

type Ctes = HashMap<usize, (LogicalOperator, Arc<Mutex<ColumnDataCollection>>)>;

/// `RelationEdge`.
#[derive(Clone, Copy)]
struct Edge {
    left: Origin,
    right: Origin,
}

/// A scan of a table and its filters, by column, as text that does not depend on the scan.
struct Source {
    table: usize,
    filters: Vec<(usize, String)>,
}

/// `RelationGraph`.
#[derive(Default)]
struct Graph {
    sources: Vec<Source>,
    edges: Vec<Edge>,
}

impl Graph {
    fn source(&self, table: usize) -> Option<&Source> {
        self.sources.iter().find(|s| s.table == table)
    }
}

/// The aggregates that give the same for rows repeated (`NOT_DISTINCT_DEPENDENT`), or DISTINCT ones.
fn repetition_invariant(aggregate: &BoundAggregate) -> bool {
    aggregate.distinct
        || matches!(
            aggregate.function.name.as_str(),
            "min" | "max" | "first" | "last" | "any_value" | "arbitrary" | "bool_and" | "bool_or"
        )
}

/// `TryReuseMaterializedAggregate`.
pub(super) fn try_reuse(op: &mut LogicalOperator, ctes: &Ctes, next: &mut usize) -> bool {
    let K::Aggregate { groups, aggregates, .. } = &op.kind else {
        return false;
    };
    let [input] = op.children.as_slice() else {
        return false;
    };
    if aggregates.is_empty() || !has_complete_grouping(op) || groups.iter().any(BoundExpr::is_volatile) {
        return false;
    }
    if aggregates
        .iter()
        .any(|a| a.children.iter().any(BoundExpr::is_volatile) || !repetition_invariant(a))
    {
        return false;
    }
    let Some(domain) = find_domain_semi(input, &mut Vec::new()) else {
        return false;
    };
    if domain.core_keys.len() != groups.len() {
        return false;
    }
    let Some((producer, data)) = ctes.get(&domain.cte_index) else {
        return false;
    };
    let mut core = Graph::default();
    let mut produced = Graph::default();
    if !collect_graph(input, Some(&domain.path), &mut Vec::new(), &mut core)
        || !collect_graph(producer, None, &mut Vec::new(), &mut produced)
        || core.sources.is_empty()
        || produced.sources.len() != core.sources.len() + 1
    {
        return false;
    }
    if core.sources.iter().any(|s| produced.source(s.table).is_none()) {
        return false;
    }
    let Some(extra) = produced
        .sources
        .iter()
        .find(|s| core.source(s.table).is_none())
        .map(|s| s.table)
    else {
        return false;
    };
    // the joins of the core are those of the producer, which joins the extra table on the keys
    let mut matched = vec![false; core.edges.len()];
    let mut cross = Vec::new();
    for edge in &produced.edges {
        let (left_core, right_core) = (
            core.source(edge.left.table).is_some(),
            core.source(edge.right.table).is_some(),
        );
        if left_core && right_core {
            let Some(at) = (0..core.edges.len()).find(|&i| !matched[i] && same_edge(edge, &core.edges[i])) else {
                return false;
            };
            matched[at] = true;
        } else if left_core != right_core {
            cross.push(*edge);
        } else {
            return false;
        }
    }
    if matched.contains(&false) || cross.len() != domain.core_keys.len() {
        return false;
    }
    let mut key_map = Vec::new();
    let mut cross_matched = vec![false; cross.len()];
    for (at, core_key) in domain.core_keys.iter().enumerate() {
        let (Some(domain_key), Some(group_key)) = (
            origin_at(producer, domain.cte_columns[at]),
            origin_of(input, &groups[at]),
        ) else {
            return false;
        };
        if group_key != *core_key || domain_key.table != extra {
            return false;
        }
        let Some(edge) = (0..cross.len()).find(|&i| {
            !cross_matched[i]
                && ((cross[i].left == *core_key && cross[i].right == domain_key)
                    || (cross[i].right == *core_key && cross[i].left == domain_key))
        }) else {
            return false;
        };
        cross_matched[edge] = true;
        key_map.push((*core_key, domain_key));
    }
    // `SourceFiltersMatch`: each table of the core has the filters it has in the producer
    for source in &core.sources {
        let other = produced.source(source.table).expect("checked above");
        let same = |a: &[(usize, String)], b: &[(usize, String)]| a.iter().all(|f| b.contains(f));
        if !same(&other.filters, &source.filters) {
            return false;
        }
        for filter in &source.filters {
            // `IsDomainCorrelationFilter`, for a key of the core
            let key = Origin {
                table: source.table,
                column: filter.0,
            };
            let correlation = filter.1 == "self" && domain.core_keys.contains(&key);
            if !other.filters.contains(filter) && !correlation {
                return false;
            }
        }
    }
    // the aggregate reads a scan of the CTE
    let types = producer.types();
    let table_index = *next;
    *next += 1;
    let reference = LogicalOperator::new(
        K::CteRef {
            table_index,
            cte_index: domain.cte_index,
            types: types.clone(),
            names: (0..types.len()).map(|i| format!("__aggregate_reuse_{i}")).collect(),
            data: data.clone(),
        },
        Vec::new(),
    );
    let rewrite = |expr: &BoundExpr| rewrite_to_cte(expr, input, producer, table_index, &types, &key_map);
    let Some(new_groups) = groups.iter().map(rewrite).collect::<Option<Vec<_>>>() else {
        return false;
    };
    let mut new_aggregates = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        let mut copy = aggregate.clone();
        let Some(children) = aggregate.children.iter().map(rewrite).collect::<Option<Vec<_>>>() else {
            return false;
        };
        copy.children = children;
        if let Some(filter) = &aggregate.filter {
            let Some(filter) = rewrite(filter) else {
                return false;
            };
            copy.filter = Some(filter);
        }
        if let Some(key) = &aggregate.distinct_key {
            let Some(key) = rewrite(key) else {
                return false;
            };
            copy.distinct_key = Some(key);
        }
        let Some(order_by) = aggregate
            .order_by
            .iter()
            .map(|(expr, modifiers)| rewrite(expr).map(|e| (e, *modifiers)))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        copy.order_by = order_by;
        new_aggregates.push(copy);
    }
    let K::Aggregate { groups, aggregates, .. } = &mut op.kind else {
        unreachable!()
    };
    *groups = new_groups;
    *aggregates = new_aggregates;
    op.children = vec![reference];
    true
}

fn same_edge(left: &Edge, right: &Edge) -> bool {
    (left.left == right.left && left.right == right.right) || (left.left == right.right && left.right == right.left)
}

/// `GetTableColumnOrigin` with filters: the column of a table that output `at` of `op` is.
fn origin_at(op: &LogicalOperator, at: usize) -> Option<Origin> {
    match &op.kind {
        K::Projection { expressions, .. } => {
            let [child] = op.children.as_slice() else {
                return None;
            };
            origin_at(child, reference_index(expressions.get(at)?, child)?)
        }
        K::Filter { .. } => origin_at(op.children.first()?, at),
        K::Join {
            join_type: JoinType::Inner | JoinType::Semi,
            ..
        } => {
            let binding = *op.column_bindings().get(at)?;
            op.children.iter().find_map(|child| {
                let position = child.column_bindings().iter().position(|b| *b == binding)?;
                origin_at(child, position)
            })
        }
        K::Get {
            table,
            column_ids,
            sample: None,
            ..
        } if op.children.is_empty() => {
            let binding = *op.column_bindings().get(at)?;
            Some(Origin {
                table: Arc::as_ptr(table) as usize,
                column: *column_ids.get(binding.column_index)?,
            })
        }
        _ => None,
    }
}

fn origin_of(op: &LogicalOperator, expr: &BoundExpr) -> Option<Origin> {
    origin_at(op, reference_index(expr, op)?)
}

/// A filter as text that does not depend on the scan it is on, and the column it reads. A filter
/// comparing a column with itself is `self`.
fn filter_key(expr: &BoundExpr, get: &LogicalOperator) -> Option<(usize, String)> {
    let K::Get { column_ids, .. } = &get.kind else {
        return None;
    };
    let bindings = get.column_bindings();
    let mut column = None;
    let mut normalized = expr.clone();
    let mut single = true;
    normalized.walk_mut(&mut |e| {
        e.alias.clear();
        if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind {
            let id = bindings
                .iter()
                .position(|b| b == binding)
                .and_then(|at| column_ids.get(bindings[at].column_index).copied());
            match (column, id) {
                (None, Some(id)) => column = Some(id),
                (Some(seen), Some(id)) if seen == id => {}
                _ => single = false,
            }
            *binding = ColumnBinding::new(0, 0);
        }
    });
    if !single {
        return None;
    }
    let column = column?;
    if let BoundExprKind::Comparison {
        op: ComparisonOp::NotDistinctFrom,
        left,
        right,
    } = &normalized.kind
        && format!("{left:?}") == format!("{right:?}")
    {
        return Some((column, "self".to_string()));
    }
    Some((column, format!("{normalized:?}")))
}

/// `CollectInnerGraph`: the scans of `op` and the equalities that join them, through projections,
/// filters on a scan and INNER joins. `domain` is the path to the SEMI join that keeps the keys,
/// whose core side is part of it.
fn collect_graph(op: &LogicalOperator, domain: Option<&Vec<usize>>, path: &mut Vec<usize>, graph: &mut Graph) -> bool {
    match &op.kind {
        K::Projection { expressions, .. } => {
            op.children.len() == 1
                && !expressions.iter().any(BoundExpr::is_volatile)
                && descend(op, 0, domain, path, graph)
        }
        K::Filter { expressions } => {
            let [child] = op.children.as_slice() else {
                return false;
            };
            if expressions.iter().any(BoundExpr::is_volatile) || !matches!(child.kind, K::Get { .. }) {
                return false;
            }
            let Some(filters) = expressions
                .iter()
                .map(|e| filter_key(e, child))
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            if !descend(op, 0, domain, path, graph) {
                return false;
            }
            graph
                .sources
                .last_mut()
                .expect("the scan below")
                .filters
                .extend(filters);
            true
        }
        K::Join {
            join_type,
            conditions,
            predicate,
            ..
        } => {
            if predicate.is_some() || op.children.len() != 2 {
                return false;
            }
            if domain == Some(path) {
                return descend(op, 0, domain, path, graph);
            }
            if *join_type != JoinType::Inner {
                return false;
            }
            for condition in conditions {
                if condition.comparison != ComparisonOp::Equal {
                    return false;
                }
                let (Some(left), Some(right)) = (
                    origin_of(&op.children[0], &condition.left),
                    origin_of(&op.children[1], &condition.right),
                ) else {
                    return false;
                };
                graph.edges.push(Edge { left, right });
            }
            descend(op, 0, domain, path, graph) && descend(op, 1, domain, path, graph)
        }
        K::Get {
            table,
            filters,
            sample: None,
            ..
        } if op.children.is_empty() => {
            let table = Arc::as_ptr(table) as usize;
            if graph.source(table).is_some() {
                return false;
            }
            let Some(filters) = filters.iter().map(|e| filter_key(e, op)).collect::<Option<Vec<_>>>() else {
                return false;
            };
            graph.sources.push(Source { table, filters });
            true
        }
        _ => false,
    }
}

fn descend(
    op: &LogicalOperator,
    child: usize,
    domain: Option<&Vec<usize>>,
    path: &mut Vec<usize>,
    graph: &mut Graph,
) -> bool {
    path.push(child);
    let result = collect_graph(&op.children[child], domain, path, graph);
    path.pop();
    result
}

/// `DomainSemiInfo`: where the SEMI join is, the CTE its other side reads, and for each condition
/// the key of the core side and the column of the CTE.
struct Domain {
    path: Vec<usize>,
    cte_index: usize,
    core_keys: Vec<Origin>,
    cte_columns: Vec<usize>,
}

/// `FindDomainSemi`.
fn find_domain_semi(op: &LogicalOperator, path: &mut Vec<usize>) -> Option<Domain> {
    match &op.kind {
        K::Projection { .. } | K::Filter { .. } => {
            if op.children.len() != 1 {
                return None;
            }
            path.push(0);
            let result = find_domain_semi(&op.children[0], path);
            path.pop();
            result
        }
        K::Join {
            join_type: JoinType::Inner,
            ..
        } => {
            let mut found = Vec::new();
            for at in 0..op.children.len() {
                path.push(at);
                found.extend(find_domain_semi(&op.children[at], path));
                path.pop();
            }
            (found.len() == 1).then(|| found.pop().expect("one"))
        }
        K::Join {
            join_type: JoinType::Semi,
            conditions,
            predicate: None,
            ..
        } if !conditions.is_empty() && op.children.len() == 2 => {
            let domain_side = &op.children[1];
            // the keys may be grouped first
            let domain_input = match unary_aggregate_path(domain_side) {
                Some(depth) => {
                    let mut aggregate = domain_side;
                    for _ in 0..depth {
                        aggregate = &aggregate.children[0];
                    }
                    let K::Aggregate {
                        groups,
                        aggregates,
                        grouping_sets,
                        grouping_functions,
                        ..
                    } = &aggregate.kind
                    else {
                        unreachable!()
                    };
                    if !aggregates.is_empty()
                        || groups.len() != conditions.len()
                        || aggregate.children.len() != 1
                        || grouping_sets.len() > 1
                        || !grouping_functions.is_empty()
                    {
                        return None;
                    }
                    &aggregate.children[0]
                }
                None => domain_side,
            };
            let cte_index = domain_cte(domain_input)?;
            let mut core_keys = Vec::new();
            let mut cte_columns = Vec::new();
            for condition in conditions {
                if condition.comparison != ComparisonOp::Equal {
                    return None;
                }
                core_keys.push(origin_of(&op.children[0], &condition.left)?);
                let at = reference_index(&condition.right, domain_side)?;
                cte_columns.push(trace_domain_output(domain_side, at, cte_index)?);
            }
            Some(Domain {
                path: path.clone(),
                cte_index,
                core_keys,
                cte_columns,
            })
        }
        _ => None,
    }
}

/// `GetDomainCTERef`: the CTE below projections.
fn domain_cte(op: &LogicalOperator) -> Option<usize> {
    let mut current = op;
    while let K::Projection { expressions, .. } = &current.kind {
        if current.children.len() != 1 || expressions.iter().any(BoundExpr::is_volatile) {
            return None;
        }
        current = &current.children[0];
    }
    match current.kind {
        K::CteRef { cte_index, .. } => Some(cte_index),
        _ => None,
    }
}

/// `TraceDomainOutput`: the column of the CTE that output `at` of `op` is.
fn trace_domain_output(op: &LogicalOperator, at: usize, cte_index: usize) -> Option<usize> {
    match &op.kind {
        K::CteRef { cte_index: index, .. } => (*index == cte_index).then_some(at),
        K::Projection { expressions, .. } => {
            let [child] = op.children.as_slice() else {
                return None;
            };
            trace_domain_output(child, reference_index(expressions.get(at)?, child)?, cte_index)
        }
        K::Aggregate { groups, .. } => {
            let [child] = op.children.as_slice() else {
                return None;
            };
            trace_domain_output(child, reference_index(groups.get(at)?, child)?, cte_index)
        }
        _ => None,
    }
}

/// `FindProducerOutput`.
fn producer_output(producer: &LogicalOperator, origin: Origin) -> Option<usize> {
    (0..producer.column_bindings().len()).find(|&at| origin_at(producer, at) == Some(origin))
}

/// `RewriteToCTE`: `expr` over `scope`, reading the scan `table_index` of the CTE instead.
fn rewrite_to_cte(
    expr: &BoundExpr,
    scope: &LogicalOperator,
    producer: &LogicalOperator,
    table_index: usize,
    types: &[pintail_common::types::LogicalType],
    key_map: &[(Origin, Origin)],
) -> Option<BoundExpr> {
    if let BoundExprKind::ColumnRef { depth: 0, .. } = &expr.kind {
        let at = reference_index(expr, scope)?;
        if let K::Projection { expressions, .. } = &scope.kind {
            let [child] = scope.children.as_slice() else {
                return None;
            };
            return rewrite_to_cte(expressions.get(at)?, child, producer, table_index, types, key_map);
        }
        let mut origin = origin_at(scope, at)?;
        if let Some((_, domain)) = key_map.iter().find(|(core, _)| *core == origin) {
            origin = *domain;
        }
        let column = producer_output(producer, origin)?;
        return Some(BoundExpr::column_ref(
            ColumnBinding::new(table_index, column),
            types.get(column)?.clone(),
            expr.alias.clone(),
        ));
    }
    if matches!(
        expr.kind,
        BoundExprKind::ColumnRef { .. } | BoundExprKind::Subquery { .. }
    ) {
        return None;
    }
    let mut result = expr.clone();
    let mut ok = true;
    for child in result.children_mut() {
        match rewrite_to_cte(child, scope, producer, table_index, types, key_map) {
            Some(rewritten) => *child = rewritten,
            None => ok = false,
        }
    }
    ok.then_some(result)
}
