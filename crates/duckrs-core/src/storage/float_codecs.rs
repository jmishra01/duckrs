//! Ported from duckdb/src/include/duckdb/storage/compression/{chimp,patas}/ (chimp_scan.hpp,
//! patas_scan.hpp, algorithm/*.hpp) and duckdb/src/storage/compression/{chimp/chimp.cpp,patas.cpp}
//! @ rust-port-baseline.
//!
//! The scans of the Chimp and Patas segments of FLOAT and DOUBLE columns. C++ no longer writes
//! either (ALP took their place), but files written before v0.9 have them. Both XOR each value with
//! an earlier one and store what differs, in groups of 1024 values whose metadata the segment
//! keeps at its end, read backward from there. Everything is checked against the segment as C++'s
//! `CompressionSegmentReader` does, with its messages.

use super::block_manager::corrupted;
use duckrs_common::{Error, Result};

/// `ChimpPrimitives::CHIMP_SEQUENCE_SIZE`, `PatasPrimitives::PATAS_GROUP_SIZE`.
const GROUP_SIZE: usize = 1024;
/// `ChimpPrimitives::HEADER_SIZE`, `PatasPrimitives::HEADER_SIZE`: where the metadata ends.
const HEADER_SIZE: usize = 4;

/// `ChimpConstants::Decompression::LEADING_REPRESENTATION`.
const LEADING_REPRESENTATION: [u8; 8] = [0, 8, 12, 16, 18, 20, 22, 24];
/// `ChimpConstants::BUFFER_SIZE`: the ring of earlier values a value refers to.
const RING_SIZE: usize = 128;

/// `ChimpConstants::Flags`.
const VALUE_IDENTICAL: u8 = 0;
const TRAILING_EXCEEDS_THRESHOLD: u8 = 1;
const LEADING_ZERO_EQUALITY: u8 = 2;
const LEADING_ZERO_LOAD: u8 = 3;

fn corrupt(context: &str, message: &str) -> Error {
    corrupted(&format!("Corrupted {context}: {message}"))
}

/// `CompressionSegmentReader` over part of a segment: reads that leave it fail as C++'s do.
struct Reader<'a> {
    data: &'a [u8],
    /// where the next backward read ends
    position: usize,
    context: &'static str,
}

impl<'a> Reader<'a> {
    /// `CompressionSegmentReader::FromSegment` and the header both codecs start with: the metadata
    /// ends where the first four bytes say, which is where its backward reads start.
    fn segment(segment: &'a [u8], context: &'static str, before_header: &str) -> Result<Self> {
        let end = segment
            .first_chunk::<4>()
            .ok_or_else(|| corrupt(context, "attempted to read past the end of the segment"))?;
        let end = u32::from_le_bytes(*end) as usize;
        if end < HEADER_SIZE {
            return Err(corrupt(context, before_header));
        }
        let data = segment
            .get(..end)
            .ok_or_else(|| corrupt(context, "requested range extends past the end of the segment"))?;
        Ok(Self {
            data,
            position: end,
            context,
        })
    }

    /// `GetSubReader`.
    fn sub(&self, offset: usize, size: usize, context: &'static str) -> Result<Reader<'a>> {
        if offset > self.data.len() {
            return Err(corrupt(self.context, "offset is outside the segment"));
        }
        let data = offset
            .checked_add(size)
            .and_then(|end| self.data.get(offset..end))
            .ok_or_else(|| corrupt(self.context, "requested range extends past the end of the segment"))?;
        Ok(Reader {
            data,
            position: size,
            context,
        })
    }

    /// `ReadBytesBackward`.
    fn bytes_backward(&mut self, size: usize) -> Result<&'a [u8]> {
        let start = self
            .position
            .checked_sub(size)
            .ok_or_else(|| corrupt(self.context, "attempted to read before the start of the segment"))?;
        let bytes = &self.data[start..self.position];
        self.position = start;
        Ok(bytes)
    }

    fn u8_backward(&mut self) -> Result<u8> {
        Ok(self.bytes_backward(1)?[0])
    }

    fn u32_backward(&mut self) -> Result<u32> {
        let bytes = self.bytes_backward(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    /// `AlignBackward`: to a multiple of `alignment` from the start of the segment.
    fn align_backward(&mut self, alignment: usize) {
        self.position -= self.position % alignment;
    }

    fn u16s_backward(&mut self, count: usize) -> Result<Vec<u16>> {
        let size = count
            .checked_mul(2)
            .ok_or_else(|| corrupt(self.context, "array count extends past the end of the segment"))?;
        let bytes = self.bytes_backward(size)?;
        Ok(bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect())
    }
}

/// A FLOAT or DOUBLE by the bits of its value, as both codecs work on them.
trait Bits: Copy {
    const BITS: u8;
    /// `SignificantBits<T>::mask`: of the low bits of a packed value of Chimp (and Patas).
    const SIGNIFICANT_MASK: u16;
    fn from_u64(value: u64) -> Self;
    fn extend(self, out: &mut Vec<u8>);
}

impl Bits for u64 {
    const BITS: u8 = 64;
    const SIGNIFICANT_MASK: u16 = 63;
    fn from_u64(value: u64) -> Self {
        value
    }
    fn extend(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
}

impl Bits for u32 {
    const BITS: u8 = 32;
    const SIGNIFICANT_MASK: u16 = 31;
    fn from_u64(value: u64) -> Self {
        value as u32
    }
    fn extend(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
}

/// `UnpackedData`, from `PackedDataUtils::Unpack`: an index of the ring (7 bits), a leading-zero
/// code (3 bits) and the significant bits (the low 6 bits, 5 for FLOAT).
#[derive(Clone, Copy)]
struct Unpacked {
    index: u8,
    leading_zero: u8,
    significant_bits: u8,
}

fn unpack<T: Bits>(packed: u16) -> Unpacked {
    Unpacked {
        index: ((packed >> 9) & 0x7F) as u8,
        leading_zero: ((packed >> 6) & 0x7) as u8,
        significant_bits: (packed & T::SIGNIFICANT_MASK) as u8,
    }
}

/// `BitReader`: bits from the most significant one of each byte on.
struct BitReader<'a> {
    input: &'a [u8],
    index: usize,
}

impl BitReader<'_> {
    /// `ReadValue`, of `size` bits; the group's bits were checked to be there.
    fn read(&mut self, size: u8) -> u64 {
        let mut result = 0u64;
        for _ in 0..size {
            let bit = (self.input[self.index >> 3] >> (7 - (self.index & 7))) & 1;
            result = (result << 1) | u64::from(bit);
            self.index += 1;
        }
        result
    }
}

/// `ChimpScanState`: the values of a Chimp segment of `count` values of width `T`.
fn chimp<T: Bits>(segment: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
    const CONTEXT: &str = "Chimp segment";
    let mut metadata = Reader::segment(segment, CONTEXT, "metadata ends before the segment header")?;
    let data = metadata.sub(HEADER_SIZE, metadata.data.len() - HEADER_SIZE, "Chimp data")?;
    let mut data_bit_position = 0usize;
    let mut scanned = 0;
    while scanned < count {
        // the group's offset in the data, which the scan does not use
        metadata.u32_backward()?;
        let leading_zero_blocks = metadata.u8_backward()?;
        if usize::from(leading_zero_blocks) > GROUP_SIZE / 8 {
            return Err(corrupt(
                CONTEXT,
                &format!(
                    "leading-zero block count {leading_zero_blocks} exceeds {}",
                    GROUP_SIZE / 8
                ),
            ));
        }
        let leading_zero_bytes = metadata.bytes_backward(3 * usize::from(leading_zero_blocks))?;
        let group_size = (count - scanned).min(GROUP_SIZE);
        // the first value of a group has no flag
        let flag_count = group_size - 1;
        let flag_bytes = metadata.bytes_backward(flag_count.div_ceil(4))?;
        let mut flags = Vec::with_capacity(group_size);
        flags.push(VALUE_IDENTICAL);
        flags.extend((0..flag_count).map(|i| (flag_bytes[i >> 2] >> (6 - 2 * (i & 3))) & 3));

        let stored_leading_zeros = usize::from(leading_zero_blocks) * 8;
        let required = flags[1..]
            .iter()
            .filter(|&&f| f == LEADING_ZERO_LOAD)
            .count()
            .next_multiple_of(8);
        if stored_leading_zeros != required {
            return Err(corrupt(
                CONTEXT,
                &format!("leading-zero metadata contains {stored_leading_zeros} values but {required} are required"),
            ));
        }
        let leading_zeros: Vec<u8> = (0..stored_leading_zeros)
            .map(|i| {
                let block = &leading_zero_bytes[(i >> 3) * 3..(i >> 3) * 3 + 3];
                let bits = u32::from_le_bytes([block[0], block[1], block[2], 0]);
                LEADING_REPRESENTATION[((bits >> (3 * (i & 7))) & 7) as usize]
            })
            .collect();

        let packed_count = flags[1..].iter().filter(|&&f| f == TRAILING_EXCEEDS_THRESHOLD).count();
        metadata.align_backward(2);
        let packed: Vec<Unpacked> = metadata
            .u16s_backward(packed_count)?
            .into_iter()
            .map(|packed| {
                let mut unpacked = unpack::<T>(packed);
                if unpacked.significant_bits == 0 {
                    unpacked.significant_bits = 64;
                }
                unpacked.leading_zero = LEADING_REPRESENTATION[usize::from(unpacked.leading_zero)];
                unpacked
            })
            .collect();

        // `ValidateAndCalculateDataBitCount`: what the values take, checked before reading any
        let mut bit_count = usize::from(T::BITS);
        let (mut packed_at, mut leading_at) = (0, 0);
        let mut leading_zero = u8::MAX;
        for &flag in &flags[1..] {
            bit_count += match flag {
                VALUE_IDENTICAL => 7,
                TRAILING_EXCEEDS_THRESHOLD => {
                    let unpacked = packed[packed_at];
                    packed_at += 1;
                    if unpacked.leading_zero > T::BITS || unpacked.significant_bits > T::BITS - unpacked.leading_zero {
                        return Err(corrupt(
                            CONTEXT,
                            &format!(
                                "packed data uses {} leading-zero and {} significant bits for a {}-bit value",
                                unpacked.leading_zero,
                                unpacked.significant_bits,
                                T::BITS
                            ),
                        ));
                    }
                    leading_zero = unpacked.leading_zero;
                    usize::from(unpacked.significant_bits)
                }
                LEADING_ZERO_EQUALITY => {
                    if leading_zero > T::BITS {
                        return Err(corrupt(
                            CONTEXT,
                            "leading-zero equality has no preceding leading-zero value",
                        ));
                    }
                    usize::from(T::BITS - leading_zero)
                }
                _ => {
                    leading_zero = leading_zeros[leading_at];
                    leading_at += 1;
                    usize::from(T::BITS - leading_zero)
                }
            };
        }
        let bit_offset = data_bit_position & 7;
        let byte_count = (bit_offset + bit_count).div_ceil(8);
        let mut group_data = data.sub(data_bit_position >> 3, byte_count, "Chimp data")?;
        let mut input = BitReader {
            input: group_data.bytes_backward(byte_count)?,
            index: bit_offset,
        };

        // `Chimp128Decompression`
        let mut ring = [0u64; RING_SIZE];
        let mut ring_index = 0usize;
        let mut reference = input.read(T::BITS);
        ring[0] = reference;
        T::from_u64(reference).extend(out);
        let (mut packed_at, mut leading_at) = (0, 0);
        let mut leading_zero = u8::MAX;
        for &flag in &flags[1..] {
            let value = match flag {
                VALUE_IDENTICAL => ring[input.read(7) as usize],
                TRAILING_EXCEEDS_THRESHOLD => {
                    let unpacked = packed[packed_at];
                    packed_at += 1;
                    leading_zero = unpacked.leading_zero;
                    let trailing_zeros = T::BITS - unpacked.significant_bits - leading_zero;
                    let bits = input.read(unpacked.significant_bits);
                    (bits.checked_shl(u32::from(trailing_zeros)).unwrap_or(0) ^ ring[usize::from(unpacked.index)])
                        & mask::<T>()
                }
                LEADING_ZERO_EQUALITY => input.read(T::BITS - leading_zero) ^ reference,
                _ => {
                    leading_zero = leading_zeros[leading_at];
                    leading_at += 1;
                    input.read(T::BITS - leading_zero) ^ reference
                }
            };
            reference = value;
            ring_index += 1;
            ring[ring_index % RING_SIZE] = value;
            T::from_u64(value).extend(out);
        }
        data_bit_position += bit_count;
        scanned += group_size;
    }
    Ok(())
}

fn mask<T: Bits>() -> u64 {
    u64::MAX >> (64 - u32::from(T::BITS))
}

/// `PatasScanState`: the values of a Patas segment of `count` values of width `T`.
fn patas<T: Bits>(segment: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
    const CONTEXT: &str = "Patas segment";
    const GROUP_OFFSET_SIZE: usize = 4;
    const PACKED_DATA_SIZE: usize = 2;
    let table_error = || corrupt(CONTEXT, "metadata table exceeds the segment");
    let reader = Reader::segment(segment, CONTEXT, "metadata ends before the segment header")?;
    let metadata_end = reader.data.len();
    // `ReadSegmentLayout`: the data, then a table of each group's packed values and offset
    let groups = count.div_ceil(GROUP_SIZE);
    let mut capacity = metadata_end - HEADER_SIZE;
    if groups > capacity / GROUP_OFFSET_SIZE {
        return Err(table_error());
    }
    let offsets_size = groups * GROUP_OFFSET_SIZE;
    capacity -= offsets_size;
    if count > capacity / PACKED_DATA_SIZE {
        return Err(table_error());
    }
    let metadata_size = offsets_size + count * PACKED_DATA_SIZE;
    let metadata_start = metadata_end - metadata_size;
    let data = reader.sub(HEADER_SIZE, metadata_start - HEADER_SIZE, "Patas data")?;
    let metadata = reader.sub(metadata_start, metadata_size, "Patas metadata")?;
    let width = usize::from(T::BITS / 8);
    let group_error = || corrupt(CONTEXT, "group data is outside the data region");

    let mut metadata_position = metadata.data.len();
    let mut scanned = 0;
    let mut values: Vec<u64> = Vec::with_capacity(GROUP_SIZE);
    while scanned < count {
        let group_size = (count - scanned).min(GROUP_SIZE);
        let group_metadata_size = GROUP_OFFSET_SIZE + PACKED_DATA_SIZE * group_size;
        metadata_position -= group_metadata_size;
        let mut group = metadata.sub(metadata_position, group_metadata_size, "Patas group metadata")?;
        let data_offset = group.u32_backward()? as usize;
        if data_offset < HEADER_SIZE || data_offset - HEADER_SIZE > data.data.len() {
            return Err(group_error());
        }
        let packed: Vec<Unpacked> = group.data[..group_size * PACKED_DATA_SIZE]
            .chunks_exact(2)
            .map(|pair| unpack::<T>(u16::from_le_bytes([pair[0], pair[1]])))
            .collect();
        // `LoadPackedData`: as (significant bytes, trailing zeros, index difference)
        let mut data_size = 0;
        for (i, unpacked) in packed.iter().enumerate() {
            let (bytes, trailing_zeros, index_diff) = (
                unpacked.leading_zero,
                unpacked.significant_bits,
                usize::from(unpacked.index),
            );
            if index_diff > i || (i > 0 && index_diff == 0) {
                return Err(corrupt(CONTEXT, "invalid backward reference"));
            }
            if usize::from(bytes) > width || trailing_zeros >= T::BITS {
                return Err(corrupt(CONTEXT, "invalid packed value metadata"));
            }
            data_size += read_size(bytes, trailing_zeros, width);
        }
        let data_offset = data_offset - HEADER_SIZE;
        if data_size > data.data.len() - data_offset {
            return Err(group_error());
        }
        let mut bytes = &data.data[data_offset..data_offset + data_size];

        // `PatasDecompression::DecompressValue`: the bytes, shifted back, XOR the value referred to
        values.clear();
        for (i, unpacked) in packed.iter().enumerate() {
            let size = read_size(unpacked.leading_zero, unpacked.significant_bits, width);
            let mut le = [0u8; 8];
            le[..size].copy_from_slice(&bytes[..size]);
            bytes = &bytes[size..];
            let previous = if i == 0 {
                0
            } else {
                values[i - usize::from(unpacked.index)]
            };
            let value = ((u64::from_le_bytes(le) << unpacked.significant_bits) ^ previous) & mask::<T>();
            values.push(value);
            T::from_u64(value).extend(out);
        }
        scanned += group_size;
    }
    Ok(())
}

/// `ByteReader::ReadValue(bytes, trailing_zero)`: zero bytes are a full value, unless the value is
/// its reference (eight trailing zeros or more).
fn read_size(bytes: u8, trailing_zeros: u8, width: usize) -> usize {
    match bytes {
        0 if trailing_zeros < 8 => width,
        bytes => usize::from(bytes),
    }
}

/// The values of a Chimp segment of FLOAT (`width` 4) or DOUBLE (8) values.
pub fn scan_chimp(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    match width {
        8 => chimp::<u64>(segment, count, out),
        _ => chimp::<u32>(segment, count, out),
    }
}

/// The values of a Patas segment of FLOAT (`width` 4) or DOUBLE (8) values.
pub fn scan_patas(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    match width {
        8 => patas::<u64>(segment, count, out),
        _ => patas::<u32>(segment, count, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_read_from_the_most_significant_on() {
        let mut reader = BitReader {
            input: &[0b1010_1100, 0b0101_0000],
            index: 1,
        };
        assert_eq!(reader.read(3), 0b010);
        assert_eq!(reader.read(6), 0b110_001);
    }

    #[test]
    fn a_patas_segment_of_two_values() {
        // header, two full-width values (the second an XOR with the first), then the group's packed
        // values and its data offset
        let first = 1.5f64.to_bits();
        let second = 2.25f64.to_bits();
        let mut segment = vec![0u8; 4];
        segment.extend_from_slice(&first.to_le_bytes());
        segment.extend_from_slice(&(first ^ second).to_le_bytes());
        // (index difference, significant bytes 0 = full width, trailing zeros 0)
        segment.extend_from_slice(&0u16.to_le_bytes());
        segment.extend_from_slice(&(1u16 << 9).to_le_bytes());
        segment.extend_from_slice(&4u32.to_le_bytes());
        let end = segment.len() as u32;
        segment[..4].copy_from_slice(&end.to_le_bytes());
        let mut out = Vec::new();
        scan_patas(&segment, 2, 8, &mut out).unwrap();
        let values: Vec<f64> = out
            .chunks_exact(8)
            .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(values, [1.5, 2.25]);
    }

    #[test]
    fn a_reference_to_a_later_value_is_corrupt() {
        let mut segment = vec![0u8; 4];
        segment.extend_from_slice(&[0; 8]);
        segment.extend_from_slice(&(1u16 << 9).to_le_bytes());
        segment.extend_from_slice(&4u32.to_le_bytes());
        let end = segment.len() as u32;
        segment[..4].copy_from_slice(&end.to_le_bytes());
        let error = scan_patas(&segment, 1, 8, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("invalid backward reference"), "{error}");
    }
}
