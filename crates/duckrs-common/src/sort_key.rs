// Ported from duckdb/src/function/scalar/create_sort_key.cpp and include/duckdb/common/radix.hpp
// @ rust-port-baseline
//
// A sort key is a byte string whose `memcmp` order equals the SQL order of the values it
// encodes, so sorting (and comparing) rows by several ORDER BY columns reduces to comparing
// bytes. Each value is a NULL/valid byte followed by its encoding; DESC flips the encoded bytes.

use crate::column_data::ColumnDataCollection;
use crate::types::{Interval, ListEntry, LogicalTypeId, PhysicalType};
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
                    .map(|c| KeyColumn::new(c, count, child_modifiers))
                    .collect::<Result<_>>()?,
            ),
            PhysicalType::List => {
                let v = vector.as_ref().expect("flattened");
                Kind::List(Box::new(KeyColumn::new(
                    v.list_child(),
                    v.list_size(),
                    child_modifiers,
                )?))
            }
            PhysicalType::Array => {
                let v = vector.as_ref().expect("flattened");
                let size = v.logical_type().array_size().expect("ARRAY has a size") as usize;
                Kind::Array {
                    size,
                    child: Box::new(KeyColumn::new(v.array_child(), count * size, child_modifiers)?),
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
                let (months, days, micros) = self.format.data::<Interval>()[idx].normalize();
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
    let prepared = columns
        .iter()
        .map(|(v, m)| Ok((KeyColumn::new(v, count, *m)?, m.order == OrderType::Descending)))
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
