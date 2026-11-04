// Ported from duckdb/src/function/cast/{bit_cast,bignum_casts,numeric_casts}.cpp and
// duckdb/src/common/operator/cast_operators.cpp @ rust-port-baseline
//
//! The casts of BIT and BIGNUM, row by row.

use super::cast::{CastMode, conversion_error};
use super::time_casts::null_cast;
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType, bignum, bit};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::Vector;
use duckrs_common::{Error, Result};

fn bit_value(bytes: Vec<u8>) -> Value {
    Value::new(LogicalType::new(LogicalTypeId::Bit), ValueData::Bytes(bytes)).expect("a BIT value")
}

fn bignum_value(bytes: Vec<u8>) -> Value {
    Value::new(LogicalType::new(LogicalTypeId::Bignum), ValueData::Bytes(bytes)).expect("a BIGNUM value")
}

fn bytes_of(value: &Value) -> &[u8] {
    match value.data() {
        ValueData::Bytes(bytes) => bytes,
        _ => unreachable!("stored as bytes"),
    }
}

/// `Bit::NumericToBit`: the big-endian bytes of a number.
fn numeric_bytes(value: &Value) -> Option<Vec<u8>> {
    Some(match value.data() {
        ValueData::Bool(v) => vec![u8::from(*v)],
        ValueData::Int8(v) => v.to_be_bytes().to_vec(),
        ValueData::Int16(v) => v.to_be_bytes().to_vec(),
        ValueData::Int32(v) => v.to_be_bytes().to_vec(),
        ValueData::Int64(v) => v.to_be_bytes().to_vec(),
        ValueData::Int128(v) => v.to_be_bytes().to_vec(),
        ValueData::UInt8(v) => v.to_be_bytes().to_vec(),
        ValueData::UInt16(v) => v.to_be_bytes().to_vec(),
        ValueData::UInt32(v) => v.to_be_bytes().to_vec(),
        ValueData::UInt64(v) => v.to_be_bytes().to_vec(),
        ValueData::UInt128(v) => v.to_be_bytes().to_vec(),
        ValueData::Float(v) => v.to_be_bytes().to_vec(),
        ValueData::Double(v) => v.to_be_bytes().to_vec(),
        _ => return None,
    })
}

/// `CastFromBitToNumeric`: the bits as the low bytes of the number.
fn bit_to_numeric(bits: &[u8], target: &LogicalType) -> Result<Value> {
    let physical = target.physical_type();
    let size = if physical == PhysicalType::Bool {
        1
    } else {
        physical.size()
    };
    let bytes = bit::bit_to_bytes(bits);
    if bytes.len() > size {
        // a BOOLEAN is read as a byte
        let name = if physical == PhysicalType::Bool {
            PhysicalType::UInt8
        } else {
            physical
        };
        return Err(conversion_error(format!("Bitstring doesn't fit inside of {name}")));
    }
    let mut buffer = [0u8; 16];
    buffer[16 - bytes.len()..].copy_from_slice(&bytes);
    let low = u128::from_be_bytes(buffer);
    Ok(match target.id() {
        LogicalTypeId::Boolean => Value::boolean(low > 0),
        LogicalTypeId::TinyInt => Value::tinyint(low as i8),
        LogicalTypeId::SmallInt => Value::smallint(low as i16),
        LogicalTypeId::Integer => Value::integer(low as i32),
        LogicalTypeId::BigInt => Value::bigint(low as i64),
        LogicalTypeId::HugeInt => Value::hugeint(low as i128),
        LogicalTypeId::UTinyInt => Value::utinyint(low as u8),
        LogicalTypeId::USmallInt => Value::usmallint(low as u16),
        LogicalTypeId::UInteger => Value::uinteger(low as u32),
        LogicalTypeId::UBigInt => Value::ubigint(low as u64),
        LogicalTypeId::UHugeInt => Value::uhugeint(low),
        LogicalTypeId::Float => Value::float(f32::from_bits(low as u32)),
        LogicalTypeId::Double => Value::double(f64::from_bits(low as u64)),
        other => unreachable!("{other:?} is not a number"),
    })
}

/// `BignumToIntErrorCast`.
fn bignum_to_integer(bytes: &[u8], target: &LogicalType) -> Result<Value> {
    let out_of_range = || {
        conversion_error(format!(
            "BIGNUM value {} is out of range for type {}",
            bignum::to_string(bytes),
            target.physical_type()
        ))
    };
    let (negative, magnitude) = bignum::to_u128_magnitude(bytes).ok_or_else(out_of_range)?;
    if target.id() == LogicalTypeId::UHugeInt {
        if negative && magnitude != 0 {
            return Err(out_of_range());
        }
        return Ok(Value::uhugeint(magnitude));
    }
    let value = if negative {
        0i128.checked_sub_unsigned(magnitude)
    } else {
        i128::try_from(magnitude).ok()
    }
    .ok_or_else(out_of_range)?;
    let (min, max) = super::cast_rules::integral_range(target.id());
    if !(min..=max).contains(&value) {
        return Err(out_of_range());
    }
    super::cast::integer_value(value, target)
}

fn number_type_error(value: &Value, target: &LogicalType) -> Error {
    conversion_error(format!(
        "Type {} with value {} can't be cast to the destination type {target}",
        value.logical_type().physical_type(),
        value.try_to_string().unwrap_or_default()
    ))
}

/// One non-NULL value; `None` if there is no such cast.
fn cast_value(value: &Value, target: &LogicalType) -> Option<Result<Value>> {
    use LogicalTypeId as L;
    let source = value.logical_type();
    Some(match (source.id(), target.id()) {
        (L::Varchar, L::Bit) => bit::string_to_bit(bytes_of(value))
            .map(bit_value)
            .map_err(conversion_error),
        (L::Blob, L::Bit) if bytes_of(value).is_empty() => {
            Err(conversion_error("Cannot cast empty BLOB to BIT".to_string()))
        }
        (L::Blob, L::Bit) => Ok(bit_value(bit::blob_to_bit(bytes_of(value)))),
        (L::Bit, L::Blob) => Ok(Value::blob(bit::bit_to_bytes(bytes_of(value)))),
        (L::Bit, _) if target.is_numeric() && target.id() != L::Decimal || target.id() == L::Boolean => {
            bit_to_numeric(bytes_of(value), target)
        }
        (_, L::Bit) if source.id() != L::Decimal => {
            let bytes = numeric_bytes(value)?;
            Ok(bit_value(bit::blob_to_bit(&bytes)))
        }
        (L::Varchar, L::Bignum) => bignum::from_str(bytes_of(value)).map(bignum_value).ok_or_else(|| {
            conversion_error(format!(
                "Could not convert string '{}' to {target}",
                String::from_utf8_lossy(bytes_of(value))
            ))
        }),
        (L::Float | L::Double, L::Bignum) => {
            let number = match value.data() {
                ValueData::Float(v) => f64::from(*v),
                ValueData::Double(v) => *v,
                _ => unreachable!("a floating point value"),
            };
            bignum::from_f64(number)
                .map(bignum_value)
                .ok_or_else(|| number_type_error(value, target))
        }
        (L::UHugeInt, L::Bignum) => match value.data() {
            ValueData::UInt128(v) => Ok(bignum_value(bignum::from_u128(*v))),
            _ => unreachable!("a UHUGEINT value"),
        },
        (_, L::Bignum) if source.is_integral() => Ok(bignum_value(bignum::from_i128(value.as_i128()?))),
        (L::Bignum, L::Double) => bignum::to_f64(bytes_of(value)).map(Value::double).ok_or_else(|| {
            conversion_error(format!(
                "Could not convert bignum '{}' to Double",
                bignum::to_string(bytes_of(value))
            ))
        }),
        (L::Bignum, _) if target.is_integral() => bignum_to_integer(bytes_of(value), target),
        _ => return None,
    })
}

/// Casts of BIT and BIGNUM, other than to VARCHAR.
pub(super) fn bit_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let (from, target) = (source.logical_type().clone(), result.logical_type().clone());
    let u = source.to_unified(count);
    let first = (0..count).find(|&i| u.validity.is_valid(u.sel.get(i)));
    let Some(first) = first else {
        *result = Vector::constant_null(target);
        return Ok(());
    };
    if cast_value(&source.get_value(first), &target).is_none() {
        debug_assert_ne!(from, target);
        return null_cast(source, result, count, mode);
    }
    result.prepare_flat(count.max(1));
    for i in 0..count {
        let value = source.get_value(i);
        if value.is_null() {
            result.set_null(i, true);
            continue;
        }
        match cast_value(&value, &target).expect("every value of a type casts alike") {
            Ok(cast) => result.set_value(i, &cast)?,
            Err(_) if mode == CastMode::Try => result.set_null(i, true),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
