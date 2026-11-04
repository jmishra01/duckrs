// Ported from duckdb/src/planner/binder/tableref/{bind_joinref,plan_joinref}.cpp and
// duckdb/src/planner/operator/logical_comparison_join.cpp @ rust-port-baseline
//
//! Binding and planning JOIN: each side binds on its own, so that ON sees only this join's
//! columns, and USING and NATURAL become equality conditions plus [`UsingColumnSet`]s that say
//! what the shared column means afterwards.

use super::expression::{bind_comparison, checked_cast};
use super::statements::not_implemented;
use super::{BindContext, Binder, UsingColumnSet, binder_error};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind};
use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{
    ComparisonOp, Expr, ExprKind, JoinRef, JoinRefType, JoinType, TableRef, sql_quoted_identifier,
};
use std::collections::HashSet;

/// `Binder::TryFindBinding`: the one binding of `context` with this column.
fn try_find_binding(context: &BindContext, column: &str, side: &str) -> Result<Option<String>> {
    let bindings = context.matching_bindings(column);
    if bindings.len() > 1 {
        let candidates: String = bindings
            .iter()
            .map(|b| format!("\n\t{}.{}", b.alias, context.actual_column_name(&b.alias, column)))
            .collect();
        return Err(binder_error(format!(
            "Column name \"{column}\" is ambiguous: it exists more than once on {side} side of join.\nCandidates:{candidates}"
        )));
    }
    Ok(bindings.first().map(|b| b.alias.clone()))
}

/// `Binder::RetrieveUsingBinding`: the binding a USING column reads on one side, or `None` for
/// a chained column without a primary binding, which binds by its bare name.
fn using_binding(
    context: &BindContext,
    set: Option<&UsingColumnSet>,
    column: &str,
    side: &str,
) -> Result<Option<String>> {
    match set {
        Some(set) => Ok(set.primary.clone()),
        None => try_find_binding(context, column, side)?.map(Some).ok_or_else(|| {
            binder_error(format!(
                "Column {} does not exist on {side} side of join!",
                sql_quoted_identifier(column)
            ))
        }),
    }
}

/// `SetPrimaryBinding`: the side whose value a USING column always has. A FULL join has none.
fn primary_binding(join_type: JoinType, left: &Option<String>, right: &Option<String>) -> Option<String> {
    match join_type {
        JoinType::Left | JoinType::Inner | JoinType::Semi | JoinType::Anti => left.clone(),
        JoinType::Right | JoinType::RightSemi | JoinType::RightAnti => right.clone(),
        _ => None,
    }
}

/// The columns NATURAL joins on: those of the left side that the right side has too.
fn natural_columns(left: &BindContext, right: &BindContext) -> Result<Vec<String>> {
    let mut columns: Vec<String> = Vec::new();
    for name in left.bindings.iter().flat_map(|b| &b.names) {
        if right.using_binding(name)?.is_some() || try_find_binding(right, name, "right")?.is_some() {
            columns.push(name.clone());
        }
    }
    if columns.is_empty() {
        let candidates = |context: &BindContext| -> String {
            let names: Vec<String> = context
                .bindings
                .iter()
                .flat_map(|b| b.names.iter().map(|n| format!("{}.{n}", b.alias)))
                .collect();
            names.join(", ")
        };
        return Err(binder_error(format!(
            "No columns found to join on in NATURAL JOIN.\nUse CROSS JOIN if you intended for this to be a \
             cross-product.\n   Left candidates: {}\n   Right candidates: {}",
            candidates(left),
            candidates(right)
        )));
    }
    Ok(columns)
}

/// `LogicalFilter::SplitPredicates`: the conjuncts of an expression.
fn split_predicates(expr: BoundExpr, result: &mut Vec<BoundExpr>) {
    match expr.kind {
        BoundExprKind::Conjunction { and: true, children } => {
            for child in children {
                split_predicates(child, result);
            }
        }
        _ => result.push(expr),
    }
}

/// Whether an expression reads columns of `tables` (a part of `JoinSide::GetJoinSide`).
fn references(expr: &BoundExpr, tables: &HashSet<usize>) -> bool {
    match expr.kind {
        BoundExprKind::ColumnRef { binding, .. } => tables.contains(&binding.table_index),
        _ => expr.children().into_iter().any(|c| references(c, tables)),
    }
}

fn table_indexes(plan: &LogicalOperator) -> HashSet<usize> {
    plan.column_bindings()
        .iter()
        .map(|b: &ColumnBinding| b.table_index)
        .collect()
}

/// `ExtractJoinConditions`: the equalities between the two sides become join keys, and what
/// remains is checked for each pair of rows the keys match.
fn extract_join_conditions(
    predicates: Vec<BoundExpr>,
    left: &LogicalOperator,
    right: &LogicalOperator,
) -> (Vec<JoinCondition>, Option<BoundExpr>) {
    let left_tables = table_indexes(left);
    let right_tables = table_indexes(right);
    let only = |expr: &BoundExpr, tables: &HashSet<usize>, other: &HashSet<usize>| {
        references(expr, tables) && !references(expr, other)
    };
    let mut split = Vec::new();
    for predicate in predicates {
        split_predicates(predicate, &mut split);
    }
    let mut conditions = Vec::new();
    let mut residual = Vec::new();
    for expr in split {
        let BoundExprKind::Comparison {
            op: op @ (ComparisonOp::Equal | ComparisonOp::NotDistinctFrom),
            left: first,
            right: second,
        } = expr.kind
        else {
            residual.push(expr);
            continue;
        };
        let null_safe = op == ComparisonOp::NotDistinctFrom;
        if only(&first, &left_tables, &right_tables) && only(&second, &right_tables, &left_tables) {
            conditions.push(JoinCondition {
                left: *first,
                right: *second,
                null_safe,
            });
        } else if only(&first, &right_tables, &left_tables) && only(&second, &left_tables, &right_tables) {
            conditions.push(JoinCondition {
                left: *second,
                right: *first,
                null_safe,
            });
        } else {
            residual.push(BoundExpr::new(
                BoundExprKind::Comparison {
                    op,
                    left: first,
                    right: second,
                },
                LogicalType::BOOLEAN,
            ));
        }
    }
    let predicate = match residual.len() {
        0 => None,
        1 => residual.pop(),
        _ => Some(BoundExpr::new(
            BoundExprKind::Conjunction {
                and: true,
                children: residual,
            },
            LogicalType::BOOLEAN,
        )),
    };
    (conditions, predicate)
}

fn contains_subquery(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(contains_subquery)
}

/// All of `predicates`, if there are any.
fn conjunction_of(mut predicates: Vec<BoundExpr>) -> Option<BoundExpr> {
    match predicates.len() {
        0 => None,
        1 => predicates.pop(),
        _ => Some(BoundExpr::new(
            BoundExprKind::Conjunction {
                and: true,
                children: predicates,
            },
            LogicalType::BOOLEAN,
        )),
    }
}

impl Binder<'_> {
    /// Binds a reference to a USING column in `context`: `alias.column`, or the bare column when
    /// it has no binding of its own (`AddCondition`'s `BindColumn`).
    fn bind_using_side(
        &mut self,
        context: &mut BindContext,
        alias: &Option<String>,
        column: &str,
    ) -> Result<BoundExpr> {
        let names = match alias {
            Some(alias) => vec![alias.clone(), column.to_string()],
            None => vec![column.to_string()],
        };
        std::mem::swap(&mut self.bind_context, context);
        let bound = self.bind_expression(&Expr::from(ExprKind::ColumnRef(names)));
        std::mem::swap(&mut self.bind_context, context);
        bound
    }

    /// `Binder::Bind(JoinRef)` + `CreatePlan(BoundJoinRef)`. `self.bind_context` belongs to this
    /// join when it is called; the bindings of both sides end up in it.
    pub(super) fn bind_join(&mut self, table_ref: &TableRef, join: &JoinRef) -> Result<LogicalOperator> {
        if !table_ref.alias.is_empty() || !table_ref.column_name_alias.is_empty() {
            return Err(not_implemented("aliases on joins"));
        }
        match join.ref_type {
            JoinRefType::Regular | JoinRefType::Natural | JoinRefType::Cross => {}
            JoinRefType::Positional => return Err(not_implemented("POSITIONAL JOIN")),
            JoinRefType::Asof => return Err(not_implemented("ASOF JOIN")),
            JoinRefType::Nearest => return Err(not_implemented("NEAREST JOIN")),
        }
        if matches!(join.join_type, JoinType::Mark | JoinType::Single) {
            return Err(not_implemented(join.join_type.name()));
        }
        let left = self.bind_table_ref(&join.left)?;
        let left_context = std::mem::take(&mut self.bind_context);
        self.left_of_join.push(left_context);
        // the right side can read the columns of the left side
        let enclosing = std::mem::take(&mut self.correlated);
        self.outer_contexts
            .push(self.left_of_join.last().expect("pushed above").clone());
        let right = self.bind_table_ref(&join.right);
        self.outer_contexts.pop();
        let lateral_columns = std::mem::replace(&mut self.correlated, enclosing);
        let right = right?;
        let lateral_columns = lateral_columns.lock().clone();
        let mut left_context = self.left_of_join.pop().expect("pushed above");
        // what the left side does not have is correlated with a query around this one
        let (lateral_columns, outer_columns): (Vec<_>, Vec<_>) = lateral_columns.into_iter().partition(|column| {
            let table = column.binding.table_index;
            left_context.bindings.iter().any(|b| b.table_index == table)
        });
        self.correlated.lock().extend(outer_columns);
        let mut right_context = std::mem::take(&mut self.bind_context);

        let requested = if join.ref_type == JoinRefType::Natural {
            natural_columns(&left_context, &right_context)?
        } else {
            join.using_columns.clone()
        };
        let mut using_columns: Vec<String> = Vec::new();
        for column in requested {
            if !using_columns.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
                using_columns.push(column);
            }
        }
        // the sets of earlier USING joins, looked up before this join changes them
        let mut sets = Vec::with_capacity(using_columns.len());
        for column in &using_columns {
            let left_set = left_context.using_binding(column)?.cloned();
            let right_set = right_context.using_binding(column)?.cloned();
            if left_set.is_none() {
                left_context.matching_binding(column)?;
            }
            if right_set.is_none() {
                right_context.matching_binding(column)?;
            }
            sets.push((left_set, right_set));
        }
        let mut predicates = Vec::new();
        let mut new_sets = Vec::with_capacity(using_columns.len());
        for (column, (left_set, right_set)) in using_columns.iter().zip(sets) {
            let left_alias = using_binding(&left_context, left_set.as_ref(), column, "left")?;
            let right_alias = using_binding(&right_context, right_set.as_ref(), column, "right")?;
            let left_column = self.bind_using_side(&mut left_context, &left_alias, column)?;
            let right_column = self.bind_using_side(&mut right_context, &right_alias, column)?;
            predicates.push(bind_comparison(ComparisonOp::Equal, left_column, right_column)?);
            let mut bindings = Vec::new();
            for (set, alias) in [(&left_set, &left_alias), (&right_set, &right_alias)] {
                match set {
                    Some(set) => bindings.extend(set.bindings.iter().cloned()),
                    None => bindings.extend(alias.iter().cloned()),
                }
            }
            new_sets.push(UsingColumnSet {
                column: column.clone(),
                primary: primary_binding(join.join_type, &left_alias, &right_alias),
                bindings,
            });
            left_context.remove_using_binding(left_set.as_ref());
            right_context.remove_using_binding(right_set.as_ref());
        }

        let left_aliases = left_context.aliases();
        let right_aliases = right_context.aliases();
        self.bind_context.using_columns.extend(new_sets);
        self.bind_context.add_context(left_context);
        self.bind_context.add_context(right_context);

        // the condition as written comes first, then the USING and NATURAL ones
        if let Some(condition) = &join.condition {
            let aggregate_error =
                std::mem::replace(&mut self.aggregate_error, "WHERE clause cannot contain aggregates!");
            let bound = self.bind_expression(condition);
            self.aggregate_error = aggregate_error;
            predicates.insert(0, checked_cast(bound?, &LogicalType::BOOLEAN)?);
        }
        if !lateral_columns.is_empty() {
            if !matches!(join.join_type, JoinType::Inner | JoinType::Left) {
                return Err(binder_error(
                    "The combining JOIN type must be INNER or LEFT for a LATERAL reference",
                ));
            }
            if predicates.iter().any(contains_subquery) {
                return Err(binder_error("Subqueries are not supported in LATERAL join conditions"));
            }
            let mut correlated = Vec::with_capacity(lateral_columns.len());
            let mut bindings = Vec::with_capacity(lateral_columns.len());
            for column in lateral_columns {
                let names = Expr::from(ExprKind::ColumnRef(column.names));
                correlated.push(self.bind_expression(&names)?);
                bindings.push(column.binding);
            }
            return Ok(LogicalOperator::new(
                LogicalOperatorKind::LateralJoin {
                    join_type: join.join_type,
                    right: Box::new(right),
                    bindings,
                    correlated,
                    predicate: conjunction_of(predicates),
                },
                vec![left],
            ));
        }
        // these joins do not output the other side
        match join.join_type {
            JoinType::Semi | JoinType::Anti => self.bind_context.remove_context(&right_aliases),
            JoinType::RightSemi | JoinType::RightAnti => self.bind_context.remove_context(&left_aliases),
            _ => {}
        }
        if join.ref_type == JoinRefType::Cross {
            return Ok(LogicalOperator::new(
                LogicalOperatorKind::CrossProduct,
                vec![left, right],
            ));
        }
        if predicates.is_empty() {
            predicates.push(BoundExpr::constant(Value::boolean(true)));
        }
        let (conditions, predicate) = extract_join_conditions(predicates, &left, &right);
        Ok(LogicalOperator::new(
            LogicalOperatorKind::Join {
                join_type: join.join_type,
                conditions,
                predicate,
            },
            vec![left, right],
        ))
    }
}
