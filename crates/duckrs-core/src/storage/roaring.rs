// Ported from duckdb/src/storage/compression/roaring/{analyze,common,metadata}.cpp @ rust-port-baseline
//
//! `RoaringAnalyzeState`: what Roaring compression estimates a column of bits to take. The bits are
//! cut into containers of 2048; each is an array of the positions of its zeros or of its ones, a list
//! of its runs of zeros, or a bitset, whichever `ContainerMetadata::CreateMetadata` finds cheapest.

/// `ROARING_CONTAINER_SIZE`.
const CONTAINER_SIZE: usize = 2048;
/// `COMPRESSED_SEGMENT_SIZE` and `COMPRESSED_SEGMENT_COUNT`.
const COMPRESSED_SEGMENT_COUNT: usize = CONTAINER_SIZE / 256;
/// `UNCOMPRESSED_SIZE`: the bitset of a container, in 64-bit words.
const UNCOMPRESSED_SIZE: usize = CONTAINER_SIZE / 8;
const MAX_RUN_IDX: usize = (UNCOMPRESSED_SIZE - COMPRESSED_SEGMENT_COUNT) / 2;
const MAX_ARRAY_IDX: usize = UNCOMPRESSED_SIZE - COMPRESSED_SEGMENT_COUNT;
const COMPRESSED_ARRAY_THRESHOLD: usize = 8;
const COMPRESSED_RUN_THRESHOLD: usize = 4;
const CONTAINER_TYPE_BITWIDTH: usize = 2;
const RUN_CONTAINER_SIZE_BITWIDTH: usize = 7;
/// `ROARING_COMPRESS_PENALTY`.
const COMPRESS_PENALTY: f64 = 2.0;

/// `ContainerMetadata`, as far as sizes go.
#[derive(Debug, Clone, Copy)]
enum Container {
    Bitset,
    Array(usize),
    Run(usize),
}

/// `ContainerMetadata::CreateMetadata`.
fn create_metadata(count: usize, zeros: usize, ones: usize, runs: usize) -> Container {
    let can_use_array = zeros < MAX_ARRAY_IDX || ones < MAX_ARRAY_IDX;
    let can_use_run = runs < MAX_RUN_IDX;
    if !can_use_array && !can_use_run {
        return Container::Bitset;
    }
    // the costs are `uint16_t` in C++
    let cost = |amount: usize, threshold: usize, plain: usize, compressed: usize| -> usize {
        let cost = if amount < threshold {
            amount * plain
        } else {
            COMPRESSED_SEGMENT_COUNT + amount * compressed
        };
        cost & 0xFFFF
    };
    let null_array_cost = cost(zeros, COMPRESSED_ARRAY_THRESHOLD, 2, 1);
    let non_null_array_cost = cost(ones, COMPRESSED_ARRAY_THRESHOLD, 2, 1);
    let lowest_array_cost = null_array_cost.min(non_null_array_cost);
    let lowest_run_cost = cost(runs, COMPRESSED_RUN_THRESHOLD, 4, 2);
    let bitset_cost = count.next_multiple_of(64) / 64 * 8;
    if lowest_array_cost.min(lowest_run_cost) > bitset_cost {
        return Container::Bitset;
    }
    if lowest_array_cost <= lowest_run_cost {
        Container::Array(zeros.min(ones))
    } else {
        Container::Run(runs)
    }
}

/// `ContainerMetadata::GetDataSizeInBytes`.
fn data_size(container: Container, count: usize) -> usize {
    match container {
        Container::Bitset => count / 64 * 8,
        Container::Run(runs) if runs >= COMPRESSED_RUN_THRESHOLD => COMPRESSED_SEGMENT_COUNT + runs * 2,
        Container::Run(runs) => 4 * runs,
        Container::Array(cardinality) if cardinality >= COMPRESSED_ARRAY_THRESHOLD => {
            COMPRESSED_SEGMENT_COUNT + cardinality
        }
        Container::Array(cardinality) => 2 * cardinality,
    }
}

/// `GetRequiredSize` of the bitpacker.
fn packed_size(count: usize, width: usize) -> usize {
    count.next_multiple_of(32) * width / 8
}

/// `ContainerMetadataCollection::GetMetadataSize`.
fn metadata_size(containers: usize, runs: usize, arrays: usize) -> usize {
    packed_size(containers, CONTAINER_TYPE_BITWIDTH) + packed_size(runs, RUN_CONTAINER_SIZE_BITWIDTH) + arrays
}

/// `RoaringFinalAnalyze`: the estimated size of `bits` (set for a valid row, or a true value), times
/// the penalty of compressing. `block_size` is what a segment can hold.
pub fn final_analyze(bits: &[bool], block_size: usize) -> usize {
    let mut total_size = 0usize;
    let (mut data, mut metadata) = (0usize, 0usize);
    let mut current_count = 0usize;
    // the containers of the segment so far, by kind
    let (mut runs_in_segment, mut arrays_in_segment) = (0usize, 0usize);
    let mut flush_segment =
        |data: &mut usize, metadata: &mut usize, current: &mut usize, runs: &mut usize, arrays: &mut usize| {
            if *current == 0 {
                return;
            }
            total_size += *data + *metadata;
            (*data, *metadata, *current, *runs, *arrays) = (0, 0, 0, 0, 0);
        };
    for container in bits.chunks(CONTAINER_SIZE) {
        // `HandleBit`: a run starts at each zero after a one, or at the start
        let (mut zeros, mut ones, mut runs) = (0usize, 0usize, 0usize);
        let mut last_set = None;
        for bit in container {
            if !bit && last_set.is_none_or(|last| last) {
                runs += 1;
            }
            if *bit {
                ones += 1
            } else {
                zeros += 1
            }
            last_set = Some(*bit);
        }
        let count = container.len();
        // `FlushContainer`
        let kind = create_metadata(count, zeros, ones, runs);
        let (mut runs_count, mut arrays_count) = (runs_in_segment, arrays_in_segment);
        if matches!(kind, Container::Run(_)) {
            runs_count += 1;
        } else {
            arrays_count += 1;
        }
        metadata = metadata_size(runs_count + arrays_count, runs_count, arrays_count);
        data += data_size(kind, count);
        if metadata + data > block_size {
            flush_segment(
                &mut data,
                &mut metadata,
                &mut current_count,
                &mut runs_in_segment,
                &mut arrays_in_segment,
            );
        }
        if matches!(kind, Container::Run(_)) {
            runs_in_segment += 1;
        } else {
            arrays_in_segment += 1;
        }
        current_count += count;
    }
    flush_segment(
        &mut data,
        &mut metadata,
        &mut current_count,
        &mut runs_in_segment,
        &mut arrays_in_segment,
    );
    (total_size as f64 * COMPRESS_PENALTY) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `roaring_analyze_array.test`: one valid row in 25, of 122880.
    #[test]
    fn scores_as_cpp_does() {
        let bits: Vec<bool> = (0..122_880).map(|i| i % 25 == 0).collect();
        assert_eq!(final_analyze(&bits, 262_144 - 8), 10944);
    }
}
