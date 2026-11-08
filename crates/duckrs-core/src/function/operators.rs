// Ported from duckdb/src/function/scalar/operator/arithmetic.cpp and
// duckdb/src/include/duckdb/common/operator/{add,subtract,multiply,negate}.hpp @ rust-port-baseline
//
//! The arithmetic operators `+`, `-`, `*`, `/`, `//` and `%` over the numeric types. Integer
//! arithmetic checks for overflow; floating point does not. `/` is floating point, and division
//! by zero is an error for `//` and `%` over integers and DECIMAL (`error_on_division_by_zero`).
//! DECIMAL overloads work on the unscaled integers, and take their result type from the
//! arguments. Temporal and BIGNUM overloads come later.

use super::combine_types::decimal_properties;
use super::{ScalarFunction, ScalarFunctionSet};
use duckrs_common::executor;
use duckrs_common::string_cast::{format_double, format_float};
use duckrs_common::types::{LogicalType, LogicalTypeId, NativeType, PhysicalType, decimal};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

/// A numeric type the operators work on.
trait Arithmetic: NativeType {
    fn add(self, other: Self) -> Option<Self>;
    fn sub(self, other: Self) -> Option<Self>;
    fn mul(self, other: Self) -> Option<Self>;
    fn neg(self) -> Option<Self>;
    /// `TryAbsOperator`: `None` when the result does not fit.
    fn abs(self) -> Option<Self>;
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
                // `Hugeint::TryMultiply` accepts a product equal to the minimum
                self.checked_mul(other)
            }
            fn neg(self) -> Option<Self> {
                (0 as $t).checked_sub(self)
            }
            fn abs(self) -> Option<Self> {
                let zero: $t = 0;
                if self >= zero { Some(self) } else { self.neg() }
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
            fn abs(self) -> Option<Self> {
                Some(<$t>::abs(self))
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

/// What the division operators do to a value pair: `None` means a division by zero.
trait Division: Arithmetic {
    /// `//`, which for floating point is an ordinary division
    fn divide(self, other: Self) -> Result<Option<Self>>;
    /// `%`, which never fails for floating point (IEEE: `x % 0` is NaN)
    fn modulo(self, other: Self) -> Option<Self>;
    /// `/` exists for floating point only; integers cast to DOUBLE.
    fn float_divide(self, other: Self) -> Self;
}

macro_rules! integer_division {
    ($($t:ty),*) => {$(
        impl Division for $t {
            fn divide(self, other: Self) -> Result<Option<Self>> {
                if other == 0 {
                    return Ok(None);
                }
                match self.checked_div(other) {
                    Some(result) => Ok(Some(result)),
                    None => Err(Error::new(
                        ErrorKind::OutOfRange,
                        format!("Overflow in division of {self} / {other}"),
                    )),
                }
            }
            fn modulo(self, other: Self) -> Option<Self> {
                if other == 0 {
                    return None;
                }
                // the remainder of MIN / -1 is 0 - only the division itself overflows
                Some(self.checked_rem(other).unwrap_or(0))
            }
            fn float_divide(self, _: Self) -> Self {
                unreachable!("`/` is not defined for integers")
            }
        }
    )*};
}
integer_division!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

macro_rules! float_division {
    ($($t:ty),*) => {$(
        impl Division for $t {
            fn divide(self, other: Self) -> Result<Option<Self>> {
                Ok((other != 0.0).then(|| self / other))
            }
            fn modulo(self, other: Self) -> Option<Self> {
                Some(self % other)
            }
            fn float_divide(self, other: Self) -> Self {
                self / other
            }
        }
    )*};
}
float_division!(f32, f64);

/// The error of a division by zero. Its message names the expression, which the expression
/// executor fills in.
pub(crate) const DIVISION_BY_ZERO: &str = "Division by zero";

pub(super) fn division_by_zero() -> Error {
    Error::new(ErrorKind::InvalidInput, DIVISION_BY_ZERO)
}

/// `//`
fn integer_divide<T: Division>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<T, T, T>(args.column(0), args.column(1), result, args.len(), |a, b| {
        a.divide(b)?.map(Some).ok_or_else(division_by_zero)
    })
}

/// `%`
fn modulo<T: Division>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<T, T, T>(args.column(0), args.column(1), result, args.len(), |a, b| {
        a.modulo(b).map(Some).ok_or_else(division_by_zero)
    })
}

/// `/`
fn float_divide<T: Division>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary::<T, T, T>(args.column(0), args.column(1), result, args.len(), T::float_divide);
    Ok(())
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

/// `abs` (`TryAbsOperator`).
fn absolute<T: Arithmetic>(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::unary_fallible::<T, T>(args.column(0), result, args.len(), |a| match a.abs() {
        Some(r) => Ok(Some(r)),
        None => Err(Error::new(
            ErrorKind::OutOfRange,
            format!("Overflow on abs({})", a.render()),
        )),
    })
}

/// `isnan`, `isinf` and `isfinite`, per floating point type.
macro_rules! float_check {
    ($name:ident, $check:expr) => {
        fn $name(args: &DataChunk, result: &mut Vector) -> Result<()> {
            if args.column(0).logical_type().id() == LogicalTypeId::Float {
                let check: fn(f64) -> bool = $check;
                executor::unary_fallible::<f32, bool>(args.column(0), result, args.len(), |a| {
                    Ok(Some(check(f64::from(a))))
                })
            } else {
                let check: fn(f64) -> bool = $check;
                executor::unary_fallible::<f64, bool>(args.column(0), result, args.len(), |a| Ok(Some(check(a))))
            }
        }
    };
}
float_check!(is_nan, f64::is_nan);
float_check!(is_infinite, f64::is_infinite);
float_check!(is_finite, f64::is_finite);

/// Unary `+`: the input itself.
fn identity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    *result = args.column(0).clone();
    Ok(())
}

/// `LogicalType::Numeric()`.
fn numeric_types() -> [LogicalType; 13] {
    [
        LogicalType::TINYINT,
        LogicalType::SMALLINT,
        LogicalType::INTEGER,
        LogicalType::BIGINT,
        LogicalType::HUGEINT,
        LogicalType::FLOAT,
        LogicalType::DOUBLE,
        LogicalType::new(LogicalTypeId::Decimal),
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

/// The largest absolute value of a DECIMAL stored in a hugeint is `10^38 - 1`.
const DECIMAL_38_LIMIT: i128 = 10_i128.pow(decimal::MAX_WIDTH as u32);
/// The largest absolute value of a DECIMAL stored in a bigint.
const DECIMAL_18_MAX: i64 = 999_999_999_999_999_999;

fn in_decimal_38(value: &i128) -> bool {
    -DECIMAL_38_LIMIT < *value && *value < DECIMAL_38_LIMIT
}

/// `DecimalAddOverflowCheck`, which only runs over DECIMAL(38).
fn decimal_add_overflow_check(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<i128, i128, i128>(args.column(0), args.column(1), result, args.len(), |a, b| {
        match a.checked_add(b).filter(in_decimal_38) {
            Some(r) => Ok(Some(r)),
            None => Err(Error::new(
                ErrorKind::OutOfRange,
                format!("Overflow in addition of DECIMAL(38) ({a} + {b});"),
            )),
        }
    })
}

/// `DecimalSubtractOverflowCheck`, which only runs over DECIMAL(38).
fn decimal_subtract_overflow_check(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<i128, i128, i128>(args.column(0), args.column(1), result, args.len(), |a, b| {
        match a.checked_sub(b).filter(in_decimal_38) {
            Some(r) => Ok(Some(r)),
            None => Err(Error::new(
                ErrorKind::OutOfRange,
                format!("Overflow in subtract of DECIMAL(38) ({a} - {b});"),
            )),
        }
    })
}

/// `DecimalMultiplyOverflowCheck` over DECIMAL(18).
fn decimal_multiply_overflow_check_18(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<i64, i64, i64>(args.column(0), args.column(1), result, args.len(), |a, b| {
        match a.checked_mul(b).filter(|r| r.abs() <= DECIMAL_18_MAX) {
            Some(r) => Ok(Some(r)),
            None => Err(Error::new(
                ErrorKind::OutOfRange,
                format!(
                    "Overflow in multiplication of DECIMAL(18) ({a} * {b}). You might want to add an explicit cast \
                     to a bigger decimal."
                ),
            )),
        }
    })
}

/// `DecimalMultiplyOverflowCheck` over DECIMAL(38).
fn decimal_multiply_overflow_check_38(args: &DataChunk, result: &mut Vector) -> Result<()> {
    executor::binary_fallible::<i128, i128, i128>(args.column(0), args.column(1), result, args.len(), |a, b| {
        match a.checked_mul(b).filter(in_decimal_38) {
            Some(r) => Ok(Some(r)),
            None => Err(Error::new(
                ErrorKind::OutOfRange,
                format!(
                    "Overflow in multiplication of DECIMAL(38) ({a} * {b}). You might want to add an explicit cast \
                     to a decimal with a smaller scale."
                ),
            )),
        }
    })
}

/// The callback of a DECIMAL overload before its types are resolved.
fn unresolved_decimal(_: &DataChunk, _: &mut Vector) -> Result<()> {
    unreachable!("a DECIMAL overload runs after ResolveTypes")
}

/// `LogicalType::GetDecimalProperties` for the arguments of an arithmetic operator.
fn argument_decimal_properties(ty: &LogicalType) -> Result<(u8, u8)> {
    decimal_properties(ty).ok_or_else(|| {
        Error::new(
            ErrorKind::Binder,
            format!("Could not convert type {ty} to a decimal for the arithmetic operator - add an explicit cast"),
        )
    })
}

/// `DecimalArithmeticResultType`: the result type of a decimal addition, subtraction or modulo,
/// and whether the operation needs an overflow check.
fn decimal_arithmetic_result_type(input_types: &[LogicalType], is_modulo: bool) -> Result<(LogicalType, bool)> {
    // get the max width and scale of the input arguments
    let (mut max_width, mut max_scale, mut max_width_over_scale) = (0, 0, 0);
    for input_type in input_types {
        let (width, scale) = argument_decimal_properties(input_type)?;
        max_width = max_width.max(width);
        max_scale = max_scale.max(scale);
        max_width_over_scale = max_width_over_scale.max(width - scale);
    }
    let mut required_width = (max_scale + max_width_over_scale).max(max_width);
    if !is_modulo {
        // for addition/subtraction, we add 1 to the width to ensure we don't overflow
        required_width += 1;
    }
    if required_width > decimal::MAX_WIDTH {
        // target width does not fit in decimal at all: truncate the scale and perform overflow detection
        return Ok((LogicalType::decimal(decimal::MAX_WIDTH, max_scale), true));
    }
    Ok((LogicalType::decimal(required_width, max_scale), false))
}

/// `ResolveDecimalArithmeticTypes`: the parameter types of a decimal addition, subtraction or
/// modulo with result type `result_type`.
fn decimal_arithmetic_parameters(input_types: &[LogicalType], result_type: &LogicalType) -> Result<Vec<LogicalType>> {
    let (_, result_scale) = result_type.decimal_properties().expect("a DECIMAL result");
    input_types
        .iter()
        .map(|input_type| {
            // if the argument has a matching scale and internal type as the output type, no
            // casting is necessary
            let (_, scale) = argument_decimal_properties(input_type)?;
            Ok(
                if scale == result_scale && input_type.physical_type() == result_type.physical_type() {
                    input_type.clone()
                } else {
                    result_type.clone()
                },
            )
        })
        .collect()
}

/// `DecimalAddSubtractResolveTypes<AddOperator, DecimalAddOverflowCheck>`.
fn resolve_decimal_add(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let (result_type, check_overflow) = decimal_arithmetic_result_type(arguments, false)?;
    let parameters = decimal_arithmetic_parameters(arguments, &result_type)?;
    let callback = if check_overflow {
        decimal_add_overflow_check
    } else {
        dispatch!(result_type, binary::<Add>)
    };
    Ok(ScalarFunction::new(
        function.name.clone(),
        parameters,
        result_type,
        callback,
    ))
}

/// `DecimalAddSubtractResolveTypes<SubtractOperator, DecimalSubtractOverflowCheck, true>`.
fn resolve_decimal_subtract(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let (result_type, check_overflow) = decimal_arithmetic_result_type(arguments, false)?;
    let parameters = decimal_arithmetic_parameters(arguments, &result_type)?;
    let callback = if check_overflow {
        decimal_subtract_overflow_check
    } else {
        dispatch!(result_type, binary::<Subtract>)
    };
    Ok(ScalarFunction::new(
        function.name.clone(),
        parameters,
        result_type,
        callback,
    ))
}

/// `DecimalModuloResolveTypes`.
fn resolve_decimal_modulo(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let (result_type, check_overflow) = decimal_arithmetic_result_type(arguments, true)?;
    if check_overflow {
        // fallback to DOUBLE if the decimal type is not guaranteed to fit within the max decimal width
        return Ok(ScalarFunction::new(
            function.name.clone(),
            vec![LogicalType::DOUBLE; arguments.len()],
            LogicalType::DOUBLE,
            modulo::<f64>,
        ));
    }
    let parameters = decimal_arithmetic_parameters(arguments, &result_type)?;
    let callback = dispatch!(result_type, modulo);
    Ok(ScalarFunction::new(
        function.name.clone(),
        parameters,
        result_type,
        callback,
    ))
}

/// `DecimalMultiplyResultType`: the result type of a decimal multiplication, and whether it
/// needs an overflow check.
fn decimal_multiply_result_type(input_types: &[LogicalType]) -> Result<(LogicalType, bool)> {
    const MAX_WIDTH_INT64: u8 = 18;
    let mut check_overflow = false;
    let (mut result_width, mut result_scale, mut max_width) = (0_u32, 0_u32, 0);
    for input_type in input_types {
        let (width, scale) = argument_decimal_properties(input_type)?;
        max_width = max_width.max(width);
        result_width += u32::from(width);
        result_scale += u32::from(scale);
    }
    let max_decimal = u32::from(decimal::MAX_WIDTH);
    if result_scale > max_decimal {
        return Err(Error::new(
            ErrorKind::OutOfRange,
            format!(
                "Needed scale {result_scale} to accurately represent the multiplication result, but this is out of \
                 range of the DECIMAL type. Max scale is {max_decimal}; could not perform an accurate \
                 multiplication. Either add a cast to DOUBLE, or add an explicit cast to a decimal with a lower \
                 scale."
            ),
        ));
    }
    let max_int64 = u32::from(MAX_WIDTH_INT64);
    if result_width > max_int64 && max_width <= MAX_WIDTH_INT64 && result_scale < max_int64 {
        check_overflow = true;
        result_width = max_int64;
    }
    if result_width > max_decimal {
        check_overflow = true;
        result_width = max_decimal;
    }
    Ok((
        LogicalType::decimal(result_width as u8, result_scale as u8),
        check_overflow,
    ))
}

/// `DecimalMultiplyResolveTypes`.
fn resolve_decimal_multiply(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let (result_type, check_overflow) = decimal_multiply_result_type(arguments)?;
    let (result_width, _) = result_type.decimal_properties().expect("a DECIMAL result");
    // since our scale is the summation of our input scales, we do not need to cast to the result
    // scale; however, we might need to cast to the correct internal type
    let parameters = arguments
        .iter()
        .map(|argument| {
            if argument.physical_type() == result_type.physical_type() {
                return argument.clone();
            }
            let (_, scale) = decimal_properties(argument).unwrap_or((0, 0));
            LogicalType::decimal(result_width, scale)
        })
        .collect();
    let callback = match (check_overflow, result_type.physical_type()) {
        (true, PhysicalType::Int64) => decimal_multiply_overflow_check_18,
        (true, _) => decimal_multiply_overflow_check_38,
        (false, _) => dispatch!(result_type, binary::<Multiply>),
    };
    Ok(ScalarFunction::new(
        function.name.clone(),
        parameters,
        result_type,
        callback,
    ))
}

/// `NopDecimalResolveTypes`: unary `+` over a DECIMAL.
fn resolve_decimal_identity(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    Ok(ScalarFunction::new(
        function.name.clone(),
        arguments.to_vec(),
        arguments[0].clone(),
        identity,
    ))
}

/// `DecimalNegateResolveTypes`: unary `-` over a DECIMAL.
fn resolve_decimal_negate(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let callback = dispatch!(arguments[0], negate);
    Ok(ScalarFunction::new(
        function.name.clone(),
        arguments.to_vec(),
        arguments[0].clone(),
        callback,
    ))
}

/// `DecimalUnaryOpBind`: `abs` over a DECIMAL.
fn resolve_decimal_abs(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let callback = dispatch!(arguments[0], absolute);
    Ok(ScalarFunction::new(
        function.name.clone(),
        arguments.to_vec(),
        arguments[0].clone(),
        callback,
    ))
}

/// `ConcatOperator`: `||`, NULL if either side is.
fn concat(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let (mut left, mut right) = (args.column(0).clone(), args.column(1).clone());
    left.flatten(args.len());
    right.flatten(args.len());
    let mut buffer = Vec::new();
    for row in 0..args.len() {
        match (left.str_at(row), right.str_at(row)) {
            (Some(left), Some(right)) => {
                buffer.clear();
                buffer.extend_from_slice(left);
                buffer.extend_from_slice(right);
                result.set_str(row, &buffer);
            }
            _ => result.set_null(row, true),
        }
    }
    Ok(())
}

/// `ListConcatFunction` of the `||` operator: any NULL makes the result NULL.
fn concat_lists(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    super::bit_functions::strict_rows(args, result, |row| {
        let mut items = Vec::new();
        for list in row {
            if let ValueData::List(values) = list.data() {
                items.extend_from_slice(values);
            }
        }
        Value::new(ty.clone(), ValueData::List(items))
    })
}

/// `ResolveConcatTypesInternal`: blobs stay blobs, and anything else becomes a string.
fn resolve_concat(function: &ScalarFunction, arguments: &[LogicalType]) -> Result<ScalarFunction> {
    let nested = |id| matches!(id, LogicalTypeId::List | LogicalTypeId::Array);
    if arguments.iter().any(|ty| nested(ty.id())) {
        // `ResolveListConcatTypes`
        let mut element = LogicalType::SQLNULL;
        for argument in arguments {
            if argument.id() == LogicalTypeId::SqlNull {
                continue;
            }
            let Some(child) = argument.child_type().filter(|_| nested(argument.id())) else {
                let types: Vec<String> = arguments.iter().map(ToString::to_string).collect();
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!(
                        "Cannot concatenate types {} - an explicit cast is required",
                        types.join(" and ")
                    ),
                ));
            };
            element = super::combine_types::try_max_logical_type(&element, child).ok_or_else(|| {
                Error::new(
                    ErrorKind::Binder,
                    format!(
                        "Cannot concatenate lists of types {element}[] and {child}[] - an explicit cast is required"
                    ),
                )
            })?;
        }
        let ty = LogicalType::list(element);
        return Ok(ScalarFunction::new(
            function.name.clone(),
            vec![ty.clone(); arguments.len()],
            ty,
            concat_lists,
        ));
    }
    let all_blob = arguments.iter().all(|ty| ty.id() == LogicalTypeId::Blob);
    let ty = if all_blob {
        LogicalType::BLOB
    } else {
        LogicalType::VARCHAR
    };
    Ok(ScalarFunction::new(
        function.name.clone(),
        vec![ty.clone(); arguments.len()],
        ty,
        concat,
    ))
}

/// A DECIMAL overload of an operator, whose types and callback come from `resolve_types`.
fn decimal_overload(
    name: &str,
    arity: usize,
    ty: &LogicalType,
    resolve_types: super::ResolveTypesCallback,
) -> ScalarFunction {
    ScalarFunction::new(name, vec![ty.clone(); arity], ty.clone(), unresolved_decimal).with_resolve_types(resolve_types)
}

/// `OperatorAddFun`, `OperatorSubtractFun` and `OperatorMultiplyFun` for the numeric types.
pub(super) fn arithmetic_operators() -> Vec<ScalarFunctionSet> {
    let mut add = ScalarFunctionSet::new("+");
    let mut subtract = ScalarFunctionSet::new("-");
    let mut multiply = ScalarFunctionSet::new("*");
    let mut full_divide = ScalarFunctionSet::new("//");
    let mut remainder = ScalarFunctionSet::new("%");
    let mut fp_divide = ScalarFunctionSet::new("/");
    for ty in [LogicalType::FLOAT, LogicalType::DOUBLE] {
        fp_divide.add(ScalarFunction::new(
            "/",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, float_divide),
        ));
    }
    for ty in numeric_types() {
        if ty.id() == LogicalTypeId::Decimal {
            remainder.add(decimal_overload("%", 2, &ty, resolve_decimal_modulo));
            add.add(decimal_overload("+", 1, &ty, resolve_decimal_identity));
            add.add(decimal_overload("+", 2, &ty, resolve_decimal_add));
            subtract.add(decimal_overload("-", 1, &ty, resolve_decimal_negate));
            subtract.add(decimal_overload("-", 2, &ty, resolve_decimal_subtract));
            multiply.add(decimal_overload("*", 2, &ty, resolve_decimal_multiply));
            continue;
        }
        full_divide.add(ScalarFunction::new(
            "//",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, integer_divide),
        ));
        remainder.add(ScalarFunction::new(
            "%",
            vec![ty.clone(), ty.clone()],
            ty.clone(),
            dispatch!(ty, modulo),
        ));
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
    let mut abs = ScalarFunctionSet::new("abs");
    for ty in numeric_types() {
        if ty.id() == LogicalTypeId::Decimal {
            abs.add(decimal_overload("abs", 1, &ty, resolve_decimal_abs));
        } else {
            abs.add(ScalarFunction::new(
                "abs",
                vec![ty.clone()],
                ty.clone(),
                dispatch!(ty, absolute),
            ));
        }
    }
    let mut concat_operator = ScalarFunctionSet::new("||");
    concat_operator.add(
        ScalarFunction::new(
            "||",
            vec![LogicalType::new(LogicalTypeId::Any); 2],
            LogicalType::VARCHAR,
            unresolved_decimal,
        )
        .with_resolve_types(resolve_concat),
    );
    subtract.add(super::generic::interval_negation());
    for overload in super::bit_functions::bignum_operators() {
        match overload.name.as_str() {
            "+" => add.add(overload),
            "-" => subtract.add(overload),
            "*" => multiply.add(overload),
            "//" => full_divide.add(overload),
            _ => remainder.add(overload),
        }
    }
    let mut sets = vec![
        add,
        subtract,
        multiply,
        full_divide,
        remainder,
        fp_divide,
        abs,
        concat_operator,
    ];
    let checks: [(&str, super::ScalarCallback); 3] =
        [("isnan", is_nan), ("isinf", is_infinite), ("isfinite", is_finite)];
    for (name, check) in checks {
        let mut set = ScalarFunctionSet::new(name);
        for ty in [LogicalType::FLOAT, LogicalType::DOUBLE] {
            set.add(ScalarFunction::new(name, vec![ty], LogicalType::BOOLEAN, check));
        }
        // `IsInfiniteFun` and `IsFiniteFun` take DATE, TIMESTAMP and TIMESTAMPTZ too
        if name != "isnan" {
            let temporal: super::ScalarCallback = if name == "isinf" {
                temporal_is_infinite
            } else {
                temporal_is_finite
            };
            for ty in [
                LogicalType::DATE,
                LogicalType::TIMESTAMP,
                LogicalType::new(LogicalTypeId::TimestampTz),
            ] {
                set.add(ScalarFunction::new(name, vec![ty], LogicalType::BOOLEAN, temporal));
            }
        }
        sets.push(set);
    }
    sets
}

/// `Value::IsFinite` of a DATE or TIMESTAMP: not one of the infinities.
fn temporal_finite(value: &Value) -> bool {
    match value.data() {
        ValueData::Int32(days) => days.abs() != i32::MAX,
        ValueData::Int64(micros) => micros.abs() != i64::MAX,
        _ => true,
    }
}

fn temporal_is_finite(args: &DataChunk, result: &mut Vector) -> Result<()> {
    super::bit_functions::strict_rows(args, result, |row| Ok(Value::boolean(temporal_finite(&row[0]))))
}

fn temporal_is_infinite(args: &DataChunk, result: &mut Vector) -> Result<()> {
    super::bit_functions::strict_rows(args, result, |row| Ok(Value::boolean(!temporal_finite(&row[0]))))
}
