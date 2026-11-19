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
//! Ported so far: integral columns of ORDER BY and of the groups of an aggregate. Not yet: strings,
//! DISTINCT, the build side of joins, geometry and variant columns.

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
        self.create_projections(op, &can)
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
        self.create_projections(op, &can)
    }

    /// `CreateProjections`: a compressing projection below `op` and a restoring one above it, if
    /// a column compresses.
    fn create_projections(&mut self, op: &mut LogicalOperator, can: &[bool]) -> HashMap<ColumnBinding, ColumnBinding> {
        let child = &op.children[0];
        let (bindings, types) = (child.column_bindings(), child.types());
        let mut compressed: Vec<Option<(Compression, ColumnStatistics)>> = Vec::new();
        let mut expressions = Vec::new();
        for (i, (&binding, ty)) in bindings.iter().zip(&types).enumerate() {
            let input = BoundExpr::column_ref(binding, ty.clone(), "");
            let stats = can[i].then(|| self.statistics.get(binding));
            match stats.as_ref().and_then(|s| integral_compress(input.clone(), s)) {
                Some(c) => {
                    compressed.push(Some((c.how, stats.expect("some"))));
                    expressions.push(c.expression);
                }
                None => {
                    compressed.push(None);
                    expressions.push(input);
                }
            }
        }
        if compressed.iter().all(Option::is_none) {
            return HashMap::new();
        }
        // what the operator outputs, which what is above reads
        let outputs_before = op.column_bindings();
        // below: the child's columns, compressed
        let compress_index = self.take();
        let compressed_types: Vec<LogicalType> = expressions.iter().map(|e| e.return_type.clone()).collect();
        let child = op.children.pop().expect("one child");
        op.children.push(LogicalOperator::new(
            K::Projection {
                table_index: compress_index,
                expressions,
            },
            vec![child],
        ));
        let below: HashMap<ColumnBinding, (ColumnBinding, LogicalType)> = bindings
            .iter()
            .enumerate()
            .map(|(i, b)| (*b, (ColumnBinding::new(compress_index, i), compressed_types[i].clone())))
            .collect();
        // the operator reads the compressed columns, with their types
        replace(&mut op.kind, &below, true);
        // which of its outputs are compressed columns: an ORDER BY's are its input's, an
        // aggregate's groups the columns they read
        let outputs = op.column_bindings();
        let output_types = op.types();
        let mut restore_of: HashMap<ColumnBinding, (Compression, LogicalType)> = HashMap::new();
        match &op.kind {
            K::Order { .. } => {
                for (i, c) in compressed.iter().enumerate() {
                    if let Some((how, _)) = c {
                        restore_of.insert(ColumnBinding::new(compress_index, i), (how.clone(), types[i].clone()));
                    }
                }
            }
            K::Aggregate {
                group_index, groups, ..
            } => {
                for (g, group) in groups.iter().enumerate() {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = group.kind
                        && binding.table_index == compress_index
                        && let Some((how, _)) = &compressed[binding.column_index]
                    {
                        restore_of.insert(
                            ColumnBinding::new(*group_index, g),
                            (how.clone(), types[binding.column_index].clone()),
                        );
                    }
                }
            }
            _ => unreachable!(),
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

/// `CreateRestoreExpression`.
fn restore(input: BoundExpr, how: &Compression, original: &LogicalType) -> BoundExpr {
    match how {
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
