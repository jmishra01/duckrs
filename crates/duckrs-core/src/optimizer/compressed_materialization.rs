// Ported from duckdb/src/optimizer/compressed_materialization.cpp,
// duckdb/src/optimizer/compressed_materialization/compress_order.cpp,
// duckdb/src/optimizer/compressed_materialization/compress_aggregate.cpp and
// duckdb/src/function/scalar/compressed_materialization/compress_integral.cpp @ rust-port-baseline
//
//! `CompressedMaterialization` (`COMPRESSED_MATERIALIZATION`): an operator that materializes its
//! input (an ORDER BY, the groups of an aggregate) does so on narrower values where the statistics
//! say they fit. A projection below it compresses, one above it restores: an INTEGER of 300..309 is
//! `__internal_compress_integral_utinyint(i, 300)` below and
//! `__internal_decompress_integral_integer(c, 300)` above; one of 1..100 is `CAST(i AS TINYINT)`.
//!
//! C++ runs this inside statistics propagation, operator by operator from the leaves up. Here it
//! runs after it, on the statistics it found, at the same point of the optimizer, and rebinds what
//! is above each operator it compressed as `RebindAncestors` does.
//!
//! A VARCHAR shorter than an integral type is that integer, its bytes from the most significant one
//! down and its length in the least significant one: `__internal_compress_string_uinteger(s)`,
//! and `__internal_decompress_string(c)` above.
//!
//! Ported: integral and VARCHAR columns of ORDER BY, DISTINCT, the groups of an aggregate and the
//! build side and keys of a comparison join. Not yet: geometry and variant columns.

use super::common_subplan::{max_table_index, visit_bindings};
use super::statistics::StatisticsMap;
use crate::catalog::ColumnStatistics;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind as K};
use duckrs_common::Result;
use duckrs_common::executor;
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use std::collections::{HashMap, HashSet};

/// `CompressedMaterialization::Compress` over the plan.
pub fn compress(plan: &mut LogicalOperator, statistics: &mut StatisticsMap) {
    let mut next = max_table_index(plan) + 1;
    let mut pass = Pass {
        statistics,
        next: &mut next,
    };
    pass.visit(plan);
}

struct Pass<'a> {
    statistics: &'a mut StatisticsMap,
    next: &'a mut usize,
}

/// How a column was compressed (`CompressedMaterializationType`).
#[derive(Clone)]
enum Compression {
    /// a value-preserving cast to a narrower integral type
    Cast,
    /// `__internal_compress_integral_*` of the offset from `min`
    Offset(Value),
    /// `__internal_compress_string_*`
    String,
}

/// `CompressExpression`.
struct Compressed {
    expression: BoundExpr,
    how: Compression,
}

fn replace(kind: &mut K, map: &HashMap<ColumnBinding, (ColumnBinding, LogicalType)>, set_type: bool) {
    // the bindings, and with them the types where the compressed operator reads them
    let walk = |expr: &mut BoundExpr| {
        expr.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                && let Some((to, ty)) = map.get(binding)
            {
                *binding = *to;
                if set_type {
                    e.return_type = ty.clone();
                }
            }
        });
    };
    let mut probe = LogicalOperator::new(std::mem::replace(kind, K::CrossProduct), Vec::new());
    for expr in probe.expressions_mut() {
        walk(expr);
    }
    *kind = probe.kind;
    visit_bindings(kind, &mut |binding, depth| {
        if depth == 0
            && let Some((to, _)) = map.get(binding)
        {
            *binding = *to;
        }
    });
}

fn collect(expr: &BoundExpr, out: &mut HashSet<ColumnBinding>) {
    expr.clone().walk_mut(&mut |e| {
        if let BoundExprKind::ColumnRef { binding, depth: 0 } = &e.kind {
            out.insert(*binding);
        }
    });
}

impl Pass<'_> {
    fn take(&mut self) -> usize {
        let index = *self.next;
        *self.next += 1;
        index
    }

    /// The operators from the leaves up; gives the bindings that changed, which the operators
    /// above read from now on.
    fn visit(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let mut changed = HashMap::new();
        for child in &mut op.children {
            changed.extend(self.visit(child));
        }
        if !changed.is_empty() {
            let typed = changed.iter().map(|(k, v)| (*k, (*v, LogicalType::SQLNULL))).collect();
            replace(&mut op.kind, &typed, false);
        }
        let new = match op.kind {
            K::Order { .. } => self.compress_order(op),
            K::Aggregate { .. } => self.compress_aggregate(op),
            K::Distinct { .. } => self.compress_distinct(op),
            // a MARK join's right side keeps its bindings while it changes type
            K::Join { .. } => self.compress_comparison_join(op),
            _ => HashMap::new(),
        };
        changed.extend(new);
        changed
    }

    /// `CompressOrder`: every column passes through the sort, so every one it does not order by an
    /// expression can be compressed.
    fn compress_order(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let K::Order { orders } = &op.kind else {
            unreachable!()
        };
        let mut referenced = HashSet::new();
        for order in orders {
            if !matches!(order.expr.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
                collect(&order.expr, &mut referenced);
            }
        }
        let child = &op.children[0];
        let can: Vec<bool> = child.column_bindings().iter().map(|b| !referenced.contains(b)).collect();
        self.create_projections(op, &[can], &HashMap::new())
    }

    /// `CompressAggregate`: the columns that are only groups.
    fn compress_aggregate(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let K::Aggregate {
            groups,
            aggregates,
            grouping_sets,
            grouping_functions,
            ..
        } = &op.kind
        else {
            unreachable!()
        };
        if groups.is_empty() || grouping_sets.len() > 1 || !grouping_functions.is_empty() {
            return HashMap::new();
        }
        let mut group_bindings = HashSet::new();
        let mut referenced = HashSet::new();
        for group in groups {
            match group.kind {
                BoundExprKind::ColumnRef { binding, depth: 0 } => {
                    // a duplicate group is not compressed
                    if !group_bindings.insert(binding) {
                        return HashMap::new();
                    }
                }
                _ => collect(group, &mut referenced),
            }
        }
        // what the aggregates read stays
        for aggregate in aggregates {
            for child in aggregate.children.iter().chain(&aggregate.filter).chain(&aggregate.distinct_key) {
                collect(child, &mut referenced);
            }
            for (order, _) in &aggregate.order_by {
                if !matches!(order.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
                    collect(order, &mut referenced);
                }
            }
        }
        // the columns the aggregate does not read cannot be read above it
        let can: Vec<bool> = op.children[0]
            .column_bindings()
            .iter()
            .map(|b| group_bindings.contains(b) && !referenced.contains(b))
            .collect();
        self.create_projections(op, &[can], &HashMap::new())
    }

    /// `CreateProjections`: a compressing projection below each child of `op` that has a column to
    /// compress, and a restoring one above it. `can[c][i]`: column `i` of child `c` may be
    /// compressed; `overrides` are the statistics to compress a column by in place of its own.
    fn create_projections(
        &mut self,
        op: &mut LogicalOperator,
        can: &[Vec<bool>],
        overrides: &HashMap<ColumnBinding, ColumnStatistics>,
    ) -> HashMap<ColumnBinding, ColumnBinding> {
        // per child: its compressing projection's index, and how each column is compressed
        let mut children: Vec<Option<(usize, Vec<Option<Compression>>, Vec<LogicalType>)>> = Vec::new();
        let outputs_before = op.column_bindings();
        let mut below: HashMap<ColumnBinding, (ColumnBinding, LogicalType)> = HashMap::new();
        for (c, child_can) in can.iter().enumerate() {
            let child = &op.children[c];
            let (bindings, types) = (child.column_bindings(), child.types());
            let mut how = Vec::new();
            let mut expressions = Vec::new();
            for (i, (&binding, ty)) in bindings.iter().zip(&types).enumerate() {
                let input = BoundExpr::column_ref(binding, ty.clone(), "");
                let stats = child_can[i]
                    .then(|| overrides.get(&binding).cloned().unwrap_or_else(|| self.statistics.get(binding)));
                let compress = |s: &ColumnStatistics| {
                    integral_compress(input.clone(), s).or_else(|| string_compress(input.clone(), s))
                };
                match stats.as_ref().and_then(compress) {
                    Some(compressed) => {
                        how.push(Some(compressed.how));
                        expressions.push(compressed.expression);
                    }
                    None => {
                        how.push(None);
                        expressions.push(input);
                    }
                }
            }
            if how.iter().all(Option::is_none) {
                children.push(None);
                continue;
            }
            let index = self.take();
            for (i, (b, e)) in bindings.iter().zip(&expressions).enumerate() {
                below.insert(*b, (ColumnBinding::new(index, i), e.return_type.clone()));
            }
            let input = std::mem::replace(&mut op.children[c], LogicalOperator::new(K::CrossProduct, Vec::new()));
            op.children[c] = LogicalOperator::new(
                K::Projection {
                    table_index: index,
                    expressions,
                },
                vec![input],
            );
            children.push(Some((index, how, types)));
        }
        if children.iter().all(Option::is_none) {
            return HashMap::new();
        }
        // the operator reads the compressed columns, with their types
        replace(&mut op.kind, &below, true);
        // which of its outputs are compressed columns: those of an ORDER BY, a DISTINCT and a join
        // are their inputs', an aggregate's groups the columns they read
        let outputs = op.column_bindings();
        let output_types = op.types();
        let mut restore_of: HashMap<ColumnBinding, (Compression, LogicalType)> = HashMap::new();
        let compressed_column = |binding: ColumnBinding| {
            children.iter().flatten().find(|(index, ..)| *index == binding.table_index).and_then(
                |(_, how, types)| {
                    how[binding.column_index]
                        .clone()
                        .map(|h| (h, types[binding.column_index].clone()))
                },
            )
        };
        match &op.kind {
            K::Aggregate {
                group_index, groups, ..
            } => {
                for (g, group) in groups.iter().enumerate() {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = group.kind
                        && let Some(restore) = compressed_column(binding)
                    {
                        restore_of.insert(ColumnBinding::new(*group_index, g), restore);
                    }
                }
            }
            _ => {
                for binding in &outputs {
                    if let Some(restore) = compressed_column(*binding) {
                        restore_of.insert(*binding, restore);
                    }
                }
            }
        }
        // above: the operator's columns as they were
        let decompress_index = self.take();
        let restored: Vec<BoundExpr> = outputs
            .iter()
            .zip(&output_types)
            .map(|(binding, ty)| {
                let input = BoundExpr::column_ref(*binding, ty.clone(), "");
                match restore_of.get(binding) {
                    Some((how, original)) => restore(input, how, original),
                    None => input,
                }
            })
            .collect();
        let materializing = std::mem::replace(op, LogicalOperator::new(K::CrossProduct, Vec::new()));
        *op = LogicalOperator::new(
            K::Projection {
                table_index: decompress_index,
                expressions: restored,
            },
            vec![materializing],
        );
        // what is above reads the restored columns, which have the statistics the columns had
        let mut changed = HashMap::new();
        for (i, before) in outputs_before.iter().enumerate() {
            let after = ColumnBinding::new(decompress_index, i);
            self.statistics.copy(*before, after);
            changed.insert(*before, after);
        }
        changed
    }

    /// `CompressDistinct`: every column passes through, so every one it does not deduplicate on
    /// by an expression can be compressed.
    fn compress_distinct(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let K::Distinct { targets, .. } = &op.kind else {
            unreachable!()
        };
        let mut referenced = HashSet::new();
        for target in targets {
            if !matches!(target.kind, BoundExprKind::ColumnRef { depth: 0, .. }) {
                collect(target, &mut referenced);
            }
        }
        let can = vec![op.children[0].column_bindings().iter().map(|b| !referenced.contains(b)).collect()];
        self.create_projections(op, &can, &HashMap::new())
    }

    /// `CompressComparisonJoin`: the columns of the build side that only pass through, and the keys
    /// of a join on one pair of columns, both compressed by their merged statistics so that they
    /// still compare. The probe side streams, so its other columns stay.
    fn compress_comparison_join(&mut self, op: &mut LogicalOperator) -> HashMap<ColumnBinding, ColumnBinding> {
        let K::Join {
            conditions, predicate, ..
        } = &op.kind
        else {
            unreachable!()
        };
        // release builds only compress large build sides that the join does not multiply much
        let build = super::join_order::estimated_cardinality(&op.children[1]);
        if build < JOIN_BUILD_CARDINALITY_THRESHOLD {
            return HashMap::new();
        }
        if op.children[1].types().len() < JOIN_BUILD_COLUMN_COUNT_THRESHOLD {
            let joined = super::join_order::estimated_cardinality(op);
            if joined as f64 / build as f64 > JOIN_CARDINALITY_RATIO_THRESHOLD {
                return HashMap::new();
            }
        }
        let mut referenced = HashSet::new();
        if let Some(predicate) = predicate {
            collect(predicate, &mut referenced);
        }
        let mut probe_keys = HashSet::new();
        let mut overrides = HashMap::new();
        for condition in conditions {
            if conditions.len() == 1
                && let (
                    BoundExprKind::ColumnRef { binding: l, depth: 0 },
                    BoundExprKind::ColumnRef { binding: r, depth: 0 },
                ) = (&condition.left.kind, &condition.right.kind)
                && !referenced.contains(l)
                && !referenced.contains(r)
            {
                // both by the same statistics, or the comparisons fail
                let (left, right) = (self.statistics.get(*l), self.statistics.get(*r));
                let merged = merge(&left, &right);
                let compresses = integral_compress(condition.left.clone(), &merged)
                    .or_else(|| string_compress(condition.left.clone(), &merged))
                    .is_some();
                if compresses && condition.left.return_type == condition.right.return_type {
                    overrides.insert(*l, merged.clone());
                    overrides.insert(*r, merged);
                    probe_keys.insert(*l);
                    continue;
                }
            }
            collect(&condition.left, &mut referenced);
            collect(&condition.right, &mut referenced);
        }
        let probe: Vec<bool> = op.children[0].column_bindings().iter().map(|b| probe_keys.contains(b)).collect();
        let build: Vec<bool> = op.children[1].column_bindings().iter().map(|b| !referenced.contains(b)).collect();
        self.create_projections(op, &[probe, build], &overrides)
    }
}

/// `JOIN_BUILD_CARDINALITY_THRESHOLD`, `JOIN_BUILD_COLUMN_COUNT_THRESHOLD` and
/// `JOIN_CARDINALITY_RATIO_THRESHOLD` (release builds).
const JOIN_BUILD_CARDINALITY_THRESHOLD: u64 = 1_048_576;
const JOIN_BUILD_COLUMN_COUNT_THRESHOLD: usize = 20;
const JOIN_CARDINALITY_RATIO_THRESHOLD: f64 = 8.0;

/// `BaseStatistics::Merge`: what holds of the values of both.
fn merge(a: &ColumnStatistics, b: &ColumnStatistics) -> ColumnStatistics {
    use crate::optimizer::statistics::compare_values;
    use duckrs_parser::ast::ComparisonOp;
    let pick = |x: &Option<Value>, y: &Option<Value>, op: ComparisonOp, valid_x: bool, valid_y: bool| match (x, y) {
        // a side without values does not bound the other
        _ if !valid_x => y.clone(),
        _ if !valid_y => x.clone(),
        (Some(x), Some(y)) => Some(if compare_values(op, x, y) == Some(true) { x.clone() } else { y.clone() }),
        _ => None,
    };
    let mut merged = a.clone();
    merged.min = pick(&a.min, &b.min, ComparisonOp::LessThanOrEqual, a.can_be_valid, b.can_be_valid);
    merged.max = pick(&a.max, &b.max, ComparisonOp::GreaterThanOrEqual, a.can_be_valid, b.can_be_valid);
    merged.can_be_null = a.can_be_null || b.can_be_null;
    merged.can_be_valid = a.can_be_valid || b.can_be_valid;
    merged.strings = match (&a.strings, &b.strings) {
        (Some(x), Some(y)) => {
            let mut s = x.clone();
            s.max_length = x.max_length.zip(y.max_length).map(|(p, q)| p.max(q));
            s.bounds = None;
            Some(s)
        }
        _ => None,
    };
    merged
}

/// `GetSameWidthIntegralType`.
fn same_width(size: usize, signed: bool) -> Option<LogicalType> {
    Some(match (size, signed) {
        (1, true) => LogicalType::TINYINT,
        (1, false) => LogicalType::UTINYINT,
        (2, true) => LogicalType::SMALLINT,
        (2, false) => LogicalType::USMALLINT,
        (4, true) => LogicalType::INTEGER,
        (4, false) => LogicalType::UINTEGER,
        (8, true) => LogicalType::BIGINT,
        (8, false) => LogicalType::UBIGINT,
        _ => return None,
    })
}

fn is_signed(ty: &LogicalType) -> bool {
    matches!(
        ty.id(),
        LogicalTypeId::TinyInt | LogicalTypeId::SmallInt | LogicalTypeId::Integer | LogicalTypeId::BigInt | LogicalTypeId::HugeInt
    )
}

/// The range of an integral type, as `i128` (`UHUGEINT` beyond it has none).
fn bounds(ty: &LogicalType) -> Option<(i128, i128)> {
    Some(match ty.id() {
        LogicalTypeId::TinyInt => (i8::MIN.into(), i8::MAX.into()),
        LogicalTypeId::SmallInt => (i16::MIN.into(), i16::MAX.into()),
        LogicalTypeId::Integer => (i32::MIN.into(), i32::MAX.into()),
        LogicalTypeId::BigInt => (i64::MIN.into(), i64::MAX.into()),
        LogicalTypeId::UTinyInt => (0, u8::MAX.into()),
        LogicalTypeId::USmallInt => (0, u16::MAX.into()),
        LogicalTypeId::UInteger => (0, u32::MAX.into()),
        LogicalTypeId::UBigInt => (0, u64::MAX.into()),
        _ => return None,
    })
}

/// A value of an integral type.
fn integral_value(ty: &LogicalType, v: i128) -> Value {
    match ty.id() {
        LogicalTypeId::TinyInt => Value::tinyint(v as i8),
        LogicalTypeId::SmallInt => Value::smallint(v as i16),
        LogicalTypeId::Integer => Value::integer(v as i32),
        LogicalTypeId::BigInt => Value::bigint(v as i64),
        LogicalTypeId::HugeInt => Value::hugeint(v),
        LogicalTypeId::UTinyInt => Value::utinyint(v as u8),
        LogicalTypeId::USmallInt => Value::usmallint(v as u16),
        LogicalTypeId::UInteger => Value::uinteger(v as u32),
        LogicalTypeId::UBigInt => Value::ubigint(v as u64),
        _ => Value::uhugeint(v as u128),
    }
}

/// `GetIntegralCompress`: the narrowest type the range of the values fits, as a cast where the
/// values themselves fit it (`GetIntegralCastType`), as their offset from the minimum otherwise.
fn integral_compress(input: BoundExpr, stats: &ColumnStatistics) -> Option<Compressed> {
    let ty = input.return_type.clone();
    if !ty.is_integral() {
        return None;
    }
    let size = ty.physical_type().size();
    if size == 1 {
        return None;
    }
    // `GetIntegralOffsetCompressInfo`
    let all_null = !stats.can_be_valid;
    let (min, max) = if all_null {
        (0, 0)
    } else {
        (stats.min.as_ref()?.as_i128()?, stats.max.as_ref()?.as_i128()?)
    };
    let range = u64::try_from(max.checked_sub(min)?).ok()?;
    let offset_size = match range {
        r if r <= u8::MAX.into() => 1,
        r if r <= u16::MAX.into() => 2,
        r if r <= u32::MAX.into() => 4,
        _ => 8,
    };
    if offset_size >= size {
        return None;
    }
    let fits = |target: &LogicalType| {
        all_null || bounds(target).is_some_and(|(lo, hi)| lo <= min && max <= hi)
    };
    let preferred = same_width(offset_size, is_signed(&ty))?;
    let alternate = same_width(offset_size, !is_signed(&ty))?;
    for target in [preferred, alternate] {
        if fits(&target) {
            return Some(Compressed {
                expression: input.cast_to(&target, false),
                how: Compression::Cast,
            });
        }
    }
    let target = same_width(offset_size, false)?;
    let min = if all_null { Value::null(ty.clone()) } else { integral_value(&ty, min) };
    let callback = compress_callback(&ty, &target)?;
    let name = format!("__internal_compress_integral_{}", type_name(&target));
    let expression = crate::function::nested::bound_function(
        &name,
        vec![input, BoundExpr::constant(min.clone())],
        target,
        callback,
    );
    Some(Compressed {
        expression,
        how: Compression::Offset(min),
    })
}

/// `GetStringCompress`: the smallest integral type longer than the longest string
/// (`GetStringCompressInfo`); with strings of at most one byte below 255, a `UTINYINT` of the length
/// plus that byte (`CreateStringCompressStats`, `MiniStringCompress`).
fn string_compress(input: BoundExpr, stats: &ColumnStatistics) -> Option<Compressed> {
    if input.return_type.id() != LogicalTypeId::Varchar || input.return_type.collation().is_some() {
        return None;
    }
    let mut target = if !stats.can_be_valid {
        LogicalType::UTINYINT
    } else {
        let max_length = stats.strings.as_ref()?.max_length?;
        [
            LogicalType::UTINYINT,
            LogicalType::USMALLINT,
            LogicalType::UINTEGER,
            LogicalType::UBIGINT,
            LogicalType::UHUGEINT,
        ]
        .into_iter()
        .find(|t| (max_length as usize) < t.physical_type().size())?
    };
    if target.id() == LogicalTypeId::USmallInt
        && let Some((_, max)) = stats.strings.as_ref().and_then(|s| s.bounds.as_ref())
    {
        let max_byte = max.first().copied().unwrap_or(0);
        if max_byte < u8::MAX {
            target = LogicalType::UTINYINT;
        }
    }
    let callback: Callback = match target.id() {
        LogicalTypeId::UTinyInt => compress_mini_string,
        LogicalTypeId::USmallInt => compress_string::<u16>,
        LogicalTypeId::UInteger => compress_string::<u32>,
        LogicalTypeId::UBigInt => compress_string::<u64>,
        _ => compress_string::<u128>,
    };
    let name = format!("__internal_compress_string_{}", type_name(&target));
    Some(Compressed {
        expression: crate::function::nested::bound_function(&name, vec![input], target, callback),
        how: Compression::String,
    })
}

/// An unsigned integer a string compresses into.
trait Packed: NativeType {
    const SIZE: usize;
    fn from_u128(v: u128) -> Self;
    fn to_u128(self) -> u128;
}

macro_rules! packed {
    ($($t:ty),*) => {$(
        impl Packed for $t {
            const SIZE: usize = size_of::<$t>();
            fn from_u128(v: u128) -> Self {
                v as $t
            }
            fn to_u128(self) -> u128 {
                self as u128
            }
        }
    )*};
}
packed!(u8, u16, u32, u64, u128);

/// `StringCompress`: the bytes from the most significant byte down, the length in the lowest.
fn compress_string<R: Packed>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::unary_str::<R>(args.column(0), result, args.len(), |bytes| {
        let mut value: u128 = 0;
        for i in 0..R::SIZE - 1 {
            value = (value << 8) | u128::from(bytes.get(i).copied().unwrap_or(0));
        }
        R::from_u128((value << 8) | bytes.len() as u128)
    });
    Ok(())
}

/// `MiniStringCompress`: the length plus the byte of a string of at most one.
fn compress_mini_string(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::unary_str::<u8>(args.column(0), result, args.len(), |bytes| {
        (bytes.len() as u8).wrapping_add(bytes.first().copied().unwrap_or(0))
    });
    Ok(())
}

/// `StringDecompress`.
fn decompress_string<C: Packed>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::unary_to_str::<C>(args.column(0), result, args.len(), |packed, out| {
        let value = packed.to_u128();
        if C::SIZE == 1 {
            // `MiniStringDecompress`
            if value != 0 {
                out.push((value - 1) as u8);
            }
            return;
        }
        let length = (value & 0xff) as usize;
        for i in 0..length {
            out.push((value >> (8 * (C::SIZE - 1 - i))) as u8);
        }
    });
    Ok(())
}

/// `CreateRestoreExpression`.
fn restore(input: BoundExpr, how: &Compression, original: &LogicalType) -> BoundExpr {
    match how {
        Compression::String => {
            let callback: Callback = match input.return_type.id() {
                LogicalTypeId::UTinyInt => decompress_string::<u8>,
                LogicalTypeId::USmallInt => decompress_string::<u16>,
                LogicalTypeId::UInteger => decompress_string::<u32>,
                LogicalTypeId::UBigInt => decompress_string::<u64>,
                _ => decompress_string::<u128>,
            };
            crate::function::nested::bound_function(
                "__internal_decompress_string",
                vec![input],
                original.clone(),
                callback,
            )
        }
        Compression::Cast => input.cast_to(original, false),
        Compression::Offset(min) => {
            let callback = decompress_callback(&input.return_type, original).expect("a compressed type");
            let name = format!("__internal_decompress_integral_{}", type_name(original));
            crate::function::nested::bound_function(
                &name,
                vec![input, BoundExpr::constant(min.clone())],
                original.clone(),
                callback,
            )
        }
    }
}

/// `StringUtil::Lower(LogicalTypeIdToString(...))`.
fn type_name(ty: &LogicalType) -> &'static str {
    match ty.id() {
        LogicalTypeId::SmallInt => "smallint",
        LogicalTypeId::Integer => "integer",
        LogicalTypeId::BigInt => "bigint",
        LogicalTypeId::HugeInt => "hugeint",
        LogicalTypeId::UTinyInt => "utinyint",
        LogicalTypeId::USmallInt => "usmallint",
        LogicalTypeId::UInteger => "uinteger",
        LogicalTypeId::UBigInt => "ubigint",
        LogicalTypeId::UHugeInt => "uhugeint",
        _ => "tinyint",
    }
}

/// An integer that the offsets compute in.
trait Integral: NativeType {
    fn to_i128(self) -> i128;
    fn from_i128(v: i128) -> Self;
}

macro_rules! integral {
    ($($t:ty),*) => {$(
        impl Integral for $t {
            fn to_i128(self) -> i128 {
                self as i128
            }
            fn from_i128(v: i128) -> Self {
                v as $t
            }
        }
    )*};
}
integral!(i16, i32, i64, i128, u8, u16, u32, u64, u128);

/// `IntegralCompressFunction`: `input - min`.
fn compress_values<I: Integral, R: Integral>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary::<I, I, R>(args.column(0), args.column(1), result, args.len(), |x, min| {
        R::from_i128(x.to_i128().wrapping_sub(min.to_i128()))
    });
    Ok(())
}

/// `IntegralDecompressFunction`: `min + input`.
fn decompress_values<C: Integral, R: Integral>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary::<C, R, R>(args.column(0), args.column(1), result, args.len(), |x, min| {
        R::from_i128(min.to_i128().wrapping_add(x.to_i128()))
    });
    Ok(())
}

type Callback = crate::function::ScalarCallback;

macro_rules! by_target {
    ($f:ident, $a:ty, $target:expr) => {
        match $target.id() {
            LogicalTypeId::UTinyInt => Some($f::<$a, u8> as Callback),
            LogicalTypeId::USmallInt => Some($f::<$a, u16> as Callback),
            LogicalTypeId::UInteger => Some($f::<$a, u32> as Callback),
            LogicalTypeId::UBigInt => Some($f::<$a, u64> as Callback),
            _ => None,
        }
    };
}

fn compress_callback(input: &LogicalType, target: &LogicalType) -> Option<Callback> {
    match input.id() {
        LogicalTypeId::SmallInt => by_target!(compress_values, i16, target),
        LogicalTypeId::Integer => by_target!(compress_values, i32, target),
        LogicalTypeId::BigInt => by_target!(compress_values, i64, target),
        LogicalTypeId::HugeInt => by_target!(compress_values, i128, target),
        LogicalTypeId::USmallInt => by_target!(compress_values, u16, target),
        LogicalTypeId::UInteger => by_target!(compress_values, u32, target),
        LogicalTypeId::UBigInt => by_target!(compress_values, u64, target),
        LogicalTypeId::UHugeInt => by_target!(compress_values, u128, target),
        _ => None,
    }
}

macro_rules! by_original {
    ($c:ty, $original:expr) => {
        match $original.id() {
            LogicalTypeId::SmallInt => Some(decompress_values::<$c, i16> as Callback),
            LogicalTypeId::Integer => Some(decompress_values::<$c, i32> as Callback),
            LogicalTypeId::BigInt => Some(decompress_values::<$c, i64> as Callback),
            LogicalTypeId::HugeInt => Some(decompress_values::<$c, i128> as Callback),
            LogicalTypeId::USmallInt => Some(decompress_values::<$c, u16> as Callback),
            LogicalTypeId::UInteger => Some(decompress_values::<$c, u32> as Callback),
            LogicalTypeId::UBigInt => Some(decompress_values::<$c, u64> as Callback),
            LogicalTypeId::UHugeInt => Some(decompress_values::<$c, u128> as Callback),
            _ => None,
        }
    };
}

fn decompress_callback(compressed: &LogicalType, original: &LogicalType) -> Option<Callback> {
    match compressed.id() {
        LogicalTypeId::UTinyInt => by_original!(u8, original),
        LogicalTypeId::USmallInt => by_original!(u16, original),
        LogicalTypeId::UInteger => by_original!(u32, original),
        LogicalTypeId::UBigInt => by_original!(u64, original),
        _ => None,
    }
}
