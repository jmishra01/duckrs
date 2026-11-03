// Ported from duckdb/src/function/scalar/operator/arithmetic.cpp and
// duckdb/src/include/duckdb/common/operator/{add,subtract,multiply,negate}.hpp @ rust-port-baseline
//
//! The arithmetic operators `+`, `-` and `*` over the numeric types. Integer arithmetic checks
//! for overflow; floating point does not. DECIMAL, temporal and BIGNUM overloads come later.

use super::{ScalarFunction, ScalarFunctionSet};
use duckrs_common::executor;
use duckrs_common::string_cast::{format_double, format_float};
use duckrs_common::types::{LogicalType, NativeType, PhysicalType};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

/// A numeric type the operators work on.
trait Arithmetic: NativeType {
    fn add(self, other: Self) -> Option<Self>;
    fn sub(self, other: Self) -> Option<Self>;
    fn mul(self, other: Self) -> Option<Self>;
    fn neg(self) -> Option<Self>;
    /// `NumericHelper::ToString`, for overflow messages.
    fn render(self) -> String;
}

macro_rules! integer_arithmetic {
    ($($t:ty),*) => {$(
        impl Arithmetic for $t {
            fn add(self, other: Self) -> Option<Self> {
                self.checked_add(other)
            }
            fn sub(self, other: Self) -> Option<Self> {
                self.checked_sub(other)
            }
            fn mul(self, other: Self) -> Option<Self> {
                self.checked_mul(other)
            }
            fn neg(self) -> Option<Self> {
                (0 as $t).checked_sub(self)
            }
            fn render(self) -> String {
                self.to_string()
            }
        }
    )*};
}
integer_arithmetic!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

macro_rules! float_arithmetic {
    ($($t:ty => $render:ident),*) => {$(
        impl Arithmetic for $t {
            fn add(self, other: Self) -> Option<Self> {
                Some(self + other)
            }
            fn sub(self, other: Self) -> Option<Self> {
                Some(self - other)
            }
            fn mul(self, other: Self) -> Option<Self> {
                Some(self * other)
            }
            fn neg(self) -> Option<Self> {
                Some(-self)
            }
            fn render(self) -> String {
                $render(self)
            }
        }
    )*};
}
float_arithmetic!(f32 => format_float, f64 => format_double);

/// A binary operator: how it computes, and how its overflow error reads.
trait BinaryOperator {
    /// e.g. "addition", in `Overflow in addition of INT32 (1 + 2)!`
    const NAME: &'static str;
    const SYMBOL: &'static str;
    fn apply<T: Arithmetic>(a: T, b: T) -> Option<T>;
}

struct Add;
struct Subtract;
struct Multiply;

impl BinaryOperator for Add {
    const NAME: &'static str = "addition";
    const SYMBOL: &'static str = "+";
    fn apply<T: Arithmetic>(a: T, b: T) -> Option<T> {
        a.add(b)
    }
}

impl BinaryOperator for Subtract {
    const NAME: &'static str = "subtraction";
    const SYMBOL: &'static str = "-";
    fn apply<T: Arithmetic>(a: T, b: T) -> Option<T> {
        a.sub(b)
    }
}

impl BinaryOperator for Multiply {
    const NAME: &'static str = "multiplication";
    const SYMBOL: &'static str = "*";
    fn apply<T: Arithmetic>(a: T, b: T) -> Option<T> {
        a.mul(b)
    }
}

/// `ScalarFunction::BinaryFunction<T, T, T, OP>` with `*OperatorOverflowCheck`.
fn binary<T: Arithmetic, OP: BinaryOperator>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<T, T, T>(
        args.column(0),
        args.column(1),
        result,
        args.len(),
        |a, b| match OP::apply(a, b) {
            Some(r) => Ok(Some(r)),
            None => Err(Error::new(
                ErrorKind::OutOfRange,
                format!(
                    "Overflow in {} of {} ({} {} {})!",
                    OP::NAME,
                    T::PHYSICAL,
                    a.render(),
                    OP::SYMBOL,
                    b.render()
                ),
            )),
        },
    )
}

/// Unary `-` (`NegateOperator`).
fn negate<T: Arithmetic>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::unary_fallible::<T, T>(args.column(0), result, args.len(), |a| match a.neg() {
        Some(r) => Ok(Some(r)),
        None => Err(Error::new(
            ErrorKind::OutOfRange,
            "Overflow in negation of numeric value!",
        )),
    })
}

/// Unary `+`: the input itself.
fn identity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    *result = args.column(0).clone();
    Ok(())
}

/// The numeric types with arithmetic, in the order of `LogicalType::Numeric()` (DECIMAL left out).
fn numeric_types() -> [LogicalType; 12] {
    [
        LogicalType::TINYINT,
        LogicalType::SMALLINT,
        LogicalType::INTEGER,
        LogicalType::BIGINT,
        LogicalType::HUGEINT,
        LogicalType::FLOAT,
        LogicalType::DOUBLE,
        LogicalType::UTINYINT,
        LogicalType::USMALLINT,
        LogicalType::UINTEGER,
        LogicalType::UBIGINT,
        LogicalType::UHUGEINT,
    ]
}

macro_rules! dispatch {
    ($ty:expr, $f:ident $(::<$op:ty>)?) => {
        match $ty.physical_type() {
            PhysicalType::Int8 => $f::<i8 $(, $op)?> as super::ScalarCallback,
            PhysicalType::Int16 => $f::<i16 $(, $op)?>,
            PhysicalType::Int32 => $f::<i32 $(, $op)?>,
            PhysicalType::Int64 => $f::<i64 $(, $op)?>,
            PhysicalType::Int128 => $f::<i128 $(, $op)?>,
            PhysicalType::UInt8 => $f::<u8 $(, $op)?>,
            PhysicalType::UInt16 => $f::<u16 $(, $op)?>,
            PhysicalType::UInt32 => $f::<u32 $(, $op)?>,
            PhysicalType::UInt64 => $f::<u64 $(, $op)?>,
            PhysicalType::UInt128 => $f::<u128 $(, $op)?>,
            PhysicalType::Float => $f::<f32 $(, $op)?>,
            PhysicalType::Double => $f::<f64 $(, $op)?>,
            other => unreachable!("no arithmetic on {other}"),
        }
    };
}

/// `OperatorAddFun`, `OperatorSubtractFun` and `OperatorMultiplyFun` for the numeric types.
pub(super) fn arithmetic_operators() -> Vec<ScalarFunctionSet> {
    let mut add = ScalarFunctionSet::new("+");
    let mut subtract = ScalarFunctionSet::new("-");
    let mut multiply = ScalarFunctionSet::new("*");
    for ty in numeric_types() {
        // unary add function is a nop, but only exists for numeric types
        add.add(ScalarFunction::new("+", vec![ty.clone()], ty.clone(), identity));
        add.add(ScalarFunction::new(
            "+",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, binary::<Add>),
        ));
        // unary subtract function, negates the input
        subtract.add(ScalarFunction::new(
            "-",
            vec![ty.clone()],
            ty.clone(),
            dispatch!(ty, negate),
        ));
        subtract.add(ScalarFunction::new(
            "-",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, binary::<Subtract>),
        ));
        multiply.add(ScalarFunction::new(
            "*",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, binary::<Multiply>),
        ));
    }
    vec![add, subtract, multiply]
}
