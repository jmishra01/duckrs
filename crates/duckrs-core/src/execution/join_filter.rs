// Ported in spirit from duckdb/src/optimizer/join_filter_pushdown_optimizer.cpp and the min/max and
// Bloom filters of duckdb/src/execution/operator/join/physical_hash_join.cpp @ rust-port-baseline
//
//! Join filters: a hash join whose probe rows without a match go nowhere tells a scan below its
//! probe side which keys its build side has, as a range and a Bloom filter, so that the scan drops
//! the other rows before it reads the rest of their columns, and before they reach the join.

use crate::planner::expression::{BoundExpr, BoundExprKind};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::vector::Vector;
use duckrs_parser::ast::JoinType;
use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A build side with more keys than this is not worth a filter: it would take long to fill and be
/// too large to stay in a cache.
const MAX_KEYS: usize = 1 << 24;

/// A table with fewer rows than this is read too fast for a filter to pay off.
const MIN_TABLE_ROWS: usize = 1 << 16;

/// Bits of the Bloom filter per key.
const BITS_PER_KEY: usize = 16;

/// A filter checks this many rows before it can find that it keeps too many.
const ROWS_TO_JUDGE: u64 = 1 << 20;

/// What a join filter knows of the build keys of its join.
pub struct JoinFilter {
    /// `IS NOT DISTINCT FROM`: a NULL can match
    null_safe: bool,
    /// the keys once the build side is done, or `None` if they are not something a filter can use
    keys: OnceLock<Option<Keys>>,
    checked: AtomicU64,
    passed: AtomicU64,
    /// whether the filter gave up: it let most rows through, or its join was built twice
    off: AtomicBool,
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
    pub fn new(null_safe: bool) -> Self {
        Self {
            null_safe,
            keys: OnceLock::new(),
            checked: AtomicU64::new(0),
            passed: AtomicU64::new(0),
            off: AtomicBool::new(false),
        }
    }

    /// Takes the keys of the whole build side, `count` rows of `keys`; `complete` is false when some
    /// of the build side is not there (it went to a file), and then the filter lets everything pass.
    pub fn set(&self, keys: &Vector, count: usize, complete: bool) {
        let usable = complete && count <= MAX_KEYS && supported(keys.logical_type());
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
        if self.keys.set(value).is_err() {
            // a join that runs twice would give a scan the keys of the time before
            self.off.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the filter has the keys of its join and still drops rows with them.
    pub fn is_active(&self) -> bool {
        !self.off.load(Ordering::Relaxed) && self.keys.get().is_some_and(Option::is_some)
    }

    /// The rows of `count` of `values` that can match a build key, or `None` if all of them can.
    pub fn select(&self, values: &Vector, count: usize) -> Option<Vec<u32>> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let keys = self.keys.get()?.as_ref()?;
        let null_passes = self.null_safe && keys.has_null;
        let mut kept = Vec::with_capacity(count);
        for_each_value(values, count, |row, value| {
            let pass = match value {
                Some(value) => keys.may_contain(value),
                None => null_passes,
            };
            if pass {
                kept.push(row as u32);
            }
        });
        let checked = self.checked.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
        let passed = self.passed.fetch_add(kept.len() as u64, Ordering::Relaxed) + kept.len() as u64;
        if checked >= ROWS_TO_JUDGE && passed * 10 > checked * 8 {
            self.off.store(true, Ordering::Relaxed);
        }
        Some(kept)
    }
}

/// `JoinFilterPushdownOptimizer`: gives each hash join that drops the probe rows without a match a
/// filter for each equality on a column of a scan below its probe side, in that scan.
pub fn push_join_filters(op: &mut LogicalOperator) {
    if matches!(op.kind, LogicalOperatorKind::RecursiveCte { .. }) {
        // its operators run again for each round
        return;
    }
    for child in &mut op.children {
        push_join_filters(child);
    }
    let LogicalOperatorKind::Join {
        join_type: JoinType::Inner | JoinType::Semi | JoinType::RightSemi | JoinType::RightAnti | JoinType::Right,
        conditions,
        ..
    } = &mut op.kind
    else {
        return;
    };
    for condition in conditions.iter_mut() {
        let BoundExprKind::Reference(column) = condition.left.kind else {
            continue;
        };
        // the range joins of C++ push min and max filters of their keys, which duckrs does not
        if condition.filter.is_some()
            || !condition.is_equality()
            || !supported(&condition.left.return_type)
            || condition.left.return_type != condition.right.return_type
        {
            continue;
        }
        let filter = std::sync::Arc::new(JoinFilter::new(condition.null_safe()));
        if push_into(&mut op.children[0], column, &condition.left, &filter) {
            condition.filter = Some(filter);
        }
    }
}

/// Puts `filter` on output column `column` of `op` into the scan it comes from, if the way there
/// keeps the values of the column as they are, and drops nothing that a row of the scan would make.
fn push_into(op: &mut LogicalOperator, column: usize, key: &BoundExpr, filter: &std::sync::Arc<JoinFilter>) -> bool {
    let left_columns = || op.children.first().map_or(0, |c| c.types().len());
    match &mut op.kind {
        LogicalOperatorKind::Get {
            table,
            column_ids,
            filters,
            projection,
            index_rows: None,
            ..
        } => {
            let position = match projection {
                Some(projection) => projection[column],
                None => column,
            };
            if position >= column_ids.len() || table.row_count() < MIN_TABLE_ROWS {
                return false;
            }
            let child = BoundExpr::new(BoundExprKind::Reference(position), key.return_type.clone())
                .with_alias(key.alias.clone());
            let expr = BoundExpr::new(
                BoundExprKind::JoinFilter {
                    child: Box::new(child),
                    filter: filter.clone(),
                },
                LogicalType::BOOLEAN,
            );
            filters.push(expr);
            true
        }
        LogicalOperatorKind::Filter { expressions } => {
            !expressions.iter().any(BoundExpr::is_volatile) && push_into(&mut op.children[0], column, key, filter)
        }
        LogicalOperatorKind::ColumnSelect { keep } => {
            let Some(kept) = keep.get(column) else {
                return false;
            };
            match op.children[0]
                .column_bindings()
                .iter()
                .position(|binding| binding == kept)
            {
                Some(child_column) => push_into(&mut op.children[0], child_column, key, filter),
                None => false,
            }
        }
        LogicalOperatorKind::Projection { expressions, .. } => match expressions.get(column).map(|e| &e.kind) {
            Some(BoundExprKind::Reference(child_column)) => {
                let child_column = *child_column;
                push_into(&mut op.children[0], child_column, key, filter)
            }
            _ => false,
        },
        LogicalOperatorKind::CrossProduct
        | LogicalOperatorKind::Join {
            join_type: JoinType::Inner,
            ..
        } => {
            let left = left_columns();
            if column < left {
                push_into(&mut op.children[0], column, key, filter)
            } else {
                push_into(&mut op.children[1], column - left, key, filter)
            }
        }
        LogicalOperatorKind::Join {
            join_type: JoinType::Left,
            ..
        } => column < left_columns() && push_into(&mut op.children[0], column, key, filter),
        LogicalOperatorKind::Join {
            join_type: JoinType::Semi | JoinType::Anti,
            ..
        } => push_into(&mut op.children[0], column, key, filter),
        LogicalOperatorKind::Join {
            join_type: JoinType::Right,
            ..
        } => {
            let left = left_columns();
            column >= left && push_into(&mut op.children[1], column - left, key, filter)
        }
        LogicalOperatorKind::Join {
            join_type: JoinType::RightSemi | JoinType::RightAnti,
            ..
        } => push_into(&mut op.children[1], column, key, filter),
        LogicalOperatorKind::MarkJoin { .. } => {
            column < left_columns() && push_into(&mut op.children[0], column, key, filter)
        }
        _ => false,
    }
}
