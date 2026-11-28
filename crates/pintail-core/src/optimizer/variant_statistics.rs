// Ported from duckdb/src/storage/statistics/variant_stats.cpp,
// duckdb/src/function/cast/cast_statistics.cpp (`StatisticsPropagateVariant`) and
// duckdb/src/function/scalar/variant/variant_extract.cpp (`VariantExtractPropagateStats`) @ rust-port-baseline
//
//! The statistics of VARIANT values: whether they are shredded, and those of the typed part, which is a
//! STRUCT(typed_value T, untyped_value_index UINTEGER) where not every value has a type `T` (see
//! `pintail_common::variant_shredding`).

use super::statistics::{stats_fields, unknown};
use crate::catalog::{ColumnStatistics as Stats, VariantStatistics};
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::Value;

/// Child `at` of `stats`, or unknown statistics if they are not known.
fn child(stats: &Stats, at: usize) -> Stats {
    stats.children.get(at).cloned().unwrap_or_else(unknown)
}

/// `GetTypedStats`: of a STRUCT(typed_value, ...) those of `typed_value`, else the level itself.
fn typed(ty: &LogicalType, stats: &Stats) -> (LogicalType, Stats) {
    if ty.id() != LogicalTypeId::Struct {
        return (ty.clone(), stats.clone());
    }
    (ty.struct_children()[0].1.clone(), child(stats, 0))
}

/// `VariantShreddedStats::IsFullyShredded`: whether every value of this level is in its typed part.
fn fully_shredded(ty: &LogicalType, stats: &Stats) -> bool {
    if !ty.is_nested() || ty.id() != LogicalTypeId::Struct || ty.struct_children().len() == 1 {
        return true;
    }
    let typed = child(stats, 0);
    let untyped = child(stats, 1);
    if !typed.can_be_null || !untyped.can_be_valid {
        return true;
    }
    // only missing fields of objects (0) are in the untyped part
    match (&untyped.min, &untyped.max) {
        (Some(min), Some(max)) => min == max && min.as_i128() == Some(0),
        _ => false,
    }
}

/// `GetShreddedStatsStruct`: the statistics of the levels that are (or are not) fully shredded.
fn shredded_struct(ty: &LogicalType, stats: &Stats, fully: bool) -> Option<Value> {
    if fully_shredded(ty, stats) != fully {
        return None;
    }
    let (typed_type, typed_stats) = typed(ty, stats);
    match typed_type.id() {
        LogicalTypeId::List => {
            let element = shredded_struct(typed_type.child_type()?, &child(&typed_stats, 0), fully)?;
            Some(Value::struct_value(vec![("child_stats".to_string(), element)]))
        }
        LogicalTypeId::Struct => {
            let mut fields: Vec<(usize, &(String, LogicalType))> =
                typed_type.struct_children().iter().enumerate().collect();
            fields.sort_by(|a, b| a.1.0.cmp(&b.1.0));
            let entries: Vec<(String, Value)> = fields
                .into_iter()
                .filter_map(|(at, (name, field))| {
                    shredded_struct(field, &child(&typed_stats, at), fully).map(|value| (name.clone(), value))
                })
                .collect();
            (!entries.is_empty()).then(|| Value::struct_value(entries))
        }
        _ => Some(Value::struct_value(vec![
            ("type".to_string(), Value::varchar(typed_type.to_string())),
            ("stats".to_string(), stats_fields(&typed_stats, &typed_type)?),
        ])),
    }
}

/// `VariantStats::ToStruct`: the shredding state, and of the typed part what is fully shredded and what not.
pub(super) fn variant_fields(stats: &Stats) -> Vec<(String, Value)> {
    let state = match stats.variant.as_deref() {
        Some(VariantStatistics::Uninitialized) => "UNINITIALIZED",
        Some(VariantStatistics::NotShredded) => "NOT_SHREDDED",
        Some(VariantStatistics::Shredded(..)) => "SHREDDED",
        _ => "INCONSISTENT",
    };
    let mut fields = vec![("shredding_state".to_string(), Value::varchar(state))];
    if let Some(VariantStatistics::Shredded(ty, shredded)) = stats.variant.as_deref() {
        if let Some(value) = shredded_struct(ty, shredded, true) {
            fields.push(("fully_shredded".to_string(), value));
        }
        if let Some(value) = shredded_struct(ty, shredded, false) {
            fields.push(("partially_shredded".to_string(), value));
        }
    }
    fields
}

/// `WrapTypedValue` without an untyped part: a primitive level as it is, a nested one in a STRUCT.
fn wrap(typed_type: LogicalType, typed_stats: Stats) -> (LogicalType, Stats) {
    if !typed_type.is_nested() {
        return (typed_type, typed_stats);
    }
    let stats = Stats {
        can_be_null: typed_stats.can_be_null,
        can_be_valid: typed_stats.can_be_valid,
        children: vec![typed_stats],
        ..unknown()
    };
    (
        LogicalType::struct_type(vec![("typed_value".to_string(), typed_type)]),
        stats,
    )
}

/// `TryBuildShreddingStats`: the typed part that a cast of values of `ty` to VARIANT has, with its statistics.
fn shredding_of(ty: &LogicalType, stats: &Stats) -> Option<(LogicalType, Stats)> {
    match ty.id() {
        LogicalTypeId::Struct => {
            let fields = ty.struct_children();
            if fields.is_empty() {
                return None;
            }
            let mut types = Vec::with_capacity(fields.len());
            let mut children = Vec::with_capacity(fields.len());
            for (at, (name, field)) in fields.iter().enumerate() {
                let (field_type, field_stats) = shredding_of(field, &child(stats, at))?;
                types.push((name.clone(), field_type));
                children.push(field_stats);
            }
            let typed = Stats {
                can_be_null: stats.can_be_null,
                can_be_valid: stats.can_be_valid,
                children,
                ..unknown()
            };
            Some(wrap(LogicalType::struct_type(types), typed))
        }
        LogicalTypeId::List | LogicalTypeId::Array => {
            let (element_type, element_stats) = shredding_of(ty.child_type()?, &child(stats, 0))?;
            let typed = Stats {
                can_be_null: stats.can_be_null,
                can_be_valid: stats.can_be_valid,
                children: vec![element_stats],
                ..unknown()
            };
            Some(wrap(LogicalType::list(element_type), typed))
        }
        LogicalTypeId::Map | LogicalTypeId::Union | LogicalTypeId::Enum | LogicalTypeId::Variant => None,
        _ if ty.is_nested() || crate::function::json::is_json_type(ty) => None,
        _ => Some((
            ty.clone(),
            Stats {
                variant: None,
                ..stats.clone()
            },
        )),
    }
}

/// `StatisticsPropagateToVariant`: the statistics of a cast of values of `source` to VARIANT, which shreds
/// each of them into the same typed part.
pub(super) fn cast_to_variant(source: &LogicalType, stats: &Stats) -> Option<Stats> {
    let (ty, shredded) = shredding_of(source, stats)?;
    Some(Stats {
        can_be_null: stats.can_be_null,
        can_be_valid: stats.can_be_valid,
        distinct: stats.distinct,
        variant: Some(Box::new(VariantStatistics::Shredded(ty, shredded))),
        ..unknown()
    })
}

/// `ToStructuredType`: the type the typed part holds its values as, without the levels around them.
fn structured_type(ty: &LogicalType) -> LogicalType {
    if ty.id() != LogicalTypeId::Struct {
        return ty.clone();
    }
    let typed = &ty.struct_children()[0].1;
    match typed.id() {
        LogicalTypeId::Struct => {
            let mut fields: Vec<(String, LogicalType)> = typed
                .struct_children()
                .iter()
                .map(|(name, field)| (name.clone(), structured_type(field)))
                .collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            LogicalType::struct_type(fields)
        }
        LogicalTypeId::List => LogicalType::list(structured_type(typed.child_type().expect("a list"))),
        _ => typed.clone(),
    }
}

/// `StatisticsPropagateVariant`: the statistics of a cast of a VARIANT to `target`, which are those of its
/// typed part when every value is in it with that type.
pub(super) fn cast_from_variant(stats: &Stats, target: &LogicalType) -> Option<Stats> {
    if target.is_nested() || target.id() == LogicalTypeId::Variant {
        return None;
    }
    let Some(VariantStatistics::Shredded(ty, shredded)) = stats.variant.as_deref() else {
        return None;
    };
    if !fully_shredded(ty, shredded) {
        return None;
    }
    let typed_type = structured_type(ty);
    let (_, mut result) = typed(ty, shredded);
    // `CastStatistics::TryPropagate` of typed values of another integer type: their bounds, cast
    if typed_type != *target {
        if !typed_type.is_integral() || !target.is_integral() {
            return None;
        }
        let convert = |value: Option<Value>| {
            let cast = crate::planner::expression::BoundExpr::constant(value?).cast_to(target, false);
            let converted = crate::execution::expression_executor::try_evaluate_scalar(&cast)?;
            (converted.logical_type() == target).then_some(converted)
        };
        let (min, max) = (convert(result.min.take())?, convert(result.max.take())?);
        result = Stats {
            min: Some(min),
            max: Some(max),
            can_be_null: result.can_be_null,
            can_be_valid: result.can_be_valid,
            ..Stats::unknown()
        };
    }
    if stats.can_be_null {
        result.can_be_null = true;
    }
    Some(result)
}

/// `GetVariantCompressInfo`: of a VARIANT fully shredded on a primitive type whose equal values are the same
/// VARIANT, that type, and the statistics of the values cast to it (NULL where the VARIANT is).
pub(super) fn compress_info(stats: &Stats) -> Option<(LogicalType, Stats)> {
    let Some(VariantStatistics::Shredded(ty, shredded)) = stats.variant.as_deref() else {
        return None;
    };
    let structured = structured_type(ty);
    if structured.is_nested() || !fully_shredded(ty, shredded) {
        return None;
    }
    use LogicalTypeId::*;
    // FLOAT and DOUBLE (-0.0 = 0.0) and INTERVAL ('1 month' = '30 days') have equal values that differ
    if !matches!(
        structured.id(),
        Boolean
            | TinyInt
            | SmallInt
            | Integer
            | BigInt
            | HugeInt
            | UTinyInt
            | USmallInt
            | UInteger
            | UBigInt
            | UHugeInt
            | Decimal
            | Varchar
            | Blob
            | Date
            | Time
            | TimeNs
            | TimestampSec
            | TimestampMs
            | Timestamp
            | TimestampNs
            | TimestampTz
            | TimestampTzNs
            | Uuid
    ) {
        return None;
    }
    let (_, mut typed_stats) = typed(ty, shredded);
    typed_stats.can_be_null |= stats.can_be_null;
    Some((structured, typed_stats))
}

/// What the statistics of the VARIANT columns that `column` gives say of `expr`, where it is a path of one
/// (`variant_extract`s with constant paths) cast to a primitive type: the statistics `ExpressionFilter::
/// CheckStatistics` takes of a filter's expression. `None` for any other expression, or where nothing is known.
pub(crate) fn variant_path_statistics(
    expr: &crate::planner::expression::BoundExpr,
    column: &dyn Fn(usize) -> Option<Stats>,
) -> Option<Stats> {
    use crate::planner::expression::BoundExprKind;
    match &expr.kind {
        BoundExprKind::Reference(at) if expr.return_type.id() == LogicalTypeId::Variant => column(*at),
        BoundExprKind::Function { function, children, .. }
            if function.name == "variant_extract" && children.len() == 2 =>
        {
            let BoundExprKind::Constant(key) = &children[1].kind else {
                return None;
            };
            extract(&variant_path_statistics(&children[0], column)?, key)
        }
        BoundExprKind::Cast { child, try_cast: false } if child.return_type.id() == LogicalTypeId::Variant => {
            cast_from_variant(&variant_path_statistics(child, column)?, &expr.return_type)
        }
        _ => None,
    }
}

/// `VariantExtractPropagateStats`: the statistics of a field (`key`) or an element (`index`) of a VARIANT.
pub(super) fn extract(stats: &Stats, key: &Value) -> Option<Stats> {
    let Some(VariantStatistics::Shredded(ty, shredded)) = stats.variant.as_deref() else {
        return None;
    };
    if !fully_shredded(ty, shredded) {
        return None;
    }
    let (typed_type, typed_stats) = typed(ty, shredded);
    let by_index = key.logical_type().is_integral();
    let (field_type, field_stats) = if by_index {
        if typed_type.id() != LogicalTypeId::List {
            return None;
        }
        (typed_type.child_type()?.clone(), child(&typed_stats, 0))
    } else {
        if typed_type.id() != LogicalTypeId::Struct {
            return None;
        }
        let name = key.try_to_string().ok()?;
        let at = typed_type
            .struct_children()
            .iter()
            .position(|(field, _)| *field == name)?;
        (typed_type.struct_children()[at].1.clone(), child(&typed_stats, at))
    };
    if !fully_shredded(&field_type, &field_stats) {
        return None;
    }
    // `WrapExtractedFieldAsVariant`: NULL where the field's typed value is
    let mut can_be_null = stats.can_be_null || by_index;
    let (mut level_type, mut level_stats) = (field_type.clone(), field_stats.clone());
    loop {
        can_be_null |= level_stats.can_be_null;
        if level_type.id() != LogicalTypeId::Struct {
            break;
        }
        (level_type, level_stats) = typed(&level_type, &level_stats);
    }
    Some(Stats {
        can_be_null,
        can_be_valid: stats.can_be_valid,
        variant: Some(Box::new(VariantStatistics::Shredded(field_type, field_stats))),
        ..unknown()
    })
}
