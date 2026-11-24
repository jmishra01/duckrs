// Ported in spirit from duckdb/src/optimizer/unnest_rewriter.cpp @ rust-port-baseline
//
//! `UnnestRewriter`: a LATERAL subquery that only unnests columns of the row it is joined to,
//! `FROM t, (SELECT UNNEST(t.l))`, is an UNNEST over `t`. C++ finds it as a DELIM join over a
//! DELIM_GET; duckrs plans LATERAL subqueries as a lateral join that runs its right side for each
//! row, so it finds that: an INNER lateral join whose right side is a projection over an UNNEST
//! of nothing but columns of the left side. The UNNEST then goes over the left side, and a
//! projection gives the columns of both in the order of the join.

use super::common_subplan::{max_table_index, visit_bindings};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_parser::ast::JoinType;
use std::collections::HashMap;

pub fn rewrite(mut plan: LogicalOperator) -> LogicalOperator {
    // the operators above a rewritten join read its columns under new bindings, which reaches
    // neither into subqueries nor into the right sides of other lateral joins
    if count_laterals(&plan) == 0 || count_laterals(&plan) > 1 || has_subquery(&plan) {
        return plan;
    }
    let mut next = max_table_index(&plan) + 1;
    let mut replacements = HashMap::new();
    visit(&mut plan, &mut next, &mut replacements);
    plan
}

fn count_laterals(op: &LogicalOperator) -> usize {
    let own = usize::from(matches!(op.kind, K::LateralJoin { .. }));
    let inner = match &op.kind {
        K::LateralJoin { right, .. } | K::RecursiveCte { recursive: right, .. } => count_laterals(right),
        _ => 0,
    };
    own + inner + op.children.iter().map(count_laterals).sum::<usize>()
}

fn has_subquery(op: &LogicalOperator) -> bool {
    let mut op = op.clone();
    let mut found = false;
    for expr in op.expressions_mut() {
        expr.walk_mut(&mut |e| found |= matches!(e.kind, BoundExprKind::Subquery { .. }));
    }
    found || matches!(op.kind, K::RecursiveCte { .. }) || op.children.iter().any(has_subquery)
}

fn visit(op: &mut LogicalOperator, next: &mut usize, replacements: &mut HashMap<ColumnBinding, ColumnBinding>) {
    for child in &mut op.children {
        visit(child, next, replacements);
    }
    if !replacements.is_empty() {
        visit_bindings(&mut op.kind, &mut |binding, depth| {
            if depth == 0
                && let Some(new) = replacements.get(binding)
            {
                *binding = *new;
            }
        });
    }
    if let Some(rewritten) = try_rewrite(op, next, replacements) {
        *op = rewritten;
    }
}

/// The columns of the left side that the right side reads, as the left side has them.
fn substitute(expr: &mut BoundExpr, bindings: &[ColumnBinding], correlated: &[BoundExpr]) -> bool {
    let mut ok = true;
    expr.walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth } = &e.kind
            && *depth > 0
        {
            match bindings.iter().position(|b| b == binding) {
                Some(at) if *depth == 1 => {
                    let alias = e.alias.clone();
                    *e = correlated[at].clone();
                    e.alias = alias;
                }
                _ => ok = false,
            }
        }
        if matches!(e.kind, BoundExprKind::Subquery { .. }) {
            ok = false;
        }
    });
    ok
}

fn try_rewrite(
    op: &mut LogicalOperator,
    next: &mut usize,
    replacements: &mut HashMap<ColumnBinding, ColumnBinding>,
) -> Option<LogicalOperator> {
    let K::LateralJoin {
        join_type: JoinType::Inner,
        right,
        bindings,
        correlated,
        predicate: None,
    } = &op.kind
    else {
        return None;
    };
    // `projection <- UNNEST <- dummy scan`, the plan of a subquery without FROM
    let K::Projection {
        table_index: projection_index,
        expressions,
    } = &right.kind
    else {
        return None;
    };
    let [unnest] = right.children.as_slice() else {
        return None;
    };
    let K::Unnest {
        table_index: unnest_index,
        expressions: unnested,
    } = &unnest.kind
    else {
        return None;
    };
    if !matches!(
        unnest.children.as_slice(),
        [LogicalOperator {
            kind: K::DummyScan { .. },
            ..
        }]
    ) {
        return None;
    }
    // the columns of the dummy scan stand for nothing on the left side
    let dummy = unnest.children[0].column_bindings();
    let reads_dummy = |exprs: &[BoundExpr]| {
        let mut found = false;
        for expr in exprs {
            expr.clone().walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
                    found |= dummy.contains(binding);
                }
            });
        }
        found
    };
    if reads_dummy(expressions) || reads_dummy(unnested) {
        return None;
    }
    let mut unnested = unnested.clone();
    let mut expressions = expressions.clone();
    for expr in unnested.iter_mut().chain(expressions.iter_mut()) {
        if !substitute(expr, bindings, correlated) {
            return None;
        }
    }
    let (projection_index, unnest_index) = (*projection_index, *unnest_index);
    let left = op.children.pop().expect("a lateral join has a left side");
    let (left_bindings, left_types) = (left.column_bindings(), left.types());
    let unnest = LogicalOperator::new(
        K::Unnest {
            table_index: unnest_index,
            expressions: unnested,
        },
        vec![left],
    );
    // the columns of the join, the left side's first
    let table_index = *next;
    *next += 1;
    let mut columns = Vec::with_capacity(left_bindings.len() + expressions.len());
    for (at, (binding, ty)) in left_bindings.iter().zip(left_types).enumerate() {
        columns.push(BoundExpr::new(
            BoundExprKind::ColumnRef {
                binding: *binding,
                depth: 0,
            },
            ty,
        ));
        replacements.insert(*binding, ColumnBinding::new(table_index, at));
    }
    for (at, expr) in expressions.into_iter().enumerate() {
        replacements.insert(
            ColumnBinding::new(projection_index, at),
            ColumnBinding::new(table_index, left_bindings.len() + at),
        );
        columns.push(expr);
    }
    Some(LogicalOperator::new(
        K::Projection {
            table_index,
            expressions: columns,
        },
        vec![unnest],
    ))
}
