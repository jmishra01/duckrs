// Ported from duckdb/src/include/duckdb/common/operator/comparison_operators.hpp @ rust-port-baseline

use crate::types::Interval;
use std::cmp::Ordering;

/// DuckDB's total order on stored values (`Equals` / `GreaterThan` and friends). It differs
/// from Rust's `PartialOrd` only for floats: NaN equals NaN and is greater than every other
/// value, and `-0.0 == 0.0`. Intervals compare in normalized form.
pub trait DuckOrd: Copy {
    fn duck_cmp(&self, other: &Self) -> Ordering;

    #[inline]
    fn duck_eq(&self, other: &Self) -> bool {
        self.duck_cmp(other) == Ordering::Equal
    }
}

macro_rules! ord_via_ord {
    ($($t:ty),*) => {$(
        impl DuckOrd for $t {
            #[inline]
            fn duck_cmp(&self, other: &Self) -> Ordering {
                self.cmp(other)
            }
            #[inline]
            fn duck_eq(&self, other: &Self) -> bool {
                self == other
            }
        }
    )*};
}
ord_via_ord!(bool, i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, Interval);

macro_rules! ord_float {
    ($($t:ty),*) => {$(
        impl DuckOrd for $t {
            #[inline]
            fn duck_cmp(&self, other: &Self) -> Ordering {
                match (self.is_nan(), other.is_nan()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    _ => self.partial_cmp(other).expect("non-NaN floats are ordered"),
                }
            }
            #[inline]
            fn duck_eq(&self, other: &Self) -> bool {
                self == other || (self.is_nan() && other.is_nan())
            }
        }
    )*};
}
ord_float!(f32, f64);

/// A comparison between two values, including the NULL-aware `DISTINCT FROM` forms
/// (the comparison `ExpressionType`s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonType {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    DistinctFrom,
    NotDistinctFrom,
}

impl ComparisonType {
    /// Evaluates the comparison given each side's NULL flag and, when both are non-NULL, their
    /// ordering (`ComparisonOperationWrapper`). Ordinary comparisons are never true for NULLs.
    #[inline]
    pub fn evaluate(self, left_null: bool, right_null: bool, ordering: impl FnOnce() -> Ordering) -> bool {
        match self {
            Self::DistinctFrom if left_null || right_null => left_null != right_null,
            Self::NotDistinctFrom if left_null || right_null => left_null == right_null,
            _ if left_null || right_null => false,
            _ => {
                let o = ordering();
                match self {
                    Self::Equal | Self::NotDistinctFrom => o == Ordering::Equal,
                    Self::NotEqual | Self::DistinctFrom => o != Ordering::Equal,
                    Self::LessThan => o == Ordering::Less,
                    Self::LessThanOrEqual => o != Ordering::Greater,
                    Self::GreaterThan => o == Ordering::Greater,
                    Self::GreaterThanOrEqual => o != Ordering::Less,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_order() {
        assert!(f64::NAN.duck_eq(&f64::NAN));
        assert_eq!(f64::NAN.duck_cmp(&f64::INFINITY), Ordering::Greater);
        assert!((-0.0f64).duck_eq(&0.0));
        assert_eq!(1.0f32.duck_cmp(&2.0), Ordering::Less);
    }

    #[test]
    fn null_semantics() {
        let eq = || Ordering::Equal;
        assert!(!ComparisonType::Equal.evaluate(true, true, eq));
        assert!(ComparisonType::NotDistinctFrom.evaluate(true, true, eq));
        assert!(!ComparisonType::NotDistinctFrom.evaluate(true, false, eq));
        assert!(ComparisonType::DistinctFrom.evaluate(false, true, eq));
        assert!(!ComparisonType::DistinctFrom.evaluate(false, false, eq));
        assert!(ComparisonType::GreaterThanOrEqual.evaluate(false, false, eq));
    }
}
