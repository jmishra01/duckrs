// Ported from duckdb/src/storage/compression/bitpacking.cpp and
// duckdb/src/include/duckdb/common/bitpacking.hpp @ rust-port-baseline
//
//! `BitpackingState`: groups of 2048 values, each stored as a constant, a constant delta, packed
//! deltas or packed differences to a frame of reference, as C++ decides it. The same decisions give
//! the size `BitpackingFinalAnalyze` scores a column with and the segments it is written as.
//!
//! Values are kept as their raw bits; `Kind` says how wide their type is and whether it is signed,
//! so the arithmetic of the type (`T`) and of its signed counterpart (`T_S`) can be done on them,
//! with the overflow checks of `TrySubtractOperator`.

use super::encode::Encoded;

/// `BITPACKING_METADATA_GROUP_SIZE`.
pub const GROUP_ROWS: usize = 2048;
/// `BITPACKING_ALGORITHM_GROUP_SIZE`.
const ALGORITHM_GROUP: usize = 32;
/// `BITPACKING_HEADER_SIZE`.
const HEADER_SIZE: usize = 8;
/// `sizeof(bitpacking_metadata_encoded_t)`.
const METADATA_SIZE: usize = 4;
/// `AlignValue(sizeof(bitpacking_width_t))`.
const ALIGNED_WIDTH_SIZE: usize = 8;

const MODE_CONSTANT: u32 = 2;
const MODE_CONSTANT_DELTA: u32 = 3;
const MODE_DELTA_FOR: u32 = 4;
const MODE_FOR: u32 = 5;

/// `BitpackingMode`, as `force_bitpacking_mode` sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Auto,
    Constant,
    ConstantDelta,
    DeltaFor,
    For,
}

impl Mode {
    /// `EnumUtil::FromString<BitpackingMode>`; anything else is AUTO.
    pub fn parse(text: &str) -> Self {
        match text.to_lowercase().as_str() {
            "constant" => Self::Constant,
            "constant_delta" => Self::ConstantDelta,
            "delta_for" => Self::DeltaFor,
            "for" => Self::For,
            _ => Self::Auto,
        }
    }
}

/// The type of the values: its width in bytes, and whether it is signed.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub width: usize,
    pub signed: bool,
}

impl Kind {
    fn bits(self) -> u32 {
        self.width as u32 * 8
    }

    fn mask(self) -> u128 {
        if self.bits() == 128 {
            u128::MAX
        } else {
            (1u128 << self.bits()) - 1
        }
    }

    /// The value of raw bits as a signed number of the width (`T_S`).
    fn as_signed(self, raw: u128) -> i128 {
        let bits = self.bits();
        if bits == 128 {
            return raw as i128;
        }
        let shift = 128 - bits;
        ((raw << shift) as i128) >> shift
    }

    /// Whether `a < b` in the type `T`.
    fn less(self, a: u128, b: u128) -> bool {
        if self.signed {
            self.as_signed(a) < self.as_signed(b)
        } else {
            a < b
        }
    }

    /// The bounds of `T_S`.
    fn signed_bounds(self) -> (i128, i128) {
        let bits = self.bits();
        if bits == 128 {
            (i128::MIN, i128::MAX)
        } else {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        }
    }

    /// `TrySubtractOperator` in `T_S`.
    fn signed_sub(self, a: i128, b: i128) -> Option<i128> {
        let (low, high) = self.signed_bounds();
        a.checked_sub(b).filter(|result| (low..=high).contains(result))
    }

    /// `TrySubtractOperator` in `T`: the difference as raw bits.
    fn sub(self, a: u128, b: u128) -> Option<u128> {
        if self.signed {
            self.signed_sub(self.as_signed(a), self.as_signed(b))
                .map(|d| d as u128 & self.mask())
        } else {
            a.checked_sub(b)
        }
    }

    /// `GetEffectiveWidth`: a width this close to that of the type takes all of it.
    fn effective_width(self, width: u32) -> u32 {
        if width + self.width as u32 > self.bits() {
            self.bits()
        } else {
            width
        }
    }

    /// `MinimumBitWidth<T, is_signed>` of a single value (both its minimum and maximum).
    fn minimum_bit_width(self, raw: u128, is_signed: bool) -> u32 {
        let mut value: u128 = if is_signed {
            let signed = self.as_signed(raw);
            if signed == self.signed_bounds().0 {
                return self.bits();
            }
            signed.unsigned_abs()
        } else if self.signed {
            // the loop shifts the signed value: a negative one never becomes zero
            let signed = self.as_signed(raw);
            if signed < 0 {
                return self.bits();
            }
            signed as u128
        } else {
            raw
        };
        if value == 0 {
            return 0;
        }
        let mut width = u32::from(is_signed);
        while value != 0 {
            width += 1;
            value >>= 1;
        }
        self.effective_width(width)
    }
}

/// `GetRequiredSize`.
fn required_size(count: usize, width: u32) -> usize {
    count.next_multiple_of(ALGORITHM_GROUP) * width as usize / 8
}

/// `PackBuffer`: `width` bits of each value, after one another from the lowest bit, in groups of
/// 32 values (the last one filled up with zeros).
fn pack(values: &[u128], width: u32) -> Vec<u8> {
    let width = width as usize;
    let mut packed = vec![0u8; required_size(values.len(), width as u32)];
    for (row, value) in values.iter().enumerate() {
        let start = row * width;
        for bit in 0..width {
            if value >> bit & 1 != 0 {
                let at = start + bit;
                packed[at / 8] |= 1 << (at % 8);
            }
        }
    }
    packed
}

/// A group as it is stored: its mode, its bytes, and what `total_size` counts for it.
#[derive(Debug)]
pub struct Group {
    pub mode: u32,
    pub bytes: Vec<u8>,
    pub analyzed_size: usize,
    pub rows: usize,
}

/// `BitpackingState::Flush` of one group. `None` where the group can't be bitpacked.
fn flush_group(values: &[u128], valid: &[bool], kind: Kind, mode: Mode) -> Option<Group> {
    let width = kind.width;
    let raw = |value: u128| value.to_le_bytes()[..width].to_vec();
    let rows = values.len();
    let all_valid = valid.iter().all(|v| *v);
    let all_invalid = !valid.iter().any(|v| *v);
    // `Reset`: the minimum starts at the largest value of the type and the maximum at the smallest
    let (type_min, type_max) = if kind.signed {
        let (low, high) = kind.signed_bounds();
        (low as u128 & kind.mask(), high as u128 & kind.mask())
    } else {
        (0, kind.mask())
    };
    let (mut minimum, mut maximum) = (type_max, type_min);
    for (value, _) in values.iter().zip(valid).filter(|(_, valid)| **valid) {
        if kind.less(*value, minimum) {
            minimum = *value;
        }
        if kind.less(maximum, *value) {
            maximum = *value;
        }
    }
    if (all_invalid || maximum == minimum) && matches!(mode, Mode::Auto | Mode::Constant) {
        return Some(Group {
            mode: MODE_CONSTANT,
            bytes: raw(maximum),
            analyzed_size: width + METADATA_SIZE,
            rows,
        });
    }
    // `CalculateFORStats`
    let min_max_diff = kind.sub(maximum, minimum);
    let can_do_for = min_max_diff.is_some();
    let min_max_diff = min_max_diff.unwrap_or(0);

    // `CalculateDeltaStats`
    let delta = (|| {
        if !kind.signed && maximum > kind.signed_bounds().1 as u128 {
            return None;
        }
        if rows < 2 || !all_valid {
            return None;
        }
        let signed: Vec<i128> = values.iter().map(|v| kind.as_signed(*v)).collect();
        let mut deltas = Vec::with_capacity(rows);
        let mut previous = 0i128;
        for value in &signed {
            deltas.push(kind.signed_sub(*value, previous)?);
            previous = *value;
        }
        let maximum_delta = *deltas[1..].iter().max().expect("two rows");
        let minimum_delta = *deltas[1..].iter().min().expect("two rows");
        deltas[0] = minimum_delta;
        let min_max_delta_diff = kind.signed_sub(maximum_delta, minimum_delta)?;
        let delta_offset = kind.signed_sub(signed[0], minimum_delta)?;
        Some((deltas, minimum_delta, maximum_delta, min_max_delta_diff, delta_offset))
    })();

    if let Some((deltas, minimum_delta, maximum_delta, min_max_delta_diff, delta_offset)) = delta {
        if maximum_delta == minimum_delta && !matches!(mode, Mode::For | Mode::DeltaFor) {
            let mut bytes = raw(values[0]);
            bytes.extend(raw(maximum_delta as u128 & kind.mask()));
            return Some(Group {
                mode: MODE_CONSTANT_DELTA,
                bytes,
                analyzed_size: 2 * width + METADATA_SIZE,
                rows,
            });
        }
        let delta_width = kind.minimum_bit_width(min_max_delta_diff as u128 & kind.mask(), false);
        let regular_width = kind.minimum_bit_width(min_max_diff, kind.signed);
        let prefer_for = can_do_for && delta_width >= regular_width;
        if !prefer_for && mode != Mode::For {
            let packed: Vec<u128> = deltas
                .iter()
                .map(|d| (*d as u128).wrapping_sub(minimum_delta as u128) & kind.mask())
                .collect();
            let mut bytes = raw(minimum_delta as u128 & kind.mask());
            bytes.extend(raw(u128::from(delta_width)));
            bytes.extend(raw(delta_offset as u128 & kind.mask()));
            bytes.extend(pack(&packed, delta_width));
            return Some(Group {
                mode: MODE_DELTA_FOR,
                bytes,
                analyzed_size: width + ALIGNED_WIDTH_SIZE + width + required_size(rows, delta_width),
                rows,
            });
        }
    }
    if can_do_for {
        let for_width = kind.minimum_bit_width(min_max_diff, false);
        // the rows that are NULL hold whatever was there; the minimum packs as nothing
        let packed: Vec<u128> = values
            .iter()
            .zip(valid)
            .map(|(value, valid)| {
                if *valid {
                    value.wrapping_sub(minimum) & kind.mask()
                } else {
                    0
                }
            })
            .collect();
        let mut bytes = raw(minimum);
        bytes.extend(raw(u128::from(for_width)));
        bytes.extend(pack(&packed, for_width));
        return Some(Group {
            mode: MODE_FOR,
            bytes,
            analyzed_size: required_size(rows, for_width) + width + ALIGNED_WIDTH_SIZE,
            rows,
        });
    }
    None
}

/// The groups a column is bitpacked as, or `None` where bitpacking can't store it (`BitpackingAnalyze`
/// failing). `data` holds the values of the rows, `width` bytes each.
pub fn groups(data: &[u8], valid: &[bool], kind: Kind, mode: Mode, block_size: usize) -> Option<Vec<Group>> {
    // the groups can be larger than a block: then bitpacking is out, conservatively by a factor 2
    if kind.width * GROUP_ROWS * 2 > block_size {
        return None;
    }
    let width = kind.width;
    let mut result = Vec::with_capacity(valid.len().div_ceil(GROUP_ROWS));
    for (chunk, valid) in data.chunks(GROUP_ROWS * width).zip(valid.chunks(GROUP_ROWS)) {
        let values: Vec<u128> = chunk
            .chunks_exact(width)
            .map(|bytes| {
                let mut value = [0u8; 16];
                value[..width].copy_from_slice(bytes);
                u128::from_le_bytes(value)
            })
            .collect();
        result.push(flush_group(&values, valid, kind, mode)?);
    }
    Some(result)
}

/// `BitpackingFinalAnalyze`: the `total_size` of the groups.
pub fn analyzed_size(groups: &[Group]) -> usize {
    groups.iter().map(|group| group.analyzed_size).sum()
}

/// `BitpackingCompressionState`: the groups after one another in segments, each with, from the back
/// of the block, where each group starts and its mode; a segment ends where the next group no longer
/// fits (`CanStore`), and is compacted with its metadata next to its data (`FlushSegment`).
pub fn segments(groups: Vec<Group>, block_size: usize) -> Vec<Encoded> {
    let mut segments = Vec::new();
    let mut body = vec![0u8; HEADER_SIZE];
    let mut entries: Vec<u32> = Vec::new();
    let mut rows = 0;
    let finish = |body: &mut Vec<u8>, entries: &mut Vec<u32>, rows: &mut usize, segments: &mut Vec<Encoded>| {
        let metadata_at = body.len().next_multiple_of(8);
        body.resize(metadata_at, 0);
        for entry in entries.iter().rev() {
            body.extend_from_slice(&entry.to_le_bytes());
        }
        let end = body.len() as u64;
        body[..HEADER_SIZE].copy_from_slice(&end.to_le_bytes());
        segments.push((std::mem::replace(body, vec![0u8; HEADER_SIZE]), *rows));
        entries.clear();
        *rows = 0;
    };
    for group in groups {
        // `CanStore`: the data aligned, and one more entry of metadata, below the header
        let free = block_size - entries.len() * METADATA_SIZE - body.len();
        if group.bytes.len().next_multiple_of(8) + METADATA_SIZE + HEADER_SIZE > free && rows > 0 {
            finish(&mut body, &mut entries, &mut rows, &mut segments);
        }
        entries.push(body.len() as u32 | group.mode << 24);
        body.extend_from_slice(&group.bytes);
        rows += group.rows;
    }
    if rows > 0 {
        finish(&mut body, &mut entries, &mut rows, &mut segments);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(values: &[i64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    const BIGINT: Kind = Kind { width: 8, signed: true };

    #[test]
    fn modes_as_cpp_chooses_them() {
        let valid = vec![true; 5];
        let constant = groups(&bytes_of(&[7; 5]), &valid, BIGINT, Mode::Auto, 262144).unwrap();
        assert_eq!(constant[0].mode, MODE_CONSTANT);
        let steps = groups(&bytes_of(&[2, 4, 6, 8, 10]), &valid, BIGINT, Mode::Auto, 262144).unwrap();
        assert_eq!(steps[0].mode, MODE_CONSTANT_DELTA);
        // a sequence with small, varying steps: deltas take fewer bits than the range
        let ramp: Vec<i64> = (0..100).map(|i| i * 1000 + i % 3).collect();
        let ramp = groups(&bytes_of(&ramp), &[true; 100], BIGINT, Mode::Auto, 262144).unwrap();
        assert_eq!(ramp[0].mode, MODE_DELTA_FOR);
        // small steps either way: deltas need 3 bits, the frame of reference 4 with the sign bit
        let wobble = groups(&bytes_of(&[5, 1, 4, 2, 3]), &valid, BIGINT, Mode::Auto, 262144).unwrap();
        assert_eq!(wobble[0].mode, MODE_DELTA_FOR);
        // jumps as wide as the range: the frame of reference wins on equal widths
        let scattered = groups(&bytes_of(&[1, 100, 2, 99, 3]), &valid, BIGINT, Mode::Auto, 262144).unwrap();
        assert_eq!(scattered[0].mode, MODE_FOR);
        // forcing CONSTANT on values that differ still finds a constant delta
        let forced = groups(&bytes_of(&[2, 4, 6, 8, 10]), &valid, BIGINT, Mode::Constant, 262144).unwrap();
        assert_eq!(forced[0].mode, MODE_CONSTANT_DELTA);
    }

    #[test]
    fn widths_round_up_near_the_type_width() {
        assert_eq!(BIGINT.minimum_bit_width(1u128 << 56, false), 64);
        assert_eq!(BIGINT.minimum_bit_width(255, false), 8);
        assert_eq!(BIGINT.minimum_bit_width(255, true), 9);
    }
}
