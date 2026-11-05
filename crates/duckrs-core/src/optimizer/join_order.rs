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
//! LEFT, SEMI and ANTI joins are relations here, with their sides optimized apart, where DuckDB
//! also moves them with conflict detection. The cardinality estimator is reduced to total domains
//! per equivalence class and DuckDB's selectivities, without the caps for composite keys.

use super::conjunction_of;
use super::filter_pushdown::condition_expression;
use super::rewriter::split_conjuncts;
use super::statistics::table_distinct_counts;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::HashMap;
use std::sync::Arc;

/// `approximate_join_order_threshold`: from here on the order is picked greedily.
const APPROXIMATE_THRESHOLD: usize = 12;
/// Past this many pairs DPhyp gives up.
const MAX_PAIRS: usize = 10_000;
/// A set of relations is a mask, so there can be this many.
const MAX_RELATIONS: usize = 63;
/// `RelationStatisticsHelper::DEFAULT_SELECTIVITY`.
const DEFAULT_SELECTIVITY: f64 = 0.2;
/// What a recursive CTE round is assumed to hold.
const UNKNOWN_CARDINALITY: f64 = 1000.0;

/// `RelationStats`: the estimated rows of a plan, and the distinct values of its columns.
#[derive(Debug, Clone, Default)]
struct RelationStats {
    cardinality: f64,
    distinct: HashMap<ColumnBinding, f64>,
}

impl RelationStats {
    fn scaled(mut self, cardinality: f64) -> Self {
        self.cardinality = cardinality;
        for distinct in self.distinct.values_mut() {
            *distinct = distinct.min(cardinality.max(1.0));
        }
        self
    }
}

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

/// Whether `op` is an operator of a tree of joins, or filters on such a tree.
fn is_tree(op: &LogicalOperator) -> bool {
    match &op.kind {
        K::Filter { expressions } => !expressions.iter().any(BoundExpr::is_volatile) && is_tree(&op.children[0]),
        _ => is_binary_tree(op),
    }
}

/// The relations of a tree, whose operators `is_tree` accepts.
fn leaves<'a>(op: &'a LogicalOperator, result: &mut Vec<&'a LogicalOperator>) {
    if is_tree(op) {
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

/// `CardinalityEstimator`, for one tree.
struct Estimator {
    cardinalities: Vec<f64>,
    /// per equivalence class: the relations in it and its total domain
    classes: Vec<(u64, f64)>,
    /// per predicate outside of the classes: its relations and what it divides the size by
    denominators: Vec<(u64, f64)>,
    estimates: HashMap<u64, f64>,
}

impl Estimator {
    fn new(relations: &[RelationStats], predicates: &[Predicate], relation_of: &HashMap<ColumnBinding, usize>) -> Self {
        let distinct_of = |binding: &ColumnBinding| {
            relation_of.get(binding).map(|&relation| {
                let stats = &relations[relation];
                stats
                    .distinct
                    .get(binding)
                    .copied()
                    .unwrap_or(stats.cardinality)
                    .max(1.0)
            })
        };
        let domain_of = |expr: &BoundExpr| {
            let mut columns = Vec::new();
            expression_columns(expr, &mut columns);
            columns.iter().filter_map(distinct_of).reduce(f64::max)
        };
        // the columns that equalities make equal, as a disjoint set
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
        let mut denominators = Vec::new();
        for predicate in predicates {
            if predicate.set.count_ones() < 2 {
                continue;
            }
            if let BoundExprKind::Comparison { op, left, right } = &predicate.expr.kind {
                let domain = [domain_of(left), domain_of(right)]
                    .into_iter()
                    .flatten()
                    .reduce(f64::max)
                    .unwrap_or(1.0);
                match op {
                    ComparisonOp::Equal | ComparisonOp::NotDistinctFrom => {
                        if let (Some(l), Some(r)) = (bare_column(left), bare_column(right))
                            && relation_of.contains_key(&l)
                            && relation_of.contains_key(&r)
                        {
                            let (l, r) = (find(&mut parents, l), find(&mut parents, r));
                            parents.insert(l, r);
                        } else {
                            denominators.push((predicate.set, domain));
                        }
                    }
                    ComparisonOp::NotEqual | ComparisonOp::DistinctFrom => {
                        if domain > 1.0 {
                            denominators.push((predicate.set, domain / (domain - 1.0)));
                        }
                    }
                    _ => denominators.push((predicate.set, domain.powf(2.0 / 3.0))),
                }
            } else {
                denominators.push((predicate.set, 1.0 / DEFAULT_SELECTIVITY));
            }
        }
        let mut classes: HashMap<ColumnBinding, (u64, f64)> = HashMap::new();
        for binding in parents.keys().copied().collect::<Vec<_>>() {
            let root = find(&mut parents, binding);
            let class = classes.entry(root).or_insert((0, 1.0));
            class.0 |= 1 << relation_of[&binding];
            class.1 = class.1.max(distinct_of(&binding).unwrap_or(1.0));
        }
        let mut classes: Vec<_> = classes.into_values().collect();
        classes.sort_by_key(|class| class.0);
        Self {
            cardinalities: relations.iter().map(|r| r.cardinality).collect(),
            classes,
            denominators,
            estimates: HashMap::new(),
        }
    }

    /// `EstimateCardinalityWithSet`: the product of the relations over what the predicates remove.
    fn estimate(&mut self, set: u64) -> f64 {
        if let Some(&estimate) = self.estimates.get(&set) {
            return estimate;
        }
        let mut numerator = 1.0;
        for (relation, cardinality) in self.cardinalities.iter().enumerate() {
            if set & (1 << relation) != 0 {
                numerator *= cardinality;
            }
        }
        let mut denominator = 1.0;
        for &(relations, domain) in &self.classes {
            let joined = (relations & set).count_ones();
            if joined > 1 {
                denominator *= domain.powi(joined as i32 - 1);
            }
        }
        for &(relations, by) in &self.denominators {
            if relations & !set == 0 {
                denominator *= by;
            }
        }
        let estimate = numerator / denominator;
        self.estimates.insert(set, estimate);
        estimate
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
        if self.relations >= APPROXIMATE_THRESHOLD || !self.solve_exactly() || !self.plans.contains_key(&all) {
            self.solve_approximately();
        }
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
        for slot in &mut self.predicates {
            let applies = slot
                .as_ref()
                .is_some_and(|p| subset(p.set, set) && !subset(p.set, left_set) && !subset(p.set, right_set));
            if !applies {
                continue;
            }
            let predicate = slot.take().expect("just looked at it");
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
                        null_safe: op == ComparisonOp::NotDistinctFrom,
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
            },
            vec![left, right],
        )
    }
}

/// `JoinOrderOptimizer`.
#[derive(Default)]
struct JoinOrderOptimizer {
    /// the size of what a `DelimGet` reads, by the rows it shares with its `DelimJoin`
    delim: HashMap<usize, f64>,
}

impl JoinOrderOptimizer {
    /// `RelationStatisticsHelper`, without the optimizer in between.
    fn stats(&self, op: &LogicalOperator) -> RelationStats {
        match &op.kind {
            K::Get {
                table,
                table_index,
                column_ids,
            } => {
                let Some(counts) = table_distinct_counts(table) else {
                    return RelationStats {
                        cardinality: UNKNOWN_CARDINALITY,
                        distinct: HashMap::new(),
                    };
                };
                let distinct = column_ids
                    .iter()
                    .enumerate()
                    .filter_map(|(at, &column)| {
                        let count = if column == COLUMN_IDENTIFIER_ROW_ID {
                            Some(counts.rows)
                        } else {
                            counts.columns.get(column).copied().flatten()
                        };
                        Some((ColumnBinding::new(*table_index, at), count? as f64))
                    })
                    .collect();
                RelationStats {
                    cardinality: counts.rows as f64,
                    distinct,
                }
            }
            K::Range { count, .. } => RelationStats {
                cardinality: *count as f64,
                distinct: HashMap::new(),
            },
            K::EmptyResult { .. } => RelationStats::default(),
            K::DummyScan { .. } => RelationStats {
                cardinality: 1.0,
                distinct: HashMap::new(),
            },
            K::ExpressionGet { expressions, .. } => RelationStats {
                cardinality: expressions.len() as f64,
                distinct: HashMap::new(),
            },
            K::DelimGet { delim, .. } => RelationStats {
                cardinality: self
                    .delim
                    .get(&(Arc::as_ptr(delim) as *const () as usize))
                    .copied()
                    .unwrap_or(UNKNOWN_CARDINALITY),
                distinct: HashMap::new(),
            },
            K::WorkingTable { .. } | K::RecursiveCte { .. } => RelationStats {
                cardinality: UNKNOWN_CARDINALITY,
                distinct: HashMap::new(),
            },
            K::Filter { expressions } => {
                let child = self.stats(&op.children[0]);
                let cardinality = filter_cardinality(expressions, &child);
                child.scaled(cardinality)
            }
            K::Projection {
                table_index,
                expressions,
            } => {
                let child = self.stats(&op.children[0]);
                let distinct = expressions
                    .iter()
                    .enumerate()
                    .filter_map(|(at, expr)| {
                        let source = child.distinct.get(&bare_column(expr)?)?;
                        Some((ColumnBinding::new(*table_index, at), *source))
                    })
                    .collect();
                RelationStats {
                    cardinality: child.cardinality,
                    distinct,
                }
            }
            K::Aggregate {
                group_index, groups, ..
            } => {
                let child = self.stats(&op.children[0]);
                if groups.is_empty() {
                    return RelationStats {
                        cardinality: 1.0,
                        distinct: HashMap::new(),
                    };
                }
                let mut distinct = HashMap::new();
                let mut cardinality: f64 = 1.0;
                for (at, group) in groups.iter().enumerate() {
                    let count = bare_column(group)
                        .and_then(|column| child.distinct.get(&column).copied())
                        .unwrap_or(child.cardinality);
                    cardinality = cardinality.max(count);
                    distinct.insert(ColumnBinding::new(*group_index, at), count);
                }
                RelationStats {
                    cardinality: cardinality.min(child.cardinality),
                    distinct,
                }
            }
            K::Limit { limit, .. } => {
                let child = self.stats(&op.children[0]);
                let cardinality = limit.map_or(child.cardinality, |limit| child.cardinality.min(limit as f64));
                child.scaled(cardinality)
            }
            K::Union { .. } => RelationStats {
                cardinality: op.children.iter().map(|c| self.stats(c).cardinality).sum(),
                distinct: HashMap::new(),
            },
            K::CrossProduct => {
                let (mut left, right) = (self.stats(&op.children[0]), self.stats(&op.children[1]));
                left.cardinality *= right.cardinality;
                left.distinct.extend(right.distinct);
                left
            }
            K::Join { join_type, .. } => {
                let (mut left, right) = (self.stats(&op.children[0]), self.stats(&op.children[1]));
                match join_type {
                    JoinType::Semi | JoinType::Anti => {
                        let cardinality = left.cardinality * DEFAULT_SELECTIVITY;
                        left.scaled(cardinality)
                    }
                    _ => {
                        left.cardinality = left.cardinality.max(right.cardinality);
                        left.distinct.extend(right.distinct);
                        left
                    }
                }
            }
            _ if !op.children.is_empty() => self.stats(&op.children[0]),
            _ => RelationStats {
                cardinality: UNKNOWN_CARDINALITY,
                distinct: HashMap::new(),
            },
        }
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
        let inherited = by_position
            && matches!(
                op.kind,
                K::Filter { .. } | K::Order { .. } | K::Limit { .. } | K::LimitPercent { .. } | K::Sample { .. }
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
        if let K::DelimJoin { delim, conditions, .. } = &op.kind {
            let key = Arc::as_ptr(delim) as *const () as usize;
            let conditions: Vec<BoundExpr> = conditions.iter().map(|c| c.left.clone()).collect();
            let mut children = std::mem::take(&mut op.children).into_iter();
            let left = self.optimize(children.next().expect("a join has two sides"), false);
            let stats = self.stats(&left);
            let keys = conditions
                .iter()
                .filter_map(|c| stats.distinct.get(&bare_column(c)?).copied())
                .fold(1.0, f64::max)
                .min(stats.cardinality);
            self.delim.insert(key, keys);
            let right = self.optimize(children.next().expect("a join has two sides"), false);
            op.children = vec![left, right];
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

    /// Takes the operators of a tree off its relations, gathering the conjuncts on them.
    fn extract(&mut self, op: LogicalOperator, relations: &mut Vec<LogicalOperator>, conjuncts: &mut Vec<BoundExpr>) {
        if !is_tree(&op) {
            relations.push(self.optimize(op, false));
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
            self.extract(child, relations, conjuncts);
        }
    }

    fn reorder(&mut self, op: LogicalOperator) -> LogicalOperator {
        let mut relations = Vec::new();
        let mut conjuncts = Vec::new();
        self.extract(op, &mut relations, &mut conjuncts);
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
                predicates.push(Predicate { expr, set, sides });
            }
        }
        let relations: Vec<LogicalOperator> = relations
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

/// How many rows filters leave of `child`: an equality with a constant keeps a value's worth of
/// them, and anything else the default selectivity, once.
fn filter_cardinality(expressions: &[BoundExpr], child: &RelationStats) -> f64 {
    let mut conjuncts = Vec::new();
    for expr in expressions {
        split_conjuncts(expr.clone(), &mut conjuncts);
    }
    let mut cardinality = child.cardinality;
    let mut other = false;
    for conjunct in &conjuncts {
        let equal_constant = match &conjunct.kind {
            BoundExprKind::Comparison {
                op: ComparisonOp::Equal,
                left,
                right,
            } => match (&left.kind, &right.kind) {
                (BoundExprKind::Constant(value), _) if !Value::is_null(value) => bare_column(right),
                (_, BoundExprKind::Constant(value)) if !Value::is_null(value) => bare_column(left),
                _ => None,
            },
            _ => None,
        };
        match equal_constant.and_then(|column| child.distinct.get(&column)) {
            Some(distinct) => cardinality /= distinct.max(1.0),
            None => other = true,
        }
    }
    if other {
        cardinality *= DEFAULT_SELECTIVITY;
    }
    cardinality
}

/// Reorders the trees of joins of `plan`, whose output stays in order at the top.
pub fn reorder_joins(plan: LogicalOperator) -> LogicalOperator {
    JoinOrderOptimizer::default().optimize(plan, true)
}
