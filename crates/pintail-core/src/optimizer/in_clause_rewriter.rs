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
use pintail_common::STANDARD_VECTOR_SIZE;
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use pintail_common::vector::{DataChunk, Vector};
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
    // what pintail keeps as the predicate of an inner join is a filter above it in C++, where the
    // rewriter finds its IN lists
    if let K::Join {
        join_type: pintail_parser::ast::JoinType::Inner,
        predicate: predicate @ Some(_),
        ..
    } = &mut op.kind
        && predicate.as_ref().is_some_and(has_rewritable_in)
    {
        let mut expressions = Vec::new();
        super::rewriter::split_conjuncts(predicate.take().expect("a predicate"), &mut expressions);
        let join = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *op = LogicalOperator::new(K::Filter { expressions }, vec![join]);
    }
    let mut hidden = None;
    // `PushdownGet` puts `NOT (col IN (...))` into the scan as it is, which stays an IN there
    let mut kept_in_scan = Vec::new();
    if matches!(op.kind, K::Filter { .. } | K::Projection { .. }) && op.expressions_mut().iter().any(|e| has_in(e)) {
        if let K::Filter { expressions } = &op.kind
            && matches!(op.children[0].kind, K::Get { .. })
        {
            let scan = op.children[0].column_bindings();
            kept_in_scan = expressions.iter().map(|e| is_scan_not_in(e, &scan)).collect();
            // and `col IN (...)` there as an optional filter, as well as here; the shorter lists
            // `push_into_scans` sees as the ORs [`lower_remaining`] makes of them
            let optional: Vec<BoundExpr> = expressions
                .iter()
                .filter(|e| {
                    matches!(&e.kind, BoundExprKind::Operator { op: BoundOperator::In, children }
                        if children.len() >= IN_CLAUSE_REWRITE_THRESHOLD)
                        && is_constant_column_in(e, &scan)
                        && !is_dense_column_in(e)
                })
                .cloned()
                .collect();
            if let K::Get { optional_filters, .. } = &mut op.children[0].kind {
                optional_filters.extend(optional);
            }
        }
        let child = std::mem::replace(&mut op.children[0], LogicalOperator::new(K::CrossProduct, Vec::new()));
        let scan = matches!(op.kind, K::Filter { .. })
            .then_some(&child)
            .filter(|c| matches!(c.kind, K::Get { .. }));
        // `PushdownGet` had put a dense list of integers into the scan as a range, and the list of
        // a column with an index into the scan for an index scan (an optional filter in C++, which
        // pintail does not have)
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
            if keep.get(at).copied().unwrap_or(false) || kept_in_scan.get(at).copied().unwrap_or(false) {
                continue;
            }
            rewriter.visit(expr);
        }
        hidden = rewriter.joined.then_some(child_bindings);
        op.children[0] = rewriter.root;
    }
    lower_remaining(op, &kept_in_scan);
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

/// `col IN (c1, ...)` of non-NULL constants on a column of the scan.
fn is_constant_column_in(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let BoundExprKind::Operator {
        op: BoundOperator::In,
        children,
    } = &expr.kind
    else {
        return false;
    };
    matches!(&children[0].kind, BoundExprKind::ColumnRef { binding, depth: 0 } if scan.contains(binding))
        && children[1..]
            .iter()
            .all(|c| matches!(&c.kind, BoundExprKind::Constant(v) if !v.is_null()))
}

/// `NOT (col IN (...))` (or `col NOT IN (...)`) of one column of the scan, which `TryPushdownGenericExpression` moves into it.
fn is_scan_not_in(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let list = match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::Not,
            children,
        } => &children[0],
        _ => expr,
    };
    let BoundExprKind::Operator { op, children } = &list.kind else {
        return false;
    };
    if !(*op == BoundOperator::NotIn || (*op == BoundOperator::In && !std::ptr::eq(list, expr))) {
        return false;
    }
    fn columns_of(expr: &BoundExpr, columns: &mut Vec<(ColumnBinding, usize)>) {
        if let BoundExprKind::ColumnRef { binding, depth } = &expr.kind {
            columns.push((*binding, *depth));
        }
        for child in expr.children() {
            columns_of(child, columns);
        }
    }
    let mut columns = Vec::new();
    columns_of(expr, &mut columns);
    matches!(children[0].kind, BoundExprKind::ColumnRef { .. })
        && !columns.is_empty()
        && columns
            .iter()
            .all(|c| c.1 == 0 && c.0 == columns[0].0 && scan.contains(&c.0))
        && !expr.is_volatile()
        && !super::rewriter::can_throw(expr)
}
/// `col IN (c1, ...)` of non-NULL constants on a column of the scan that a key or an index of just that column
/// indexes.
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
        && table.scan_keys().iter().any(|key| {
            // a constraint's key is its columns, an index's its expressions: a plain one is the column itself
            let plain = key.expressions.as_ref().is_none_or(|expressions| {
                matches!(expressions.as_slice(), [expression] if matches!(expression.kind, BoundExprKind::Reference(_)))
            });
            plain && key.columns.as_slice() == [column]
        })
}

/// An IN that becomes a join: of enough values, all constants.
/// `InClauseRewriter::HasRewritableInClause`: an IN of enough constants for the rewriter to make it a join.
pub(super) fn has_rewritable_in(expr: &BoundExpr) -> bool {
    let own = matches!(&expr.kind, BoundExprKind::Operator { op, children }
        if op.is_in()
            && children.len() >= IN_CLAUSE_REWRITE_THRESHOLD
            && children[1..].iter().all(BoundExpr::is_foldable));
    own || expr.children().into_iter().any(has_rewritable_in)
}

fn has_in(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Operator { op, .. } if op.is_in()) || expr.children().into_iter().any(has_in)
}

/// The INs that stay, as their ORs, simplified the way the rewriter would have.
fn lower_remaining(op: &mut LogicalOperator, kept: &[bool]) {
    // the filters of a scan are its table filters, which the rewriter does not see
    if matches!(op.kind, K::Get { .. }) {
        return;
    }
    let is_filter = matches!(op.kind, K::Filter { .. });
    let lower = |expr: &mut BoundExpr, in_filter: bool| {
        if !has_in(expr) {
            return;
        }
        expr.walk_mut(&mut |e| {
            // a volatile input runs once for the IN, and would run once per value in the ORs
            if let BoundExprKind::Operator { op, children } = &e.kind
                && op.is_in()
                && !children[0].is_volatile()
            {
                let alias = e.alias.clone();
                *e = in_as_comparisons(*op == BoundOperator::NotIn, children).with_alias(alias);
            }
        });
        rewrite_expression(expr, in_filter);
    };
    for (at, expr) in op.expressions_mut().into_iter().enumerate() {
        if !kept.get(at).copied().unwrap_or(false) {
            lower(expr, is_filter);
        }
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
                    bound: None,
                    left: input,
                    right: BoundExpr::column_ref(ColumnBinding::new(chunk_index, 0), in_type, ""),
                    comparison: pintail_parser::ast::ComparisonOp::Equal,
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
