//! Ported from duckdb/src/storage/compression/{rle,bitpacking}.cpp @ rust-port-baseline.
//!
//! The encoders of the codecs that `compression.rs` decodes. Each takes the fixed-size values of
//! rows and makes segments that fit in a block, with how many rows each holds.

use super::compression::{ALP_FRAC_F32, ALP_FRAC_F64};
use super::fsst::SymbolTable;
use duckrs_common::types::PhysicalType;
use std::collections::HashMap;

/// The packer of C++ fills groups of this many values, also for fewer.
const PACKING_ROWS: usize = 32;
const HEADER_SIZE: usize = 8;

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

pub(super) fn pack(values: &[u128], bits: usize) -> Vec<u8> {
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

/// `GetCompactionFlushLimit`: a segment that fills less of a block than this is moved to its start.
fn compaction_limit(block_size: usize) -> usize {
    block_size / 5 * 4
}

/// `BitpackingPrimitives::GetRequiredSize`.
fn packed_bytes(count: usize, width: usize) -> usize {
    count.next_multiple_of(PACKING_ROWS) * width / 8
}

/// The strings of a segment, from its end down, and where the dictionary ends (`StringDictionaryContainer`).
/// A segment that fills a block keeps them at the end; another one ends right after them.
fn place_dictionary(mut head: Vec<u8>, dictionary: &[u8], block_size: usize) -> (Vec<u8>, usize) {
    let total = head.len() + dictionary.len();
    if total >= compaction_limit(block_size) {
        head.resize(block_size - dictionary.len(), 0);
        head.extend_from_slice(dictionary);
        (head, block_size)
    } else {
        head.extend_from_slice(dictionary);
        (head, total)
    }
}

/// `DictionaryCompressionCompressState`, the DICTIONARY of files before v1.3.0: per row the index of its
/// string, where 0 is NULL, a table of where each string ends, and the strings from the end of the segment
/// down. `None` where a string is too long for it.
pub fn encode_old_dictionary(
    strings: &[&[u8]],
    valid: &[bool],
    block_size: usize,
    limit: usize,
) -> Option<Vec<Encoded>> {
    const HEADER: usize = 20;
    let required = |count: usize, index_count: usize, dict_size: usize, width: usize| {
        HEADER + dict_size + index_count * 4 + packed_bytes(count, width)
    };
    let mut segments = Vec::new();
    let mut row = 0;
    while row < strings.len() || segments.is_empty() {
        let mut positions: HashMap<&[u8], u32> = HashMap::new();
        let mut index_buffer: Vec<u32> = vec![0];
        let mut selection: Vec<u128> = Vec::new();
        // in the order the strings came; the segment has them the other way round
        let mut added: Vec<&[u8]> = Vec::new();
        let (mut dict_size, mut width) = (0usize, 0usize);
        while row < strings.len() {
            let string = strings[row];
            if valid[row] && string.len() >= limit {
                return None;
            }
            let known = !valid[row] || positions.contains_key(string);
            let (new_count, new_size, new_width) = if known {
                (index_buffer.len(), dict_size, width)
            } else {
                (
                    index_buffer.len() + 1,
                    dict_size + string.len(),
                    bit_width(index_buffer.len() as u64),
                )
            };
            if required(selection.len() + 1, new_count, new_size, new_width) > block_size {
                if selection.is_empty() {
                    return None;
                }
                break;
            }
            let index = if !valid[row] {
                0
            } else if let Some(&index) = positions.get(string) {
                index
            } else {
                dict_size += string.len();
                index_buffer.push(dict_size as u32);
                added.push(string);
                let index = (index_buffer.len() - 1) as u32;
                positions.insert(string, index);
                width = new_width;
                index
            };
            selection.push(u128::from(index));
            row += 1;
        }
        let count = selection.len();
        let index_offset = HEADER + packed_bytes(count, width);
        let mut head = vec![0u8; HEADER];
        head.extend_from_slice(&pack(&selection, width)[..packed_bytes(count, width)]);
        for offset in &index_buffer {
            head.extend_from_slice(&offset.to_le_bytes());
        }
        let dictionary: Vec<u8> = added.iter().rev().flat_map(|string| string.iter().copied()).collect();
        let (mut segment, dict_end) = place_dictionary(head, &dictionary, block_size);
        for (at, value) in [dict_size, dict_end, index_offset, index_buffer.len(), width]
            .iter()
            .enumerate()
        {
            segment[at * 4..at * 4 + 4].copy_from_slice(&(*value as u32).to_le_bytes());
        }
        segments.push((segment, count));
        if row >= strings.len() {
            break;
        }
    }
    Some(segments)
}

/// `FSSTCompressionState`, the FSST of files before v1.3.0: per row how long its compressed string is, the
/// symbol table, and the strings from the end of the segment down. NULLs and empty strings have no bytes.
pub fn encode_old_fsst(
    strings: &[&[u8]],
    valid: &[bool],
    block_size: usize,
    limit: usize,
    table: &SymbolTable,
) -> Option<Vec<Encoded>> {
    const HEADER: usize = 16;
    let symbols = table.export();
    let mut segments = Vec::new();
    let mut row = 0;
    let mut entry = Vec::new();
    while row < strings.len() || segments.is_empty() {
        let mut lengths: Vec<u128> = Vec::new();
        let mut added: Vec<Vec<u8>> = Vec::new();
        let (mut dict_size, mut longest) = (0usize, 0usize);
        while row < strings.len() {
            let string = strings[row];
            if valid[row] && string.len() >= limit {
                return None;
            }
            if valid[row] && !string.is_empty() {
                table.compress(string, &mut entry);
            } else {
                entry.clear();
            }
            let width = bit_width(longest.max(entry.len()) as u64);
            let required = HEADER + dict_size + packed_bytes(lengths.len() + 1, width) + entry.len() + symbols.len();
            if required > block_size {
                if lengths.is_empty() {
                    return None;
                }
                break;
            }
            longest = longest.max(entry.len());
            dict_size += entry.len();
            lengths.push(entry.len() as u128);
            added.push(entry.clone());
            row += 1;
        }
        let count = lengths.len();
        let width = bit_width(longest as u64);
        let table_offset = HEADER + packed_bytes(count, width);
        let mut head = vec![0u8; HEADER];
        head.extend_from_slice(&pack(&lengths, width)[..packed_bytes(count, width)]);
        head.extend_from_slice(&symbols);
        let dictionary: Vec<u8> = added.iter().rev().flatten().copied().collect();
        let full = head.len() + dictionary.len() >= compaction_limit(block_size);
        let (mut segment, dict_end) = place_dictionary(head, &dictionary, block_size);
        // a full block keeps the dictionary of a new segment in its header (`SetDictionary` isn't called)
        let (size_field, end_field) = if full { (0, block_size) } else { (dict_size, dict_end) };
        for (at, value) in [size_field, end_field, width, table_offset].iter().enumerate() {
            segment[at * 4..at * 4 + 4].copy_from_slice(&(*value as u32).to_le_bytes());
        }
        segments.push((segment, count));
        if row >= strings.len() {
            break;
        }
    }
    Some(segments)
}

/// `AlpConstants::ALP_VECTOR_SIZE`.
const ALP_VECTOR_ROWS: usize = 1024;
/// How many values of a vector decide which of the candidates it takes.
const ALP_SAMPLES: usize = 32;
/// How many pairs of exponent and factor a row group keeps.
const ALP_CANDIDATES: usize = 5;

/// The floats of a column, as bits that don't lose anything.
trait AlpFloat: Copy {
    const WIDTH: usize;
    const MAX_EXPONENT: u8;
    fn read(bytes: &[u8]) -> Self;
    /// `AlpCompression::EncodeValue`, if the value has a chance.
    fn encode(self, exponent: u8, factor: u8) -> Option<i64>;
    /// As `scan_alp` does it: what comes back has to be this value, bit for bit.
    fn decode(encoded: i64, exponent: u8, factor: u8) -> Self;
    fn same_bits(self, other: Self) -> bool;
}

macro_rules! alp_float {
    ($t:ty, $width:expr, $max:expr, $frac:expr) => {
        impl AlpFloat for $t {
            const WIDTH: usize = $width;
            const MAX_EXPONENT: u8 = $max;
            fn read(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes.try_into().expect("the width of the type"))
            }
            fn encode(self, exponent: u8, factor: u8) -> Option<i64> {
                let scaled = f64::from(self) * 10f64.powi(i32::from(exponent)) * 10f64.powi(-i32::from(factor));
                if !scaled.is_finite() || scaled.abs() >= 9.0e18 {
                    return None;
                }
                Some(scaled.round() as i64)
            }
            fn decode(encoded: i64, exponent: u8, factor: u8) -> Self {
                let factor = 10i64.wrapping_pow(u32::from(factor));
                encoded as $t * factor as $t * $frac[usize::from(exponent)]
            }
            fn same_bits(self, other: Self) -> bool {
                self.to_bits() == other.to_bits()
            }
        }
    };
}
alp_float!(f64, 8, 18, ALP_FRAC_F64);
alp_float!(f32, 4, 10, ALP_FRAC_F32);

/// What a pair of exponent and factor makes of some values: the integers, or `None` for exceptions.
fn alp_try<T: AlpFloat>(values: &[T], exponent: u8, factor: u8) -> Vec<Option<i64>> {
    values
        .iter()
        .map(|&value| {
            value
                .encode(exponent, factor)
                .filter(|&encoded| T::decode(encoded, exponent, factor).same_bits(value))
        })
        .collect()
}

/// The size in bits of values, with what doesn't encode as exceptions.
fn alp_cost<T: AlpFloat>(encoded: &[Option<i64>]) -> usize {
    let mut range: Option<(i64, i64)> = None;
    let mut exceptions = 0;
    for value in encoded {
        match value {
            Some(value) => {
                let (low, high) = range.unwrap_or((*value, *value));
                range = Some((low.min(*value), high.max(*value)));
            }
            None => exceptions += 1,
        }
    }
    let bits = range.map_or(0, |(low, high)| bit_width(high.wrapping_sub(low) as u64));
    bits * encoded.len() + exceptions * (T::WIDTH + 2) * 8
}

/// `AlpCompression::FindTopKCombinations`: over a sample of the values.
fn alp_candidates<T: AlpFloat>(values: &[T]) -> Vec<(u8, u8)> {
    let step = (values.len() / 512).max(1);
    let sample: Vec<T> = values.iter().copied().step_by(step).collect();
    let mut scored = Vec::new();
    for exponent in 0..=T::MAX_EXPONENT {
        for factor in 0..=exponent {
            scored.push((alp_cost::<T>(&alp_try(&sample, exponent, factor)), exponent, factor));
        }
    }
    // for the same cost the larger exponent and factor, as C++ prefers
    scored.sort_by_key(|&(cost, exponent, factor)| (cost, std::cmp::Reverse((exponent, factor))));
    scored.truncate(ALP_CANDIDATES);
    scored
        .into_iter()
        .map(|(_, exponent, factor)| (exponent, factor))
        .collect()
}

fn alp_vector<T: AlpFloat>(values: &[T], raw: &[u8], candidates: &[(u8, u8)], fallback: bool) -> Vec<u8> {
    const UNCOMPRESSED: u8 = 255;
    let step = (values.len() / ALP_SAMPLES).max(1);
    let sample: Vec<T> = values.iter().copied().step_by(step).collect();
    let &(exponent, factor) = candidates
        .iter()
        .min_by_key(|&&(exponent, factor)| alp_cost::<T>(&alp_try(&sample, exponent, factor)))
        .expect("a row group has candidates");
    let encoded = alp_try(values, exponent, factor);
    let filler = encoded.iter().flatten().next().copied().unwrap_or(0);
    let low = encoded.iter().flatten().copied().min().unwrap_or(0);
    let high = encoded.iter().flatten().copied().max().unwrap_or(0);
    let bits = bit_width(high.wrapping_sub(low) as u64);
    let differences: Vec<u128> = encoded
        .iter()
        .map(|value| u128::from(value.unwrap_or(filler).wrapping_sub(low) as u64))
        .collect();
    let exceptions: Vec<usize> = (0..values.len()).filter(|&row| encoded[row].is_none()).collect();
    let mut vector = vec![exponent, factor];
    vector.extend_from_slice(&(exceptions.len() as u16).to_le_bytes());
    vector.extend_from_slice(&low.to_le_bytes());
    vector.push(bits as u8);
    vector.extend_from_slice(&pack(&differences, bits));
    for &row in &exceptions {
        vector.extend_from_slice(&raw[row * T::WIDTH..(row + 1) * T::WIDTH]);
    }
    for &row in &exceptions {
        vector.extend_from_slice(&(row as u16).to_le_bytes());
    }
    if fallback && vector.len() > raw.len() {
        let mut plain = vec![UNCOMPRESSED];
        plain.extend_from_slice(raw);
        return plain;
    }
    vector
}

/// Vectors, and at the end of the segment, from the back, where each starts. `header` comes first.
fn alp_segments(header: &[u8], vectors: Vec<(Vec<u8>, usize)>, block_size: usize) -> Vec<Encoded> {
    let mut segments = Vec::new();
    let mut body = header.to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    let mut rows = 0;
    let finish = |body: &mut Vec<u8>, offsets: &mut Vec<u32>, rows: &mut usize, segments: &mut Vec<Encoded>| {
        if *rows == 0 {
            return;
        }
        body.resize(body.len().next_multiple_of(8), 0);
        for offset in offsets.iter().rev() {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        let end = body.len() as u32;
        body[..4].copy_from_slice(&end.to_le_bytes());
        segments.push((std::mem::replace(body, header.to_vec()), *rows));
        offsets.clear();
        *rows = 0;
    };
    for (vector, vector_rows) in vectors {
        let at = body.len().next_multiple_of(8);
        if (at + vector.len()).next_multiple_of(8) + (offsets.len() + 1) * 4 > block_size {
            finish(&mut body, &mut offsets, &mut rows, &mut segments);
        }
        let at = body.len().next_multiple_of(8);
        body.resize(at, 0);
        offsets.push(at as u32);
        body.extend_from_slice(&vector);
        rows += vector_rows;
    }
    finish(&mut body, &mut offsets, &mut rows, &mut segments);
    segments
}

fn encode_alp_typed<T: AlpFloat>(data: &[u8], block_size: usize, fallback: bool) -> Vec<Encoded> {
    let values: Vec<T> = data.chunks_exact(T::WIDTH).map(T::read).collect();
    let candidates = alp_candidates(&values);
    let vectors = values
        .chunks(ALP_VECTOR_ROWS)
        .zip(data.chunks(ALP_VECTOR_ROWS * T::WIDTH))
        .map(|(values, raw)| (alp_vector(values, raw, &candidates, fallback), values.len()))
        .collect();
    alp_segments(&[0; 4], vectors, block_size)
}

/// `AlpCompressionState`: floats that are decimals, as integers with an exponent and a factor. `fallback`:
/// a vector that doesn't get smaller is stored as it is, which files have since v1.5.0.
pub fn encode_alp(data: &[u8], width: usize, block_size: usize, fallback: bool) -> Vec<Encoded> {
    if width == 8 {
        encode_alp_typed::<f64>(data, block_size, fallback)
    } else {
        encode_alp_typed::<f32>(data, block_size, fallback)
    }
}

/// The left parts that are most common, for this cut of the bits.
fn alprd_dictionary(values: &[u64], right: usize) -> Vec<u16> {
    const DICTIONARY_SIZE: usize = 8;
    let mut counts: HashMap<u16, usize> = HashMap::new();
    for value in values {
        *counts.entry((value >> right) as u16).or_default() += 1;
    }
    let mut common: Vec<(u16, usize)> = counts.into_iter().collect();
    common.sort_by_key(|&(left, count)| (std::cmp::Reverse(count), left));
    common.truncate(DICTIONARY_SIZE);
    common.into_iter().map(|(left, _)| left).collect()
}

/// `AlpRDCompressionState`: the low bits as they are, and the high bits through a dictionary, which
/// suits floats that aren't decimals. A value whose high bits aren't in it is an exception.
pub fn encode_alprd(data: &[u8], width: usize, block_size: usize, fallback: bool) -> Vec<Encoded> {
    /// `AlpRDConstants::CUTTING_LIMIT`.
    const CUTTING_LIMIT: usize = 16;
    const UNCOMPRESSED: u16 = u16::MAX;
    let bits = width * 8;
    let values: Vec<u64> = data
        .chunks_exact(width)
        .map(|value| {
            let mut bytes = [0; 8];
            bytes[..width].copy_from_slice(value);
            u64::from_le_bytes(bytes)
        })
        .collect();
    let sample: Vec<u64> = values.iter().copied().step_by((values.len() / 1024).max(1)).collect();
    let cost = |right: usize| {
        let dictionary = alprd_dictionary(&sample, right);
        let misses = sample
            .iter()
            .filter(|value| !dictionary.contains(&((*value >> right) as u16)))
            .count();
        right * sample.len() + misses * 32 + bit_width(dictionary.len() as u64 - 1) * sample.len()
    };
    let right = ((bits - CUTTING_LIMIT)..bits)
        .min_by_key(|&right| cost(right))
        .expect("a range of cuts");
    let dictionary = alprd_dictionary(&values, right);
    let left_bits = bit_width(dictionary.len() as u64 - 1);
    let positions: HashMap<u16, usize> = dictionary
        .iter()
        .enumerate()
        .map(|(index, &left)| (left, index))
        .collect();
    let mut header = vec![0; 4];
    header.extend_from_slice(&[right as u8, left_bits as u8, dictionary.len() as u8]);
    for entry in &dictionary {
        header.extend_from_slice(&entry.to_le_bytes());
    }
    let right_mask = (1u64 << right) - 1;
    let vectors = values
        .chunks(ALP_VECTOR_ROWS)
        .zip(data.chunks(ALP_VECTOR_ROWS * width))
        .map(|(values, raw)| {
            let mut lefts = Vec::with_capacity(values.len());
            let mut rights = Vec::with_capacity(values.len());
            let mut exceptions = Vec::new();
            for (row, value) in values.iter().enumerate() {
                let left = (value >> right) as u16;
                match positions.get(&left) {
                    Some(&index) => lefts.push(index as u128),
                    None => {
                        lefts.push(0);
                        exceptions.push((row, left));
                    }
                }
                rights.push(u128::from(value & right_mask));
            }
            let mut vector = (exceptions.len() as u16).to_le_bytes().to_vec();
            vector.extend_from_slice(&pack(&lefts, left_bits));
            vector.extend_from_slice(&pack(&rights, right));
            for (_, left) in &exceptions {
                vector.extend_from_slice(&left.to_le_bytes());
            }
            for (row, _) in &exceptions {
                vector.extend_from_slice(&(*row as u16).to_le_bytes());
            }
            if fallback && vector.len() > raw.len() + 1 {
                vector = UNCOMPRESSED.to_le_bytes().to_vec();
                vector.extend_from_slice(raw);
            }
            (vector, values.len())
        })
        .collect();
    alp_segments(&header, vectors, block_size)
}

/// What each of these can hold. C++ also packs HUGEINT, by a packer of its own.
pub fn supports_rle(physical: PhysicalType) -> bool {
    use PhysicalType::*;
    matches!(
        physical,
        Bool | Int8 | Int16 | Int32 | Int64 | Int128 | UInt8 | UInt16 | UInt32 | UInt64 | UInt128 | Float | Double
    )
}

/// `BitpackingFun::TypeIsSupported` (LIST offsets are written uncompressed here).
pub fn supports_bitpacking(physical: PhysicalType) -> bool {
    use PhysicalType::*;
    matches!(
        physical,
        Bool | Int8 | Int16 | Int32 | Int64 | Int128 | UInt8 | UInt16 | UInt32 | UInt64 | UInt128
    )
}

/// Whether the type packed for `physical` is signed: BOOL is packed as `int8_t`.
pub fn is_signed(physical: PhysicalType) -> bool {
    matches!(
        physical,
        PhysicalType::Bool
            | PhysicalType::Int8
            | PhysicalType::Int16
            | PhysicalType::Int32
            | PhysicalType::Int64
            | PhysicalType::Int128
    )
}
