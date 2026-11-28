// Ported from duckdb/src/optimizer/common_subplan_optimizer.cpp @ rust-port-baseline
//
//! `CommonSubplanOptimizer`: subplans that occur more than once in a plan (a view or a CTE that
//! a query reads twice) run once, as a materialized CTE that each occurrence scans.
//!
//! C++ compares subplans by serializing each operator with its table indexes made canonical, and
//! holds references to the slots of the plan. Here an operator's canonical form is its `Debug`
//! text with the same renaming, and subplans are found again by their path of child positions,
//! which the rewrites keep up to date.
//!
//! Not ported yet: combining the states of ungrouped aggregates. Subqueries have plans of their own here, so
//! identical subqueries are not shared.

use super::join_order::{PlanEstimate, estimate_plan};
use super::rewriter::{aggregate_can_throw, can_throw, contains_barrier};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use parking_lot::Mutex;
use pintail_common::column_data::ColumnDataCollection;
use pintail_common::types::LogicalType;
use pintail_parser::ast::JoinType;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

type Path = Vec<usize>;

/// Stand-ins for the table indexes an operator defines while its canonical form is made.
const PLACEHOLDER: usize = 1 << 60;
/// Stand-ins for the tables of an operator's inputs, in the order its children output them.
const INPUT: usize = 1 << 59;

/// `CommonSubplanOptimizer::Optimize`.
pub fn optimize(mut plan: LogicalOperator) -> LogicalOperator {
    if contains_dml(&plan) {
        return plan;
    }
    let mut next_index = max_table_index(&plan) + 1;
    CommonJoinFilters::default().normalize(&mut plan);
    let mut finder = Finder::default();
    finder.find(&mut plan);
    finder.filter(&mut plan);

    finder.convert(&mut plan, &mut next_index);
    plan
}

fn contains_dml(op: &LogicalOperator) -> bool {
    matches!(
        op.kind,
        K::Insert { .. } | K::Delete { .. } | K::Update { .. } | K::MergeInto { .. }
    ) || op.children.iter().any(contains_dml)
}

pub(super) fn node<'a>(root: &'a LogicalOperator, path: &[usize]) -> &'a LogicalOperator {
    path.iter().fold(root, |op, &i| &op.children[i])
}

pub(super) fn node_mut<'a>(root: &'a mut LogicalOperator, path: &[usize]) -> &'a mut LogicalOperator {
    path.iter().fold(root, |op, &i| &mut op.children[i])
}

/// `LogicalOperator::GetTableIndex`: the table indexes the operator's own columns have.
fn defined_indexes(kind: &K) -> Vec<usize> {
    let mut kind = kind.clone();
    defined_indexes_mut(&mut kind).into_iter().map(|i| *i).collect()
}

pub(super) fn defined_indexes_mut(kind: &mut K) -> Vec<&mut usize> {
    match kind {
        K::Get { table_index, .. }
        | K::Range { table_index, .. }
        | K::CsvScan { table_index, .. }
        | K::JsonScan { table_index, .. }
        | K::ParquetScan { table_index, .. }
        | K::DummyScan { table_index }
        | K::ChunkGet { table_index, .. }
        | K::ExpressionGet { table_index, .. }
        | K::Window { table_index, .. }
        | K::MatchRecognize { table_index, .. }
        | K::Unnest { table_index, .. }
        | K::Union { table_index, .. }
        | K::ExceptIntersect { table_index, .. }
        | K::RecursiveCte { table_index, .. }
        | K::WorkingTable { table_index, .. }
        | K::DelimGet { table_index, .. }
        | K::Projection { table_index, .. }
        | K::MarkJoin { table_index, .. }
        | K::MaterializedCte { table_index, .. }
        | K::CteRef { table_index, .. } => vec![table_index],
        K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index,
            ..
        } => vec![group_index, aggregate_index, groupings_index],
        K::DelimJoin { mark_index, .. } => mark_index.iter_mut().collect(),
        _ => Vec::new(),
    }
}

/// Runs `f` on an operator that holds `kind`, and puts the kind back.
fn with_operator<R>(kind: &mut K, f: impl FnOnce(&mut LogicalOperator) -> R) -> R {
    let mut op = LogicalOperator::new(std::mem::replace(kind, K::CrossProduct), Vec::new());
    let result = f(&mut op);
    *kind = op.kind;
    result
}

/// Calls `f` on each column reference of the operator's expressions (and of a `ColumnSelect`).
pub(super) fn visit_bindings(kind: &mut K, f: &mut impl FnMut(&mut ColumnBinding, usize)) {
    if let K::ColumnSelect { keep } = kind {
        for binding in keep {
            f(binding, 0);
        }
        return;
    }
    with_operator(kind, |op| {
        let mut visit = |expr: &mut BoundExpr| {
            expr.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth } = &mut e.kind {
                    f(binding, *depth);
                }
            })
        };
        match &mut op.kind {
            K::Join {
                conditions, predicate, ..
            }
            | K::DelimJoin {
                conditions, predicate, ..
            }
            | K::MarkJoin {
                conditions, predicate, ..
            } => {
                for condition in conditions {
                    visit(&mut condition.left);
                    visit(&mut condition.right);
                }
                predicate.iter_mut().for_each(visit);
            }
            K::AsOfJoin { conditions, .. } => {
                for condition in conditions {
                    visit(&mut condition.left);
                    visit(&mut condition.right);
                }
                for expr in op.expressions_mut() {
                    visit(expr);
                }
            }
            _ => {
                for expr in op.expressions_mut() {
                    visit(expr);
                }
            }
        }
    });
}

/// Whether some expression of the operator is `f`.
fn any_expression(kind: &K, f: &impl Fn(&BoundExpr) -> bool) -> bool {
    let mut kind = kind.clone();
    let mut found = false;
    with_operator(&mut kind, |op| {
        if let K::Join { conditions, .. } | K::DelimJoin { conditions, .. } | K::MarkJoin { conditions, .. } = &op.kind
        {
            found |= conditions.iter().any(|c| f(&c.left) || f(&c.right));
        }
        if let K::AsOfJoin { conditions, .. } = &op.kind {
            found |= conditions.iter().any(|c| f(&c.left) || f(&c.right));
        }
        found |= op.expressions_mut().into_iter().any(|e| f(e));
    });
    found
}

fn expression_any(expr: &BoundExpr, f: &impl Fn(&BoundExpr) -> bool) -> bool {
    f(expr) || expr.children().into_iter().any(|c| expression_any(c, f))
}

/// The highest table index of the plan, of its subqueries too.
pub(super) fn max_table_index(op: &LogicalOperator) -> usize {
    let mut max = defined_indexes(&op.kind).into_iter().max().unwrap_or(0);
    let mut kind = op.kind.clone();
    visit_bindings(&mut kind, &mut |binding, _| max = max.max(binding.table_index));
    let mut subqueries = Vec::new();
    collect_subqueries(&op.kind, &mut subqueries);
    for plan in subqueries {
        max = max.max(max_table_index(&plan));
    }
    for child in &op.children {
        max = max.max(max_table_index(child));
    }
    if let K::RecursiveCte { recursive, .. } | K::LateralJoin { right: recursive, .. } = &op.kind {
        max = max.max(max_table_index(recursive));
    }
    max
}

fn collect_subqueries(kind: &K, plans: &mut Vec<LogicalOperator>) {
    let mut kind = kind.clone();
    with_operator(&mut kind, |op| {
        for expr in op.expressions_mut() {
            expr.walk_mut(&mut |e| {
                if let BoundExprKind::Subquery { subquery, .. } = &e.kind {
                    plans.push(subquery.plan.clone());
                }
            });
        }
    });
}

/// `CanShareSubplanExpression`.
fn can_share_expression(expr: &BoundExpr) -> bool {
    !expr.is_volatile()
        && !can_throw(expr)
        && !contains_barrier(expr)
        && !expression_any(expr, &|e| {
            matches!(e.kind, BoundExprKind::ColumnRef { depth, .. } if depth != 0)
                || matches!(e.kind, BoundExprKind::Subquery { .. })
        })
}

/// `CTEInlining::EndsInAggregateOrDistinct`.
pub(super) fn ends_in_aggregate_or_distinct(op: &LogicalOperator) -> bool {
    match op.kind {
        K::Aggregate { .. } | K::Distinct { .. } | K::Window { .. } => true,
        _ => op.children.len() == 1 && ends_in_aggregate_or_distinct(&op.children[0]),
    }
}

/// The scans and the operators with expressions under the columns of an operator.
#[derive(Default)]
struct Sources {
    scans: Vec<Path>,
    expressions: Vec<Path>,
}

/// `GetExpandedColumnBindings`: the columns an operator can pass on, as if no operator below it
/// dropped columns, with where they come from, by their path from `op`.
fn expanded_bindings(op: &LogicalOperator, path: &mut Path, sources: &mut Option<&mut Sources>) -> Vec<ColumnBinding> {
    let child = |at: usize, path: &mut Path, sources: &mut Option<&mut Sources>| {
        path.push(at);
        let result = expanded_bindings(&op.children[at], path, sources);
        path.pop();
        result
    };
    match &op.kind {
        K::Filter { .. }
        | K::Order { .. }
        | K::Limit { .. }
        | K::TopN { .. }
        | K::Distinct { .. }
        | K::ColumnSelect { .. } => child(0, path, sources),
        K::Window { table_index, windows } => {
            let mut bindings = child(0, path, sources);
            bindings.extend((0..windows.len()).map(|i| ColumnBinding::new(*table_index, i)));
            bindings
        }
        K::Unnest {
            table_index,
            expressions,
        } => {
            let mut bindings = child(0, path, sources);
            bindings.extend((0..expressions.len()).map(|i| ColumnBinding::new(*table_index, i)));
            bindings
        }
        K::Join { join_type, .. } | K::AsOfJoin { join_type, .. } => match join_type {
            JoinType::RightSemi | JoinType::RightAnti => child(1, path, sources),
            JoinType::Semi | JoinType::Anti => child(0, path, sources),
            _ => {
                let mut bindings = child(0, path, sources);
                bindings.extend(child(1, path, sources));
                bindings
            }
        },
        K::MarkJoin { table_index, .. } => {
            let mut bindings = child(0, path, sources);
            bindings.push(ColumnBinding::new(*table_index, 0));
            bindings
        }
        K::CrossProduct | K::PositionalJoin => {
            let mut bindings = child(0, path, sources);
            bindings.extend(child(1, path, sources));
            bindings
        }
        K::Get { .. } => {
            if let Some(sources) = sources {
                sources.scans.push(path.clone());
            }
            op.column_bindings()
        }
        K::Projection { .. } | K::Aggregate { .. } => {
            if let Some(sources) = sources {
                sources.expressions.push(path.clone());
            }
            op.column_bindings()
        }
        _ => op.column_bindings(),
    }
}

/// An operator that is a subplan, and the columns it has in canonical terms.
struct Subplan {
    path: Path,
    canonical_bindings: Vec<ColumnBinding>,
}

/// `SubplanInfo`: the occurrences of one subplan.
struct SubplanInfo {
    canonical_table_indexes: Vec<usize>,
    subplans: Vec<Subplan>,
    lowest_common_ancestor: Path,
    operator_count: usize,
    removed: bool,
}

/// An output of an operator that can output more of them: an expression of a projection, or an
/// aggregate of an aggregate without groups.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum Expandable {
    Expression(BoundExpr),
    Aggregate(BoundAggregate),
}

impl Expandable {
    fn expressions_mut(&mut self) -> Vec<&mut BoundExpr> {
        match self {
            Expandable::Expression(expr) => vec![expr],
            Expandable::Aggregate(aggregate) => aggregate
                .children
                .iter_mut()
                .chain(aggregate.filter.iter_mut())
                .chain(aggregate.order_by.iter_mut().map(|(expr, _)| expr))
                .chain(aggregate.distinct_key.iter_mut())
                .collect(),
        }
    }

    fn visit_bindings(&mut self, f: &mut impl FnMut(&mut ColumnBinding)) {
        for expr in self.expressions_mut() {
            expr.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind {
                    f(binding);
                }
            });
        }
    }
}

/// The outputs of an operator that can output more of them, and the table index they have.
fn expandable_outputs(kind: &K) -> Option<(usize, Vec<Expandable>)> {
    match kind {
        K::Projection {
            table_index,
            expressions,
        } => Some((
            *table_index,
            expressions.iter().cloned().map(Expandable::Expression).collect(),
        )),
        K::Aggregate {
            aggregate_index,
            groups,
            grouping_sets,
            grouping_functions,
            aggregates,
            ..
        } if groups.is_empty() && grouping_sets.is_empty() && grouping_functions.is_empty() => Some((
            *aggregate_index,
            aggregates.iter().cloned().map(Expandable::Aggregate).collect(),
        )),
        _ => None,
    }
}

/// `CanCombineAggregateState`: aggregates whose states are of a fixed size, which one aggregate
/// can compute side by side without using more memory than computing them apart.
pub(super) fn can_combine_aggregate(aggregate: &BoundAggregate) -> bool {
    if aggregate.distinct || !aggregate.order_by.is_empty() {
        return false;
    }
    let fixed = |i: usize| {
        aggregate
            .children
            .get(i)
            .is_none_or(|c| c.return_type.physical_type().is_constant_size())
    };
    match aggregate.function.name.as_str() {
        "count_star" | "count" => true,
        "sum" | "sum_no_overflow" | "avg" | "mean" | "product" | "bool_and" | "bool_or" | "bit_and" | "bit_or"
        | "bit_xor" | "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" | "min" | "max"
        | "any_value" | "arbitrary" | "first" | "last" => fixed(0),
        _ => false,
    }
}

/// An expression whose operator can output more of them (`SubplanBindings::ExpressionSource`).
struct ExpressionSource {
    path: Path,
    output_table: usize,
    expressions: Vec<Expandable>,
    input: Box<Bindings>,
}

/// `SubplanBindings`: where an operator can get each canonical column from.
#[derive(Default)]
struct Bindings {
    bindings: HashMap<ColumnBinding, ColumnBinding>,
    /// by canonical table index; `None` where it occurs more than once
    scans: HashMap<usize, Option<Path>>,
    expression_sources: HashMap<usize, Option<ExpressionSource>>,
}

impl Bindings {
    fn contains(&self, binding: &ColumnBinding) -> bool {
        if self.bindings.contains_key(binding) || matches!(self.scans.get(&binding.table_index), Some(Some(_))) {
            return true;
        }
        let Some(Some(source)) = self.expression_sources.get(&binding.table_index) else {
            return false;
        };
        let Some(expression) = source.expressions.get(binding.column_index) else {
            return false;
        };
        let mut available = true;
        expression
            .clone()
            .visit_bindings(&mut |binding| available &= source.input.contains(binding));
        available
    }

    /// `Expose`: makes the operator at `root` output the canonical column `binding`, and returns
    /// its binding there.
    fn expose(&mut self, root: &mut LogicalOperator, binding: ColumnBinding) -> ColumnBinding {
        if let Some(found) = self.bindings.get(&binding) {
            return *found;
        }
        let result = if let Some(Some(path)) = self.scans.get(&binding.table_index) {
            let K::Get {
                table_index,
                column_ids,
                projection,
                ..
            } = &mut node_mut(root, path).kind
            else {
                unreachable!("a scan")
            };
            let position = match column_ids.iter().position(|&c| c == binding.column_index) {
                Some(position) => position,
                None => {
                    column_ids.push(binding.column_index);
                    column_ids.len() - 1
                }
            };
            // a projection prunes the columns of a scan; it does not reorder them. The binder's
            // `rowid` stays out of it.
            *projection = column_ids.contains(&COLUMN_IDENTIFIER_ROW_ID).then(|| {
                (0..column_ids.len())
                    .filter(|&p| column_ids[p] != COLUMN_IDENTIFIER_ROW_ID)
                    .collect()
            });
            ColumnBinding::new(*table_index, position)
        } else {
            let source = self
                .expression_sources
                .get_mut(&binding.table_index)
                .and_then(Option::as_mut)
                .expect("an expression source");
            let mut expression = source.expressions[binding.column_index].clone();
            let input = &mut source.input;
            expression.visit_bindings(&mut |binding| *binding = input.expose(root, *binding));
            let count = match (&mut node_mut(root, &source.path).kind, expression) {
                (K::Projection { expressions, .. }, Expandable::Expression(expression)) => {
                    expressions.push(expression);
                    expressions.len()
                }
                (K::Aggregate { aggregates, .. }, Expandable::Aggregate(aggregate)) => {
                    aggregates.push(aggregate);
                    aggregates.len()
                }
                _ => unreachable!("an operator with expandable outputs"),
            };
            ColumnBinding::new(source.output_table, count - 1)
        };
        self.bindings.insert(binding, result);
        result
    }
}

#[derive(Default)]
struct Finder {
    /// the signature of each operator, by its path
    signatures: HashMap<Path, usize>,
    /// signatures by their text
    interned: HashMap<String, usize>,
    /// by signature
    infos: HashMap<usize, SubplanInfo>,
    /// in the order they were found
    order: Vec<usize>,
    to_canonical: HashMap<usize, usize>,
    /// the canonical column of a column, by table index, where it is not the column itself
    column_maps: HashMap<usize, HashMap<usize, usize>>,
    /// the canonical columns of the columns of the CTE scans made so far
    generated: HashMap<ColumnBinding, ColumnBinding>,
    /// the outputs of operators that can output more of them, by canonical output table
    canonical_expressions: HashMap<usize, Vec<Expandable>>,
    min_cte_index: Option<usize>,
}

impl Finder {
    /// `FindCommonSubplans`.
    fn find(&mut self, plan: &mut LogicalOperator) {
        let mut root = Vec::new();
        while node(plan, &root).children.len() == 1 {
            root.push(0);
        }
        let start = node(plan, &root);
        for at in 0..start.children.len() {
            let mut path = root.clone();
            path.push(at);
            self.visit(plan, &mut path);
        }
    }

    fn visit(&mut self, plan: &mut LogicalOperator, path: &mut Path) {
        for at in 0..node(plan, path).children.len() {
            path.push(at);
            self.visit(plan, path);
            path.pop();
        }
        let Some(signature) = self.signature(node(plan, path), path) else {
            return;
        };
        self.signatures.insert(path.clone(), signature);
        let op = node(plan, path);
        if matches!(op.kind, K::ColumnSelect { .. }) {
            return;
        }
        let defined = defined_indexes(&op.kind);
        match self
            .infos
            .get(&signature)
            .map(|info| info.canonical_table_indexes.clone())
        {
            None => {
                let mut canonical = Vec::new();
                for index in defined {
                    let next = self.to_canonical.len();
                    canonical.push(next);
                    self.to_canonical.insert(index, next);
                }
                self.register_expression_bindings(op);
                let Some(bindings) = self.canonical_bindings(&op.column_bindings()) else {
                    return;
                };
                let operator_count = self.operator_count(op);
                self.infos.insert(
                    signature,
                    SubplanInfo {
                        canonical_table_indexes: canonical,
                        subplans: vec![Subplan {
                            path: path.clone(),
                            canonical_bindings: bindings,
                        }],
                        lowest_common_ancestor: path.clone(),
                        operator_count,
                        removed: false,
                    },
                );
                self.order.push(signature);
            }
            Some(canonical) => {
                for (index, canonical) in defined.into_iter().zip(canonical) {
                    self.to_canonical.insert(index, canonical);
                }
                self.register_expression_bindings(op);
                let Some(bindings) = self.canonical_bindings(&op.column_bindings()) else {
                    return;
                };
                let info = self.infos.get_mut(&signature).expect("found above");
                info.subplans.push(Subplan {
                    path: path.clone(),
                    canonical_bindings: bindings,
                });
                let common = info
                    .lowest_common_ancestor
                    .iter()
                    .zip(path.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                info.lowest_common_ancestor.truncate(common);
            }
        }
    }

    fn operator_count(&self, op: &LogicalOperator) -> usize {
        let own = usize::from(!matches!(op.kind, K::ColumnSelect { .. }));
        own + op.children.iter().map(|c| self.operator_count(c)).sum::<usize>()
    }

    /// `GetCanonicalBinding`.
    fn canonical_binding(&self, binding: &ColumnBinding) -> Option<ColumnBinding> {
        if let Some(canonical) = self.generated.get(binding) {
            return Some(*canonical);
        }
        let table = *self.to_canonical.get(&binding.table_index)?;
        let column = self
            .column_maps
            .get(&binding.table_index)
            .and_then(|map| map.get(&binding.column_index))
            .copied()
            .unwrap_or(binding.column_index);
        Some(ColumnBinding::new(table, column))
    }

    fn canonical_bindings(&self, bindings: &[ColumnBinding]) -> Option<Vec<ColumnBinding>> {
        bindings.iter().map(|b| self.canonical_binding(b)).collect()
    }

    /// `PlanSignature::OperatorIsSupported`.
    fn supported(op: &LogicalOperator) -> bool {
        match &op.kind {
            K::Projection { .. }
            | K::Filter { .. }
            | K::Aggregate { .. }
            | K::Window { .. }
            | K::Unnest { .. }
            | K::Limit { .. }
            | K::Order { .. }
            | K::TopN { .. }
            | K::Distinct { .. }
            | K::DummyScan { .. }
            | K::Join { .. }
            | K::MarkJoin { .. }
            | K::CrossProduct
            | K::PositionalJoin
            | K::AsOfJoin { .. }
            | K::Union { .. }
            | K::ExceptIntersect { .. }
            | K::Range { .. }
            | K::ColumnSelect { .. } => true,
            // C++ refuses scans of virtual columns; the binder scans `rowid` without outputting it
            // C++ pushes a sample into its scan (`SamplingPushdown`) only after this optimizer, so a
            // sample never shares here; a sample without a seed is not repeatable either
            // (`SampleIsRepeatable`)
            K::Get { sample: Some(_), .. } => false,
            K::Get {
                index_rows,
                column_ids,
                projection,
                ..
            } => {
                // a scan of only the row id is one of no column (`GetAnyColumn`), which only counts rows
                let rowid = match projection {
                    Some(positions) if positions.len() == 1 => false,
                    Some(positions) => positions.iter().any(|&p| column_ids[p] == COLUMN_IDENTIFIER_ROW_ID),
                    None => column_ids.contains(&COLUMN_IDENTIFIER_ROW_ID),
                };
                index_rows.is_none() && !rowid
            }
            // the rows of a volatile table function differ between its calls
            K::ExpressionGet { function, volatile, .. } => {
                !*volatile
                    && function
                        .as_deref()
                        .is_none_or(|f| !crate::planner::expression::VOLATILE_TABLE_FUNCTIONS.contains(&f))
            }
            // `LogicalColumnDataGet`: not too many rows to compare
            K::ChunkGet { chunks, .. } => chunks.iter().map(|c| c.len()).sum::<usize>() < 1000,
            _ => false,
        }
    }

    /// `CanExpandExpressionOutputs`: whether occurrences of the operator that differ in their
    /// expressions are the same subplan, whose materialization outputs the expressions of all.
    fn can_expand(&self, op: &LogicalOperator) -> bool {
        let Some((_, outputs)) = expandable_outputs(&op.kind) else {
            return false;
        };
        let shareable = outputs.into_iter().all(|mut output| {
            let combinable = match &output {
                // C++ shares subplans before statistics propagation, which here has made a `sum` it proved can't
                // overflow a `sum_no_overflow`: that can't throw either
                Expandable::Aggregate(aggregate) => {
                    can_combine_aggregate(aggregate)
                        && (aggregate.proven_no_overflow
                            || !aggregate_can_throw(&aggregate.function, &aggregate.children))
                }
                Expandable::Expression(_) => true,
            };
            combinable && output.expressions_mut().into_iter().all(|e| can_share_expression(e))
        });
        if !shareable {
            return false;
        }
        let mut inputs: HashMap<usize, usize> = HashMap::new();
        for binding in expanded_bindings(&op.children[0], &mut Vec::new(), &mut None) {
            let Some(&canonical) = self.to_canonical.get(&binding.table_index) else {
                return false;
            };
            if *inputs.entry(canonical).or_insert(binding.table_index) != binding.table_index {
                // matching expressions must tell repeated occurrences of an input apart
                return false;
            }
        }
        true
    }

    /// `RegisterExpressionBindings`.
    fn register_expression_bindings(&mut self, op: &LogicalOperator) {
        if !self.can_expand(op) {
            return;
        }
        let Some((table_index, outputs)) = expandable_outputs(&op.kind) else {
            return;
        };
        let canonical_table = self.to_canonical[&table_index];
        let mut map = HashMap::new();
        let mut used = HashSet::new();
        for (at, mut expression) in outputs.into_iter().enumerate() {
            let mut ok = true;
            expression.visit_bindings(&mut |binding| match self.canonical_binding(binding) {
                Some(canonical) => *binding = canonical,
                None => ok = false,
            });
            if !ok {
                return;
            }
            let text = format!("{expression:?}");
            let known = self.canonical_expressions.entry(canonical_table).or_default();
            let index = (0..known.len())
                .find(|i| !used.contains(i) && format!("{:?}", known[*i]) == text)
                .unwrap_or_else(|| {
                    known.push(expression);
                    known.len() - 1
                });
            used.insert(index);
            map.insert(at, index);
        }
        self.column_maps.insert(table_index, map);
    }

    /// `CreatePlanSignature`: the operator's canonical form, with those of its children.
    fn signature(&mut self, op: &LogicalOperator, path: &Path) -> Option<usize> {
        let mut children = Vec::new();
        for at in 0..op.children.len() {
            let mut child = path.clone();
            child.push(at);
            children.push(*self.signatures.get(&child)?);
        }
        if !Self::supported(op) {
            return None;
        }
        // `Subquery` expressions run plans of their own, and volatile ones differ between runs
        if any_expression(&op.kind, &|e| {
            expression_any(e, &|e| {
                e.is_volatile()
                    || matches!(e.kind, BoundExprKind::Subquery { .. })
                    || matches!(e.kind, BoundExprKind::ColumnRef { depth, .. } if depth != 0)
            })
        }) {
            return None;
        }
        let expand = self.can_expand(op);
        let mut positional = Vec::new();
        if matches!(op.kind, K::Union { .. } | K::ExceptIntersect { .. }) {
            // these read their children by position, not by binding
            for child in &op.children {
                positional.push(self.canonical_bindings(&child.column_bindings())?);
            }
        }
        let local = self.canonical_form(op, expand)?;
        let text = format!("{local}|{positional:?}|{children:?}");
        let next = self.interned.len();
        Some(*self.interned.entry(text).or_insert(next))
    }

    /// The operator without its children, with its own table indexes made placeholders and the
    /// columns it reads canonical (`PlanSignatureTableIndexMap::Convert`).
    fn canonical_form(&mut self, op: &LogicalOperator, expand: bool) -> Option<String> {
        let mut kind = op.kind.clone();
        let defined = defined_indexes(&kind);
        if let K::Get {
            table_index,
            column_ids,
            ..
        } = &kind
        {
            // a scan's columns are the table's columns, whichever of them it reads
            let map: HashMap<usize, usize> = column_ids.iter().copied().enumerate().collect();
            self.column_maps.insert(*table_index, map);
        }
        // the tables of the inputs, numbered in the order the children output them, which tells
        // two occurrences of one input apart (`Initialize`)
        let mut inputs: HashMap<usize, usize> = HashMap::new();
        for child in &op.children {
            for binding in expanded_bindings(child, &mut Vec::new(), &mut None) {
                let next = inputs.len();
                inputs.entry(binding.table_index).or_insert(next);
            }
        }
        let column_of = |maps: &HashMap<usize, HashMap<usize, usize>>, binding: &ColumnBinding| {
            maps.get(&binding.table_index)
                .and_then(|m| m.get(&binding.column_index))
                .copied()
                .unwrap_or(binding.column_index)
        };
        let mut ok = true;
        visit_bindings(&mut kind, &mut |binding, _| {
            if let Some(&input) = inputs.get(&binding.table_index) {
                *binding = ColumnBinding::new(INPUT + input, column_of(&self.column_maps, binding));
            } else if let Some(at) = defined.iter().position(|&d| d == binding.table_index) {
                let column = self
                    .column_maps
                    .get(&binding.table_index)
                    .and_then(|m| m.get(&binding.column_index))
                    .copied()
                    .unwrap_or(binding.column_index);
                *binding = ColumnBinding::new(PLACEHOLDER + at, column);
            } else {
                ok = false;
            }
        });
        if !ok {
            return None;
        }
        for (at, index) in defined_indexes_mut(&mut kind).into_iter().enumerate() {
            *index = PLACEHOLDER + at;
        }
        match &mut kind {
            K::Get {
                table,
                filters,
                zone_filters,
                ..
            } => {
                return Some(format!(
                    "GET {}.{} {:p} {filters:?} {zone_filters:?}",
                    table.schema,
                    table.name,
                    Arc::as_ptr(table)
                ));
            }
            K::Join { conditions, .. } | K::MarkJoin { conditions, .. } => {
                for condition in conditions {
                    condition.filter = None;
                }
            }
            K::AsOfJoin { conditions, .. } => {
                for condition in conditions {
                    condition.filter = None;
                }
            }
            K::Projection { expressions, .. } if expand => expressions.clear(),
            K::Aggregate { aggregates, .. } if expand => aggregates.clear(),
            K::ChunkGet { chunks, types, .. } => {
                let rows: Vec<Vec<String>> = chunks
                    .iter()
                    .flat_map(|chunk| {
                        (0..chunk.len()).map(|row| {
                            (0..types.len())
                                .map(|c| format!("{:?}", chunk.get_value(c, row)))
                                .collect()
                        })
                    })
                    .collect();
                return Some(format!("CHUNK_GET {types:?} {rows:?}"));
            }
            _ => {}
        }
        Some(format!("{kind:?}"))
    }

    /// `GetAvailableBindings`, of the operator at `path` of `plan`.
    fn available_bindings(&self, plan: &LogicalOperator, path: &[usize]) -> Bindings {
        let op = node(plan, path);
        let mut sources = Sources::default();
        let mut relative = Vec::new();
        let expanded = expanded_bindings(op, &mut relative, &mut Some(&mut sources));
        let mut result = Bindings::default();
        for binding in expanded {
            if let Some(canonical) = self.canonical_binding(&binding) {
                result.bindings.insert(canonical, binding);
            }
        }
        for scan in sources.scans {
            let get = node(op, &scan);
            let K::Get { table_index, .. } = &get.kind else {
                continue;
            };
            let Some(&table) = self.to_canonical.get(table_index) else {
                continue;
            };
            let mut full = path.to_vec();
            full.extend(&scan);
            result
                .scans
                .entry(table)
                .and_modify(|e| *e = None)
                .or_insert(Some(full));
        }
        for source in sources.expressions {
            let source_op = node(op, &source);
            let Some((table_index, _)) = expandable_outputs(&source_op.kind) else {
                continue;
            };
            let table_index = &table_index;
            let Some(&table) = self.to_canonical.get(table_index) else {
                continue;
            };
            let Some(expressions) = self.canonical_expressions.get(&table) else {
                continue;
            };
            let mut full = path.to_vec();
            full.extend(&source);
            let mut input_path = full.clone();
            input_path.push(0);
            let entry = ExpressionSource {
                path: full,
                output_table: *table_index,
                expressions: expressions.clone(),
                input: Box::new(self.available_bindings(plan, &input_path)),
            };
            match result.expression_sources.entry(table) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.insert(None);
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(Some(entry));
                }
            }
        }
        result
    }

    /// `SelectPrimarySubplan`: puts first an occurrence that can output all columns that any
    /// occurrence has.
    fn select_primary(&self, plan: &LogicalOperator, info: &mut SubplanInfo) -> bool {
        let required: HashSet<ColumnBinding> = info
            .subplans
            .iter()
            .flat_map(|s| s.canonical_bindings.iter().copied())
            .collect();
        let mut primary: Option<(usize, usize)> = None;
        for (at, subplan) in info.subplans.iter().enumerate() {
            let available = self.available_bindings(plan, &subplan.path);
            if !required.iter().all(|b| available.contains(b)) {
                continue;
            }
            let count = subplan.canonical_bindings.len();
            if primary.is_none_or(|(_, best)| count > best) {
                primary = Some((at, count));
            }
        }
        let Some((at, _)) = primary else {
            return false;
        };
        info.subplans.swap(0, at);
        true
    }

    /// `ShouldMaterialize`.
    fn should_materialize(plan: &LogicalOperator, info: &SubplanInfo) -> bool {
        let subplan = node(plan, &info.subplans[0].path);
        if ends_in_aggregate_or_distinct(subplan) {
            return true;
        }
        let estimate = estimate_plan(subplan);
        is_selective_multi_table_plan(&estimate, estimate.cardinality)
    }

    /// `FilterSubplans`.
    fn filter(&mut self, plan: &mut LogicalOperator) {
        let mut removed = Vec::new();
        for &signature in &self.order {
            let info = &self.infos[&signature];
            if info.operator_count == 1 || info.subplans.len() == 1 {
                removed.push(signature);
                continue;
            }
            let first = &info.subplans[0].path;
            if let Some((_, parent)) = first.split_last().map(|(last, rest)| (last, rest.to_vec()))
                && let Some(parent_signature) = self.signatures.get(&parent)
                && let Some(parent_info) = self.infos.get(parent_signature)
            {
                let count = info.subplans.len() * info.operator_count;
                let parent_count = parent_info.subplans.len() * parent_info.operator_count;
                if parent_info.subplans.len() != 1
                    && parent_count >= count
                    && Self::should_materialize(plan, parent_info)
                {
                    // the parent is better, this one is redundant
                    removed.push(signature);
                    continue;
                }
            }
            // a subplan with a column twice holds another occurrence of a subplan
            let duplicate = info.subplans.iter().any(|s| {
                let mut seen = HashSet::new();
                !s.canonical_bindings.iter().all(|b| seen.insert(*b))
            });
            if duplicate {
                removed.push(signature);
            }
        }
        for signature in removed {
            self.infos.get_mut(&signature).expect("an info").removed = true;
        }
        let candidates: Vec<usize> = self.order.iter().copied().filter(|s| !self.infos[s].removed).collect();
        for signature in candidates {
            let mut info = self.infos.remove(&signature).expect("an info");
            if !self.select_primary(plan, &mut info) {
                info.removed = true;
            }
            self.infos.insert(signature, info);
        }
    }

    /// `ConvertSubplansToCTEs`.
    fn convert(&mut self, plan: &mut LogicalOperator, next_index: &mut usize) {
        let mut sorted: Vec<usize> = self.order.iter().copied().filter(|s| !self.infos[s].removed).collect();
        sorted.sort_by_key(|s| self.infos[s].operator_count);
        let mut cte_number = 1;
        for signature in sorted {
            let mut info = self.infos.remove(&signature).expect("an info");
            if !Self::should_materialize(plan, &info) || !self.select_primary(plan, &mut info) {
                continue;
            }
            let mut fresh = || {
                let index = *next_index;
                *next_index += 1;
                index
            };
            let cte_index = fresh();
            let cte_name = format!("__common_subplan_{cte_number}");
            cte_number += 1;
            self.min_cte_index.get_or_insert(cte_index);

            let old: Vec<(Vec<ColumnBinding>, Vec<LogicalType>)> = info
                .subplans
                .iter()
                .map(|s| {
                    let op = node(plan, &s.path);
                    (op.column_bindings(), op.types())
                })
                .collect();
            let mut required = Vec::new();
            let mut seen = HashSet::new();
            for subplan in &info.subplans {
                for binding in &subplan.canonical_bindings {
                    if seen.insert(*binding) {
                        required.push(*binding);
                    }
                }
            }
            let primary_path = info.subplans[0].path.clone();
            let mut available = self.available_bindings(plan, &primary_path);
            let materialized: Vec<ColumnBinding> = required
                .iter()
                .map(|binding| available.expose(plan, *binding))
                .collect();
            let primary = node_mut(plan, &primary_path);
            pass_all_columns(primary);
            let (bindings, all_types) = (primary.column_bindings(), primary.types());
            let types: Vec<LogicalType> = materialized
                .iter()
                .map(|b| all_types[bindings.iter().position(|x| x == b).expect("an exposed column")].clone())
                .collect();
            let names: Vec<String> = (1..=types.len()).map(|i| format!("{cte_name}_col_{i}")).collect();
            let position: HashMap<ColumnBinding, usize> = required.iter().enumerate().map(|(i, b)| (*b, i)).collect();
            let data = Arc::new(Mutex::new(ColumnDataCollection::new(types.clone())));

            // the CTE scans, and what each occurrence's columns become
            let mut replacements: Vec<(ColumnBinding, ColumnBinding)> = Vec::new();
            let mut scans = Vec::new();
            for (at, subplan) in info.subplans.iter().enumerate() {
                let columns: Vec<usize> = subplan.canonical_bindings.iter().map(|b| position[b]).collect();
                let reorder = columns.len() != types.len() || columns.iter().enumerate().any(|(i, &c)| c != i);
                let ref_index = fresh();
                let mut scan = LogicalOperator::new(
                    K::CteRef {
                        table_index: ref_index,
                        cte_index,
                        types: types.clone(),
                        names: names.clone(),
                        data: data.clone(),
                    },
                    Vec::new(),
                );
                if reorder {
                    let expressions = columns
                        .iter()
                        .map(|&c| {
                            BoundExpr::new(
                                BoundExprKind::ColumnRef {
                                    binding: ColumnBinding::new(ref_index, c),
                                    depth: 0,
                                },
                                types[c].clone(),
                            )
                        })
                        .collect();
                    scan = LogicalOperator::new(
                        K::Projection {
                            table_index: fresh(),
                            expressions,
                        },
                        vec![scan],
                    );
                }
                let new = scan.column_bindings();
                debug_assert_eq!(new.len(), old[at].0.len());
                for (i, binding) in new.iter().enumerate() {
                    replacements.push((old[at].0[i], *binding));
                    self.generated.insert(*binding, subplan.canonical_bindings[i]);
                }
                scans.push(scan);
            }

            // the occurrences become CTE scans, and the CTE goes in at their lowest common ancestor
            let lca = info.lowest_common_ancestor.clone();
            let mut primary_op = None;
            for (subplan, scan) in info.subplans.iter().zip(scans) {
                let slot = node_mut(plan, &subplan.path);
                let replaced = std::mem::replace(slot, scan);
                primary_op.get_or_insert(replaced);
            }
            let expressions = materialized
                .iter()
                .zip(&types)
                .map(|(binding, ty)| {
                    BoundExpr::new(
                        BoundExprKind::ColumnRef {
                            binding: *binding,
                            depth: 0,
                        },
                        ty.clone(),
                    )
                })
                .collect();
            let definition = LogicalOperator::new(
                K::Projection {
                    table_index: fresh(),
                    expressions,
                },
                vec![primary_op.expect("a primary subplan")],
            );
            let slot = node_mut(plan, &lca);
            let rest = std::mem::replace(slot, LogicalOperator::new(K::CrossProduct, Vec::new()));
            *slot = LogicalOperator::new(
                K::MaterializedCte {
                    name: cte_name,
                    table_index: cte_index,
                    data,
                    always: false,
                    never: false,
                },
                vec![definition, rest],
            );
            self.update_paths(|path| {
                if path.len() > primary_path.len() && path.starts_with(&primary_path) {
                    let mut moved = lca.clone();
                    moved.extend([0, 0]);
                    moved.extend(&path[primary_path.len()..]);
                    *path = moved;
                } else if path.len() > lca.len() && path.starts_with(&lca) {
                    path.insert(lca.len(), 1);
                }
            });

            // replace the columns of the occurrences, except in the CTE's definition
            let map: HashMap<ColumnBinding, ColumnBinding> = replacements.into_iter().collect();
            let mut definition_path = lca.clone();
            definition_path.push(0);
            replace_bindings(plan, &mut Vec::new(), &definition_path, &map);

            // C++ needs the CTEs this makes in the order it makes them, for its pipelines
            let mut current = lca;
            loop {
                let min = self.min_cte_index.expect("set above");
                let cte = node_mut(plan, &current);
                let K::MaterializedCte { .. } = &cte.kind else { break };
                let below = &cte.children[1];
                let K::MaterializedCte {
                    table_index: child_index,
                    ..
                } = below.kind
                else {
                    break;
                };
                if !(min..cte_index).contains(&child_index) {
                    break;
                }
                let mut child = cte.children.remove(1);
                let grandchild = child.children.remove(1);
                cte.children.push(grandchild);
                let outer = std::mem::replace(cte, LogicalOperator::new(K::CrossProduct, Vec::new()));
                child.children.push(outer);
                *cte = child;
                let at = current.clone();
                self.update_paths(|path| swap_paths(path, &at));
                current.push(1);
            }
        }
    }

    fn update_paths(&mut self, mut f: impl FnMut(&mut Path)) {
        for info in self.infos.values_mut() {
            for subplan in &mut info.subplans {
                f(&mut subplan.path);
            }
            f(&mut info.lowest_common_ancestor);
        }
    }
}

/// Where the slots of `New{def_new, Old{def_old, rest}}` at `at` are once it is
/// `Old{def_old, New{def_new, rest}}`.
fn swap_paths(path: &mut Path, at: &[usize]) {
    if path.len() <= at.len() || !path.starts_with(at) {
        return;
    }
    let tail = path[at.len()..].to_vec();
    let mut result = at.to_vec();
    match tail.as_slice() {
        [0, rest @ ..] => {
            result.extend([1, 0]);
            result.extend(rest);
        }
        [1] => result.extend([1, 1]),
        [1, 0, rest @ ..] => {
            result.push(0);
            result.extend(rest);
        }
        [1, 1] => result.push(1),
        _ => return,
    }
    *path = result;
}

/// `ClearProjectionMaps`: makes each `ColumnSelect` of the plan pass on all columns.
fn pass_all_columns(op: &mut LogicalOperator) {
    for child in &mut op.children {
        pass_all_columns(child);
    }
    if let K::ColumnSelect { keep } = &mut op.kind {
        *keep = op.children[0].column_bindings();
    }
}

/// `ColumnBindingReplacer`, everywhere but under `skip`.
fn replace_bindings(
    op: &mut LogicalOperator,
    path: &mut Path,
    skip: &[usize],
    map: &HashMap<ColumnBinding, ColumnBinding>,
) {
    if path.as_slice() == skip {
        return;
    }
    for at in 0..op.children.len() {
        path.push(at);
        replace_bindings(&mut op.children[at], path, skip, map);
        path.pop();
    }
    visit_bindings(&mut op.kind, &mut |binding, depth| {
        if depth == 0
            && let Some(new) = map.get(binding)
        {
            *binding = *new;
        }
    });
}

/// `IsSelectiveMultiTablePlan`, on the estimates of a plan.
fn is_selective_multi_table_plan(estimate: &PlanEstimate, cardinality: u64) -> bool {
    const CARDINALITY_THRESHOLD: u64 = 2048;
    const CARDINALITY_RATIO: u64 = 2;
    fn stats(estimate: &PlanEstimate) -> (usize, u64) {
        if estimate.children.is_empty() {
            return (1, estimate.cardinality);
        }
        estimate
            .children
            .iter()
            .map(stats)
            .fold((0, 0), |(count, max), (c, m)| (count + c, max.max(m)))
    }
    let (base_tables, max_cardinality) = stats(estimate);
    if base_tables < 2 {
        return false;
    }
    cardinality <= CARDINALITY_THRESHOLD || cardinality < max_cardinality / CARDINALITY_RATIO
}

/// A tree of inner joins whose scans' filters could differ from those of another (`Candidate`).
struct JoinCandidate {
    path: Path,
    /// the scans whose filters the signature leaves out, in the order it reaches them
    scans: Vec<Path>,
}

/// `CandidateGroup`: candidates that differ in the filters of the scan at `differing_scan` only.
struct CandidateGroup {
    candidates: Vec<usize>,
    differing_scan: Option<usize>,
}

/// `CommonJoinFilters`: inner joins that are the same but for the filters of one of their scans
/// become the same join, of the scan with the disjunction of all their filters, each with its own
/// filter on top again. The common subplans that follow share the join.
#[derive(Default)]
struct CommonJoinFilters {
    /// the canonical forms of the joins without the filters of their scans
    finder: Finder,
    candidates: Vec<JoinCandidate>,
    /// candidates by their signature, in the order found
    by_signature: Vec<(usize, Vec<usize>)>,
}

fn is_inner_join(op: &LogicalOperator) -> bool {
    matches!(
        op.kind,
        K::Join {
            join_type: JoinType::Inner,
            ..
        }
    )
}

/// The filters of a scan with its own table index made 0, to compare them with another's.
fn filters_text(op: &LogicalOperator) -> String {
    let K::Get {
        table_index, filters, ..
    } = &op.kind
    else {
        return String::new();
    };
    let mut filters = filters.clone();
    for filter in &mut filters {
        filter.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind
                && binding.table_index == *table_index
            {
                binding.table_index = 0;
            }
        });
    }
    format!("{filters:?}")
}

impl CommonJoinFilters {
    /// `Normalize`.
    fn normalize(&mut self, plan: &mut LogicalOperator) {
        self.find_candidates(plan, &mut Vec::new());
        let mut changes = Vec::new();
        for (_, members) in &self.by_signature {
            let mut groups: Vec<CandidateGroup> = Vec::new();
            for &candidate in members {
                if !groups.iter_mut().any(|group| self.try_add(plan, group, candidate)) {
                    groups.push(CandidateGroup {
                        candidates: vec![candidate],
                        differing_scan: None,
                    });
                }
            }
            changes.extend(groups);
        }
        // the candidates are apart, so each keeps its path while the others change
        for group in changes {
            self.normalize_group(plan, &group);
        }
    }

    /// `FindCandidates`.
    fn find_candidates(&mut self, plan: &LogicalOperator, path: &mut Path) {
        let op = node(plan, path);
        if is_inner_join(op) {
            let mut stripped = op.clone();
            let mut scans = Vec::new();
            if Self::strip(&mut stripped, &mut Vec::new(), true, &mut scans) {
                self.finder.signatures.clear();
                self.finder.visit(&mut stripped, &mut Vec::new());
                if let Some(&signature) = self.finder.signatures.get(&Vec::new()) {
                    let at = self.candidates.len();
                    self.candidates.push(JoinCandidate {
                        path: path.clone(),
                        scans: scans
                            .into_iter()
                            .map(|scan: Path| path.iter().copied().chain(scan).collect())
                            .collect(),
                    });
                    match self.by_signature.iter_mut().find(|(s, _)| *s == signature) {
                        Some((_, members)) => members.push(at),
                        None => self.by_signature.push((signature, vec![at])),
                    }
                }
            }
            return;
        }
        for at in 0..op.children.len() {
            path.push(at);
            self.find_candidates(plan, path);
            path.pop();
        }
    }

    /// The `Signature` walk: takes the filters off the scans below inner joins and filters, noting
    /// their paths; false where an expression there cannot be shared.
    fn strip(op: &mut LogicalOperator, path: &mut Path, movable: bool, scans: &mut Vec<Path>) -> bool {
        let traverse = movable && (is_inner_join(op) || matches!(op.kind, K::Filter { .. } | K::ColumnSelect { .. }));
        if traverse && any_expression(&op.kind, &|e| !can_share_expression(e)) {
            return false;
        }
        for at in 0..op.children.len() {
            path.push(at);
            let ok = Self::strip(&mut op.children[at], path, traverse, scans);
            path.pop();
            if !ok {
                return false;
            }
        }
        if !movable {
            return true;
        }
        if let K::Get {
            filters,
            zone_filters,
            index_rows,
            ..
        } = &mut op.kind
            && op.children.is_empty()
            && zone_filters.is_empty()
            && index_rows.is_none()
            && filters.iter().all(can_share_expression)
        {
            filters.clear();
            scans.push(path.clone());
        }
        true
    }

    /// `CandidateGroup::TryAdd`.
    fn try_add(&self, plan: &LogicalOperator, group: &mut CandidateGroup, candidate: usize) -> bool {
        let primary = &self.candidates[group.candidates[0]];
        let other = &self.candidates[candidate];
        if primary.scans.len() != other.scans.len() {
            return false;
        }
        let mut varying = group.differing_scan;
        for (at, (left, right)) in primary.scans.iter().zip(&other.scans).enumerate() {
            let (left, right) = (node(plan, left), node(plan, right));
            let (
                K::Get {
                    column_ids: a,
                    extracts: a_extracts,
                    ..
                },
                K::Get {
                    column_ids: b,
                    extracts: b_extracts,
                    ..
                },
            ) = (&left.kind, &right.kind)
            else {
                return false;
            };
            // the fields a scan outputs of a column are not compared
            if a != b || a_extracts.iter().chain(b_extracts).any(Option::is_some) {
                return false;
            }
            if filters_text(left) == filters_text(right) {
                continue;
            }
            // one varying input avoids cross-combinations between independent filter disjunctions
            if varying.is_some_and(|v| v != at) {
                return false;
            }
            varying = Some(at);
        }
        group.differing_scan = varying;
        group.candidates.push(candidate);
        true
    }

    /// `NormalizeGroup`.
    fn normalize_group(&self, plan: &mut LogicalOperator, group: &CandidateGroup) {
        let Some(differing) = group.differing_scan else {
            return;
        };
        if group.candidates.len() < 2 {
            return;
        }
        let cardinality: u64 = group
            .candidates
            .iter()
            .map(|&c| super::join_order::estimated_cardinality(node(plan, &self.candidates[c].path)))
            .sum();
        let primary = node(plan, &self.candidates[group.candidates[0]].path);
        if !is_selective_multi_table_plan(&estimate_plan(primary), cardinality) {
            return;
        }
        // `ScanPredicate` of each
        let mut predicates = Vec::new();
        let mut table_indexes = Vec::new();
        for &c in &group.candidates {
            let K::Get {
                table_index, filters, ..
            } = &node(plan, &self.candidates[c].scans[differing]).kind
            else {
                unreachable!("a scan")
            };
            predicates.push(if filters.is_empty() {
                BoundExpr::constant(pintail_common::value::Value::boolean(true))
            } else {
                super::conjunction_of(filters.clone())
            });
            table_indexes.push(*table_index);
        }
        for (i, &c) in group.candidates.iter().enumerate() {
            let candidate = &self.candidates[c];
            let old_bindings = node(plan, &candidate.path).column_bindings();
            let scan = node_mut(plan, &candidate.scans[differing]);
            let K::Get {
                table_index,
                column_ids,
                filters,
                projection,
                ..
            } = &mut scan.kind
            else {
                unreachable!("a scan")
            };
            let own = *table_index;
            let disjuncts: Vec<BoundExpr> = predicates
                .iter()
                .zip(&table_indexes)
                .map(|(predicate, &from)| {
                    let mut predicate = predicate.clone();
                    predicate.walk_mut(&mut |e| {
                        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                            && binding.table_index == from
                        {
                            binding.table_index = own;
                        }
                    });
                    predicate
                })
                .collect();
            filters.clear();
            // all its columns go on, but the row ids the binder reads for nothing
            *projection = column_ids.contains(&COLUMN_IDENTIFIER_ROW_ID).then(|| {
                (0..column_ids.len())
                    .filter(|&p| column_ids[p] != COLUMN_IDENTIFIER_ROW_ID)
                    .collect()
            });
            let get = std::mem::replace(scan, LogicalOperator::new(K::CrossProduct, Vec::new()));
            *scan = LogicalOperator::new(
                K::Filter {
                    expressions: vec![BoundExpr::new(
                        BoundExprKind::Conjunction {
                            and: false,
                            children: disjuncts,
                        },
                        LogicalType::BOOLEAN,
                    )],
                },
                vec![get],
            );
            // `ExposeFilterColumns`: the joins pass on all columns, for the filter on top
            let op = node_mut(plan, &candidate.path);
            Self::expose_filter_columns(op);
            let new_bindings = op.column_bindings();
            let join = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
            let mut residual = LogicalOperator::new(
                K::Filter {
                    expressions: vec![predicates[i].clone()],
                },
                vec![join],
            );
            if new_bindings != old_bindings {
                residual = LogicalOperator::new(K::ColumnSelect { keep: old_bindings }, vec![residual]);
            }
            *op = residual;
        }
    }

    fn expose_filter_columns(op: &mut LogicalOperator) {
        if !(is_inner_join(op) || matches!(op.kind, K::Filter { .. } | K::ColumnSelect { .. })) {
            return;
        }
        for child in &mut op.children {
            while matches!(child.kind, K::ColumnSelect { .. }) {
                let below = child.children.remove(0);
                *child = below;
            }
            Self::expose_filter_columns(child);
        }
    }
}
