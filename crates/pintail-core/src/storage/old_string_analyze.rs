// Ported from duckdb/src/storage/compression/dictionary_compression.cpp, dictionary/analyze.cpp and fsst.cpp
// @ rust-port-baseline
//
//! The `final_analyze` scores of DICTIONARY and FSST, the string compressions of files before v1.3.0, which
//! `ColumnDataCheckpointer::DetectBestCompressionMethod` picks between with UNCOMPRESSED and ZSTD.
//!
//! The FSST analyze of C++ samples a quarter of the vectors at random (`ANALYSIS_SAMPLE_SIZE`, from an unseeded
//! `RandomEngine`), so its score is not the same from run to run. Here it is the score that sampling gives on
//! average: the vectors up to the first with a string to train on, which C++ always takes, count four times, as
//! the sample is scaled up by four, and the vectors after them once. The symbol table is trained on all of them.

use super::dict_fsst_analyze::{bit_width, packed_size};
use super::fsst::SymbolTable;
use pintail_common::STANDARD_VECTOR_SIZE;
use hashbrown::HashSet;

/// `DictionaryCompression::MINIMUM_COMPRESSION_RATIO`, a `float`.
const DICTIONARY_COMPRESSION_RATIO: f32 = 1.2;
/// `FSSTStorage::MINIMUM_COMPRESSION_RATIO`, a `double`.
const FSST_COMPRESSION_RATIO: f64 = 1.2;
/// `sizeof(dictionary_compression_header_t)`.
const DICTIONARY_HEADER_SIZE: usize = 20;
/// `FSSTStorage::ANALYSIS_SAMPLE_SIZE`.
const ANALYSIS_SAMPLE_SIZE: f64 = 0.25;
/// `sizeof(duckdb_fsst_decoder_t)`.
const FSST_DECODER_SIZE: usize = 2304;

/// `DictionaryCompression::RequiredSpace`.
fn dictionary_required_space(count: usize, unique_count: usize, dict_size: usize, width: usize) -> usize {
    DICTIONARY_HEADER_SIZE + dict_size + unique_count * 4 + packed_size(count, width)
}

/// `DictionaryCompressionStorage::StringFinalAnalyze` after `DictionaryCompression::UpdateState` of the strings
/// (`strings[i]` is empty where `valid[i]` is not set): the blocks the segments that fill take, and what the last
/// one needs. `None` where a string is too long for a segment (`limit`, `GetStringBlockLimit`).
pub fn dictionary_final_analyze(strings: &[&[u8]], valid: &[bool], block_size: usize, limit: usize) -> Option<usize> {
    let mut segment_count = 0;
    let mut tuple_count = 0;
    let mut unique_count = 0;
    let mut dict_size = 0;
    let mut width = 0;
    let mut set: HashSet<&[u8]> = HashSet::new();
    for (string, valid) in strings.iter().zip(valid) {
        let size = if *valid { string.len() } else { 0 };
        if *valid && size >= limit {
            return None;
        }
        let mut new_string = *valid && !set.contains(*string);
        // `CalculateSpaceRequirements`: a new string takes a code more (one for NULL, one for it)
        let fits = |new_string: bool, tuple_count: usize, unique_count: usize, dict_size: usize, width: usize| {
            if new_string {
                let next_width = bit_width(unique_count + 2);
                let required =
                    dictionary_required_space(tuple_count + 1, unique_count + 1, dict_size + size, next_width);
                (required <= block_size, next_width)
            } else {
                let required = dictionary_required_space(tuple_count + 1, unique_count, dict_size, width);
                (required <= block_size, width)
            }
        };
        let (mut fit, mut next_width) = fits(new_string, tuple_count, unique_count, dict_size, width);
        if !fit {
            // `Flush`
            segment_count += 1;
            tuple_count = 0;
            unique_count = 0;
            dict_size = 0;
            set.clear();
            new_string = true;
            (fit, next_width) = fits(new_string, tuple_count, unique_count, dict_size, width);
            debug_assert!(fit, "a string below the block limit fits a new segment");
        }
        tuple_count += 1;
        if *valid && new_string {
            unique_count += 1;
            dict_size += size;
            set.insert(string);
            width = next_width;
        }
    }
    let width = bit_width(unique_count + 1);
    let required = dictionary_required_space(tuple_count, unique_count, dict_size, width);
    let total = segment_count * block_size + required;
    Some((DICTIONARY_COMPRESSION_RATIO * total as f32) as usize)
}

/// `FSSTStorage::StringFinalAnalyze`, as its sampling gives it on average (see the module). `None` where a string
/// is too long for a segment, or where there is no string to train on (`INVALID_INDEX`, which is never chosen).
pub fn fsst_final_analyze(strings: &[&[u8]], valid: &[bool], block_size: usize, limit: usize) -> Option<usize> {
    if strings
        .iter()
        .zip(valid)
        .any(|(string, valid)| *valid && string.len() >= limit)
    {
        return None;
    }
    // `have_valid_row`: the vectors up to the first with a string that isn't empty are always sampled
    let first_sampled = strings
        .iter()
        .zip(valid)
        .position(|(string, valid)| *valid && !string.is_empty())?;
    let always_sampled = (first_sampled / STANDARD_VECTOR_SIZE + 1) * STANDARD_VECTOR_SIZE;
    let weight = |row: usize| {
        if row < always_sampled {
            1.0 / ANALYSIS_SAMPLE_SIZE
        } else {
            1.0
        }
    };
    let samples: Vec<&[u8]> = strings
        .iter()
        .zip(valid)
        .filter(|(string, valid)| **valid && !string.is_empty())
        .map(|(string, _)| *string)
        .collect();
    let table = SymbolTable::train(&samples)?;
    let mut compressed_size = 0.0;
    let mut longest = 0;
    let mut offsets = 0.0;
    let mut out = Vec::new();
    for (row, (string, valid)) in strings.iter().zip(valid).enumerate() {
        if !*valid {
            continue;
        }
        offsets += weight(row);
        if string.is_empty() {
            continue;
        }
        out.clear();
        table.compress(string, &mut out);
        compressed_size += out.len() as f64 * weight(row);
        longest = longest.max(out.len());
    }
    // `GetRequiredSize` of the offsets is in groups of 32 values; their count is as the sample scaled up gives it
    let offsets_size = packed_size(offsets.round() as usize, bit_width(longest)) as f64;
    let base_size = offsets_size + compressed_size;
    let blocks = base_size / (block_size - FSST_DECODER_SIZE) as f64;
    let estimated = base_size + blocks * FSST_DECODER_SIZE as f64;
    Some((estimated * FSST_COMPRESSION_RATIO) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_of_few_values_in_one_segment() {
        let strings: Vec<Vec<u8>> = (0..1000).map(|i| format!("v{}", i % 10).into_bytes()).collect();
        let refs: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
        let valid = vec![true; refs.len()];
        // 10 values of 2 bytes, 10 indices, 1000 codes of 4 bits
        let required = DICTIONARY_HEADER_SIZE + 20 + 40 + packed_size(1000, 4);
        let expected = (1.2f32 * required as f32) as usize;
        assert_eq!(dictionary_final_analyze(&refs, &valid, 262_144, 4096), Some(expected));
    }

    #[test]
    fn fsst_needs_a_string_to_train_on() {
        assert_eq!(fsst_final_analyze(&[b"", b""], &[true, false], 262_144, 4096), None);
    }
}
