// Ported in spirit from duckdb/src/optimizer/join_filter_pushdown_optimizer.cpp and the min/max and
// Bloom filters of duckdb/src/execution/operator/join/physical_hash_join.cpp @ rust-port-baseline
//
//! Join filters: a hash join whose probe rows without a match go nowhere tells a scan below its
//! probe side which keys its build side has, as a range and a Bloom filter, so that the scan drops
//! the other rows before it reads the rest of their columns, and before they reach the join.

use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::value::Value;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys};
use duckrs_common::vector::Vector;
use duckrs_parser::ast::JoinType;
use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A build side with more keys than this is not worth a filter: it would take long to fill and be
/// too large to stay in a cache.
const MAX_KEYS: usize = 1 << 24;

/// A table with fewer rows than this is read too fast for a Bloom filter to pay off.
const MIN_BLOOM_TABLE_ROWS: usize = 1 << 16;

/// Bits of the Bloom filter per key.
const BITS_PER_KEY: usize = 16;

/// A filter checks this many rows before it can find that it keeps too many.
const ROWS_TO_JUDGE: u64 = 1 << 20;

/// `dynamic_or_filter_threshold`: a build side of at most this many rows gives an IN filter. Set by
/// the client for the plans it runs.
static IN_FILTER_THRESHOLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(50);

pub fn set_in_filter_threshold(threshold: usize) {
    IN_FILTER_THRESHOLD.store(threshold, Ordering::Relaxed);
}

/// What a join filter knows of the build keys of its join.
pub struct JoinFilter {
    /// `IS NOT DISTINCT FROM`: a NULL can match
    null_safe: bool,
    /// the type of the keys
    ty: LogicalType,
    /// `dynamic_or_filter_threshold` when the filter was planned
    in_threshold: usize,
    /// the keys once the build side is done, or `None` if they are not something a filter can use
    keys: OnceLock<Option<Keys>>,
    /// what `JoinFilterPushdownInfo::FinalizeFilters` gives the scans, once the build side is done
    published: OnceLock<Option<Published>>,
    checked: AtomicU64,
    passed: AtomicU64,
    /// whether the filter gave up: its join was built twice
    off: AtomicBool,
    /// whether the Bloom filter gave up: it let most rows through
    bloom_off: AtomicBool,
    /// whether a scan it went to is large enough for the Bloom filter
    bloom: AtomicBool,
}

/// The filter of the build keys that a scan applies and shows (`Dynamic Filters`).
#[derive(Debug, Clone)]
enum Published {
    /// one key: `k = v`
    Single(Key),
    /// a few keys that are not a range of all integers between: `k IN (...)`, in order
    In(Vec<Key>),
    /// `k >= min AND k <= max`
    Range(Key, Key),
}

/// A key value, by the bytes that order it among the others of its type.
#[derive(Debug, Clone)]
struct Key {
    order: Vec<u8>,
    value: Value,
    /// the value of an integer type
    int: Option<i64>,
}

const ASCENDING: OrderModifiers = OrderModifiers::new(OrderType::Ascending, NullOrder::NullsLast);

/// The keys of `count` rows of `vector`, `None` for NULL.
fn keys_of(vector: &Vector, count: usize) -> Vec<Option<Key>> {
    let Ok(orders) = create_sort_keys(&[(vector, ASCENDING)], count) else {
        return Vec::new();
    };
    let unified = vector.to_unified(count);
    let integer = supported(vector.logical_type());
    let mut ints = vec![None; count];
    if integer {
        for_each_value(vector, count, |row, value| ints[row] = value);
    }
    orders
        .into_iter()
        .enumerate()
        .map(|(row, order)| {
            unified.is_valid(row).then(|| Key {
                order,
                value: vector.get_value(row),
                int: ints[row],
            })
        })
        .collect()
}

/// `PushdownJoinFilterExpressionInternal`: no nested types, and no INTERVAL.
pub(crate) fn filterable(ty: &LogicalType) -> bool {
    !ty.is_nested()
        && !matches!(
            ty.id(),
            LogicalTypeId::Interval | LogicalTypeId::Any | LogicalTypeId::Unknown | LogicalTypeId::SqlNull
        )
}

impl fmt::Debug for JoinFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JoinFilter")
    }
}

struct Keys {
    min: i64,
    max: i64,
    /// a Bloom filter of 64-bit blocks, each key setting three bits of one block
    blocks: Vec<u64>,
    /// how many low bits of a hash the block index drops
    shift: u32,
    has_null: bool,
}

/// What types a filter can take its keys of: the integers keys of tables are.
fn supported(ty: &LogicalType) -> bool {
    matches!(
        ty.id(),
        LogicalTypeId::Integer | LogicalTypeId::BigInt | LogicalTypeId::Date
    )
}

#[inline]
fn hash(value: i64) -> u64 {
    // MurmurHash3's finalizer
    let mut h = value as u64;
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

#[inline]
fn block_bits(h: u64) -> u64 {
    (1 << (h & 63)) | (1 << ((h >> 6) & 63)) | (1 << ((h >> 12) & 63))
}

impl Keys {
    #[inline]
    fn block(&self, h: u64) -> usize {
        if self.shift >= 64 {
            0
        } else {
            (h >> self.shift) as usize
        }
    }

    #[inline]
    fn may_contain(&self, value: i64) -> bool {
        if value < self.min || value > self.max {
            return false;
        }
        let h = hash(value);
        let bits = block_bits(h);
        self.blocks[self.block(h)] & bits == bits
    }
}

/// Calls `f` with each row's value, or `None` for a NULL, of an integer vector of a supported type.
fn for_each_value(vector: &Vector, count: usize, mut f: impl FnMut(usize, Option<i64>)) {
    let unified = vector.to_unified(count);
    if vector.logical_type().id() == LogicalTypeId::BigInt {
        let data = unified.data::<i64>();
        for row in 0..count {
            f(row, unified.is_valid(row).then(|| data[unified.sel.get(row)]));
        }
    } else {
        let data = unified.data::<i32>();
        for row in 0..count {
            f(
                row,
                unified.is_valid(row).then(|| i64::from(data[unified.sel.get(row)])),
            );
        }
    }
}

impl JoinFilter {
    pub fn new(null_safe: bool, ty: LogicalType) -> Self {
        Self {
            null_safe,
            ty,
            in_threshold: IN_FILTER_THRESHOLD.load(Ordering::Relaxed),
            keys: OnceLock::new(),
            published: OnceLock::new(),
            checked: AtomicU64::new(0),
            passed: AtomicU64::new(0),
            off: AtomicBool::new(false),
            bloom_off: AtomicBool::new(false),
            bloom: AtomicBool::new(false),
        }
    }

    /// Takes the keys of the whole build side, `count` rows of `keys`; `complete` is false when some
    /// of the build side is not there (it went to a file), and then the filter lets everything pass.
    pub fn set(&self, keys: &Vector, count: usize, complete: bool) {
        let typed = supported(keys.logical_type());
        let usable = complete && count <= MAX_KEYS && typed;
        let value = usable.then(|| {
            let blocks = (count * BITS_PER_KEY / 64).max(1).next_power_of_two();
            let mut result = Keys {
                min: i64::MAX,
                max: i64::MIN,
                blocks: vec![0; blocks],
                shift: 64 - blocks.trailing_zeros(),
                has_null: false,
            };
            for_each_value(keys, count, |_, value| match value {
                Some(value) => {
                    result.min = result.min.min(value);
                    result.max = result.max.max(value);
                    let h = hash(value);
                    let block = result.block(h);
                    result.blocks[block] |= block_bits(h);
                }
                None => result.has_null = true,
            });
            result
        });
        let published = complete.then(|| self.finalize_filters(keys, count)).flatten();
        if self.keys.set(value).is_err() || self.published.set(published).is_err() {
            // a join that runs twice would give a scan the keys of the time before
            self.off.store(true, Ordering::Relaxed);
        }
    }

    /// `FinalizeFilters`: one key, an IN list of a few, or their range. NULL keys match no row
    /// with `=`; with `IS NOT DISTINCT FROM` the NULL rows of the scan stay anyway.
    fn finalize_filters(&self, keys: &Vector, count: usize) -> Option<Published> {
        if count > MAX_KEYS {
            return None;
        }
        let mut values: Vec<Key> = keys_of(keys, count).into_iter().flatten().collect();
        values.sort_by(|a, b| a.order.cmp(&b.order));
        let (min, max) = (values.first()?.clone(), values.last()?.clone());
        if min.order == max.order {
            return Some(Published::Single(min));
        }
        // `PushInFilter`: neither NULL (they are out already) nor every integer of the range
        if count > 1 && count <= self.in_threshold {
            values.dedup_by(|a, b| a.order == b.order);
            let dense = match (min.int, max.int) {
                (Some(low), Some(high)) => (high as i128 - low as i128 + 1) == values.len() as i128,
                _ => false,
            };
            if !dense {
                return Some(Published::In(values));
            }
        }
        Some(Published::Range(min, max))
    }

    /// Whether the filter has the keys of its join and still drops rows with them.
    pub fn is_active(&self) -> bool {
        if self.off.load(Ordering::Relaxed) {
            return false;
        }
        self.published.get().is_some_and(Option::is_some) || self.keys.get().is_some_and(Option::is_some)
    }

    /// `PhysicalTableScan::GetFilterInfo` of what the filter gives the scan, for the column `name`.
    pub fn describe(&self, name: &str) -> Option<String> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let literal = |key: &Key| BoundExpr::constant(key.value.clone()).to_string();
        // `RetainRuntimeFilterNulls`
        let retain = |filter: String| {
            if self.null_safe {
                format!("({name} IS NULL OR {filter})")
            } else {
                filter
            }
        };
        Some(match self.published.get()?.as_ref()? {
            Published::Single(v) => {
                let op = if self.null_safe { "IS NOT DISTINCT FROM" } else { "=" };
                retain(format!("{name} {op} {}", literal(v)))
            }
            Published::In(values) => {
                let list: Vec<String> = values.iter().map(literal).collect();
                format!("optional: {}", retain(format!("{name} IN ({})", list.join(", "))))
            }
            Published::Range(min, max) => format!(
                "(optional: {} AND optional: {})",
                retain(format!("{name} >= {}", literal(min))),
                retain(format!("{name} <= {}", literal(max)))
            ),
        })
    }

    /// The rows of `count` of `values` that can match a build key, or `None` if all of them can.
    pub fn select(&self, values: &Vector, count: usize) -> Option<Vec<u32>> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let published = self.published.get().and_then(Option::as_ref);
        // the Bloom filter only where it is complete, and while it drops enough rows
        let keys = self
            .keys
            .get()
            .and_then(Option::as_ref)
            .filter(|_| self.bloom.load(Ordering::Relaxed) && !self.bloom_off.load(Ordering::Relaxed));
        // an IN list and a range are optional filters (`CreateOptionalFilterExpression`), which C++
        // only prunes row groups by: only one key drops rows
        let published = published.filter(|p| matches!(p, Published::Single(_)));
        if published.is_none() && keys.is_none() {
            return None;
        }
        let null_passes = self.null_safe;
        let mut kept = Vec::with_capacity(count);
        if supported(values.logical_type()) {
            let int = |key: &Key| key.int.unwrap_or_default();
            for_each_value(values, count, |row, value| {
                let pass = match value {
                    None => null_passes,
                    Some(value) => {
                        let exact = match published {
                            Some(Published::Single(key)) => value == int(key),
                            Some(Published::In(keys)) => keys.binary_search_by(|k| int(k).cmp(&value)).is_ok(),
                            Some(Published::Range(min, max)) => value >= int(min) && value <= int(max),
                            None => true,
                        };
                        exact && keys.is_none_or(|keys| keys.may_contain(value))
                    }
                };
                if pass {
                    kept.push(row as u32);
                }
            });
        } else {
            let Some(published) = published else {
                return None;
            };
            for (row, key) in keys_of(values, count).into_iter().enumerate() {
                let pass = match key {
                    None => null_passes,
                    Some(key) => match published {
                        Published::Single(single) => key.order == single.order,
                        Published::In(keys) => keys.binary_search_by(|k| k.order.cmp(&key.order)).is_ok(),
                        Published::Range(min, max) => key.order >= min.order && key.order <= max.order,
                    },
                };
                if pass {
                    kept.push(row as u32);
                }
            }
        }
        let checked = self.checked.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
        let passed = self.passed.fetch_add(kept.len() as u64, Ordering::Relaxed) + kept.len() as u64;
        if checked >= ROWS_TO_JUDGE && passed * 10 > checked * 8 {
            self.bloom_off.store(true, Ordering::Relaxed);
        }
        Some(kept)
    }
}

thread_local! {
    /// The `Dynamic Filters` of the table scans of the statement that runs, by their order in the
    /// plan, for EXPLAIN ANALYZE, which shows a plan of its own.
    static RECORDED: std::cell::RefCell<Option<Vec<Option<String>>>> = const { std::cell::RefCell::new(None) };
}

/// Makes the next statement that runs keep what its table scans were given.
pub fn start_recording() {
    RECORDED.with(|r| *r.borrow_mut() = Some(Vec::new()));
}

/// What the statement since [`start_recording`] gave its table scans.
pub fn take_recorded() -> Vec<Option<String>> {
    RECORDED.with(|r| r.borrow_mut().take()).unwrap_or_default()
}

/// Keeps what the table scans of `op` were given, once, if [`start_recording`] asked for it.
pub fn record(op: &dyn crate::execution::physical::PhysicalOperator) {
    fn collect(op: &dyn crate::execution::physical::PhysicalOperator, list: &mut Vec<Option<String>>) {
        if op.name() == "TABLE_SCAN" {
            list.push(op.dynamic_filters());
        }
        for child in op.children() {
            collect(child.as_ref(), list);
        }
    }
    RECORDED.with(|r| {
        if let Some(list) = r.borrow_mut().as_mut()
            && list.is_empty()
        {
            collect(op, list);
            // nothing to show still says the statement ran
            list.push(None);
        }
    });
}

/// `JoinFilterPushdownOptimizer`: gives each hash join that drops the probe rows without a match a
/// filter for each equality on a column of a scan below its probe side, in that scan
/// (`JoinTypeIsSupported`). The range joins of C++ push min and max filters of their keys, which
/// duckrs does not.
pub fn push_join_filters(op: &mut LogicalOperator) {
    // the recursive part runs again for each round: only the anchor (`VisitOperator`)
    for child in &mut op.children {
        push_join_filters(child);
    }
    let LogicalOperatorKind::Join {
        join_type: JoinType::Inner | JoinType::Semi | JoinType::RightSemi | JoinType::Right,
        conditions,
        ..
    } = &mut op.kind
    else {
        return;
    };
    // `CanGenerateJoinFilters`
    if conditions.iter().any(|c| c.left.is_volatile()) {
        return;
    }
    for condition in conditions.iter_mut() {
        let BoundExprKind::Reference(column) = condition.left.kind else {
            continue;
        };
        if condition.filter.is_some()
            || !condition.is_equality()
            || !filterable(&condition.left.return_type)
            || condition.left.return_type != condition.right.return_type
        {
            continue;
        }
        let filter = std::sync::Arc::new(JoinFilter::new(condition.null_safe(), condition.left.return_type.clone()));
        if push_into(&mut op.children[0], column, &filter) {
            condition.filter = Some(filter);
        }
    }
}

fn has_volatile_expressions(op: &mut LogicalOperator) -> bool {
    op.expressions_mut().iter().any(|e| e.is_volatile())
}

/// `CanSkipSubtree`: what an empty build side lets the join skip.
fn can_skip(op: &mut LogicalOperator) -> bool {
    use LogicalOperatorKind as K;
    if matches!(
        op.kind,
        K::Insert { .. }
            | K::Update { .. }
            | K::Delete { .. }
            | K::MergeInto { .. }
            | K::CopyToFile { .. }
            | K::CopyToParquet { .. }
            | K::CteRef { .. }
            | K::RecursiveCte { .. }
    ) || has_volatile_expressions(op)
    {
        return false;
    }
    op.children.iter_mut().all(can_skip)
}

/// `FindFilterTargets`: puts `filter` on output column `column` of `op` into the scans its values
/// come from, where the way there keeps them as they are and drops no row that a row of the scan
/// would make. Gives whether it went anywhere.
fn push_into(op: &mut LogicalOperator, column: usize, filter: &std::sync::Arc<JoinFilter>) -> bool {
    use LogicalOperatorKind as K;
    if has_volatile_expressions(op) {
        return false;
    }
    let left_columns = op.children.first().map_or(0, |c| c.types().len());
    match &mut op.kind {
        // `FindScanFilterTargets`
        K::Get {
            table,
            column_ids,
            filters,
            projection,
            index_rows: None,
            sample: None,
            ..
        } => {
            let position = match projection {
                Some(projection) => match projection.get(column) {
                    Some(position) => *position,
                    None => return false,
                },
                None => column,
            };
            let Some(column_id) = column_ids.get(position) else {
                return false;
            };
            let Some(definition) = table.columns.get(*column_id) else {
                return false;
            };
            if definition.ty != filter.ty {
                return false;
            }
            let already = filters.iter().any(|f| {
                matches!(&f.kind, BoundExprKind::JoinFilter { child, filter: other }
                    if std::sync::Arc::ptr_eq(other, filter)
                        && matches!(child.kind, BoundExprKind::Reference(p) if p == position))
            });
            if table.row_count() >= MIN_BLOOM_TABLE_ROWS {
                filter.bloom.store(true, Ordering::Relaxed);
            }
            if !already {
                let child = BoundExpr::new(BoundExprKind::Reference(position), definition.ty.clone())
                    .with_alias(definition.name.clone());
                filters.push(BoundExpr::new(
                    BoundExprKind::JoinFilter {
                        child: Box::new(child),
                        filter: filter.clone(),
                    },
                    LogicalType::BOOLEAN,
                ));
            }
            true
        }
        // a SECURE VIEW takes a join filter as it takes a pushed WHERE filter: it cannot fail
        K::Filter { .. } | K::Order { .. } | K::Distinct { on: false, .. } | K::SecureView { .. } => {
            push_into(&mut op.children[0], column, filter)
        }
        K::ColumnSelect { keep } => {
            let Some(kept) = keep.get(column).copied() else {
                return false;
            };
            match op.children[0].column_bindings().iter().position(|binding| *binding == kept) {
                Some(child_column) => push_into(&mut op.children[0], child_column, filter),
                None => false,
            }
        }
        // `PushdownProjectionColumns`
        K::Projection { expressions, .. } => match expressions.get(column).map(|e| &e.kind) {
            Some(BoundExprKind::Reference(child_column)) => {
                let child_column = *child_column;
                push_into(&mut op.children[0], child_column, filter)
            }
            _ => false,
        },
        // `PushdownAggregateColumns`: a group of every grouping set
        K::Aggregate {
            groups, grouping_sets, ..
        } => {
            if !grouping_sets.iter().all(|set| set.contains(&column)) {
                return false;
            }
            match groups.get(column).map(|g| &g.kind) {
                Some(BoundExprKind::Reference(child_column)) => {
                    let child_column = *child_column;
                    push_into(&mut op.children[0], child_column, filter)
                }
                _ => false,
            }
        }
        // `FindWindowFilterTargets`: a column all windows partition by
        K::Window { windows, .. } => {
            let shared = column < left_columns
                && windows.iter().all(|window| {
                    window
                        .partitions
                        .iter()
                        .any(|p| matches!(p.kind, BoundExprKind::Reference(c) if c == column))
                });
            shared && push_into(&mut op.children[0], column, filter)
        }
        // `FindSetOperationFilterTargets`: EXCEPT keeps the rows of its first child
        K::Union { .. } | K::ExceptIntersect { except: false, .. } => {
            let mut any = false;
            for child in &mut op.children {
                any |= push_into(child, column, filter);
            }
            any
        }
        K::ExceptIntersect { except: true, .. } => push_into(&mut op.children[0], column, filter),
        // the columns of its input, not those it unnests
        K::Unnest { .. } => column < left_columns && push_into(&mut op.children[0], column, filter),
        K::MaterializedCte { .. } => can_skip(&mut op.children[0]) && push_into(&mut op.children[1], column, filter),
        // `FindJoinFilterTargets`: an INNER join takes the filter on either side, and to the columns
        // its equalities make equal; a filtered build side makes the probe side's rows go
        K::CrossProduct
        | K::Join {
            join_type: JoinType::Inner,
            ..
        } => {
            let mut equal = Vec::new();
            if let K::Join { conditions, .. } = &op.kind {
                for condition in conditions {
                    if condition.comparison != duckrs_parser::ast::ComparisonOp::Equal
                        || condition.left.return_type != condition.right.return_type
                    {
                        continue;
                    }
                    if let (BoundExprKind::Reference(l), BoundExprKind::Reference(r)) =
                        (&condition.left.kind, &condition.right.kind)
                    {
                        if column < left_columns && *l == column {
                            equal.push((1, *r));
                        } else if column >= left_columns && *r == column - left_columns {
                            equal.push((0, *l));
                        }
                    }
                }
            }
            let own = if column < left_columns {
                (0, column)
            } else {
                (1, column - left_columns)
            };
            let mut targets = vec![own];
            targets.extend(equal);
            let left_skippable = can_skip(&mut op.children[0]);
            let mut any = false;
            for (side, at) in targets {
                if side == 1 && !left_skippable {
                    continue;
                }
                any |= push_into(&mut op.children[side], at, filter);
            }
            any
        }
        // `FilterTargetPreservesLeftInput`
        K::Join {
            join_type: JoinType::Left | JoinType::Semi | JoinType::Anti | JoinType::Single,
            ..
        }
        | K::MarkJoin { .. } => column < left_columns && push_into(&mut op.children[0], column, filter),
        _ => false,
    }
}
