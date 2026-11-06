//! Ported from duckdb/src/storage/compression/{rle,bitpacking}.cpp @ rust-port-baseline.
//!
//! The encoders of the codecs that `compression.rs` decodes. Each takes the fixed-size values of
//! rows and makes segments that fit in a block, with how many rows each holds.

use duckrs_common::types::PhysicalType;

/// `BITPACKING_METADATA_GROUP_SIZE`.
const GROUP_ROWS: usize = 2048;
/// The packer of C++ fills groups of this many values, also for fewer.
const PACKING_ROWS: usize = 32;
const HEADER_SIZE: usize = 8;
const MODE_CONSTANT: u32 = 2;
const MODE_FOR: u32 = 5;

/// A segment and the rows it holds.
pub type Encoded = (Vec<u8>, usize);

/// NULL rows take the value before them, or the first there is. Their value doesn't count, as the
/// validity column has them, so this way they neither break runs nor widen a frame of references.
pub fn fill_nulls(data: &mut [u8], width: usize, valid: &[bool]) {
    let Some(first) = valid.iter().position(|v| *v) else {
        data.fill(0);
        return;
    };
    let value: Vec<u8> = data[first * width..(first + 1) * width].to_vec();
    let mut previous = value;
    for (row, valid) in valid.iter().enumerate() {
        let at = row * width;
        if *valid {
            previous.copy_from_slice(&data[at..at + width]);
        } else {
            data[at..at + width].copy_from_slice(&previous);
        }
    }
}

/// `RLECompressState`: values, then how often each repeats.
pub fn encode_rle(data: &[u8], width: usize, block_size: usize) -> Vec<Encoded> {
    const MAX_RUN: usize = u16::MAX as usize;
    let max_entries = (block_size - HEADER_SIZE) / (width + 2);
    let mut segments = Vec::new();
    let mut values: Vec<u8> = Vec::new();
    let mut counts: Vec<u8> = Vec::new();
    let mut rows = 0;
    let mut finish = |values: &mut Vec<u8>, counts: &mut Vec<u8>, rows: &mut usize| {
        if *rows == 0 {
            return;
        }
        let mut segment = Vec::with_capacity(HEADER_SIZE + values.len() + counts.len());
        segment.extend_from_slice(&((HEADER_SIZE + values.len()) as u64).to_le_bytes());
        segment.extend_from_slice(values);
        segment.extend_from_slice(counts);
        segments.push((segment, *rows));
        values.clear();
        counts.clear();
        *rows = 0;
    };
    let mut row = 0;
    let total = data.len() / width;
    while row < total {
        let value = &data[row * width..(row + 1) * width];
        let mut run = 1;
        while row + run < total && run < MAX_RUN && &data[(row + run) * width..(row + run + 1) * width] == value {
            run += 1;
        }
        if counts.len() / 2 == max_entries {
            finish(&mut values, &mut counts, &mut rows);
        }
        values.extend_from_slice(value);
        counts.extend_from_slice(&(run as u16).to_le_bytes());
        rows += run;
        row += run;
    }
    finish(&mut values, &mut counts, &mut rows);
    segments
}

/// A value of `width` bytes as a number, sign extended for `signed` types.
fn number(bytes: &[u8], signed: bool) -> i128 {
    let mut value = [0u8; 16];
    value[..bytes.len()].copy_from_slice(bytes);
    let unsigned = u128::from_le_bytes(value);
    let bits = bytes.len() * 8;
    if signed && bits < 128 && unsigned >> (bits - 1) & 1 != 0 {
        (unsigned | (u128::MAX << bits)) as i128
    } else {
        unsigned as i128
    }
}

fn pack(values: &[u128], bits: usize) -> Vec<u8> {
    let rows = values.len().next_multiple_of(PACKING_ROWS);
    let mut packed = vec![0u8; (rows * bits).div_ceil(8)];
    for (row, value) in values.iter().enumerate() {
        let start = row * bits;
        for bit in 0..bits {
            if value >> bit & 1 != 0 {
                let at = start + bit;
                packed[at / 8] |= 1 << (at % 8);
            }
        }
    }
    packed
}

/// The widths that the packer of C++ has for a type: all of them, but 0 is a constant.
fn encode_group(group: &[u8], width: usize, signed: bool) -> (Vec<u8>, u32) {
    let mut smallest = None;
    let mut largest = None;
    for value in group.chunks_exact(width) {
        let value = number(value, signed);
        smallest = Some(smallest.map_or(value, |s: i128| s.min(value)));
        largest = Some(largest.map_or(value, |l: i128| l.max(value)));
    }
    let (smallest, largest) = (smallest.expect("a group has rows"), largest.expect("a group has rows"));
    let frame = &(smallest as u128).to_le_bytes()[..width];
    if smallest == largest {
        return (frame.to_vec(), MODE_CONSTANT);
    }
    let range = largest.wrapping_sub(smallest) as u128;
    let bits = (128 - range.leading_zeros()) as usize;
    let differences: Vec<u128> = group
        .chunks_exact(width)
        .map(|value| number(value, signed).wrapping_sub(smallest) as u128)
        .collect();
    let mut data = frame.to_vec();
    data.extend_from_slice(&(bits as u128).to_le_bytes()[..width]);
    data.extend_from_slice(&pack(&differences, bits));
    (data, MODE_FOR)
}

/// `BitpackingCompressionState`: groups of 2048 rows, each a constant or a frame of reference and
/// packed differences, and at the end of the segment, from the back, where each group is.
pub fn encode_bitpacking(data: &[u8], width: usize, signed: bool, block_size: usize) -> Vec<Encoded> {
    let mut segments = Vec::new();
    let mut body = vec![0u8; HEADER_SIZE];
    let mut entries: Vec<u32> = Vec::new();
    let mut rows = 0;
    let mut finish = |body: &mut Vec<u8>, entries: &mut Vec<u32>, rows: &mut usize| {
        if *rows == 0 {
            return;
        }
        let table_at = body.len().next_multiple_of(8);
        body.resize(table_at, 0);
        for entry in entries.iter().rev() {
            body.extend_from_slice(&entry.to_le_bytes());
        }
        let end = body.len() as u64;
        body[..HEADER_SIZE].copy_from_slice(&end.to_le_bytes());
        segments.push((std::mem::replace(body, vec![0u8; HEADER_SIZE]), *rows));
        entries.clear();
        *rows = 0;
    };
    for group in data.chunks(GROUP_ROWS * width) {
        let (encoded, mode) = encode_group(group, width, signed);
        let needed = body.len() + encoded.len() + 8 + (entries.len() + 1) * 4;
        if needed > block_size {
            finish(&mut body, &mut entries, &mut rows);
        }
        entries.push(body.len() as u32 | mode << 24);
        body.extend_from_slice(&encoded);
        rows += group.len() / width;
    }
    finish(&mut body, &mut entries, &mut rows);
    segments
}

/// One Roaring container: arrays of the rows that are the exception, or a bitset. C++ also has
/// runs, which take less for long stretches, but it reads these just as well.
fn roaring_container(bits: &[bool], body: &mut Vec<u8>, types: &mut Vec<u128>, cardinalities: &mut Vec<u8>) {
    const IS_INVERTED: u128 = 1;
    const COMPRESSED_SEGMENT_COUNT: usize = 8;
    const COMPRESSED_SEGMENT_SIZE: usize = 256;
    const COMPRESSED_ARRAY_THRESHOLD: usize = 8;
    /// `MAX_ARRAY_IDX`.
    const MAX_ARRAY: usize = 248;
    const BITSET_SENTINEL: u8 = 249;
    let set = bits.iter().filter(|b| **b).count();
    let unset = bits.len() - set;
    let (listed, inverted): (Vec<usize>, bool) = if unset < set {
        ((0..bits.len()).filter(|&row| !bits[row]).collect(), true)
    } else {
        ((0..bits.len()).filter(|&row| bits[row]).collect(), false)
    };
    if listed.len() > MAX_ARRAY {
        body.resize(body.len().next_multiple_of(8), 0);
        let mut words = vec![0u8; bits.len().div_ceil(64) * 8];
        for row in (0..bits.len()).filter(|&row| bits[row]) {
            words[row / 8] |= 1 << (row % 8);
        }
        body.extend_from_slice(&words);
        types.push(0);
        cardinalities.push(BITSET_SENTINEL);
        return;
    }
    types.push(if inverted { IS_INVERTED } else { 0 });
    cardinalities.push(listed.len() as u8);
    if listed.len() < COMPRESSED_ARRAY_THRESHOLD {
        body.resize(body.len().next_multiple_of(2), 0);
        for row in listed {
            body.extend_from_slice(&(row as u16).to_le_bytes());
        }
        return;
    }
    let mut counts = [0u8; COMPRESSED_SEGMENT_COUNT];
    for row in &listed {
        counts[row / COMPRESSED_SEGMENT_SIZE] += 1;
    }
    body.extend_from_slice(&counts);
    body.extend(listed.iter().map(|row| (row % COMPRESSED_SEGMENT_SIZE) as u8));
}

/// `RoaringCompressState`: what the containers hold, then what kind each is. `bits` says what is set,
/// which for a validity column means that a row isn't NULL.
pub fn encode_roaring(bits: &[bool], block_size: usize) -> Vec<Encoded> {
    const CONTAINER_ROWS: usize = 2048;
    /// A bitset, the alignment in front of it, and its entries in the metadata.
    const LARGEST_CONTAINER: usize = 256 + 8 + 2;
    let containers_per_segment = ((block_size - 2 * HEADER_SIZE) / LARGEST_CONTAINER).max(1);
    let mut segments = Vec::new();
    for rows in bits.chunks(containers_per_segment * CONTAINER_ROWS) {
        let mut body = Vec::new();
        let mut types = Vec::new();
        let mut cardinalities = Vec::new();
        for container in rows.chunks(CONTAINER_ROWS) {
            roaring_container(container, &mut body, &mut types, &mut cardinalities);
        }
        body.resize(body.len().next_multiple_of(8), 0);
        let mut segment = (body.len() as u64).to_le_bytes().to_vec();
        segment.extend_from_slice(&body);
        segment.extend_from_slice(&pack(&types, 2));
        segment.extend_from_slice(&cardinalities);
        segments.push((segment, rows.len()));
    }
    segments
}

fn bit_width(value: u64) -> usize {
    (64 - value.leading_zeros()) as usize
}

/// DICT_FSST in its dictionary mode: the distinct strings, how long each is, and per row which one
/// it is, where 0 is NULL. FSST on top of that takes a symbol table, which this doesn't train.
/// `None` for strings that are too long, which go to overflow blocks instead.
pub fn encode_dictionary(strings: &[&[u8]], valid: &[bool], block_size: usize, limit: usize) -> Option<Vec<Encoded>> {
    use std::collections::HashMap;
    const HEADER: usize = 16;
    let packed = |rows: usize, bits: usize| (rows.next_multiple_of(PACKING_ROWS) * bits).div_ceil(8);
    let size = |dictionary: usize, count: usize, longest: usize, rows: usize| {
        (HEADER + dictionary).next_multiple_of(8)
            + packed(count, bit_width(longest as u64)).next_multiple_of(8)
            + packed(rows, bit_width(count as u64 - 1))
    };
    let mut segments = Vec::new();
    let mut start = 0;
    while start < strings.len() {
        let mut positions: HashMap<&[u8], u32> = HashMap::new();
        let mut lengths: Vec<u128> = vec![0];
        let mut dictionary: Vec<u8> = Vec::new();
        let mut indices: Vec<u128> = Vec::new();
        let mut longest = 0;
        let mut row = start;
        while row < strings.len() {
            let string = strings[row];
            if string.len() >= limit {
                return None;
            }
            let known = !valid[row] || positions.contains_key(string);
            let new_size = dictionary.len() + if known { 0 } else { string.len() };
            let new_count = lengths.len() + usize::from(!known);
            let new_longest = longest.max(string.len());
            if row > start && size(new_size, new_count, new_longest, row + 1 - start) + 8 > block_size {
                break;
            }
            let index = if !valid[row] {
                0
            } else if let Some(&index) = positions.get(string) {
                index
            } else {
                let index = lengths.len() as u32;
                positions.insert(string, index);
                lengths.push(string.len() as u128);
                dictionary.extend_from_slice(string);
                longest = new_longest;
                index
            };
            indices.push(u128::from(index));
            row += 1;
        }
        let lengths_width = bit_width(longest as u64);
        if lengths_width == 0 && lengths.len() > 2 {
            return None;
        }
        let indices_width = bit_width(lengths.len() as u64 - 1);
        let mut segment = Vec::new();
        segment.extend_from_slice(&(dictionary.len() as u32).to_le_bytes());
        segment.extend_from_slice(&(lengths.len() as u32).to_le_bytes());
        segment.extend_from_slice(&[0, lengths_width as u8, indices_width as u8, 0]);
        segment.extend_from_slice(&0u32.to_le_bytes());
        segment.extend_from_slice(&dictionary);
        segment.resize(segment.len().next_multiple_of(8), 0);
        segment.extend_from_slice(&pack(&lengths, lengths_width));
        segment.resize(segment.len().next_multiple_of(8), 0);
        segment.extend_from_slice(&pack(&indices, indices_width));
        segments.push((segment, row - start));
        start = row;
    }
    Some(segments)
}

/// What each of these can hold. C++ also packs HUGEINT, by a packer of its own.
pub fn supports_rle(physical: PhysicalType) -> bool {
    use PhysicalType::*;
    matches!(
        physical,
        Bool | Int8 | Int16 | Int32 | Int64 | Int128 | UInt8 | UInt16 | UInt32 | UInt64 | UInt128 | Float | Double
    )
}

pub fn supports_bitpacking(physical: PhysicalType) -> bool {
    use PhysicalType::*;
    matches!(
        physical,
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64
    )
}

pub fn is_signed(physical: PhysicalType) -> bool {
    matches!(
        physical,
        PhysicalType::Int8 | PhysicalType::Int16 | PhysicalType::Int32 | PhysicalType::Int64
    )
}
