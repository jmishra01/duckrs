// Ported from duckdb/src/optimizer/filter_pushdown.cpp and duckdb/src/optimizer/pushdown/
// {pushdown_aggregate,pushdown_cross_product,pushdown_distinct,pushdown_filter,pushdown_inner_join,
// pushdown_left_join,pushdown_limit,pushdown_projection,pushdown_semi_anti_join,
// pushdown_set_operation,pushdown_unnest}.cpp @ rust-port-baseline
//
//! `FilterPushdown`: moves filters as far down the plan as they go, and turns cross products with
//! a filter on both sides into joins. Operators it has no rule for keep the filters on top of them.
//!
//! `FilterCombiner` is reduced to splitting conjuncts, dropping duplicates and deciding constant
//! filters; its transitive filters and ranges are not ported.

use super::conjunction_of;
use super::rewriter::{can_throw, expressions_equal, fold_boolean, rewrite_expression, split_conjuncts};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::BTreeSet;

/// `LogicalOperatorKind::EmptyResult` in place of `op`.
pub(crate) fn empty_result(op: &LogicalOperator) -> LogicalOperator {
    LogicalOperator::new(
        LogicalOperatorKind::EmptyResult {
            types: op.types(),
            bindings: op.column_bindings(),
        },
        Vec::new(),
    )
}

/// The table indexes an expression reads at this level (`LogicalJoin::GetExpressionBindings`).
fn expression_tables(expr: &BoundExpr, tables: &mut BTreeSet<usize>) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        tables.insert(binding.table_index);
    }
    for child in expr.children() {
        expression_tables(child, tables);
    }
}

/// `LogicalJoin::GetTableReferences`.
fn plan_tables(op: &LogicalOperator) -> BTreeSet<usize> {
    op.column_bindings().iter().map(|b| b.table_index).collect()
}

fn visit_columns(expr: &mut BoundExpr, f: &mut impl FnMut(&mut BoundExpr)) {
    expr.walk_mut(&mut |e| {
        if matches!(e.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
            f(e);
        }
    });
}

/// Replaces the columns of `expr` with `f`, leaving what `f` puts there alone: a replacement refers
/// to the columns below the operator, which mean something else there.
fn replace_columns(expr: &mut BoundExpr, f: &mut impl FnMut(&mut BoundExpr)) {
    if matches!(expr.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
        f(expr);
        return;
    }
    for child in expr.children_mut() {
        replace_columns(child, f);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinSide {
    None,
    Left,
    Right,
    Both,
}

fn join_side(tables: &BTreeSet<usize>, left: &BTreeSet<usize>, right: &BTreeSet<usize>) -> JoinSide {
    if tables.is_empty() {
        JoinSide::None
    } else if tables.is_subset(left) {
        JoinSide::Left
    } else if tables.is_subset(right) {
        JoinSide::Right
    } else {
        JoinSide::Both
    }
}

/// `FilterPushdown::Filter`.
#[derive(Debug, Clone)]
struct Filter {
    filter: BoundExpr,
    tables: BTreeSet<usize>,
}

impl Filter {
    fn new(filter: BoundExpr) -> Self {
        let mut tables = BTreeSet::new();
        expression_tables(&filter, &mut tables);
        Self { filter, tables }
    }
}

/// The filters on their way down to an operator.
#[derive(Debug, Default)]
struct FilterPushdown {
    filters: Vec<Filter>,
}

/// Runs the pass over a plan.
pub fn push_down(plan: LogicalOperator) -> LogicalOperator {
    FilterPushdown::default().rewrite(plan)
}

impl FilterPushdown {
    /// `FilterCombiner::AddFilter`: false if the filter can never hold.
    fn add_filter(&mut self, expr: BoundExpr) -> bool {
        let mut conjuncts = Vec::new();
        split_conjuncts(expr, &mut conjuncts);
        for conjunct in conjuncts {
            match fold_boolean(&conjunct) {
                Some(Some(true)) => continue,
                Some(_) => return false,
                None => {}
            }
            if !self.filters.iter().any(|f| expressions_equal(&f.filter, &conjunct)) {
                self.filters.push(Filter::new(conjunct));
            }
        }
        true
    }

    fn rewrite(mut self, mut op: LogicalOperator) -> LogicalOperator {
        match &op.kind {
            LogicalOperatorKind::Aggregate { .. } => self.pushdown_aggregate(op),
            LogicalOperatorKind::Filter { .. } => {
                let LogicalOperatorKind::Filter { expressions } = &mut op.kind else {
                    unreachable!()
                };
                for expr in std::mem::take(expressions) {
                    if !self.add_filter(expr) {
                        return empty_result(&op);
                    }
                }
                self.rewrite(op.children.remove(0))
            }
            LogicalOperatorKind::CrossProduct => self.pushdown_cross_product(op),
            LogicalOperatorKind::Join { join_type, .. } => match join_type {
                JoinType::Inner => self.pushdown_inner_join(op),
                JoinType::Left => self.pushdown_left_join(op),
                JoinType::Semi | JoinType::Anti => self.pushdown_semi_anti_join(op),
                _ => self.finish_pushdown(op),
            },
            LogicalOperatorKind::DelimJoin { .. } => self.pushdown_delim_join(op),
            LogicalOperatorKind::Projection { .. } => self.pushdown_projection(op),
            LogicalOperatorKind::Union { .. } | LogicalOperatorKind::ExceptIntersect { .. } => {
                self.pushdown_set_operation(op)
            }
            LogicalOperatorKind::Distinct { .. } => self.pushdown_distinct(op),
            LogicalOperatorKind::Order { .. } => {
                let child = op.children.remove(0);
                op.children.push(self.rewrite(child));
                op
            }
            LogicalOperatorKind::Limit { limit: Some(0), .. } => empty_result(&op),
            LogicalOperatorKind::Unnest { .. } => self.pushdown_unnest(op),
            _ => self.finish_pushdown(op),
        }
    }

    /// `PushdownMarkJoin` and `PushdownSingleJoin`: each left row stays, once, so what reads only
    /// the left side can go there, which also makes the domain smaller.
    fn pushdown_delim_join(self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let (left, rest): (Vec<_>, Vec<_>) = self
            .filters
            .into_iter()
            .partition(|f| !f.tables.is_empty() && f.tables.is_subset(&left_tables));
        let right = op.children.pop().expect("a join has two children");
        let left_child = op.children.pop().expect("a join has two children");
        op.children.push(FilterPushdown { filters: left }.rewrite(left_child));
        op.children.push(FilterPushdown::default().rewrite(right));
        FilterPushdown { filters: rest }.push_final_filters(op)
    }

    /// `FilterPushdown::PushFinalFilters`.
    fn push_final_filters(self, op: LogicalOperator) -> LogicalOperator {
        add_filter_operator(op, self.filters.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::FinishPushdown`: the filters stop here, and the children start over.
    fn finish_pushdown(self, mut op: LogicalOperator) -> LogicalOperator {
        op.children = std::mem::take(&mut op.children).into_iter().map(push_down).collect();
        if let LogicalOperatorKind::LateralJoin { right, .. } = &mut op.kind {
            let plan = std::mem::replace(
                &mut **right,
                LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![]),
            );
            **right = push_down(plan);
        }
        self.push_final_filters(op)
    }

    /// `FilterPushdown::PushdownAggregate`: a filter on groups that are in every grouping set
    /// holds for all rows of a group or none.
    fn pushdown_aggregate(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Aggregate {
            group_index,
            groups,
            grouping_sets,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let (group_index, groups, grouping_sets) = (*group_index, groups.clone(), grouping_sets.clone());
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for mut f in std::mem::take(&mut self.filters) {
            let mut columns = Vec::new();
            visit_columns(&mut f.filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            let in_all_sets = columns
                .iter()
                .all(|b| b.table_index == group_index && grouping_sets.iter().all(|set| set.contains(&b.column_index)));
            if groups.is_empty() || columns.is_empty() || !in_all_sets {
                remaining.push(f);
                continue;
            }
            // below the aggregate, a group that can fail would run ahead of the filters there
            if can_throw(&f.filter)
                || f.filter.is_volatile()
                || columns.iter().any(|b| {
                    let group = &groups[b.column_index];
                    group.is_volatile() || can_throw(group)
                })
            {
                remaining.push(f);
                continue;
            }
            let mut filter = f.filter;
            replace_columns(&mut filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    *e = groups[binding.column_index].clone();
                }
            });
            if !below.add_filter(filter) {
                return empty_result(&op);
            }
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownCrossProduct`.
    fn pushdown_cross_product(self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let right_tables = plan_tables(&op.children[1]);
        let (mut left, mut right) = (FilterPushdown::default(), FilterPushdown::default());
        let mut join_expressions = Vec::new();
        let mut above = Vec::new();
        for f in self.filters {
            match join_side(&f.tables, &left_tables, &right_tables) {
                JoinSide::Left => left.filters.push(f),
                JoinSide::Right => right.filters.push(f),
                JoinSide::Both => join_expressions.push(f.filter),
                JoinSide::None => above.push(f.filter),
            }
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        let children = vec![left.rewrite(left_child), right.rewrite(right_child)];
        let result = if join_expressions.is_empty() {
            LogicalOperator::new(LogicalOperatorKind::CrossProduct, children)
        } else {
            let (conditions, predicates) = extract_join_conditions(join_expressions, &left_tables, &right_tables);
            let kind = LogicalOperatorKind::Join {
                join_type: JoinType::Inner,
                conditions,
                predicate: (!predicates.is_empty()).then(|| conjunction_of(predicates)),
            };
            LogicalOperator::new(kind, children)
        };
        add_filter_operator(result, above)
    }

    /// `FilterPushdown::PushdownInnerJoin`: the conditions become filters, and then the join is a
    /// cross product with filters.
    fn pushdown_inner_join(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Join {
            conditions, predicate, ..
        } = &mut op.kind
        else {
            unreachable!()
        };
        let (conditions, predicate) = (std::mem::take(conditions), predicate.take());
        let expressions = conditions.into_iter().map(condition_expression).chain(predicate);
        for expr in expressions {
            if !self.add_filter(expr) {
                return empty_result(&op);
            }
        }
        self.pushdown_cross_product(LogicalOperator::new(LogicalOperatorKind::CrossProduct, op.children))
    }

    /// `FilterPushdown::PushdownLeftJoin`: filters on the left go below the join. One that
    /// removes the rows without a match makes this an inner join.
    fn pushdown_left_join(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let left_tables = plan_tables(&op.children[0]);
        let right_tables = plan_tables(&op.children[1]);
        let mut left = FilterPushdown::default();
        let mut remaining = Vec::new();
        let filters = std::mem::take(&mut self.filters);
        let removes_null = filters.iter().any(|f| {
            join_side(&f.tables, &left_tables, &right_tables) != JoinSide::Left
                && filter_removes_null(&f.filter, &right_tables)
        });
        if removes_null {
            let LogicalOperatorKind::Join { join_type, .. } = &mut op.kind else {
                unreachable!()
            };
            *join_type = JoinType::Inner;
            self.filters = filters;
            return self.pushdown_inner_join(op);
        }
        for f in filters {
            if join_side(&f.tables, &left_tables, &right_tables) == JoinSide::Left {
                left.filters.push(f);
            } else {
                remaining.push(f);
            }
        }
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        op.children = vec![left.rewrite(left_child), push_down(right_child)];
        add_filter_operator(op, remaining.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownSemiAntiJoin`: the output is the left side's, filters and all.
    fn pushdown_semi_anti_join(self, mut op: LogicalOperator) -> LogicalOperator {
        let right_child = op.children.remove(1);
        let left_child = op.children.remove(0);
        op.children = vec![self.rewrite(left_child), push_down(right_child)];
        op
    }

    /// `FilterPushdown::PushdownProjection`.
    fn pushdown_projection(self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } = &op.kind
        else {
            unreachable!()
        };
        let table_index = *table_index;
        let projected = expressions.clone();
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for mut f in self.filters {
            let mut columns = Vec::new();
            visit_columns(&mut f.filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    columns.push(binding);
                }
            });
            // copying a subquery would run it twice, and below the projection an entry that can
            // fail would run ahead of the filters there
            let stays = can_throw(&f.filter)
                || columns.iter().any(|b| {
                    let entry = projected.get(b.column_index);
                    b.table_index != table_index
                        || entry.is_none_or(|e| e.is_volatile() || contains_subquery(e) || can_throw(e))
                });
            if stays {
                remaining.push(f.filter);
                continue;
            }
            let mut filter = f.filter;
            replace_columns(&mut filter, &mut |e| {
                if let BoundExprKind::ColumnRef { binding, .. } = e.kind {
                    let alias = std::mem::take(&mut e.alias);
                    *e = projected[binding.column_index].clone();
                    if !alias.is_empty() {
                        e.alias = alias;
                    }
                }
            });
            if !below.add_filter(filter) {
                return empty_result(&op);
            }
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining)
    }

    /// `FilterPushdown::PushdownSetOperation`.
    fn pushdown_set_operation(self, mut op: LogicalOperator) -> LogicalOperator {
        let (table_index, left_only) = match &op.kind {
            LogicalOperatorKind::Union { table_index, .. } => (*table_index, false),
            LogicalOperatorKind::ExceptIntersect { table_index, .. } => (*table_index, true),
            _ => unreachable!(),
        };
        let children = std::mem::take(&mut op.children);
        let mut result = Vec::with_capacity(children.len());
        for (index, child) in children.into_iter().enumerate() {
            let mut below = FilterPushdown::default();
            // a filter on the result of EXCEPT and INTERSECT only applies to their left child
            if index == 0 || !left_only {
                let bindings = child.column_bindings();
                for f in &self.filters {
                    let mut filter = f.filter.clone();
                    replace_set_operation_bindings(&mut filter, table_index, &bindings);
                    below.filters.push(Filter::new(filter));
                }
            }
            result.push(below.rewrite(child));
        }
        op.children = result;
        op
    }

    /// `FilterPushdown::PushdownDistinct`. DISTINCT ON picks a row of each group, so what it picks
    /// changes with a filter below it.
    fn pushdown_distinct(mut self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Distinct { targets } = &op.kind else {
            unreachable!()
        };
        if targets.len() != op.children[0].column_bindings().len() {
            return self.finish_pushdown(op);
        }
        let (volatile, pushed) = std::mem::take(&mut self.filters)
            .into_iter()
            .partition::<Vec<_>, _>(|f| f.filter.is_volatile());
        self.filters = pushed;
        let child = op.children.remove(0);
        op.children.push(self.rewrite(child));
        add_filter_operator(op, volatile.into_iter().map(|f| f.filter).collect())
    }

    /// `FilterPushdown::PushdownUnnest`.
    fn pushdown_unnest(self, mut op: LogicalOperator) -> LogicalOperator {
        let LogicalOperatorKind::Unnest { table_index, .. } = &op.kind else {
            unreachable!()
        };
        let table_index = *table_index;
        let mut below = FilterPushdown::default();
        let mut remaining = Vec::new();
        for f in self.filters {
            if f.tables.contains(&table_index) {
                remaining.push(f.filter);
            } else if !below.add_filter(f.filter) {
                return empty_result(&op);
            }
        }
        let child = op.children.remove(0);
        op.children.push(below.rewrite(child));
        add_filter_operator(op, remaining)
    }
}

fn contains_subquery(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(contains_subquery)
}

fn replace_set_operation_bindings(filter: &mut BoundExpr, table_index: usize, bindings: &[ColumnBinding]) {
    visit_columns(filter, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind
            && binding.table_index == table_index
        {
            *binding = bindings[binding.column_index];
        }
    });
}

/// `FilterPushdown::AddLogicalFilter`.
fn add_filter_operator(op: LogicalOperator, expressions: Vec<BoundExpr>) -> LogicalOperator {
    if expressions.is_empty() {
        return op;
    }
    LogicalOperator::new(LogicalOperatorKind::Filter { expressions }, vec![op])
}

/// `JoinCondition::CreateExpression`.
fn condition_expression(condition: JoinCondition) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op: if condition.null_safe {
                ComparisonOp::NotDistinctFrom
            } else {
                ComparisonOp::Equal
            },
            left: Box::new(condition.left),
            right: Box::new(condition.right),
        },
        duckrs_common::types::LogicalType::BOOLEAN,
    )
}

/// `LogicalComparisonJoin::ExtractJoinConditions`: equalities with a side of the join on each
/// side of them become conditions, which a hash join can use. The rest is a predicate.
fn extract_join_conditions(
    expressions: Vec<BoundExpr>,
    left: &BTreeSet<usize>,
    right: &BTreeSet<usize>,
) -> (Vec<JoinCondition>, Vec<BoundExpr>) {
    let mut conditions = Vec::new();
    let mut predicates = Vec::new();
    for expr in expressions {
        if let BoundExprKind::Comparison {
            op: op @ (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom),
            left: first,
            right: second,
        } = &expr.kind
        {
            let tables = |e: &BoundExpr| {
                let mut tables = BTreeSet::new();
                expression_tables(e, &mut tables);
                tables
            };
            let sides = (
                join_side(&tables(first), left, right),
                join_side(&tables(second), left, right),
            );
            let null_safe = *op == ComparisonOp::NotDistinctFrom;
            let sides = match sides {
                (JoinSide::Left, JoinSide::Right) => Some((first, second)),
                (JoinSide::Right, JoinSide::Left) => Some((second, first)),
                _ => None,
            };
            if let Some((l, r)) = sides {
                conditions.push(JoinCondition {
                    left: (**l).clone(),
                    right: (**r).clone(),
                    null_safe,
                });
                continue;
            }
        }
        predicates.push(expr);
    }
    (conditions, predicates)
}

/// `FilterRemovesNull`: whether a filter is false or NULL for a row whose right side is NULL.
fn filter_removes_null(filter: &BoundExpr, right: &BTreeSet<usize>) -> bool {
    let mut copy = filter.clone();
    visit_columns(&mut copy, &mut |e| {
        if let BoundExprKind::ColumnRef { binding, .. } = e.kind
            && right.contains(&binding.table_index)
        {
            e.kind = BoundExprKind::Constant(Value::null(e.return_type.clone()));
        }
    });
    rewrite_expression(&mut copy, true);
    let mut conjuncts = Vec::new();
    split_conjuncts(copy, &mut conjuncts);
    for conjunct in conjuncts {
        if !conjunct.is_foldable() {
            return false;
        }
        if let Some(None | Some(false)) = fold_boolean(&conjunct) {
            return true;
        }
    }
    false
}
