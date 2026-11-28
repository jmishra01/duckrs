// Ported from duckdb/src/optimizer/pushdown/pushdown_get.cpp and the table filter part of
// duckdb/src/optimizer/filter_combiner.cpp @ rust-port-baseline
//
//! Moves filters on one column into the scan below them (`TableFilterSet`), where they run on each
//! chunk as it is read. In-memory scans have no zone maps yet, so all this gains is that no
//! operator is left to pass the rows on, and that the columns only they read stay in the scan. It
//! runs late, so that the passes before it see filters as operators, and only takes what can run
//! ahead of the filters that stay: nothing that can fail, unless it is the only filter.

use super::rewriter::{can_throw, contains_barrier, split_conjuncts};
use crate::planner::expression::BoundOperator;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use pintail_parser::ast::ComparisonOp;

/// The columns of the expression, in the query around the plan too, with `outer` for those.
fn columns_of(expr: &BoundExpr, columns: &mut Vec<ColumnBinding>, outer: &mut bool) {
    if let BoundExprKind::ColumnRef { binding, depth } = &expr.kind {
        *outer |= *depth != 0;
        columns.push(*binding);
    }
    for child in expr.children() {
        columns_of(child, columns, outer);
    }
}

/// `TryPushdownGenericExpression`, for a filter on the output of a scan.
fn can_move_into_scan(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile() && !can_throw(expr)
}

/// [`can_move_into_scan`], for a filter that may fail.
fn can_move_ignoring_errors(expr: &BoundExpr, scan: &[ColumnBinding]) -> bool {
    let mut columns = Vec::new();
    let mut outer = false;
    columns_of(expr, &mut columns, &mut outer);
    let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]);
    one_column && !outer && scan.contains(&columns[0]) && !expr.is_volatile()
}

/// A comparison with the constant on the left turned around, as `FilterCombiner` makes a table
/// filter of the column: `5 < x` is `x > 5`.
fn column_first(mut expr: BoundExpr) -> BoundExpr {
    if let BoundExprKind::Comparison { op, left, right } = &mut expr.kind
        && matches!(left.kind, BoundExprKind::Constant(_))
        && !matches!(right.kind, BoundExprKind::Constant(_))
    {
        std::mem::swap(left, right);
        *op = super::statistics::flip(*op);
    }
    expr
}

/// `TryPushdownPrefixFilter`: `prefix(s, 'abc')` on a column is `s >= 'abc' AND s < 'abd'`, which
/// a scan's zone maps can use.
fn prefix_bounds(expr: &BoundExpr) -> Option<[BoundExpr; 2]> {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return None;
    };
    if !matches!(function.name.as_str(), "prefix" | "starts_with" | "^@") {
        return None;
    }
    let [
        column,
        BoundExpr {
            kind: BoundExprKind::Constant(value),
            ..
        },
    ] = children.as_slice()
    else {
        return None;
    };
    if !matches!(column.kind, BoundExprKind::ColumnRef { .. }) || value.is_null() {
        return None;
    }
    let prefix = value.try_to_string().ok()?;
    if prefix.is_empty() || column.return_type.collation().is_some() {
        return None;
    }
    // `Utf8Proc::FindNextLegalUTF8`: the next string of the same length, by its last character
    let mut chars: Vec<char> = prefix.chars().collect();
    let last = chars.pop()?;
    let next = (u32::from(last) + 1..=u32::from(char::MAX)).find_map(char::from_u32)?;
    chars.push(next);
    let upper: String = chars.into_iter().collect();
    let compare = |op, bound: String| {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(column.clone()),
                right: Box::new(BoundExpr::constant(pintail_common::value::Value::varchar(bound))),
            },
            pintail_common::types::LogicalType::BOOLEAN,
        )
    };
    Some([
        compare(pintail_parser::ast::ComparisonOp::GreaterThanOrEqual, prefix),
        compare(pintail_parser::ast::ComparisonOp::LessThan, upper),
    ])
}

/// What `FilterCombiner` puts into a scan for a filter.
pub(super) enum TablePushdown {
    /// `PUSHED_DOWN_FULLY`: these filters in place of it
    Fully(Vec<BoundExpr>),
    /// `PUSHED_DOWN_PARTIALLY` with filters of the scan: these go in, and the filter stays
    Partially(Vec<BoundExpr>),
    /// `PUSHED_DOWN_PARTIALLY`: an optional filter for the zone maps, and the filter stays
    Optional(BoundExpr),
    None,
}

fn compare(op: ComparisonOp, column: &BoundExpr, value: Value) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(column.clone()),
            right: Box::new(BoundExpr::constant(value)),
        },
        LogicalType::BOOLEAN,
    )
}

/// `TryPushdownDenseInFilter`: consecutive integers as `col >= min AND col <= max`.
fn dense_range(column: &BoundExpr, values: &[Value]) -> Option<Vec<BoundExpr>> {
    let mut numbers = Vec::with_capacity(values.len());
    for value in values {
        let ty = value.logical_type();
        if !ty.is_integral() || ty == &LogicalType::UHUGEINT {
            return None;
        }
        numbers.push((value.as_i128()?, value));
    }
    numbers.sort_by_key(|(n, _)| *n);
    if numbers.is_empty()
        || !numbers
            .windows(2)
            .all(|pair| pair[1].0.checked_sub(pair[0].0) == Some(1))
    {
        return None;
    }
    let min = numbers[0].1.clone();
    let max = numbers[numbers.len() - 1].1.clone();
    Some(vec![
        compare(ComparisonOp::GreaterThanOrEqual, column, min),
        compare(ComparisonOp::LessThanOrEqual, column, max),
    ])
}

fn in_list(column: &BoundExpr, values: Vec<Value>) -> BoundExpr {
    let children = std::iter::once(column.clone())
        .chain(values.into_iter().map(BoundExpr::constant))
        .collect();
    BoundExpr::new(
        BoundExprKind::Operator {
            op: BoundOperator::In,
            children,
        },
        LogicalType::BOOLEAN,
    )
}

/// `TryPushdownInFilter` and `TryPushdownOrClause`.
/// `Utf8Proc::FindNextLegalUTF8`: the next string of the same length, by its last character.
fn next_legal_utf8(text: &str) -> Option<String> {
    let mut chars: Vec<char> = text.chars().collect();
    let last = chars.pop()?;
    let next = (u32::from(last) + 1..=u32::from(char::MAX)).find_map(char::from_u32)?;
    chars.push(next);
    Some(chars.into_iter().collect())
}

/// `TryPushdownLikeFilter`: `col LIKE 'abc%...'` has `col >= 'abc' AND col < 'abd'`, and without a
/// wildcard is `col = 'abc'`; `ILIKE` has the bounds of its prefix in any case (`GetCaseInsensitivePrefixBounds`).
fn like_pushdown(expr: &BoundExpr) -> Option<TablePushdown> {
    let BoundExprKind::Function { function, children, .. } = &expr.kind else {
        return None;
    };
    let insensitive = function.name == "~~*";
    if function.name != "~~" && !insensitive {
        return None;
    }
    let [
        column,
        BoundExpr {
            kind: BoundExprKind::Constant(value),
            ..
        },
    ] = children.as_slice()
    else {
        return None;
    };
    if !matches!(column.kind, BoundExprKind::ColumnRef { .. })
        || value.is_null()
        || column.return_type.collation().is_some()
    {
        return None;
    }
    let pattern = value.try_to_string().ok()?;
    if pattern.is_empty() || pattern.starts_with(['%', '_']) {
        return None;
    }
    let end = pattern.find(['%', '_']);
    let prefix = &pattern[..end.unwrap_or(pattern.len())];
    let compare = |op, bound: String| {
        BoundExpr::new(
            BoundExprKind::Comparison {
                op,
                left: Box::new(column.clone()),
                right: Box::new(BoundExpr::constant(Value::varchar(bound))),
            },
            LogicalType::BOOLEAN,
        )
    };
    if end.is_none() && !insensitive {
        return Some(TablePushdown::Fully(vec![compare(
            ComparisonOp::Equal,
            prefix.to_string(),
        )]));
    }
    if insensitive {
        if !prefix.is_ascii() {
            return None;
        }
        let min = prefix.to_ascii_uppercase();
        let max: String = prefix
            .chars()
            .map(|c| match c.to_ascii_lowercase() {
                'i' => "\u{130}".to_string(),
                'k' => "\u{212A}".to_string(),
                lower => lower.to_string(),
            })
            .collect();
        let upper = next_legal_utf8(&max)?;
        return Some(TablePushdown::Partially(vec![
            compare(ComparisonOp::GreaterThanOrEqual, min),
            compare(ComparisonOp::LessThan, upper),
        ]));
    }
    let mut bounds = vec![compare(ComparisonOp::GreaterThanOrEqual, prefix.to_string())];
    if let Some(upper) = next_legal_utf8(prefix) {
        bounds.push(compare(ComparisonOp::LessThan, upper));
    }
    Some(TablePushdown::Partially(bounds))
}

/// `GetTemporalCastMargin`: how far, in the units of the column's type, a cast of `source` to `target` can move a
/// value away from the order of the column: 2h of DST ambiguity, a day of time of day, a second of rounding.
fn temporal_cast_margin(source: &LogicalType, target: &LogicalType) -> Option<i64> {
    use pintail_common::types::LogicalTypeId::*;
    const MICROS_PER_SEC: i64 = 1_000_000;
    const MICROS_PER_HOUR: i64 = 3_600 * MICROS_PER_SEC;
    const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;
    if source.id() == target.id() {
        return matches!(
            source.id(),
            Date | Timestamp | TimestampTz | TimestampNs | TimestampTzNs | TimestampMs | TimestampSec
        )
        .then_some(0);
    }
    Some(match (source.id(), target.id()) {
        (Timestamp | TimestampTz, Timestamp | TimestampTz) => 2 * MICROS_PER_HOUR,
        (Timestamp | TimestampTz, Date) => MICROS_PER_DAY,
        (Timestamp | TimestampTz, TimestampMs | TimestampSec) => MICROS_PER_SEC,
        (TimestampNs | TimestampTzNs, Timestamp | TimestampMs | TimestampSec) => MICROS_PER_SEC * 1000,
        (TimestampNs | TimestampTzNs, Date) => MICROS_PER_DAY * 1000,
        (TimestampMs, Date) => MICROS_PER_DAY / 1000,
        (TimestampMs, TimestampSec) => 1000,
        (TimestampSec, Date) => MICROS_PER_DAY / MICROS_PER_SEC,
        (Date, Timestamp | TimestampMs | TimestampSec | TimestampNs) => 0,
        (Date, TimestampTz | TimestampTzNs) => 1,
        _ => return None,
    })
}

/// `AdjustTemporalValue`: `value` moved by `margin` of its units; none where that overflows.
fn adjust_temporal(value: &Value, margin: i64) -> Option<Value> {
    use pintail_common::value::ValueData;
    if margin == 0 {
        return Some(value.clone());
    }
    let data = match value.data() {
        // `MakeTemporalValue` clamps a DATE to its range
        ValueData::Int32(days) => {
            let moved = i64::from(*days).checked_add(margin)?;
            ValueData::Int32(moved.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        }
        ValueData::Int64(raw) => ValueData::Int64(raw.checked_add(margin)?),
        _ => return None,
    };
    Value::new(value.logical_type().clone(), data).ok()
}

/// `TryPushdownTemporalCastFilter`: `CAST(col AS T) > c` of temporal types gives the scan the optional
/// `col >= c' - margin`, with `c` as a value of the column's type, so that its zone maps skip row groups.
fn temporal_cast_pushdown(expr: &BoundExpr) -> Option<TablePushdown> {
    let BoundExprKind::Comparison { op, left, right } = &expr.kind else {
        return None;
    };
    let (cast, constant, op) = match (&left.kind, &right.kind) {
        (BoundExprKind::Cast { .. }, BoundExprKind::Constant(value)) => (&**left, value, *op),
        (BoundExprKind::Constant(value), BoundExprKind::Cast { .. }) => (&**right, value, super::statistics::flip(*op)),
        _ => return None,
    };
    if !matches!(
        op,
        ComparisonOp::Equal
            | ComparisonOp::LessThan
            | ComparisonOp::LessThanOrEqual
            | ComparisonOp::GreaterThan
            | ComparisonOp::GreaterThanOrEqual
    ) {
        return None;
    }
    let BoundExprKind::Cast { child, .. } = &cast.kind else {
        return None;
    };
    if !matches!(child.kind, BoundExprKind::ColumnRef { depth: 0, .. }) || constant.is_null() {
        return None;
    }
    let margin = temporal_cast_margin(&child.return_type, &cast.return_type)?;
    let value = crate::execution::parquet::try_cast_value(constant, &child.return_type)?;
    let mut parts = Vec::new();
    if matches!(
        op,
        ComparisonOp::GreaterThan | ComparisonOp::GreaterThanOrEqual | ComparisonOp::Equal
    ) {
        parts.push(compare(
            ComparisonOp::GreaterThanOrEqual,
            child,
            adjust_temporal(&value, -margin)?,
        ));
    }
    if matches!(
        op,
        ComparisonOp::LessThan | ComparisonOp::LessThanOrEqual | ComparisonOp::Equal
    ) {
        parts.push(compare(
            ComparisonOp::LessThanOrEqual,
            child,
            adjust_temporal(&value, margin)?,
        ));
    }
    let optional = match parts.len() {
        1 => parts.pop().expect("one"),
        _ => BoundExpr::new(
            BoundExprKind::Conjunction {
                and: true,
                children: parts,
            },
            LogicalType::BOOLEAN,
        ),
    };
    Some(TablePushdown::Optional(optional))
}

pub(super) fn table_pushdown(expr: &BoundExpr) -> TablePushdown {
    if let Some(pushdown) = like_pushdown(expr) {
        return pushdown;
    }
    match &expr.kind {
        BoundExprKind::Operator {
            op: BoundOperator::In,
            children,
        } => {
            if !matches!(children[0].kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
                return TablePushdown::None;
            }
            let mut values = Vec::with_capacity(children.len() - 1);
            for child in &children[1..] {
                match &child.kind {
                    BoundExprKind::Constant(value) if !value.is_null() => values.push(value.clone()),
                    _ => return TablePushdown::None,
                }
            }
            if values.len() == 1 {
                let value = values.pop().expect("one value");
                return TablePushdown::Fully(vec![compare(ComparisonOp::Equal, &children[0], value)]);
            }
            if let Some(range) = dense_range(&children[0], &values) {
                return TablePushdown::Fully(range);
            }
            TablePushdown::Optional(expr.clone())
        }
        BoundExprKind::Conjunction { and: false, children } if !children.is_empty() => {
            let mut column: Option<&BoundExpr> = None;
            let mut equalities = Vec::new();
            let mut only_equalities = true;
            let mut parts = Vec::new();
            for child in children {
                let BoundExprKind::Comparison { op, left, right } = &child.kind else {
                    return TablePushdown::None;
                };
                let (col, value, op) = match (&left.kind, &right.kind) {
                    (BoundExprKind::ColumnRef { depth: 0, .. }, BoundExprKind::Constant(v)) => (&**left, v, *op),
                    (BoundExprKind::Constant(v), BoundExprKind::ColumnRef { depth: 0, .. }) => {
                        (&**right, v, super::statistics::flip(*op))
                    }
                    _ => return TablePushdown::None,
                };
                let binding = |e: &BoundExpr| match &e.kind {
                    BoundExprKind::ColumnRef { binding, .. } => Some(*binding),
                    _ => None,
                };
                match column {
                    None => column = Some(col),
                    Some(first) if binding(first) == binding(col) => {}
                    Some(_) => return TablePushdown::None,
                }
                only_equalities &= op == ComparisonOp::Equal;
                if value.is_null() {
                    match op {
                        ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom => {
                            let null_op = if op == ComparisonOp::DistinctFrom {
                                BoundOperator::IsNotNull
                            } else {
                                BoundOperator::IsNull
                            };
                            parts.push(BoundExpr::new(
                                BoundExprKind::Operator {
                                    op: null_op,
                                    children: vec![col.clone()],
                                },
                                LogicalType::BOOLEAN,
                            ));
                        }
                        // `x = NULL` in an OR passes no row
                        _ => {}
                    }
                    continue;
                }
                if op == ComparisonOp::Equal {
                    equalities.push(value.clone());
                }
                parts.push(compare(op, col, value.clone()));
            }
            let column = column.expect("an OR has children");
            if only_equalities && !equalities.is_empty() {
                if let Some(range) = dense_range(column, &equalities) {
                    return TablePushdown::Fully(range);
                }
                return TablePushdown::Optional(in_list(column, equalities));
            }
            let optional = match parts.len() {
                0 => return TablePushdown::None,
                1 => parts.pop().expect("one part"),
                _ => BoundExpr::new(
                    BoundExprKind::Conjunction {
                        and: false,
                        children: parts,
                    },
                    LogicalType::BOOLEAN,
                ),
            };
            TablePushdown::Optional(optional)
        }
        _ => temporal_cast_pushdown(expr).unwrap_or(TablePushdown::None),
    }
}

/// Moves the filters directly on scans into them.
pub fn push_into_scans(op: &mut LogicalOperator, late: &std::collections::HashSet<usize>) {
    push_into(op, None, late);
}

/// [`push_into_scans`] of the filters on scans of tables and Parquet files only, whose table filters statistics
/// then check (`PushdownGet` runs before statistics for any `LogicalGet` with `filter_pushdown`).
pub fn push_into_table_scans(op: &mut LogicalOperator, filtered: &std::collections::HashSet<usize>) {
    push_into(op, Some(filtered), &std::collections::HashSet::new());
}

/// The table indexes of the scans with a filter right above them.
pub fn scans_with_filters(op: &LogicalOperator) -> std::collections::HashSet<usize> {
    fn visit(op: &LogicalOperator, found: &mut std::collections::HashSet<usize>) {
        for child in &op.children {
            visit(child, found);
        }
        if let K::LateralJoin { right, .. } = &op.kind {
            visit(right, found);
        }
        if let K::Filter { .. } = &op.kind
            && let Some(K::Get { table_index, .. } | K::ParquetScan { table_index, .. }) =
                op.children.first().map(|child| &child.kind)
        {
            found.insert(*table_index);
        }
    }
    let mut found = std::collections::HashSet::new();
    visit(op, &mut found);
    found
}

/// `before_statistics`: the scans whose filters go in now, those the filter pushdown left filters on. `late`: the
/// scans a filter only reached after the filter pushdown, which keep it above them.
fn push_into(
    op: &mut LogicalOperator,
    before_statistics: Option<&std::collections::HashSet<usize>>,
    late: &std::collections::HashSet<usize>,
) {
    for child in &mut op.children {
        push_into(child, before_statistics, late);
    }
    if let K::LateralJoin { right, .. } = &mut op.kind {
        push_into(right, before_statistics, late);
    }
    let K::Filter { expressions } = &mut op.kind else {
        return;
    };
    if let Some(K::Get { table_index, .. } | K::ParquetScan { table_index, .. }) =
        op.children.first().map(|child| &child.kind)
        && late.contains(table_index)
    {
        return;
    }
    if let Some(filtered) = before_statistics {
        match &op.children[0].kind {
            K::Get { table_index, .. } | K::ParquetScan { table_index, .. } if filtered.contains(table_index) => {}
            _ => return,
        }
    }
    let before_statistics = before_statistics.is_some();
    // `MultiFileFunction` has no `filter_pushdown` for CSV and JSON, only `pushdown_complex_filter`, which
    // leaves out files
    if matches!(op.children[0].kind, K::CsvScan { .. } | K::JsonScan { .. }) {
        let bindings = op.children[0].column_bindings();
        let mut conjuncts = Vec::new();
        for expr in std::mem::take(expressions) {
            split_conjuncts(expr, &mut conjuncts);
        }
        match &mut op.children[0].kind {
            K::CsvScan { scan: data, .. } => match std::sync::Arc::get_mut(data) {
                Some(data) => prune_files(data, &bindings, &mut conjuncts),
                None => {
                    let mut copy = super::type_pushdown::clone_scan(data);
                    prune_files(&mut copy, &bindings, &mut conjuncts);
                    *data = std::sync::Arc::new(copy);
                }
            },
            K::JsonScan { scan: data, .. } => prune_files(std::sync::Arc::make_mut(data), &bindings, &mut conjuncts),
            _ => unreachable!("checked above"),
        }
        *expressions = conjuncts;
        if expressions.is_empty() {
            *op = op.children.remove(0);
        }
        return;
    }
    if !matches!(op.children[0].kind, K::Get { .. } | K::ParquetScan { .. }) {
        return;
    }
    // `PushdownGet`: a scan that has its table filters takes no more; what comes later (the ranges the joins'
    // statistics derive) stays above it
    if !before_statistics
        && let K::Get {
            filters,
            zone_filters,
            optional_filters,
            ..
        } = &op.children[0].kind
        && !(filters.is_empty() && zone_filters.is_empty() && optional_filters.is_empty())
    {
        return;
    }
    let scan = op.children[0].column_bindings();
    let mut conjuncts = Vec::new();
    for expr in std::mem::take(expressions) {
        split_conjuncts(expr, &mut conjuncts);
    }
    let (filters, mut optional) = match &mut op.children[0].kind {
        K::Get {
            filters,
            optional_filters,
            ..
        } => (filters, Some(optional_filters)),
        // `MultiFileFunction` has `filter_pushdown`; these run as the file is read
        K::ParquetScan { scan: data, .. } => {
            let data = std::sync::Arc::make_mut(data);
            prune_files(data, &scan, &mut conjuncts);
            (&mut data.filters, Some(&mut data.optional_filters))
        }
        _ => unreachable!("checked above"),
    };
    // a column with an optional filter already has its filter above the scan: the one statistics rewrote there
    // stays (it went into the scan partly when the filters were pushed before statistics)
    let optionally_pushed: Vec<ColumnBinding> = optional
        .as_deref()
        .map(|optional| {
            optional
                .iter()
                .filter_map(|filter| {
                    let mut columns = Vec::new();
                    let mut outer = false;
                    columns_of(filter, &mut columns, &mut outer);
                    columns
                        .first()
                        .copied()
                        .filter(|c| !outer && columns.iter().all(|other| other == c))
                })
                .collect()
        })
        .unwrap_or_default();
    let kept_above = |conjunct: &BoundExpr| {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(conjunct, &mut columns, &mut outer);
        columns
            .first()
            .is_some_and(|c| columns.iter().all(|other| other == c) && optionally_pushed.contains(c))
    };
    let (barred, conjuncts): (Vec<BoundExpr>, Vec<BoundExpr>) = conjuncts.into_iter().partition(contains_barrier);
    // what went into the scan only as an optional filter, which stays here as well
    let mut partial = Vec::new();
    for conjunct in conjuncts {
        if kept_above(&conjunct) && matches!(table_pushdown(&conjunct), TablePushdown::None) {
            partial.push(conjunct);
            continue;
        }
        if can_move_into_scan(&conjunct, &scan) {
            match (table_pushdown(&conjunct), optional.as_deref_mut()) {
                (TablePushdown::Fully(pushed), _) => {
                    filters.extend(pushed);
                    continue;
                }
                (TablePushdown::Optional(pushed), Some(optional)) => {
                    // the scan may have it from when the filters went in before statistics
                    if !optional.iter().any(|o| super::rewriter::expressions_equal(o, &pushed)) {
                        optional.push(pushed);
                    }
                    partial.push(conjunct);
                    continue;
                }
                (TablePushdown::Partially(pushed), _) => {
                    filters.extend(pushed);
                    partial.push(conjunct);
                    continue;
                }
                _ => {}
            }
            if let Some(bounds) = prefix_bounds(&conjunct) {
                filters.extend(bounds);
                continue;
            }
            filters.push(column_first(conjunct));
        } else {
            // `TryPushdownTemporalCastFilter` runs on filters that can throw too: the optional filter only skips
            // row groups, and the filter itself stays
            if can_move_ignoring_errors(&conjunct, &scan)
                && let (Some(TablePushdown::Optional(pushed)), Some(optional)) =
                    (temporal_cast_pushdown(&conjunct), optional.as_deref_mut())
                && !optional.iter().any(|o| super::rewriter::expressions_equal(o, &pushed))
            {
                optional.push(pushed);
            }
            expressions.push(conjunct);
        }
    }
    // `PushdownGet`: a filter that can fail goes into the scan too when it is the only one left, as then there is
    // no other filter it would run ahead of
    if let [only] = expressions.as_slice()
        && barred.is_empty()
        && partial.is_empty()
        && can_move_ignoring_errors(only, &scan)
    {
        let only = expressions.pop().expect("one filter");
        match prefix_bounds(&only) {
            Some(bounds) => filters.extend(bounds),
            None => filters.push(column_first(only)),
        }
    }
    // `PushdownBarrierFilters`: into the scan, after the filters there, only if no other filter stays on top
    for conjunct in barred {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(&conjunct, &mut columns, &mut outer);
        let one_column = !columns.is_empty() && columns.iter().all(|c| *c == columns[0]) && scan.contains(&columns[0]);
        if expressions.is_empty() && partial.is_empty() && one_column && !outer && !conjunct.is_volatile() {
            filters.push(conjunct);
        } else {
            expressions.push(conjunct);
        }
    }
    expressions.splice(0..0, partial);
    // `TryCreateMultiColumnExpressionFilter`: a filter on several columns of a table stays, and a copy tells the
    // scan which row groups none of its rows pass in
    if let K::Get { zone_filters, .. } = &mut op.children[0].kind {
        for expr in expressions.iter() {
            let mut columns = Vec::new();
            let mut outer = false;
            columns_of(expr, &mut columns, &mut outer);
            let several = columns.iter().any(|c| *c != columns[0]);
            // `CanPropagateExpressionStatistics`: not of what is volatile or can throw
            if several
                && !outer
                && columns.iter().all(|c| scan.contains(c))
                && !expr.is_volatile()
                && !super::rewriter::can_throw(expr)
            {
                zone_filters.push(expr.clone());
            }
        }
    }
    if expressions.is_empty() {
        *op = op.children.remove(0);
    }
}

/// The files of a multi-file scan, with the columns that have one value for each of them.
pub(crate) trait FileList {
    fn paths(&self) -> Vec<String>;
    /// The value of output column `at` in the file at `path`, if it is the same for all its rows.
    fn known_value(&self, at: usize, path: &str) -> Option<Value>;
    /// Keeps the files `keep` says, as filters `file_filters` left them of `total`.
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize);
}

impl FileList for crate::execution::parquet::ParquetScanData {
    fn paths(&self) -> Vec<String> {
        self.files.clone()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::parquet::ParquetScanData::known_value(self, at, path)
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

impl FileList for crate::execution::csv::CsvScanData {
    fn paths(&self) -> Vec<String> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::json_scan::known_value(
            &self.names,
            self.filename,
            &self.hive,
            &self.virtual_columns,
            at,
            path,
        )
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        // the bytes sniffing read are of the first file
        if keep.first() == Some(&false) {
            *self.first_data.lock().expect("not poisoned") = None;
        }
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

impl FileList for crate::execution::json_scan::JsonScanData {
    fn paths(&self) -> Vec<String> {
        self.files.clone()
    }
    fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        crate::execution::json_scan::JsonScanData::known_value(self, at, path)
    }
    fn keep_files(&mut self, keep: &[bool], file_filters: &str, total: usize) {
        let mut keep = keep.iter();
        self.files.retain(|_| keep.next().copied().unwrap_or(true));
        self.file_filters.push_str(file_filters);
        self.total_files = Some(total);
    }
}

/// `HivePartitioning::ApplyFiltersToFileList`: the filters that only read the hive partitions and the file name
/// are worked out for each file, those of the files they are false for are left out, and the filters that could
/// be worked out for every file are no longer needed.
fn prune_files(scan: &mut impl FileList, bindings: &[ColumnBinding], conjuncts: &mut Vec<BoundExpr>) {
    let paths = scan.paths();
    let Some(first) = paths.first() else {
        return;
    };
    let known: Vec<ColumnBinding> = bindings
        .iter()
        .copied()
        .filter(|b| scan.known_value(b.column_index, first).is_some())
        .collect();
    if known.is_empty() || conjuncts.is_empty() {
        return;
    }
    let mut keep = vec![true; paths.len()];
    let mut file_filters = String::new();
    conjuncts.retain(|conjunct| {
        let mut columns = Vec::new();
        let mut outer = false;
        columns_of(conjunct, &mut columns, &mut outer);
        // a subquery runs with the query (in a recursive CTE, once per round): not before it
        fn has_subquery(expr: &BoundExpr) -> bool {
            matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
        }
        if columns.is_empty()
            || outer
            || conjunct.is_volatile()
            || has_subquery(conjunct)
            || !columns.iter().all(|c| known.contains(c))
        {
            return true;
        }
        let mut preserved = false;
        let mut applied = false;
        for (path, keep) in paths.iter().zip(keep.iter_mut()) {
            let mut copy = conjunct.clone();
            let mut known_values = true;
            copy.walk_mut(&mut |e| {
                if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
                    match scan.known_value(binding.column_index, path) {
                        Some(value) => *e = BoundExpr::constant(value),
                        None => known_values = false,
                    }
                }
            });
            let result = known_values
                .then(|| crate::execution::expression_executor::try_evaluate_scalar(&copy))
                .flatten();
            match result {
                None => preserved = true,
                Some(value) if value.is_null() || value.data() == &pintail_common::value::ValueData::Bool(false) => {
                    *keep = false;
                    applied = true;
                }
                Some(_) => {}
            }
        }
        // `ExtraOperatorInfo::file_filters`: the filters' texts, one after the other
        if applied {
            file_filters.push_str(&crate::planner::expression::filter_text(conjunct));
        }
        preserved
    });
    scan.keep_files(&keep, &file_filters, paths.len());
}
