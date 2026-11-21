// Ported from duckdb/src/optimizer/expression_heuristics.cpp @ rust-port-baseline
//
//! `ExpressionHeuristics` (`REORDER_FILTER`): the expressions of a filter and the children of a
//! conjunction run cheapest first, by a cost of what they compute. Nothing moves where one of them
//! can fail or holds a barrier.

use super::rewriter::can_throw;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::types::{LogicalTypeId, PhysicalType};

/// `ExpressionHeuristics::Rewrite`.
pub fn reorder_filters(mut plan: LogicalOperator) -> LogicalOperator {
    visit(&mut plan);
    plan
}

fn visit(op: &mut LogicalOperator) {
    if let K::Filter { expressions } = &mut op.kind
        && expressions.len() > 1
    {
        reorder(expressions);
    }
    for child in &mut op.children {
        visit(child);
    }
    // the filters of a scan are its table filters, which are not expressions of an operator in C++
    if matches!(op.kind, K::Get { .. } | K::ParquetScan { .. } | K::CsvScan { .. } | K::JsonScan { .. }) {
        return;
    }
    for expr in op.expressions_mut() {
        reorder_conjunctions(expr);
    }
}

/// `VisitReplace(BoundConjunctionExpression)`, everywhere in the expression.
fn reorder_conjunctions(expr: &mut BoundExpr) {
    for child in expr.children_mut() {
        reorder_conjunctions(child);
    }
    if let BoundExprKind::Conjunction { children, .. } = &mut expr.kind {
        reorder(children);
    }
}

fn has_barrier(expr: &BoundExpr) -> bool {
    let mut found = false;
    expr.clone().walk_mut(&mut |e| {
        found |= matches!(&e.kind, BoundExprKind::Function { function, .. } if function.name == "__internal_barrier");
    });
    found
}

/// `ReorderExpressions`.
fn reorder(expressions: &mut Vec<BoundExpr>) {
    if expressions.iter().any(|e| can_throw(e) || has_barrier(e)) {
        return;
    }
    let mut costed: Vec<(u64, BoundExpr)> = expressions.drain(..).map(|e| (cost(&e), e)).collect();
    // `std::sort` of so few is an insertion sort, which keeps ties in order
    costed.sort_by_key(|(cost, _)| *cost);
    expressions.extend(costed.into_iter().map(|(_, e)| e));
}

/// `ExpressionCost(PhysicalType, multiplier)`.
fn type_cost(ty: &duckrs_common::types::LogicalType, multiplier: u64) -> u64 {
    match ty.physical_type() {
        PhysicalType::Varchar => 5 * multiplier,
        PhysicalType::Float | PhysicalType::Double => 2 * multiplier,
        _ => multiplier,
    }
}

/// `ExpressionHeuristics::Cost`.
fn cost(expr: &BoundExpr) -> u64 {
    match &expr.kind {
        BoundExprKind::Case { checks, else_expr } => {
            checks.iter().map(|(when, then)| cost(when) + cost(then)).sum::<u64>() + cost(else_expr)
        }
        BoundExprKind::Conjunction { children, .. } => 5 + children.iter().map(cost).sum::<u64>(),
        BoundExprKind::Between { input, lower, upper } => cost(input) + cost(lower) + cost(upper) + 10,
        BoundExprKind::Comparison { left, right, .. } => cost(left) + 5 + cost(right),
        BoundExprKind::Cast { child, .. } => {
            let source = &child.return_type;
            let target = &expr.return_type;
            let cast_cost = if target == source {
                0
            } else if [target.id(), source.id()]
                .iter()
                .any(|id| matches!(id, LogicalTypeId::Varchar | LogicalTypeId::Blob))
            {
                200
            } else {
                5
            };
            cost(child) + cast_cost
        }
        BoundExprKind::Function { function, children, .. } => {
            let children_cost: u64 = children.iter().map(cost).sum();
            let own = match function.name.as_str() {
                "+" | "-" | "&" | "#" | ">>" | "<<" | "abs" => 5,
                "*" | "%" => 10,
                "/" => 15,
                "date_part" | "year" => 20,
                "round" => 100,
                "~~" | "!~~" | "regexp_matches" | "||" => 200,
                _ => 1000,
            };
            children_cost + own
        }
        BoundExprKind::Operator { op, children } => {
            let sum: u64 = children.iter().map(cost).sum();
            match op {
                BoundOperator::IsNull | BoundOperator::IsNotNull => sum + 5,
                BoundOperator::In | BoundOperator::NotIn => sum + (children.len() as u64 - 1) * 100,
                BoundOperator::Not => sum + 10,
                _ => sum + 1000,
            }
        }
        BoundExprKind::ColumnRef { .. } | BoundExprKind::Reference(_) => type_cost(&expr.return_type, 8),
        BoundExprKind::Constant(_) => type_cost(&expr.return_type, 1),
        _ => 1000,
    }
}
