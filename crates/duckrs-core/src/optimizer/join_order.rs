// Ported from duckdb/src/optimizer/join_order/{join_order_optimizer,relation_manager,
// query_graph_manager,query_graph,plan_enumerator,cardinality_estimator,cost_model}.cpp and
// duckdb/src/optimizer/build_probe_side_optimizer.cpp @ rust-port-baseline
//
//! `JoinOrderOptimizer`: picks the order of a tree of inner joins and cross products. The operands
//! of such a tree are its relations, the conditions and filters on it the edges of a hypergraph.
//! DPhyp ("Dynamic Programming Strikes Back") finds the cheapest order, where the cost of a join
//! is the estimated size of its result on top of that of its inputs. With many relations, or
//! when that takes too many steps, Greedy Operator Ordering takes over.
//!
//! A SEMI or ANTI join on a tree goes down to the one relation it reads, and stays on top of the
//! tree's other joins otherwise. LEFT joins are relations, with their sides optimized apart, where
//! DuckDB also moves them with conflict detection. The cardinality estimator is reduced to total
//! domains per equivalence class, the cap for composite keys and DuckDB's selectivities.

use super::conjunction_of;
use super::filter_pushdown::condition_expression;
use super::rewriter::split_conjuncts;
use super::relation_statistics::{
    equivalence_binding, ColumnStats, CountSource, DistinctCount, RelationStats, SemiAntiCondition, apply_direct_filter_domain,
    estimate_distinct_cardinality, semi_anti_join_stats,
};
use super::statistics::{table_distinct_counts, table_statistics};
use crate::catalog::{ColumnStatistics, TableCatalogEntry};
use duckrs_common::types::LogicalType;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

thread_local! {
    /// `approximate_join_order_threshold`: from here on the order is picked greedily.
    static APPROXIMATE_THRESHOLD: std::cell::Cell<usize> = const { std::cell::Cell::new(12) };
    /// `debug_force_no_cross_product`
    static FORCE_NO_CROSS_PRODUCT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// what the join order refused, for the client to raise
    static ERROR: std::cell::RefCell<Option<duckrs_common::Error>> = const { std::cell::RefCell::new(None) };
}

/// The settings of the join order for the plans optimized from here on.
pub fn configure(approximate_threshold: usize, force_no_cross_product: bool) {
    APPROXIMATE_THRESHOLD.with(|t| t.set(approximate_threshold));
    FORCE_NO_CROSS_PRODUCT.with(|f| f.set(force_no_cross_product));
}

/// The error the last join order ran into (`PlanEnumerator::SolveJoinOrder` throws).
pub fn take_error() -> duckrs_common::Result<()> {
    match ERROR.with(|e| e.borrow_mut().take()) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
/// Past this many pairs DPhyp gives up.
const MAX_PAIRS: usize = 10_000;
/// A set of relations is a mask, so there can be this many.
const MAX_RELATIONS: usize = 63;
use super::relation_statistics::DEFAULT_SELECTIVITY;
/// `CompositeJoinPairStats::MAX_CARDINALITY_TO_DISTINCT_RATIO`: with more rows than this per value
/// of a key column, the rows say little about how many keys there are.
const MAX_CARDINALITY_TO_DISTINCT_RATIO: f64 = 8.0;
/// What a recursive CTE round is assumed to hold.
const UNKNOWN_CARDINALITY: f64 = 1000.0;

fn is_binary_tree(op: &LogicalOperator) -> bool {
    match &op.kind {
        K::CrossProduct => true,
        K::Join {
            join_type: JoinType::Inner,
            predicate,
            ..
        } => !predicate.as_ref().is_some_and(BoundExpr::is_volatile),
        _ => false,
    }
}

/// What a SEMI or ANTI join reads of its left side.
fn left_columns(op: &LogicalOperator) -> Vec<ColumnBinding> {
    let K::Join {
        conditions, predicate, ..
    } = &op.kind
    else {
        return Vec::new();
    };
    let mut columns = Vec::new();
    for condition in conditions {
        expression_columns(&condition.left, &mut columns);
    }
    if let Some(predicate) = predicate {
        expression_columns(predicate, &mut columns);
    }
    let right = op.children[1].column_bindings();
    columns.retain(|column| !right.contains(column));
    columns
}

/// Whether `op` is a SEMI or ANTI join that can go down to one relation of the tree on its left.
/// Below the joins of the tree it removes the same rows, as it reads only that relation.
fn is_movable_semi(op: &LogicalOperator) -> bool {
    let K::Join {
        join_type: JoinType::Semi | JoinType::Anti,
        predicate,
        ..
    } = &op.kind
    else {
        return false;
    };
    if predicate.as_ref().is_some_and(BoundExpr::is_volatile) || !is_tree(&op.children[0]) {
        return false;
    }
    let columns = left_columns(op);
    let mut relations = Vec::new();
    leaves(&op.children[0], &mut relations);
    !columns.is_empty()
        && relations.iter().any(|relation| {
            let bindings = relation.column_bindings();
            columns.iter().all(|column| bindings.contains(column))
        })
}

/// How many SEMI and ANTI joins, RIGHT ones too, with equalities and without volatile predicates,
/// are stacked at `op` on what they keep the rows of.
fn semi_chain_length(op: &LogicalOperator) -> usize {
    let K::Join {
        join_type,
        conditions,
        predicate,
        ..
    } = &op.kind
    else {
        return 0;
    };
    let kept = match join_type {
        JoinType::Semi | JoinType::Anti => 0,
        JoinType::RightSemi | JoinType::RightAnti => 1,
        _ => return 0,
    };
    if conditions.is_empty() || predicate.as_ref().is_some_and(BoundExpr::is_volatile) {
        return 0;
    }
    1 + semi_chain_length(&op.children[kept])
}

/// A RIGHT SEMI or RIGHT ANTI join as the SEMI or ANTI join it was.
fn unflip_semi(op: &mut LogicalOperator) {
    let K::Join {
        join_type, conditions, ..
    } = &mut op.kind
    else {
        return;
    };
    *join_type = match join_type {
        JoinType::RightSemi => JoinType::Semi,
        JoinType::RightAnti => JoinType::Anti,
        _ => return,
    };
    for condition in conditions {
        std::mem::swap(&mut condition.left, &mut condition.right);
    }
    op.children.swap(0, 1);
}

/// Whether `op` is an operator of a tree of joins, or filters on such a tree.
fn is_tree(op: &LogicalOperator) -> bool {
    match &op.kind {
        // a barrier must not end up below one of the joins (`must_remain_at_root`)
        K::Filter { expressions } => {
            !expressions
                .iter()
                .any(|e| e.is_volatile() || super::rewriter::contains_barrier(e))
                && is_tree(&op.children[0])
        }
        _ => is_binary_tree(op) || is_movable_semi(op),
    }
}

/// The relations of a tree, whose operators `is_tree` accepts.
fn leaves<'a>(op: &'a LogicalOperator, result: &mut Vec<&'a LogicalOperator>) {
    if is_movable_semi(op) {
        leaves(&op.children[0], result);
    } else if is_tree(op) {
        for child in &op.children {
            leaves(child, result);
        }
    } else {
        result.push(op);
    }
}

/// A conjunct on a tree of joins (`FilterInfo`), with the relations it reads.
struct Predicate {
    expr: BoundExpr,
    set: u64,
    /// the relations of the two sides of a comparison, if each reads some
    sides: Option<(u64, u64)>,
    /// for an equality of two columns: which set of columns that are all equal they belong to
    class: Option<usize>,
}

/// Columns equal to the same column are equal to each other: with `a.x = b.y` and `a.x = c.z`,
/// `b.y = c.z` holds too, and `b` and `c` can be joined first on it. Adds the equalities that are
/// not there, and gives all of those of a set its number, as a join needs only one of them: each
/// side of a join has had the equalities between its own relations, so one across makes the rest.
fn add_implied_equalities(predicates: &mut Vec<Predicate>, relation_of: &HashMap<ColumnBinding, usize>) {
    let plain = |expr: &BoundExpr| match expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } if relation_of.contains_key(&binding) => Some(binding),
        _ => None,
    };
    // the sets, each a list of columns with an expression for them
    let mut classes: Vec<Vec<(ColumnBinding, BoundExpr)>> = Vec::new();
    let mut members: Vec<(usize, ColumnBinding, ColumnBinding)> = Vec::new();
    for (at, predicate) in predicates.iter().enumerate() {
        let BoundExprKind::Comparison {
            op: ComparisonOp::Equal,
            left,
            right,
        } = &predicate.expr.kind
        else {
            continue;
        };
        let (Some(l), Some(r)) = (plain(left), plain(right)) else {
            continue;
        };
        if left.return_type != right.return_type || relation_of[&l] == relation_of[&r] {
            continue;
        }
        let class_of = |classes: &[Vec<(ColumnBinding, BoundExpr)>], column| {
            classes.iter().position(|class| class.iter().any(|(c, _)| *c == column))
        };
        match (class_of(&classes, l), class_of(&classes, r)) {
            (Some(a), Some(b)) if a != b => {
                let moved = std::mem::take(&mut classes[b]);
                classes[a].extend(moved);
            }
            (Some(_), Some(_)) => {}
            (Some(a), None) => classes[a].push((r, (**right).clone())),
            (None, Some(b)) => classes[b].push((l, (**left).clone())),
            (None, None) => classes.push(vec![(l, (**left).clone()), (r, (**right).clone())]),
        }
        members.push((at, l, r));
    }
    let class_of = |column: ColumnBinding| {
        classes
            .iter()
            .position(|class| class.iter().any(|(c, _)| *c == column))
            .expect("each column of an equality is in a set")
    };
    // with two columns of one relation in a set, an equality across a join does not make the others
    // of the set so: nothing has made those two equal. Such a set is left as it was written
    let distinct_relations = |class: &[(ColumnBinding, BoundExpr)]| {
        let mut relations: Vec<usize> = class.iter().map(|(column, _)| relation_of[column]).collect();
        relations.sort_unstable();
        relations.windows(2).all(|pair| pair[0] != pair[1])
    };
    let mut present: Vec<(ColumnBinding, ColumnBinding)> = Vec::new();
    for (at, l, r) in members {
        let class = class_of(l);
        if distinct_relations(&classes[class]) {
            predicates[at].class = Some(class);
        }
        present.push((l, r));
    }
    for (number, class) in classes.iter().enumerate() {
        if !distinct_relations(class) {
            continue;
        }
        for (i, (first, first_expr)) in class.iter().enumerate() {
            for (second, second_expr) in &class[i + 1..] {
                let (a, b) = (relation_of[first], relation_of[second]);
                let there = present.contains(&(*first, *second)) || present.contains(&(*second, *first));
                if a == b || there || first_expr.return_type != second_expr.return_type {
                    continue;
                }
                let expr = BoundExpr::new(
                    BoundExprKind::Comparison {
                        op: ComparisonOp::Equal,
                        left: Box::new(first_expr.clone()),
                        right: Box::new(second_expr.clone()),
                    },
                    duckrs_common::types::LogicalType::BOOLEAN,
                );
                predicates.push(Predicate {
                    expr,
                    set: 1 << a | 1 << b,
                    sides: Some((1 << a, 1 << b)),
                    class: Some(number),
                });
            }
        }
    }
}

/// The columns an expression reads.
fn expression_columns(expr: &BoundExpr, columns: &mut Vec<ColumnBinding>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        columns.push(*binding);
    }
    for child in expr.children() {
        expression_columns(child, columns);
    }
}

/// `QueryGraphManager::GetEquivalenceBinding`: the column an expression is, maybe after a cast.
fn bare_column(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        BoundExprKind::Cast { child, .. } => bare_column(child),
        _ => None,
    }
}

/// An ON-clause equality between two columns (`JoinEqualityGroups::AddEquality`).
fn residual_equality(conjunct: &BoundExpr) -> Option<(ColumnBinding, ColumnBinding)> {
    match &conjunct.kind {
        BoundExprKind::Comparison {
            op: ComparisonOp::Equal | ComparisonOp::NotDistinctFrom,
            left,
            right,
        } => bare_column(left).zip(bare_column(right)),
        _ => None,
    }
}

/// `ApplyJoinPairCap`: what a join on several keys divides by, for that of its keys on their own.
/// A composite key has at most as many values as the smaller side has rows, where it is unique.
fn composite_denominator(domains: &[f64], left_rows: f64, right_rows: f64) -> Option<f64> {
    let cap = left_rows.min(right_rows);
    let largest = domains.iter().copied().fold(0.0, f64::max);
    let product: f64 = domains.iter().product();
    let applies = domains.len() >= 2 && product > cap && cap >= largest;
    (applies && cap > 0.0 && cap <= largest * MAX_CARDINALITY_TO_DISTINCT_RATIO).then_some(cap)
}

/// A predicate of the tree as `CardinalityEstimator` sees it: an edge between the relations its
/// sides read (`FilterInfoWithTotalDomains`).
#[derive(Debug, Clone)]
struct Edge {
    /// the relations it reads
    set: u64,
    /// the relations of its two sides
    left: u64,
    right: u64,
    /// the equivalence class of an equality of two columns
    class: Option<usize>,
    /// the comparison it makes, `None` for something else
    op: Option<ComparisonOp>,
    /// for an equality of a column of each of two relations: those two (`GetEqualityJoinPair`)
    pair: Option<u64>,
}

impl Edge {
    fn is_equality(&self) -> bool {
        self.class.is_some()
    }
}

/// `RelationsSetToStats`: a domain and the edges over it.
#[derive(Debug)]
struct DomainGroup {
    /// `GetDistinctCount`
    distinct: f64,
    /// whether that count comes from statistics, not from row counts
    reliable: bool,
    edges: Vec<Edge>,
}

impl DomainGroup {
    /// `IsSelectivityOnlyGroup`: only comparisons other than equalities.
    fn selectivity_only(&self) -> bool {
        !self.edges.is_empty() && self.edges.iter().all(|e| !e.is_equality() && e.op.is_some_and(|op| !matches!(op, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom)))
    }
}

/// `CompositeJoinPairStats`.
#[derive(Debug, Default, Clone, Copy)]
struct PairStats {
    first_distinct: f64,
    max_distinct: f64,
    has_distinct: bool,
}

impl PairStats {
    fn register(&mut self, distinct: f64) {
        if !self.has_distinct {
            self.first_distinct = distinct;
            self.has_distinct = true;
        }
        self.max_distinct = self.max_distinct.max(distinct);
    }

    fn can_apply_cap(&self, cap: f64) -> bool {
        self.has_distinct && self.max_distinct > 0.0 && cap <= self.max_distinct * MAX_CARDINALITY_TO_DISTINCT_RATIO
    }
}

#[derive(PartialEq)]
enum EdgeKind {
    InnerEquivalence,
    NonEquality,
    RedundantTransitive,
    Other,
}

/// `SelectivityState`.
#[derive(Default)]
struct SelectivityState {
    subgraphs: Vec<(u64, f64)>,
    applied_classes: HashSet<usize>,
    pair_stats: Vec<(u64, PairStats)>,
    capped: HashSet<u64>,
    unused_domains: HashSet<u64>,
}

/// `CardinalityEstimator`, for one tree: the product of the relations' rows times the selectivity
/// of the predicates among them.
struct Estimator {
    cardinalities: Vec<f64>,
    /// sorted as `SortTdoms` sorts them
    groups: Vec<DomainGroup>,
    /// the relations of the predicates that are not comparisons of two sides
    residuals: Vec<u64>,
    /// pairs of relations with equalities in several classes (`HasDirectCompositeEquality`)
    composites: HashSet<u64>,
    estimates: HashMap<u64, f64>,
}

impl Estimator {
    fn new(relations: &[RelationStats], predicates: &[Predicate], relation_of: &HashMap<ColumnBinding, usize>) -> Self {
        // a column's distinct values, and whether statistics say so rather than the row count
        let distinct_of = |binding: &ColumnBinding| -> Option<DistinctCount> {
            relation_of
                .get(binding)
                .map(|&relation| relations[relation].column(binding).total)
        };
        // `SelectTotalDomain`, and whether it is `HasReliableDistinctCount`
        let select = |domains: &[DistinctCount]| -> (f64, bool) {
            match super::relation_statistics::select_total_domain(domains) {
                Some(domain) => (domain.count.max(1.0), domain.is_reliable()),
                None => (f64::MAX, false),
            }
        };
        let domain_of_expr = |expr: &BoundExpr| -> Vec<DistinctCount> {
            let mut columns = Vec::new();
            expression_columns(expr, &mut columns);
            columns.iter().filter_map(distinct_of).collect()
        };
        let relation_bit = |binding: &ColumnBinding| relation_of.get(binding).map(|&r| 1u64 << r);
        // equivalence classes of columns
        let mut parents: HashMap<ColumnBinding, ColumnBinding> = HashMap::new();
        fn find(parents: &mut HashMap<ColumnBinding, ColumnBinding>, binding: ColumnBinding) -> ColumnBinding {
            let parent = *parents.entry(binding).or_insert(binding);
            if parent == binding {
                return binding;
            }
            let root = find(parents, parent);
            parents.insert(binding, root);
            root
        }
        let equality_columns = |predicate: &Predicate| -> Option<(ColumnBinding, ColumnBinding)> {
            if let BoundExprKind::Comparison { op, left, right } = &predicate.expr.kind
                && matches!(op, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom)
                && let (Some(l), Some(r)) = (bare_column(left), bare_column(right))
                && relation_of.contains_key(&l)
                && relation_of.contains_key(&r)
                && relation_of[&l] != relation_of[&r]
            {
                return Some((l, r));
            }
            None
        };
        for predicate in predicates {
            if let Some((l, r)) = equality_columns(predicate) {
                let (l, r) = (find(&mut parents, l), find(&mut parents, r));
                parents.insert(l, r);
            }
        }
        let mut roots: Vec<ColumnBinding> = parents
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|b| find(&mut parents, b))
            .collect();
        roots.sort_by_key(|b| (b.table_index, b.column_index));
        roots.dedup();
        let class_of_root: HashMap<ColumnBinding, usize> = roots.iter().enumerate().map(|(i, r)| (*r, i)).collect();
        let mut class_groups: Vec<DomainGroup> = roots
            .iter()
            .map(|root| {
                let columns: Vec<ColumnBinding> =
                    parents.keys().copied().filter(|b| find(&mut parents.clone(), *b) == *root).collect();
                let domains: Vec<DistinctCount> = columns.iter().filter_map(distinct_of).collect();
                let (mut distinct, reliable) = select(&domains);
                // `RestrictFilterDomains`: with every column of the class filtered to a finite set of
                // values, and its domain known from statistics, the class has at most the most of those
                let mut retained: Option<f64> = Some(0.0);
                for column in &columns {
                    let stats = relations[relation_of[column]].column(column);
                    retained = match (retained, stats.evidence.filter_bound()) {
                        (Some(r), Some(bound)) if stats.total.source != CountSource::Cardinality => Some(r.max(bound)),
                        _ => None,
                    };
                }
                if let Some(retained) = retained
                    && !columns.is_empty()
                {
                    distinct = distinct.min(retained.max(1.0));
                }
                DomainGroup {
                    distinct,
                    reliable,
                    edges: Vec::new(),
                }
            })
            .collect();
        let mut other_groups = Vec::new();
        let mut residuals = Vec::new();
        let mut pair_classes: HashMap<u64, HashSet<usize>> = HashMap::new();
        for predicate in predicates {
            if predicate.set.count_ones() < 2 {
                continue;
            }
            if let Some((l, r)) = equality_columns(predicate) {
                let class = class_of_root[&find(&mut parents, l)];
                let (left, right) = (relation_bit(&l).expect("known"), relation_bit(&r).expect("known"));
                let op = match &predicate.expr.kind {
                    BoundExprKind::Comparison { op, .. } => *op,
                    _ => unreachable!("an equality"),
                };
                pair_classes.entry(left | right).or_default().insert(class);
                class_groups[class].edges.push(Edge {
                    set: predicate.set,
                    left,
                    right,
                    class: Some(class),
                    op: Some(op),
                    pair: Some(left | right),
                });
                continue;
            }
            // a comparison of two sides is an edge over its own domain; anything else is residual
            match (&predicate.expr.kind, predicate.sides) {
                (BoundExprKind::Comparison { op, left, right }, Some((left_set, right_set))) => {
                    let mut domains = domain_of_expr(left);
                    domains.extend(domain_of_expr(right));
                    let (distinct, reliable) = select(&domains);
                    other_groups.push(DomainGroup {
                        distinct,
                        reliable,
                        edges: vec![Edge {
                            set: predicate.set,
                            left: left_set,
                            right: right_set,
                            class: None,
                            op: Some(*op),
                            pair: None,
                        }],
                    });
                }
                _ => residuals.push(predicate.set),
            }
        }
        let mut groups: Vec<DomainGroup> = class_groups.into_iter().filter(|g| !g.edges.is_empty()).collect();
        groups.extend(other_groups);
        // `SortTdoms`: comparisons last, then by their domains, largest first
        groups.sort_by(|a, b| {
            a.selectivity_only()
                .cmp(&b.selectivity_only())
                .then(b.distinct.partial_cmp(&a.distinct).unwrap_or(std::cmp::Ordering::Equal))
        });
        let composites = pair_classes.into_iter().filter(|(_, c)| c.len() >= 2).map(|(pair, _)| pair).collect();
        Self {
            cardinalities: relations.iter().map(|r| r.cardinality).collect(),
            groups,
            residuals,
            composites,
            estimates: HashMap::new(),
        }
    }

    /// `GetNumerator`: an empty relation counts as one row here.
    fn numerator(&self, set: u64) -> f64 {
        (0..self.cardinalities.len())
            .filter(|r| set & (1 << r) != 0)
            .map(|r| if self.cardinalities[r] == 0.0 { 1.0 } else { self.cardinalities[r] })
            .product()
    }

    /// `EstimateCardinality` of a set of relations joined by inner joins.
    fn estimate(&mut self, set: u64) -> f64 {
        if let Some(&estimate) = self.estimates.get(&set) {
            return estimate;
        }
        let empty = (0..self.cardinalities.len()).any(|r| set & (1 << r) != 0 && self.cardinalities[r] == 0.0);
        let mut estimate = if empty { 0.0 } else { self.numerator(set) * self.inner_selectivity(set) };
        // the product of many large relations leaves the range of a double before the selectivity
        // divides it down again: work in units of the largest relation, as the equalities scale with it
        if !empty && !(estimate.is_finite() && estimate > 0.0) {
            let scale = self.cardinalities.iter().copied().fold(1.0, f64::max);
            let scaled = Estimator {
                cardinalities: self.cardinalities.iter().map(|c| c / scale).collect(),
                groups: self
                    .groups
                    .iter()
                    .map(|g| DomainGroup {
                        distinct: g.distinct / scale,
                        reliable: g.reliable,
                        edges: g.edges.clone(),
                    })
                    .collect(),
                residuals: self.residuals.clone(),
                composites: self.composites.clone(),
                estimates: HashMap::new(),
            };
            let rescaled = scaled.numerator(set) * scaled.inner_selectivity(set) * scale;
            if rescaled.is_finite() {
                estimate = rescaled;
            }
        }
        self.estimates.insert(set, estimate);
        estimate
    }

    /// `GetInnerSelectivity`: that of the edges, and the default one of each residual predicate.
    fn inner_selectivity(&self, set: u64) -> f64 {
        let residuals = self.residuals.iter().filter(|r| *r & !set == 0).count();
        self.selectivity(set) * DEFAULT_SELECTIVITY.powi(residuals as i32)
    }

    /// `GetSelectivity`.
    fn selectivity(&self, set: u64) -> f64 {
        let mut state = SelectivityState::default();
        for group in &self.groups {
            for edge in &group.edges {
                if edge.set & !set != 0 || edge.left == edge.right {
                    continue;
                }
                self.process_edge(edge, group, set, &mut state);
            }
        }
        // `MergeDisconnectedSelectivitySubgraphs`
        let selectivity = if state.subgraphs.is_empty() {
            1.0
        } else {
            state.subgraphs.iter().map(|s| s.1).product()
        };
        selectivity / (1.0 + state.unused_domains.len() as f64)
    }

    fn classify(edge: &Edge, state: &SelectivityState) -> EdgeKind {
        if edge.class.is_some_and(|c| state.applied_classes.contains(&c)) {
            return EdgeKind::RedundantTransitive;
        }
        match edge.op {
            Some(_) if edge.is_equality() => EdgeKind::InnerEquivalence,
            Some(_) => EdgeKind::NonEquality,
            None => EdgeKind::Other,
        }
    }

    /// `ProcessSelectivityEdge`.
    fn process_edge(&self, edge: &Edge, group: &DomainGroup, set: u64, state: &mut SelectivityState) {
        let kind = Self::classify(edge, state);
        if state.subgraphs.len() == 1 && state.subgraphs[0].0 == set {
            let (relations, mut selectivity) = state.subgraphs[0];
            self.apply_caps(&mut selectivity, relations, state);
            state.subgraphs[0].1 = selectivity;
            if kind == EdgeKind::RedundantTransitive {
                return;
            }
            if kind == EdgeKind::InnerEquivalence && self.apply_increment(&mut selectivity, edge, relations, state) {
                state.subgraphs[0].1 = selectivity;
                return;
            }
            if group.reliable {
                state.unused_domains.insert(group.distinct.to_bits());
            }
            return;
        }
        if let Some(class) = edge.class {
            state.applied_classes.insert(class);
        }
        if let Some(pair) = edge.pair {
            match state.pair_stats.iter_mut().find(|(p, _)| *p == pair) {
                Some((_, stats)) => stats.register(group.distinct),
                None => {
                    let mut stats = PairStats::default();
                    stats.register(group.distinct);
                    state.pair_stats.push((pair, stats));
                }
            }
        }
        let connects = |subgraph: u64| subset(edge.left, subgraph) || subset(edge.right, subgraph);
        let mut connections = Vec::new();
        'outer: for outer in 0..state.subgraphs.len() {
            for inner in outer + 1..state.subgraphs.len() {
                if connects(state.subgraphs[outer].0) && connects(state.subgraphs[inner].0) {
                    connections = vec![outer, inner];
                    break 'outer;
                }
            }
            if connects(state.subgraphs[outer].0) {
                connections = vec![outer];
                break;
            }
        }
        match connections.as_slice() {
            [] => {
                let mut selectivity = Self::edge_selectivity(1.0, edge, group);
                let relations = edge.left | edge.right;
                self.apply_caps(&mut selectivity, relations, state);
                state.subgraphs.push((relations, selectivity));
            }
            [index] => {
                let (relations, mut selectivity) = state.subgraphs[*index];
                if subset(edge.left, relations) && subset(edge.right, relations) {
                    if kind == EdgeKind::InnerEquivalence {
                        self.apply_increment(&mut selectivity, edge, relations, state);
                    }
                    self.apply_caps(&mut selectivity, relations, state);
                    state.subgraphs[*index].1 = selectivity;
                    return;
                }
                let other = if subset(edge.right, relations) { edge.left } else { edge.right };
                let mut selectivity = Self::edge_selectivity(selectivity, edge, group);
                let relations = relations | other;
                self.apply_caps(&mut selectivity, relations, state);
                state.subgraphs[*index] = (relations, selectivity);
            }
            [into, from] => {
                let (into_relations, into_selectivity) = state.subgraphs[*into];
                let (from_relations, from_selectivity) = state.subgraphs[*from];
                let mut selectivity = Self::edge_selectivity(into_selectivity * from_selectivity, edge, group);
                let relations = into_relations | from_relations;
                self.apply_caps(&mut selectivity, relations, state);
                state.subgraphs[*into] = (relations, selectivity);
                state.subgraphs.remove(*from);
            }
            _ => unreachable!("at most two subgraphs"),
        }
    }

    /// `CalculateInnerJoinSelectivity` / `ApplyComparisonRatio`.
    fn edge_selectivity(base: f64, edge: &Edge, group: &DomainGroup) -> f64 {
        let d = if group.distinct <= 0.0 { 1.0 } else { group.distinct };
        match edge.op {
            None | Some(ComparisonOp::Equal | ComparisonOp::NotDistinctFrom) => base / d,
            Some(ComparisonOp::NotEqual | ComparisonOp::DistinctFrom) => {
                if d > 1.0 {
                    base * (d - 1.0) / d
                } else {
                    base
                }
            }
            // strict comparisons keep half the cross product, also of small domains
            Some(ComparisonOp::LessThan | ComparisonOp::GreaterThan) => base / 2.0,
            Some(ComparisonOp::LessThanOrEqual | ComparisonOp::GreaterThanOrEqual) => base * (d + 1.0) / (2.0 * d),
        }
    }

    /// `GetJoinPairCap`: the rows of the smaller of the two relations.
    fn pair_cap(&self, pair: u64) -> f64 {
        let mut cap = f64::MAX;
        for relation in (0..self.cardinalities.len()).filter(|r| pair & (1 << r) != 0) {
            let rows = if self.cardinalities[relation] == 0.0 { 1.0 } else { self.cardinalities[relation] };
            if rows <= 0.0 {
                return 0.0;
            }
            cap = cap.min(rows);
        }
        if cap == f64::MAX { 0.0 } else { cap }
    }

    /// `ApplyJoinPairCap`: a pair joined on several keys is an FK/PK join at most as large as its
    /// larger side.
    fn apply_pair_cap(&self, selectivity: &mut f64, pair: u64, state: &mut SelectivityState) -> bool {
        if state.capped.contains(&pair) {
            return false;
        }
        let cap = self.pair_cap(pair);
        let Some((_, stats)) = state.pair_stats.iter_mut().find(|(p, _)| *p == pair) else {
            return false;
        };
        if !stats.can_apply_cap(cap) {
            return false;
        }
        let first = stats.first_distinct;
        if cap <= 0.0 || first <= 0.0 || first > cap {
            return false;
        }
        if first < cap {
            *selectivity = *selectivity * first / cap;
            stats.first_distinct = cap;
        }
        state.capped.insert(pair);
        true
    }

    /// `ApplyCompositeJoinPairCaps`.
    fn apply_caps(&self, selectivity: &mut f64, scope: u64, state: &mut SelectivityState) {
        let pairs: Vec<u64> = state.pair_stats.iter().map(|(p, _)| *p).collect();
        for pair in pairs {
            if state.capped.contains(&pair) || !subset(pair, scope) || !self.composites.contains(&pair) {
                continue;
            }
            self.apply_pair_cap(selectivity, pair, state);
        }
    }

    /// `ApplyJoinIncrement`: a further equality of a pair already capped changes nothing.
    fn apply_increment(&self, selectivity: &mut f64, edge: &Edge, scope: u64, state: &mut SelectivityState) -> bool {
        let Some(pair) = edge.pair.filter(|_| edge.is_equality()) else {
            return false;
        };
        if !subset(pair, scope) || !self.composites.contains(&pair) {
            return false;
        }
        if state.capped.contains(&pair) {
            return true;
        }
        self.apply_pair_cap(selectivity, pair, state)
    }
}

/// What `conjunct`, on a join, divides its size by (`ApplyComparisonRatio`).
fn predicate_denominator(conjunct: &BoundExpr, domain_of: &dyn Fn(&BoundExpr) -> Option<f64>) -> f64 {
    let BoundExprKind::Comparison { op, left, right } = &conjunct.kind else {
        return 1.0 / DEFAULT_SELECTIVITY;
    };
    let domain = [domain_of(left), domain_of(right)]
        .into_iter()
        .flatten()
        .reduce(f64::max)
        .unwrap_or(1.0);
    match op {
        ComparisonOp::Equal | ComparisonOp::NotDistinctFrom => domain,
        ComparisonOp::NotEqual | ComparisonOp::DistinctFrom => {
            if domain > 1.0 {
                domain / (domain - 1.0)
            } else {
                1.0
            }
        }
        // a strict range comparison keeps (d - 1) / (2 d) of the pairs for independent columns
        // over a shared domain: half the cross product, so small domains can't make it look cheap
        ComparisonOp::LessThan | ComparisonOp::GreaterThan => 2.0,
        // a non-strict one also keeps the 1 / d of the pairs that are equal
        ComparisonOp::LessThanOrEqual | ComparisonOp::GreaterThanOrEqual => 2.0 * domain / (domain + 1.0),
    }
}

/// `DPJoinNode`.
#[derive(Debug, Clone, Copy)]
struct JoinNode {
    /// both are 0 for a relation
    left: u64,
    right: u64,
    cost: f64,
}

/// `PlanEnumerator`.
struct Enumerator<'a> {
    relations: usize,
    /// both ways around
    edges: Vec<(u64, u64)>,
    estimator: &'a mut Estimator,
    plans: HashMap<u64, JoinNode>,
    pairs: usize,
}

fn subset(set: u64, of: u64) -> bool {
    set & !of == 0
}

impl Enumerator<'_> {
    /// `QueryGraphEdges::GetNeighbors`, as the lowest relation of each set next to `node`.
    fn neighbors(&self, node: u64, exclusion: u64) -> Vec<u64> {
        let mut neighbors: Vec<u64> = self
            .edges
            .iter()
            .filter(|(from, to)| subset(*from, node) && to & to.wrapping_neg() & exclusion == 0)
            .map(|(_, to)| to & to.wrapping_neg())
            .collect();
        neighbors.sort_unstable();
        neighbors.dedup();
        neighbors
    }

    fn connected(&self, left: u64, right: u64) -> bool {
        self.edges
            .iter()
            .any(|(from, to)| subset(*from, left) && subset(*to, right))
    }

    /// `EmitPair`: keeps the join of two plans if it is the cheapest for its relations.
    fn emit_pair(&mut self, left: u64, right: u64) {
        let (Some(l), Some(r)) = (self.plans.get(&left), self.plans.get(&right)) else {
            return;
        };
        let inputs = l.cost + r.cost;
        let set = left | right;
        let cost = self.estimator.estimate(set) + inputs;
        if self.plans.get(&set).is_none_or(|old| cost < old.cost) {
            self.plans.insert(set, JoinNode { left, right, cost });
        }
    }

    fn try_emit_pair(&mut self, left: u64, right: u64) -> bool {
        self.pairs += 1;
        if self.pairs >= MAX_PAIRS {
            return false;
        }
        self.emit_pair(left, right);
        true
    }

    /// `GetAllNeighborSets`: the sets of `neighbors` that are not empty, small ones first.
    fn neighbor_sets(neighbors: &[u64]) -> Vec<u64> {
        let mut sets: Vec<u64> = (1u64..1 << neighbors.len())
            .map(|picked| {
                neighbors
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| picked & (1 << at) != 0)
                    .fold(0, |set, (_, neighbor)| set | neighbor)
            })
            .collect();
        sets.sort_by_key(|set| set.count_ones());
        sets
    }

    fn emit_csg(&mut self, node: u64) -> bool {
        if node.count_ones() as usize == self.relations {
            return true;
        }
        let lowest = node & node.wrapping_neg();
        let exclusion = (lowest - 1) | node;
        let mut neighbors = self.neighbors(node, exclusion);
        neighbors.reverse();
        let mut with_neighbors = neighbors.iter().fold(exclusion, |set, n| set | n);
        for neighbor in neighbors {
            if self.connected(node, neighbor) && !self.try_emit_pair(node, neighbor) {
                return false;
            }
            if !self.enumerate_cmp(node, neighbor, with_neighbors) {
                return false;
            }
            with_neighbors &= !neighbor;
        }
        true
    }

    fn enumerate_cmp(&mut self, left: u64, right: u64, exclusion: u64) -> bool {
        let neighbors = self.neighbors(right, exclusion);
        if neighbors.is_empty() {
            return true;
        }
        let sets: Vec<u64> = Self::neighbor_sets(&neighbors).iter().map(|set| right | set).collect();
        for &combined in &sets {
            if self.plans.contains_key(&combined)
                && self.connected(left, combined)
                && !self.try_emit_pair(left, combined)
            {
                return false;
            }
        }
        let exclusion = neighbors.iter().fold(exclusion, |set, n| set | n);
        sets.into_iter()
            .all(|combined| self.enumerate_cmp(left, combined, exclusion))
    }

    fn enumerate_csg(&mut self, node: u64, exclusion: u64) -> bool {
        let neighbors = self.neighbors(node, exclusion);
        if neighbors.is_empty() {
            return true;
        }
        let sets: Vec<u64> = Self::neighbor_sets(&neighbors).iter().map(|set| node | set).collect();
        for &set in &sets {
            if self.plans.contains_key(&set) && !self.emit_csg(set) {
                return false;
            }
        }
        let exclusion = neighbors.iter().fold(exclusion, |set, n| set | n);
        sets.into_iter().all(|set| self.enumerate_csg(set, exclusion))
    }

    fn solve_exactly(&mut self) -> bool {
        for relation in (0..self.relations).rev() {
            let node = 1 << relation;
            if !self.emit_csg(node) || !self.enumerate_csg(node, (node << 1) - 1) {
                return false;
            }
        }
        true
    }

    /// `SolveJoinOrderApproximately`: joins the cheapest pair until one plan is left, with cross
    /// products when nothing connects the plans.
    fn solve_approximately(&mut self) {
        let mut sets: Vec<u64> = (0..self.relations).map(|relation| 1 << relation).collect();
        while sets.len() > 1 {
            let mut best: Option<(bool, f64, u64, u64)> = None;
            for (at, &left) in sets.iter().enumerate() {
                for &right in &sets[at + 1..] {
                    let inputs = self.plans[&left].cost + self.plans[&right].cost;
                    let connected = self.connected(left, right) || self.connected(right, left);
                    let cost = self.estimator.estimate(left | right) + inputs;
                    let better = match best {
                        None => true,
                        Some((best_connected, best_cost, ..)) => {
                            (connected && !best_connected) || (connected == best_connected && cost < best_cost)
                        }
                    };
                    if better {
                        best = Some((connected, cost, left, right));
                    }
                }
            }
            let Some((_, _, left, right)) = best else {
                return;
            };
            self.plans.remove(&(left | right));
            self.emit_pair(left, right);
            sets.retain(|&set| set != left && set != right);
            sets.push(left | right);
        }
    }

    fn solve(&mut self) {
        let all = (1 << self.relations) - 1;
        if self.relations >= APPROXIMATE_THRESHOLD.with(std::cell::Cell::get)
            || !self.solve_exactly()
            || !self.plans.contains_key(&all)
        {
            self.solve_approximately();
        }
        if FORCE_NO_CROSS_PRODUCT.with(std::cell::Cell::get) && self.uses_cross_product(all) {
            ERROR.with(|e| {
                *e.borrow_mut() = Some(duckrs_common::Error::new(
                    duckrs_common::ErrorKind::InvalidInput,
                    "Query requires a cross-product, but 'force_no_cross_product' PRAGMA is enabled",
                ));
            });
        }
    }

    /// `PlanUsesCrossProduct`: a join of the plan between sets no edge connects.
    fn uses_cross_product(&self, set: u64) -> bool {
        let Some(node) = self.plans.get(&set) else {
            return set.count_ones() > 1;
        };
        if node.left == 0 || node.right == 0 {
            return false;
        }
        !(self.connected(node.left, node.right) || self.connected(node.right, node.left))
            || self.uses_cross_product(node.left)
            || self.uses_cross_product(node.right)
    }
}

/// One tree of joins while it is taken apart and put together again.
struct Tree {
    relations: Vec<Option<LogicalOperator>>,
    plans: HashMap<u64, JoinNode>,
    predicates: Vec<Option<Predicate>>,
    estimator: Estimator,
}

impl Tree {
    /// `QueryGraphManager::GenerateJoins`. Each predicate goes to the lowest join with all of its
    /// relations, and of an inner join the side with the fewest rows is the one to build.
    fn build(&mut self, set: u64) -> LogicalOperator {
        let node = self.plans[&set];
        if node.left == 0 {
            let relation = set.trailing_zeros() as usize;
            return self.relations[relation].take().expect("each relation is joined once");
        }
        let (mut left_set, mut right_set) = (node.left, node.right);
        if self.estimator.estimate(left_set) < self.estimator.estimate(right_set) {
            std::mem::swap(&mut left_set, &mut right_set);
        }
        let left = self.build(left_set);
        let right = self.build(right_set);
        let mut conditions = Vec::new();
        let mut residual = Vec::new();
        // the sets of equal columns this join has an equality of already: one makes the others so
        let mut classes = Vec::new();
        for slot in &mut self.predicates {
            let applies = slot
                .as_ref()
                .is_some_and(|p| subset(p.set, set) && !subset(p.set, left_set) && !subset(p.set, right_set));
            if !applies {
                continue;
            }
            let predicate = slot.take().expect("just looked at it");
            if let Some(class) = predicate.class {
                if classes.contains(&class) {
                    continue;
                }
                classes.push(class);
            }
            match (predicate.sides, predicate.expr.kind) {
                (
                    Some((first, second)),
                    BoundExprKind::Comparison {
                        op: op @ (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom),
                        left: a,
                        right: b,
                    },
                ) if (subset(first, left_set) && subset(second, right_set))
                    || (subset(first, right_set) && subset(second, left_set)) =>
                {
                    let (a, b) = if subset(first, left_set) { (a, b) } else { (b, a) };
                    conditions.push(JoinCondition {
                        left: *a,
                        right: *b,
                        comparison: op,
                        filter: None,
                    });
                }
                (_, kind) => residual.push(BoundExpr::new(kind, predicate.expr.return_type)),
            }
        }
        if conditions.is_empty() && residual.is_empty() {
            return LogicalOperator::new(K::CrossProduct, vec![left, right]);
        }
        LogicalOperator::new(
            K::Join {
                join_type: JoinType::Inner,
                conditions,
                predicate: (!residual.is_empty()).then(|| conjunction_of(residual)),
                algorithm: Default::default(),
            },
            vec![left, right],
        )
    }
}

/// `JoinOrderOptimizer`.
#[derive(Default, Clone)]
struct JoinOrderOptimizer {
    /// the size of what a `DelimGet` reads, by the rows it shares with its `DelimJoin`
    delim: HashMap<usize, f64>,
    /// what the rows of a materialized CTE (or the working table of a recursive one) are, by its
    /// data, with the bindings of its columns in order
    ctes: HashMap<usize, (Vec<ColumnBinding>, RelationStats)>,
}

/// A filter the statistics made from the keys of a join (`CreateFilterFromJoinStats`).
fn is_join_stats_filter(expr: &BoundExpr) -> bool {
    expr.alias == super::statistics::JOIN_STATS_FILTER
}

/// `RelationStatisticsHelper::RebindOutputStats`: the columns of `stats`, in the order of `from`,
/// under the bindings `to`.
fn rebind(stats: &RelationStats, from: &[ColumnBinding], to: &[ColumnBinding]) -> RelationStats {
    let mut result = RelationStats::new(stats.cardinality);
    result.retention = stats.retention;
    for (at, binding) in to.iter().enumerate() {
        let column = from.get(at).map_or_else(|| stats.column(binding), |source| stats.column(source));
        result.columns.insert(*binding, column);
    }
    result
}

/// `GetChildColumnBinding`: the first column an expression reads, unless it is volatile.
/// A cast of a column between integral types, or a compression of it (`CompressedMaterialization`),
/// which gives different values for different values.
fn keeps_values_apart(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Cast { child, try_cast: false } => {
            matches!(child.kind, BoundExprKind::ColumnRef { .. })
                && child.return_type.is_integral()
                && expr.return_type.is_integral()
        }
        BoundExprKind::Function { function, children, .. } => {
            (function.name.starts_with("__internal_compress_") || function.name.starts_with("__internal_decompress_"))
                && children.first().is_some_and(|c| matches!(c.kind, BoundExprKind::ColumnRef { .. }))
        }
        _ => false,
    }
}

fn child_column_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        _ if expr.is_volatile() => None,
        _ => expr.children().into_iter().find_map(child_column_binding),
    }
}

/// `GetEqualityDomainBounds` and `CombineColumnStats`: the equalities of a tree of INNER joins
/// make classes of columns, all of which have no more values than the one with the fewest.
fn narrow_equality_classes(op: &LogicalOperator, stats: &mut RelationStats) {
    fn collect(op: &LogicalOperator, pairs: &mut Vec<(ColumnBinding, ColumnBinding)>) {
        match &op.kind {
            K::Join {
                join_type: JoinType::Inner,
                conditions,
                ..
            } => {
                for condition in conditions {
                    if let (Some(l), Some(r)) = (equivalence_binding(&condition.left), equivalence_binding(&condition.right)) {
                        pairs.push((l, r));
                    }
                }
            }
            K::CrossProduct => {}
            _ => return,
        }
        for child in &op.children {
            collect(child, pairs);
        }
    }
    let mut pairs = Vec::new();
    collect(op, &mut pairs);
    let mut parents: HashMap<ColumnBinding, ColumnBinding> = HashMap::new();
    fn find(parents: &mut HashMap<ColumnBinding, ColumnBinding>, binding: ColumnBinding) -> ColumnBinding {
        let parent = *parents.entry(binding).or_insert(binding);
        if parent == binding {
            return binding;
        }
        let root = find(parents, parent);
        parents.insert(binding, root);
        root
    }
    for (l, r) in &pairs {
        let (l, r) = (find(&mut parents, *l), find(&mut parents, *r));
        if l != r {
            parents.insert(l, r);
        }
    }
    let members: Vec<ColumnBinding> = parents.keys().copied().collect();
    let mut bounds: HashMap<ColumnBinding, Option<f64>> = HashMap::new();
    for binding in &members {
        let root = find(&mut parents, *binding);
        let current = stats.columns.get(binding).map(|c| c.current.count);
        let entry = bounds.entry(root).or_insert(Some(f64::MAX));
        // a class with a column the statistics do not have is not complete
        *entry = match (*entry, current) {
            (Some(bound), Some(current)) => Some(bound.min(current)),
            _ => None,
        };
    }
    for binding in members {
        let root = find(&mut parents, binding);
        if let (Some(Some(bound)), Some(column)) = (bounds.get(&root), stats.columns.get_mut(&binding)) {
            column.current = DistinctCount::cardinality(column.current.count.min(*bound));
        }
    }
}

impl JoinOrderOptimizer {
    /// `RelationStatisticsHelper`, without the optimizer in between: the statistics of the rows
    /// of `op`, as `ExtractOperatorStats` has them.
    fn stats(&self, op: &LogicalOperator) -> RelationStats {
        match &op.kind {
            K::Get {
                table,
                table_index,
                column_ids,
                filters,
                ..
            } => {
                let Some(counts) = table_distinct_counts(table) else {
                    return RelationStats::uniform(op.column_bindings(), UNKNOWN_CARDINALITY);
                };
                // `ExtractGetStats`
                let base = counts.rows as f64;
                let mut stats = RelationStats::new(base);
                stats.retention = if base == 0.0 { 0.0 } else { 1.0 };
                for (at, &column) in column_ids.iter().enumerate() {
                    let count = if column == COLUMN_IDENTIFIER_ROW_ID {
                        None
                    } else {
                        counts.columns.get(column).copied().flatten().filter(|&c| c > 0)
                    };
                    let total = match count {
                        Some(count) => DistinctCount::new(count as f64, CountSource::Hll),
                        None => DistinctCount::cardinality(base),
                    };
                    let mut column_stats = ColumnStats::new(total);
                    // no two rows share a row id, which a SEMI join on them (late materialization) counts on
                    if column == COLUMN_IDENTIFIER_ROW_ID {
                        column_stats.evidence.mark_unique();
                    }
                    stats.columns.insert(ColumnBinding::new(*table_index, at), column_stats);
                }
                let filters: Vec<BoundExpr> = filters
                    .iter()
                    .filter(|filter| !is_join_stats_filter(filter))
                    .filter(|filter| !is_proven_not_null(filter, table, *table_index, column_ids))
                    .cloned()
                    .collect();
                let cardinality = scan_filter_cardinality(table, *table_index, column_ids, &filters, &stats);
                stats.apply_cardinality(cardinality);
                for (binding, column) in stats.columns.iter_mut() {
                    for filter in &filters {
                        apply_direct_filter_domain(column, filter, *binding);
                    }
                }
                stats.project(&op.column_bindings())
            }
            // a scan whose filters left out all of its files has no estimate (`LogicalGet` takes 1)
            K::ParquetScan { scan, .. } if scan.files.is_empty() && scan.total_files.is_some() => {
                RelationStats::uniform(op.column_bindings(), 1.0)
            }
            K::ParquetScan { table_index, scan } => {
                let Some(rows) = crate::execution::parquet::scan_cardinality(scan) else {
                    return RelationStats::uniform(op.column_bindings(), UNKNOWN_CARDINALITY);
                };
                let base = rows as f64;
                let mut stats = RelationStats::uniform(op.column_bindings(), base);
                stats.retention = if base == 0.0 { 0.0 } else { 1.0 };
                // the range of an integral column's values bounds how many distinct ones it has
                for (column, column_stats) in crate::execution::parquet::scan_statistics(scan).into_iter().enumerate() {
                    let Some(column_stats) = column_stats else {
                        continue;
                    };
                    // `GetMinMaxDistinctCount`: integers of up to 64 bits whose span and its
                    // successor fit 64 bits
                    use duckrs_common::types::PhysicalType as P;
                    if scan.types[column].id() == duckrs_common::types::LogicalTypeId::Decimal
                        || !matches!(
                            scan.types[column].physical_type(),
                            P::Int8 | P::Int16 | P::Int32 | P::Int64 | P::UInt8 | P::UInt16 | P::UInt32 | P::UInt64
                        )
                    {
                        continue;
                    }
                    let distinct = (|| {
                        let (min, max) = (column_stats.min?.as_i128()?, column_stats.max?.as_i128()?);
                        let span = u64::try_from(max.checked_sub(min)?).ok()?;
                        let distinct = span.checked_add(1)?.min(rows);
                        (rows > 0 && distinct != u64::MAX).then_some(distinct)
                    })();
                    if let Some(distinct) = distinct {
                        let binding = ColumnBinding::new(*table_index, column);
                        if stats.columns.contains_key(&binding) {
                            let domain = DistinctCount::new(distinct as f64, CountSource::MinMax);
                            stats.columns.insert(binding, ColumnStats::new(domain));
                        }
                    }
                }
                let filters: Vec<BoundExpr> =
                    scan.filters.iter().filter(|f| !is_join_stats_filter(f)).cloned().collect();
                let cardinality = filter_cardinality(&filters, &stats);
                stats.apply_cardinality(cardinality);
                for (binding, column) in stats.columns.iter_mut() {
                    for filter in &filters {
                        apply_direct_filter_domain(column, filter, *binding);
                    }
                }
                stats
            }
            K::Range { count, .. } => RelationStats::uniform(op.column_bindings(), *count as f64),
            K::EmptyResult { .. } => RelationStats::uniform(op.column_bindings(), 0.0),
            K::DummyScan { .. } => RelationStats::uniform(op.column_bindings(), 1.0),
            K::ChunkGet { chunks, .. } => {
                RelationStats::uniform(op.column_bindings(), chunks.iter().map(|c| c.len()).sum::<usize>() as f64)
            }
            K::ExpressionGet { expressions, .. } => {
                RelationStats::uniform(op.column_bindings(), expressions.len() as f64)
            }
            // `ExtractDelimGetStats`: a value per column
            K::DelimGet { delim, .. } => {
                let cardinality = self
                    .delim
                    .get(&(Arc::as_ptr(delim) as *const () as usize))
                    .copied()
                    .unwrap_or(UNKNOWN_CARDINALITY);
                let mut stats = RelationStats::new(cardinality);
                for binding in op.column_bindings() {
                    stats.columns.insert(binding, ColumnStats::new(DistinctCount::cardinality(1.0)));
                }
                stats.cap_current_domains();
                stats
            }
            K::MaterializedCte { .. } => {
                let stats = self.stats(&op.children[1]);
                stats.project(&op.column_bindings())
            }
            // `ReadSingleCSVFileCardinality`: a file's bytes over 5 per column of a row, 42 rows where
            // its size is not known
            // a scan whose filters left out all of its files has no estimate (`LogicalGet` takes 1)
            K::CsvScan { scan, .. } if scan.files.is_empty() => RelationStats::uniform(op.column_bindings(), 1.0),
            K::CsvScan { scan, .. } => RelationStats::uniform(
                op.column_bindings(),
                scan.files
                    .iter()
                    .map(|file| match std::fs::metadata(&file.path) {
                        Ok(metadata) if !scan.types.is_empty() => metadata.len() / (scan.types.len() as u64 * 5),
                        _ => 42,
                    })
                    .sum::<u64>() as f64,
            ),
            // `ReadSingleJSONFileCardinality` for each file: the estimate of the first, 42 without one
            K::JsonScan { scan, .. } => RelationStats::uniform(
                op.column_bindings(),
                if scan.files.is_empty() {
                    1.0
                } else {
                    (scan.cardinality_per_file.unwrap_or(42) * scan.files.len() as u64) as f64
                },
            ),
            // `RebindOutputStats` of the statistics of the CTE
            K::CteRef { data, .. } => match self.ctes.get(&(Arc::as_ptr(data) as *const () as usize)) {
                Some((bindings, stats)) => rebind(stats, bindings, &op.column_bindings()),
                None => RelationStats::uniform(op.column_bindings(), UNKNOWN_CARDINALITY),
            },
            K::WorkingTable { working, .. } => match self.ctes.get(&(Arc::as_ptr(working) as *const () as usize)) {
                Some((bindings, stats)) => rebind(stats, bindings, &op.column_bindings()),
                None => RelationStats::uniform(op.column_bindings(), UNKNOWN_CARDINALITY),
            },
            // a recursive CTE runs its recursive part over the rows of the one before, assumed 1000 times;
            // the statistics of one round say nothing of all of them
            K::RecursiveCte {
                working, recursive, ..
            } => {
                let anchor = self.stats(&op.children[0]);
                let mut inner = self.clone();
                inner.ctes.insert(
                    Arc::as_ptr(working) as *const () as usize,
                    (op.children[0].column_bindings(), anchor.clone()),
                );
                let rounds = inner.stats(recursive);
                let cardinality = anchor.cardinality + rounds.cardinality * 1000.0;
                RelationStats::uniform(op.column_bindings(), cardinality)
            }
            K::Filter { expressions } => {
                // `ExtractFilterStats`
                let child = self.stats(&op.children[0]);
                let expressions: Vec<BoundExpr> =
                    expressions.iter().filter(|e| !is_join_stats_filter(e)).cloned().collect();
                let expressions = &expressions;
                let cardinality = filter_cardinality(expressions, &child);
                let mut stats = child.project(&op.column_bindings());
                stats.apply_cardinality(cardinality);
                for (binding, column) in stats.columns.iter_mut() {
                    for expr in expressions {
                        apply_direct_filter_domain(column, expr, *binding);
                    }
                }
                stats
            }
            K::ColumnSelect { .. } | K::Order { .. } | K::SecureView { .. } => {
                let child = self.stats(&op.children[0]);
                child.project(&op.column_bindings())
            }
            K::Projection {
                table_index,
                expressions,
            } => {
                // `ExtractProjectionStats`
                let child = self.stats(&op.children[0]);
                let mut stats = RelationStats::new(child.cardinality);
                stats.retention = child.retention;
                for (at, expr) in expressions.iter().enumerate() {
                    let binding = ColumnBinding::new(*table_index, at);
                    let column = if expr.is_foldable() {
                        ColumnStats::new(DistinctCount::new(child.cardinality.min(1.0), CountSource::Hll))
                    } else if let Some(source) = child_column_binding(expr) {
                        let source_column = child.column(&source);
                        let mut column = source_column.plain();
                        if matches!(expr.kind, BoundExprKind::ColumnRef { .. }) || keeps_values_apart(expr) {
                            column.evidence = source_column.evidence;
                        }
                        column
                    } else {
                        ColumnStats::new(DistinctCount::cardinality(child.cardinality))
                    };
                    stats.columns.insert(binding, column);
                }
                stats
            }
            K::Aggregate { .. } => self.aggregate_stats(op),
            K::Window { table_index, .. } => {
                let child = self.stats(&op.children[0]);
                let mut stats = child.project(&op.children[0].column_bindings());
                for binding in op.column_bindings() {
                    if binding.table_index == *table_index {
                        let window = ColumnStats::new(DistinctCount::cardinality(stats.cardinality));
                        stats.columns.insert(binding, window);
                    }
                }
                stats
            }
            K::Distinct { targets, .. } => {
                // `ExtractDistinctStats`
                let child = self.stats(&op.children[0]);
                let mut stats = child.project(&op.column_bindings());
                stats.retention = 1.0;
                let counts: Option<Vec<DistinctCount>> = if targets.is_empty() {
                    Some(op.children[0].column_bindings().iter().map(|b| child.column(b).current).collect())
                } else {
                    targets
                        .iter()
                        .map(|target| match &target.kind {
                            BoundExprKind::ColumnRef { binding, depth: 0 } => Some(child.column(binding).current),
                            BoundExprKind::Constant(_) => {
                                Some(DistinctCount::new(child.cardinality.min(1.0), CountSource::Hll))
                            }
                            _ => None,
                        })
                        .collect()
                };
                let Some(counts) = counts else {
                    return stats;
                };
                stats.cardinality = estimate_distinct_cardinality(&counts, child.cardinality);
                stats.cap_current_domains();
                let unique = match targets.as_slice() {
                    [] if stats.columns.len() == 1 => stats.columns.keys().next().copied(),
                    [target] => bare_column(target),
                    _ => None,
                };
                if let Some(column) = unique.and_then(|binding| stats.columns.get_mut(&binding)) {
                    column.evidence.mark_unique();
                }
                stats
            }
            // `ExtractLimitStats`; a TOP N was an ORDER BY and a LIMIT
            K::Limit { limit, offset, .. } => self.limit_stats(op, *limit, *offset, None),
            K::TopN { limit, offset, .. } => self.limit_stats(op, Some(*limit), *offset, None),
            K::LimitPercent { percent, offset } => self.limit_stats(op, None, *offset, Some(*percent)),
            K::Union { .. } | K::ExceptIntersect { .. } => self.set_operation_stats(op),
            K::CrossProduct => {
                let (left, right) = (self.stats(&op.children[0]), self.stats(&op.children[1]));
                let mut stats = RelationStats::new(left.cardinality * right.cardinality);
                for (binding, column) in left.columns.iter().chain(&right.columns) {
                    stats.columns.insert(*binding, column.plain());
                }
                match self.tree_cardinality(op) {
                    Some(cardinality) => {
                        stats.cardinality = cardinality;
                        stats.cap_current_domains();
                        stats
                    }
                    None => {
                        stats.cap_current_domains();
                        stats
                    }
                }
            }
            K::MarkJoin { table_index, .. } => {
                let left = self.stats(&op.children[0]);
                let mut stats = RelationStats::new(left.cardinality);
                for (binding, column) in &left.columns {
                    stats.columns.insert(*binding, column.plain());
                }
                let mark = DistinctCount::cardinality(left.cardinality.min(3.0));
                stats.columns.insert(ColumnBinding::new(*table_index, 0), ColumnStats::new(mark));
                stats.cap_current_domains();
                stats
            }
            K::Join {
                join_type,
                conditions,
                predicate,
                ..
            } => {
                let (left, right) = (self.stats(&op.children[0]), self.stats(&op.children[1]));
                if matches!(
                    join_type,
                    JoinType::Semi | JoinType::Anti | JoinType::RightSemi | JoinType::RightAnti
                ) {
                    let residual = predicate.as_ref().map(|p| (p, p));
                    let mut semi_conditions: Vec<SemiAntiCondition> = conditions
                        .iter()
                        .map(|c| SemiAntiCondition {
                            left: &c.left,
                            right: &c.right,
                            op: c.comparison,
                        })
                        .collect();
                    if let Some((l, r)) = residual {
                        semi_conditions.push(SemiAntiCondition {
                            left: l,
                            right: r,
                            op: ComparisonOp::LessThan,
                        });
                    }
                    return semi_anti_join_stats(&semi_conditions, *join_type, &left, &right);
                }
                let (left_rows, right_rows) = (left.cardinality, right.cardinality);
                let mut columns: HashMap<ColumnBinding, ColumnStats> = HashMap::new();
                for (binding, column) in left.columns.iter().chain(&right.columns) {
                    columns.insert(*binding, column.plain());
                }
                let domain_of = |expr: &BoundExpr| {
                    let mut found = Vec::new();
                    expression_columns(expr, &mut found);
                    let distinct = found.iter().filter_map(|c| columns.get(c).map(|s| s.current.count));
                    distinct.reduce(f64::max)
                };
                // `GetLeftJoinSelectivity`: equalities that connect the same columns
                // (`JoinEqualityGroups`) count once, so `a = x AND a = y` is no composite key
                let mut parents: HashMap<ColumnBinding, ColumnBinding> = HashMap::new();
                fn root(parents: &mut HashMap<ColumnBinding, ColumnBinding>, binding: ColumnBinding) -> ColumnBinding {
                    let mut current = binding;
                    while let Some(&parent) = parents.get(&current) {
                        if parent == current {
                            break;
                        }
                        current = parent;
                    }
                    current
                }
                let comparisons = conditions.iter().filter(|c| !c.is_equality()).cloned().map(condition_expression);
                let mut conjuncts: Vec<BoundExpr> = comparisons.collect();
                if let Some(predicate) = predicate {
                    split_conjuncts(predicate.clone(), &mut conjuncts);
                }
                let mut grouped: Vec<Option<(ColumnBinding, ColumnBinding)>> = Vec::new();
                for condition in conditions.iter().filter(|condition| condition.is_equality()) {
                    let pair = bare_column(&condition.left).zip(bare_column(&condition.right));
                    if let Some((l, r)) = pair {
                        let (a, b) = (root(&mut parents, l), root(&mut parents, r));
                        parents.entry(a).or_insert(a);
                        parents.insert(b, a);
                    }
                    grouped.push(pair);
                }
                for (l, r) in conjuncts.iter().filter_map(residual_equality) {
                    let (a, b) = (root(&mut parents, l), root(&mut parents, r));
                    parents.entry(a).or_insert(a);
                    parents.insert(b, a);
                }
                let mut group_roots: Vec<ColumnBinding> = Vec::new();
                let keep: Vec<bool> = grouped
                    .iter()
                    .map(|pair| match pair {
                        Some((l, _)) => {
                            let group = root(&mut parents, *l);
                            if group_roots.contains(&group) {
                                false
                            } else {
                                group_roots.push(group);
                                true
                            }
                        }
                        None => true,
                    })
                    .collect();
                // past the optimizer, the comparisons of the predicate are conditions too
                let domains: Vec<f64> = conditions
                    .iter()
                    .filter(|condition| condition.is_equality())
                    .zip(keep)
                    .filter_map(|(condition, keep)| keep.then_some(condition))
                    .map(|condition| {
                        let sides = [domain_of(&condition.left), domain_of(&condition.right)];
                        let domain = sides.into_iter().flatten().reduce(f64::max);
                        domain.unwrap_or(left_rows.max(right_rows)).max(1.0)
                    })
                    .collect();
                let mut denominator =
                    composite_denominator(&domains, left_rows, right_rows).unwrap_or_else(|| domains.iter().product());
                for conjunct in &conjuncts {
                    // a same-side equality joins the group of an equality between the sides, and
                    // a group of its own is a residual (`GetResidualOnSelectivity`) once
                    if let Some(pair) = residual_equality(conjunct) {
                        let group = root(&mut parents, pair.0);
                        if !group_roots.contains(&group) {
                            group_roots.push(group);
                            denominator /= DEFAULT_SELECTIVITY;
                        }
                        continue;
                    }
                    denominator *= predicate_denominator(conjunct, &domain_of);
                }
                let inner = left_rows * right_rows / denominator;
                let cardinality = match join_type {
                    // `EstimateCardinality` of all relations of the tree below
                    JoinType::Inner => self.tree_cardinality(op).unwrap_or(inner),
                    JoinType::Left => inner.max(left_rows),
                    JoinType::Right => inner.max(right_rows),
                    JoinType::Single | JoinType::Mark => left_rows,
                    _ => inner.max(left_rows).max(right_rows),
                };
                let mut stats = RelationStats::new(cardinality);
                stats.columns = columns;
                // `GetEqualityDomainBounds`: the columns of an INNER equality class have the values of
                // the one with the fewest
                if *join_type == JoinType::Inner {
                    narrow_equality_classes(op, &mut stats);
                }
                for column in stats.columns.values_mut() {
                    column.current.count = column.current.count.min(column.total.count);
                }
                stats.cap_current_domains();
                stats
            }
            _ if !op.children.is_empty() => self.stats(&op.children[0]),
            _ => RelationStats::uniform(op.column_bindings(), UNKNOWN_CARDINALITY),
        }
    }

    /// `ExtractLimitStats`.
    fn limit_stats(&self, op: &LogicalOperator, limit: Option<u64>, offset: u64, percent: Option<f64>) -> RelationStats {
        let child = self.stats(&op.children[0]);
        let mut stats = child.project(&op.column_bindings());
        let available = if offset as f64 >= child.cardinality {
            0.0
        } else {
            child.cardinality - offset as f64
        };
        let mut cardinality = available;
        if let Some(limit) = limit {
            cardinality = cardinality.min(limit as f64);
        }
        if let Some(percent) = percent {
            let rows = if percent < 100.0 {
                (child.cardinality * percent / 100.0).floor()
            } else {
                child.cardinality
            };
            cardinality = cardinality.min(rows);
        }
        stats.apply_cardinality(cardinality);
        stats
    }

    /// `ExtractSetOperationStats`: the columns by position.
    fn set_operation_stats(&self, op: &LogicalOperator) -> RelationStats {
        let children: Vec<RelationStats> = op.children.iter().map(|c| self.stats(c)).collect();
        let child_bindings: Vec<Vec<ColumnBinding>> = op.children.iter().map(|c| c.column_bindings()).collect();
        let (union, except) = match &op.kind {
            K::Union { .. } => (true, false),
            K::ExceptIntersect { except, .. } => (false, *except),
            _ => unreachable!(),
        };
        let cardinality = if union {
            children.iter().map(|c| c.cardinality).sum()
        } else if except {
            children[0].cardinality
        } else {
            children.iter().map(|c| c.cardinality).reduce(f64::min).unwrap_or(0.0)
        };
        let mut stats = RelationStats::new(cardinality);
        for (at, binding) in op.column_bindings().into_iter().enumerate() {
            let Some(first) = child_bindings[0].get(at) else {
                return RelationStats::uniform(op.column_bindings(), cardinality);
            };
            let first = children[0].column(first);
            let (mut total, mut current) = (first.total, first.current);
            for (child, bindings) in children.iter().zip(&child_bindings).skip(1) {
                let Some(column) = bindings.get(at).map(|b| child.column(b)) else {
                    return RelationStats::uniform(op.column_bindings(), cardinality);
                };
                if union {
                    total = DistinctCount::cardinality(total.count + column.total.count);
                    current = DistinctCount::cardinality(current.count + column.current.count);
                } else if !except {
                    total = DistinctCount::cardinality(total.count.min(column.total.count));
                    current = DistinctCount::cardinality(current.count.min(column.current.count));
                }
            }
            total.count = total.count.max(current.count);
            stats.columns.insert(binding, ColumnStats::with(total, current));
        }
        stats.cap_current_domains();
        stats
    }

    /// `ExtractAggregationStats`.
    fn aggregate_stats(&self, op: &LogicalOperator) -> RelationStats {
        let K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index,
            groups,
            aggregates,
            grouping_sets,
            grouping_functions,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let child = self.stats(&op.children[0]);
        let all_groups: BTreeSet<usize> = (0..groups.len()).collect();
        let sets: Vec<&BTreeSet<usize>> = if grouping_sets.is_empty() {
            vec![&all_groups]
        } else {
            grouping_sets.iter().collect()
        };
        let mut counts: Vec<DistinctCount> = Vec::new();
        for set in &sets {
            let mut set_counts = Vec::new();
            for &group in set.iter() {
                let Some(binding) = groups.get(group).and_then(|g| match &g.kind {
                    BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
                    _ => None,
                }) else {
                    continue;
                };
                let mut count = child.column(&binding).current;
                count.count = count.count.max(1.0);
                set_counts.push(count);
            }
            if set_counts.len() > counts.len() {
                counts = set_counts;
            }
        }
        let mut cardinality = if groups.is_empty() {
            1.0
        } else {
            estimate_distinct_cardinality(&counts, child.cardinality)
        };
        let preserves_evidence = sets.len() == 1 && sets[0].len() == groups.len();
        if preserves_evidence && !groups.is_empty() && counts.len() == groups.len() {
            let positions: Option<Vec<usize>> = {
                let bindings = op.children[0].column_bindings();
                groups
                    .iter()
                    .map(|g| bare_column(g).and_then(|b| bindings.iter().position(|x| *x == b)))
                    .collect()
            };
            if let Some(positions) = positions
                && let Some(union_groups) = self.union_groups(&op.children[0], positions, child.cardinality)
            {
                // per-arm products of the domains do not combine keys of different arms
                cardinality = cardinality.min(union_groups);
            }
        }
        // `CapGroupedAggregateCardinalityByKeyDomain`, which only ever lowers the estimate
        if let K::Aggregate {
            domain_cap: Some(cap), ..
        } = &op.kind
            && sets.len() == 1
        {
            cardinality = cardinality.min(*cap as f64);
        }
        let mut stats = RelationStats::new(cardinality);
        for (at, group) in groups.iter().enumerate() {
            let binding = ColumnBinding::new(*group_index, at);
            let mut column = match group.kind {
                BoundExprKind::ColumnRef { binding: source, depth: 0 } => {
                    let source = child.column(&source);
                    let mut column = source.plain();
                    if preserves_evidence {
                        column.evidence = source.evidence;
                    }
                    column
                }
                _ => ColumnStats::new(DistinctCount::cardinality(cardinality)),
            };
            if groups.len() == 1 && sets.len() == 1 && sets[0].len() == 1 && sets[0].contains(&0) {
                column.evidence.mark_unique();
            }
            stats.columns.insert(binding, column);
        }
        for at in 0..aggregates.len() {
            let column = ColumnStats::new(DistinctCount::cardinality(cardinality));
            stats.columns.insert(ColumnBinding::new(*aggregate_index, at), column);
        }
        for at in 0..grouping_functions.len() {
            let count = cardinality.min(grouping_sets.len().max(1) as f64);
            let column = ColumnStats::new(DistinctCount::cardinality(count));
            stats.columns.insert(ColumnBinding::new(*groupings_index, at), column);
        }
        stats.cap_current_domains();
        stats
    }

    /// `EstimateUnionGroups`: below projections of the groups, the groups of the arms of a UNION
    /// added up, each arm's the product of its domains.
    fn union_groups(&self, input: &LogicalOperator, groups: Vec<usize>, maximum: f64) -> Option<f64> {
        let mut pending: Vec<(&LogicalOperator, Vec<usize>)> = vec![(input, groups)];
        let mut result = 0.0;
        let mut found_union = false;
        while let Some((mut op, mut positions)) = pending.pop() {
            while let K::Projection { expressions, .. } = &op.kind {
                let bindings = op.children[0].column_bindings();
                let mapped: Option<Vec<usize>> = positions
                    .iter()
                    .map(|&at| {
                        let expr = expressions.get(at)?;
                        // through the compression of a column too
                        let column = if keeps_values_apart(expr) { &expr.children()[0] } else { expr };
                        match &column.kind {
                            BoundExprKind::ColumnRef { binding, depth: 0 } => bindings.iter().position(|b| b == binding),
                            _ => None,
                        }
                    })
                    .collect();
                let Some(mapped) = mapped else {
                    break;
                };
                op = &op.children[0];
                positions = mapped;
            }
            if matches!(op.kind, K::Union { .. }) {
                found_union = true;
                for child in &op.children {
                    pending.push((child, positions.clone()));
                }
                continue;
            }
            if !found_union {
                return None;
            }
            let stats = self.stats(op);
            let bindings = op.column_bindings();
            let cardinality = stats.cardinality;
            let mut estimate = cardinality.min(1.0);
            for at in positions {
                let domain = bindings.get(at).map_or(1.0, |b| stats.current(b)).max(1.0);
                if estimate > (cardinality / domain).floor() {
                    estimate = cardinality;
                    break;
                }
                estimate *= domain;
            }
            result += estimate.min(maximum - result);
        }
        Some(result)
    }

    /// The estimate of a tree of inner joins from all its relations and predicates, as the join
    /// order optimizer has it for each set of relations (`EstimateCardinality`).
    fn tree_cardinality(&self, op: &LogicalOperator) -> Option<f64> {
        fn collect(op: &LogicalOperator, relations: &mut Vec<LogicalOperator>, conjuncts: &mut Vec<BoundExpr>) {
            match &op.kind {
                K::Join {
                    conditions, predicate, ..
                } if is_binary_tree(op) => {
                    conjuncts.extend(conditions.iter().cloned().map(condition_expression));
                    if let Some(predicate) = predicate {
                        split_conjuncts(predicate.clone(), conjuncts);
                    }
                }
                K::CrossProduct => {}
                // the projection maps of the joins, which C++ adds once it has its estimates
                K::ColumnSelect { .. } if is_binary_tree(&op.children[0]) => {}
                K::Filter { expressions } if is_tree(op) && is_binary_tree(&op.children[0]) => {
                    for expr in expressions {
                        split_conjuncts(expr.clone(), conjuncts);
                    }
                }
                _ => {
                    relations.push(op.clone());
                    return;
                }
            }
            for child in &op.children {
                collect(child, relations, conjuncts);
            }
        }
        if !is_binary_tree(op) {
            return None;
        }
        let mut relations = Vec::new();
        let mut conjuncts = Vec::new();
        collect(op, &mut relations, &mut conjuncts);
        if relations.len() < 2 || relations.len() > MAX_RELATIONS {
            return None;
        }
        let mut relation_of = HashMap::new();
        for (at, relation) in relations.iter().enumerate() {
            for binding in relation.column_bindings() {
                relation_of.insert(binding, at);
            }
        }
        let set_of = |expr: &BoundExpr| -> Option<u64> {
            let mut columns = Vec::new();
            expression_columns(expr, &mut columns);
            columns
                .iter()
                .try_fold(0u64, |set, c| Some(set | 1 << *relation_of.get(c)?))
        };
        let mut on_relation: Vec<Vec<BoundExpr>> = vec![Vec::new(); relations.len()];
        let mut predicates = Vec::new();
        for expr in conjuncts {
            let set = set_of(&expr).unwrap_or(0);
            if set == 0 {
                continue;
            } else if set.count_ones() == 1 {
                on_relation[set.trailing_zeros() as usize].push(expr);
            } else {
                let sides = match &expr.kind {
                    BoundExprKind::Comparison { left, right, .. } => set_of(left).zip(set_of(right)),
                    _ => None,
                };
                let sides = sides.filter(|(l, r)| *l != 0 && *r != 0 && l & r == 0);
                predicates.push(Predicate {
                    expr,
                    set,
                    sides,
                    class: None,
                });
            }
        }
        add_implied_equalities(&mut predicates, &relation_of);
        let stats: Vec<RelationStats> = relations
            .into_iter()
            .zip(on_relation)
            .map(|(relation, expressions)| {
                if expressions.is_empty() {
                    self.stats(&relation)
                } else {
                    self.stats(&LogicalOperator::new(K::Filter { expressions }, vec![relation]))
                }
            })
            .collect();
        let all = (1u64 << stats.len()) - 1;
        Some(Estimator::new(&stats, &predicates, &relation_of).estimate(all))
    }

    /// Remembers the size of what `DelimGet`s of a `DelimJoin` read, once its left side is known.
    fn note_delim(&mut self, op: &LogicalOperator) {
        if let K::MaterializedCte { data, .. } = &op.kind {
            let stats = self.stats(&op.children[0]);
            let bindings = op.children[0].column_bindings();
            self.ctes.insert(Arc::as_ptr(data) as *const () as usize, (bindings, stats));
            return;
        }
        if let K::RecursiveCte { working, .. } = &op.kind {
            let stats = self.stats(&op.children[0]);
            let bindings = op.children[0].column_bindings();
            self.ctes.insert(Arc::as_ptr(working) as *const () as usize, (bindings, stats));
            return;
        }
        let K::DelimJoin { delim, conditions, .. } = &op.kind else {
            return;
        };
        let stats = self.stats(&op.children[0]);
        let keys = conditions
            .iter()
            .filter_map(|c| Some(stats.current(&bare_column(&c.left)?)))
            .fold(1.0, f64::max)
            .min(stats.cardinality);
        self.delim.insert(Arc::as_ptr(delim) as *const () as usize, keys);
    }

    /// `JoinOrderOptimizer::Optimize` for a plan. With `by_position` something above reads the
    /// columns of `op` in order instead of by binding, so they stay where they are.
    fn optimize(&mut self, mut op: LogicalOperator, by_position: bool) -> LogicalOperator {
        if !by_position && is_tree(&op) {
            let mut relations = Vec::new();
            leaves(&op, &mut relations);
            let mut bindings: Vec<ColumnBinding> = relations.iter().flat_map(|r| r.column_bindings()).collect();
            let columns = bindings.len();
            bindings.sort_by_key(|b| (b.table_index, b.column_index));
            bindings.dedup();
            if (2..=MAX_RELATIONS).contains(&relations.len()) && bindings.len() == columns {
                return self.reorder(op);
            }
        }
        if !by_position && semi_chain_length(&op) >= 2 {
            return self.reorder_semi_chain(op);
        }
        let inherited = by_position
            && matches!(
                op.kind,
                K::Filter { .. }
                    | K::Order { .. }
                    | K::TopN { .. }
                    | K::Limit { .. }
                    | K::LimitPercent { .. }
                    | K::Sample { .. }
            );
        let positional = inherited
            || matches!(
                op.kind,
                K::Union { .. }
                    | K::ExceptIntersect { .. }
                    | K::RecursiveCte { .. }
                    | K::Insert { .. }
                    | K::Update { .. }
                    | K::Delete { .. }
                    | K::MergeInto { .. }
                    | K::CreateTableAs { .. }
                    | K::Distinct { .. }
            );
        if matches!(op.kind, K::DelimJoin { .. }) {
            let right = op.children.pop().expect("a join has two sides");
            let left = op.children.pop().expect("a join has two sides");
            op.children.push(self.optimize(left, false));
            op.children.push(right);
            self.note_delim(&op);
            let right = op.children.pop().expect("a join has two sides");
            op.children.push(self.optimize(right, false));
            return op;
        }
        op.children = std::mem::take(&mut op.children)
            .into_iter()
            .map(|child| self.optimize(child, positional))
            .collect();
        if let K::LateralJoin { right, .. } = &mut op.kind {
            let plan = std::mem::replace(&mut **right, LogicalOperator::new(K::CrossProduct, Vec::new()));
            **right = self.optimize(plan, false);
        }
        op
    }

    /// Takes the operators of a tree off its relations, gathering the conjuncts on them and the
    /// SEMI and ANTI joins, which keep their right side.
    fn extract(
        &mut self,
        mut op: LogicalOperator,
        relations: &mut Vec<LogicalOperator>,
        conjuncts: &mut Vec<BoundExpr>,
        semis: &mut Vec<(Vec<ColumnBinding>, LogicalOperator)>,
    ) {
        if !is_tree(&op) {
            relations.push(self.optimize(op, false));
            return;
        }
        if is_movable_semi(&op) {
            let columns = left_columns(&op);
            let right = op.children.pop().expect("a join has two sides");
            let left = op.children.pop().expect("a join has two sides");
            op.children.push(self.optimize(right, false));
            semis.push((columns, op));
            self.extract(left, relations, conjuncts, semis);
            return;
        }
        match op.kind {
            K::Filter { expressions } => {
                for expr in expressions {
                    split_conjuncts(expr, conjuncts);
                }
            }
            K::Join {
                conditions, predicate, ..
            } => {
                conjuncts.extend(conditions.into_iter().map(condition_expression));
                if let Some(predicate) = predicate {
                    split_conjuncts(predicate, conjuncts);
                }
            }
            _ => {}
        }
        for child in op.children {
            self.extract(child, relations, conjuncts, semis);
        }
    }

    /// Two or more SEMI and ANTI joins on one relation that is not a tree of joins: they filter
    /// it each by its own columns, so any order gives the same rows, and the plan enumerator, for
    /// which their right sides are relations, picks the cheapest. Build sides were chosen already;
    /// they are chosen again for the new order.
    fn reorder_semi_chain(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let mut semis = Vec::new();
        while semi_chain_length(&op) >= 1 {
            unflip_semi(&mut op);
            let right = op.children.pop().expect("a join has two sides");
            let left = op.children.pop().expect("a join has two sides");
            op.children.push(self.optimize(right, false));
            semis.push(op);
            op = left;
        }
        let relation = self.optimize(op, false);
        semis.reverse();
        let mut plan = self.stack_semis(relation, semis);
        super::build_side::choose_build_sides(&mut plan, false);
        plan
    }

    /// Puts the SEMI and ANTI joins of one relation on it, in the order the plan enumerator would
    /// join them in: the least rows in all, and of orders as good, the one with the most rows on
    /// the right side of the last join (`EmitPair`'s tiebreaker).
    fn stack_semis(&self, relation: LogicalOperator, semis: Vec<LogicalOperator>) -> LogicalOperator {
        let build = |relation: LogicalOperator, order: &[usize], semis: &[LogicalOperator]| {
            let mut op = relation;
            for &at in order {
                let mut semi = semis[at].clone();
                semi.children.insert(0, op);
                op = semi;
            }
            op
        };
        if semis.len() < 2 || semis.len() > 4 {
            let order: Vec<usize> = (0..semis.len()).collect();
            return build(relation, &order, &semis);
        }
        fn permutations(items: Vec<usize>) -> Vec<Vec<usize>> {
            if items.len() <= 1 {
                return vec![items];
            }
            let mut result = Vec::new();
            for at in 0..items.len() {
                let mut rest = items.clone();
                let first = rest.remove(at);
                for mut tail in permutations(rest) {
                    tail.insert(0, first);
                    result.push(tail);
                }
            }
            result
        }
        let right_rows: Vec<f64> = semis.iter().map(|semi| self.stats(&semi.children[0]).cardinality).collect();
        let mut best: Option<(f64, f64, Vec<usize>)> = None;
        for order in permutations((0..semis.len()).collect()) {
            let mut cost = 0.0;
            for depth in 1..=order.len() {
                cost += self.stats(&build(relation.clone(), &order[..depth], &semis)).cardinality;
            }
            let right = right_rows[*order.last().expect("two or more")];
            let better = match &best {
                None => true,
                Some((best_cost, best_right, _)) => cost < *best_cost || (cost == *best_cost && right > *best_right),
            };
            if better {
                best = Some((cost, right, order));
            }
        }
        let (_, _, order) = best.expect("two or more");
        build(relation, &order, &semis)
    }

    fn reorder(&mut self, op: LogicalOperator) -> LogicalOperator {
        let mut relations = Vec::new();
        let mut conjuncts = Vec::new();
        let mut semis = Vec::new();
        self.extract(op, &mut relations, &mut conjuncts, &mut semis);
        let mut relation_of = HashMap::new();
        for (at, relation) in relations.iter().enumerate() {
            for binding in relation.column_bindings() {
                relation_of.insert(binding, at);
            }
        }
        let set_of = |expr: &BoundExpr| -> Option<u64> {
            let mut columns = Vec::new();
            expression_columns(expr, &mut columns);
            columns
                .iter()
                .try_fold(0u64, |set, c| Some(set | 1 << *relation_of.get(c)?))
        };
        // what reads no relation, or something else, stays on top of the tree
        let mut above = Vec::new();
        let mut on_relation: Vec<Vec<BoundExpr>> = vec![Vec::new(); relations.len()];
        let mut predicates = Vec::new();
        for expr in conjuncts {
            let set = set_of(&expr).unwrap_or(0);
            if set == 0 {
                above.push(expr);
            } else if set.count_ones() == 1 {
                on_relation[set.trailing_zeros() as usize].push(expr);
            } else {
                let sides = match &expr.kind {
                    BoundExprKind::Comparison { left, right, .. } => set_of(left).zip(set_of(right)),
                    _ => None,
                };
                let sides = sides.filter(|(l, r)| *l != 0 && *r != 0 && l & r == 0);
                predicates.push(Predicate {
                    expr,
                    set,
                    sides,
                    class: None,
                });
            }
        }
        add_implied_equalities(&mut predicates, &relation_of);
        let mut relations: Vec<LogicalOperator> = relations
            .into_iter()
            .zip(on_relation)
            .map(|(relation, expressions)| {
                if expressions.is_empty() {
                    relation
                } else {
                    LogicalOperator::new(K::Filter { expressions }, vec![relation])
                }
            })
            .collect();
        // innermost first, as they were
        let mut stacks: Vec<Vec<LogicalOperator>> = (0..relations.len()).map(|_| Vec::new()).collect();
        for (columns, semi) in semis.into_iter().rev() {
            stacks[relation_of[&columns[0]]].push(semi);
        }
        for (at, stack) in stacks.into_iter().enumerate() {
            let relation = std::mem::replace(&mut relations[at], LogicalOperator::new(K::CrossProduct, Vec::new()));
            relations[at] = self.stack_semis(relation, stack);
        }
        let stats: Vec<RelationStats> = relations.iter().map(|r| self.stats(r)).collect();
        let mut estimator = Estimator::new(&stats, &predicates, &relation_of);
        let mut edges = Vec::new();
        for (left, right) in predicates.iter().filter_map(|p| p.sides) {
            edges.push((left, right));
            edges.push((right, left));
        }
        edges.sort_unstable();
        edges.dedup();
        let mut plans = HashMap::new();
        for at in 0..relations.len() {
            let node = JoinNode {
                left: 0,
                right: 0,
                cost: 0.0,
            };
            plans.insert(1u64 << at, node);
        }
        let mut enumerator = Enumerator {
            relations: relations.len(),
            edges,
            estimator: &mut estimator,
            plans,
            pairs: 0,
        };
        enumerator.solve();
        let plans = enumerator.plans;
        let all = (1u64 << relations.len()) - 1;
        let mut tree = Tree {
            relations: relations.into_iter().map(Some).collect(),
            plans,
            predicates: predicates.into_iter().map(Some).collect(),
            estimator,
        };
        let plan = tree.build(all);
        if above.is_empty() {
            return plan;
        }
        LogicalOperator::new(K::Filter { expressions: above }, vec![plan])
    }
}

/// Whether a filter of a scan is `x IS NOT NULL` for a column that has no NULLs, so keeps all rows.
fn is_proven_not_null(filter: &BoundExpr, table: &Arc<TableCatalogEntry>, table_index: usize, ids: &[usize]) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::IsNotNull,
        children,
    } = &filter.kind
    else {
        return false;
    };
    let Some(column) = children.first().and_then(bare_column) else {
        return false;
    };
    if column.table_index != table_index {
        return false;
    }
    let Some(statistics) = table_statistics(table) else {
        return false;
    };
    let id = ids.get(column.column_index).copied();
    id.and_then(|id| statistics.get(id)).is_some_and(|s| !s.can_be_null)
}

/// `FilterFraction`: `fraction` of `cardinality`, at least one row and at most all of them.
fn filter_fraction(cardinality: f64, fraction: f64) -> f64 {
    if cardinality <= 0.0 {
        return 0.0;
    }
    (cardinality * fraction).floor().clamp(1.0, cardinality.max(1.0))
}

/// `EstimateFilterRows` for one conjunct: the rows it keeps, and whether it is modeled.
fn conjunct_rows(cardinality: f64, conjunct: &BoundExpr, child: &RelationStats) -> (f64, bool) {
    match &conjunct.kind {
        BoundExprKind::Constant(value) if conjunct.return_type.id() == duckrs_common::types::LogicalTypeId::Boolean => {
            let keeps = *value == Value::boolean(true);
            (if keeps { cardinality } else { 0.0 }, true)
        }
        // `EstimateNullCheckRows`
        BoundExprKind::Operator {
            op: op @ (BoundOperator::IsNull | BoundOperator::IsNotNull),
            children,
        } if children.len() == 1 && bare_column(&children[0]).is_some() => {
            let fraction = if *op == BoundOperator::IsNull {
                DEFAULT_SELECTIVITY
            } else {
                1.0 - DEFAULT_SELECTIVITY
            };
            (filter_fraction(cardinality, fraction), true)
        }
        // `EstimateEqualityRows`
        BoundExprKind::Comparison {
            op: op @ (ComparisonOp::Equal | ComparisonOp::NotEqual),
            left,
            right,
        } => {
            let column = match (&left.kind, &right.kind) {
                (BoundExprKind::Constant(value), _) if !Value::is_null(value) => bare_column(right),
                (_, BoundExprKind::Constant(value)) if !Value::is_null(value) => bare_column(left),
                _ => None,
            };
            let Some(column) = column else {
                return (cardinality, false);
            };
            let equal = *op == ComparisonOp::Equal;
            if cardinality <= 0.0 {
                return (0.0, true);
            }
            let stats = child.column(&column);
            match (stats.total.source != CountSource::Cardinality).then_some(stats.current.count) {
                None => (
                    filter_fraction(
                        cardinality,
                        if equal {
                            DEFAULT_SELECTIVITY
                        } else {
                            1.0 - DEFAULT_SELECTIVITY
                        },
                    ),
                    true,
                ),
                Some(current) => {
                    let domain = current.min(cardinality);
                    if domain <= 0.0 {
                        return (cardinality, false);
                    }
                    if equal {
                        ((cardinality / domain).ceil(), true)
                    } else {
                        (filter_fraction(cardinality, 1.0 - 1.0 / domain), true)
                    }
                }
            }
        }
        _ => (cardinality, false),
    }
}

/// `IntegerRangeEstimate`: the values of an integer column of at most 64 bits the range comparisons
/// with constants of one filter leave of those between its minimum and its maximum.
#[derive(Default)]
struct IntegerRange {
    bounds: Option<(i128, i128, i128)>,
}

impl IntegerRange {
    /// `AddBound`: false where the comparison says nothing of the range.
    fn add_bound(&mut self, op: ComparisonOp, constant: &Value, ty: &LogicalType, column: &ColumnStatistics) -> bool {
        use duckrs_common::types::LogicalTypeId as T;
        if constant.is_null()
            || constant.logical_type() != ty
            || !matches!(
                ty.id(),
                T::TinyInt | T::SmallInt | T::Integer | T::BigInt | T::UTinyInt | T::USmallInt | T::UInteger | T::UBigInt
            )
        {
            return false;
        }
        let (Some(min), Some(max)) = (column.min.as_ref().and_then(Value::as_i128), column.max.as_ref().and_then(Value::as_i128))
        else {
            return false;
        };
        let Some(value) = constant.as_i128() else {
            return false;
        };
        if self.bounds.is_none() {
            if min > max {
                return false;
            }
            // a complete 64-bit interval has 2^64 values
            self.bounds = Some((min, max, max - min + 1));
        }
        let (lower, upper, _) = self.bounds.as_mut().expect("just set");
        match op {
            ComparisonOp::LessThan => *upper = (*upper).min(value - 1),
            ComparisonOp::LessThanOrEqual => *upper = (*upper).min(value),
            ComparisonOp::GreaterThan => *lower = (*lower).max(value + 1),
            ComparisonOp::GreaterThanOrEqual => *lower = (*lower).max(value),
            _ => return false,
        }
        true
    }

    /// `Fraction`.
    fn fraction(&self) -> Option<f64> {
        let (lower, upper, count) = self.bounds?;
        Some((upper - lower + 1).max(0) as f64 / count as f64)
    }
}

/// `ExpressionFilter::CheckExpressionStatistics` of a filter on a column with `column` statistics.
#[derive(PartialEq)]
enum FilterPropagate {
    NoPruning,
    AlwaysTrue,
    TrueOrNull,
    AlwaysFalse,
}

fn check_filter_statistics(filter: &BoundExpr, column: &ColumnStatistics) -> FilterPropagate {
    match &filter.kind {
        BoundExprKind::Operator { op, children } if children.len() == 1 && strict_column(&children[0]).is_some() => {
            let (null, valid) = (column.can_be_null, column.can_be_valid);
            match (op, null, valid) {
                (BoundOperator::IsNull, false, _) | (BoundOperator::IsNotNull, _, false) => FilterPropagate::AlwaysFalse,
                (BoundOperator::IsNull, true, false) | (BoundOperator::IsNotNull, false, true) => FilterPropagate::AlwaysTrue,
                _ => FilterPropagate::NoPruning,
            }
        }
        BoundExprKind::Comparison { op, left, right } => {
            let (op, constant) = match (strict_column(left), &right.kind, strict_column(right), &left.kind) {
                (Some(_), BoundExprKind::Constant(value), _, _) => (*op, (value, &right.return_type)),
                (_, _, Some(_), BoundExprKind::Constant(value)) => (flip(*op), (value, &left.return_type)),
                _ => return FilterPropagate::NoPruning,
            };
            if constant.0.is_null() || matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
                return FilterPropagate::NoPruning;
            }
            if !column.can_be_valid {
                return FilterPropagate::AlwaysFalse;
            }
            // `CheckComparisonStatistics`: with NULLs only an answer of false stands
            match super::statistics::decide_with_constant(op, column, constant.0, constant.1) {
                Some(false) => FilterPropagate::AlwaysFalse,
                Some(true) if !column.can_be_null => FilterPropagate::AlwaysTrue,
                _ => FilterPropagate::NoPruning,
            }
        }
        _ => FilterPropagate::NoPruning,
    }
}

/// `FilterColumn`: the column `expr` is, itself.
fn strict_column(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        _ => None,
    }
}

fn flip(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// `EstimateFilterRows` with the statistics of the scanned column and its integer range.
fn scan_conjunct_rows(
    cardinality: f64,
    conjunct: &BoundExpr,
    child: &RelationStats,
    column: Option<(&ColumnStatistics, &LogicalType)>,
    range: &mut IntegerRange,
) -> (f64, bool) {
    // a BETWEEN is its two bounds, as `GenerateTableScanFilters` pushes them
    if let BoundExprKind::Between { input, lower, upper } = &conjunct.kind {
        let bound = |op: ComparisonOp, value: &BoundExpr| {
            BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: input.clone(),
                    right: Box::new(value.clone()),
                },
                LogicalType::BOOLEAN,
            )
        };
        let both = BoundExpr::new(
            BoundExprKind::Conjunction {
                and: true,
                children: vec![bound(ComparisonOp::GreaterThanOrEqual, lower), bound(ComparisonOp::LessThanOrEqual, upper)],
            },
            LogicalType::BOOLEAN,
        );
        return scan_conjunct_rows(cardinality, &both, child, column, range);
    }
    if let BoundExprKind::Conjunction { and: true, children } = &conjunct.kind {
        let mut result = (cardinality, true);
        for child_expr in children {
            let (rows, modeled) = scan_conjunct_rows(cardinality, child_expr, child, column, range);
            result = (result.0.min(rows), result.1 && modeled);
        }
        return result;
    }
    let mut pruning = FilterPropagate::NoPruning;
    if let Some((statistics, _)) = column {
        pruning = check_filter_statistics(conjunct, statistics);
        match pruning {
            FilterPropagate::AlwaysTrue => return (cardinality, true),
            FilterPropagate::AlwaysFalse => return (0.0, true),
            _ => {}
        }
    }
    let BoundExprKind::Comparison { op, left, right } = &conjunct.kind else {
        return conjunct_rows(cardinality, conjunct, child);
    };
    // `EstimateComparisonRows`
    let (op, value) = match (strict_column(left), strict_column(right)) {
        (Some(_), _) => (*op, right),
        (None, Some(_)) => (flip(*op), left),
        _ => return (cardinality, false),
    };
    let BoundExprKind::Constant(constant) = &value.kind else {
        return (cardinality, false);
    };
    if constant.is_null() {
        return match op {
            ComparisonOp::NotDistinctFrom => (filter_fraction(cardinality, DEFAULT_SELECTIVITY), true),
            ComparisonOp::DistinctFrom => (filter_fraction(cardinality, 1.0 - DEFAULT_SELECTIVITY), true),
            _ => (0.0, true),
        };
    }
    if pruning == FilterPropagate::TrueOrNull && !matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
        return (filter_fraction(cardinality, 1.0 - DEFAULT_SELECTIVITY), true);
    }
    match op {
        ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual | ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual => {
            if let Some((statistics, ty)) = column
                && range.add_bound(op, constant, ty, statistics)
            {
                return (cardinality, true);
            }
            (filter_fraction(cardinality, DEFAULT_SELECTIVITY), true)
        }
        _ => conjunct_rows(cardinality, conjunct, child),
    }
}

/// `ExtractGetStats`: the rows the filters of a scan leave, each estimated with the statistics of
/// its column, and the range comparisons on one column together.
fn scan_filter_cardinality(
    table: &Arc<TableCatalogEntry>,
    table_index: usize,
    column_ids: &[usize],
    filters: &[BoundExpr],
    stats: &RelationStats,
) -> f64 {
    let base = stats.cardinality;
    let statistics = super::statistics::table_statistics(table);
    // the filters of each column, in the order of their first
    let mut groups: Vec<(Option<usize>, Vec<&BoundExpr>)> = Vec::new();
    for filter in filters {
        fn first_column(expr: &BoundExpr, table_index: usize) -> Option<usize> {
            match &expr.kind {
                BoundExprKind::ColumnRef { binding, depth: 0 } if binding.table_index == table_index => {
                    Some(binding.column_index)
                }
                _ => expr.children().into_iter().find_map(|child| first_column(child, table_index)),
            }
        }
        let position = first_column(filter, table_index);
        match groups.iter_mut().find(|(at, _)| *at == position) {
            Some((_, members)) => members.push(filter),
            None => groups.push((position, vec![filter])),
        }
    }
    let mut cardinality = base;
    let mut supported = true;
    for (position, members) in groups {
        let column = position.and_then(|at| column_ids.get(at)).copied().filter(|&c| c != COLUMN_IDENTIFIER_ROW_ID);
        let column_stats = column.and_then(|c| Some((statistics.as_ref()?.get(c)?, &table.columns.get(c)?.ty)));
        let mut range = IntegerRange::default();
        for filter in members {
            let (rows, modeled) = scan_conjunct_rows(base, filter, stats, column_stats, &mut range);
            cardinality = cardinality.min(rows);
            supported &= modeled;
        }
        // intersect the bounds on one column before the floor of the default selectivity
        if let Some(fraction) = range.fraction() {
            let rows = if fraction == 0.0 {
                0.0
            } else {
                filter_fraction(base, fraction.max(DEFAULT_SELECTIVITY))
            };
            cardinality = cardinality.min(rows);
        }
    }
    if !supported {
        cardinality = cardinality.min(filter_fraction(base, DEFAULT_SELECTIVITY));
    }
    cardinality
}

/// How many rows filters leave of `child` (`EstimateFilterRows` and `ApplyFilterEstimate`): the
/// smallest estimate of a conjunct, and the default selectivity when one is not modeled.
fn filter_cardinality(expressions: &[BoundExpr], child: &RelationStats) -> f64 {
    let mut conjuncts = Vec::new();
    for expr in expressions {
        split_conjuncts(expr.clone(), &mut conjuncts);
    }
    let mut cardinality = child.cardinality;
    let mut supported = true;
    for conjunct in &conjuncts {
        let (rows, modeled) = scan_conjunct_rows(child.cardinality, conjunct, child, None, &mut IntegerRange::default());
        cardinality = cardinality.min(rows);
        supported &= modeled;
    }
    if !supported {
        cardinality = cardinality.min(filter_fraction(child.cardinality, DEFAULT_SELECTIVITY));
    }
    cardinality
}

/// The estimated rows of each operator of a plan, for EXPLAIN (`estimated_cardinality`).
#[derive(Debug)]
pub struct PlanEstimate {
    pub cardinality: u64,
    pub children: Vec<PlanEstimate>,
}

fn estimate(op: &LogicalOperator, optimizer: &mut JoinOrderOptimizer) -> PlanEstimate {
    let mut children = Vec::with_capacity(op.children.len());
    for (at, child) in op.children.iter().enumerate() {
        children.push(estimate(child, optimizer));
        if at == 0 {
            optimizer.note_delim(op);
        }
    }
    // the recursive part, a round of it over the rows of the anchor, as a second child
    if let K::RecursiveCte {
        working, unresolved, ..
    } = &op.kind
    {
        let anchor = optimizer.stats(&op.children[0]);
        let mut inner = optimizer.clone();
        inner.ctes.insert(
            Arc::as_ptr(working) as *const () as usize,
            (op.children[0].column_bindings(), anchor),
        );
        children.push(estimate(unresolved, &mut inner));
    }
    PlanEstimate {
        // `CardinalityFromDouble`: truncated, and a row for what is not nothing
        cardinality: {
            let rows = optimizer.stats(op).cardinality.max(0.0);
            if rows > 0.0 { rows.floor().max(1.0) as u64 } else { 0 }
        },
        children,
    }
}

/// The estimated rows of one operator (`estimated_cardinality`), truncated as C++ keeps them.
pub fn estimated_cardinality(op: &LogicalOperator) -> u64 {
    let rows = JoinOrderOptimizer::default().stats(op).cardinality.max(0.0);
    if rows > 0.0 { rows.floor().max(1.0) as u64 } else { 0 }
}

/// `RelationStatsExtractor::Extract`: the statistics of the rows of `op`.
pub(super) fn relation_stats(op: &LogicalOperator) -> RelationStats {
    JoinOrderOptimizer::default().stats(op)
}

/// `LogicalOperator::EstimateCardinality` for all operators of a plan.
pub fn estimate_plan(op: &LogicalOperator) -> PlanEstimate {
    estimate(op, &mut JoinOrderOptimizer::default())
}

/// Reorders the trees of joins of `plan`, whose output stays in order at the top.
pub fn reorder_joins(plan: LogicalOperator) -> LogicalOperator {
    JoinOrderOptimizer::default().optimize(plan, true)
}
