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
use duckrs_common::vector::{SelectionVector, Vector};
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

/// `BF_CHECK_N`: the vectors a Bloom filter checks before it can pause.
const VECTORS_TO_CHECK: u32 = 6;

/// `BF_THRESHOLD`: a Bloom filter that lets this share of the rows through pauses.
const SELECTIVITY_THRESHOLD: f64 = 0.5;

/// `VECTOR_PAUSE`: a paused filter checks again after this many vectors, times how many times in a
/// row it paused.
const VECTOR_PAUSE: u32 = 10;

/// Gives each join filter its own number, which the selectivity of each thread is kept by.
static NEXT_FILTER_ID: AtomicU64 = AtomicU64::new(0);

/// `dynamic_or_filter_threshold`: a build side of at most this many rows gives an IN filter. Set by
/// the client for the plans it runs.
static IN_FILTER_THRESHOLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(50);

pub fn set_in_filter_threshold(threshold: usize) {
    IN_FILTER_THRESHOLD.store(threshold, Ordering::Relaxed);
}

/// `disabled_optimizers` without `join_filter_pushdown`, as the client last planned with it.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// `DynamicFilterData` of a Top-N (`TopN::PushdownDynamicFilters`): the scans below drop the rows whose sort key
/// sorts after the boundary of the rows the Top-N keeps, once it has one.
pub struct TopNFilter {
    /// the type of the sort key
    pub ty: LogicalType,
    /// how a row's sort key compares to the boundary for the row to pass: `<` or `>`, `<=` or `>=` with
    /// more than one order
    pub comparison: duckrs_parser::ast::ComparisonOp,
    /// `NULLS FIRST`: a NULL sort key can be among the first rows, and passes
    pub nulls_first: bool,
    /// the first sort key of the last row the Top-N keeps, once it keeps as many as it needs
    boundary: parking_lot::RwLock<Option<Value>>,
}

impl TopNFilter {
    pub fn new(ty: LogicalType, ascending: bool, single: bool, nulls_first: bool) -> Self {
        use duckrs_parser::ast::ComparisonOp as C;
        let comparison = match (ascending, single) {
            (true, true) => C::LessThan,
            (true, false) => C::LessThanOrEqual,
            (false, true) => C::GreaterThan,
            (false, false) => C::GreaterThanOrEqual,
        };
        Self {
            ty,
            comparison,
            nulls_first,
            boundary: parking_lot::RwLock::new(None),
        }
    }

    /// `DynamicFilterData::SetValue`. A NULL boundary is not one a comparison can use: the filter stays as it was.
    pub fn set(&self, value: Value) {
        if !value.is_null() {
            *self.boundary.write() = Some(value);
        }
    }

    /// `DynamicFilterData::ToExpression` of the input at `Reference(0)`: what a row passes, or `None` while every
    /// row does.
    pub fn expression(&self) -> Option<BoundExpr> {
        self.expression_on(BoundExpr::new(BoundExprKind::Reference(0), self.ty.clone()))
    }

    /// What a row passes, with `input` its sort key, or `None` while every row does.
    pub fn expression_on(&self, input: BoundExpr) -> Option<BoundExpr> {
        let boundary = self.boundary.read().clone()?;
        let compare = BoundExpr::new(
            BoundExprKind::Comparison {
                op: self.comparison,
                left: Box::new(input.clone()),
                right: Box::new(BoundExpr::constant(boundary)),
            },
            LogicalType::BOOLEAN,
        );
        if !self.nulls_first {
            return Some(compare);
        }
        let is_null = BoundExpr::new(
            BoundExprKind::Operator {
                op: crate::planner::expression::BoundOperator::IsNull,
                children: vec![input],
            },
            LogicalType::BOOLEAN,
        );
        Some(BoundExpr::new(
            BoundExprKind::Conjunction {
                and: false,
                children: vec![is_null, compare],
            },
            LogicalType::BOOLEAN,
        ))
    }
}

impl fmt::Debug for TopNFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TopNFilter({:?} {:?})", self.comparison, self.boundary.read())
    }
}

/// The filter `FindFilterTargets` puts into the scans: a join's, or a Top-N's, which only goes where every scan
/// can take it (`ok`).
enum Pushed<'a> {
    Join(&'a std::sync::Arc<JoinFilter>),
    TopN(&'a std::sync::Arc<TopNFilter>, &'a std::cell::Cell<bool>),
}

impl Pushed<'_> {
    /// The filter on the scan's column at `position`, of type `ty`, reached through `casts`.
    fn on_column(&self, position: usize, ty: &LogicalType, name: &str, casts: &[(LogicalType, bool)]) -> Option<BoundExpr> {
        match self {
            Pushed::Join(filter) => {
                let (input, keep_null) = filter_input(position, ty, name, casts, filter)?;
                Some(BoundExpr::new(
                    BoundExprKind::JoinFilter {
                        child: Box::new(input),
                        filter: (*filter).clone(),
                        keep_null,
                    },
                    LogicalType::BOOLEAN,
                ))
            }
            // the sort key rebuilt on the column, through casts that keep its order: no TRY_CAST, and none that
            // can fail
            Pushed::TopN(filter, ok) => {
                let mut input = BoundExpr::new(BoundExprKind::Reference(position), ty.clone()).with_alias(name.to_string());
                let mut source = ty.clone();
                for (target, try_cast) in casts {
                    if *try_cast || cast_can_fail(&source, target) {
                        ok.set(false);
                        return None;
                    }
                    if source != *target {
                        input = input.cast_to(target, false);
                    }
                    source = target.clone();
                }
                if source != filter.ty {
                    ok.set(false);
                    return None;
                }
                Some(BoundExpr::new(
                    BoundExprKind::TopNFilter {
                        child: Box::new(input),
                        filter: (*filter).clone(),
                    },
                    LogicalType::BOOLEAN,
                ))
            }
        }
    }

    /// Whether `existing` is this filter on the same input already.
    fn same(&self, existing: &BoundExpr, pushed: &BoundExpr) -> bool {
        match (&existing.kind, &pushed.kind) {
            (
                BoundExprKind::JoinFilter { child, filter, .. },
                BoundExprKind::JoinFilter {
                    child: other_child,
                    filter: other,
                    ..
                },
            ) => std::sync::Arc::ptr_eq(filter, other) && format!("{child:?}") == format!("{other_child:?}"),
            (
                BoundExprKind::TopNFilter { child, filter },
                BoundExprKind::TopNFilter {
                    child: other_child,
                    filter: other,
                },
            ) => std::sync::Arc::ptr_eq(filter, other) && format!("{child:?}") == format!("{other_child:?}"),
            _ => false,
        }
    }
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
    /// whether the filter gave up: its join was built twice
    off: AtomicBool,
    /// what the selectivity of the Bloom filter on each thread is kept by
    id: u64,
    /// whether a scan it went to is large enough for the Bloom filter
    bloom: AtomicBool,
    /// what decides on a Bloom or prefix-range filter (`CanUseBloomFilter`)
    shape: JoinShape,
    /// the largest estimated rows of a scan it went to (`estimated_scan_cardinality`)
    scan_estimate: AtomicU64,
    /// the filter of the build keys published next to `published`
    runtime: OnceLock<Option<RuntimeFilter>>,
    /// `published` with its keys cast to the type of a scan's column, by that type
    converted: parking_lot::Mutex<Vec<(LogicalType, Option<Published>)>>,
}

/// What of its join decides which filters of the build keys a join filter publishes.
#[derive(Debug, Clone, Default)]
pub struct JoinShape {
    /// the equalities among its conditions
    pub equalities: usize,
    /// the estimated rows of the probe side
    pub probe_estimate: u64,
    /// `build_side_has_filter`
    pub build_filtered: bool,
    /// the build key, as the filter names it
    pub key_name: String,
}

/// `DeferredRuntimeFilterType`: what a scan checks its values against besides `Published`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RuntimeFilter {
    /// `k IN BF(key)`
    Bloom,
    /// `k IN PRF(key)`
    PrefixRange,
}

const BUILD_TO_PROBE_RATIO_THRESHOLD: f64 = 1.0;
const NON_FILTERING_RATIO_THRESHOLD: f64 = 0.1;
const NON_FILTERING_BUILD_SIDE_THRESHOLD: usize = 4_194_304;
/// `SMALL_EXACT_PRF_BITS`
const SMALL_EXACT_PRF_BITS: u128 = 1 << 26;

/// `BloomFilterBitBudget`: `GetNumberOfSectors` of the rows, in bits.
fn bloom_filter_bits(rows: usize) -> u128 {
    let bits = 512usize.max(rows.saturating_mul(12)).next_power_of_two();
    ((bits >> 6).min(1 << 26) * 64) as u128
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

/// The rows of the smallest and the largest key of `count` rows of `vector`, NULLs left out.
fn extreme_rows(vector: &Vector, count: usize) -> Option<(usize, usize)> {
    let mut ends: Option<(usize, usize)> = None;
    if supported(vector.logical_type()) {
        let mut bounds = (i64::MAX, i64::MIN);
        for_each_value(vector, count, |row, value| {
            if let Some(value) = value {
                let (low, high) = ends.get_or_insert((row, row));
                if value < bounds.0 {
                    (bounds.0, *low) = (value, row);
                }
                if value > bounds.1 {
                    (bounds.1, *high) = (value, row);
                }
            }
        });
        return ends;
    }
    let orders = create_sort_keys(&[(vector, ASCENDING)], count).ok()?;
    let unified = vector.to_unified(count);
    for (row, order) in orders.iter().enumerate().filter(|&(row, _)| unified.is_valid(row)) {
        let (low, high) = ends.get_or_insert((row, row));
        if order < &orders[*low] {
            *low = row;
        }
        if order > &orders[*high] {
            *high = row;
        }
    }
    ends
}

/// The name of the column an expression of a scan's column reads.
fn column_name(expr: &BoundExpr) -> String {
    match &expr.kind {
        BoundExprKind::Cast { child, .. } => column_name(child),
        _ => expr.alias.clone(),
    }
}

/// The size and signedness of the integer types a join filter goes through casts of.
fn integral(ty: &LogicalType) -> Option<(u8, bool)> {
    Some(match ty.id() {
        LogicalTypeId::TinyInt => (1, true),
        LogicalTypeId::SmallInt => (2, true),
        LogicalTypeId::Integer => (4, true),
        LogicalTypeId::BigInt => (8, true),
        LogicalTypeId::UTinyInt => (1, false),
        LogicalTypeId::USmallInt => (2, false),
        LogicalTypeId::UInteger => (4, false),
        LogicalTypeId::UBigInt => (8, false),
        _ => return None,
    })
}

/// `RuntimeFilterCastCanFail`.
fn cast_can_fail(source: &LogicalType, target: &LogicalType) -> bool {
    if source == target {
        return false;
    }
    let (Some((source_size, source_signed)), Some((target_size, target_signed))) = (integral(source), integral(target))
    else {
        return true;
    };
    if source_size > target_size {
        return true;
    }
    if source_signed == target_signed {
        return false;
    }
    source_signed || source_size >= target_size
}

/// `PushdownJoinFilterExpressionInternal`: the output column under integer casts, also of a VARIANT to an
/// integer (`IsJoinFilterPushdownVariantIntegralCast`), with the casts from the column outward and whether
/// each was a TRY_CAST.
fn peel_casts(expr: &BoundExpr) -> Option<(usize, Vec<(LogicalType, bool)>)> {
    match &expr.kind {
        BoundExprKind::Reference(column) => Some((*column, Vec::new())),
        BoundExprKind::Cast { child, try_cast }
            if (integral(&child.return_type).is_some() || child.return_type.id() == LogicalTypeId::Variant)
                && integral(&expr.return_type).is_some() =>
        {
            let (column, mut casts) = peel_casts(child)?;
            casts.push((expr.return_type.clone(), *try_cast));
            Some((column, casts))
        }
        _ => None,
    }
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
    pub fn new(null_safe: bool, ty: LogicalType, shape: JoinShape) -> Self {
        Self {
            shape,
            scan_estimate: AtomicU64::new(0),
            runtime: OnceLock::new(),
            converted: parking_lot::Mutex::new(Vec::new()),
            null_safe,
            ty,
            in_threshold: IN_FILTER_THRESHOLD.load(Ordering::Relaxed),
            keys: OnceLock::new(),
            published: OnceLock::new(),
            off: AtomicBool::new(false),
            id: NEXT_FILTER_ID.fetch_add(1, Ordering::Relaxed),
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
        let runtime = match &published {
            Some(Published::In(values)) => self.runtime_filter(values.first(), values.last(), count),
            Some(Published::Range(min, max)) => self.runtime_filter(Some(min), Some(max), count),
            _ => None,
        };
        if self.keys.set(value).is_err() || self.published.set(published).is_err() || self.runtime.set(runtime).is_err()
        {
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
        if count > self.in_threshold {
            // `FinalizeMinMax`: too many keys for an IN list, so only the smallest and largest
            let (low, high) = extreme_rows(keys, count)?;
            let mut ends = keys.clone();
            ends.slice(&SelectionVector::from_vec(vec![low as u32, high as u32]), 2);
            let mut ends = keys_of(&ends, 2).into_iter();
            let (min, max) = (ends.next()??, ends.next()??);
            if min.order == max.order {
                return Some(Published::Single(min));
            }
            return Some(Published::Range(min, max));
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

    /// `CanUseBloomFilter`: the join has one equality, and its build side is not larger than its
    /// probe side, nor large and unfiltered next to it.
    fn can_use_bloom_filter(&self, build_rows: usize) -> bool {
        let mut probe = self.shape.probe_estimate;
        if self.shape.build_filtered {
            probe = probe.max(self.scan_estimate.load(Ordering::Relaxed));
        }
        let ratio = build_rows as f64 / probe as f64;
        self.shape.equalities == 1
            && ratio <= BUILD_TO_PROBE_RATIO_THRESHOLD
            && !(!self.shape.build_filtered
                && ratio > NON_FILTERING_RATIO_THRESHOLD
                && build_rows > NON_FILTERING_BUILD_SIDE_THRESHOLD)
    }

    /// `TryComputeSpan`: how far apart the smallest and largest key are, where the type has that.
    fn span(&self, min: &Key, max: &Key) -> Option<u128> {
        if let (Some(low), Some(high)) = (min.int, max.int) {
            let (lowest, highest): (i128, i128) = match self.ty.id() {
                LogicalTypeId::TinyInt => (i8::MIN.into(), i8::MAX.into()),
                LogicalTypeId::SmallInt => (i16::MIN.into(), i16::MAX.into()),
                LogicalTypeId::Integer | LogicalTypeId::Date => (i32::MIN.into(), i32::MAX.into()),
                _ => (i64::MIN.into(), i64::MAX.into()),
            };
            let span = high as i128 - low as i128;
            return (span >= lowest && span <= highest).then_some(span as u128);
        }
        if self.ty.id() == LogicalTypeId::Varchar {
            // `GetPrefixIntegerComparable`: the first four bytes
            let prefix = |key: &Key| {
                let text = key.value.try_to_string().unwrap_or_default();
                let mut bytes = [0u8; 4];
                for (at, byte) in text.bytes().take(4).enumerate() {
                    bytes[at] = byte;
                }
                u32::from_be_bytes(bytes)
            };
            return prefix(max).checked_sub(prefix(min)).map(u128::from);
        }
        None
    }

    /// The prefix-range filter where the keys are close enough together, else the Bloom filter,
    /// for keys that are not all one (`FinalizeFilters`).
    fn runtime_filter(&self, min: Option<&Key>, max: Option<&Key>, build_rows: usize) -> Option<RuntimeFilter> {
        if !self.can_use_bloom_filter(build_rows) {
            return None;
        }
        let prefix_type = supported(&self.ty)
            || matches!(
                self.ty.id(),
                LogicalTypeId::TinyInt
                    | LogicalTypeId::SmallInt
                    | LogicalTypeId::UTinyInt
                    | LogicalTypeId::USmallInt
                    | LogicalTypeId::UInteger
                    | LogicalTypeId::UBigInt
                    | LogicalTypeId::Varchar
            );
        if prefix_type
            && let (Some(min), Some(max)) = (min, max)
            && let Some(span) = self.span(min, max)
            && (span < SMALL_EXACT_PRF_BITS || span <= bloom_filter_bits(build_rows))
        {
            return Some(RuntimeFilter::PrefixRange);
        }
        Some(RuntimeFilter::Bloom)
    }

    /// Whether the filter has the keys of its join and still drops rows with them.
    pub fn is_active(&self) -> bool {
        if self.off.load(Ordering::Relaxed) {
            return false;
        }
        self.published.get().is_some_and(Option::is_some) || self.keys.get().is_some_and(Option::is_some)
    }

    /// `published` for values of `ty`: a scan whose column the join's key is a cast of that cannot
    /// fail compares the column with the keys cast to its type (`FinalizeFilters`). A key that
    /// does not fit leaves no filter, or the range for an IN list.
    /// `DynamicFilter::CheckStatistics` of integers: whether no key the build published lies in `low..=high`, the
    /// bounds of a vector of the scan's column of `ty`.
    pub fn excludes_range(&self, ty: &LogicalType, low: i64, high: i64) -> bool {
        let outside = |key: &Key| key.int.is_some_and(|k| k < low || k > high);
        match self.published_for(ty) {
            Some(Published::Single(key)) => outside(&key),
            Some(Published::In(keys)) => keys.iter().all(outside),
            Some(Published::Range(min, max)) => match (min.int, max.int) {
                (Some(min), Some(max)) => max < low || min > high,
                _ => false,
            },
            None => false,
        }
    }

    fn published_for(&self, ty: &LogicalType) -> Option<Published> {
        let published = self.published.get()?.as_ref()?;
        if *ty == self.ty {
            return Some(published.clone());
        }
        let mut converted = self.converted.lock();
        if let Some((_, result)) = converted.iter().find(|(t, _)| t == ty) {
            return result.clone();
        }
        let cast = |key: &Key| -> Option<Key> {
            let value = BoundExpr::constant(key.value.clone()).cast_to(ty, true);
            let value = crate::execution::expression_executor::evaluate_scalar(&value).ok()?;
            if value.is_null() {
                return None;
            }
            keys_of(&Vector::constant(&value), 1).pop().flatten()
        };
        let result = match published {
            Published::Single(key) => cast(key).map(Published::Single),
            Published::Range(min, max) => cast(min).zip(cast(max)).map(|(min, max)| Published::Range(min, max)),
            Published::In(keys) => match keys.iter().map(cast).collect::<Option<Vec<_>>>() {
                Some(keys) => Some(Published::In(keys)),
                None => keys
                    .first()
                    .and_then(cast)
                    .zip(keys.last().and_then(cast))
                    .map(|(min, max)| Published::Range(min, max)),
            },
        };
        converted.push((ty.clone(), result.clone()));
        result
    }

    /// `PhysicalTableScan::GetFilterInfo` of what the filter gives the scan through `input`, an
    /// expression of its column. `keep_null`: a NULL of `input` is a value its cast failed on.
    pub fn describe(&self, input: &BoundExpr, keep_null: bool) -> Option<String> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let column = column_name(input);
        let name = &input.to_string();
        let literal = |key: &Key| BoundExpr::constant(key.value.clone()).to_string();
        // `RetainRuntimeFilterNulls`
        let retain = |filter: String| {
            if self.null_safe || keep_null {
                format!("({name} IS NULL OR {filter})")
            } else {
                filter
            }
        };
        let runtime = self.runtime.get().copied().flatten().map(|runtime| {
            let kind = match runtime {
                RuntimeFilter::Bloom => "BF",
                RuntimeFilter::PrefixRange => "PRF",
            };
            format!("optional: {column} IN {kind}({})", self.shape.key_name)
        });
        let published = match &self.published_for(&input.return_type)? {
            Published::Single(v) => {
                let op = if self.null_safe { "IS NOT DISTINCT FROM" } else { "=" };
                retain(format!("{name} {op} {}", literal(v)))
            }
            Published::In(values) => {
                let list: Vec<String> = values.iter().map(literal).collect();
                format!("optional: {}", retain(format!("{name} IN ({})", list.join(", "))))
            }
            // a prefix-range filter takes the place of the range
            Published::Range(..) if runtime.is_some() && self.runtime.get().copied().flatten() == Some(RuntimeFilter::PrefixRange) => {
                return runtime;
            }
            Published::Range(min, max) => format!(
                "(optional: {} AND optional: {})",
                retain(format!("{name} >= {}", literal(min))),
                retain(format!("{name} <= {}", literal(max)))
            ),
        };
        // `TableFilterSet::PushFilter`: the runtime filter comes after, ANDed
        Some(match runtime {
            Some(runtime) => format!("({published} AND {runtime})"),
            None => published,
        })
    }

    /// The rows of `count` of `values` that can match a build key, or `None` if all of them can.
    pub fn select(&self, values: &Vector, count: usize, keep_null: bool) -> Option<Vec<u32>> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let converted = self.published_for(values.logical_type());
        let published = converted.as_ref();
        // an IN list and a range are optional filters (`CreateOptionalFilterExpression`), which C++
        // only prunes row groups by: only one key drops rows, and then exactly
        let published = published.filter(|p| matches!(p, Published::Single(_)));
        // the Bloom filter where it is complete, and on each thread while it drops enough rows
        // (`BloomFilterSelect`)
        let keys = self
            .keys
            .get()
            .and_then(Option::as_ref)
            .filter(|_| published.is_none() && self.bloom.load(Ordering::Relaxed))
            .filter(|_| {
                selectivity(self.id, |stats| {
                    // a paused filter lets every row through, and only counts the vector
                    let active = stats.is_active();
                    if !active {
                        stats.update(0, 0);
                    }
                    active
                })
            });
        if published.is_none() && keys.is_none() {
            return None;
        }
        let null_passes = self.null_safe || keep_null;
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
        if keys.is_some() {
            selectivity(self.id, |stats| stats.update(kept.len(), count));
        }
        Some(kept)
    }
}

/// `SelectivityOptionalFilterState::SelectivityStats`: how many rows a filter let through on one
/// thread, which pauses it while it lets most of them through, for longer each time.
#[derive(Debug, Default)]
struct SelectivityStats {
    accepted: usize,
    processed: usize,
    vectors: u32,
    paused: bool,
    pause_multiplier: u32,
}

impl SelectivityStats {
    fn is_active(&self) -> bool {
        !self.paused
    }

    /// `Update`, after a vector: `processed` rows of which `accepted` passed, both 0 while paused.
    fn update(&mut self, accepted: usize, processed: usize) {
        self.vectors += 1;
        self.accepted += accepted;
        self.processed += processed;
        if self.vectors == self.pause_multiplier.max(1) * VECTOR_PAUSE {
            (self.vectors, self.accepted, self.processed) = (0, 0, 0);
            self.paused = false;
        } else if !self.paused && self.vectors >= VECTORS_TO_CHECK {
            let selectivity = if self.processed == 0 {
                0.0
            } else {
                self.accepted as f64 / self.processed as f64
            };
            if selectivity >= SELECTIVITY_THRESHOLD {
                self.paused = true;
                self.pause_multiplier += 1;
            } else {
                self.pause_multiplier = 0;
            }
        }
    }
}

thread_local! {
    /// The selectivity of the Bloom filters this thread ran, by filter: C++ keeps it in the state of
    /// each thread's scan (`BloomFilterLocalState`)
    static SELECTIVITY: std::cell::RefCell<Vec<(u64, SelectivityStats)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with this thread's selectivity of filter `id`.
fn selectivity<T>(id: u64, f: impl FnOnce(&mut SelectivityStats) -> T) -> T {
    SELECTIVITY.with(|all| {
        let mut all = all.borrow_mut();
        let at = match all.iter().position(|(other, _)| *other == id) {
            Some(at) => at,
            None => {
                // the filters of statements that ran before are done with
                if all.len() >= 64 {
                    all.remove(0);
                }
                all.push((id, SelectivityStats::default()));
                all.len() - 1
            }
        };
        f(&mut all[at].1)
    })
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
        if matches!(op.name(), "TABLE_SCAN" | "READ_PARQUET") {
            list.push(op.dynamic_filters());
        }
        // what is inside a secure view is not shown, so neither are its scans
        if op.name() == "SECURE_VIEW" {
            return;
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

thread_local! {
    /// The estimated rows of the probe side of each join, in pre-order, of the plan about to be
    /// planned: the estimates of the plan before its column bindings were resolved.
    static PROBE_ESTIMATES: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

/// Keeps the estimates of the probe sides of the joins of `plan`, which has its column bindings
/// still, for the next [`push_join_filters`].
pub fn note_probe_estimates(plan: &LogicalOperator) {
    fn collect(op: &LogicalOperator, out: &mut Vec<u64>) {
        if matches!(op.kind, LogicalOperatorKind::Join { .. }) {
            out.push(crate::optimizer::join_order::estimated_cardinality(&op.children[0]));
        }
        for child in &op.children {
            collect(child, out);
        }
    }
    let mut estimates = Vec::new();
    collect(plan, &mut estimates);
    PROBE_ESTIMATES.with(|e| *e.borrow_mut() = Some(estimates));
}

/// `JoinFilterPushdownOptimizer`: gives each hash join that drops the probe rows without a match a
/// filter for each equality on a column of a scan below its probe side, in that scan
/// (`JoinTypeIsSupported`). The range joins of C++ push min and max filters of their keys, which
/// duckrs does not.
pub fn push_join_filters(op: &mut LogicalOperator) {
    let estimates = PROBE_ESTIMATES.with(|e| e.borrow_mut().take());
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    push_join_filters_with(op, estimates.as_deref(), &mut 0);
}

fn push_join_filters_with(op: &mut LogicalOperator, estimates: Option<&[u64]>, next: &mut usize) {
    let noted = if matches!(op.kind, LogicalOperatorKind::Join { .. }) {
        *next += 1;
        estimates.and_then(|e| e.get(*next - 1).copied())
    } else {
        None
    };
    // the recursive part runs again for each round: only the anchor (`VisitOperator`)
    for child in &mut op.children {
        push_join_filters_with(child, estimates, next);
    }
    if let LogicalOperatorKind::TopN { .. } = op.kind {
        push_top_n_filter(op);
        return;
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
    let equalities = conditions.iter().filter(|c| c.is_equality()).count();
    let probe_estimate =
        noted.unwrap_or_else(|| crate::optimizer::join_order::estimated_cardinality(&op.children[0]));
    let build_filtered = is_filtering(&op.children[1]);
    for condition in conditions.iter_mut() {
        let Some((column, casts)) = peel_casts(&condition.left) else {
            continue;
        };
        if condition.filter.is_some()
            || !condition.is_equality()
            || !filterable(&condition.left.return_type)
            || condition.left.return_type != condition.right.return_type
        {
            continue;
        }
        let filter = std::sync::Arc::new(JoinFilter::new(
            condition.null_safe(),
            condition.left.return_type.clone(),
            JoinShape {
                equalities,
                probe_estimate,
                build_filtered,
                key_name: condition.right.to_string(),
            },
        ));
        if push_into(&mut op.children[0], column, &casts, &Pushed::Join(&filter)) {
            condition.filter = Some(filter);
        }
    }
}

/// `TopN::PushdownDynamicFilters`: a Top-N on a column of numbers or strings gives the scans its values come from
/// a filter of its boundary, where each of them can rebuild the sort key on its column.
fn push_top_n_filter(op: &mut LogicalOperator) {
    let LogicalOperatorKind::TopN { orders, dynamic, .. } = &op.kind else {
        return;
    };
    if dynamic.is_some() {
        return;
    }
    let Some(first) = orders.first() else {
        return;
    };
    let BoundExprKind::Reference(column) = first.expr.kind else {
        return;
    };
    let ty = first.expr.return_type.clone();
    // `TypeIsNumeric` of its physical type, or VARCHAR
    use duckrs_common::types::PhysicalType as P;
    let numeric = matches!(
        ty.physical_type(),
        P::Int8 | P::Int16 | P::Int32 | P::Int64 | P::Int128 | P::UInt8 | P::UInt16 | P::UInt32 | P::UInt64 | P::UInt128
            | P::Float | P::Double
    );
    if !(numeric || ty.id() == LogicalTypeId::Varchar) {
        return;
    }
    let filter = std::sync::Arc::new(TopNFilter::new(
        ty,
        first.modifiers.order == OrderType::Ascending,
        orders.len() == 1,
        first.modifiers.nulls == NullOrder::NullsFirst,
    ));
    // all scans or none
    let ok = std::cell::Cell::new(true);
    let mut child = op.children[0].clone();
    if !push_into(&mut child, column, &[], &Pushed::TopN(&filter, &ok)) || !ok.get() {
        return;
    }
    op.children[0] = child;
    if let LogicalOperatorKind::TopN { dynamic, .. } = &mut op.kind {
        *dynamic = Some(filter);
    }
}

/// `JoinFilterPushdownOptimizer::IsFiltering`.
fn is_filtering(op: &LogicalOperator) -> bool {
    use LogicalOperatorKind as K;
    match &op.kind {
        K::Get { filters, .. } => !filters.is_empty(),
        K::Filter { .. } | K::TopN { .. } => true,
        _ => op.children.iter().any(is_filtering),
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

/// `CreateRuntimeFilterInputExpression`: what a join filter on the scan's column at `position`, of type `ty`, reads
/// through `casts`, and whether a row whose cast fails stays for the query's own cast to fail on. None where the
/// casts do not lead to the filter's type.
fn filter_input(
    position: usize,
    ty: &LogicalType,
    name: &str,
    casts: &[(LogicalType, bool)],
    filter: &JoinFilter,
) -> Option<(BoundExpr, bool)> {
    if casts.last().map_or(ty, |(ty, _)| ty) != &filter.ty {
        return None;
    }
    // a cast that can fail is a TRY_CAST, and where the query's own cast would have failed the row
    // stays for it (`preserves_cast_errors`)
    let column_ref = BoundExpr::new(BoundExprKind::Reference(position), ty.clone()).with_alias(name.to_string());
    let mut input = column_ref.clone();
    let (mut reconstruct, mut keep_null) = (false, false);
    let mut source = ty.clone();
    for (target, try_cast) in casts {
        let can_fail = cast_can_fail(&source, target);
        if source != *target {
            input = input.cast_to(target, *try_cast || can_fail);
        }
        reconstruct |= can_fail;
        keep_null |= !try_cast && can_fail;
        source = target.clone();
    }
    // `RequiresRuntimeFilterExpressionReconstruction`: without a cast that can fail the column itself is
    // compared, with the keys cast to its type
    let input = if reconstruct || filter.null_safe { input } else { column_ref };
    Some((input, keep_null))
}

/// `FindFilterTargets`: puts `filter` on output column `column` of `op` into the scans its values
/// come from, where the way there keeps them as they are and drops no row that a row of the scan
/// would make. Gives whether it went anywhere.
fn push_into(
    op: &mut LogicalOperator,
    column: usize,
    casts: &[(LogicalType, bool)],
    filter: &Pushed,
) -> bool {
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
            // the row id, which the scan's vectors know without reading anything
            let row_id = (*column_id == crate::planner::logical::COLUMN_IDENTIFIER_ROW_ID)
                .then(|| (LogicalType::BIGINT, "rowid".to_string()));
            let Some((ty, name)) =
                row_id.or_else(|| table.columns.get(*column_id).map(|d| (d.ty.clone(), d.name.clone())))
            else {
                return false;
            };
            let Some(pushed) = filter.on_column(position, &ty, &name, casts) else {
                return false;
            };
            if let Pushed::Join(filter) = filter {
                if table.row_count() >= MIN_BLOOM_TABLE_ROWS {
                    filter.bloom.store(true, Ordering::Relaxed);
                }
                filter.scan_estimate.fetch_max(table.row_count() as u64, Ordering::Relaxed);
            }
            if !filters.iter().any(|f| filter.same(f, &pushed)) {
                filters.push(pushed);
            }
            true
        }
        // `FindScanFilterTargets` of a `read_parquet` scan: a column of the files, before the fields pushed into it
        K::ParquetScan { scan, .. } => {
            let position = match &scan.output {
                Some(output) => match output.get(column) {
                    Some(position) => *position,
                    None => return false,
                },
                None => column,
            };
            let (Some(ty), Some(name)) = (scan.types.get(position), scan.names.get(position)) else {
                return false;
            };
            let Some(pushed) = filter.on_column(position, ty, name, casts) else {
                return false;
            };
            if !scan.filters.iter().any(|f| filter.same(f, &pushed)) {
                std::sync::Arc::make_mut(scan).filters.push(pushed);
            }
            true
        }
        // a SECURE VIEW takes a join filter as it takes a pushed WHERE filter: it cannot fail
        K::Filter { .. } | K::Order { .. } | K::Distinct { on: false, .. } | K::SecureView { .. } => {
            push_into(&mut op.children[0], column, casts, filter)
        }
        K::ColumnSelect { keep } => {
            let Some(kept) = keep.get(column).copied() else {
                return false;
            };
            match op.children[0].column_bindings().iter().position(|binding| *binding == kept) {
                Some(child_column) => push_into(&mut op.children[0], child_column, casts, filter),
                None => false,
            }
        }
        // `PushdownProjectionColumns`: through integer casts too
        K::Projection { expressions, .. } => match expressions.get(column).and_then(peel_casts) {
            Some((child_column, mut inner)) => {
                inner.extend_from_slice(casts);
                push_into(&mut op.children[0], child_column, &inner, filter)
            }
            None => false,
        },
        // `PushdownAggregateColumns`: a group of every grouping set
        K::Aggregate {
            groups, grouping_sets, ..
        } => {
            if !grouping_sets.iter().all(|set| set.contains(&column)) {
                return false;
            }
            match groups.get(column).and_then(peel_casts) {
                Some((child_column, mut inner)) => {
                    inner.extend_from_slice(casts);
                    push_into(&mut op.children[0], child_column, &inner, filter)
                }
                None => false,
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
            shared && push_into(&mut op.children[0], column, casts, filter)
        }
        // `FindSetOperationFilterTargets`: EXCEPT keeps the rows of its first child
        K::Union { .. } | K::ExceptIntersect { except: false, .. } => {
            let mut any = false;
            for child in &mut op.children {
                any |= push_into(child, column, casts, filter);
            }
            any
        }
        K::ExceptIntersect { except: true, .. } => push_into(&mut op.children[0], column, casts, filter),
        // the columns of its input, not those it unnests
        K::Unnest { .. } => column < left_columns && push_into(&mut op.children[0], column, casts, filter),
        K::MaterializedCte { .. } => can_skip(&mut op.children[0]) && push_into(&mut op.children[1], column, casts, filter),
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
                any |= push_into(&mut op.children[side], at, casts, filter);
            }
            any
        }
        // `FilterTargetPreservesLeftInput`
        K::Join {
            join_type: JoinType::Left | JoinType::Semi | JoinType::Anti | JoinType::Single,
            ..
        }
        | K::MarkJoin { .. } => column < left_columns && push_into(&mut op.children[0], column, casts, filter),
        _ => false,
    }
}
