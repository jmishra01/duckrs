// Ported from duckdb/src/function/cast_rules.cpp @ rust-port-baseline
//
//! Implicit cast costs (`CastRules::ImplicitCast`): how cheaply a value of one type can be
//! converted to another without an explicit CAST, or -1 if it cannot. Function overload
//! resolution and type unification pick the cheapest option.

use duckrs_common::types::{LogicalType, LogicalTypeId};

/// `AnyType::GetCastScore` of a plain `ANY` parameter.
const ANY_CAST_SCORE: i64 = 5;

/// The target type determines the preferred implicit casts (`TargetTypeCost`).
fn target_type_cost(ty: &LogicalType) -> i64 {
    use LogicalTypeId::*;
    match ty.id() {
        BigInt => 101,
        Integer => 102,
        HugeInt => 103,
        Double => 104,
        Decimal => 105,
        Bignum => 106,
        TimestampNs => 119,
        Timestamp => 120,
        TimestampMs => 121,
        TimestampSec => 122,
        TimestampTz => 123,
        TimestampTzNs => 124,
        Varchar => 149,
        Struct | Tuple | Map | List | Union | Array => 160,
        Any => ANY_CAST_SCORE,
        // we can cast anything to a template type, but prefer to cast to anything else
        Template => 1_000_000,
        _ => 110,
    }
}

/// The targets each source type casts to implicitly (`ImplicitCastTinyint` ... `ImplicitCastVariant`).
fn implicit_targets(from: LogicalTypeId) -> Option<&'static [LogicalTypeId]> {
    use LogicalTypeId::*;
    Some(match from {
        TinyInt => &[SmallInt, Integer, BigInt, HugeInt, Float, Double, Decimal, Bignum],
        SmallInt => &[Integer, BigInt, HugeInt, Float, Double, Decimal, Bignum],
        Integer => &[BigInt, HugeInt, Float, Double, Decimal, Bignum],
        BigInt => &[Float, Double, HugeInt, Decimal, Bignum],
        UTinyInt => &[
            USmallInt, UInteger, UBigInt, SmallInt, Integer, BigInt, HugeInt, UHugeInt, Float, Double, Decimal, Bignum,
        ],
        USmallInt => &[
            UInteger, UBigInt, Integer, BigInt, HugeInt, UHugeInt, Float, Double, Decimal, Bignum,
        ],
        UInteger => &[UBigInt, BigInt, UHugeInt, HugeInt, Float, Double, Decimal, Bignum],
        UBigInt => &[Float, Double, UHugeInt, HugeInt, Decimal, Bignum],
        HugeInt | UHugeInt => &[Float, Double, Decimal, Bignum],
        Float => &[Double],
        Double => &[Bignum],
        Decimal => &[Float, Double],
        Date => &[
            Timestamp,
            TimestampTz,
            TimestampTzNs,
            TimestampMs,
            TimestampNs,
            TimestampSec,
        ],
        Enum => &[Varchar],
        TimestampSec => &[Timestamp, TimestampMs, TimestampNs, TimestampTz, TimestampTzNs],
        TimestampMs => &[Timestamp, TimestampNs, TimestampTz, TimestampTzNs],
        // we allow casting ALL timestamps, including nanosecond ones, to TimestampNS
        TimestampNs => &[Timestamp],
        Timestamp => &[TimestampNs, TimestampTz, TimestampTzNs],
        Bignum => &[Double],
        _ => return None,
    })
}

/// `LogicalTypeIsValid`: whether a string literal can be cast to `ty` (it must be fully specified).
fn logical_type_is_valid(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    match ty.id() {
        Struct | Tuple | Union | Variant | List | Map | Array | Decimal if !ty.has_params() => false,
        Any | Invalid | Unknown => false,
        Struct | Tuple | Union => ty.struct_children().iter().all(|(_, c)| logical_type_is_valid(c)),
        List | Map | Array => ty.child_type().is_some_and(logical_type_is_valid),
        _ => true,
    }
}

/// `IntegerLiteral::FitsInType`: whether an INTEGER_LITERAL's value fits `target`.
pub fn integer_literal_fits(literal: &LogicalType, target: &LogicalType) -> bool {
    // we can always cast integer literals to float and double
    if matches!(target.id(), LogicalTypeId::Float | LogicalTypeId::Double) {
        return true;
    }
    if !target.is_integral() {
        return false;
    }
    let (_, value, beyond_i128) = literal.integer_literal_value().expect("an INTEGER_LITERAL");
    if beyond_i128 {
        return target.id() == LogicalTypeId::UHugeInt;
    }
    let (min, max) = integral_range(target.id());
    (min..=max).contains(&value)
}

/// The value range of an integral type, as `i128` (UHUGEINT's upper bound saturates).
pub fn integral_range(id: LogicalTypeId) -> (i128, i128) {
    use LogicalTypeId::*;
    match id {
        TinyInt => (i8::MIN as i128, i8::MAX as i128),
        SmallInt => (i16::MIN as i128, i16::MAX as i128),
        Integer => (i32::MIN as i128, i32::MAX as i128),
        BigInt => (i64::MIN as i128, i64::MAX as i128),
        HugeInt => (i128::MIN, i128::MAX),
        UTinyInt => (0, u8::MAX as i128),
        USmallInt => (0, u16::MAX as i128),
        UInteger => (0, u32::MAX as i128),
        UBigInt => (0, u64::MAX as i128),
        UHugeInt => (0, i128::MAX),
        _ => panic!("{id:?} is not integral"),
    }
}

/// `ImplicitCastToUnionMember`: the cheapest cast to one of a union's members.
fn implicit_cast_to_union_member(from: &LogicalType, to: &LogicalType) -> i64 {
    if !to.has_params() {
        return -1;
    }
    // the first child is the tag
    to.struct_children()[1..]
        .iter()
        .map(|(_, member)| implicit_cast(from, member))
        .filter(|&cost| cost != -1)
        .min()
        .unwrap_or(-1)
}

fn is_struct(id: LogicalTypeId) -> bool {
    matches!(id, LogicalTypeId::Struct | LogicalTypeId::Tuple)
}

/// `CastRules::ImplicitCast`: the cost of implicitly casting `from` to `to`, or -1.
pub fn implicit_cast(from: &LogicalType, to: &LogicalType) -> i64 {
    use LogicalTypeId::*;
    if from.id() == SqlNull && to.id() == Template {
        // prefer the TEMPLATE type for NULL casts, as it is the most generic
        return 5;
    }
    if from.id() == SqlNull || to.id() == Any || to.id() == Template {
        // NULL expression can be cast to anything
        return target_type_cost(to);
    }
    if from.id() == Unknown {
        // parameter expression can be cast to anything for no cost
        return 0;
    }
    if from.id() == StringLiteral {
        // string literals can be cast to any type for low cost as long as the type is valid
        if !logical_type_is_valid(to) {
            return -1;
        }
        if to.id() == Varchar && to.alias().is_none() {
            return 1;
        }
        return 20;
    }
    if from.id() == IntegerLiteral {
        let (literal_type, ..) = from.integer_literal_value().expect("an INTEGER_LITERAL");
        // the integer literal has an underlying type - this type always matches
        if literal_type.id() == to.id() {
            return 0;
        }
        // integer literals can be cast to any other integer type for a low cost, but only if the literal fits
        if integer_literal_fits(from, to) {
            // to avoid ties we prefer BIGINT, INT, ...
            let target_cost = target_type_cost(to);
            assert!(
                target_cost >= 100,
                "Integer literal implicit cast - TargetTypeCost should be >= 100"
            );
            return target_cost - 90;
        }
        // in any other case we use the casting rules of the preferred type of the literal
        return implicit_cast(&literal_type, to);
    }
    if from.alias() == Some("JSON") && to.id() == Varchar && to.alias().is_none() {
        return 1;
    }
    if to.alias() == Some("JSON") && to.id() == Varchar && from.alias().is_none() {
        // `RegisterJSONCastFunctions`: a parse, so one more than a cast to STRUCT
        return match from.id() {
            Varchar => implicit_cast(&LogicalType::SQLNULL, &LogicalType::new(Struct)) + 1,
            SqlNull => implicit_cast(&LogicalType::SQLNULL, &LogicalType::VARCHAR) + 1,
            _ => -1,
        };
    }
    if from.alias() != to.alias() {
        // if aliases are different, an implicit cast is not possible, except to a union with the type as member
        if to.id() == Union {
            return implicit_cast_to_union_member(from, to);
        }
        return -1;
    }
    match (from.id(), to.id()) {
        (List, List) => {
            // lists can be cast if their child types can be cast
            let mut child_cost = implicit_cast(from.child_type().unwrap(), to.child_type().unwrap());
            if child_cost >= 1 {
                // we prefer LIST[X] -> LIST[VARCHAR] over LIST[X] -> VARCHAR
                child_cost -= 1;
            }
            return child_cost;
        }
        (Array, Array) => {
            let to_any_size = to.array_size() == Some(0);
            if from.array_size() == to.array_size() || to_any_size {
                let mut child_cost = implicit_cast(from.child_type().unwrap(), to.child_type().unwrap());
                if child_cost >= 100 {
                    // we prefer ARRAY[X] -> ARRAY[VARCHAR] over ARRAY[X] -> VARCHAR
                    child_cost -= 1;
                }
                return child_cost;
            }
            return -1;
        }
        (Array, List) => {
            let child_cost = implicit_cast(from.child_type().unwrap(), to.child_type().unwrap());
            if child_cost < 0 {
                return -1;
            }
            // we prefer ARRAY->ARRAY casts over ARRAY->LIST casts
            return child_cost + 1;
        }
        (List, Array) if to.array_size() != Some(0) => {
            return implicit_cast(from.child_type().unwrap(), to.child_type().unwrap());
        }
        (Union, Union) => {
            if !to.has_params() {
                return 0;
            }
            // unions can be cast if the source tags are a subset of the target tags; the most expensive cost is used
            let mut cost = -1;
            for (name, from_member) in &from.struct_children()[1..] {
                let Some((_, to_member)) = to.struct_children()[1..].iter().find(|(n, _)| n == name) else {
                    return -1;
                };
                cost = cost.max(implicit_cast(from_member, to_member));
            }
            if cost != -1 {
                return cost;
            }
        }
        _ => {}
    }
    if is_struct(from.id()) && is_struct(to.id()) {
        if !to.has_params() {
            return 0;
        }
        let (source, target) = (from.struct_children(), to.struct_children());
        if source.len() != target.len() {
            return -1;
        }
        if source.is_empty() {
            return 0;
        }
        let unnamed = |children: &[(String, LogicalType)]| children.iter().all(|(n, _)| n.is_empty());
        let mut cost = -1;
        if !unnamed(source) && !unnamed(target) {
            // match the source members to the target members by name
            let mut remaining: Vec<&(String, LogicalType)> = target.iter().collect();
            for (i, (name, _)) in target.iter().enumerate() {
                if target[..i].iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    return -1;
                }
            }
            for (name, source_type) in source {
                let Some(pos) = remaining.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) else {
                    return -1;
                };
                let (_, target_type) = remaining.remove(pos);
                let child_cost = implicit_cast(source_type, target_type);
                if child_cost == -1 {
                    return -1;
                }
                cost = cost.max(child_cost);
            }
        } else {
            for ((_, source_type), (_, target_type)) in source.iter().zip(target) {
                let child_cost = implicit_cast(source_type, target_type);
                if child_cost == -1 {
                    return -1;
                }
                cost = cost.max(child_cost);
            }
        }
        return cost;
    }
    if from.id() == to.id() {
        // arguments match: do nothing
        return 0;
    }
    if to.id() == Union {
        return implicit_cast_to_union_member(from, to);
    }
    if from.id() == Variant {
        return target_type_cost(to);
    }
    match implicit_targets(from.id()) {
        Some(targets) if targets.contains(&to.id()) => target_type_cost(to),
        _ => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: i128) -> LogicalType {
        LogicalType::integer_literal(LogicalTypeId::Integer, value, false)
    }

    #[test]
    fn numeric_widening() {
        assert_eq!(implicit_cast(&LogicalType::INTEGER, &LogicalType::INTEGER), 0);
        assert_eq!(implicit_cast(&LogicalType::INTEGER, &LogicalType::BIGINT), 101);
        assert_eq!(implicit_cast(&LogicalType::INTEGER, &LogicalType::DOUBLE), 104);
        assert_eq!(implicit_cast(&LogicalType::BIGINT, &LogicalType::INTEGER), -1);
        assert_eq!(implicit_cast(&LogicalType::DOUBLE, &LogicalType::FLOAT), -1);
        assert_eq!(implicit_cast(&LogicalType::INTEGER, &LogicalType::VARCHAR), -1);
    }

    #[test]
    fn literals() {
        // the literal's own type is free; other integer types it fits are cheap
        assert_eq!(implicit_cast(&literal(2), &LogicalType::INTEGER), 0);
        assert_eq!(implicit_cast(&literal(2), &LogicalType::TINYINT), 20);
        assert_eq!(implicit_cast(&literal(2), &LogicalType::BIGINT), 11);
        assert_eq!(implicit_cast(&literal(1000), &LogicalType::TINYINT), -1);
        let string = LogicalType::new(LogicalTypeId::StringLiteral);
        assert_eq!(implicit_cast(&string, &LogicalType::VARCHAR), 1);
        assert_eq!(implicit_cast(&string, &LogicalType::INTEGER), 20);
        assert_eq!(implicit_cast(&LogicalType::SQLNULL, &LogicalType::INTEGER), 102);
    }
}
