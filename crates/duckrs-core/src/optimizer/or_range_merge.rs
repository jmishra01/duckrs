// Ported from duckdb/src/optimizer/rule/or_range_merge.cpp @ rust-port-baseline
//
//! `OrRangeMergeRule`: ranges with constant bounds on the same expression that an OR joins merge
//! into one range where they overlap, touch, or are adjacent integers:
//! `x BETWEEN 1 AND 20 OR x BETWEEN 21 AND 40` is `x >= 1 AND x <= 40`.

use crate::optimizer::statistics::compare_values;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::PhysicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;

/// One side of a range; a side that is not present leaves the range open on that side.
#[derive(Clone, Default)]
struct RangeBound {
    value: Option<Value>,
    inclusive: bool,
}

/// A child of the OR that restricts one expression to a range with constant bounds.
struct RangePredicate {
    child: usize,
    input: BoundExpr,
    lower: RangeBound,
    upper: RangeBound,
    grouped: bool,
}

fn less(a: &Value, b: &Value) -> bool {
    compare_values(ComparisonOp::LessThan, a, b) == Some(true)
}

fn equal(a: &Value, b: &Value) -> bool {
    compare_values(ComparisonOp::Equal, a, b) == Some(true)
}

/// `SupportsRangeMerge`: types whose constants compare the same way as the comparison operators do.
fn supports_range_merge(expr: &BoundExpr) -> bool {
    matches!(
        expr.return_type.physical_type(),
        PhysicalType::Int8
            | PhysicalType::Int16
            | PhysicalType::Int32
            | PhysicalType::Int64
            | PhysicalType::Int128
            | PhysicalType::UInt8
            | PhysicalType::UInt16
            | PhysicalType::UInt32
            | PhysicalType::UInt64
            | PhysicalType::UInt128
            | PhysicalType::Float
            | PhysicalType::Double
            | PhysicalType::Varchar
            | PhysicalType::Interval
    )
}

/// `IsMergeableInput`: equal inputs compute the same value. One that holds a constant may only look
/// the same (`v + INTERVAL '1 month'` and `v + INTERVAL '30 days'`).
fn is_mergeable_input(input: &BoundExpr) -> bool {
    let mut constant = false;
    input
        .clone()
        .walk_mut(&mut |e| constant |= matches!(e.kind, BoundExprKind::Constant(_)));
    !input.is_volatile() && !constant
}

/// `TryGetBound`: a constant that is not NULL, of the type of the input.
fn bound(expr: &BoundExpr, input: &BoundExpr, inclusive: bool) -> Option<RangeBound> {
    let BoundExprKind::Constant(value) = &expr.kind else {
        return None;
    };
    (!value.is_null() && value.logical_type() == &input.return_type).then(|| RangeBound {
        value: Some(value.clone()),
        inclusive,
    })
}

/// `TryGetComparisonRange`: `x > a`, `x >= a`, `x < b`, `x <= b`, the constant on either side.
fn comparison_range(expr: &BoundExpr, lower: &mut RangeBound, upper: &mut RangeBound) -> Option<BoundExpr> {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return None;
    };
    if !matches!(
        op,
        ComparisonOp::LessThan
            | ComparisonOp::LessThanOrEqual
            | ComparisonOp::GreaterThan
            | ComparisonOp::GreaterThanOrEqual
    ) {
        return None;
    }
    let (input, constant, op) = if matches!(right.kind, BoundExprKind::Constant(_)) {
        (left, right, *op)
    } else if matches!(left.kind, BoundExprKind::Constant(_)) {
        // `a > x` is `x < a`
        let flipped = match op {
            ComparisonOp::LessThan => ComparisonOp::GreaterThan,
            ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
            ComparisonOp::GreaterThan => ComparisonOp::LessThan,
            _ => ComparisonOp::LessThanOrEqual,
        };
        (right, left, flipped)
    } else {
        return None;
    };
    let inclusive = matches!(op, ComparisonOp::LessThanOrEqual | ComparisonOp::GreaterThanOrEqual);
    let is_upper = matches!(op, ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual);
    let found = bound(constant, input, inclusive)?;
    if is_upper {
        *upper = found;
    } else {
        *lower = found;
    }
    Some((**input).clone())
}

/// `TryGetRange`: a BETWEEN, a single comparison, or an AND of a lower and an upper comparison on
/// the same expression (how the binder plans BETWEEN of a column).
fn range(expr: &BoundExpr) -> Option<(BoundExpr, RangeBound, RangeBound)> {
    let (mut lower, mut upper) = (RangeBound::default(), RangeBound::default());
    match &expr.kind {
        BoundExprKind::Between {
            input,
            lower: low,
            upper: high,
        } => {
            let lower = bound(low, input, true)?;
            let upper = bound(high, input, true)?;
            Some(((**input).clone(), lower, upper))
        }
        BoundExprKind::Conjunction { and: true, children } => {
            let [first, second] = children.as_slice() else {
                return None;
            };
            let input = comparison_range(first, &mut lower, &mut upper)?;
            let second_input = comparison_range(second, &mut lower, &mut upper)?;
            (lower.value.is_some()
                && upper.value.is_some()
                && super::rewriter::expressions_equal(&input, &second_input)
                && is_mergeable_input(&input))
            .then_some((input, lower, upper))
        }
        _ => {
            let input = comparison_range(expr, &mut lower, &mut upper)?;
            Some((input, lower, upper))
        }
    }
}

/// `IsNonEmptyRange`.
fn is_non_empty(lower: &RangeBound, upper: &RangeBound) -> bool {
    let (Some(low), Some(high)) = (&lower.value, &upper.value) else {
        return true;
    };
    less(low, high) || (equal(low, high) && lower.inclusive && upper.inclusive)
}

/// `LowerBoundLessThan`: open first, then by value, inclusive before exclusive on a tie.
fn lower_bound_order(a: &RangeBound, b: &RangeBound) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (&a.value, &b.value) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) if equal(x, y) => match (a.inclusive, b.inclusive) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
        (Some(x), Some(y)) if less(x, y) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

/// `IsAdjacentInteger`: `lower` is the integer that follows `upper`.
fn is_adjacent_integer(upper: &Value, lower: &Value) -> bool {
    if !upper.logical_type().is_integral() {
        return false;
    }
    match (upper.as_i128(), lower.as_i128()) {
        (Some(upper), Some(lower)) => lower - 1 == upper,
        _ => false,
    }
}

/// `Connects`: a range that starts at `lower` continues the one that ends at `upper`.
fn connects(upper: &RangeBound, lower: &RangeBound) -> bool {
    let (Some(high), Some(low)) = (&upper.value, &lower.value) else {
        return true;
    };
    if less(low, high) {
        return true;
    }
    if equal(low, high) {
        // [1, 20] and (20, 40] both cover 20 together, [1, 20) and (20, 40] leave it out
        return upper.inclusive || lower.inclusive;
    }
    upper.inclusive && lower.inclusive && is_adjacent_integer(high, low)
}

/// `ExtendUpperBound`.
fn extend_upper(upper: &mut RangeBound, other: &RangeBound) {
    let Some(current) = &upper.value else {
        return;
    };
    match &other.value {
        None => *upper = other.clone(),
        Some(value) if less(current, value) => *upper = other.clone(),
        Some(value) if equal(current, value) => upper.inclusive |= other.inclusive,
        _ => {}
    }
}

fn comparison(op: ComparisonOp, input: &BoundExpr, bound: &RangeBound) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(input.clone()),
            right: Box::new(BoundExpr::constant(bound.value.clone().expect("a bound"))),
        },
        duckrs_common::types::LogicalType::BOOLEAN,
    )
}

/// `MakeRange`: the comparisons of a column, otherwise a BETWEEN when both bounds are there.
fn make_range(input: &BoundExpr, lower: &RangeBound, upper: &RangeBound) -> BoundExpr {
    let lower_op = if lower.inclusive {
        ComparisonOp::GreaterThanOrEqual
    } else {
        ComparisonOp::GreaterThan
    };
    let upper_op = if upper.inclusive {
        ComparisonOp::LessThanOrEqual
    } else {
        ComparisonOp::LessThan
    };
    match (&lower.value, &upper.value) {
        (None, _) => comparison(upper_op, input, upper),
        (_, None) => comparison(lower_op, input, lower),
        (Some(low), Some(high)) => {
            if matches!(input.kind, BoundExprKind::ColumnRef { .. }) || !(lower.inclusive && upper.inclusive) {
                BoundExpr::new(
                    BoundExprKind::Conjunction {
                        and: true,
                        children: vec![comparison(lower_op, input, lower), comparison(upper_op, input, upper)],
                    },
                    duckrs_common::types::LogicalType::BOOLEAN,
                )
            } else {
                BoundExpr::new(
                    BoundExprKind::Between {
                        input: Box::new(input.clone()),
                        lower: Box::new(BoundExpr::constant(low.clone())),
                        upper: Box::new(BoundExpr::constant(high.clone())),
                    },
                    duckrs_common::types::LogicalType::BOOLEAN,
                )
            }
        }
    }
}

/// `OrRangeMergeRule::Apply`.
pub fn or_range_merge(expr: &mut BoundExpr) -> bool {
    let BoundExprKind::Conjunction { and: false, children } = &mut expr.kind else {
        return false;
    };
    let mut ranges: Vec<RangePredicate> = Vec::new();
    for (child, expr) in children.iter().enumerate() {
        let Some((input, lower, upper)) = range(expr) else {
            continue;
        };
        if !supports_range_merge(&input) || !is_mergeable_input(&input) || !is_non_empty(&lower, &upper) {
            continue;
        }
        ranges.push(RangePredicate {
            child,
            input,
            lower,
            upper,
            grouped: false,
        });
    }
    if ranges.len() < 2 {
        return false;
    }
    let mut replacements: Vec<Option<BoundExpr>> = vec![None; children.len()];
    let mut dropped = vec![false; children.len()];
    let mut merged_any = false;
    for i in 0..ranges.len() {
        if ranges[i].grouped {
            continue;
        }
        let mut group = Vec::new();
        for j in i..ranges.len() {
            if !ranges[j].grouped && super::rewriter::expressions_equal(&ranges[j].input, &ranges[i].input) {
                ranges[j].grouped = true;
                group.push(j);
            }
        }
        if group.len() < 2 {
            continue;
        }
        // a stable sort, as `std::sort` leaves ties of the lower bound in an order of its own
        group.sort_by(|&a, &b| lower_bound_order(&ranges[a].lower, &ranges[b].lower));
        let input = ranges[group[0]].input.clone();
        let mut members: Vec<usize> = Vec::new();
        let (mut lower, mut upper) = (RangeBound::default(), RangeBound::default());
        let mut flush = |members: &Vec<usize>, lower: &RangeBound, upper: &RangeBound| {
            if members.len() < 2 {
                return;
            }
            merged_any = true;
            let first = *members.iter().min().expect("members");
            replacements[first] = Some(make_range(&input, lower, upper));
            for &member in members {
                dropped[member] = member != first;
            }
        };
        for &at in &group {
            let range = &ranges[at];
            // a range that is open on both sides is not a range anymore: start over instead
            let joins = !members.is_empty()
                && connects(&upper, &range.lower)
                && (lower.value.is_some() || range.upper.value.is_some());
            if joins {
                extend_upper(&mut upper, &range.upper);
            } else {
                flush(&members, &lower, &upper);
                members.clear();
                lower = range.lower.clone();
                upper = range.upper.clone();
            }
            members.push(range.child);
        }
        flush(&members, &lower, &upper);
    }
    if !merged_any {
        return false;
    }
    let mut kept = Vec::new();
    for (at, child) in std::mem::take(children).into_iter().enumerate() {
        if dropped[at] {
            continue;
        }
        kept.push(replacements[at].take().unwrap_or(child));
    }
    if kept.len() == 1 {
        *expr = kept.pop().expect("one");
    } else {
        *children = kept;
    }
    true
}
