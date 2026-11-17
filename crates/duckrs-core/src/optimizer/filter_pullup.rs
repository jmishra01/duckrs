// Ported from duckdb/src/optimizer/filter_pullup.cpp and duckdb/src/optimizer/pullup/
// {pullup_both_side,pullup_filter,pullup_from_left,pullup_projection,pullup_set_operation}.cpp
// @ rust-port-baseline
//
//! `FilterPullup`: filters below inner joins and cross products go above them, with the inner
//! joins' conditions, so that filter pushdown can then push each down to every side it holds for
//! (`i = 5` on one side of `a.i = b.i` becomes `i = 5` on both). The left sides of LEFT, ANTI,
//! SEMI and AsOf joins and of EXCEPT and INTERSECT give their filters up too.
//!
//! Not ported: pulling filters through window operators (`PullupWindow`), as filter pushdown
//! here does not push them back below one.

use super::rewriter::can_throw;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::LogicalType;
use duckrs_parser::ast::{ComparisonOp, JoinType};

/// Runs the pass over a plan.
pub fn pull_up(plan: LogicalOperator) -> LogicalOperator {
    FilterPullup::default().rewrite(plan)
}

#[derive(Default)]
struct FilterPullup {
    /// whether the filters can go above the operator this rewrites
    can_pullup: bool,
    /// whether a projection above may get columns the filters read (not under EXCEPT, INTERSECT)
    can_add_column: bool,
    filters: Vec<BoundExpr>,
}

fn filter(child: LogicalOperator, expressions: Vec<BoundExpr>) -> LogicalOperator {
    LogicalOperator::new(K::Filter { expressions }, vec![child])
}

/// `JoinCondition::CreateExpression`.
fn condition_expression(condition: JoinCondition) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op: condition.comparison,
            left: Box::new(condition.left),
            right: Box::new(condition.right),
        },
        LogicalType::BOOLEAN,
    )
}

impl FilterPullup {
    fn new(can_pullup: bool, can_add_column: bool) -> Self {
        Self {
            can_pullup,
            can_add_column,
            filters: Vec::new(),
        }
    }

    fn rewrite(&mut self, op: LogicalOperator) -> LogicalOperator {
        match &op.kind {
            K::Filter { .. } => self.pullup_filter(op),
            K::Projection { .. } => self.pullup_projection(op),
            K::CrossProduct => self.pullup_both_side(op),
            K::Join { join_type, .. } | K::DelimJoin { join_type, .. } => match join_type {
                JoinType::Inner if matches!(op.kind, K::Join { .. }) => self.pullup_inner_join(op),
                JoinType::Inner => op,
                JoinType::Semi if matches!(&op.kind, K::Join { conditions, .. } if !conditions.is_empty()) => {
                    self.pullup_semi_join(op)
                }
                JoinType::Semi | JoinType::Left | JoinType::Anti => self.pullup_from_left(op),
                _ => self.finish_pullup(op),
            },
            K::AsOfJoin { join_type, .. } => match join_type {
                // only the left side of an AsOf join passes its rows on as they are
                JoinType::Inner | JoinType::Semi | JoinType::Left | JoinType::Anti => self.pullup_from_left(op),
                _ => self.finish_pullup(op),
            },
            K::ExceptIntersect { .. } => self.pullup_set_operation(op),
            K::Distinct { on, .. } => {
                if *on {
                    // DISTINCT ON keeps the first row of each group of what passes
                    return self.finish_pullup(op);
                }
                let mut op = op;
                let child = op.children.pop().expect("a DISTINCT has a child");
                let child = self.rewrite(child);
                if self.filters.iter().any(BoundExpr::is_volatile) {
                    // volatile filters run once per input row, below the DISTINCT
                    let pulled = std::mem::take(&mut self.filters);
                    op.children.push(filter(child, pulled));
                } else {
                    op.children.push(child);
                }
                op
            }
            K::Order { .. } => {
                let mut op = op;
                let child = op.children.pop().expect("a sort has a child");
                op.children.push(self.rewrite(child));
                op
            }
            _ => self.finish_pullup(op),
        }
    }

    /// `FinishPullup`: the children start over, and what was pulled up stays here.
    fn finish_pullup(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        op.children = std::mem::take(&mut op.children)
            .into_iter()
            .map(|child| FilterPullup::default().rewrite(child))
            .collect();
        if self.filters.is_empty() {
            return op;
        }
        filter(op, std::mem::take(&mut self.filters))
    }

    /// `PullupFilter`.
    fn pullup_filter(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let child = op.children.pop().expect("a filter has a child");
        if self.can_pullup {
            let child = self.rewrite(child);
            let K::Filter { expressions } = op.kind else { unreachable!() };
            self.filters.extend(expressions);
            return child;
        }
        op.children.push(self.rewrite(child));
        op
    }

    /// `PullupBothSide`.
    fn pullup_both_side(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let mut left = FilterPullup::new(true, self.can_add_column);
        let mut right = FilterPullup::new(true, self.can_add_column);
        let right_child = op.children.pop().expect("two children");
        let left_child = op.children.pop().expect("two children");
        op.children.push(left.rewrite(left_child));
        op.children.push(right.rewrite(right_child));
        left.filters.extend(right.filters);
        if left.filters.is_empty() {
            return op;
        }
        filter(op, left.filters)
    }

    /// `PullupFromLeft`: the left side's filters go above the operator if the right side has none.
    fn pullup_from_left(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let mut left = FilterPullup::new(true, self.can_add_column);
        let mut right = FilterPullup::new(false, self.can_add_column);
        let right_child = op.children.pop().expect("two children");
        let left_child = op.children.pop().expect("two children");
        op.children.push(left.rewrite(left_child));
        op.children.push(right.rewrite(right_child));
        if !left.filters.is_empty() && right.filters.is_empty() {
            return filter(op, left.filters);
        }
        op
    }

    /// `PullupSemiJoin`: the left side's filters go above it, with what the right side's filters
    /// tell of the left side through the join's equalities.
    fn pullup_semi_join(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let mut left = FilterPullup::new(true, self.can_add_column);
        let mut right = FilterPullup::new(true, self.can_add_column);
        let right_child = op.children.pop().expect("two children");
        let left_child = op.children.pop().expect("two children");
        let left_child = left.rewrite(left_child);
        let mut right_child = right.rewrite(right_child);
        let mut left_filters = left.filters;
        if !right.filters.is_empty() {
            if let K::Join { conditions, .. } = &op.kind {
                left_filters.extend(join_filters(conditions, &right.filters, true));
            }
            right_child = filter(right_child, right.filters);
        }
        op.children.push(left_child);
        op.children.push(right_child);
        if left_filters.is_empty() {
            return op;
        }
        filter(op, left_filters)
    }

    /// `PullupInnerJoin`: the join becomes a cross product, its conditions filters above it.
    fn pullup_inner_join(&mut self, op: LogicalOperator) -> LogicalOperator {
        let mut op = self.pullup_both_side(op);
        let (mut expressions, op) = if let K::Filter { expressions } = &mut op.kind {
            let expressions = std::mem::take(expressions);
            (expressions, op.children.pop().expect("a filter has a child"))
        } else if !self.can_pullup {
            return op;
        } else {
            (Vec::new(), op)
        };
        let K::Join {
            conditions, predicate, ..
        } = op.kind
        else {
            unreachable!("pullup_both_side keeps the join")
        };
        expressions.extend(conditions.into_iter().map(condition_expression));
        expressions.extend(predicate);
        let cross = LogicalOperator::new(K::CrossProduct, op.children);
        if self.can_pullup {
            self.filters.extend(expressions);
            cross
        } else {
            filter(cross, expressions)
        }
    }

    /// `PullupProjection`.
    fn pullup_projection(&mut self, mut op: LogicalOperator) -> LogicalOperator {
        let child = op.children.pop().expect("a projection has a child");
        op.children.push(self.rewrite(child));
        if self.filters.is_empty() {
            return op;
        }
        // fallible filters, and what guards them, stay below the projection
        if self.filters.iter().any(can_throw) {
            self.revert(&mut op);
            return op;
        }
        let K::Projection {
            table_index,
            expressions,
        } = &mut op.kind
        else {
            unreachable!()
        };
        let table_index = *table_index;
        if !self.can_add_column {
            // EXCEPT and INTERSECT compare all columns: the projection can't get more
            let mut copy = expressions.clone();
            let mut changed = self.filters.clone();
            for filter in &mut changed {
                replace_binding(&mut copy, filter, table_index);
            }
            if copy.len() > expressions.len() {
                self.revert(&mut op);
                return op;
            }
            self.filters = changed;
            return op;
        }
        for filter in &mut self.filters {
            replace_binding(expressions, filter, table_index);
        }
        op
    }

    /// `RevertFilterPullup`: the filters go back below the projection.
    fn revert(&mut self, op: &mut LogicalOperator) {
        let child = op.children.pop().expect("a projection has a child");
        op.children.push(filter(child, std::mem::take(&mut self.filters)));
    }

    /// `PullupSetOperation`: EXCEPT and INTERSECT return rows of the left side.
    fn pullup_set_operation(&mut self, op: LogicalOperator) -> LogicalOperator {
        self.can_add_column = false;
        self.can_pullup = true;
        let mut op = self.pullup_from_left(op);
        if let K::Filter { expressions } = &mut op.kind
            && let K::ExceptIntersect { table_index, .. } = &op.children[0].kind
        {
            let table_index = *table_index;
            for expr in expressions {
                expr.walk_mut(&mut |e| {
                    if let BoundExprKind::ColumnRef { binding, .. } = &mut e.kind {
                        binding.table_index = table_index;
                    }
                });
            }
        }
        op
    }
}

/// `ReplaceExpressionBinding`: the filter reads the projection's columns, which get those it
/// reads that it does not have.
fn replace_binding(projection: &mut Vec<BoundExpr>, expr: &mut BoundExpr, table_index: usize) {
    if let BoundExprKind::ColumnRef { binding, .. } = &expr.kind {
        let binding = *binding;
        let found = projection.iter().position(|p| {
            matches!(&p.kind, BoundExprKind::ColumnRef { binding: b, .. } if *b == binding)
                && p.return_type == expr.return_type
        });
        let at = match found {
            Some(at) => at,
            None => {
                projection.push(expr.clone());
                projection.len() - 1
            }
        };
        if let BoundExprKind::ColumnRef { binding, .. } = &mut expr.kind {
            *binding = ColumnBinding::new(table_index, at);
        }
        return;
    }
    for child in expr.children_mut() {
        replace_binding(projection, child, table_index);
    }
}

/// `FilterCombiner::GetComparedExpression`: the one side of a comparison with constants (the first
/// child of a BETWEEN or IN) that is not one.
fn compared_expression(filter: &BoundExpr) -> Option<&BoundExpr> {
    if filter.is_volatile() {
        return None;
    }
    let first_only = match &filter.kind {
        BoundExprKind::Comparison { op, .. } => {
            if matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom) {
                return None;
            }
            false
        }
        BoundExprKind::Between { .. } => true,
        BoundExprKind::Operator {
            op: crate::planner::expression::BoundOperator::In,
            ..
        } => true,
        _ => return None,
    };
    let mut compared = None;
    for (at, child) in filter.children().into_iter().enumerate() {
        if !child.is_foldable() {
            if compared.is_some() || (first_only && at != 0) {
                return None;
            }
            compared = Some(child);
        }
    }
    compared
}

/// `FilterCombiner::GenerateJoinFilters`: the filters of one side of the equalities as filters of
/// the other, the left side if `to_left`.
pub(super) fn join_filters(conditions: &[JoinCondition], filters: &[BoundExpr], to_left: bool) -> Vec<BoundExpr> {
    let mut result = Vec::new();
    for condition in conditions {
        if !matches!(condition.comparison, ComparisonOp::Equal | ComparisonOp::NotDistinctFrom) {
            continue;
        }
        let (source, target) = if to_left {
            (&condition.right, &condition.left)
        } else {
            (&condition.left, &condition.right)
        };
        if source.is_volatile()
            || target.is_volatile()
            || can_throw(target)
            || source.is_foldable()
            || target.is_foldable()
        {
            continue;
        }
        for filter in filters {
            if compared_expression(filter).is_some_and(|c| super::rewriter::expressions_equal(c, source)) {
                let mut generated = filter.clone();
                replace_expression(&mut generated, source, target);
                result.push(generated);
            }
        }
    }
    result
}

fn replace_expression(expr: &mut BoundExpr, from: &BoundExpr, to: &BoundExpr) {
    if super::rewriter::expressions_equal(expr, from) {
        *expr = to.clone();
        return;
    }
    for child in expr.children_mut() {
        replace_expression(child, from, to);
    }
}
