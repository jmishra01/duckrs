// Ported from duckdb/extension/core_functions/scalar/bit/bitstring.cpp,
// duckdb/extension/core_functions/scalar/operators/bitwise.cpp and
// duckdb/src/function/scalar/string/length.cpp @ rust-port-baseline
//
//! The functions and operators on BIT values, and the arithmetic of BIGNUM.

use super::nested::row_wise;
use super::operators::division_by_zero;
use super::{ScalarCallback, ScalarFunction, ScalarFunctionSet};
use duckrs_common::types::{LogicalType, LogicalTypeId, bignum, bit};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};

fn bit_type() -> LogicalType {
    LogicalType::new(LogicalTypeId::Bit)
}

fn bit_value(bytes: Vec<u8>) -> Value {
    Value::new(bit_type(), ValueData::Bytes(bytes)).expect("a BIT value")
}

fn bytes_of(value: &Value) -> &[u8] {
    match value.data() {
        ValueData::Bytes(bytes) => bytes,
        _ => unreachable!("stored as bytes"),
    }
}

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// Runs `f` on rows without NULLs; the others give NULL.
pub(super) fn strict_rows(args: &DataChunk, result: &mut Vector, f: impl Fn(&[Value]) -> Result<Value>) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row.iter().any(Value::is_null) {
            return Ok(Value::null(ty.clone()));
        }
        f(row)
    })
}

/// `BitCntOperator` over the width of the argument's type.
fn integer_bit_count(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let value = row[0].as_i128().expect("an integer");
        let bits = match row[0].logical_type().id() {
            LogicalTypeId::TinyInt => (value as i8).count_ones(),
            LogicalTypeId::SmallInt => (value as i16).count_ones(),
            LogicalTypeId::Integer => (value as i32).count_ones(),
            LogicalTypeId::BigInt => (value as i64).count_ones(),
            _ => value.count_ones(),
        };
        Ok(Value::tinyint(bits as i8))
    })
}

fn integer_of(value: &Value) -> i32 {
    value.as_i128().expect("an INTEGER argument") as i32
}

fn bit_index(input: &[u8], n: i32) -> Result<usize> {
    let length = bit::bit_length(input);
    if n < 0 || n as usize >= length {
        return Err(Error::new(
            ErrorKind::OutOfRange,
            format!("bit index {n} out of valid range (0..{})", length - 1),
        ));
    }
    Ok(n as usize)
}

fn bitstring(args: &DataChunk, result: &mut Vector, from_string: bool) -> Result<()> {
    strict_rows(args, result, |row| {
        let (input, n) = (bytes_of(&row[0]), integer_of(&row[1]));
        if n < 0 {
            return Err(invalid_input("The bitstring length cannot be negative"));
        }
        let n = n as usize;
        let bits: Vec<bool> = if from_string {
            if input.is_empty() {
                return Err(Error::new(ErrorKind::Conversion, "Cannot cast empty string to BIT"));
            }
            if let Some(c) = input.iter().find(|c| !matches!(c, b'0' | b'1')) {
                return Err(Error::new(
                    ErrorKind::Conversion,
                    format!(
                        "Invalid character encountered in string -> bit conversion: '{}'",
                        *c as char
                    ),
                ));
            }
            input.iter().map(|c| *c == b'1').collect()
        } else {
            (0..bit::bit_length(input)).map(|i| bit::get_bit(input, i)).collect()
        };
        if n < bits.len() {
            return Err(invalid_input("Length must be equal or larger than input string"));
        }
        let shift = n - bits.len();
        Ok(bit_value(bit::bits_to_bit(
            (0..n).map(|i| i >= shift && bits[i - shift]),
        )))
    })
}

fn bitstring_from_string(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bitstring(args, result, true)
}

fn bitstring_from_bit(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bitstring(args, result, false)
}

fn get_bit(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = bytes_of(&row[0]);
        let n = bit_index(input, integer_of(&row[1]))?;
        Ok(Value::integer(i32::from(bit::get_bit(input, n))))
    })
}

fn set_bit(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = bytes_of(&row[0]);
        let new_value = integer_of(&row[2]);
        if new_value != 0 && new_value != 1 {
            return Err(invalid_input("The new bit must be 1 or 0"));
        }
        let n = bit_index(input, integer_of(&row[1]))?;
        let length = bit::bit_length(input);
        Ok(bit_value(bit::bits_to_bit((0..length).map(|i| {
            if i == n { new_value == 1 } else { bit::get_bit(input, i) }
        }))))
    })
}

fn bit_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::bigint(bit::bit_length(bytes_of(&row[0])) as i64))
    })
}

fn octet_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::bigint(bytes_of(&row[0]).len() as i64 - 1))
    })
}

fn bit_count(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::bigint(bit::bit_count(bytes_of(&row[0])) as i64))
    })
}

fn bit_position(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (substring, input) = (bytes_of(&row[0]), bytes_of(&row[1]));
        if bit::bit_length(substring) == 0 {
            return Err(invalid_input("Substring must not be empty"));
        }
        Ok(Value::integer(bit::bit_position(substring, input) as i32))
    })
}

fn bitwise(args: &DataChunk, result: &mut Vector, name: &str, op: fn(u8, u8) -> u8) -> Result<()> {
    strict_rows(args, result, |row| {
        let (left, right) = (bytes_of(&row[0]), bytes_of(&row[1]));
        if bit::bit_length(left) != bit::bit_length(right) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Cannot {name} bit strings of different sizes"),
            ));
        }
        Ok(bit_value(bit::bitwise(left, right, op)))
    })
}

fn bit_and(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bitwise(args, result, "AND", |a, b| a & b)
}

fn bit_or(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bitwise(args, result, "OR", |a, b| a | b)
}

fn bit_xor(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bitwise(args, result, "XOR", |a, b| a ^ b)
}

fn bit_not(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = bytes_of(&row[0]);
        Ok(bit_value(bit::bits_to_bit(
            (0..bit::bit_length(input)).map(|i| !bit::get_bit(input, i)),
        )))
    })
}

fn shifted(input: &[u8], shift: i64) -> Vec<u8> {
    let length = bit::bit_length(input);
    bit::bits_to_bit((0..length).map(|i| {
        let source = i as i64 + shift;
        (0..length as i64).contains(&source) && bit::get_bit(input, source as usize)
    }))
}

fn shift_left(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let shift = integer_of(&row[1]);
        if shift < 0 {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                format!("Cannot left-shift by negative number {shift}"),
            ));
        }
        Ok(bit_value(shifted(bytes_of(&row[0]), i64::from(shift))))
    })
}

fn shift_right(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let (input, shift) = (bytes_of(&row[0]), integer_of(&row[1]));
        let length = bit::bit_length(input) as i64;
        // a negative shift clears the string
        let shift = if shift < 0 { length } else { i64::from(shift) };
        Ok(bit_value(shifted(input, -shift)))
    })
}

/// `bitstring_byte_comparable`: bytes that order like the bits.
fn byte_comparable(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        let input = bytes_of(&row[0]);
        Ok(Value::blob(bit::byte_comparable(input)))
    })
}

fn bignum_type() -> LogicalType {
    LogicalType::new(LogicalTypeId::Bignum)
}

fn bignum_value(bytes: Vec<u8>) -> Value {
    Value::new(bignum_type(), ValueData::Bytes(bytes)).expect("a BIGNUM value")
}

fn out_of_range(message: String) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

/// An operation of `bignum`: the result, or the message of the error.
type BignumOperation = fn(&[u8], &[u8]) -> std::result::Result<Vec<u8>, String>;

fn bignum_binary(args: &DataChunk, result: &mut Vector, op: BignumOperation) -> Result<()> {
    strict_rows(args, result, |row| {
        op(bytes_of(&row[0]), bytes_of(&row[1]))
            .map(bignum_value)
            .map_err(out_of_range)
    })
}

fn bignum_add(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bignum_binary(args, result, bignum::add)
}

fn bignum_subtract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bignum_binary(args, result, bignum::subtract)
}

fn bignum_multiply(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bignum_binary(args, result, bignum::multiply)
}

fn bignum_negate(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(bignum_value(bignum::negate(bytes_of(&row[0])))))
}

fn bignum_identity(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| Ok(row[0].clone()))
}

fn bignum_division(args: &DataChunk, result: &mut Vector, remainder: bool) -> Result<()> {
    strict_rows(args, result, |row| {
        let (quotient, rest) = bignum::div_mod(bytes_of(&row[0]), bytes_of(&row[1])).ok_or_else(division_by_zero)?;
        Ok(bignum_value(if remainder { rest } else { quotient }))
    })
}

fn bignum_divide(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bignum_division(args, result, false)
}

fn bignum_modulo(args: &DataChunk, result: &mut Vector) -> Result<()> {
    bignum_division(args, result, true)
}

/// The overloads of the arithmetic operators for BIGNUM, for the sets of `+`, `-`, `*`, `//` and
/// `%`.
pub(super) fn bignum_operators() -> [ScalarFunction; 7] {
    let ty = bignum_type();
    let binary = |name: &str, callback: ScalarCallback| {
        ScalarFunction::new(name, vec![ty.clone(), ty.clone()], ty.clone(), callback)
    };
    let unary =
        |name: &str, callback: ScalarCallback| ScalarFunction::new(name, vec![ty.clone()], ty.clone(), callback);
    [
        binary("+", bignum_add),
        unary("+", bignum_identity),
        binary("-", bignum_subtract),
        unary("-", bignum_negate),
        binary("*", bignum_multiply),
        binary("//", bignum_divide),
        binary("%", bignum_modulo),
    ]
}

/// A function's name, parameters, return type and callback.
type Definition = (&'static str, Vec<LogicalType>, LogicalType, ScalarCallback);

/// The functions on BIT.
pub(super) fn bit_functions() -> Vec<ScalarFunctionSet> {
    let (bit, integer, bigint, varchar) = (
        bit_type(),
        LogicalType::INTEGER,
        LogicalType::BIGINT,
        LogicalType::VARCHAR,
    );
    let definitions: Vec<Definition> = vec![
        (
            "bitstring",
            vec![varchar, integer.clone()],
            bit.clone(),
            bitstring_from_string,
        ),
        (
            "bitstring",
            vec![bit.clone(), integer.clone()],
            bit.clone(),
            bitstring_from_bit,
        ),
        ("get_bit", vec![bit.clone(), integer.clone()], integer.clone(), get_bit),
        (
            "set_bit",
            vec![bit.clone(), integer.clone(), integer.clone()],
            bit.clone(),
            set_bit,
        ),
        ("bit_length", vec![bit.clone()], bigint.clone(), bit_length),
        ("octet_length", vec![bit.clone()], bigint.clone(), octet_length),
        ("bit_count", vec![bit.clone()], bigint, bit_count),
        (
            "bit_count",
            vec![LogicalType::TINYINT],
            LogicalType::TINYINT,
            integer_bit_count,
        ),
        (
            "bit_count",
            vec![LogicalType::SMALLINT],
            LogicalType::TINYINT,
            integer_bit_count,
        ),
        (
            "bit_count",
            vec![LogicalType::INTEGER],
            LogicalType::TINYINT,
            integer_bit_count,
        ),
        (
            "bit_count",
            vec![LogicalType::BIGINT],
            LogicalType::TINYINT,
            integer_bit_count,
        ),
        (
            "bit_count",
            vec![LogicalType::HUGEINT],
            LogicalType::TINYINT,
            integer_bit_count,
        ),
        (
            "bit_position",
            vec![bit.clone(), bit.clone()],
            integer.clone(),
            bit_position,
        ),
        (
            "bitstring_byte_comparable",
            vec![bit.clone()],
            LogicalType::BLOB,
            byte_comparable,
        ),
        ("&", vec![bit.clone(), bit.clone()], bit.clone(), bit_and),
        ("|", vec![bit.clone(), bit.clone()], bit.clone(), bit_or),
        ("xor", vec![bit.clone(), bit.clone()], bit.clone(), bit_xor),
        ("~", vec![bit.clone()], bit.clone(), bit_not),
        ("<<", vec![bit.clone(), integer.clone()], bit.clone(), shift_left),
        (">>", vec![bit.clone(), integer], bit.clone(), shift_right),
    ];
    let mut sets: Vec<ScalarFunctionSet> = Vec::new();
    for (name, arguments, return_type, callback) in definitions {
        if sets.last().is_none_or(|set| set.name != name) {
            sets.push(ScalarFunctionSet::new(name));
        }
        sets.last_mut()
            .expect("just pushed")
            .add(ScalarFunction::new(name, arguments, return_type, callback));
    }
    sets
}
