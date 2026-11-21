// Ported from duckdb/src/optimizer/filter_combiner.cpp @ rust-port-baseline
//
//! `FilterCombiner`: the filters on their way down, as equivalence sets of the expressions that
//! are equal to each other, each with the comparisons with constants that hold for it, which
//! prune each other (`x > 5 AND x > 3` is `x > 5`; `x = 5 AND x > 7` holds for no row). A
//! comparison of two expressions adds the bounds the constants of one tell of the other
//! (`x = 5 AND y >= x` is also `y >= 5`). What is not a comparison stays as it is.
//!
//! Not ported: the table filters the combiner makes for scans (`GenerateTableScanFilters`), which
//! `scan_filters` does here.

use super::rewriter::{can_throw, expressions_equal};
use super::statistics::{compare_values, flip};
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;
use std::collections::BTreeMap;

/// `FilterResult`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum FilterResult {
    Success,
    Unsatisfiable,
    Unsupported,
}

/// `ExpressionValueInformation`: `<expression> op constant`.
#[derive(Debug, Clone)]
struct ValueInfo {
    op: ComparisonOp,
    constant: Value,
    /// the alias of the filter it came from, which marks the filters of the join statistics
    alias: String,
}

/// `ValueComparisonResult`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pruning {
    PruneLeft,
    PruneRight,
    Nothing,
    Unsatisfiable,
}

fn is_greater(op: ComparisonOp) -> bool {
    matches!(op, ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual)
}

fn is_less(op: ComparisonOp) -> bool {
    matches!(op, ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual)
}

/// `left op right` of the constants; unknown is false.
fn holds(op: ComparisonOp, left: &Value, right: &Value) -> bool {
    compare_values(op, left, right).unwrap_or(false)
}

fn invert(result: Pruning) -> Pruning {
    match result {
        Pruning::PruneLeft => Pruning::PruneRight,
        Pruning::PruneRight => Pruning::PruneLeft,
        other => other,
    }
}

/// `CompareValueInformation`.
fn compare_info(left: &ValueInfo, right: &ValueInfo) -> Pruning {
    use ComparisonOp as Op;
    let (l, r) = (&left.constant, &right.constant);
    if left.op == Op::Equal {
        let prune_right = match right.op {
            Op::LessThan => holds(Op::LessThan, l, r),
            Op::LessThanOrEqual => holds(Op::LessThanOrEqual, l, r),
            Op::GreaterThan => holds(Op::GreaterThan, l, r),
            Op::GreaterThanOrEqual => holds(Op::GreaterThanOrEqual, l, r),
            Op::NotEqual => holds(Op::NotEqual, l, r),
            _ => holds(Op::Equal, l, r),
        };
        return if prune_right { Pruning::PruneRight } else { Pruning::Unsatisfiable };
    }
    if right.op == Op::Equal {
        return invert(compare_info(right, left));
    }
    if left.op == Op::NotEqual {
        let prune_left = match right.op {
            Op::LessThan => holds(Op::GreaterThanOrEqual, l, r),
            Op::LessThanOrEqual => holds(Op::GreaterThan, l, r),
            Op::GreaterThan => holds(Op::LessThanOrEqual, l, r),
            Op::GreaterThanOrEqual => holds(Op::LessThan, l, r),
            _ => holds(Op::Equal, l, r),
        };
        return if prune_left { Pruning::PruneLeft } else { Pruning::Nothing };
    }
    if right.op == Op::NotEqual {
        return invert(compare_info(right, left));
    }
    if is_greater(left.op) && is_greater(right.op) {
        return if holds(Op::GreaterThan, l, r) {
            Pruning::PruneRight
        } else if holds(Op::LessThan, l, r) {
            Pruning::PruneLeft
        } else if left.op == Op::GreaterThanOrEqual {
            // the same constant: > is the narrower
            Pruning::PruneLeft
        } else {
            Pruning::PruneRight
        };
    }
    if is_less(left.op) && is_less(right.op) {
        return if holds(Op::LessThan, l, r) {
            Pruning::PruneRight
        } else if holds(Op::GreaterThan, l, r) {
            Pruning::PruneLeft
        } else if left.op == Op::LessThanOrEqual {
            Pruning::PruneLeft
        } else {
            Pruning::PruneRight
        };
    }
    if is_less(left.op) {
        // left is an upper bound and right a lower one
        return if holds(Op::GreaterThan, l, r) {
            Pruning::Nothing
        } else if holds(Op::LessThan, l, r) {
            Pruning::Unsatisfiable
        } else if left.op == Op::LessThanOrEqual && right.op == Op::GreaterThanOrEqual {
            Pruning::Nothing
        } else {
            Pruning::Unsatisfiable
        };
    }
    invert(compare_info(right, left))
}

/// `FilterCombiner::AddConstantComparison`.
fn add_constant_comparison(list: &mut Vec<ValueInfo>, info: ValueInfo) -> FilterResult {
    if info.constant.is_null() {
        return FilterResult::Unsatisfiable;
    }
    let mut at = 0;
    while at < list.len() {
        match compare_info(&list[at], &info) {
            Pruning::PruneLeft => {
                list.remove(at);
                continue;
            }
            Pruning::PruneRight => return FilterResult::Success,
            Pruning::Unsatisfiable => {
                list.push(info);
                return FilterResult::Unsatisfiable;
            }
            Pruning::Nothing => {}
        }
        at += 1;
    }
    list.push(info);
    FilterResult::Success
}

fn comparison(op: ComparisonOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        LogicalType::BOOLEAN,
    )
}

#[derive(Default)]
pub(super) struct FilterCombiner {
    /// `stored_expressions`, with the equivalence set of each
    nodes: Vec<(BoundExpr, usize)>,
    /// `equivalence_map` and `constant_values`, by set
    sets: BTreeMap<usize, (Vec<usize>, Vec<ValueInfo>)>,
    next_set: usize,
    remaining: Vec<BoundExpr>,
}

impl FilterCombiner {
    /// `GetNode` and `GetEquivalenceSet`: the node of the expression, and its set.
    fn node(&mut self, expr: &BoundExpr) -> usize {
        if let Some(at) = self.nodes.iter().position(|(node, _)| expressions_equal(node, expr)) {
            return at;
        }
        let set = self.next_set;
        self.next_set += 1;
        self.nodes.push((expr.clone(), set));
        let at = self.nodes.len() - 1;
        self.sets.insert(set, (vec![at], Vec::new()));
        at
    }

    fn set_of(&self, node: usize) -> usize {
        self.nodes[node].1
    }

    /// `AddFilter(unique_ptr<Expression>)`: what the combiner does not take stays as it is.
    pub(super) fn add(&mut self, expr: BoundExpr) -> FilterResult {
        let result = self.add_filter(&expr);
        if result == FilterResult::Unsupported {
            self.remaining.push(expr);
            return FilterResult::Success;
        }
        result
    }

    /// `AddFilter(Expression &)`.
    fn add_filter(&mut self, expr: &BoundExpr) -> FilterResult {
        if expr.is_foldable() {
            let as_boolean = expr.clone().cast_to(&LogicalType::BOOLEAN, false);
            let Some(value) = try_evaluate_scalar(&as_boolean) else {
                return FilterResult::Unsupported;
            };
            return if value.is_null() || value == Value::boolean(false) {
                FilterResult::Unsatisfiable
            } else {
                FilterResult::Success
            };
        }
        match &expr.kind {
            BoundExprKind::Between { input, lower, upper } if lower.is_foldable() || upper.is_foldable() => {
                let lower_comparison = comparison(ComparisonOp::GreaterThanOrEqual, (**input).clone(), (**lower).clone())
                    .with_alias(expr.alias.clone());
                let result = self.add_bound_comparison(&lower_comparison);
                if result != FilterResult::Success {
                    return result;
                }
                let upper_comparison = comparison(ComparisonOp::LessThanOrEqual, (**input).clone(), (**upper).clone())
                    .with_alias(expr.alias.clone());
                self.add_bound_comparison(&upper_comparison)
            }
            BoundExprKind::Comparison { .. } => self.add_bound_comparison(expr),
            _ => FilterResult::Unsupported,
        }
    }

    /// `AddBoundComparisonFilter`.
    fn add_bound_comparison(&mut self, expr: &BoundExpr) -> FilterResult {
        let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
            return FilterResult::Unsupported;
        };
        let op = *op;
        if !matches!(
            op,
            ComparisonOp::Equal
                | ComparisonOp::NotEqual
                | ComparisonOp::LessThan
                | ComparisonOp::LessThanOrEqual
                | ComparisonOp::GreaterThan
                | ComparisonOp::GreaterThanOrEqual
        ) {
            return FilterResult::Unsupported;
        }
        let (left_scalar, right_scalar) = (left.is_foldable(), right.is_foldable());
        if left_scalar || right_scalar {
            let (scalar, other) = if left_scalar { (left, right) } else { (right, left) };
            let node = self.node(other);
            let set = self.set_of(node);
            let Some(constant) = try_evaluate_scalar(scalar) else {
                return FilterResult::Unsupported;
            };
            if constant.is_null() {
                return FilterResult::Unsatisfiable;
            }
            let info = ValueInfo {
                op: if left_scalar { flip(op) } else { op },
                constant,
                alias: expr.alias.clone(),
            };
            if self.nodes[node].0.return_type != *info.constant.logical_type() {
                return FilterResult::Unsupported;
            }
            let list = &mut self.sets.get_mut(&set).expect("a set of the node").1;
            let result = add_constant_comparison(list, info);
            if let Some(transitive) = self.find_transitive_filter(other) {
                match self.add_transitive_filters(&transitive, true) {
                    FilterResult::Unsupported => self.remaining.push(transitive),
                    FilterResult::Unsatisfiable => return FilterResult::Unsatisfiable,
                    FilterResult::Success => {}
                }
            }
            return result;
        }
        // a comparison of two expressions: only equality joins their sets
        if op != ComparisonOp::Equal {
            return FilterResult::Unsupported;
        }
        let (left_node, right_node) = (self.node(left), self.node(right));
        if expressions_equal(&self.nodes[left_node].0, &self.nodes[right_node].0) {
            return FilterResult::Unsupported;
        }
        let (left_set, right_set) = (self.set_of(left_node), self.set_of(right_node));
        if left_set == right_set {
            return FilterResult::Success;
        }
        let (right_members, right_constants) = self.sets.remove(&right_set).expect("a set");
        for &member in &right_members {
            self.nodes[member].1 = left_set;
        }
        let left_entry = self.sets.get_mut(&left_set).expect("a set");
        left_entry.0.extend(right_members);
        for constant in right_constants {
            if add_constant_comparison(&mut left_entry.1, constant) == FilterResult::Unsatisfiable {
                return FilterResult::Unsatisfiable;
            }
        }
        FilterResult::Success
    }

    /// `AddTransitiveFilters`: the bounds that `comparison` and the constants of its right side
    /// tell of its left side.
    fn add_transitive_filters(&mut self, expr: &BoundExpr, is_root: bool) -> FilterResult {
        let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
            return FilterResult::Unsupported;
        };
        let op = *op;
        if !is_greater(op) && !is_less(op) {
            return FilterResult::Unsupported;
        }
        // bounds must not copy what is volatile or can fail
        if expr.is_volatile() || can_throw(expr) {
            return FilterResult::Unsupported;
        }
        let (left_node, right_node) = (self.node(left), self.node(right));
        if expressions_equal(&self.nodes[left_node].0, &self.nodes[right_node].0) {
            return FilterResult::Unsupported;
        }
        let (left_set, right_set) = (self.set_of(left_node), self.set_of(right_node));
        if left_set == right_set {
            // j > i with j = i holds for no row
            if matches!(op, ComparisonOp::GreaterThan | ComparisonOp::LessThan) {
                return FilterResult::Unsatisfiable;
            }
            return FilterResult::Success;
        }
        let right_constants = self.sets[&right_set].1.clone();
        let (mut successful, mut inserted) = (false, false);
        for right_constant in right_constants {
            let info_op = if right_constant.op == ComparisonOp::Equal {
                // j >= i AND i = 10: j >= 10, and j >= i goes
                op
            } else if (op == ComparisonOp::GreaterThanOrEqual && is_greater(right_constant.op))
                || (op == ComparisonOp::LessThanOrEqual && is_less(right_constant.op))
            {
                // j >= i AND i > 10: j > 10, and j >= i stays
                if !inserted {
                    self.remaining.push(expr.clone());
                    inserted = true;
                }
                right_constant.op
            } else if (op == ComparisonOp::GreaterThan && is_greater(right_constant.op))
                || (op == ComparisonOp::LessThan && is_less(right_constant.op))
            {
                // j > i AND i >= 10: j > 10, and j > i stays
                if !inserted {
                    self.remaining.push(expr.clone());
                    inserted = true;
                }
                op
            } else {
                continue;
            };
            let info = ValueInfo {
                op: info_op,
                constant: right_constant.constant,
                alias: String::new(),
            };
            let left_list = &mut self.sets.get_mut(&left_set).expect("a set").1;
            if add_constant_comparison(left_list, info) == FilterResult::Unsatisfiable {
                return FilterResult::Unsatisfiable;
            }
            successful = true;
        }
        if !successful {
            return FilterResult::Unsupported;
        }
        if is_root && let Some(transitive) = self.find_transitive_filter(left) {
            match self.add_transitive_filters(&transitive, false) {
                FilterResult::Unsupported => self.remaining.push(transitive),
                FilterResult::Unsatisfiable => return FilterResult::Unsatisfiable,
                FilterResult::Success => {}
            }
        }
        FilterResult::Success
    }

    /// `FindTransitiveFilter`: a comparison among the remaining filters whose right side is the
    /// column, taken out of them.
    fn find_transitive_filter(&mut self, expr: &BoundExpr) -> Option<BoundExpr> {
        if !matches!(expr.kind, BoundExprKind::ColumnRef { .. }) {
            return None;
        }
        let at = self.remaining.iter().position(|filter| {
            matches!(&filter.kind, BoundExprKind::Comparison { op, right, .. }
                if *op != ComparisonOp::NotEqual && expressions_equal(right, expr))
        })?;
        Some(self.remaining.remove(at))
    }

    /// `GenerateFilters`: the remaining filters, then each set's equalities and bounds.
    pub(super) fn generate(self) -> Vec<BoundExpr> {
        let bound = |info: &ValueInfo, entry: &BoundExpr| {
            comparison(info.op, entry.clone(), BoundExpr::constant(info.constant.clone())).with_alias(info.alias.clone())
        };
        let mut result = self.remaining;
        for (members, constants) in self.sets.values() {
            for (i, &member) in members.iter().enumerate() {
                let entry = &self.nodes[member].0;
                for &other in &members[i + 1..] {
                    result.push(comparison(ComparisonOp::Equal, entry.clone(), self.nodes[other].0.clone()));
                }
                let (mut lower, mut upper) = (None, None);
                for info in constants {
                    if is_greater(info.op) {
                        lower = Some(info);
                    } else if is_less(info.op) {
                        upper = Some(info);
                    } else {
                        result.push(bound(info, entry));
                    }
                }
                match (lower, upper) {
                    (Some(lower), Some(upper)) => {
                        let inclusive = lower.op == ComparisonOp::GreaterThanOrEqual
                            && upper.op == ComparisonOp::LessThanOrEqual;
                        let alias = if lower.alias == upper.alias { lower.alias.clone() } else { String::new() };
                        if inclusive && lower.constant == upper.constant {
                            result.push(
                                comparison(
                                    ComparisonOp::Equal,
                                    entry.clone(),
                                    BoundExpr::constant(lower.constant.clone()),
                                )
                                .with_alias(alias),
                            );
                        } else if inclusive {
                            result.push(
                                BoundExpr::new(
                                    BoundExprKind::Between {
                                        input: Box::new(entry.clone()),
                                        lower: Box::new(BoundExpr::constant(lower.constant.clone())),
                                        upper: Box::new(BoundExpr::constant(upper.constant.clone())),
                                    },
                                    LogicalType::BOOLEAN,
                                )
                                .with_alias(alias),
                            );
                        } else {
                            // a BETWEEN with an exclusive bound is its two comparisons here
                            result.push(bound(lower, entry));
                            result.push(bound(upper, entry));
                        }
                    }
                    (Some(info), None) | (None, Some(info)) => result.push(bound(info, entry)),
                    (None, None) => {}
                }
            }
        }
        result
    }
}
