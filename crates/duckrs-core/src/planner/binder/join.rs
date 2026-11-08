// Ported from duckdb/src/planner/binder/tableref/{bind_joinref,plan_joinref}.cpp and
// duckdb/src/planner/operator/logical_comparison_join.cpp @ rust-port-baseline
//
//! Binding and planning JOIN: each side binds on its own, so that ON sees only this join's
//! columns, and USING and NATURAL become equality conditions plus [`UsingColumnSet`]s that say
//! what the shared column means afterwards.

use super::expression::{bind_comparison, checked_cast};
use super::statements::not_implemented;
use super::{BindContext, Binder, Binding, UsingColumnSet, binder_error};
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{JoinCondition, LogicalOperator, LogicalOperatorKind};
use duckrs_common::Result;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{
    ComparisonOp, Expr, ExprKind, JoinRef, JoinRefType, JoinType, Literal, NullOrder, OperatorType, OrderByNode,
    OrderType, QueryNode, ResultModifier, SelectNode, SelectStatement, TableRef, TableRefKind, sql_quoted_identifier,
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
pub(super) fn using_binding(
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
pub(super) fn primary_binding(join_type: JoinType, left: &Option<String>, right: &Option<String>) -> Option<String> {
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

/// `PlanMarkJoin`: an explicit MARK join only runs the comparison families the mark join
/// implements, which `ExtractJoinConditions` and `TryGetMarkJoinGroupTypes` decide.
fn plan_mark_join(predicates: &[BoundExpr], left: &LogicalOperator, right: &LogicalOperator) -> Result<()> {
    let unsupported = || crate::error_not_implemented("Unsupported explicit MARK join conditions");
    let left_tables = table_indexes(left);
    let right_tables = table_indexes(right);
    // `JoinSide::GetJoinSide`: 0 none, 1 left, 2 right, 3 both
    let side =
        |expr: &BoundExpr| u8::from(references(expr, &left_tables)) | (u8::from(references(expr, &right_tables)) << 1);
    let mut split = Vec::new();
    for predicate in predicates {
        split_predicates(predicate.clone(), &mut split);
    }
    // the comparisons, as (op, left type, right type)
    let mut conditions = Vec::with_capacity(split.len());
    for expr in &split {
        // `CanEliminate`: a constant TRUE condition drops out
        if matches!(&expr.kind, BoundExprKind::Constant(value) if *value == Value::boolean(true)) {
            continue;
        }
        let BoundExprKind::Comparison {
            op,
            left: first,
            right: second,
        } = &expr.kind
        else {
            return Err(unsupported());
        };
        if contains_subquery(expr) {
            return Err(unsupported());
        }
        let (a, b) = (side(first), side(second));
        if !matches!((a, b), (0 | 1, 0 | 2) | (0 | 2, 0 | 1)) {
            return Err(unsupported());
        }
        conditions.push((*op, &first.return_type, &second.return_type));
    }
    if mark_join_group_types(&conditions) {
        return Ok(());
    }
    let all_equal = !conditions.is_empty() && conditions.iter().all(|(op, ..)| *op == ComparisonOp::Equal);
    let null_safe = conditions.iter().any(|(op, ..)| *op == ComparisonOp::NotDistinctFrom)
        && conditions
            .iter()
            .all(|(op, ..)| matches!(op, ComparisonOp::NotDistinctFrom | ComparisonOp::DistinctFrom));
    if all_equal || null_safe || conditions.len() == 1 {
        return Ok(());
    }
    Err(unsupported())
}

/// `LogicalComparisonJoin::TryGetMarkJoinGroupTypes`: null-safe group keys, then one comparison.
fn mark_join_group_types(conditions: &[(ComparisonOp, &LogicalType, &LogicalType)]) -> bool {
    if conditions.len() < 2 {
        return false;
    }
    for (i, (op, lhs, rhs)) in conditions.iter().enumerate() {
        if lhs != rhs {
            return false;
        }
        if i + 1 < conditions.len() {
            if *op != ComparisonOp::NotDistinctFrom {
                return false;
            }
            continue;
        }
        let id = lhs.id();
        if id == LogicalTypeId::Union || (id == LogicalTypeId::Tuple && *op != ComparisonOp::Equal) {
            return false;
        }
        match op {
            ComparisonOp::Equal | ComparisonOp::NotEqual => {}
            ComparisonOp::LessThan
            | ComparisonOp::GreaterThan
            | ComparisonOp::LessThanOrEqual
            | ComparisonOp::GreaterThanOrEqual => {
                if lhs.is_nested() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// `ExtractJoinConditions`: the equalities between the two sides become join keys, and what
/// remains is checked for each pair of rows the keys match.
pub(super) fn extract_join_conditions(
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
pub(super) fn conjunction_of(mut predicates: Vec<BoundExpr>) -> Option<BoundExpr> {
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
    pub(super) fn bind_using_side(
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
        if join.ref_type == JoinRefType::Nearest {
            return self.bind_nearest_join(table_ref, join);
        }
        if join.join_type == JoinType::Single {
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
            // the last column of ASOF JOIN ... USING is >=
            let last = using_columns.last().is_some_and(|c| c == column);
            let op = if join.ref_type == JoinRefType::Asof && last {
                ComparisonOp::GreaterThanOrEqual
            } else {
                ComparisonOp::Equal
            };
            predicates.push(bind_comparison(op, left_column, right_column)?);
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
        if join.ref_type == JoinRefType::Positional {
            return Ok(LogicalOperator::new(
                LogicalOperatorKind::PositionalJoin,
                vec![left, right],
            ));
        }
        if join.join_type == JoinType::Mark {
            plan_mark_join(&predicates, &left, &right)?;
            self.bind_context.remove_context(&right_aliases);
            let table_index = self.generate_table_index();
            self.bind_context.add_binding(Binding {
                alias: "__mark_join".to_string(),
                schema: None,
                table_index,
                names: vec!["__mark_join_marker".to_string()],
                types: vec![LogicalType::BOOLEAN],
                row_id: None,
                generated: Vec::new(),
            });
            return Ok(LogicalOperator::new(
                LogicalOperatorKind::MarkJoin {
                    table_index,
                    conditions: Vec::new(),
                    predicate: conjunction_of(predicates),
                },
                vec![left, right],
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
        if join.ref_type == JoinRefType::Asof {
            return self.plan_asof_join(join.join_type, predicates, left, right);
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

    /// `Binder::BindNearestJoin`: a lateral join against the top rows of the target.
    fn bind_nearest_join(&mut self, table_ref: &TableRef, join: &JoinRef) -> Result<LogicalOperator> {
        let ranking = join
            .ranking_expression
            .as_deref()
            .ok_or_else(|| binder_error("NEAREST join without a ranking expression"))?;
        let alias = if !join.right.alias.is_empty() {
            join.right.alias.clone()
        } else if let TableRefKind::BaseTable { name, .. } = &join.right.kind {
            name.name.clone()
        } else {
            String::new()
        };
        let select = SelectNode {
            select_list: vec![Expr::star()],
            from_table: (*join.right).clone(),
            where_clause: Some(Box::new(Expr::operator(OperatorType::IsNotNull, vec![ranking.clone()]))),
            ..SelectNode::default()
        };
        let mut node: QueryNode = select.into();
        node.modifiers.push(ResultModifier::Order(vec![OrderByNode {
            order: if join.nearest_descending {
                OrderType::Descending
            } else {
                OrderType::Ascending
            },
            nulls: NullOrder::Default,
            expr: ranking.clone(),
        }]));
        node.modifiers.push(ResultModifier::Limit {
            limit: Some(Box::new(Expr::constant(Literal::integer(join.nearest_count as i64)))),
            offset: None,
            percent: false,
        });
        let mut right = TableRef::subquery(SelectStatement { node });
        right.alias = alias;
        let lateral = JoinRef {
            right: Box::new(right),
            ref_type: JoinRefType::Regular,
            condition: Some(Box::new(Expr::constant(Literal::boolean(true)))),
            ranking_expression: None,
            ..join.clone()
        };
        self.bind_join(table_ref, &lateral)
    }

    /// `Binder::CreatePlan(BoundJoinRef)` for ASOF: finds the inequality of the predicates.
    fn plan_asof_join(
        &self,
        join_type: JoinType,
        predicates: Vec<BoundExpr>,
        left: LogicalOperator,
        right: LogicalOperator,
    ) -> Result<LogicalOperator> {
        let left_tables = table_indexes(&left);
        let right_tables = table_indexes(&right);
        let mut split = Vec::new();
        for predicate in predicates {
            split_predicates(predicate, &mut split);
        }
        let reads = |expr: &BoundExpr, tables: &HashSet<usize>, other: &HashSet<usize>| {
            references(expr, tables) && !references(expr, other)
        };
        let mut inequality = None;
        let mut rest = Vec::new();
        for expr in split {
            let is_inequality = matches!(
                &expr.kind,
                BoundExprKind::Comparison {
                    op: ComparisonOp::GreaterThan
                        | ComparisonOp::GreaterThanOrEqual
                        | ComparisonOp::LessThan
                        | ComparisonOp::LessThanOrEqual,
                    left: first,
                    right: second,
                } if (reads(first, &left_tables, &right_tables) && reads(second, &right_tables, &left_tables))
                    || (reads(first, &right_tables, &left_tables) && reads(second, &left_tables, &right_tables))
            );
            if !is_inequality {
                rest.push(expr);
                continue;
            }
            if inequality.is_some() {
                return Err(binder_error("Multiple ASOF JOIN inequalities"));
            }
            inequality = Some(expr);
        }
        let Some(inequality) = inequality else {
            return Err(binder_error("Missing ASOF JOIN inequality"));
        };
        let (conditions, predicate) = extract_join_conditions(rest, &left, &right);
        if predicate.is_some() && matches!(join_type, JoinType::Right | JoinType::Outer) {
            return Err(crate::error_not_implemented(&format!(
                "Unsupported ASOF JOIN type ({}) with arbitrary predicate",
                join_type.name()
            )));
        }
        if matches!(join_type, JoinType::Semi | JoinType::Anti) {
            // the match on the right does not matter, so a regular join does it
            let (conditions, residual) = (
                conditions,
                conjunction_of([inequality].into_iter().chain(predicate).collect()),
            );
            return Ok(LogicalOperator::new(
                LogicalOperatorKind::Join {
                    join_type,
                    conditions,
                    predicate: residual,
                },
                vec![left, right],
            ));
        }
        let BoundExprKind::Comparison {
            op,
            left: first,
            right: second,
        } = inequality.kind
        else {
            unreachable!("checked above");
        };
        let (first, op, second) = if reads(&first, &left_tables, &right_tables) {
            (*first, op, *second)
        } else {
            let flipped = match op {
                ComparisonOp::GreaterThan => ComparisonOp::LessThan,
                ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
                ComparisonOp::LessThan => ComparisonOp::GreaterThan,
                _ => ComparisonOp::GreaterThanOrEqual,
            };
            (*second, flipped, *first)
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::AsOfJoin {
                join_type,
                conditions,
                left: first,
                op,
                right: second,
                predicate,
            },
            vec![left, right],
        ))
    }
}
