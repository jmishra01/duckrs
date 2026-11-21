// Ported from duckdb/src/optimizer/rule/monotone_preimage.cpp @ rust-port-baseline
//
//! `MonotonePreimageRule`: `f(col) OP c` for a monotone unary `f` is a range on `col`, found by
//! bisection over the column's domain: `year(dt) = 2020` is `dt >= '2020-01-01' AND dt <= '2020-12-31'`.

use crate::execution::expression_executor::try_evaluate_scalar;
use crate::optimizer::statistics::compare_values;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::Date;
use duckrs_common::types::Timestamp;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_parser::ast::ComparisonOp;

/// `GetFiniteDomain`: the integers of a column type, between the infinities of one that has them.
fn finite_domain(ty: &LogicalType) -> Option<(i64, i64, bool)> {
    Some(match ty.id() {
        LogicalTypeId::TinyInt => (i64::from(i8::MIN), i64::from(i8::MAX), false),
        LogicalTypeId::SmallInt => (i64::from(i16::MIN), i64::from(i16::MAX), false),
        LogicalTypeId::Integer => (i64::from(i32::MIN), i64::from(i32::MAX), false),
        LogicalTypeId::Date => (i64::from(Date::NINFINITY.0) + 1, i64::from(Date::INFINITY.0) - 1, true),
        LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz => {
            (Timestamp::NINFINITY.0 + 1, Timestamp::INFINITY.0 - 1, true)
        }
        _ => return None,
    })
}

/// `Value::Numeric`: the value of a column type with the internal integer `i`.
fn numeric(ty: &LogicalType, i: i64) -> Value {
    match ty.id() {
        LogicalTypeId::TinyInt => Value::tinyint(i as i8),
        LogicalTypeId::SmallInt => Value::smallint(i as i16),
        LogicalTypeId::Integer => Value::integer(i as i32),
        LogicalTypeId::Date => Value::date(Date(i as i32)),
        LogicalTypeId::TimestampTz => Value::timestamp_tz(Timestamp(i)),
        _ => Value::timestamp(Timestamp(i)),
    }
}

fn infinity(ty: &LogicalType, negative: bool) -> Value {
    match (ty.id(), negative) {
        (LogicalTypeId::Date, true) => Value::date(Date::NINFINITY),
        (LogicalTypeId::Date, false) => Value::date(Date::INFINITY),
        (LogicalTypeId::TimestampTz, true) => Value::timestamp_tz(Timestamp::NINFINITY),
        (LogicalTypeId::TimestampTz, false) => Value::timestamp_tz(Timestamp::INFINITY),
        (_, true) => Value::timestamp(Timestamp::NINFINITY),
        (_, false) => Value::timestamp(Timestamp::INFINITY),
    }
}

/// `SafeMidpoint`.
fn midpoint(l: i64, h: i64) -> i64 {
    l.wrapping_add(((h as u64).wrapping_sub(l as u64) / 2) as i64)
}

/// `FirstTrueSuffix`: the first index of `[lo, hi]` where `pred` holds, `hi + 1` if none.
fn first_true_suffix(pred: &dyn Fn(i64) -> Option<bool>, lo: i64, hi: i64) -> Option<i64> {
    if !pred(hi)? {
        return Some(hi + 1);
    }
    if pred(lo)? {
        return Some(lo);
    }
    let (mut l, mut h) = (lo, hi);
    while (h as u64).wrapping_sub(l as u64) > 1 {
        let mid = midpoint(l, h);
        if pred(mid)? {
            h = mid;
        } else {
            l = mid;
        }
    }
    Some(h)
}

/// `LastTruePrefix`: the last index of `[lo, hi]` where `pred` holds, `lo - 1` if none.
fn last_true_prefix(pred: &dyn Fn(i64) -> Option<bool>, lo: i64, hi: i64) -> Option<i64> {
    if !pred(lo)? {
        return Some(lo - 1);
    }
    if pred(hi)? {
        return Some(hi);
    }
    let (mut l, mut h) = (lo, hi);
    while (h as u64).wrapping_sub(l as u64) > 1 {
        let mid = midpoint(l, h);
        if pred(mid)? {
            l = mid;
        } else {
            h = mid;
        }
    }
    Some(l)
}

fn comparison(op: ComparisonOp, left: BoundExpr, right: Value) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(left),
            right: Box::new(BoundExpr::constant(right)),
        },
        LogicalType::BOOLEAN,
    )
}

/// `MakeColBound`: `col >= bound` or `col <= bound`; on the edge of the finite domain of a type with
/// infinities, a strict comparison with the infinity next to it.
fn column_bound(lower: bool, column: &BoundExpr, bound: i64, edge: i64, has_infinity: bool) -> BoundExpr {
    let ty = &column.return_type;
    if has_infinity && bound == edge {
        let op = if lower { ComparisonOp::GreaterThan } else { ComparisonOp::LessThan };
        return comparison(op, column.clone(), infinity(ty, lower));
    }
    let op = if lower { ComparisonOp::GreaterThanOrEqual } else { ComparisonOp::LessThanOrEqual };
    comparison(op, column.clone(), numeric(ty, bound))
}

fn flip(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::LessThan => ComparisonOp::GreaterThan,
        ComparisonOp::LessThanOrEqual => ComparisonOp::GreaterThanOrEqual,
        ComparisonOp::GreaterThan => ComparisonOp::LessThan,
        ComparisonOp::GreaterThanOrEqual => ComparisonOp::LessThanOrEqual,
        other => other,
    }
}

/// `MonotonePreimageRule::Apply`. `is_root` says the comparison is a conjunct of a filter.
pub fn monotone_preimage(expr: &mut BoundExpr, is_root: bool) -> bool {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return false;
    };
    let (function, constant, op) = if matches!(left.kind, BoundExprKind::Function { .. }) && right.is_foldable() {
        (&**left, &**right, *op)
    } else if matches!(right.kind, BoundExprKind::Function { .. }) && left.is_foldable() {
        (&**right, &**left, flip(*op))
    } else {
        return false;
    };
    let BoundExprKind::Function { function: f, children, .. } = &function.kind else {
        return false;
    };
    let [column] = children.as_slice() else {
        return false;
    };
    let Some(increasing) = crate::execution::zone_map::unary_monotonicity(&f.name) else {
        return false;
    };
    if function.is_volatile() || !matches!(column.kind, BoundExprKind::ColumnRef { .. }) {
        return false;
    }
    let Some((dom_lo, dom_hi, has_infinity)) = finite_domain(&column.return_type) else {
        return false;
    };
    // infinities can make date and time functions NULL of what is not
    if has_infinity && !is_root {
        return false;
    }
    let Some(mut c) = try_evaluate_scalar(constant).filter(|c| !c.is_null()) else {
        return false;
    };
    if c.logical_type() != &function.return_type {
        let cast = BoundExpr::constant(c).cast_to(&function.return_type, true);
        match try_evaluate_scalar(&cast) {
            Some(value) if !value.is_null() => c = value,
            _ => return false,
        }
    }
    if !matches!(
        op,
        ComparisonOp::Equal
            | ComparisonOp::LessThan
            | ComparisonOp::LessThanOrEqual
            | ComparisonOp::GreaterThan
            | ComparisonOp::GreaterThanOrEqual
    ) {
        return false;
    }
    let column_type = column.return_type.clone();
    // `EvalFunc`: f of the value with internal integer `i`, nothing where it is NULL or fails
    let evaluate = |i: i64| -> Option<Value> {
        let mut probe = function.clone();
        if let BoundExprKind::Function { children, .. } = &mut probe.kind {
            children[0] = BoundExpr::constant(numeric(&column_type, i));
        }
        try_evaluate_scalar(&probe).filter(|v| !v.is_null())
    };
    let ge = |i: i64| compare_values(ComparisonOp::GreaterThanOrEqual, &evaluate(i)?, &c);
    let gt = |i: i64| compare_values(ComparisonOp::GreaterThan, &evaluate(i)?, &c);
    let need_ge = matches!(op, ComparisonOp::GreaterThanOrEqual | ComparisonOp::LessThan | ComparisonOp::Equal);
    let need_gt = matches!(op, ComparisonOp::GreaterThan | ComparisonOp::LessThanOrEqual | ComparisonOp::Equal);
    let (mut a, mut b);
    if increasing {
        let x_ge = if need_ge { first_true_suffix(&ge, dom_lo, dom_hi) } else { Some(0) };
        let x_gt = if need_gt { first_true_suffix(&gt, dom_lo, dom_hi) } else { Some(0) };
        let (Some(x_ge), Some(x_gt)) = (x_ge, x_gt) else {
            return false;
        };
        (a, b) = match op {
            ComparisonOp::GreaterThanOrEqual => (x_ge, dom_hi),
            ComparisonOp::GreaterThan => (x_gt, dom_hi),
            ComparisonOp::LessThan => (dom_lo, x_ge - 1),
            ComparisonOp::LessThanOrEqual => (dom_lo, x_gt - 1),
            _ => (x_ge, x_gt - 1),
        };
    } else {
        let y_ge = if need_ge { last_true_prefix(&ge, dom_lo, dom_hi) } else { Some(0) };
        let y_gt = if need_gt { last_true_prefix(&gt, dom_lo, dom_hi) } else { Some(0) };
        let (Some(y_ge), Some(y_gt)) = (y_ge, y_gt) else {
            return false;
        };
        (a, b) = match op {
            ComparisonOp::GreaterThanOrEqual => (dom_lo, y_ge),
            ComparisonOp::GreaterThan => (dom_lo, y_gt),
            ComparisonOp::LessThan => (y_ge + 1, dom_hi),
            ComparisonOp::LessThanOrEqual => (y_gt + 1, dom_hi),
            _ => (y_gt + 1, y_ge),
        };
    }
    a = a.max(dom_lo);
    b = b.min(dom_hi);
    let column = (*column).clone();
    if a > b {
        // f of an infinity is NULL rather than false, which a constant would get wrong
        if has_infinity {
            return false;
        }
        let call = crate::function::BuiltinFunctions::get().scalar("constant_or_null").and_then(|set| {
            crate::function::binder::bind_scalar_function(
                set,
                vec![BoundExpr::constant(Value::boolean(false)), column],
                false,
                &try_evaluate_scalar,
            )
            .ok()
        });
        let Some(call) = call else {
            return false;
        };
        *expr = call;
        return true;
    }
    // with infinities both bounds stay, which keeps them out; otherwise a bound that narrows nothing goes
    let need_lower = a > dom_lo || has_infinity;
    let need_upper = b < dom_hi || has_infinity;
    if !need_lower && !need_upper {
        return false;
    }
    // the finite value below -infinity, where it meets the comparison
    let mut below_matches = false;
    if has_infinity && a == dom_lo {
        let Some(below) = evaluate(dom_lo - 2) else {
            return false;
        };
        let Some(matches) = compare_values(op, &below, &c) else {
            return false;
        };
        below_matches = matches;
    }
    let lower = if need_lower && below_matches {
        Some(comparison(ComparisonOp::NotEqual, column.clone(), infinity(&column.return_type, true)))
    } else if need_lower {
        Some(column_bound(true, &column, a, dom_lo, has_infinity))
    } else {
        None
    };
    let upper = need_upper.then(|| column_bound(false, &column, b, dom_hi, has_infinity));
    *expr = match (lower, upper) {
        (Some(lower), Some(upper)) => BoundExpr::new(
            BoundExprKind::Conjunction {
                and: true,
                children: vec![lower, upper],
            },
            LogicalType::BOOLEAN,
        ),
        (Some(bound), None) | (None, Some(bound)) => bound,
        (None, None) => unreachable!("checked"),
    };
    true
}
