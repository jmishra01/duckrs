// Ported from duckdb/src/function/scalar/create_sort_key.cpp and include/duckdb/common/radix.hpp
// @ rust-port-baseline
//
// A sort key is a byte string whose `memcmp` order equals the SQL order of the values it
// encodes, so sorting (and comparing) rows by several ORDER BY columns reduces to comparing
// bytes. Each value is a NULL/valid byte followed by its encoding; DESC flips the encoded bytes.

use crate::column_data::ColumnDataCollection;
use crate::types::{Interval, ListEntry, LogicalTypeId, PhysicalType};
use crate::value::{Value, ValueData};
use crate::vector::{DataChunk, SelectionVector, UnifiedFormat, Vector};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    NullsFirst,
    NullsLast,
}

/// `ASC`/`DESC` plus `NULLS FIRST`/`NULLS LAST` (`OrderModifiers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderModifiers {
    pub order: OrderType,
    pub nulls: NullOrder,
}

impl OrderModifiers {
    pub const fn new(order: OrderType, nulls: NullOrder) -> Self {
        Self { order, nulls }
    }

    pub const ASC_NULLS_LAST: Self = Self::new(OrderType::Ascending, NullOrder::NullsLast);

    /// Parses `ASC NULLS LAST`-style text (`OrderModifiers::Parse`).
    pub fn parse(text: &str) -> Result<Self> {
        let normalized = text.to_ascii_lowercase().replace('_', " ");
        let (order, nulls) = match normalized.trim() {
            "asc nulls first" => (OrderType::Ascending, NullOrder::NullsFirst),
            "asc nulls last" => (OrderType::Ascending, NullOrder::NullsLast),
            "desc nulls first" => (OrderType::Descending, NullOrder::NullsFirst),
            "desc nulls last" => (OrderType::Descending, NullOrder::NullsLast),
            _ => {
                return Err(Error::new(
                    crate::ErrorKind::Binder,
                    format!(
                        "Unrecognized order modifier: \"{text}\" - expected one of \"ASC NULLS FIRST\", \"ASC NULLS LAST\", \"DESC NULLS FIRST\", \"DESC NULLS LAST\""
                    ),
                ));
            }
        };
        Ok(Self::new(order, nulls))
    }
}

const NULL_FIRST_BYTE: u8 = 1;
const NULL_LAST_BYTE: u8 = 2;
const STRING_DELIMITER: u8 = 0;
const LIST_DELIMITER: u8 = 0;
const BLOB_ESCAPE: u8 = 1;

/// A column prepared for row-at-a-time key encoding (`SortKeyVectorData`).
struct KeyColumn {
    format: UnifiedFormat,
    physical: PhysicalType,
    is_varchar: bool,
    is_bit: bool,
    is_timetz: bool,
    is_variant: bool,
    /// `create_sort_key` encodes an INTERVAL as its fields are; sorting normalizes them
    raw_interval: bool,
    null_byte: u8,
    valid_byte: u8,
    kind: Kind,
}

enum Kind {
    Value,
    Struct(Vec<KeyColumn>),
    List(Box<KeyColumn>),
    Array { size: usize, child: Box<KeyColumn> },
}

impl KeyColumn {
    fn new(input: &Vector, count: usize, modifiers: OrderModifiers) -> Result<Self> {
        Self::new_with(input, count, modifiers, false)
    }

    fn new_with(input: &Vector, count: usize, modifiers: OrderModifiers, raw_interval: bool) -> Result<Self> {
        let (null_byte, valid_byte) = match modifiers.nulls {
            NullOrder::NullsFirst => (NULL_FIRST_BYTE, NULL_LAST_BYTE),
            NullOrder::NullsLast => (NULL_LAST_BYTE, NULL_FIRST_BYTE),
        };
        // NULLS FIRST/LAST only applies at the top level; nested values put NULLs last when
        // ascending and first when descending (as Postgres does).
        let child_nulls = match modifiers.order {
            OrderType::Ascending => NullOrder::NullsLast,
            OrderType::Descending => NullOrder::NullsFirst,
        };
        let child_modifiers = OrderModifiers::new(modifiers.order, child_nulls);
        let physical = input.physical_type();
        let (vector, format) = match physical {
            PhysicalType::Struct | PhysicalType::List | PhysicalType::Array => {
                let mut flat = input.clone();
                flat.flatten(count);
                let format = flat.to_unified(count);
                (Some(flat), format)
            }
            _ => (None, input.to_unified(count)),
        };
        let kind = match physical {
            PhysicalType::Struct => Kind::Struct(
                vector
                    .as_ref()
                    .expect("flattened")
                    .struct_children()
                    .iter()
                    .map(|c| KeyColumn::new_with(c, count, child_modifiers, raw_interval))
                    .collect::<Result<_>>()?,
            ),
            PhysicalType::List => {
                let v = vector.as_ref().expect("flattened");
                Kind::List(Box::new(KeyColumn::new_with(
                    v.list_child(),
                    v.list_size(),
                    child_modifiers,
                    raw_interval,
                )?))
            }
            PhysicalType::Array => {
                let v = vector.as_ref().expect("flattened");
                let size = v.logical_type().array_size().expect("ARRAY has a size") as usize;
                Kind::Array {
                    size,
                    child: Box::new(KeyColumn::new_with(
                        v.array_child(),
                        count * size,
                        child_modifiers,
                        raw_interval,
                    )?),
                }
            }
            PhysicalType::Bit | PhysicalType::Unknown | PhysicalType::Invalid => {
                return Err(Error::not_implemented(format!(
                    "Unsupported type {} in ConstructSortKey",
                    input.logical_type()
                )));
            }
            _ => Kind::Value,
        };
        Ok(Self {
            format,
            physical,
            is_varchar: input.logical_type().id() == LogicalTypeId::Varchar,
            is_bit: input.logical_type().id() == LogicalTypeId::Bit,
            is_timetz: input.logical_type().id() == LogicalTypeId::TimeTz,
            is_variant: input.logical_type().id() == LogicalTypeId::Variant,
            raw_interval,
            null_byte,
            valid_byte,
            kind,
        })
    }

    /// Appends the key of row `r` to `out`.
    fn encode(&self, r: usize, flip: bool, out: &mut Vec<u8>) {
        let idx = self.format.sel.get(r);
        let valid = self.format.validity.is_valid(idx);
        out.push(if valid { self.valid_byte } else { self.null_byte });
        let start = out.len();
        match &self.kind {
            Kind::Struct(children) => {
                // Children are written even for a NULL struct.
                for child in children {
                    child.encode(r, flip, out);
                }
                return;
            }
            Kind::List(child) => {
                if !valid {
                    return;
                }
                let entry = self.format.data::<ListEntry>()[idx];
                for e in entry.offset..entry.offset + entry.length {
                    child.encode(e as usize, flip, out);
                }
                out.push(if flip { !LIST_DELIMITER } else { LIST_DELIMITER });
                return;
            }
            Kind::Array { size, child } => {
                // Array elements are written even for a NULL array.
                for e in idx * size..(idx + 1) * size {
                    child.encode(e, flip, out);
                }
                out.push(if flip { !LIST_DELIMITER } else { LIST_DELIMITER });
                return;
            }
            Kind::Value => {}
        }
        if !valid {
            return;
        }
        macro_rules! signed {
            ($t:ty) => {{
                let mut bytes = self.format.data::<$t>()[idx].to_be_bytes();
                bytes[0] ^= 0x80;
                out.extend_from_slice(&bytes);
            }};
        }
        macro_rules! unsigned {
            ($t:ty) => {
                out.extend_from_slice(&self.format.data::<$t>()[idx].to_be_bytes())
            };
        }
        match self.physical {
            PhysicalType::Bool => out.push(u8::from(self.format.data::<bool>()[idx])),
            PhysicalType::Int8 => signed!(i8),
            PhysicalType::Int16 => signed!(i16),
            PhysicalType::Int32 => signed!(i32),
            PhysicalType::Int64 => signed!(i64),
            PhysicalType::UInt8 => unsigned!(u8),
            PhysicalType::UInt16 => unsigned!(u16),
            PhysicalType::UInt32 => unsigned!(u32),
            // TIMETZ orders by its time at UTC, then by its offset (`dtime_tz_t::sort_key`)
            PhysicalType::UInt64 if self.is_timetz => {
                let bits = self.format.data::<u64>()[idx];
                let (micros, offset) = (bits >> 24, bits & 0xFF_FFFF);
                out.extend_from_slice(&(((micros + offset * 1_000_000) << 24) | offset).to_be_bytes());
            }
            PhysicalType::UInt64 => unsigned!(u64),
            PhysicalType::Int128 => {
                let v = self.format.data::<i128>()[idx];
                let mut upper = ((v >> 64) as i64).to_be_bytes();
                upper[0] ^= 0x80;
                out.extend_from_slice(&upper);
                out.extend_from_slice(&(v as u64).to_be_bytes());
            }
            PhysicalType::UInt128 => unsigned!(u128),
            PhysicalType::Float => out.extend_from_slice(&encode_float(self.format.data::<f32>()[idx]).to_be_bytes()),
            PhysicalType::Double => out.extend_from_slice(&encode_double(self.format.data::<f64>()[idx]).to_be_bytes()),
            PhysicalType::Interval => {
                // The layout of Radix::EncodeData<interval_t>, over the normalized fields, so that
                // '1 month' and '30 days' order and group as equals (issue #25108).
                let interval = self.format.data::<Interval>()[idx];
                let (months, days, micros) = if self.raw_interval {
                    (i64::from(interval.months), i64::from(interval.days), interval.micros)
                } else {
                    interval.normalize()
                };
                let months = months.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                for mut bytes in [
                    months.to_be_bytes().to_vec(),
                    (days as i32).to_be_bytes().to_vec(),
                    micros.to_be_bytes().to_vec(),
                ] {
                    bytes[0] ^= 0x80;
                    out.extend_from_slice(&bytes);
                }
            }
            PhysicalType::Varchar if self.is_bit => {
                // BIT orders by its bits, not by its padded bytes
                out.extend(crate::types::bit::byte_comparable(self.format.str_at(r)));
                out.push(STRING_DELIMITER);
            }
            // `variant_comparator`, the collation of VARIANT: by the meaning of the value, not its bytes
            PhysicalType::Varchar if self.is_variant => {
                let bytes = self.format.str_at(r);
                let key = variant_comparator_key(bytes).unwrap_or_else(|_| bytes.to_vec());
                for b in key {
                    if b <= BLOB_ESCAPE {
                        out.push(BLOB_ESCAPE);
                    }
                    out.push(b);
                }
                out.push(STRING_DELIMITER);
            }
            PhysicalType::Varchar if self.is_varchar => {
                // Every byte + 1, so none collides with the 0 delimiter (UTF-8 never has 0xFF).
                out.extend(self.format.str_at(r).iter().map(|b| b.wrapping_add(1)));
                out.push(STRING_DELIMITER);
            }
            PhysicalType::Varchar => {
                // BLOBs escape 0x00 and 0x01 with 0x01.
                for &b in self.format.str_at(r) {
                    if b <= BLOB_ESCAPE {
                        out.push(BLOB_ESCAPE);
                    }
                    out.push(b);
                }
                out.push(STRING_DELIMITER);
            }
            other => unreachable!("{other} handled above"),
        }
        if flip {
            out[start..].iter_mut().for_each(|b| *b = !*b);
        }
    }
}

/// The bytes of a key that fit in a [`RowKey`] without an allocation.
const INLINE_KEY_SIZE: usize = 30;

/// What [`create_row_keys`] gives for a row: the bytes of its sort key. Most keys are a few integers
/// or a short string, so those stay in here. A key has one representation, so comparing the bytes
/// compares the keys.
#[derive(Debug, Clone)]
pub enum RowKey {
    Inline { len: u8, bytes: [u8; INLINE_KEY_SIZE] },
    Heap(Vec<u8>),
}

impl RowKey {
    /// The key of these bytes: in here if they fit in [`INLINE_KEY_SIZE`], so that a key has one form.
    pub fn from_bytes(new: &[u8]) -> Self {
        if new.len() <= INLINE_KEY_SIZE {
            let mut bytes = [0; INLINE_KEY_SIZE];
            bytes[..new.len()].copy_from_slice(new);
            Self::Inline {
                len: new.len() as u8,
                bytes,
            }
        } else {
            Self::Heap(new.to_vec())
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..*len as usize],
            Self::Heap(bytes) => bytes,
        }
    }
}

impl PartialEq for RowKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // the bytes past `len` are zero in every inline key, so the arrays compare as the keys do
            (Self::Inline { len: a, bytes: x }, Self::Inline { len: b, bytes: y }) => a == b && x == y,
            _ => self.as_bytes() == other.as_bytes(),
        }
    }
}

impl Eq for RowKey {}

impl std::hash::Hash for RowKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.as_bytes());
    }
}

/// A multiplicative hash of the key bytes. The keys come from the query's own data and nothing
/// here relies on keeping them apart, so it need not resist chosen input, unlike the default.
#[derive(Default, Clone, Copy)]
pub struct KeyHasher(u64);

const KEY_HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

impl KeyHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(23) ^ word).wrapping_mul(KEY_HASH_MULTIPLIER);
    }
}

impl std::hash::Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn finish(&self) -> u64 {
        // the table reads the low bits, and the multiplication moves entropy to the high ones
        let hash = self.0 ^ (self.0 >> 32);
        hash.wrapping_mul(KEY_HASH_MULTIPLIER) ^ (hash >> 29)
    }
}

/// A map from row keys, which can be looked up by the bytes of a key ([`KeyBytes`]).
pub type RowKeyMap<V> = hashbrown::HashMap<RowKey, V, std::hash::BuildHasherDefault<KeyHasher>>;
/// A set of row keys.
pub type RowKeySet = hashbrown::HashSet<RowKey, std::hash::BuildHasherDefault<KeyHasher>>;

/// The bytes of a row key, which find it in a [`RowKeyMap`] without a [`RowKey`] made of them.
#[derive(Debug, Clone, Copy)]
pub struct KeyBytes<'a>(pub &'a [u8]);

impl std::hash::Hash for KeyBytes<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // as `RowKey` hashes
        state.write(self.0);
    }
}

impl hashbrown::Equivalent<RowKey> for KeyBytes<'_> {
    fn equivalent(&self, key: &RowKey) -> bool {
        self.0 == key.as_bytes()
    }
}

/// The keys of [`create_row_keys`] for the rows of a chunk, one after another in one buffer.
#[derive(Debug, Default)]
pub struct RowKeyBuffer {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl RowKeyBuffer {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The key of row `row`.
    pub fn get(&self, row: usize) -> KeyBytes<'_> {
        let start = if row == 0 { 0 } else { self.ends[row - 1] };
        KeyBytes(&self.bytes[start..self.ends[row]])
    }
}

/// [`create_row_keys`] into one buffer, a column at a time: the length of each row's key first, then each column's
/// bytes at their place in each key. A column of a kind that isn't written so is encoded row by row once.
pub fn create_row_key_buffer(columns: &[&Vector], count: usize) -> Result<RowKeyBuffer> {
    let columns = columns
        .iter()
        .map(|column| KeyColumn::new(column, count, OrderModifiers::ASC_NULLS_LAST))
        .collect::<Result<Vec<_>>>()?;
    let mut lengths = vec![0usize; count];
    let mut encoded: Vec<Option<RowKeyBuffer>> = Vec::with_capacity(columns.len());
    for column in &columns {
        let format = &column.format;
        if let Some(width) = column.fixed_width() {
            for (row, length) in lengths.iter_mut().enumerate() {
                *length += if format.validity.is_valid(format.sel.get(row)) {
                    1 + width
                } else {
                    1
                };
            }
            encoded.push(None);
        } else if column.plain_string() {
            for (row, length) in lengths.iter_mut().enumerate() {
                *length += if format.validity.is_valid(format.sel.get(row)) {
                    2 + format.str_at(row).len()
                } else {
                    1
                };
            }
            encoded.push(None);
        } else {
            let mut rows = RowKeyBuffer {
                bytes: Vec::new(),
                ends: Vec::with_capacity(count),
            };
            for (row, length) in lengths.iter_mut().enumerate() {
                let start = rows.bytes.len();
                column.encode(row, false, &mut rows.bytes);
                rows.ends.push(rows.bytes.len());
                *length += rows.bytes.len() - start;
            }
            encoded.push(Some(rows));
        }
    }
    let mut ends = Vec::with_capacity(count);
    let mut total = 0;
    for length in &lengths {
        total += length;
        ends.push(total);
    }
    let mut bytes = vec![0u8; total];
    // where each row's key goes on
    let mut at: Vec<usize> = ends.iter().zip(&lengths).map(|(end, length)| end - length).collect();
    for (column, encoded) in columns.iter().zip(&encoded) {
        match encoded {
            Some(rows) => {
                for (row, at) in at.iter_mut().enumerate() {
                    let key = rows.get(row).0;
                    bytes[*at..*at + key.len()].copy_from_slice(key);
                    *at += key.len();
                }
            }
            None => column.write_at(&mut bytes, &mut at),
        }
    }
    Ok(RowKeyBuffer { bytes, ends })
}

impl KeyColumn {
    /// The bytes of a value of a column of fixed width, which `write_at` writes.
    fn fixed_width(&self) -> Option<usize> {
        if !matches!(self.kind, Kind::Value) || self.is_timetz {
            return None;
        }
        Some(match self.physical {
            PhysicalType::Bool | PhysicalType::Int8 | PhysicalType::UInt8 => 1,
            PhysicalType::Int16 | PhysicalType::UInt16 => 2,
            PhysicalType::Int32 | PhysicalType::UInt32 | PhysicalType::Float => 4,
            PhysicalType::Int64 | PhysicalType::UInt64 | PhysicalType::Double => 8,
            PhysicalType::Int128 | PhysicalType::UInt128 => 16,
            _ => return None,
        })
    }

    /// A VARCHAR that `encode` writes as its bytes plus one each.
    fn plain_string(&self) -> bool {
        matches!(self.kind, Kind::Value)
            && self.physical == PhysicalType::Varchar
            && self.is_varchar
            && !self.is_bit
            && !self.is_variant
    }

    /// `encode` of each row at `at[row]` of `bytes`, for a column of fixed width or a plain VARCHAR, moving `at`
    /// past what it wrote.
    fn write_at(&self, bytes: &mut [u8], at: &mut [usize]) {
        let format = &self.format;
        macro_rules! fixed {
            ($t:ty, $encode:expr) => {{
                let data = format.data::<$t>();
                let encode = $encode;
                for (row, at) in at.iter_mut().enumerate() {
                    let index = format.sel.get(row);
                    if format.validity.is_valid(index) {
                        let value = encode(data[index]);
                        bytes[*at] = self.valid_byte;
                        bytes[*at + 1..*at + 1 + value.len()].copy_from_slice(&value);
                        *at += 1 + value.len();
                    } else {
                        bytes[*at] = self.null_byte;
                        *at += 1;
                    }
                }
            }};
        }
        match self.physical {
            PhysicalType::Bool => fixed!(bool, |v: bool| [u8::from(v)]),
            PhysicalType::Int8 => fixed!(i8, |v: i8| [v as u8 ^ 0x80]),
            PhysicalType::Int16 => fixed!(i16, |v: i16| (v as u16 ^ (1 << 15)).to_be_bytes()),
            PhysicalType::Int32 => fixed!(i32, |v: i32| (v as u32 ^ (1 << 31)).to_be_bytes()),
            PhysicalType::Int64 => fixed!(i64, |v: i64| (v as u64 ^ (1 << 63)).to_be_bytes()),
            PhysicalType::Int128 => fixed!(i128, |v: i128| (v as u128 ^ (1 << 127)).to_be_bytes()),
            PhysicalType::UInt8 => fixed!(u8, |v: u8| [v]),
            PhysicalType::UInt16 => fixed!(u16, |v: u16| v.to_be_bytes()),
            PhysicalType::UInt32 => fixed!(u32, |v: u32| v.to_be_bytes()),
            PhysicalType::UInt64 => fixed!(u64, |v: u64| v.to_be_bytes()),
            PhysicalType::UInt128 => fixed!(u128, |v: u128| v.to_be_bytes()),
            PhysicalType::Float => fixed!(f32, |v: f32| encode_float(v).to_be_bytes()),
            PhysicalType::Double => fixed!(f64, |v: f64| encode_double(v).to_be_bytes()),
            PhysicalType::Varchar => {
                for (row, at) in at.iter_mut().enumerate() {
                    if !format.validity.is_valid(format.sel.get(row)) {
                        bytes[*at] = self.null_byte;
                        *at += 1;
                        continue;
                    }
                    let text = format.str_at(row);
                    bytes[*at] = self.valid_byte;
                    // every byte + 1, so none is the 0 that ends the string
                    for (to, from) in bytes[*at + 1..*at + 1 + text.len()].iter_mut().zip(text) {
                        *to = from.wrapping_add(1);
                    }
                    bytes[*at + 1 + text.len()] = STRING_DELIMITER;
                    *at += 2 + text.len();
                }
            }
            other => unreachable!("{other} is not written a column at a time"),
        }
    }
}

/// A key per row, in the bytes of [`create_sort_keys`] for ascending columns with NULLs last.
pub fn create_row_keys(columns: &[&Vector], count: usize) -> Result<Vec<RowKey>> {
    let buffer = create_row_key_buffer(columns, count)?;
    Ok((0..count).map(|row| RowKey::from_bytes(buffer.get(row).0)).collect())
}

/// `Radix::EncodeFloat`: an order-preserving u32 (NaN largest, -0.0 == 0.0).
fn encode_float(x: f32) -> u32 {
    if x == 0.0 {
        return 1 << 31;
    }
    if x.is_nan() {
        return u32::MAX;
    }
    if x > f32::MAX {
        return u32::MAX - 1;
    }
    if x < -f32::MAX {
        return 0;
    }
    let bits = x.to_bits();
    if bits & (1 << 31) == 0 { bits | (1 << 31) } else { !bits }
}

/// `Radix::EncodeDouble`.
fn encode_double(x: f64) -> u64 {
    if x == 0.0 {
        return 1 << 63;
    }
    if x.is_nan() {
        return u64::MAX;
    }
    if x > f64::MAX {
        return u64::MAX - 1;
    }
    if x < -f64::MAX {
        return 0;
    }
    let bits = x.to_bits();
    if bits < (1 << 63) { bits + (1 << 63) } else { !bits }
}

/// Builds one sort key per row from several ORDER BY columns (`CreateSortKeyHelpers::CreateSortKey`).
pub fn create_sort_keys(columns: &[(&Vector, OrderModifiers)], count: usize) -> Result<Vec<Vec<u8>>> {
    sort_keys_of(columns, count, false)
}

/// The keys of the `create_sort_key` function, which encodes values as they are: an INTERVAL by
/// its fields, not normalized.
pub fn create_raw_sort_keys(columns: &[(&Vector, OrderModifiers)], count: usize) -> Result<Vec<Vec<u8>>> {
    sort_keys_of(columns, count, true)
}

fn sort_keys_of(columns: &[(&Vector, OrderModifiers)], count: usize, raw_interval: bool) -> Result<Vec<Vec<u8>>> {
    let prepared = columns
        .iter()
        .map(|(v, m)| {
            Ok((
                KeyColumn::new_with(v, count, *m, raw_interval)?,
                m.order == OrderType::Descending,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((0..count)
        .map(|r| {
            let mut key = Vec::new();
            for (column, flip) in &prepared {
                column.encode(r, *flip, &mut key);
            }
            key
        })
        .collect())
}

/// The row order of `chunk` under the given ORDER BY columns, as a selection. Ties keep their
/// input order.
pub fn sort_chunk(chunk: &DataChunk, orders: &[(usize, OrderModifiers)]) -> Result<SelectionVector> {
    let columns: Vec<(&Vector, OrderModifiers)> = orders.iter().map(|(c, m)| (chunk.column(*c), *m)).collect();
    let keys = create_sort_keys(&columns, chunk.len())?;
    let mut rows: Vec<u32> = (0..chunk.len() as u32).collect();
    rows.sort_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]));
    Ok(SelectionVector::from_vec(rows))
}

/// Sorts a whole collection in memory. External (spilling) sorts come with Phase 8.
pub fn sort_collection(
    input: &ColumnDataCollection,
    orders: &[(usize, OrderModifiers)],
) -> Result<ColumnDataCollection> {
    let mut keyed: Vec<(Vec<u8>, usize, usize)> = Vec::with_capacity(input.len());
    for (c, chunk) in input.chunks().enumerate() {
        let columns: Vec<(&Vector, OrderModifiers)> = orders.iter().map(|(i, m)| (chunk.column(*i), *m)).collect();
        for (r, key) in create_sort_keys(&columns, chunk.len())?.into_iter().enumerate() {
            keyed.push((key, c, r));
        }
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut output = ColumnDataCollection::new(input.types().to_vec());
    for batch in keyed.chunks(crate::STANDARD_VECTOR_SIZE) {
        let mut out = DataChunk::with_capacity(input.types(), batch.len());
        for (col, column) in out.columns_mut().iter_mut().enumerate() {
            for (i, (_, c, r)) in batch.iter().enumerate() {
                let source = input.chunk(*c).column(col);
                column.copy_from(
                    source,
                    &SelectionVector::from_vec(vec![*r as u32]),
                    1,
                    i,
                    crate::vector::StringCopy::Reference,
                );
            }
        }
        out.set_len(batch.len());
        output.append(&out);
    }
    Ok(output)
}

#[cfg(test)]
mod tests;

// Ported from duckdb/src/function/scalar/variant/variant_comparator.cpp and
// duckdb/src/include/duckdb/common/types/variant_comparison.hpp @ rust-port-baseline

/// `VariantComparisonType`: the rank of a value, which orders values of different types.
#[derive(Clone, Copy)]
#[repr(u8)]
enum VariantRank {
    Boolean = 1,
    Number,
    Real,
    Varchar,
    Blob,
    Uuid,
    Timestamp,
    TimestampTz,
    Time,
    TimeTz,
    Interval,
    Geometry,
    Bitstring,
    Array,
    Object,
    Null,
}

/// `CreateVariantComparator` of one VARIANT, in its encoding: a key that orders values as their meaning does.
pub fn variant_comparator_key(bytes: &[u8]) -> Result<Vec<u8>> {
    let node = crate::variant::VariantNode::decode(bytes)?;
    let mut out = Vec::new();
    encode_variant_node(&node, &mut out)?;
    Ok(out)
}

/// `EncodeVariantValue`.
fn encode_variant_node(node: &crate::variant::VariantNode, out: &mut Vec<u8>) -> Result<()> {
    use crate::variant::VariantNode as N;
    const DELIMITER: u8 = 0;
    match node {
        N::Null => out.push(VariantRank::Null as u8),
        N::Array(children) => {
            out.push(VariantRank::Array as u8);
            for child in children {
                encode_variant_node(child, out)?;
            }
            out.push(DELIMITER);
        }
        N::Object(fields) => {
            out.push(VariantRank::Object as u8);
            // by key, so that the order objects were written in doesn't matter
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            for (key, value) in fields {
                encode_variant_string(key.as_bytes(), true, out);
                encode_variant_node(value, out)?;
            }
            out.push(DELIMITER);
        }
        N::Scalar(value) => encode_variant_scalar(value, out)?,
    }
    Ok(())
}

fn encode_variant_string(bytes: &[u8], varchar: bool, out: &mut Vec<u8>) {
    if varchar {
        out.extend(bytes.iter().map(|b| b.wrapping_add(1)));
    } else {
        for &b in bytes {
            if b <= BLOB_ESCAPE {
                out.push(BLOB_ESCAPE);
            }
            out.push(b);
        }
    }
    out.push(STRING_DELIMITER);
}

fn encode_i128(v: i128, out: &mut Vec<u8>) {
    let mut upper = ((v >> 64) as i64).to_be_bytes();
    upper[0] ^= 0x80;
    out.extend_from_slice(&upper);
    out.extend_from_slice(&(v as u64).to_be_bytes());
}

fn encode_i64(v: i64, out: &mut Vec<u8>) {
    out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
}

fn encode_variant_scalar(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    use LogicalTypeId as T;
    let integer = |value: &Value| -> i128 {
        match value.data() {
            ValueData::Int32(v) => i128::from(*v),
            ValueData::Int64(v) => i128::from(*v),
            ValueData::UInt64(v) => i128::from(*v),
            ValueData::Int128(v) => *v,
            _ => 0,
        }
    };
    let id = value.logical_type().id();
    match id {
        T::Boolean => {
            out.push(VariantRank::Boolean as u8);
            out.push(u8::from(value.data() == &ValueData::Bool(true)));
        }
        T::TinyInt
        | T::SmallInt
        | T::Integer
        | T::BigInt
        | T::HugeInt
        | T::UTinyInt
        | T::USmallInt
        | T::UInteger
        | T::UBigInt
        | T::UHugeInt
        | T::Decimal
        | T::Bignum => {
            out.push(VariantRank::Number as u8);
            encode_variant_number(&value.try_to_string()?, out);
        }
        T::Float | T::Double => {
            out.push(VariantRank::Real as u8);
            let real = match value.data() {
                ValueData::Float(v) => f64::from(*v),
                ValueData::Double(v) => *v,
                _ => 0.0,
            };
            out.extend_from_slice(&encode_double(real).to_be_bytes());
        }
        T::Varchar => {
            out.push(VariantRank::Varchar as u8);
            if let ValueData::Bytes(bytes) = value.data() {
                encode_variant_string(bytes, true, out);
            }
        }
        T::Blob | T::Geometry => {
            out.push(if id == T::Blob {
                VariantRank::Blob
            } else {
                VariantRank::Geometry
            } as u8);
            if let ValueData::Bytes(bytes) = value.data() {
                encode_variant_string(bytes, false, out);
            }
        }
        T::Uuid => {
            out.push(VariantRank::Uuid as u8);
            encode_i128(integer(value), out);
        }
        // nanoseconds since the epoch; a DATE is its midnight
        T::Date | T::TimestampSec | T::TimestampMs | T::Timestamp | T::TimestampNs => {
            out.push(VariantRank::Timestamp as u8);
            let factor: i128 = match id {
                T::Date => 86_400_000_000_000,
                T::TimestampSec => 1_000_000_000,
                T::TimestampMs => 1_000_000,
                T::Timestamp => 1_000,
                _ => 1,
            };
            encode_i128(integer(value).saturating_mul(factor), out);
        }
        T::TimestampTz | T::TimestampTzNs => {
            out.push(VariantRank::TimestampTz as u8);
            let factor: i128 = if id == T::TimestampTz { 1_000 } else { 1 };
            encode_i128(integer(value).saturating_mul(factor), out);
        }
        // nanoseconds since midnight
        T::Time | T::TimeNs => {
            out.push(VariantRank::Time as u8);
            let factor: i64 = if id == T::Time { 1_000 } else { 1 };
            encode_i64((integer(value) as i64).saturating_mul(factor), out);
        }
        T::TimeTz => {
            out.push(VariantRank::TimeTz as u8);
            let bits = integer(value) as u64;
            let (micros, offset) = (bits >> 24, bits & 0xFF_FFFF);
            out.extend_from_slice(&(((micros + offset * 1_000_000) << 24) | offset).to_be_bytes());
        }
        T::Interval => {
            out.push(VariantRank::Interval as u8);
            if let ValueData::Interval(interval) = value.data() {
                let (months, days, micros) = interval.normalize();
                let months = months.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                for mut bytes in [
                    months.to_be_bytes().to_vec(),
                    (days as i32).to_be_bytes().to_vec(),
                    micros.to_be_bytes().to_vec(),
                ] {
                    bytes[0] ^= 0x80;
                    out.extend_from_slice(&bytes);
                }
            }
        }
        T::Bit => {
            out.push(VariantRank::Bitstring as u8);
            if let ValueData::Bytes(bits) = value.data() {
                out.extend(crate::types::bit::byte_comparable(bits));
            }
            out.push(STRING_DELIMITER);
        }
        other => {
            return Err(crate::Error::new(
                crate::ErrorKind::NotImplemented,
                format!("Variant type {} is not supported in variant_comparator", other.name()),
            ));
        }
    }
    Ok(())
}

/// `VariantEncodeNumber` of `VariantGetNumberKey`: the value as `d1.d2... * 10^adjexp`, from its decimal text, so
/// that equal numbers of any width or scale have the same key and others order by their value.
fn encode_variant_number(text: &str, out: &mut Vec<u8>) {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let all: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes())
        .filter(u8::is_ascii_digit)
        .collect();
    let Some(first) = all.iter().position(|d| *d != b'0') else {
        // zero: the class says it all
        out.push(1);
        return;
    };
    let last = all.iter().rposition(|d| *d != b'0').expect("a digit that isn't 0");
    // the position of the first significant digit relative to the decimal point
    let adjusted_exponent = whole.len() as i64 - 1 - first as i64;
    out.push(if negative { 0 } else { 2 });
    let emit = |b: u8, out: &mut Vec<u8>| out.push(if negative { !b } else { b });
    if (-127..=126).contains(&adjusted_exponent) {
        emit((adjusted_exponent + 128) as u8, out);
    } else {
        emit(if adjusted_exponent < 0 { 0x00 } else { 0xFF }, out);
        for b in ((adjusted_exponent as u64) ^ (1 << 63)).to_be_bytes() {
            emit(b, out);
        }
    }
    for d in &all[first..=last] {
        emit(d - b'0' + 1, out);
    }
    emit(0, out);
}
