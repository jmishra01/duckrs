// Ported from duckdb/src/optimizer/in_clause_rewriter.cpp @ rust-port-baseline
//
//! `InClauseRewriter` (`IN_CLAUSE`): `x IN (c1, ..., cn)` with many constants in a filter or a
//! projection becomes a MARK join of the operator's child with the constants, and the IN the
//! join's marker. The binder spells out the lists this does not take (`bind_in`); what of an IN is
//! left after this pass, becomes such ORs too.

use super::common_subplan::max_table_index;
use super::rewriter::rewrite_expression;
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding, in_as_comparisons};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use std::sync::Arc;

/// `InClauseRewriter::IN_CLAUSE_REWRITE_THRESHOLD`: the children (input and values) of an IN that
/// becomes a join.
pub const IN_CLAUSE_REWRITE_THRESHOLD: usize = 6;

/// `InClauseRewriter::Rewrite` over the plan.
pub fn rewrite(mut plan: LogicalOperator) -> LogicalOperator {
    if contains_in(&mut plan) {
        let mut next_table_index = max_table_index(&plan) + 1;
        visit_operator(&mut plan, &mut next_table_index, false);
    }
    plan
}

fn contains_in(op: &mut LogicalOperator) -> bool {
    op.expressions_mut().iter().any(|e| has_in(e))
        || matches!(&op.kind, K::Join { conditions, .. } | K::DelimJoin { conditions, .. } | K::MarkJoin { conditions, .. }
            if conditions.iter().any(|c| has_in(&c.left) || has_in(&c.right)))
        || op.children.iter_mut().any(contains_in)
        || match &mut op.kind {
            K::LateralJoin { right, .. } | K::RecursiveCte { recursive: right, .. } => contains_in(right),
            _ => false,
        }
}

struct Rewriter<'a> {
    /// the child of the operator whose expressions are visited, with the joins put on top of it
    root: LogicalOperator,
    next_table_index: &'a mut usize,
    /// whether a MARK join went on top of `root`
    joined: bool,
}

/// `binds` says the parent reads the columns of `op` by their bindings and puts out columns of its
/// own, so that one more column of `op` does not change what the parent puts out.
fn visit_operator(op: &mut LogicalOperator, next_table_index: &mut usize, binds: bool) {
    let mut hidden = None;
    if matches!(op.kind, K::Filter { .. } | K::Projection { .. }) && op.expressions_mut().iter().any(|e| has_in(e)) {
        let child = std::mem::replace(&mut op.children[0], LogicalOperator::new(K::CrossProduct, Vec::new()));
        let scan = matches!(op.kind, K::Filter { .. }).then_some(&child).filter(|c| matches!(c.kind, K::Get { .. }));
        // `PushdownGet` had put a dense list of integers into the scan as a range, and the list of
        // a column with an index into the scan for an index scan (an optional filter in C++, which
        // duckrs does not have)
        let keep: Vec<bool> = match &op.kind {
            K::Filter { expressions } => expressions
                .iter()
                .map(|e| scan.is_some_and(|scan| is_dense_column_in(e) || is_indexed_column_in(e, scan)))
                .collect(),
            _ => Vec::new(),
        };
        let child_bindings = child.column_bindings();
        let mut rewriter = Rewriter {
            root: child,
            next_table_index,
            joined: false,
        };
        for (at, expr) in op.expressions_mut().into_iter().enumerate() {
            if keep.get(at).copied().unwrap_or(false) {
                continue;
            }
            rewriter.visit(expr);
        }
        hidden = rewriter.joined.then_some(child_bindings);
        op.children[0] = rewriter.root;
    }
    lower_remaining(op);
    let own_binds = matches!(op.kind, K::Projection { .. } | K::Aggregate { .. });
    for child in &mut op.children {
        visit_operator(child, next_table_index, own_binds);
    }
    match &mut op.kind {
        K::LateralJoin { right, .. } | K::RecursiveCte { recursive: right, .. } => {
            visit_operator(right, next_table_index, false)
        }
        _ => {}
    }
    // a filter has the columns of its child, which now has the markers too: C++ projects them out
    // with the filter's projection map
    if let Some(keep) = hidden.filter(|_| !binds && matches!(op.kind, K::Filter { .. })) {
        let filter = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *op = LogicalOperator::new(K::ColumnSelect { keep }, vec![filter]);
    }
}

/// `col IN (c1, ...)` of non-NULL constants on a column of the scan that a key of one column indexes.
fn is_indexed_column_in(expr: &BoundExpr, scan: &LogicalOperator) -> bool {
    let (
        BoundExprKind::Operator {
            op: BoundOperator::In,
            children,
        },
        K::Get { table, column_ids, .. },
    ) = (&expr.kind, &scan.kind)
    else {
        return false;
    };
    let BoundExprKind::ColumnRef { binding, depth: 0 } = &children[0].kind else {
        return false;
    };
    let Some(&column) = column_ids.get(binding.column_index) else {
        return false;
    };
    children[1..]
        .iter()
        .all(|c| matches!(&c.kind, BoundExprKind::Constant(v) if !v.is_null()))
        && table
            .scan_keys()
            .iter()
            .any(|key| key.columns.as_slice() == [column] && key.expressions.is_none())
}

fn has_in(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Operator { op, .. } if op.is_in()) || expr.children().into_iter().any(has_in)
}

/// The INs that stay, as their ORs, simplified the way the rewriter would have.
fn lower_remaining(op: &mut LogicalOperator) {
    let is_filter = matches!(op.kind, K::Filter { .. });
    let lower = |expr: &mut BoundExpr, in_filter: bool| {
        if !has_in(expr) {
            return;
        }
        expr.walk_mut(&mut |e| {
            if let BoundExprKind::Operator { op, children } = &e.kind
                && op.is_in()
            {
                let alias = e.alias.clone();
                *e = in_as_comparisons(*op == BoundOperator::NotIn, children).with_alias(alias);
            }
        });
        rewrite_expression(expr, in_filter);
    };
    for expr in op.expressions_mut() {
        lower(expr, is_filter);
    }
    if let K::Join { conditions, .. } | K::DelimJoin { conditions, .. } | K::MarkJoin { conditions, .. } = &mut op.kind
    {
        for condition in conditions {
            lower(&mut condition.left, false);
            lower(&mut condition.right, false);
        }
    }
}

/// `col IN (1, 2, 3, ...)` of consecutive integers (`FilterCombiner::IsDenseRange`).
fn is_dense_column_in(expr: &BoundExpr) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::In,
        children,
    } = &expr.kind
    else {
        return false;
    };
    if !matches!(children[0].kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
        return false;
    }
    let mut values = Vec::with_capacity(children.len() - 1);
    for child in &children[1..] {
        let BoundExprKind::Constant(value) = &child.kind else {
            return false;
        };
        if value.is_null() || !value.logical_type().is_integral() || value.logical_type() == &LogicalType::UHUGEINT {
            return false;
        }
        let Some(value) = value.as_i128() else {
            return false;
        };
        values.push(value);
    }
    values.sort_unstable();
    values.windows(2).all(|pair| pair[1].checked_sub(pair[0]) == Some(1))
}

impl Rewriter<'_> {
    fn take_index(&mut self) -> usize {
        let index = *self.next_table_index;
        *self.next_table_index += 1;
        index
    }

    /// `InClauseRewriter::VisitExpression`.
    fn visit(&mut self, expr: &mut BoundExpr) {
        match &mut expr.kind {
            // an IN below an error capture must stay an expression, the join would raise outside it
            BoundExprKind::Operator {
                op: BoundOperator::Try | BoundOperator::HasError,
                ..
            } => return,
            // only the first child of these runs for every row
            BoundExprKind::Conjunction { .. }
            | BoundExprKind::Case { .. }
            | BoundExprKind::Operator {
                op: BoundOperator::Coalesce,
                ..
            } => {
                if let Some(first) = expr.children_mut().into_iter().next() {
                    self.visit(first);
                }
                return;
            }
            _ => {}
        }
        if let Some(replacement) = self.replace(expr) {
            *expr = replacement;
            return;
        }
        for child in expr.children_mut() {
            self.visit(child);
        }
    }

    /// `InClauseRewriter::VisitReplace(BoundOperatorExpression)`.
    fn replace(&mut self, expr: &mut BoundExpr) -> Option<BoundExpr> {
        let BoundExprKind::Operator { op, children } = &mut expr.kind else {
            return None;
        };
        if !op.is_in() {
            return None;
        }
        let negated = *op == BoundOperator::NotIn;
        let rewrite_to_join =
            children.len() >= IN_CLAUSE_REWRITE_THRESHOLD && children[1..].iter().all(BoundExpr::is_foldable);
        if !rewrite_to_join {
            return None;
        }
        let in_type = children[0].return_type.clone();
        let mut values = Vec::with_capacity(children.len() - 1);
        for child in &children[1..] {
            values.push(try_evaluate_scalar(child)?);
        }
        let chunks = chunks_of(&in_type, &values)?;
        let chunk_index = self.take_index();
        let chunk_scan = LogicalOperator::new(
            K::ChunkGet {
                table_index: chunk_index,
                types: vec![in_type.clone()],
                chunks: Arc::new(chunks),
            },
            Vec::new(),
        );
        let mark_index = self.take_index();
        self.joined = true;
        let input = children.swap_remove(0);
        let left = std::mem::replace(&mut self.root, LogicalOperator::new(K::CrossProduct, Vec::new()));
        self.root = LogicalOperator::new(
            K::MarkJoin {
                table_index: mark_index,
                conditions: vec![JoinCondition {
                    left: input,
                    right: BoundExpr::column_ref(ColumnBinding::new(chunk_index, 0), in_type, ""),
                    null_safe: false,
                    filter: None,
                }],
                predicate: None,
            },
            vec![left, chunk_scan],
        );
        let marker = BoundExpr::column_ref(ColumnBinding::new(mark_index, 0), LogicalType::BOOLEAN, "IN (...)");
        let result = if negated {
            BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::Not,
                    children: vec![marker],
                },
                LogicalType::BOOLEAN,
            )
        } else {
            marker
        };
        Some(if expr.alias.is_empty() {
            result
        } else {
            result.with_alias(expr.alias.clone())
        })
    }
}

/// The values as chunks of one column of `ty`.
fn chunks_of(ty: &LogicalType, values: &[Value]) -> Option<Vec<DataChunk>> {
    values
        .chunks(STANDARD_VECTOR_SIZE)
        .map(|part| {
            let mut vector = Vector::new(ty.clone(), part.len());
            for (row, value) in part.iter().enumerate() {
                vector.set_value(row, value).ok()?;
            }
            Some(DataChunk::from_columns(vec![vector], part.len()))
        })
        .collect()
}
