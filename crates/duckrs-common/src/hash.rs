// Ported from duckdb/src/common/types/hash.cpp and common/vector_operations/vector_hash.cpp
// @ rust-port-baseline
//
// Hash values must match DuckDB bit for bit: they decide hash-table and partition placement,
// and `hash()` exposes them to SQL.

use crate::types::{Interval, LogicalType, PhysicalType, TimeTz};
use crate::vector::{Vector, VectorType};
use crate::{Error, Result};

/// The hash of a NULL value (`HashOp::NULL_HASH`).
pub const NULL_HASH: u64 = 0xbf58_476d_1ce4_e5b9;
/// The hash of a row of an empty STRUCT.
const EMPTY_STRUCT_HASH: u64 = 0x9e37_79b9_7f4a_7c15;
const MULTIPLIER: u64 = 0xd6e8_feb8_6659_fd93;

/// `MurmurHash64`: a 64-bit finalizer with good avalanche behavior.
#[inline]
pub fn murmur64(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^= x >> 32;
    x
}

/// Hashes a byte string (`HashBytes`).
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xe17a_1465 ^ (bytes.len() as u64).wrapping_mul(0xc6a4_a793_5bd1_e995);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        h ^= u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
        h = h.wrapping_mul(MULTIPLIER);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut block = [0u8; 8];
        block[..rest.len()].copy_from_slice(rest);
        h ^= u64::from_le_bytes(block);
        h = h.wrapping_mul(MULTIPLIER);
    }
    murmur64(h)
}

/// Mixes the hash of another column into a row hash (`CombineHashScalar`).
#[inline]
pub fn combine_hash_scalar(a: u64, b: u64) -> u64 {
    let a = (a ^ (a >> 32)).wrapping_mul(MULTIPLIER);
    a ^ b
}

/// DuckDB's `Hash<T>` for a stored value.
pub trait HashValue: Copy {
    fn hash_value(self) -> u64;
}

macro_rules! hash_via_u32 {
    ($($t:ty),*) => {$(
        impl HashValue for $t {
            /// The generic `Hash<T>`: `MurmurHash32(static_cast<uint32_t>(value))`.
            #[inline]
            fn hash_value(self) -> u64 {
                murmur64(u64::from(self as u32))
            }
        }
    )*};
}
hash_via_u32!(i8, i16, i32, u8, u16, u32);

impl HashValue for bool {
    #[inline]
    fn hash_value(self) -> u64 {
        murmur64(u64::from(self))
    }
}

impl HashValue for i64 {
    #[inline]
    fn hash_value(self) -> u64 {
        murmur64(self as u64)
    }
}

impl HashValue for u64 {
    #[inline]
    fn hash_value(self) -> u64 {
        murmur64(self)
    }
}

impl HashValue for i128 {
    #[inline]
    fn hash_value(self) -> u64 {
        murmur64(self as u64) ^ murmur64((self >> 64) as u64)
    }
}

impl HashValue for u128 {
    #[inline]
    fn hash_value(self) -> u64 {
        murmur64(self as u64) ^ murmur64((self >> 64) as u64)
    }
}

impl HashValue for f32 {
    /// `-0.0` hashes like `0.0`, and every NaN like the canonical quiet NaN, so values that
    /// compare equal hash equally.
    #[inline]
    fn hash_value(self) -> u64 {
        let v = if self == 0.0 {
            0.0
        } else if self.is_nan() {
            f32::NAN
        } else {
            self
        };
        murmur64(u64::from(v.to_bits()))
    }
}

impl HashValue for f64 {
    #[inline]
    fn hash_value(self) -> u64 {
        let v = if self == 0.0 {
            0.0
        } else if self.is_nan() {
            f64::NAN
        } else {
            self
        };
        murmur64(v.to_bits())
    }
}

impl HashValue for Interval {
    /// Hashes the normalized form, so equal intervals (`30 days`, `1 month`) hash equally.
    fn hash_value(self) -> u64 {
        let (months, days, micros) = self.normalize();
        days.hash_value() ^ months.hash_value() ^ micros.hash_value()
    }
}

impl HashValue for TimeTz {
    fn hash_value(self) -> u64 {
        self.0.hash_value()
    }
}

/// Hashes rows `0..count` of `input` into a UBIGINT vector (`VectorOperations::Hash`). NULLs
/// hash to [`NULL_HASH`]. A constant input gives a constant result.
pub fn hash_vector(input: &Vector, count: usize) -> Result<Vector> {
    let mut hashes = Vector::new(LogicalType::UBIGINT, count.max(1));
    if input.physical_type() == PhysicalType::Struct {
        let mut flat = input.clone();
        flat.flatten(count);
        let children = flat.struct_children();
        let Some((first, rest)) = children.split_first() else {
            hashes.data_mut::<u64>()[0] = EMPTY_STRUCT_HASH;
            hashes.set_constant();
            return Ok(hashes);
        };
        hashes = hash_vector(first, count)?;
        for child in rest {
            combine_hash(&mut hashes, child, count)?;
        }
        return Ok(hashes);
    }
    let constant = input.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    let values = row_hashes(input, rows)?;
    hashes.data_mut::<u64>()[..rows].copy_from_slice(&values);
    if constant {
        hashes.set_constant();
    }
    Ok(hashes)
}

/// Mixes the hashes of `input` into `hashes` row by row (`VectorOperations::CombineHash`).
pub fn combine_hash(hashes: &mut Vector, input: &Vector, count: usize) -> Result<()> {
    if input.physical_type() == PhysicalType::Struct {
        let mut flat = input.clone();
        flat.flatten(count);
        for child in flat.struct_children() {
            combine_hash(hashes, child, count)?;
        }
        return Ok(());
    }
    if hashes.vector_type() == VectorType::Constant && input.vector_type() == VectorType::Constant {
        let other = row_hashes(input, 1)?[0];
        let h = &mut hashes.data_mut::<u64>()[0];
        *h = combine_hash_scalar(*h, other);
        return Ok(());
    }
    hashes.flatten(count);
    let other = row_hashes(input, count)?;
    for (h, o) in hashes.data_mut::<u64>()[..count].iter_mut().zip(other) {
        *h = combine_hash_scalar(*h, o);
    }
    Ok(())
}

/// The hash of each of the first `rows` rows of a non-nested vector.
fn row_hashes(input: &Vector, rows: usize) -> Result<Vec<u64>> {
    let u = input.to_unified(rows);
    let mut out = Vec::with_capacity(rows);
    macro_rules! hash_rows {
        ($t:ty) => {{
            let data = u.data::<$t>();
            for i in 0..rows {
                let idx = u.sel.get(i);
                out.push(if u.validity.is_valid(idx) {
                    data[idx].hash_value()
                } else {
                    NULL_HASH
                });
            }
        }};
    }
    match input.physical_type() {
        PhysicalType::Bool => hash_rows!(bool),
        PhysicalType::Int8 => hash_rows!(i8),
        PhysicalType::Int16 => hash_rows!(i16),
        PhysicalType::Int32 => hash_rows!(i32),
        PhysicalType::Int64 => hash_rows!(i64),
        PhysicalType::Int128 => hash_rows!(i128),
        PhysicalType::UInt8 => hash_rows!(u8),
        PhysicalType::UInt16 => hash_rows!(u16),
        PhysicalType::UInt32 => hash_rows!(u32),
        PhysicalType::UInt64 => hash_rows!(u64),
        PhysicalType::UInt128 => hash_rows!(u128),
        PhysicalType::Float => hash_rows!(f32),
        PhysicalType::Double => hash_rows!(f64),
        PhysicalType::Interval => hash_rows!(Interval),
        PhysicalType::Varchar => {
            for i in 0..rows {
                out.push(if u.is_valid(i) {
                    hash_bytes(u.str_at(i))
                } else {
                    NULL_HASH
                });
            }
        }
        other => {
            return Err(Error::not_implemented(format!(
                "hashing {other} vectors ({}) is not supported yet",
                input.logical_type()
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Date;
    use crate::value::Value;

    /// Expected values come from `SELECT hash(x)` in DuckDB.
    #[test]
    fn scalar_hashes_match_duckdb() {
        assert_eq!(42i32.hash_value(), 7_199_933_130_570_745_587);
        assert_eq!(42i64.hash_value(), 7_199_933_130_570_745_587);
        assert_eq!((-1i8).hash_value(), 4_739_667_815_145_166_545);
        assert_eq!(true.hash_value(), 4_717_996_019_076_358_352);
        assert_eq!(hash_bytes(b"hello"), 8_628_213_525_773_890_682);
        assert_eq!(hash_bytes(b""), 5_104_928_228_550_385_088);
        assert_eq!(hash_bytes(b"exactly12chr"), 11_925_296_942_996_445_559);
        assert_eq!(hash_bytes(b"a longer string value!"), 10_220_128_699_376_362_924);
        assert_eq!(1.5f64.hash_value(), 1_706_605_666_616_485_939);
        assert_eq!((-0.0f64).hash_value(), 0);
        assert_eq!(0.0f64.hash_value(), 0);
        assert_eq!(1.5f32.hash_value(), 877_323_241_837_685_928);
        assert_eq!(42i128.hash_value(), 7_199_933_130_570_745_587);
        assert_eq!((-42i128).hash_value(), 10_453_212_082_686_659_020);
        assert_eq!(42u128.hash_value(), 7_199_933_130_570_745_587);
        assert_eq!(Interval::new(0, 1, 0).hash_value(), 4_717_996_019_076_358_352);
        assert_eq!(
            Interval::new(0, 0, 24 * 3_600_000_000).hash_value(),
            4_717_996_019_076_358_352
        );
        assert_eq!(Date(8035).0.hash_value(), 10_463_151_129_187_366_419);
        assert_eq!(42u64.hash_value(), 7_199_933_130_570_745_587);
        assert_eq!(f64::NAN.hash_value(), (-f64::NAN).hash_value());
        assert_eq!(NULL_HASH, 13_787_848_793_156_543_929);
        let tz = TimeTz::new(crate::types::Time::from_hms(10, 11, 12, 0), 5 * 3600 + 30 * 60);
        assert_eq!(tz.hash_value(), 4_612_889_634_841_790_229);
    }

    #[test]
    fn vector_hashes() {
        let mut v = Vector::new(LogicalType::INTEGER, 3);
        v.set_value(0, &Value::integer(42)).unwrap();
        v.set_null(1, true);
        v.set_value(2, &Value::integer(42)).unwrap();
        let h = hash_vector(&v, 3).unwrap();
        assert_eq!(
            h.data::<u64>()[..3],
            [7_199_933_130_570_745_587, NULL_HASH, 7_199_933_130_570_745_587]
        );

        // hash(42::INTEGER, 'hello') in DuckDB.
        let mut combined = Vector::constant(&Value::integer(42));
        combined = hash_vector(&combined, 1).unwrap();
        combine_hash(&mut combined, &Vector::constant(&Value::varchar("hello")), 1).unwrap();
        assert_eq!(combined.vector_type(), VectorType::Constant);
        assert_eq!(combined.data::<u64>()[0], 17_001_236_256_828_606_196);

        // A flat column combined with a constant one.
        let mut flat = hash_vector(&v, 3).unwrap();
        combine_hash(&mut flat, &Vector::constant(&Value::varchar("hello")), 3).unwrap();
        assert_eq!(flat.data::<u64>()[0], 17_001_236_256_828_606_196);
    }

    #[test]
    fn struct_hash_combines_children() {
        let s = Value::struct_value(vec![
            ("a".into(), Value::integer(42)),
            ("b".into(), Value::varchar("hello")),
        ]);
        let v = Vector::constant(&s);
        let h = hash_vector(&v, 1).unwrap();
        assert_eq!(h.get_value(0), Value::ubigint(17_001_236_256_828_606_196));
    }

    #[test]
    fn lists_are_not_supported_yet() {
        let v = Vector::new(LogicalType::list(LogicalType::INTEGER), 1);
        assert!(hash_vector(&v, 1).is_err());
    }
}
