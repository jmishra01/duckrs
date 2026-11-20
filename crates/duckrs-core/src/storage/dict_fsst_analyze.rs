// Ported from duckdb/src/storage/compression/dict_fsst/analyze.cpp @ rust-port-baseline
//
//! `DictFSSTAnalyzeState`: what DICT_FSST estimates a column of strings to take. The dictionary of a
//! segment is local to its block, so the estimate simulates how the values fill blocks: each block
//! stores its distinct values once, and an index per row. A block that fills up with its dictionary
//! as it is may continue with the dictionary FSST encoded, when the ratio measured on a sample of the
//! values says that pays off.
//!
//! The ratio is measured with the FSST of `fsst.rs`, which trains as C++ does but is not byte for byte
//! the same; a dictionary that FSST encodes can be estimated a little differently from C++.

use super::fsst::SymbolTable;
use hashbrown::HashSet;

/// `DictFSSTCompression::STRING_SIZE_LIMIT`.
const STRING_SIZE_LIMIT: usize = 16384;
/// `DictFSSTCompression::DICTIONARY_ENCODE_THRESHOLD`.
const DICTIONARY_ENCODE_THRESHOLD: usize = 4096;
/// `sizeof(duckdb_fsst_decoder_t)`.
const FSST_SYMBOL_TABLE_SIZE: usize = 2304;
/// `sizeof(dict_fsst_compression_header_t)`.
const HEADER_SIZE: usize = 16;
const FSST_RATIO_SCALE: usize = 1024;
const FSST_OUTPUT_SLACK: usize = 7;
const FSST_MEASURE_SIZE: usize = 16384;
const FSST_MEASURE_INTERVAL: usize = 4;

/// `DictionaryAppendState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Regular,
    Encoded,
    NotEncoded,
    EncodedAllUnique,
}

impl Mode {
    fn is_encoded(self) -> bool {
        matches!(self, Mode::Encoded | Mode::EncodedAllUnique)
    }
}

fn align(value: usize) -> usize {
    value.next_multiple_of(8)
}

/// `BitpackingPrimitives::MinimumBitWidth` of an unsigned 64-bit value.
fn bit_width(value: usize) -> usize {
    let width = (usize::BITS - value.leading_zeros()) as usize;
    // `GetEffectiveWidth`
    if width + 8 > 64 { 64 } else { width }
}

/// `BitpackingPrimitives::GetRequiredSize`.
fn packed_size(count: usize, width: usize) -> usize {
    count.next_multiple_of(32) * width / 8
}

/// `GetStringSizeLimit`: the longest string a block can hold on its own.
fn string_size_limit(available: usize, fsst_encoded: bool) -> usize {
    let mut max_length = STRING_SIZE_LIMIT - 1;
    if fsst_encoded {
        max_length *= 2;
    }
    let lengths_space = packed_size(2, bit_width(max_length));
    let indices_space = packed_size(1, bit_width(1));
    let mut metadata = align(HEADER_SIZE);
    if fsst_encoded {
        metadata += 7;
    }
    metadata += 8 - 1;
    if fsst_encoded {
        metadata += align(FSST_SYMBOL_TABLE_SIZE);
    }
    metadata += align(lengths_space) + indices_space;
    let mut max_size = available.saturating_sub(metadata);
    if fsst_encoded {
        max_size /= 2;
    }
    STRING_SIZE_LIMIT.min(max_size + 1)
}

/// `DictFSSTAnalyzeState::RequiredSpace`.
fn required_space(
    mode: Mode,
    tuple_count: usize,
    unique_count: usize,
    dict_size: usize,
    max_length: usize,
    symbol_table_size: usize,
) -> usize {
    let dict_count = unique_count + 1;
    let lengths_space = packed_size(dict_count, bit_width(max_length));
    let mut space = align(HEADER_SIZE) + dict_size;
    space = align(space);
    if mode.is_encoded() {
        space = align(space + symbol_table_size);
    }
    space = align(space + lengths_space);
    if mode != Mode::EncodedAllUnique {
        space += packed_size(tuple_count, bit_width(dict_count - 1));
    }
    space
}

struct State {
    block_size: usize,
    string_size_limit: usize,
    fsst_string_size_limit: usize,
    disable_fsst: bool,
    contains_nulls: bool,
    total_count: usize,
    simulated_space: usize,
    mode: Mode,
    tuple_count: usize,
    unique_count: usize,
    dict_size: usize,
    max_length: usize,
    symbol_table_size: usize,
    indices_width: usize,
    required: usize,
    max_set_size: usize,
    set: HashSet<Vec<u8>>,
    sample: Vec<Vec<u8>>,
    sample_size: usize,
    fsst_ratio: usize,
    fsst_symbol_table_size: usize,
    encode_decisions: usize,
    next_measurement: usize,
}

impl State {
    fn new(block_size: usize) -> Self {
        let mut state = Self {
            block_size,
            string_size_limit: string_size_limit(block_size, false),
            fsst_string_size_limit: string_size_limit(block_size, true),
            disable_fsst: false,
            contains_nulls: false,
            total_count: 0,
            simulated_space: 0,
            mode: Mode::Regular,
            tuple_count: 0,
            unique_count: 0,
            dict_size: 0,
            max_length: 0,
            symbol_table_size: 0,
            indices_width: 0,
            required: 0,
            max_set_size: block_size / 2,
            set: HashSet::new(),
            sample: Vec::new(),
            sample_size: 0,
            fsst_ratio: FSST_RATIO_SCALE,
            fsst_symbol_table_size: 0,
            encode_decisions: 0,
            next_measurement: 1,
        };
        state.required = state.block_required(0, 0, 0, 0);
        state
    }

    fn block_required(&self, tuples: usize, uniques: usize, dict_size: usize, max_length: usize) -> usize {
        required_space(
            self.mode,
            tuples,
            uniques,
            dict_size,
            max_length,
            self.symbol_table_size,
        )
    }

    fn available(&self) -> usize {
        if self.mode == Mode::Regular {
            self.block_size - FSST_SYMBOL_TABLE_SIZE
        } else {
            self.block_size
        }
    }

    fn encoded_length(&self, length: usize) -> usize {
        if length == 0 {
            return 0;
        }
        (length * self.fsst_ratio).div_ceil(FSST_RATIO_SCALE).max(1)
    }

    fn stored_length(&self, length: usize) -> usize {
        if self.mode.is_encoded() {
            self.encoded_length(length)
        } else {
            length
        }
    }

    /// `SegmentCharge`: a segment above the compaction limit takes a block of its own.
    fn segment_charge(&self, required: usize) -> usize {
        if required > self.block_size / 5 * 4 {
            self.block_size
        } else {
            align(required)
        }
    }

    fn measure_fsst(&mut self) {
        let dictionary_size: usize = self.sample.iter().map(Vec::len).sum();
        if dictionary_size == 0 {
            return;
        }
        let strings: Vec<&[u8]> = self.sample.iter().map(Vec::as_slice).collect();
        let (encoded_size, table_size) = match SymbolTable::train(&strings) {
            Some(table) => {
                let mut encoded = 0;
                let mut out = Vec::new();
                for string in &strings {
                    out.clear();
                    table.compress(string, &mut out);
                    encoded += out.len();
                }
                (encoded, table.export().len())
            }
            None => (dictionary_size, 0),
        };
        self.fsst_ratio = (encoded_size * FSST_RATIO_SCALE)
            .div_ceil(dictionary_size)
            .min(2 * FSST_RATIO_SCALE);
        self.fsst_symbol_table_size = table_size;
    }

    fn choose_encoding(&mut self) -> Mode {
        if self.disable_fsst || self.dict_size < DICTIONARY_ENCODE_THRESHOLD {
            return Mode::NotEncoded;
        }
        self.encode_decisions += 1;
        if self.encode_decisions == self.next_measurement {
            self.measure_fsst();
            self.next_measurement *= FSST_MEASURE_INTERVAL;
        }
        let encoded_dict_size = self.encoded_length(self.dict_size);
        if encoded_dict_size + DICTIONARY_ENCODE_THRESHOLD > self.dict_size {
            return Mode::NotEncoded;
        }
        let before_longest = self.dict_size - self.encoded_length(self.dict_size - self.max_length);
        if before_longest < 2 * self.max_length + FSST_OUTPUT_SLACK {
            return Mode::NotEncoded;
        }
        let all_unique = !self.contains_nulls && self.unique_count == self.tuple_count;
        let mode = if all_unique {
            Mode::EncodedAllUnique
        } else {
            Mode::Encoded
        };
        let required = required_space(
            mode,
            self.tuple_count,
            self.unique_count,
            encoded_dict_size,
            self.encoded_length(self.max_length),
            self.fsst_symbol_table_size,
        );
        if required > self.block_size {
            return Mode::NotEncoded;
        }
        mode
    }

    fn switch_from_regular(&mut self) {
        self.mode = self.choose_encoding();
        if !self.mode.is_encoded() {
            return;
        }
        self.symbol_table_size = self.fsst_symbol_table_size;
        self.dict_size = self.encoded_length(self.dict_size);
        self.max_length = self.encoded_length(self.max_length);
        self.required = self.block_required(self.tuple_count, self.unique_count, self.dict_size, self.max_length);
    }

    fn try_add_index_only(&mut self) -> bool {
        loop {
            let mut growth = 0;
            if self.mode != Mode::EncodedAllUnique && self.tuple_count.is_multiple_of(32) {
                growth = packed_size(32, self.indices_width);
            }
            if self.required + growth <= self.available() {
                self.tuple_count += 1;
                self.required += growth;
                return true;
            }
            if self.mode != Mode::Regular {
                return false;
            }
            self.switch_from_regular();
            if self.mode == Mode::EncodedAllUnique {
                return false;
            }
        }
    }

    fn add_new_string(&mut self, string: &[u8]) {
        let length = string.len();
        let (stored, required) = loop {
            let stored = self.stored_length(length);
            let required = self.block_required(
                self.tuple_count + 1,
                self.unique_count + 1,
                self.dict_size + stored,
                self.max_length.max(stored),
            );
            let mut admission = required;
            if self.mode == Mode::Encoded {
                admission = self.block_required(
                    self.tuple_count + 1,
                    self.unique_count + 1,
                    self.dict_size + 2 * length + FSST_OUTPUT_SLACK,
                    self.max_length.max(2 * length),
                );
            }
            let set_is_full = self.mode != Mode::EncodedAllUnique && self.unique_count >= self.max_set_size;
            if admission <= self.available() && !set_is_full {
                break (stored, required);
            }
            if self.mode == Mode::Regular {
                self.switch_from_regular();
                continue;
            }
            self.flush_block();
        };
        self.tuple_count += 1;
        self.unique_count += 1;
        self.dict_size += stored;
        self.max_length = self.max_length.max(stored);
        self.indices_width = bit_width(self.unique_count);
        self.required = required;
        if self.mode == Mode::EncodedAllUnique {
            return;
        }
        self.set.insert(string.to_vec());
        if self.mode == Mode::Regular && self.sample_size < FSST_MEASURE_SIZE {
            self.sample.push(string.to_vec());
            self.sample_size += length;
        }
    }

    fn flush_block(&mut self) {
        self.simulated_space += self.segment_charge(self.required);
        self.mode = if self.disable_fsst {
            Mode::NotEncoded
        } else {
            Mode::Regular
        };
        self.tuple_count = 0;
        self.unique_count = 0;
        self.dict_size = 0;
        self.max_length = 0;
        self.symbol_table_size = 0;
        self.indices_width = 0;
        self.required = self.block_required(0, 0, 0, 0);
        self.set.clear();
        self.sample.clear();
        self.sample_size = 0;
    }

    /// `Analyze`; `false` where a string is too long for DICT_FSST.
    fn analyze(&mut self, strings: &[&[u8]], valid: &[bool]) -> bool {
        for (string, valid) in strings.iter().zip(valid) {
            if !*valid {
                self.contains_nulls = true;
                if !self.try_add_index_only() {
                    self.flush_block();
                    self.try_add_index_only();
                }
                continue;
            }
            let length = string.len();
            if length >= self.string_size_limit {
                return false;
            }
            if length >= self.fsst_string_size_limit {
                self.disable_fsst = true;
            }
            if self.mode == Mode::EncodedAllUnique {
                self.add_new_string(string);
                continue;
            }
            if self.set.contains(*string) {
                if self.try_add_index_only() {
                    continue;
                }
                if self.mode != Mode::EncodedAllUnique {
                    self.flush_block();
                }
            }
            self.add_new_string(string);
        }
        self.total_count += strings.len();
        true
    }

    fn final_analyze(&self) -> usize {
        if self.total_count == 0 {
            return 0;
        }
        let mut estimate = self.simulated_space;
        if self.tuple_count > 0 {
            estimate += self.segment_charge(self.required);
        }
        estimate
    }
}

/// The length from which a string leaves FSST out of the column's DICT_FSST (`disable_fsst`).
pub fn fsst_string_limit(block_size: usize) -> usize {
    string_size_limit(block_size, true)
}

/// `DictFSSTCompressionStorage::StringFinalAnalyze` of the strings of a column (`strings[i]` is empty
/// where `valid[i]` is not set); `None` where DICT_FSST can't store them. `block_size` is what a block
/// holds.
pub fn final_analyze(strings: &[&[u8]], valid: &[bool], block_size: usize) -> Option<usize> {
    let mut state = State::new(block_size);
    state.analyze(strings, valid).then(|| state.final_analyze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn few_distinct_values_fit_one_block() {
        let strings: Vec<Vec<u8>> = (0..10_000).map(|i| format!("{}", i % 500).into_bytes()).collect();
        let refs: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
        let valid = vec![true; refs.len()];
        // 500 values of up to 3 bytes, 10000 indices of 9 bits: one small segment
        let score = final_analyze(&refs, &valid, 262_136).unwrap();
        let dictionary: usize = (0..500).map(|i: i32| i.to_string().len()).sum();
        let expected = align(align(align(16 + dictionary) + packed_size(501, 2)) + packed_size(10_000, 9));
        assert_eq!(score, expected);
    }

    #[test]
    fn too_long_strings_are_refused() {
        let long = vec![b'x'; STRING_SIZE_LIMIT];
        assert_eq!(final_analyze(&[&long], &[true], 262_136), None);
    }
}
