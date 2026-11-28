// Ported from duckdb/src/execution/index/art/art_key.cpp and duckdb/src/include/duckdb/common/radix.hpp
// @ rust-port-baseline
//
//! What an ART sorts by: values as bytes that compare, one by one, as the values do.

use pintail_common::types::{NativeType, PhysicalType};
use pintail_common::vector::{UnifiedFormat, Vector};
use pintail_common::{Error, ErrorKind, Result};

/// `Radix::EncodeData`: writes a value in the order of its bytes.
pub trait Radix: Copy {
    fn encode(self, out: &mut Vec<u8>);
}

/// Big endian, as the first byte has to count the most.
macro_rules! radix_unsigned {
    ($($t:ty),*) => {$(
        impl Radix for $t {
            fn encode(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_be_bytes());
            }
        }
    )*};
}
radix_unsigned!(u8, u16, u32, u64);

/// `Radix::EncodeSigned`: with the sign flipped, as negative numbers come first.
macro_rules! radix_signed {
    ($($t:ty),*) => {$(
        impl Radix for $t {
            fn encode(self, out: &mut Vec<u8>) {
                let start = out.len();
                out.extend_from_slice(&self.to_be_bytes());
                out[start] ^= 0x80;
            }
        }
    )*};
}
radix_signed!(i8, i16, i32, i64);

impl Radix for bool {
    fn encode(self, out: &mut Vec<u8>) {
        out.push(u8::from(self));
    }
}

/// `hugeint_t`: its upper half is signed and its lower half is not.
impl Radix for i128 {
    fn encode(self, out: &mut Vec<u8>) {
        ((self >> 64) as i64).encode(out);
        (self as u64).encode(out);
    }
}

impl Radix for u128 {
    fn encode(self, out: &mut Vec<u8>) {
        ((self >> 64) as u64).encode(out);
        (self as u64).encode(out);
    }
}

/// `Radix::EncodeFloat`: zeros of both signs are one key, and NaN comes last.
impl Radix for f32 {
    fn encode(self, out: &mut Vec<u8>) {
        const SIGN: u32 = 1 << 31;
        let encoded = if self == 0.0 {
            SIGN
        } else if self.is_nan() {
            u32::MAX
        } else if self == f32::INFINITY {
            u32::MAX - 1
        } else if self == f32::NEG_INFINITY {
            0
        } else if self.to_bits() & SIGN == 0 {
            self.to_bits() | SIGN
        } else {
            !self.to_bits()
        };
        encoded.encode(out);
    }
}

/// `Radix::EncodeDouble`.
impl Radix for f64 {
    fn encode(self, out: &mut Vec<u8>) {
        const SIGN: u64 = 1 << 63;
        let encoded = if self == 0.0 {
            SIGN
        } else if self.is_nan() {
            u64::MAX
        } else if self == f64::INFINITY {
            u64::MAX - 1
        } else if self == f64::NEG_INFINITY {
            0
        } else if self.to_bits() & SIGN == 0 {
            self.to_bits() | SIGN
        } else {
            !self.to_bits()
        };
        encoded.encode(out);
    }
}

/// `ARTKey`. Compares as its bytes do, and a key of several columns is their keys in a row.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtKey {
    data: Vec<u8>,
}

impl ArtKey {
    pub fn of<T: Radix>(value: T) -> Self {
        let mut data = Vec::with_capacity(size_of::<T>());
        value.encode(&mut data);
        Self { data }
    }

    /// `CreateARTKey<string_t>`: `\0` ends a string, so that a string sorts before what extends it,
    /// and what could be taken for that is escaped.
    pub fn of_string(value: &[u8]) -> Self {
        const ESCAPE: u8 = 1;
        let mut data = Vec::with_capacity(value.len() + 1);
        for &byte in value {
            if byte <= ESCAPE {
                data.push(ESCAPE);
            }
            data.push(byte);
        }
        data.push(0);
        Self { data }
    }

    /// `ARTKey::Concat`.
    pub fn concat(&mut self, other: &ArtKey) {
        self.data.extend_from_slice(&other.data);
    }

    /// `ARTKey::GetRowId`: of a key that is just a row id.
    pub fn row_id(&self) -> Option<i64> {
        let bytes: [u8; 8] = self.data.as_slice().try_into().ok()?;
        Some(i64::from_be_bytes(bytes) ^ i64::MIN)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// A key of these bytes, made elsewhere (the collated keys of an upsert).
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The bytes of the key.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// `ARTKey::GetMismatchPos`: where they first differ, from `start`.
    pub fn mismatch_position(&self, other: &ArtKey, start: usize) -> usize {
        let (left, right) = (&self.data[start..], &other.data[start..]);
        let same = left.iter().zip(right).take_while(|(a, b)| a == b).count();
        start + same
    }
}

/// The key of each row of `columns`: `None` for a row with a NULL, which isn't in an index
/// (`GenerateKeysInternal`). A key of several columns is theirs one after another.
pub fn chunk_keys(columns: &[&Vector], count: usize) -> Result<Vec<Option<ArtKey>>> {
    fn add<T: NativeType + Radix>(format: &UnifiedFormat, keys: &mut [Option<ArtKey>]) {
        let data = format.data::<T>();
        for (row, key) in keys.iter_mut().enumerate() {
            if format.is_valid(row) {
                if let Some(key) = key {
                    data[format.sel.get(row)].encode(&mut key.data);
                }
            } else {
                *key = None;
            }
        }
    }
    let mut keys = vec![Some(ArtKey::default()); count];
    for column in columns {
        let format = column.to_unified(count);
        match column.physical_type() {
            PhysicalType::Bool => add::<bool>(&format, &mut keys),
            PhysicalType::Int8 => add::<i8>(&format, &mut keys),
            PhysicalType::Int16 => add::<i16>(&format, &mut keys),
            PhysicalType::Int32 => add::<i32>(&format, &mut keys),
            PhysicalType::Int64 => add::<i64>(&format, &mut keys),
            PhysicalType::Int128 => add::<i128>(&format, &mut keys),
            PhysicalType::UInt8 => add::<u8>(&format, &mut keys),
            PhysicalType::UInt16 => add::<u16>(&format, &mut keys),
            PhysicalType::UInt32 => add::<u32>(&format, &mut keys),
            PhysicalType::UInt64 => add::<u64>(&format, &mut keys),
            PhysicalType::UInt128 => add::<u128>(&format, &mut keys),
            PhysicalType::Float => add::<f32>(&format, &mut keys),
            PhysicalType::Double => add::<f64>(&format, &mut keys),
            PhysicalType::Varchar => {
                for (row, key) in keys.iter_mut().enumerate() {
                    if format.is_valid(row) {
                        if let Some(key) = key {
                            key.concat(&ArtKey::of_string(format.str_at(row)));
                        }
                    } else {
                        *key = None;
                    }
                }
            }
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidType,
                    format!("Invalid Type [{}]: Invalid type for index key.", other.name()),
                ));
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn integers_are_big_endian_with_the_sign_flipped() {
        assert_eq!(ArtKey::of(1i32).as_bytes(), [0x80, 0, 0, 1]);
        assert_eq!(ArtKey::of(-1i32).as_bytes(), [0x7f, 0xff, 0xff, 0xff]);
        assert_eq!(ArtKey::of(1u16).as_bytes(), [0, 1]);
        assert_eq!(ArtKey::of(true).as_bytes(), [1]);
    }

    #[test]
    fn floats_have_one_zero_and_nan_comes_last() {
        assert_eq!(ArtKey::of(0.0f64), ArtKey::of(-0.0f64));
        assert_eq!(ArtKey::of(0.0f32).as_bytes(), [0x80, 0, 0, 0]);
        let ordered = [f64::NEG_INFINITY, -1.5, -0.0, 1.5, f64::INFINITY, f64::NAN];
        assert!(ordered.windows(2).all(|pair| ArtKey::of(pair[0]) < ArtKey::of(pair[1])));
        assert_eq!(ArtKey::of(f32::NAN).as_bytes(), [0xff; 4]);
    }

    #[test]
    fn strings_end_in_a_zero_and_escape_what_looks_like_it() {
        assert_eq!(ArtKey::of_string(b"ab").as_bytes(), b"ab\0");
        assert_eq!(ArtKey::of_string(b"a\0b\x01").as_bytes(), b"a\x01\0b\x01\x01\0");
        assert!(ArtKey::of_string(b"a") < ArtKey::of_string(b"a\0"));
        assert!(ArtKey::of_string(b"a\0") < ArtKey::of_string(b"ab"));
    }

    #[test]
    fn a_row_id_comes_back() {
        for row in [0, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(ArtKey::of(row).row_id(), Some(row));
        }
        assert_eq!(ArtKey::of(1i32).row_id(), None);
    }

    #[test]
    fn keys_of_several_columns_are_in_a_row() {
        let mut key = ArtKey::of_string(b"a");
        key.concat(&ArtKey::of(1u8));
        assert_eq!(key.as_bytes(), b"a\0\x01");
        assert_eq!(key.mismatch_position(&ArtKey::of_string(b"ab"), 0), 1);
    }

    proptest! {
        #[test]
        fn integers_keep_their_order(a: i64, b: i64) {
            prop_assert_eq!(a.cmp(&b), ArtKey::of(a).cmp(&ArtKey::of(b)));
        }

        #[test]
        fn hugeints_keep_their_order(a: i128, b: i128) {
            prop_assert_eq!(a.cmp(&b), ArtKey::of(a).cmp(&ArtKey::of(b)));
        }

        #[test]
        fn doubles_keep_their_order(a: f64, b: f64) {
            prop_assume!(!a.is_nan() && !b.is_nan());
            prop_assert_eq!(a.total_cmp(&b).is_lt() && a < b, ArtKey::of(a) < ArtKey::of(b));
        }

        #[test]
        fn strings_keep_their_order(a: Vec<u8>, b: Vec<u8>) {
            prop_assert_eq!(a.cmp(&b), ArtKey::of_string(&a).cmp(&ArtKey::of_string(&b)));
        }
    }
}
