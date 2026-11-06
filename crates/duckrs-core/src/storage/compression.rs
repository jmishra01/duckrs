//! The scans of duckdb/src/storage/compression/*.cpp @ rust-port-baseline: how a column segment's
//! bytes become values. Everything here comes from a file, so it is bounds-checked and reports a
//! damaged file as an error, as C++ does since `CompressionSegmentReader`.
//!
//! These decode a whole segment. Scanning part of one, and doing so vectorized, comes with
//! scanning tables in place.

use super::block_manager::{corrupt, corrupted};
use super::statistics::BaseStatistics;
use duckrs_common::{Error, ErrorKind, Result};

/// `CompressionType`.
pub const COMPRESSION_UNCOMPRESSED: u8 = 1;
pub const COMPRESSION_CONSTANT: u8 = 2;
pub const COMPRESSION_RLE: u8 = 3;
pub const COMPRESSION_DICTIONARY: u8 = 4;
pub const COMPRESSION_BITPACKING: u8 = 6;
pub const COMPRESSION_FSST: u8 = 7;
pub const COMPRESSION_ALP: u8 = 10;
pub const COMPRESSION_ALPRD: u8 = 11;
pub const COMPRESSION_ROARING: u8 = 13;
pub const COMPRESSION_EMPTY: u8 = 14;
pub const COMPRESSION_DICT_FSST: u8 = 15;

/// `BITPACKING_ALGORITHM_GROUP_SIZE`.
const ALGORITHM_GROUP_SIZE: usize = 32;
/// `BITPACKING_METADATA_GROUP_SIZE`.
const METADATA_GROUP_SIZE: usize = 2048;

pub fn compression_name(compression: u8) -> &'static str {
    match compression {
        8 => "Chimp",
        9 => "Patas",
        12 => "ZSTD",
        _ => "this",
    }
}

/// `CompressionTypeToString`.
pub fn storage_name(compression: u8) -> &'static str {
    match compression {
        COMPRESSION_UNCOMPRESSED => "Uncompressed",
        COMPRESSION_CONSTANT => "Constant",
        COMPRESSION_RLE => "RLE",
        COMPRESSION_DICTIONARY => "Dictionary",
        COMPRESSION_BITPACKING => "BitPacking",
        COMPRESSION_FSST => "FSST",
        8 => "Chimp",
        9 => "Patas",
        COMPRESSION_ALP => "ALP",
        COMPRESSION_ALPRD => "ALPRD",
        12 => "ZSTD",
        COMPRESSION_ROARING => "Roaring",
        COMPRESSION_EMPTY => "Empty Validity",
        COMPRESSION_DICT_FSST => "DICT_FSST",
        _ => "Unknown",
    }
}

pub fn not_ported(compression: u8) -> Error {
    Error::not_implemented(format!(
        "reading {} compression from a database file is not yet ported to duckrs",
        compression_name(compression)
    ))
}

fn out_of_bounds(what: &str) -> Error {
    corrupt(format!("{what} is out of bounds"))
}

/// A sub-slice, which fails rather than panics if the file is damaged.
fn bytes<'a>(data: &'a [u8], offset: usize, size: usize, what: &str) -> Result<&'a [u8]> {
    offset
        .checked_add(size)
        .and_then(|end| data.get(offset..end))
        .ok_or_else(|| out_of_bounds(what))
}

fn u32_at(data: &[u8], offset: usize, what: &str) -> Result<u32> {
    let b = bytes(data, offset, 4, what)?;
    Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

fn u64_at(data: &[u8], offset: usize, what: &str) -> Result<u64> {
    let b = bytes(data, offset, 8, what)?;
    Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

/// `FixedSizeScan`.
pub fn scan_uncompressed(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    let past_the_end = || corrupted("Corrupted fixed-size segment: array count extends past the end of the segment");
    let size = count.checked_mul(width).ok_or_else(past_the_end)?;
    out.extend_from_slice(segment.get(..size).ok_or_else(past_the_end)?);
    Ok(())
}

/// `ConstantScanFunction`: the value of the segment is in its statistics.
pub fn scan_constant(stats: &BaseStatistics, count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    let min = stats
        .min
        .ok_or_else(|| corrupt("a constant segment has no value in its statistics"))?;
    for _ in 0..count {
        out.extend_from_slice(&min[..width]);
    }
    Ok(())
}

/// `ValidityScan`: a set bit means a value.
pub fn scan_validity(segment: &[u8], count: usize, out: &mut Vec<bool>) -> Result<()> {
    let words = segment
        .get(..count.div_ceil(64) * 8)
        .ok_or_else(|| corrupted("Corrupted validity segment: array count extends past the end of the segment"))?;
    for i in 0..count {
        out.push(words[i / 8] >> (i % 8) & 1 != 0);
    }
    Ok(())
}

/// `RLEScan`: a table of values and one of how often each repeats.
pub fn scan_rle(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    const RLE_HEADER_SIZE: usize = 8;
    let counts_at = u64_at(segment, 0, "the header of an RLE segment")? as usize;
    if counts_at < RLE_HEADER_SIZE || counts_at > segment.len() {
        return Err(Error::new(
            ErrorKind::DataCorruption,
            "Corrupted RLE segment: rle_count_offset is corrupted",
        ));
    }
    let entries = ((counts_at - RLE_HEADER_SIZE) / width).min((segment.len() - counts_at) / 2);
    let mut remaining = count;
    let mut entry = 0;
    while remaining > 0 {
        if entry >= entries {
            return Err(corrupted(
                "Corrupted RLE segment: run counts do not cover the segment row count",
            ));
        }
        let run = bytes(segment, counts_at + entry * 2, 2, "an RLE run count")?;
        let run = usize::from(u16::from_le_bytes(run.try_into().expect("2 bytes")));
        if run > remaining {
            return Err(corrupt("an RLE run exceeds the rows of the segment"));
        }
        let value = bytes(segment, RLE_HEADER_SIZE + entry * width, width, "an RLE value")?;
        for _ in 0..run {
            out.extend_from_slice(value);
        }
        remaining -= run;
        entry += 1;
    }
    Ok(())
}

/// The `width`-bit value at bit `position`. The packed layouts are one stream of bits, least
/// significant first, whatever the integer type.
fn read_bits(data: &[u8], position: usize, width: usize) -> u128 {
    let mut result = 0u128;
    let mut read = 0;
    while read < width {
        let bit = position + read;
        let take = (8 - bit % 8).min(width - read);
        let byte = u128::from(data[bit / 8] >> (bit % 8)) & ((1 << take) - 1);
        result |= byte << read;
        read += take;
    }
    result
}

/// The number of bytes the groups of `count` values at `width` bits take.
fn packed_size(count: usize, width: usize) -> usize {
    count.div_ceil(ALGORITHM_GROUP_SIZE) * (ALGORITHM_GROUP_SIZE * width / 8)
}

/// `count` packed values.
fn unpack(data: &[u8], count: usize, width: usize, what: &str) -> Result<Vec<u128>> {
    bytes(data, 0, packed_size(count, width), what)?;
    Ok((0..count).map(|i| read_bits(data, i * width, width)).collect())
}

/// `BitpackingScanPartial`. A segment is groups of 2048 values, each with its own mode, and a table
/// at the end of where they are, last group first.
pub fn scan_bitpacking(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    const HEADER_SIZE: usize = 8;
    const CONSTANT: u32 = 2;
    const CONSTANT_DELTA: u32 = 3;
    const DELTA_FOR: u32 = 4;
    const FOR: u32 = 5;

    let group_count = count.div_ceil(METADATA_GROUP_SIZE);
    let metadata_end = u64_at(segment, 0, "the header of a bitpacked segment")? as usize;
    let table_start = metadata_end
        .checked_sub(group_count * 4)
        .filter(|&start| start >= HEADER_SIZE)
        .ok_or_else(|| corrupted("Corrupted bitpacking segment: offset is outside the segment"))?;
    bytes(
        segment,
        table_start,
        group_count * 4,
        "the metadata of a bitpacked segment",
    )?;
    let group = |index: usize| -> Result<(usize, u32)> {
        let encoded = u32_at(
            segment,
            table_start + (group_count - 1 - index) * 4,
            "bitpacking metadata",
        )?;
        Ok(((encoded & 0xFFFFFF) as usize, encoded >> 24))
    };

    let bits = width * 8;
    let mask = if bits == 128 { u128::MAX } else { (1 << bits) - 1 };
    let number = |data: &[u8], at: usize| -> Result<u128> {
        let mut value = [0; 16];
        value[..width].copy_from_slice(bytes(data, at, width, "a value of a bitpacking group")?);
        Ok(u128::from_le_bytes(value))
    };

    for index in 0..group_count {
        let (offset, mode) = group(index)?;
        let end = if index + 1 < group_count {
            group(index + 1)?.0
        } else {
            table_start
        };
        if offset < HEADER_SIZE || offset > end || end > table_start {
            return Err(corrupted(
                "Corrupted bitpacking segment: group offsets do not describe a range in the data region",
            ));
        }
        let data = &segment[offset..end];
        let rows = METADATA_GROUP_SIZE.min(count - index * METADATA_GROUP_SIZE);
        let mut push = |value: u128| out.extend_from_slice(&(value & mask).to_le_bytes()[..width]);
        match mode {
            CONSTANT => {
                let value = number(data, 0)?;
                for _ in 0..rows {
                    push(value);
                }
            }
            CONSTANT_DELTA => {
                let (frame, delta) = (number(data, 0)?, number(data, width)?);
                for row in 0..rows {
                    push(delta.wrapping_mul(row as u128).wrapping_add(frame));
                }
            }
            FOR | DELTA_FOR => {
                let frame = number(data, 0)?;
                let packed_width = number(data, width)? as usize;
                if packed_width > bits {
                    return Err(corrupted(
                        "Corrupted bitpacking segment: bit width exceeds the physical type width",
                    ));
                }
                let (mut carry, header) = if mode == DELTA_FOR {
                    (number(data, width * 2)?, width * 3)
                } else {
                    (0, width * 2)
                };
                let packed = data.get(header..).ok_or_else(|| out_of_bounds("a bitpacking group"))?;
                for value in unpack(packed, rows, packed_width, "a bitpacking group")? {
                    let value = value.wrapping_add(frame);
                    if mode == DELTA_FOR {
                        carry = value.wrapping_add(carry);
                        push(carry);
                    } else {
                        push(value);
                    }
                }
            }
            other => return Err(corrupt(format!("unknown bitpacking mode {other}"))),
        }
    }
    Ok(())
}

/// Reads the block of an overflow string.
pub type PinBlock<'a> = dyn Fn(u64) -> Result<std::sync::Arc<Vec<u8>>> + 'a;

/// `ReadOverflowString`: a length, and the string, over as many blocks as it takes. The last 8
/// bytes of such a block say which block is next.
fn read_overflow_string(pin: &PinBlock<'_>, block: i64, offset: i32) -> Result<Vec<u8>> {
    let (Ok(mut block_id), Ok(mut position)) = (u64::try_from(block), usize::try_from(offset)) else {
        return Err(out_of_bounds("an overflow string"));
    };
    let mut handle = pin(block_id)?;
    let space = handle.len() - 8;
    let length = u32_at(&handle[..space], position, "the length of an overflow string")? as usize;
    position += 4;
    let mut result = Vec::with_capacity(length.min(1 << 20));
    while result.len() < length {
        let take = (length - result.len()).min(space.saturating_sub(position));
        result.extend_from_slice(bytes(&handle[..space], position, take, "an overflow string")?);
        if result.len() < length {
            block_id = u64_at(&handle, space, "the next block of an overflow string")?;
            handle = pin(block_id)?;
            position = 0;
        }
    }
    Ok(result)
}

/// `StringScan`: offsets, counted back from the end of the dictionary. A negative one is a marker
/// of a string that didn't fit, or NULL.
pub fn scan_string_uncompressed(
    segment: &[u8],
    count: usize,
    pin: &PinBlock<'_>,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    const HEADER_SIZE: usize = 8;
    const BIG_STRING_MARKER_SIZE: usize = 12;
    let dictionary_end = u32_at(segment, 4, "the header of a string segment")? as usize;
    let offsets = bytes(
        segment,
        HEADER_SIZE,
        count.checked_mul(4).ok_or_else(|| out_of_bounds("a segment"))?,
        "string offsets",
    )?;
    let dictionary_start = HEADER_SIZE + count * 4;
    if dictionary_end < dictionary_start || dictionary_end > segment.len() {
        return Err(out_of_bounds("a string dictionary"));
    }
    let dictionary = &segment[dictionary_start..dictionary_end];
    let mut previous = 0u32;
    for offset in offsets.chunks_exact(4) {
        let offset = i32::from_le_bytes(offset.try_into().expect("4 bytes"));
        let current = offset.unsigned_abs();
        let length = current
            .checked_sub(previous)
            .ok_or_else(|| corrupt("string offsets decrease"))? as usize;
        previous = current;
        let start = dictionary
            .len()
            .checked_sub(current as usize)
            .ok_or_else(|| out_of_bounds("a string offset"))?;
        let entry = bytes(dictionary, start, length, "a string")?;
        if offset >= 0 || length == 0 {
            out.push(entry.to_vec());
        } else if length == BIG_STRING_MARKER_SIZE {
            let block = i64::from_le_bytes(entry[..8].try_into().expect("8 bytes"));
            let at = i32::from_le_bytes(entry[8..].try_into().expect("4 bytes"));
            out.push(read_overflow_string(pin, block, at)?);
        } else {
            return Err(corrupt("an invalid marker of an overflow string"));
        }
    }
    Ok(())
}

/// `CompressedStringScanState`: per row a packed index of a table of offsets into a dictionary,
/// where index 0 is NULL.
pub fn scan_dictionary(segment: &[u8], count: usize, out: &mut Vec<Vec<u8>>) -> Result<()> {
    const HEADER_SIZE: usize = 20;
    let what = "the header of a dictionary segment";
    let dict_size = u32_at(segment, 0, what)? as usize;
    let dict_end = u32_at(segment, 4, what)? as usize;
    let index_offset = u32_at(segment, 8, what)? as usize;
    let index_count = u32_at(segment, 12, what)? as usize;
    let width = u32_at(segment, 16, what)? as usize;
    if index_count == 0 || index_offset < HEADER_SIZE {
        return Err(out_of_bounds("a dictionary"));
    }
    // selections refer to entries of the offset table, so its size says how wide they are
    if width != (usize::BITS - (index_count - 1).leading_zeros()) as usize {
        return Err(corrupt("the bitpacking width of a dictionary segment is invalid"));
    }
    if index_offset - HEADER_SIZE != packed_size(count, width) {
        return Err(corrupted(
            "Failed to scan dictionary string - selection buffer was out of range",
        ));
    }
    let table = bytes(segment, index_offset, index_count * 4, "a dictionary offset table")?;
    if dict_size > dict_end || dict_end - dict_size < index_offset + index_count * 4 {
        return Err(out_of_bounds("a dictionary"));
    }
    let table: Vec<u32> = table
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .collect();
    let start = dict_end
        .checked_sub(dict_size)
        .ok_or_else(|| out_of_bounds("a dictionary"))?;
    let dictionary = bytes(segment, start, dict_size, "a dictionary")?;
    let selection = bytes(
        segment,
        HEADER_SIZE,
        index_offset - HEADER_SIZE,
        "dictionary selections",
    )?;
    for index in unpack(selection, count, width, "dictionary selections")? {
        let index = index as usize;
        if index == 0 {
            out.push(Vec::new());
            continue;
        }
        let (&current, &previous) = table
            .get(index)
            .zip(table.get(index - 1))
            .ok_or_else(|| corrupted("Failed to scan dictionary string - dictionary index was out of range"))?;
        let length = current
            .checked_sub(previous)
            .ok_or_else(|| corrupt("dictionary offsets decrease"))?;
        let from = dictionary
            .len()
            .checked_sub(current as usize)
            .ok_or_else(|| corrupted("Failed to scan dictionary string - dictionary offset was out of range. Database file appears to be corrupted."))?;
        out.push(bytes(dictionary, from, length as usize, "a dictionary string")?.to_vec());
    }
    Ok(())
}

/// `duckdb_fsst_decoder_t`.
struct FsstDecoder {
    symbols: [[u8; 8]; 255],
    lengths: [u8; 255],
    /// `duckdb_fsst_import`'s result: the size of the table.
    size: usize,
}

/// `duckdb_fsst_import`: `None` if there is no table, which is how a segment of only empty strings
/// goes, as `FSST_VERSION` doesn't match then.
fn fsst_import(data: &[u8]) -> Result<Option<FsstDecoder>> {
    const FSST_VERSION: u64 = 20190218;
    let what = "an FSST symbol table";
    if data.len() < 17 {
        return Err(out_of_bounds(what));
    }
    if u64_at(data, 0, what)? >> 32 != FSST_VERSION {
        return Ok(None);
    }
    let zero_terminated = usize::from(data[8] & 1);
    let mut histogram: [u8; 8] = data[9..17].try_into().expect("8 bytes");
    let mut decoder = FsstDecoder {
        symbols: [*b"corrupt\0"; 255],
        lengths: [8; 255],
        size: 17,
    };
    decoder.symbols[0] = [0; 8];
    decoder.lengths[0] = 1;
    let mut code = zero_terminated;
    if zero_terminated == 1 {
        histogram[0] = histogram[0].wrapping_sub(1);
    }
    for l in 1..=8 {
        for _ in 0..histogram[l & 7] {
            if code >= 255 {
                return Err(out_of_bounds(what));
            }
            let length = (l & 7) + 1;
            let symbol = bytes(data, decoder.size, length, what)?;
            decoder.symbols[code] = [0; 8];
            decoder.symbols[code][..length].copy_from_slice(symbol);
            decoder.lengths[code] = length as u8;
            decoder.size += length;
            code += 1;
        }
    }
    Ok(Some(decoder))
}

impl FsstDecoder {
    /// `duckdb_fsst_decompress`.
    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>> {
        const ESCAPE: u8 = 255;
        let mut result = Vec::with_capacity(input.len() * 2);
        let mut bytes = input.iter();
        while let Some(&code) = bytes.next() {
            if code == ESCAPE {
                let &literal = bytes
                    .next()
                    .ok_or_else(|| corrupted("Failed to decompress FSST string - escape code has no following byte"))?;
                result.push(literal);
            } else {
                let code = usize::from(code);
                result.extend_from_slice(&self.symbols[code][..usize::from(self.lengths[code])]);
            }
        }
        Ok(result)
    }
}

/// What `compressed` is, through a table that `export` wrote.
#[cfg(test)]
pub fn fsst_roundtrip(table: &[u8], compressed: &[u8]) -> Vec<u8> {
    let decoder = fsst_import(table).unwrap().unwrap();
    decoder.decompress(compressed).unwrap()
}

/// `DEFAULT_STRING_BLOCK_LIMIT`, the most `StringUncompressed::GetStringBlockLimit` gives.
const STRING_BLOCK_LIMIT: usize = 4096;

/// `ThrowInvalidFSSTSegment`.
fn invalid_fsst(message: &str) -> Error {
    Error::new(
        ErrorKind::DataCorruption,
        format!("Failed to read FSST string segment - {message}. Database file appears to be corrupted."),
    )
}

/// `FSSTStorage::StringScan`: packed lengths of strings compressed with a table of up to 255
/// symbols of up to 8 bytes. The strings are stored from the end of the segment down.
pub fn scan_fsst(segment: &[u8], count: usize, out: &mut Vec<Vec<u8>>) -> Result<()> {
    const HEADER_SIZE: usize = 16;
    let what = "the header of an FSST segment";
    let dict_end = u32_at(segment, 4, what)? as usize;
    let width = u32_at(segment, 8, what)? as usize;
    let table_offset = u32_at(segment, 12, what)? as usize;
    if width > 32 {
        return Err(invalid_fsst("bitpacking width was invalid"));
    }
    if table_offset != HEADER_SIZE + packed_size(count, width) {
        return Err(invalid_fsst("bitpacking width did not match the stored layout"));
    }
    if dict_end < table_offset {
        return Err(invalid_fsst("dictionary was out of range"));
    }
    let packed = bytes(segment, HEADER_SIZE, packed_size(count, width), "FSST lengths")?;
    let lengths = unpack(packed, count, width, "FSST lengths")?;
    let table = bytes(segment, table_offset, dict_end - table_offset, "an FSST symbol table")?;
    let Some(decoder) = fsst_import(table)? else {
        if lengths.iter().any(|&length| length != 0) {
            return Err(corrupt("a nonempty FSST string has no symbol table"));
        }
        out.resize(out.len() + count, Vec::new());
        return Ok(());
    };
    let dictionary = &table[decoder.size..];
    let mut offset = 0usize;
    for length in lengths {
        let length = length as usize;
        offset += length;
        let from = dictionary
            .len()
            .checked_sub(offset)
            .ok_or_else(|| out_of_bounds("an FSST string"))?;
        let string = decoder.decompress(&dictionary[from..from + length])?;
        // what's longer goes to overflow blocks, so no valid segment has one of these
        if string.len() > STRING_BLOCK_LIMIT {
            return Err(corrupted(
                "Failed to decompress FSST string - decoded size exceeds the output buffer",
            ));
        }
        out.push(string);
    }
    Ok(())
}

/// `get_segment_info`: `DictFSSTGetSegmentInfo` and `BitpackingGetSegmentInfo`.
pub fn segment_info(compression: u8, segment: &[u8], count: usize) -> Result<Option<String>> {
    match compression {
        COMPRESSION_DICT_FSST => {
            let mode = bytes(segment, 8, 1, "a DICT_FSST segment")?[0];
            let name = ["DICTIONARY", "DICT_FSST", "FSST_ONLY"]
                .get(usize::from(mode))
                .ok_or_else(|| corrupt(format!("unrecognized DICT_FSST compression mode {mode}")))?;
            Ok(Some(format!("{name}: {count}")))
        }
        COMPRESSION_BITPACKING => {
            let groups = count.div_ceil(METADATA_GROUP_SIZE);
            let what = "the metadata of a bitpacked segment";
            let end = u64_at(segment, 0, what)? as usize;
            let start = end.checked_sub(groups * 4).ok_or_else(|| out_of_bounds(what))?;
            let mut counts = [0usize; 6];
            for group in bytes(segment, start, groups * 4, what)?.chunks_exact(4) {
                if let Some(mode) = counts.get_mut(usize::from(group[3])) {
                    *mode += 1;
                }
            }
            let names = [(2, "CONSTANT"), (3, "CONSTANT_DELTA"), (4, "DELTA_FOR"), (5, "FOR")];
            let modes = names.iter().filter(|(mode, _)| counts[*mode] > 0);
            let info: Vec<String> = modes.map(|(mode, name)| format!("{name}: {}", counts[*mode])).collect();
            Ok(Some(info.join(", ")))
        }
        _ => Ok(None),
    }
}

/// `AlpConstants::ALP_VECTOR_SIZE`.
/// The fixed part of an ALPRD segment.
const ALPRD_HEADER: usize = 7;
const ALP_VECTOR_SIZE: usize = 1024;
/// `AlpConstants::MAX_BIT_WIDTH`: the encoded values are 64 bits wide.
const ALP_MAX_BIT_WIDTH: usize = 64;

/// The table at the end of an ALP or ALPRD segment: where each vector starts, last vector first.
/// Yields the bytes of each vector. `codec` names it in what's reported, as DuckDB does.
fn alp_vectors<'a>(
    segment: &'a [u8],
    count: usize,
    data_start: usize,
    codec: &'static str,
) -> Result<impl Iterator<Item = Result<(usize, &'a [u8])>> + 'a> {
    let metadata_end = alp_metadata_end(segment, data_start.min(ALPRD_HEADER), codec)?;
    let vector_count = count.div_ceil(ALP_VECTOR_SIZE);
    let metadata_start = metadata_end
        .checked_sub(vector_count * 4)
        .filter(|&start| start >= data_start)
        .ok_or_else(|| alp_corrupt(codec, "metadata offset table exceeds the segment"))?;
    let table = &segment[metadata_start..metadata_end];
    let offset = move |index: usize| {
        let at = (vector_count - 1 - index) * 4;
        u32::from_le_bytes(table[at..at + 4].try_into().expect("4 bytes")) as usize
    };
    Ok((0..vector_count).map(move |index| {
        let start = offset(index);
        let end = if index + 1 < vector_count {
            offset(index + 1)
        } else {
            metadata_start
        };
        if start < data_start || end < data_start {
            return Err(alp_corrupt(codec, "vector offset is outside the data region"));
        }
        if start > end || end > metadata_start {
            return Err(alp_corrupt(codec, "vector offsets do not describe a data range"));
        }
        let rows = ALP_VECTOR_SIZE.min(count - index * ALP_VECTOR_SIZE);
        Ok((rows, &segment[start..end]))
    }))
}

/// `CompressionSegmentReader::ThrowForwardReadOutOfBounds`.
fn read_past(context: &str) -> Error {
    corrupted(&format!(
        "Corrupted {context}: attempted to read past the end of the segment"
    ))
}

/// `Corrupted ALP segment: ...`, from the checks in alp.cpp and alprd.cpp.
fn alp_corrupt(codec: &str, message: &str) -> Error {
    corrupted(&format!("Corrupted {codec} segment: {message}"))
}

/// Where the metadata of a segment ends, which is the end of what's in it.
fn alp_metadata_end(segment: &[u8], header: usize, codec: &str) -> Result<usize> {
    const READ_PAST: &str = "attempted to read past the end of the segment";
    let end = segment
        .first_chunk::<4>()
        .ok_or_else(|| alp_corrupt(codec, READ_PAST))?;
    let end = u32::from_le_bytes(*end) as usize;
    if end < header {
        return Err(alp_corrupt(codec, "metadata ends before the segment header"));
    }
    if end > segment.len() {
        return Err(alp_corrupt(
            codec,
            "requested range extends past the end of the segment",
        ));
    }
    Ok(end)
}

/// Takes `size` bytes off the front of `data`, which `context` names as C++ does.
fn advance<'a>(data: &mut &'a [u8], size: usize, context: &str) -> Result<&'a [u8]> {
    let (head, tail) = data.split_at_checked(size).ok_or_else(|| read_past(context))?;
    *data = tail;
    Ok(head)
}

/// The exceptions at the end of a vector: their values and where they go.
fn alp_exceptions<'a>(
    vector: &mut &'a [u8],
    count: usize,
    width: usize,
    rows: usize,
    codec: &str,
) -> Result<(&'a [u8], Vec<usize>)> {
    if count > rows {
        return Err(alp_corrupt(
            codec,
            &format!("exception count {count} exceeds vector size {rows}"),
        ));
    }
    let context = format!("{codec} vector");
    let values = advance(vector, count * width, &context)?;
    let positions = advance(vector, count * 2, &context)?
        .chunks_exact(2)
        .map(|b| usize::from(u16::from_le_bytes(b.try_into().expect("2 bytes"))))
        .collect::<Vec<_>>();
    if let Some(position) = positions.iter().find(|&&position| position >= rows) {
        return Err(alp_corrupt(
            codec,
            &format!("exception position {position} is outside vector size {rows}"),
        ));
    }
    Ok((values, positions))
}

/// `AlpTypedConstants<double>::FRAC_ARR`.
pub const ALP_FRAC_F64: [f64; 21] = [
    1.0,
    0.1,
    0.01,
    0.001,
    0.0001,
    0.00001,
    0.000001,
    0.0000001,
    0.00000001,
    0.000000001,
    0.0000000001,
    0.00000000001,
    0.000000000001,
    0.0000000000001,
    0.00000000000001,
    0.000000000000001,
    0.0000000000000001,
    0.00000000000000001,
    0.000000000000000001,
    0.0000000000000000001,
    0.00000000000000000001,
];
/// `AlpTypedConstants<float>::FRAC_ARR`.
pub const ALP_FRAC_F32: [f32; 11] = [
    1.0,
    0.1,
    0.01,
    0.001,
    0.0001,
    0.00001,
    0.000001,
    0.0000001,
    0.00000001,
    0.000000001,
    0.0000000001,
];

/// `AlpScanState`: floats written as decimals. Each vector of 1024 has an exponent and a factor,
/// and packed integers with a frame of reference; what doesn't round trip is an exception.
pub fn scan_alp(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    const UNCOMPRESSED_MODE: u8 = 255;
    const CODEC: &str = "ALP";
    const VECTOR: &str = "ALP vector";
    let max_exponent = if width == 8 { 18 } else { 10 };
    for vector in alp_vectors(segment, count, 4, CODEC)? {
        let (rows, mut vector) = vector?;
        let exponent = advance(&mut vector, 1, VECTOR)?[0];
        if exponent == UNCOMPRESSED_MODE {
            out.extend_from_slice(advance(&mut vector, rows * width, VECTOR)?);
            continue;
        }
        if exponent > max_exponent {
            return Err(alp_corrupt(
                CODEC,
                &format!("exponent {exponent} exceeds the maximum {max_exponent}"),
            ));
        }
        let factor = advance(&mut vector, 1, VECTOR)?[0];
        let exceptions = u16::from_le_bytes(advance(&mut vector, 2, VECTOR)?.try_into().expect("2 bytes"));
        if usize::from(exceptions) > rows {
            return Err(alp_corrupt(
                CODEC,
                &format!("exception count {exceptions} exceeds vector size {rows}"),
            ));
        }
        if factor > exponent {
            return Err(alp_corrupt(
                CODEC,
                &format!("factor {factor} exceeds exponent {exponent}"),
            ));
        }
        let frame = u64::from_le_bytes(advance(&mut vector, 8, VECTOR)?.try_into().expect("8 bytes"));
        let bit_width = usize::from(advance(&mut vector, 1, VECTOR)?[0]);
        if bit_width > ALP_MAX_BIT_WIDTH {
            return Err(alp_corrupt(
                CODEC,
                &format!("bit width {bit_width} exceeds {ALP_MAX_BIT_WIDTH}"),
            ));
        }
        let packed = advance(&mut vector, packed_size(rows, bit_width), VECTOR)?;
        let (values, positions) = alp_exceptions(&mut vector, usize::from(exceptions), width, rows, CODEC)?;

        let start = out.len();
        let factor = 10i64.wrapping_pow(u32::from(factor));
        for encoded in unpack(packed, rows, bit_width, "an ALP vector")? {
            let encoded = (encoded as u64).wrapping_add(frame) as i64;
            // the order of the operations is part of the format, as they round
            if width == 8 {
                let value = encoded as f64 * factor as f64 * ALP_FRAC_F64[usize::from(exponent)];
                out.extend_from_slice(&value.to_le_bytes());
            } else {
                let value = encoded as f32 * factor as f32 * ALP_FRAC_F32[usize::from(exponent)];
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        for (value, position) in values.chunks_exact(width).zip(positions) {
            out[start + position * width..start + (position + 1) * width].copy_from_slice(value);
        }
    }
    Ok(())
}

/// `AlpRDScanState`: floats split in two, the left parts through a small dictionary.
pub fn scan_alprd(segment: &[u8], count: usize, width: usize, out: &mut Vec<u8>) -> Result<()> {
    const UNCOMPRESSED_MODE: u16 = u16::MAX;
    const MAX_DICTIONARY_SIZE: usize = 8;
    const MAX_LEFT_WIDTH: usize = 3;
    const CODEC: &str = "ALPRD";
    const VECTOR: &str = "ALPRD vector";
    let what = "an ALPRD vector";
    alp_metadata_end(segment, ALPRD_HEADER, CODEC)?;
    let header = bytes(segment, 4, 3, "the header of an ALPRD segment")?;
    let (right_width, left_width, entries) = (usize::from(header[0]), usize::from(header[1]), usize::from(header[2]));
    let max_right_width = width * 8 - 1;
    if right_width > max_right_width {
        return Err(alp_corrupt(
            CODEC,
            &format!("right bit width {right_width} exceeds {max_right_width}"),
        ));
    }
    if left_width > MAX_LEFT_WIDTH {
        return Err(alp_corrupt(
            CODEC,
            &format!("left bit width {left_width} exceeds {MAX_LEFT_WIDTH}"),
        ));
    }
    if entries > MAX_DICTIONARY_SIZE {
        return Err(corrupted(
            "Corrupt database file: ALPRD dictionary size exceeds maximum",
        ));
    }
    let stored = bytes(segment, 7, entries * 2, "the dictionary of an ALPRD segment")?;
    // an index past the stored entries reads zero, which exceptions rely on
    let mut dictionary = [0u16; MAX_DICTIONARY_SIZE];
    for (entry, b) in dictionary.iter_mut().zip(stored.chunks_exact(2)) {
        *entry = u16::from_le_bytes(b.try_into().expect("2 bytes"));
    }
    for vector in alp_vectors(segment, count, 7 + entries * 2, CODEC)? {
        let (rows, mut vector) = vector?;
        let exceptions = u16::from_le_bytes(advance(&mut vector, 2, VECTOR)?.try_into().expect("2 bytes"));
        if exceptions == UNCOMPRESSED_MODE {
            out.extend_from_slice(advance(&mut vector, rows * width, VECTOR)?);
            continue;
        }
        if usize::from(exceptions) > rows {
            return Err(alp_corrupt(
                CODEC,
                &format!("exception count {exceptions} exceeds vector size {rows}"),
            ));
        }
        let left = advance(&mut vector, packed_size(rows, left_width), VECTOR)?;
        let left = unpack(left, rows, left_width, what)?;
        let right = advance(&mut vector, packed_size(rows, right_width), VECTOR)?;
        let right = unpack(right, rows, right_width, what)?;
        let (values, positions) = alp_exceptions(&mut vector, usize::from(exceptions), 2, rows, CODEC)?;

        let mut decoded: Vec<u64> = left
            .iter()
            .zip(&right)
            .map(|(&left, &right)| u64::from(dictionary[left as usize]) << right_width | right as u64)
            .collect();
        for (value, position) in values.chunks_exact(2).zip(positions) {
            let value = u64::from(u16::from_le_bytes(value.try_into().expect("2 bytes")));
            decoded[position] = value << right_width | right[position] as u64;
        }
        for value in decoded {
            out.extend_from_slice(&value.to_le_bytes()[..width]);
        }
    }
    Ok(())
}

/// `ROARING_CONTAINER_SIZE`, `COMPRESSED_SEGMENT_COUNT` and the rest of roaring.hpp.
const ROARING_CONTAINER_SIZE: usize = 2048;
const COMPRESSED_SEGMENT_COUNT: usize = 8;
const COMPRESSED_SEGMENT_SIZE: usize = 256;
const COMPRESSED_ARRAY_THRESHOLD: usize = 8;
const COMPRESSED_RUN_THRESHOLD: usize = 4;
const BITSET_CONTAINER_SENTINEL: usize = 249;

/// `ContainerSegmentScan`: positions kept as a byte, each 256 of them under a count of how many
/// are there.
struct ContainerSegments<'a> {
    counts: &'a [u8],
    index: usize,
    count: u8,
}

impl ContainerSegments<'_> {
    /// The base of the next position.
    fn next_base(&mut self) -> usize {
        while self.index < COMPRESSED_SEGMENT_COUNT && self.count >= self.counts[self.index] {
            self.count = 0;
            self.index += 1;
        }
        self.count = self.count.wrapping_add(1);
        self.index * COMPRESSED_SEGMENT_SIZE
    }
}

/// The positions of an array or run container, which are absolute or kept by 256 at a time.
fn compressed_positions(data: &[u8], stored: usize, compressed: bool, wide: usize) -> Result<Vec<usize>> {
    let what = "a Roaring container";
    if compressed {
        let counts = bytes(data, 0, COMPRESSED_SEGMENT_COUNT, what)?;
        let low = bytes(data, COMPRESSED_SEGMENT_COUNT, stored, what)?;
        let mut segments = ContainerSegments {
            counts,
            index: 0,
            count: 0,
        };
        Ok(low.iter().map(|&low| segments.next_base() + usize::from(low)).collect())
    } else {
        Ok(bytes(data, 0, stored * wide, what)?
            .chunks_exact(2)
            .map(|b| usize::from(u16::from_le_bytes(b.try_into().expect("2 bytes"))))
            .collect())
    }
}

/// Aligns `position`, an offset from the start of the data, to `to` bytes.
fn align(position: usize, to: usize) -> usize {
    position.div_ceil(to) * to
}

/// `RoaringScanState`: containers of 2048 bits, each a list of NULL runs, of positions that are
/// (or aren't) NULL, or a bitset, whichever is the smallest. A set bit is a value.
pub fn scan_roaring(segment: &[u8], count: usize, out: &mut Vec<bool>) -> Result<()> {
    const IS_RUN: u8 = 2;
    const IS_INVERTED: u8 = 1;
    const METADATA: &str = "Roaring metadata";
    const DATA: &str = "Roaring container data";
    let invalid_run = || corrupted("Corrupted Roaring segment: invalid run bounds or ordering");
    let what = "a Roaring segment";
    let metadata_offset = u64_at(segment, 0, what)? as usize;
    let body = segment.get(8..).ok_or_else(|| out_of_bounds(what))?;
    let mut metadata = body
        .get(metadata_offset..)
        .ok_or_else(|| corrupted("Corrupted Roaring segment body: offset is outside the segment"))?;

    let containers = count.div_ceil(ROARING_CONTAINER_SIZE);
    let types = advance(&mut metadata, packed_size(containers, 2), METADATA)?;
    let types = unpack(types, containers, 2, what)?;
    let runs = types.iter().filter(|&&t| t as u8 & IS_RUN != 0).count();
    let run_counts = advance(&mut metadata, packed_size(runs, 7), METADATA)?;
    let mut run_counts = unpack(run_counts, runs, 7, what)?.into_iter();
    let mut cardinalities = advance(&mut metadata, containers - runs, METADATA)?.iter();

    let mut position = 0;
    for (index, &container_type) in types.iter().enumerate() {
        let container_type = container_type as u8;
        let size = ROARING_CONTAINER_SIZE.min(count - index * ROARING_CONTAINER_SIZE);
        let start = out.len();
        if container_type & IS_RUN != 0 {
            let number = run_counts.next().expect("one count per run container") as usize;
            let compressed = number >= COMPRESSED_RUN_THRESHOLD;
            if !compressed {
                position = align(position, 4);
            }
            let data = body.get(position..).ok_or_else(|| read_past(DATA))?;
            let values = compressed_positions(data, number * 2, compressed, 2)?;
            position += if compressed {
                COMPRESSED_SEGMENT_COUNT + number * 2
            } else {
                number * 4
            };
            out.resize(start + size, true);
            let mut previous_end = 0;
            for pair in values.chunks_exact(2) {
                // compressed runs store their end, the others their length less one
                let end = if compressed { pair[1] } else { pair[0] + 1 + pair[1] };
                if pair[0] < previous_end || end <= pair[0] {
                    return Err(invalid_run());
                }
                previous_end = end;
                // finalizing can make the last run end one past the container
                let end = end.min(size);
                if pair[0] >= size {
                    return Err(invalid_run());
                }
                out[start + pair[0]..start + end].fill(false);
            }
            continue;
        }
        let cardinality = usize::from(*cardinalities.next().expect("one count per other container"));
        if cardinality == BITSET_CONTAINER_SENTINEL {
            position = align(position, 8);
            let stored = size.div_ceil(64) * 8;
            let data = body.get(position..).filter(|data| data.len() >= stored);
            scan_validity(data.ok_or_else(|| read_past(DATA))?, size, out)?;
            position += stored;
            continue;
        }
        let compressed = cardinality >= COMPRESSED_ARRAY_THRESHOLD;
        if !compressed {
            position = align(position, 2);
        }
        let data = body.get(position..).ok_or_else(|| read_past(DATA))?;
        let values = compressed_positions(data, cardinality, compressed, 2)?;
        position += if compressed {
            COMPRESSED_SEGMENT_COUNT + cardinality
        } else {
            cardinality * 2
        };
        // inverted arrays list the NULLs, and the others the values
        let listed_valid = container_type & IS_INVERTED == 0;
        out.resize(start + size, !listed_valid);
        for &value in &values {
            *out.get_mut(start + value)
                .filter(|_| value < size)
                .ok_or_else(|| corrupted("Corrupted Roaring segment: invalid array position or ordering"))? =
                listed_valid;
        }
    }
    Ok(())
}

/// `dict_fsst::CompressedStringScanState`: a dictionary, whose strings can be compressed with FSST
/// as a whole, and per row an index of it. Index 0 is NULL, and the rows that have it go to `nulls`:
/// such a segment takes care of NULLs by itself, and its validity is an empty segment that says
/// nothing.
pub fn scan_dict_fsst(segment: &[u8], count: usize, out: &mut Vec<Vec<u8>>, nulls: &mut Vec<usize>) -> Result<()> {
    const HEADER_SIZE: usize = 16;
    const DICTIONARY: u8 = 0;
    const DICT_FSST: u8 = 1;
    const FSST_ONLY: u8 = 2;
    let what = "a DICT_FSST segment";
    let dict_size = u32_at(segment, 0, what)? as usize;
    let dict_count = u32_at(segment, 4, what)? as usize;
    let header = bytes(segment, 8, 3, what)?;
    let (mode, lengths_width, indices_width) = (header[0], usize::from(header[1]), usize::from(header[2]));
    let table_size = u32_at(segment, 12, what)? as usize;
    if mode > FSST_ONLY {
        return Err(corrupt(format!("unrecognized DICT_FSST compression mode {mode}")));
    }
    if dict_count == 0 || (mode == FSST_ONLY && count > dict_count - 1) {
        return Err(corrupted(
            "Corrupted DICT_FSST segment: dictionary index is out of range",
        ));
    }
    let invalid_width = || {
        corrupted(
            "Failed to scan dictionary string - bitpacking width was invalid. Database file appears to be corrupted.",
        )
    };
    if indices_width > 32 || lengths_width > 32 || (lengths_width == 0 && mode != DICTIONARY) {
        return Err(invalid_width());
    }
    if lengths_width == 0 && dict_count > 2 {
        return Err(corrupted(
            "Corrupted DICT_FSST segment: zero-width lengths allow at most two dictionary entries",
        ));
    }
    let mut position = HEADER_SIZE;
    let dictionary = bytes(segment, position, dict_size, what)?;
    position = align(position + dict_size, 8);
    let table = bytes(segment, position, table_size, what)?;
    position = align(position + table_size, 8);
    let packed = bytes(segment, position, packed_size(dict_count, lengths_width), what)?;
    let lengths = unpack(packed, dict_count, lengths_width, what)?;
    position = align(position + packed.len(), 8);
    let packed = bytes(segment, position, packed_size(count, indices_width), what)?;
    let indices = if mode == FSST_ONLY {
        Vec::new()
    } else {
        unpack(packed, count, indices_width, what)?
    };

    if lengths[0] != 0 {
        return Err(corrupted(
            "Corrupted DICT_FSST segment: string lengths exceed the dictionary size",
        ));
    }
    let decoder = if mode == DICT_FSST || mode == FSST_ONLY {
        Some(fsst_import(table)?.ok_or_else(|| corrupted("Failed to scan DICT_FSST string segment: invalid FSST symbol table. Database file appears to be corrupted."))?)
    } else {
        None
    };
    let mut strings = Vec::with_capacity(dict_count.min(1 << 16));
    let mut offset = 0usize;
    for &length in &lengths {
        let length = length as usize;
        let entry = offset
            .checked_add(length)
            .and_then(|end| dictionary.get(offset..end))
            .ok_or_else(|| corrupted("Corrupted DICT_FSST segment: string lengths exceed the dictionary size"))?;
        offset += length;
        strings.push(match &decoder {
            Some(decoder) => decoder.decompress(entry)?,
            None => entry.to_vec(),
        });
    }
    if mode == FSST_ONLY {
        out.extend_from_slice(&strings[1..=count]);
        return Ok(());
    }
    for index in indices {
        if index == 0 {
            nulls.push(out.len());
        }
        let string = strings.get(index as usize);
        out.push(
            string
                .ok_or_else(|| corrupted("Corrupted DICT_FSST segment: dictionary index is out of range"))?
                .clone(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fsst_segment(dict_end: u32, width: u32, table_offset: u32) -> Vec<u8> {
        let mut segment = vec![0; 256];
        for (i, value) in [0, dict_end, width, table_offset].iter().enumerate() {
            segment[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        segment
    }

    #[test]
    fn fsst_layouts_must_add_up_exactly() {
        let error = |segment: &[u8]| scan_fsst(segment, 32, &mut Vec::new()).unwrap_err().to_string();
        // 32 lengths of 8 bits end at 48: a table anywhere else means the width is wrong
        for table_offset in [47, 49, 60] {
            assert_eq!(
                error(&fsst_segment(200, 8, table_offset)),
                "Data Corruption Error: Failed to read FSST string segment - bitpacking width did not match the \
                 stored layout. Database file appears to be corrupted."
            );
        }
        assert!(error(&fsst_segment(200, 33, 48)).contains("bitpacking width was invalid"));
        assert!(error(&fsst_segment(47, 8, 48)).contains("dictionary was out of range"));
    }

    fn dictionary_segment(dict_size: u32, dict_end: u32, index_offset: u32, index_count: u32, width: u32) -> Vec<u8> {
        let mut segment = vec![0; 256];
        for (i, value) in [dict_size, dict_end, index_offset, index_count, width]
            .iter()
            .enumerate()
        {
            segment[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        segment
    }

    #[test]
    fn dictionary_layouts_must_add_up_exactly() {
        let scan = |segment: Vec<u8>| scan_dictionary(&segment, 32, &mut Vec::new());
        // two entries take one bit, and 32 selections of one bit take 4 bytes
        assert!(scan(dictionary_segment(0, 100, 24, 2, 1)).is_ok());
        assert!(scan(dictionary_segment(0, 100, 24, 2, 2)).is_err(), "too wide");
        assert!(
            scan(dictionary_segment(0, 100, 28, 2, 1)).is_err(),
            "selections too long"
        );
        assert!(
            scan(dictionary_segment(0, 28, 24, 2, 1)).is_err(),
            "table past the dictionary"
        );
        assert!(
            scan(dictionary_segment(90, 100, 24, 2, 1)).is_err(),
            "dictionary over the table"
        );
    }

    #[test]
    fn a_stream_of_bits_crosses_bytes() {
        assert_eq!(read_bits(&[0b1010_0000, 0b0000_0101], 5, 6), 0b101101);
        assert_eq!(read_bits(&[0xFF; 17], 3, 128), u128::MAX);
    }

    #[test]
    fn fsst_decodes_symbols_and_escapes() {
        let mut table = vec![0; 17];
        table[..8].copy_from_slice(&(20190218u64 << 32).to_le_bytes());
        // lengths 1, 2 and 1 again: the histogram has lengths 2..8 first and 1 last
        table[9] = 1;
        table[10] = 1;
        table.extend_from_slice(b"abc");
        let decoder = fsst_import(&table).unwrap().unwrap();
        assert_eq!(decoder.size, 20);
        assert_eq!(decoder.decompress(&[0, 1, 255, b'z']).unwrap(), b"abcz");
        assert!(decoder.decompress(&[255]).is_err());
    }

    #[test]
    fn rle_runs_must_cover_the_rows_exactly() {
        let mut segment = 12u64.to_le_bytes().to_vec();
        segment.extend_from_slice(&5i32.to_le_bytes());
        segment.extend_from_slice(&3u16.to_le_bytes());
        let mut out = Vec::new();
        scan_rle(&segment, 3, 4, &mut out).unwrap();
        assert_eq!(out, [5, 0, 0, 0].repeat(3));
        assert!(
            scan_rle(&segment, 2, 4, &mut Vec::new()).is_err(),
            "a run past the rows"
        );
        assert!(scan_rle(&segment, 4, 4, &mut Vec::new()).is_err(), "rows past the runs");
    }
}
