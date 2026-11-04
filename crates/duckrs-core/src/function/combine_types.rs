// Ported from duckdb/src/common/types.cpp (TryGetMaxLogicalType, ForceMaxLogicalType and
// helpers) and duckdb/src/function/combine_types_rules.cpp @ rust-port-baseline
//
//! Unifying two types into one both can be cast to (`LogicalType::MaxLogicalType`): the type of
//! a comparison, a CASE, a set operation or a VALUES column. Registered extension casts are not
//! supported yet, so the built-in rules and [`implicit_cast`] decide.

use super::cast_rules::implicit_cast;
use duckrs_common::types::{LogicalType, LogicalTypeId, decimal};

/// `LogicalType::NormalizeType`: the type a literal type stands for.
pub fn normalize_type(ty: &LogicalType) -> LogicalType {
    match ty.id() {
        LogicalTypeId::StringLiteral => LogicalType::VARCHAR,
        LogicalTypeId::IntegerLiteral => literal_type(ty),
        _ => ty.clone(),
    }
}

/// `IntegerLiteral::GetType`.
fn literal_type(ty: &LogicalType) -> LogicalType {
    ty.integer_literal_value().expect("an INTEGER_LITERAL").0
}

/// `LogicalType::GetDecimalProperties`: the DECIMAL `(width, scale)` a numeric type fits in.
pub fn decimal_properties(ty: &LogicalType) -> Option<(u8, u8)> {
    use LogicalTypeId::*;
    Some(match ty.id() {
        SqlNull | Boolean => (1, 0),
        TinyInt => (3, 0),
        SmallInt => (5, 0),
        Integer => (10, 0),
        BigInt => (19, 0),
        UTinyInt => (3, 0),
        USmallInt => (5, 0),
        UInteger => (10, 0),
        UBigInt => (20, 0),
        // (u)hugeint: max size decimal (38, 0); note that a (u)hugeint is not guaranteed to fit
        HugeInt | UHugeInt => (38, 0),
        Decimal => return ty.decimal_properties(),
        IntegerLiteral => return decimal_properties(&literal_type(ty)),
        _ => return None,
    })
}

/// `DecimalSizeCheck`: grows a DECIMAL so that an integer type fits it too.
fn decimal_size_check(left: &LogicalType, right: &LogicalType) -> LogicalType {
    if left.id() == LogicalTypeId::Decimal {
        return decimal_size_check(right, left);
    }
    let (width, scale) = right.decimal_properties().expect("a DECIMAL");
    let (other_width, _) = decimal_properties(left).expect("Type provided to DecimalSizeCheck was not a numeric type");
    let effective_width = width - scale;
    if other_width > effective_width {
        // cap the width at max, if an actual value exceeds this, an exception will be thrown later
        let new_width = (other_width + scale).min(decimal::MAX_WIDTH);
        return LogicalType::decimal(new_width, scale);
    }
    right.clone()
}

/// `CombineNumericTypes`: the type both a signed and an unsigned integer type fit in.
fn combine_numeric_types(left: &LogicalType, right: &LogicalType) -> LogicalType {
    use LogicalTypeId::*;
    if left.id() > right.id() {
        return combine_numeric_types(right, left);
    }
    if left.id() == BigInt || right.id() == UBigInt {
        return LogicalType::HUGEINT;
    }
    if left.id() == Integer || right.id() == UInteger {
        return LogicalType::BIGINT;
    }
    if left.id() == SmallInt || right.id() == USmallInt {
        return LogicalType::INTEGER;
    }
    if left.id() == TinyInt || right.id() == UTinyInt {
        return LogicalType::SMALLINT;
    }
    // no type is larger than (u)hugeint, so casting to double is required
    if left.id() == UHugeInt || right.id() == HugeInt {
        return LogicalType::DOUBLE;
    }
    panic!("Cannot combine these numeric types ({left} & {right})");
}

/// The `LogicalTypeResolver` operation: forcing (`ForceMaxLogicalType`) always yields a type.
fn resolve(force: bool, left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    if force {
        Some(force_max_logical_type(left, right))
    } else {
        try_max_internal(false, left, right)
    }
}

fn is_struct(id: LogicalTypeId) -> bool {
    matches!(id, LogicalTypeId::Struct | LogicalTypeId::Tuple)
}

/// `CombineStructTypes`.
fn combine_struct_types(force: bool, left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    let (left_children, right_children) = (left.struct_children(), right.struct_children());
    let unnamed = |ty: &LogicalType| ty.struct_children().iter().all(|(n, _)| n.is_empty());
    let (left_unnamed, right_unnamed) = (unnamed(left), unnamed(right));
    if left_unnamed || right_unnamed {
        // at least one side is unnamed, so we attempt positional casting
        if left_children.len() != right_children.len() {
            return None;
        }
        let mut children = Vec::new();
        for ((left_name, left_type), (right_name, right_type)) in left_children.iter().zip(right_children) {
            let child = resolve(force, left_type, right_type)?;
            let name = if left_unnamed { right_name } else { left_name };
            children.push((name.clone(), child));
        }
        return Some(if children.is_empty() && left.id() == LogicalTypeId::Struct {
            left.clone()
        } else if left_unnamed && right_unnamed {
            LogicalType::tuple(children.into_iter().map(|(_, c)| c).collect())
        } else {
            LogicalType::struct_type(children)
        });
    }
    // create a super-set of the STRUCT fields
    let find = |name: &str| right_children.iter().position(|(n, _)| n.eq_ignore_ascii_case(name));
    let mut matched = vec![false; right_children.len()];
    let mut children = Vec::new();
    for (name, left_type) in left_children {
        match find(name) {
            Some(i) if !matched[i] => {
                children.push((name.clone(), resolve(force, left_type, &right_children[i].1)?));
                matched[i] = true;
            }
            _ => children.push((name.clone(), left_type.clone())),
        }
    }
    for (i, (name, right_type)) in right_children.iter().enumerate() {
        if !matched[i] && find(name) == Some(i) {
            children.push((name.clone(), right_type.clone()));
        }
    }
    Some(LogicalType::struct_type(children))
}

/// `DefaultCombineTypesRules`, matched in order: `Some(result)` when a rule applies, where the
/// result is `None` if the rule says the types cannot be combined.
fn combine_rules(force: bool, left: &LogicalType, right: &LogicalType) -> Option<Option<LogicalType>> {
    use LogicalTypeId::*;
    let (l, r) = (left.id(), right.id());
    let either = |id: LogicalTypeId| l == id || r == id;
    if left.alias().is_some() || right.alias().is_some() {
        // we always prefer aliased types
        return Some(Some(if left.alias().is_some() { left } else { right }.clone()));
    }
    if l != r {
        if either(Variant) {
            return Some(Some(if r == Variant { right } else { left }.clone()));
        }
        if either(SqlNull) || either(Unknown) {
            // NULL/unknown (parameter) types always take the other type
            let other = if l == SqlNull || (r != SqlNull && l == Unknown) {
                right
            } else {
                left
            };
            return Some(Some(normalize_type(other)));
        }
        if either(Enum) {
            // for enums, match the varchar rules
            return Some(if l == Enum {
                resolve(force, &LogicalType::VARCHAR, right)
            } else {
                resolve(force, left, &LogicalType::VARCHAR)
            });
        }
        if is_struct(l) && is_struct(r) {
            return Some(combine_struct_types(force, left, right));
        }
        if either(StringLiteral) {
            // string literals take the other type
            return Some(Some(normalize_type(if l == StringLiteral { right } else { left })));
        }
        let bignum_pair = |a: LogicalTypeId, b: LogicalTypeId| a == Bignum && matches!(b, Float | Decimal);
        if bignum_pair(l, r) || bignum_pair(r, l) {
            return Some(Some(LogicalType::DOUBLE));
        }
        return None;
    }
    Some(match l {
        StringLiteral => Some(LogicalType::VARCHAR),
        // for two integer literals we unify the underlying types
        IntegerLiteral => resolve(force, &literal_type(left), &literal_type(right)),
        // if both types are different ENUMs we do a string comparison
        Enum => Some(if left == right {
            left.clone()
        } else {
            LogicalType::VARCHAR
        }),
        // collations are not supported yet, so both sides are plain VARCHAR
        Varchar => Some(left.clone()),
        Decimal => {
            // unify the width/scale so that the resulting decimal always fits
            let (lw, ls) = left.decimal_properties().unwrap();
            let (rw, rs) = right.decimal_properties().unwrap();
            let extra_width = (lw - ls).max(rw - rs);
            let mut scale = ls.max(rs);
            let mut width = extra_width + scale;
            if width > decimal::MAX_WIDTH {
                // if the resulting decimal does not fit, we truncate the scale
                width = decimal::MAX_WIDTH;
                scale = width - extra_width;
            }
            Some(LogicalType::decimal(width, scale))
        }
        List => resolve(force, left.child_type().unwrap(), right.child_type().unwrap()).map(LogicalType::list),
        Array => {
            let child = resolve(force, left.child_type().unwrap(), right.child_type().unwrap());
            let size = left.array_size().unwrap().max(right.array_size().unwrap());
            child.map(|c| LogicalType::array(c, size))
        }
        Map => resolve(force, left.child_type().unwrap(), right.child_type().unwrap()).map(LogicalType::map_from_entry),
        Struct | Tuple => combine_struct_types(force, left, right),
        Union if left == right => Some(left.clone()),
        Union => return None,
        _ => return None,
    })
}

/// `TryCombineViaImplicitCast`: try the implicit cast rules in both directions and keep the
/// cheaper one.
fn combine_via_implicit_cast(left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    let left_to_right = implicit_cast(left, right);
    let right_to_left = implicit_cast(right, left);
    if left_to_right >= 0 && (left_to_right < right_to_left || right_to_left < 0) {
        // we can implicitly cast left to right, return right
        return Some(if right.id() == LogicalTypeId::Decimal {
            if left.id() == LogicalTypeId::Variant {
                left.clone()
            } else {
                decimal_size_check(left, right)
            }
        } else {
            right.clone()
        });
    }
    if right_to_left >= 0 {
        return Some(if left.id() == LogicalTypeId::Decimal {
            if right.id() == LogicalTypeId::Variant {
                right.clone()
            } else {
                decimal_size_check(right, left)
            }
        } else {
            left.clone()
        });
    }
    None
}

/// `TryGetMaxLogicalTypeInternal`.
fn try_max_internal(force: bool, left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    if let Some(result) = combine_rules(force, left, right) {
        return result;
    }
    if left.id() != right.id() {
        // for other types - try to combine through the generic implicit cast rules
        if let Some(result) = combine_via_implicit_cast(left, right) {
            return Some(result);
        }
        // for integer literals - rerun the operation with the underlying type
        if left.id() == LogicalTypeId::IntegerLiteral {
            return resolve(force, &literal_type(left), right);
        }
        if right.id() == LogicalTypeId::IntegerLiteral {
            return resolve(force, left, &literal_type(right));
        }
        // for unsigned/signed comparisons we have a few fallbacks
        if left.is_numeric() && right.is_numeric() {
            return Some(combine_numeric_types(left, right));
        }
        if left.id() == LogicalTypeId::Boolean && right.is_integral() {
            return Some(right.clone());
        }
        if right.id() == LogicalTypeId::Boolean && left.is_integral() {
            return Some(left.clone());
        }
        return None;
    }
    // equal types fallthrough
    Some(left.clone())
}

/// `LogicalType::TryGetMaxLogicalType`: the type both sides implicitly cast to, if any.
pub fn try_max_logical_type(left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    try_max_internal(false, left, right)
}

/// `GetLogicalTypeScore`: which type wins when neither casts to the other.
fn logical_type_score(ty: &LogicalType) -> u32 {
    use LogicalTypeId::*;
    match ty.id() {
        Invalid | SqlNull | Unknown | Any | Template | StringLiteral | IntegerLiteral => 0,
        Boolean => 10,
        UTinyInt => 11,
        TinyInt => 12,
        USmallInt => 13,
        SmallInt => 14,
        UInteger => 15,
        Integer => 16,
        UBigInt => 17,
        BigInt => 18,
        UHugeInt => 19,
        HugeInt => 20,
        Decimal => 21,
        Float => 22,
        Double => 23,
        Time | TimeTz => 50,
        TimeNs => 51,
        Date => 52,
        TimestampSec => 53,
        TimestampMs => 54,
        Timestamp | TimestampTz => 55,
        TimestampNs | TimestampTzNs => 56,
        Interval => 58,
        Char => 75,
        Varchar => 77,
        Enum => 78,
        Bit => 100,
        Blob => 101,
        Uuid => 102,
        Bignum => 103,
        Geometry => 104,
        Struct | Tuple => 125,
        List | Array => 126,
        Map => 127,
        Variant | Union | Table => 150,
        _ => 1000,
    }
}

/// `LogicalType::ForceMaxLogicalType`: like [`try_max_logical_type`], but when the types cannot
/// be unified, the one with the higher score wins.
pub fn force_max_logical_type(left: &LogicalType, right: &LogicalType) -> LogicalType {
    if let Some(result) = try_max_internal(true, left, right) {
        return result;
    }
    if logical_type_score(left) < logical_type_score(right) {
        right.clone()
    } else {
        left.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: i128) -> LogicalType {
        LogicalType::integer_literal(LogicalTypeId::Integer, value, false)
    }

    #[test]
    fn unifies_like_duckdb() {
        let max = |a: &LogicalType, b: &LogicalType| try_max_logical_type(a, b);
        assert_eq!(
            max(&LogicalType::INTEGER, &LogicalType::BIGINT),
            Some(LogicalType::BIGINT)
        );
        assert_eq!(
            max(&LogicalType::INTEGER, &LogicalType::UINTEGER),
            Some(LogicalType::BIGINT)
        );
        assert_eq!(
            max(&LogicalType::BIGINT, &LogicalType::UBIGINT),
            Some(LogicalType::HUGEINT)
        );
        assert_eq!(max(&LogicalType::TINYINT, &literal(5)), Some(LogicalType::TINYINT));
        assert_eq!(max(&LogicalType::TINYINT, &literal(1000)), Some(LogicalType::INTEGER));
        assert_eq!(
            max(&LogicalType::INTEGER, &LogicalType::DOUBLE),
            Some(LogicalType::DOUBLE)
        );
        assert_eq!(
            max(&LogicalType::INTEGER, &LogicalType::decimal(4, 1)),
            Some(LogicalType::decimal(11, 1))
        );
        assert_eq!(
            max(&LogicalType::SQLNULL, &LogicalType::VARCHAR),
            Some(LogicalType::VARCHAR)
        );
        let string = LogicalType::new(LogicalTypeId::StringLiteral);
        assert_eq!(max(&string, &LogicalType::INTEGER), Some(LogicalType::INTEGER));
        assert_eq!(max(&LogicalType::VARCHAR, &LogicalType::INTEGER), None);
        assert_eq!(
            force_max_logical_type(&LogicalType::VARCHAR, &LogicalType::INTEGER),
            LogicalType::VARCHAR
        );
    }
}
