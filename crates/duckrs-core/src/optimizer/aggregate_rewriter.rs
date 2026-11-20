// Ported from duckdb/src/optimizer/aggregate_function_rewriter.cpp and
// duckdb/src/planner/expression_nullability.cpp @ rust-port-baseline
//
//! `AggregateFunctionRewriter`: aggregates that are cheaper as others, with a projection above the
//! aggregate that computes what the original gave:
//! - `avg(x)` as `sum(x) / count(x)`, which an ungrouped aggregate always takes, and a grouped one
//!   when it can share the sum or the count with another aggregate;
//! - `sum(x + C)` of integers, ungrouped, as `sum(x) + C * count(x)`;
//! - `list(x ORDER BY x)` as `list_sort(list(x), ...)`.
//!
//! Not ported: the direct rewrites of the aggregate functions themselves (`TryDirectAggregateRewrite`).

use super::common_subplan::{max_table_index, visit_bindings};
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::function::BuiltinFunctions;
use crate::function::binder::{bind_aggregate_function, bind_scalar_function};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::sort_key::{NullOrder, OrderType};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::{ComparisonOp, JoinType};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq)]
enum Rule {
    Avg,
    Sum,
    List,
}

/// What a rewritten aggregate needs to give its original result back.
struct RewriteInfo {
    count_index: usize,
    additional: Vec<BoundExpr>,
}

/// Runs the pass over a plan; `common_aggregate` and `statistics` say whether those passes run.
pub fn rewrite(mut plan: LogicalOperator, common_aggregate: bool, statistics: bool) -> LogicalOperator {
    let mut next = max_table_index(&plan) + 1;
    for rule in [Rule::Avg, Rule::Sum, Rule::List] {
        let root = plan.clone();
        let mut rewriter = Rewriter {
            rule,
            next: &mut next,
            root: &root,
            common_aggregate,
            statistics,
        };
        rewriter.optimize(&mut plan);
    }
    plan
}

struct Rewriter<'a> {
    rule: Rule,
    next: &'a mut usize,
    root: &'a LogicalOperator,
    common_aggregate: bool,
    statistics: bool,
}

impl Rewriter<'_> {
    /// Rewrites the aggregates at and below `op`, and gives the columns they moved to.
    fn optimize(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let mut map = HashMap::new();
        for child in &mut op.children {
            map.extend(self.optimize(child));
        }
        if !map.is_empty() {
            visit_bindings(&mut op.kind, &mut |binding, depth| {
                if depth == 0
                    && let Some(new) = map.get(binding)
                {
                    *binding = *new;
                }
            });
        }
        // the columns of these are their own
        if matches!(
            op.kind,
            K::Union { .. } | K::ExceptIntersect { .. } | K::MaterializedCte { .. } | K::Projection { .. }
        ) {
            map.clear();
        }
        if matches!(op.kind, K::Aggregate { .. }) {
            map.extend(self.rewrite_aggregates(op));
        }
        map
    }

    fn should_skip(&self, op: &LogicalOperator) -> bool {
        let K::Aggregate {
            groups,
            grouping_functions,
            ..
        } = &op.kind
        else {
            return true;
        };
        match self.rule {
            Rule::Avg => !grouping_functions.is_empty(),
            Rule::Sum => !groups.is_empty(),
            Rule::List => false,
        }
    }

    /// The rule's matcher: whether it applies to the aggregate.
    fn matches(&self, aggregate: &BoundAggregate) -> bool {
        let plain = aggregate.filter.is_none()
            && aggregate.order_by.is_empty()
            && !aggregate.distinct
            && aggregate.function.export.is_none();
        match self.rule {
            Rule::Avg => {
                plain
                    && aggregate.function.name == "avg"
                    && aggregate.return_type.is_numeric()
                    && matches!(aggregate.children.as_slice(), [child] if child.return_type.is_numeric() && !child.is_volatile())
            }
            Rule::Sum => plain && aggregate.function.name == "sum" && sum_addition(aggregate).is_some(),
            // `OrderedAggregateMatcher`: list(x ORDER BY x)
            Rule::List => {
                aggregate.function.name == "list"
                    && aggregate.function.export.is_none()
                    && matches!(aggregate.children.as_slice(), [child] if !child.is_volatile())
                    && aggregate.order_by.len() == 1
                    && equals(&aggregate.order_by[0].0, &aggregate.children[0])
            }
        }
    }

    /// `AvgRewriteRule::ShouldRewrite`.
    fn should_rewrite(&self, op: &LogicalOperator, at: usize) -> bool {
        if self.rule != Rule::Avg {
            return true;
        }
        let K::Aggregate { groups, aggregates, .. } = &op.kind else {
            return false;
        };
        if groups.is_empty() {
            return true;
        }
        if !self.common_aggregate {
            return false;
        }
        let expr = &aggregates[at];
        let child = &expr.children[0];
        let (Some(sum), Some(count)) = (bind_aggregate("sum", child.clone()), bind_aggregate("count", child.clone()))
        else {
            return false;
        };
        let is_not_null = |input: &BoundExpr| match &input.kind {
            BoundExprKind::Constant(value) => !value.is_null(),
            _ => is_not_null(&op.children[0], input, self.root),
        };
        let can_share_count = self.statistics && is_not_null(child);
        for (index, other) in aggregates.iter().enumerate() {
            // identical averages share their state already
            if index == at || aggregate_equals(expr, other) {
                continue;
            }
            if aggregate_equals(&sum, other) || aggregate_equals(&count, other) {
                return true;
            }
            if !can_share_count || other.children.iter().any(BoundExpr::is_volatile) {
                continue;
            }
            let plain = other.filter.is_none() && other.order_by.is_empty() && !other.distinct;
            let counts = plain && matches!(other.function.name.as_str(), "count" | "count_star");
            let averages = plain && other.function.name == "avg";
            if !counts && !averages {
                continue;
            }
            if other.children.is_empty() || (other.children.len() == 1 && is_not_null(&other.children[0])) {
                return true;
            }
        }
        false
    }

    /// `Rewrite`: changes the aggregate, and gives what to count, if anything.
    fn rewrite(&self, aggregate: &mut BoundAggregate, additional: &mut Vec<BoundExpr>) -> Option<Option<BoundExpr>> {
        match self.rule {
            Rule::Avg => {
                let child = aggregate.children[0].clone();
                *aggregate = bind_aggregate("sum", child.clone())?;
                Some(Some(child))
            }
            Rule::Sum => {
                let (constant, main) = sum_addition(aggregate)?;
                let sum = bind_aggregate("sum", main.clone())?;
                if sum.return_type != aggregate.return_type {
                    return None;
                }
                *aggregate = sum;
                additional.push(constant);
                Some(Some(main))
            }
            Rule::List => {
                let (_, modifiers) = aggregate.order_by[0].clone();
                let sense = match modifiers.order {
                    OrderType::Ascending => "ASCENDING",
                    OrderType::Descending => "DESCENDING",
                };
                let nulls = match modifiers.nulls {
                    NullOrder::NullsFirst => "NULLS_FIRST",
                    NullOrder::NullsLast => "NULLS_LAST",
                };
                additional.push(BoundExpr::constant(Value::varchar(sense)));
                additional.push(BoundExpr::constant(Value::varchar(nulls)));
                aggregate.order_by.clear();
                Some(None)
            }
        }
    }

    /// `CreateProjectionExpression`: the original result from the new aggregate and its count.
    fn projection_expression(
        &self,
        aggregate_type: &LogicalType,
        aggregate_ref: BoundExpr,
        mut additional: Vec<BoundExpr>,
    ) -> Option<BoundExpr> {
        match self.rule {
            Rule::Avg => {
                let count = additional.pop()?;
                let _ = aggregate_type;
                bind_scalar("/", vec![aggregate_ref, count])
            }
            Rule::Sum => {
                let count = additional.pop()?;
                let constant = additional.pop()?;
                let multiply = bind_scalar(
                    "*",
                    vec![count.cast_to(aggregate_type, false), constant.cast_to(aggregate_type, false)],
                )?;
                bind_scalar("+", vec![aggregate_ref, multiply])
            }
            Rule::List => {
                let nulls = additional.remove(1);
                let sense = additional.remove(0);
                bind_scalar("list_sort", vec![aggregate_ref, sense, nulls])
            }
        }
    }

    /// `RewriteAggregates`: the rewritten aggregates and the projection above them.
    fn rewrite_aggregates(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let mut map = HashMap::new();
        if self.should_skip(op) {
            return map;
        }
        let K::Aggregate { aggregates, .. } = &op.kind else {
            return map;
        };
        let aggregate_count = aggregates.len();
        // the types the operators above see
        let original_types: Vec<LogicalType> = aggregates.iter().map(|a| a.return_type.clone()).collect();
        let candidates: Vec<usize> = (0..aggregate_count)
            .filter(|&i| self.matches(&aggregates[i]) && self.should_rewrite(op, i))
            .collect();
        let mut rewrites: HashMap<usize, RewriteInfo> = HashMap::new();
        let K::Aggregate { aggregates, .. } = &mut op.kind else {
            unreachable!()
        };
        for i in candidates {
            let mut rewritten = aggregates[i].clone();
            let mut additional = Vec::new();
            let Some(count_argument) = self.rewrite(&mut rewritten, &mut additional) else {
                continue;
            };
            // `count(x)`, or `count_star()` without an argument
            let count = match count_argument {
                Some(argument) => bind_aggregate("count", argument),
                None => BuiltinFunctions::get()
                    .aggregate("count_star")
                    .and_then(|set| bind_aggregate_function(set, Vec::new(), None, false).ok()),
            };
            let Some(count) = count else {
                continue;
            };
            aggregates[i] = rewritten;
            rewrites.insert(
                i,
                RewriteInfo {
                    count_index: aggregates.len(),
                    additional,
                },
            );
            aggregates.push(count);
        }
        if rewrites.is_empty() {
            return map;
        }
        let K::Aggregate {
            group_index,
            aggregate_index,
            groupings_index,
            groups,
            aggregates,
            grouping_functions,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        let projection_index = *self.next;
        *self.next += 1;
        let mut expressions = Vec::new();
        for (at, group) in groups.iter().enumerate() {
            let binding = ColumnBinding::new(*group_index, at);
            map.insert(binding, ColumnBinding::new(projection_index, at));
            expressions.push(BoundExpr::column_ref(binding, group.return_type.clone(), ""));
        }
        for i in 0..aggregate_count {
            let binding = ColumnBinding::new(*aggregate_index, i);
            map.insert(binding, ColumnBinding::new(projection_index, groups.len() + i));
            let aggregate_ref = BoundExpr::column_ref(binding, aggregates[i].return_type.clone(), "");
            let Some(info) = rewrites.remove(&i) else {
                expressions.push(aggregate_ref);
                continue;
            };
            let count_binding = ColumnBinding::new(*aggregate_index, info.count_index);
            let count_ref = BoundExpr::column_ref(count_binding, aggregates[info.count_index].return_type.clone(), "");
            let mut additional = info.additional;
            additional.push(count_ref);
            let original_type = &original_types[i];
            let expr = self
                .projection_expression(original_type, aggregate_ref, additional)
                .expect("the functions of the rewrite bind");
            expressions.push(if expr.return_type == *original_type {
                expr
            } else {
                expr.cast_to(original_type, false)
            });
        }
        for at in 0..grouping_functions.len() {
            let binding = ColumnBinding::new(*groupings_index, at);
            map.insert(
                binding,
                ColumnBinding::new(projection_index, groups.len() + aggregate_count + at),
            );
            expressions.push(BoundExpr::column_ref(binding, LogicalType::BIGINT, ""));
        }
        let aggregate = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *op = LogicalOperator::new(
            K::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![aggregate],
        );
        map
    }
}

/// `SUM(x + C)` of small integers: the constant and `x`.
fn sum_addition(aggregate: &BoundAggregate) -> Option<(BoundExpr, BoundExpr)> {
    let [child] = aggregate.children.as_slice() else {
        return None;
    };
    let BoundExprKind::Function { function, children, .. } = &child.kind else {
        return None;
    };
    if function.name != "+" || children.len() != 2 || !child.return_type.is_integral() {
        return None;
    }
    let small = |ty: &LogicalType| {
        matches!(
            ty.id(),
            LogicalTypeId::TinyInt
                | LogicalTypeId::SmallInt
                | LogicalTypeId::Integer
                | LogicalTypeId::BigInt
                | LogicalTypeId::UTinyInt
                | LogicalTypeId::USmallInt
                | LogicalTypeId::UInteger
                | LogicalTypeId::UBigInt
        )
    };
    let constant_at = children.iter().position(|c| matches!(c.kind, BoundExprKind::Constant(_)))?;
    let (constant, main) = (&children[constant_at], &children[1 - constant_at]);
    (small(&constant.return_type) && small(&main.return_type) && !main.is_volatile())
        .then(|| (constant.clone(), main.clone()))
}

fn bind_aggregate(name: &str, child: BoundExpr) -> Option<BoundAggregate> {
    let set = BuiltinFunctions::get().aggregate(name)?;
    bind_aggregate_function(set, vec![child], None, false).ok()
}

fn bind_scalar(name: &str, children: Vec<BoundExpr>) -> Option<BoundExpr> {
    if crate::function::generic::is_generic_function(name) {
        return crate::function::generic::bind_generic_function(name, children).ok();
    }
    let set = BuiltinFunctions::get().scalar(name)?;
    let operator = matches!(name, "+" | "-" | "*" | "/");
    bind_scalar_function(set, children, operator, &try_evaluate_scalar).ok()
}

fn equals(a: &BoundExpr, b: &BoundExpr) -> bool {
    let unnamed = |expression: &BoundExpr| {
        let mut expression = expression.clone();
        expression.walk_mut(&mut |e| e.alias.clear());
        expression.to_string()
    };
    a.return_type == b.return_type && unnamed(a) == unnamed(b)
}

pub(super) fn aggregate_equals(a: &BoundAggregate, b: &BoundAggregate) -> bool {
    a.function.name == b.function.name
        && a.return_type == b.return_type
        && a.distinct == b.distinct
        && a.children.len() == b.children.len()
        && a.children.iter().zip(&b.children).all(|(x, y)| equals(x, y))
        && a.filter.is_none() == b.filter.is_none()
        && a.filter.iter().zip(&b.filter).all(|(x, y)| equals(x, y))
        && a.order_by.len() == b.order_by.len()
        && a.order_by.iter().zip(&b.order_by).all(|((x, m), (y, n))| m == n && equals(x, y))
}

/// `FilterRejectsNull`: whether rows `filter` keeps have `expr` not NULL.
fn filter_rejects_null(filter: &BoundExpr, expr: &BoundExpr) -> bool {
    match &filter.kind {
        BoundExprKind::Conjunction { and: true, children } => children.iter().any(|c| filter_rejects_null(c, expr)),
        BoundExprKind::Conjunction { and: false, children } => {
            !children.is_empty() && children.iter().all(|c| filter_rejects_null(c, expr))
        }
        BoundExprKind::Operator {
            op: BoundOperator::IsNotNull,
            children,
        } => children.first().is_some_and(|c| equals(c, expr)),
        BoundExprKind::Comparison { op, left, right } => {
            !matches!(op, ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom)
                && (equals(left, expr) || equals(right, expr))
        }
        _ => false,
    }
}

fn column_binding(expr: &BoundExpr) -> Option<ColumnBinding> {
    match &expr.kind {
        BoundExprKind::ColumnRef { binding, depth: 0 } => Some(*binding),
        _ => None,
    }
}

/// `NotNullExpressionAnalyzer::IsNotNull`: whether `expr` over the rows of `op` is never NULL.
pub(super) fn is_not_null(op: &LogicalOperator, expr: &BoundExpr, root: &LogicalOperator) -> bool {
    match &op.kind {
        K::Projection {
            table_index,
            expressions,
        } => {
            let Some(binding) = column_binding(expr) else {
                return false;
            };
            binding.table_index == *table_index
                && expressions
                    .get(binding.column_index)
                    .is_some_and(|source| op.children.len() == 1 && is_not_null(&op.children[0], source, root))
        }
        K::Filter { expressions } => {
            expressions.iter().any(|filter| filter_rejects_null(filter, expr))
                || (op.children.len() == 1 && is_not_null(&op.children[0], expr, root))
        }
        K::Join { join_type, .. } => {
            let Some(binding) = column_binding(expr) else {
                return false;
            };
            if !op.column_bindings().contains(&binding) {
                return false;
            }
            let sides: Vec<usize> = (0..op.children.len())
                .filter(|&i| op.children[i].column_bindings().contains(&binding))
                .collect();
            let [side] = sides.as_slice() else {
                return false;
            };
            let preserves = match join_type {
                JoinType::Inner => *side < 2,
                JoinType::Left | JoinType::Semi | JoinType::Anti | JoinType::Mark | JoinType::Single => *side == 0,
                JoinType::Right | JoinType::RightSemi | JoinType::RightAnti => *side == 1,
                JoinType::Outer => false,
            };
            preserves && is_not_null(&op.children[*side], expr, root)
        }
        K::Get {
            table,
            table_index,
            column_ids,
            ..
        } => {
            let Some(binding) = column_binding(expr) else {
                return false;
            };
            if binding.table_index != *table_index {
                return false;
            }
            let Some(&column) = column_ids.get(binding.column_index) else {
                return false;
            };
            if column == COLUMN_IDENTIFIER_ROW_ID {
                return false;
            }
            // unknown statistics are no proof
            super::statistics::table_statistics(table)
                .and_then(|all| all.get(column).cloned())
                .is_some_and(|stats| stats.can_be_valid && !stats.can_be_null)
        }
        K::CteRef {
            table_index,
            cte_index,
            ..
        } => {
            let Some(binding) = column_binding(expr) else {
                return false;
            };
            if binding.table_index != *table_index {
                return false;
            }
            let Some(definition) = find_cte(root, *cte_index) else {
                return false;
            };
            let Some(source) = definition.column_bindings().get(binding.column_index).copied() else {
                return false;
            };
            is_not_null(definition, &BoundExpr::column_ref(source, expr.return_type.clone(), ""), root)
        }
        _ => false,
    }
}

fn find_cte(op: &LogicalOperator, cte: usize) -> Option<&LogicalOperator> {
    if let K::MaterializedCte { table_index, .. } = op.kind
        && table_index == cte
    {
        return op.children.first();
    }
    op.children.iter().find_map(|child| find_cte(child, cte))
}
