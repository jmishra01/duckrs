// Ported from duckdb/src/storage/compression/dict_fsst/compression.cpp @ rust-port-baseline
//
//! `DictFSSTCompressionState`: how DICT_FSST fills its segments. A segment starts as a plain dictionary; when
//! that fills the block, the dictionary is FSST encoded if that saves enough (`TryEncode`), and the segment goes
//! on with the strings encoded, without an index per row where no string repeats and nothing is NULL
//! (`ENCODED_ALL_UNIQUE`). Where that is when a segment ends depends on how well FSST encodes, and the FSST of
//! `fsst.rs` trains as C++ does but not byte for byte the same.

use super::encode::{Encoded, pack};
use super::fsst::SymbolTable;
use std::collections::HashMap;

/// `sizeof(dict_fsst_compression_header_t)`.
const HEADER_SIZE: usize = 16;
/// `DictFSSTCompression::FSST_SYMBOL_TABLE_SIZE`: `sizeof(duckdb_fsst_decoder_t)`.
const FSST_SYMBOL_TABLE_SIZE: usize = 2304;
/// `DictFSSTCompression::DICTIONARY_ENCODE_THRESHOLD`.
const DICTIONARY_ENCODE_THRESHOLD: usize = 4096;
/// `BITPACKING_ALGORITHM_GROUP_SIZE`.
const GROUP: usize = 32;
/// what fsst.h adds to its output buffer
const FSST_SLACK: usize = 7;

/// `DictionaryAppendState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Regular,
    NotEncoded,
    Encoded,
    EncodedAllUnique,
}

impl Mode {
    fn is_encoded(self) -> bool {
        matches!(self, Mode::Encoded | Mode::EncodedAllUnique)
    }

    /// `ConvertToMode`: `DictFSSTMode`.
    fn stored(self) -> u8 {
        match self {
            Mode::Regular | Mode::NotEncoded => 0,
            Mode::Encoded => 1,
            Mode::EncodedAllUnique => 2,
        }
    }
}

fn align(value: usize) -> usize {
    value.next_multiple_of(8)
}

/// `BitpackingPrimitives::MinimumBitWidth`.
fn bit_width(value: usize) -> usize {
    (usize::BITS - value.leading_zeros()) as usize
}

/// `BitpackingPrimitives::GetRequiredSize`.
fn packed_size(count: usize, width: usize) -> usize {
    count.next_multiple_of(GROUP) * width / 8
}

/// `RequiresHigherBitWidth`.
fn requires_higher(width: usize, value: usize) -> bool {
    value >= 1 << width
}

/// One segment as it fills.
struct Segment<'a> {
    mode: Mode,
    /// the dictionary as the segment has it: encoded once the mode is
    dictionary: Vec<u8>,
    /// per entry of the dictionary, its length there; 0 is NULL
    lengths: Vec<u32>,
    indices: Vec<u32>,
    tuple_count: usize,
    lengths_width: usize,
    real_lengths_width: usize,
    indices_width: usize,
    lengths_space: usize,
    indices_space: usize,
    table: Option<SymbolTable>,
    symbol_table: Vec<u8>,
    /// `ENCODED`: the strings not encoded yet, and what they may take (`to_encode_string_sum`)
    to_encode: Vec<&'a [u8]>,
    to_encode_sum: usize,
    /// `current_string_map`: the strings as they came, by their entry less one; it takes no more once full
    map: HashMap<Vec<u8>, u32>,
    map_full: bool,
}

struct Compressor<'a> {
    block_size: usize,
    disable_fsst: bool,
    contains_nulls: bool,
    map_size: usize,
    segment: Segment<'a>,
    done: Vec<Encoded>,
}

impl<'a> Segment<'a> {
    /// `CreateEmptySegment`.
    fn new(disable_fsst: bool) -> Self {
        Self {
            mode: if disable_fsst { Mode::NotEncoded } else { Mode::Regular },
            dictionary: Vec::new(),
            lengths: vec![0],
            indices: Vec::new(),
            tuple_count: 0,
            lengths_width: 0,
            real_lengths_width: 0,
            indices_width: 0,
            lengths_space: 0,
            indices_space: 0,
            table: None,
            symbol_table: Vec::new(),
            to_encode: Vec::new(),
            to_encode_sum: 0,
            map: HashMap::new(),
            map_full: false,
        }
    }

    fn dict_count(&self) -> usize {
        self.lengths.len() + self.to_encode.len()
    }

    /// `PrimitiveDictionary::Insert`: an entry is the index of its insertion.
    fn remember(&mut self, string: &[u8], map_size: usize) {
        if self.map_full || self.map.contains_key(string) {
            return;
        }
        if self.map.len() + 1 > map_size {
            self.map_full = true;
            return;
        }
        let index = self.map.len() as u32;
        self.map.insert(string.to_vec(), index);
    }

    fn available(&self, block_size: usize) -> usize {
        if self.mode == Mode::Regular {
            block_size - FSST_SYMBOL_TABLE_SIZE
        } else {
            block_size
        }
    }

    fn metadata(&self, dictionary: usize, lengths_space: usize, indices_space: usize) -> usize {
        let mut space = align(align(HEADER_SIZE) + dictionary);
        if self.mode.is_encoded() {
            space = align(space + self.symbol_table.len());
        }
        align(space + lengths_space) + indices_space
    }

    /// `AddLookup`.
    fn add_lookup(&mut self, lookup: u32, recalculate: bool, block_size: usize) -> bool {
        let mut indices_space = self.indices_space;
        if self.mode != Mode::EncodedAllUnique && recalculate {
            indices_space = packed_size(self.tuple_count + 1, self.indices_width);
        }
        let mut dictionary = self.dictionary.len();
        if self.mode == Mode::Encoded {
            dictionary += self.to_encode_sum;
        }
        if self.metadata(dictionary, self.lengths_space, indices_space) > self.available(block_size) {
            return false;
        }
        if recalculate {
            self.indices_space = indices_space;
        }
        self.indices.push(lookup);
        true
    }

    /// `AddToDictionary`: `string` as the segment stores it (encoded in `ENCODED_ALL_UNIQUE`), `original` as it
    /// came.
    fn add_to_dictionary(
        &mut self,
        string: &[u8],
        original: &'a [u8],
        recalculate_indices: bool,
        block_size: usize,
        map_size: usize,
    ) -> bool {
        let mut length = string.len();
        if self.mode == Mode::Encoded {
            // encoding can double a string at most
            length *= 2;
        }
        let dict_count = self.dict_count();
        let higher_lengths = requires_higher(self.lengths_width, length);
        let higher_indices = requires_higher(self.indices_width, dict_count);
        let recalculate_lengths = dict_count.is_multiple_of(GROUP);
        let mut lengths_width = self.lengths_width;
        let mut lengths_space = self.lengths_space;
        if higher_lengths {
            lengths_width = bit_width(length);
        }
        if higher_lengths || recalculate_lengths {
            lengths_space = packed_size(dict_count + 1, lengths_width);
        }
        let mut indices_width = self.indices_width;
        let mut indices_space = self.indices_space;
        if self.mode != Mode::EncodedAllUnique {
            if higher_indices {
                indices_width = bit_width(dict_count);
            }
            if higher_indices || recalculate_indices {
                indices_space = packed_size(self.tuple_count + 1, indices_width);
            }
        }
        let mut dictionary = self.dictionary.len() + length;
        if self.mode == Mode::Encoded {
            dictionary += if self.to_encode_sum == 0 { FSST_SLACK } else { 0 } + self.to_encode_sum;
        }
        if self.metadata(dictionary, lengths_space, indices_space) > self.available(block_size) {
            return false;
        }
        self.indices.push(dict_count as u32);
        if self.mode == Mode::Encoded {
            self.to_encode.push(original);
            if self.to_encode_sum == 0 {
                self.to_encode_sum = FSST_SLACK;
            }
            self.to_encode_sum += length;
        } else {
            self.lengths.push(length as u32);
            self.dictionary.extend_from_slice(string);
        }
        // all unique strings aren't looked up
        if self.mode != Mode::EncodedAllUnique {
            self.remember(original, map_size);
        }
        if higher_lengths {
            self.lengths_width = lengths_width;
        }
        if higher_lengths || recalculate_lengths {
            self.lengths_space = lengths_space;
        }
        if higher_indices {
            self.indices_width = indices_width;
        }
        if higher_indices || recalculate_indices {
            self.indices_space = indices_space;
        }
        true
    }

    /// `FlushEncodingBuffer`.
    fn flush_encoding_buffer(&mut self) {
        if self.to_encode.is_empty() {
            return;
        }
        let table = self.table.as_ref().expect("encoded with a table");
        self.lengths_width = self.real_lengths_width;
        let mut biggest = (1usize << self.lengths_width) - 1;
        let mut out = Vec::new();
        for string in std::mem::take(&mut self.to_encode) {
            table.compress(string, &mut out);
            biggest = biggest.max(out.len());
            self.lengths.push(out.len() as u32);
            self.dictionary.extend_from_slice(&out);
        }
        if biggest >= 1 << self.lengths_width {
            self.lengths_width = bit_width(biggest);
        }
        self.real_lengths_width = self.lengths_width;
        self.lengths_space = packed_size(self.lengths.len(), self.lengths_width);
        self.to_encode_sum = 0;
    }

    /// `TryEncode`: what the segment goes on as once its plain dictionary fills the block.
    fn try_encode(&mut self, contains_nulls: bool, block_size: usize, map_size: usize) -> Mode {
        if self.dictionary.len() < DICTIONARY_ENCODE_THRESHOLD {
            return Mode::NotEncoded;
        }
        let all_unique = self.lengths.len() - 1 == self.tuple_count;
        let mode = if !contains_nulls && all_unique {
            Mode::EncodedAllUnique
        } else {
            Mode::Encoded
        };
        let mut strings = Vec::with_capacity(self.lengths.len() - 1);
        let mut at = 0;
        for length in &self.lengths[1..] {
            let length = *length as usize;
            strings.push(&self.dictionary[at..at + length]);
            at += length;
        }
        let Some(table) = SymbolTable::train(&strings) else {
            return Mode::NotEncoded;
        };
        // `duckdb_fsst_compress` into the dictionary's own space, which stops at a string that may not fit
        let mut encoded = Vec::with_capacity(self.dictionary.len());
        let mut lengths = Vec::with_capacity(strings.len());
        let mut out = Vec::new();
        for string in &strings {
            if 2 * string.len() + FSST_SLACK > self.dictionary.len() - encoded.len() {
                return Mode::NotEncoded;
            }
            table.compress(string, &mut out);
            lengths.push(out.len() as u32);
            encoded.extend_from_slice(&out);
        }
        if encoded.len() + DICTIONARY_ENCODE_THRESHOLD > self.dictionary.len() {
            return Mode::NotEncoded;
        }
        let symbol_table = table.export();
        let longest = lengths.iter().copied().max().unwrap_or(0) as usize;
        let lengths_width = bit_width(longest);
        let lengths_space = packed_size(self.lengths.len(), lengths_width);
        let required = align(align(align(align(HEADER_SIZE) + encoded.len()) + symbol_table.len()) + lengths_space)
            + self.indices_space;
        if required > block_size {
            return Mode::NotEncoded;
        }
        if mode == Mode::EncodedAllUnique {
            self.indices_width = 0;
            self.indices_space = 0;
        }
        // the map keeps the strings as they came, but for all unique ones, which aren't looked up
        let originals: Vec<Vec<u8>> = strings.iter().map(|s| s.to_vec()).collect();
        self.map.clear();
        self.map_full = false;
        if mode == Mode::Encoded {
            for string in &originals {
                self.remember(string, map_size);
            }
        }
        self.lengths[1..].copy_from_slice(&lengths);
        self.dictionary = encoded;
        self.lengths_width = lengths_width;
        self.lengths_space = lengths_space;
        self.real_lengths_width = lengths_width;
        self.symbol_table = symbol_table;
        self.table = Some(table);
        mode
    }

    /// `Finalize`: the bytes of the segment.
    fn finalize(mut self) -> Encoded {
        self.flush_encoding_buffer();
        let symbols: &[u8] = if self.mode.is_encoded() {
            &self.symbol_table
        } else {
            &[]
        };
        let mut segment = Vec::new();
        segment.extend_from_slice(&(self.dictionary.len() as u32).to_le_bytes());
        segment.extend_from_slice(&(self.lengths.len() as u32).to_le_bytes());
        segment.extend_from_slice(&[
            self.mode.stored(),
            self.lengths_width as u8,
            self.indices_width as u8,
            0,
        ]);
        segment.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
        segment.extend_from_slice(&self.dictionary);
        segment.resize(align(segment.len()), 0);
        segment.extend_from_slice(symbols);
        segment.resize(align(segment.len()), 0);
        let lengths: Vec<u128> = self.lengths.iter().map(|l| u128::from(*l)).collect();
        segment.extend_from_slice(&pack(&lengths, self.lengths_width));
        if self.indices_width > 0 {
            segment.resize(align(segment.len()), 0);
            let indices: Vec<u128> = self.indices.iter().map(|i| u128::from(*i)).collect();
            segment.extend_from_slice(&pack(&indices, self.indices_width));
        }
        (segment, self.tuple_count)
    }
}

impl<'a> Compressor<'a> {
    /// `CompressInternal`: whether the row fits the segment.
    fn compress_row(&mut self, string: &'a [u8], is_null: bool) -> bool {
        let (block_size, map_size) = (self.block_size, self.map_size);
        let segment = &mut self.segment;
        let recalculate = segment.mode != Mode::EncodedAllUnique && segment.tuple_count.is_multiple_of(GROUP);
        let lookup = if segment.mode == Mode::EncodedAllUnique || is_null {
            Some(0)
        } else {
            segment.map.get(string).map(|index| index + 1)
        };
        match segment.mode {
            Mode::Regular | Mode::NotEncoded => match lookup {
                Some(lookup) => segment.add_lookup(lookup, recalculate, block_size),
                None => segment.add_to_dictionary(string, string, recalculate, block_size, map_size),
            },
            Mode::Encoded => {
                let add = |segment: &mut Segment<'a>| match lookup {
                    Some(lookup) => segment.add_lookup(lookup, recalculate, block_size),
                    None => segment.add_to_dictionary(string, string, recalculate, block_size, map_size),
                };
                if add(segment) {
                    return true;
                }
                if segment.to_encode.is_empty() {
                    return false;
                }
                segment.flush_encoding_buffer();
                add(segment)
            }
            Mode::EncodedAllUnique => {
                let mut encoded = Vec::new();
                segment.table.as_ref().expect("encoded").compress(string, &mut encoded);
                segment.add_to_dictionary(&encoded, string, recalculate, block_size, map_size)
            }
        }
    }

    /// `Compress` of a row.
    fn add(&mut self, string: &'a [u8], is_null: bool) {
        if !self.compress_row(string, is_null) {
            let mut fits = false;
            if self.segment.mode == Mode::Regular {
                self.segment.mode = self
                    .segment
                    .try_encode(self.contains_nulls, self.block_size, self.map_size);
                fits = self.compress_row(string, is_null);
            }
            if !fits {
                let full = std::mem::replace(&mut self.segment, Segment::new(self.disable_fsst));
                if full.tuple_count > 0 {
                    self.done.push(full.finalize());
                }
                assert!(self.compress_row(string, is_null), "a string fits an empty segment");
            }
        }
        self.segment.tuple_count += 1;
    }
}

/// The DICT_FSST segments of the strings of a column (`strings[i]` empty where `valid[i]` isn't set), where
/// `fsst_limit` is the length from which a string makes analysis leave FSST out (`disable_fsst`).
pub fn compress(strings: &[&[u8]], valid: &[bool], block_size: usize, fsst_limit: usize) -> Vec<Encoded> {
    let disable_fsst = strings.iter().zip(valid).any(|(s, v)| *v && s.len() >= fsst_limit);
    let mut compressor = Compressor {
        block_size,
        disable_fsst,
        contains_nulls: valid.iter().any(|valid| !*valid),
        map_size: strings.len().min(block_size) / 2,
        segment: Segment::new(disable_fsst),
        done: Vec::new(),
    };
    for (string, valid) in strings.iter().zip(valid) {
        compressor.add(string, !*valid);
    }
    let last = std::mem::replace(&mut compressor.segment, Segment::new(disable_fsst));
    if last.tuple_count > 0 {
        compressor.done.push(last.finalize());
    }
    compressor.done
}
