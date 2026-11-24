// Ported from duckdb/src/optimizer/outer_join_simplification.cpp @ rust-port-baseline
//
//! `OuterJoinSimplification`: an outer join whose NULL-extended rows something above removes is
//! an inner (or one-sided) join, and a LEFT join that only its unmatched rows pass, through an
//! `IS NULL` of a join key of the right side that nothing else reads, is an ANTI join.
//!
//! A join here is `LogicalComparisonJoin` when it has conditions; one with only a predicate is
//! C++'s `LogicalAnyJoin`, which the pass does not look into.

use super::rewriter::cast_is_invertible;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::HashSet;

type Bindings = HashSet<ColumnBinding>;

/// Runs the pass over a plan.
pub fn simplify(mut plan: LogicalOperator) -> LogicalOperator {
    Simplification::default().visit(&mut plan);
    plan
}

#[derive(Default)]
struct Simplification {
    /// the columns the operators above read, other than through what this pass reasons about
    required: Bindings,
    initialized: bool,
    /// the columns a filter above rejects the NULLs of
    null_filtered: Bindings,
    /// the columns a filter above keeps only the NULLs of (`IS NULL`)
    null_required: Bindings,
    /// the columns that a join below turned into an ANTI join leaves NULL
    eliminated_null: Bindings,
}

fn add_references(expr: &BoundExpr, bindings: &mut Bindings) {
    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &expr.kind {
        bindings.insert(*binding);
    }
    for child in expr.children() {
        add_references(child, bindings);
    }
}

/// `GetColumnBinding`: the column, through casts.
fn column_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => Some(*binding),
        BoundExprKind::Cast { child, .. } => column_binding(child),
        _ => None,
    }
}

/// `GetNullPreservingColumnBinding`: the column, through casts that are NULL only for NULL.
fn null_preserving_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => Some(*binding),
        BoundExprKind::Cast { child, try_cast } => {
            if *try_cast || !cast_is_invertible(&child.return_type, &expr.return_type) {
                return None;
            }
            null_preserving_binding(child)
        }
        _ => None,
    }
}

fn filters_nulls(op: ComparisonOp) -> bool {
    !matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom)
}

/// `IsNullFilter`.
fn is_null_filter(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::IsNull,
            children,
        } => null_preserving_binding(&children[0]),
        _ => None,
    }
}

impl Simplification {
    fn with_required(required: Bindings) -> Self {
        Self {
            required,
            initialized: true,
            ..Self::default()
        }
    }

    fn add_required(&mut self, expr: &BoundExpr) {
        add_references(expr, &mut self.required);
    }

    fn add_required_conditions(&mut self, conditions: &[JoinCondition], predicate: Option<&BoundExpr>) {
        for condition in conditions {
            self.add_required(&condition.left);
            self.add_required(&condition.right);
        }
        if let Some(predicate) = predicate {
            self.add_required(predicate);
        }
    }

    fn handle_expression(&mut self, expr: &BoundExpr) {
        if let Some(binding) = column_binding(expr) {
            self.null_filtered.insert(binding);
        }
    }

    /// `HandleFilterExpression`.
    fn handle_filter(&mut self, expr: &BoundExpr) {
        match &expr.kind {
            BoundExprKind::Operator {
                op: BoundOperator::IsNotNull,
                children,
            } => {
                self.handle_expression(&children[0]);
                self.add_required(expr);
            }
            BoundExprKind::Operator {
                op: BoundOperator::IsNull,
                children,
            } => {
                if let Some(binding) = null_preserving_binding(&children[0]) {
                    self.null_required.insert(binding);
                } else {
                    self.add_required(expr);
                }
            }
            BoundExprKind::Comparison { op, left, right } => {
                self.add_required(expr);
                if filters_nulls(*op) {
                    self.handle_expression(left);
                    self.handle_expression(right);
                }
            }
            _ => self.add_required(expr),
        }
    }

    fn visit(&mut self, op: &mut LogicalOperator) {
        if !self.initialized {
            self.required.extend(op.column_bindings());
            self.initialized = true;
        }
        match &op.kind {
            K::Join {
                join_type, conditions, ..
            } if !conditions.is_empty() => match join_type {
                JoinType::Inner | JoinType::Semi => self.visit_inner_or_semi(op),
                JoinType::Left | JoinType::Right | JoinType::Outer => self.visit_outer(op),
                _ => self.visit_unsupported(op),
            },
            K::Projection { .. } => self.visit_projection(op),
            K::Filter { .. } => self.visit_filter(op),
            K::Order { orders } => {
                for order in orders.clone() {
                    self.add_required(&order.expr);
                }
                self.visit(&mut op.children[0]);
            }
            K::TopN { orders, .. } => {
                for order in orders.clone() {
                    self.add_required(&order.expr);
                }
                Self::with_required(self.required.clone()).visit(&mut op.children[0]);
            }
            K::Limit { .. } => Self::with_required(self.required.clone()).visit(&mut op.children[0]),
            K::Aggregate { groups, aggregates, .. } => {
                let mut required = Bindings::new();
                for group in groups {
                    add_references(group, &mut required);
                }
                for aggregate in aggregates {
                    for child in &aggregate.children {
                        add_references(child, &mut required);
                    }
                    if let Some(filter) = &aggregate.filter {
                        add_references(filter, &mut required);
                    }
                    for (expr, _) in &aggregate.order_by {
                        add_references(expr, &mut required);
                    }
                }
                Self::with_required(required).visit(&mut op.children[0]);
            }
            _ => self.visit_unsupported(op),
        }
    }

    fn visit_unsupported(&mut self, op: &mut LogicalOperator) {
        for child in &mut op.children {
            let required = child.column_bindings().into_iter().collect();
            Self::with_required(required).visit(child);
        }
    }

    fn visit_children(&mut self, op: &mut LogicalOperator) {
        for child in &mut op.children {
            self.visit(child);
        }
    }

    fn visit_inner_or_semi(&mut self, op: &mut LogicalOperator) {
        let K::Join { conditions, .. } = &op.kind else {
            unreachable!()
        };
        for condition in conditions.clone() {
            self.add_required(&condition.left);
            self.add_required(&condition.right);
            if filters_nulls(condition.comparison) {
                self.handle_expression(&condition.left);
                self.handle_expression(&condition.right);
            }
        }
        self.visit_children(op);
    }

    /// `TryConvertLeftToAntiJoin`.
    fn try_convert_left_to_anti(&self, op: &mut LogicalOperator) -> bool {
        let right = op.children[1].column_bindings();
        let K::Join {
            join_type, conditions, ..
        } = &mut op.kind
        else {
            return false;
        };
        if *join_type != JoinType::Left {
            return false;
        }
        if !right.iter().any(|b| self.null_required.contains(b)) || right.iter().any(|b| self.required.contains(b)) {
            return false;
        }
        let matched_non_null: Bindings = conditions
            .iter()
            .filter(|c| filters_nulls(c.comparison))
            .filter_map(|c| column_binding(&c.right))
            .collect();
        if right
            .iter()
            .any(|b| self.null_required.contains(b) && matched_non_null.contains(b))
        {
            *join_type = JoinType::Anti;
            return true;
        }
        false
    }

    /// `SimplifyOuterJoinType`.
    fn simplify_join_type(&self, op: &mut LogicalOperator) {
        let rejects: Vec<bool> = op
            .children
            .iter()
            .map(|child| child.column_bindings().iter().any(|b| self.null_filtered.contains(b)))
            .collect();
        let K::Join { join_type, .. } = &mut op.kind else {
            return;
        };
        let mut preserves = [
            matches!(join_type, JoinType::Left | JoinType::Outer),
            matches!(join_type, JoinType::Right | JoinType::Outer),
        ];
        // rejecting the NULLs of one side drops the NULL-extended rows of the other
        for (side, rejected) in rejects.iter().enumerate() {
            if *rejected {
                preserves[1 - side] = false;
            }
        }
        *join_type = match preserves {
            [false, false] => JoinType::Inner,
            [true, false] => JoinType::Left,
            [false, true] => JoinType::Right,
            [true, true] => JoinType::Outer,
        };
    }

    fn visit_outer(&mut self, op: &mut LogicalOperator) {
        if self.try_convert_left_to_anti(op) {
            let right = op.children[1].column_bindings();
            if let K::Join {
                conditions, predicate, ..
            } = &op.kind
            {
                let (conditions, predicate) = (conditions.clone(), predicate.clone());
                self.add_required_conditions(&conditions, predicate.as_ref());
            }
            for binding in &right {
                self.null_required.remove(binding);
            }
            self.visit_children(op);
            // only the operators above the join see its right side as NULL
            self.eliminated_null.extend(right);
            return;
        }
        self.simplify_join_type(op);
        let K::Join {
            join_type,
            conditions,
            predicate,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let join_type = *join_type;
        if join_type == JoinType::Inner {
            self.visit(op);
            return;
        }
        let (conditions, predicate) = (conditions.clone(), predicate.clone());
        self.add_required_conditions(&conditions, predicate.as_ref());
        // the NULLs of the NULL-extended side can be this join's own
        if matches!(join_type, JoinType::Left | JoinType::Outer) {
            for binding in op.children[1].column_bindings() {
                self.null_required.remove(&binding);
            }
        }
        if matches!(join_type, JoinType::Right | JoinType::Outer) {
            for binding in op.children[0].column_bindings() {
                self.null_required.remove(&binding);
            }
        }
        self.visit_children(op);
    }

    fn visit_projection(&mut self, op: &mut LogicalOperator) {
        let K::Projection {
            table_index,
            expressions,
        } = &op.kind
        else {
            unreachable!()
        };
        let table_index = *table_index;
        let mut direct = Vec::new();
        for (at, expr) in expressions.clone().iter().enumerate() {
            let binding = ColumnBinding::new(table_index, at);
            let BoundExprKind::ColumnRef { binding: input, .. } = &expr.kind else {
                self.add_required(expr);
                continue;
            };
            direct.push((at, *input));
            if self.null_filtered.contains(&binding) {
                self.null_filtered.insert(*input);
            }
            if self.null_required.contains(&binding) {
                self.null_required.insert(*input);
            }
        }
        self.visit_children(op);
        let K::Projection { expressions, .. } = &mut op.kind else {
            unreachable!()
        };
        for (at, input) in direct {
            if !self.eliminated_null.contains(&input) {
                continue;
            }
            let ty = expressions[at].return_type.clone();
            expressions[at] = BoundExpr::constant(Value::null(ty));
            self.eliminated_null.insert(ColumnBinding::new(table_index, at));
        }
    }

    fn visit_filter(&mut self, op: &mut LogicalOperator) {
        let K::Filter { expressions } = &mut op.kind else {
            unreachable!()
        };
        super::rewriter::split_predicates(expressions);
        for expr in expressions.clone() {
            self.handle_filter(&expr);
        }
        self.visit_children(op);
        let K::Filter { expressions } = &mut op.kind else {
            unreachable!()
        };
        let eliminated = &self.eliminated_null;
        expressions.retain(|expr| is_null_filter(expr).is_none_or(|binding| !eliminated.contains(&binding)));
        if expressions.is_empty() {
            *op = op.children.pop().expect("a filter has a child");
        }
    }
}
