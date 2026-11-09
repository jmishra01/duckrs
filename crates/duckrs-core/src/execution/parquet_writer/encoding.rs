// Ported from duckdb/extension/parquet/include/{parquet_rle_bp_encoder,parquet_dbp_encoder,parquet_dlba_encoder,
// parquet_bss_encoder}.hpp, decode_utils.hpp and the BloomFilter of parquet_statistics.cpp, and the Thrift compact
// protocol that the generated parquet_types.cpp writes with @ rust-port-baseline
//
//! The byte-level pieces of the Parquet writer: Thrift's compact protocol, the encodings of values and levels, and
//! split block bloom filters.

/// `ParquetDecodeUtils::VarintEncode`: ULEB128.
pub fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// `ParquetDecodeUtils::IntToZigzag`.
pub fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// `RleBpDecoder::ComputeBitWidthFromMaxValue`.
pub fn bit_width_of_max(max_value: u64) -> u32 {
    if max_value == 0 {
        return 0;
    }
    64 - max_value.leading_zeros()
}

/// `RleBpDecoder::ComputeBitWidthFromValueCount`: a dictionary of one entry still gets one bit.
pub fn bit_width_of_count(count: u64) -> u32 {
    match count {
        0 => 0,
        1 => 1,
        _ => bit_width_of_max(count - 1),
    }
}

/// `ParquetDecodeUtils::BitPackAligned`: the low `width` bits of each value, least significant bit first.
fn bit_pack(values: &[u64], width: u32, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let total_bits = values.len() * width as usize;
    let start = out.len();
    out.resize(start + total_bits.div_ceil(8), 0);
    let mut bit = 0usize;
    for &value in values {
        for b in 0..width {
            if (value >> b) & 1 == 1 {
                out[start + (bit >> 3)] |= 1 << (bit & 7);
            }
            bit += 1;
        }
    }
}

/// `RleBpEncoder`: runs of at least four equal values as RLE runs, the rest bit-packed in blocks of 256.
pub struct RleBpEncoder {
    bit_width: u32,
    byte_width: u32,
    rle_value: u32,
    rle_count: usize,
    /// like C++, values a partial block leaves behind stay in it, and are packed along
    bp_block: [u32; Self::BP_BLOCK_SIZE],
    bp_block_count: usize,
}

impl RleBpEncoder {
    const MINIMUM_RLE_COUNT: usize = 4;
    const BP_BLOCK_SIZE: usize = 256;

    pub fn new(bit_width: u32) -> Self {
        Self {
            bit_width,
            byte_width: bit_width.div_ceil(8),
            rle_value: 0,
            rle_count: 0,
            bp_block: [0; Self::BP_BLOCK_SIZE],
            bp_block_count: 0,
        }
    }

    pub fn begin_write(&mut self) {
        self.rle_count = 0;
        self.bp_block_count = 0;
    }

    pub fn write_value(&mut self, out: &mut Vec<u8>, value: u32) {
        if self.bp_block_count != 0 {
            self.bp_block[self.bp_block_count] = value;
            self.bp_block_count += 1;
            if self.bp_block_count == Self::BP_BLOCK_SIZE {
                self.write_run(out);
            }
            return;
        }
        if self.rle_count == 0 {
            self.rle_value = value;
            self.rle_count = 1;
            return;
        }
        if self.rle_value == value {
            self.rle_count += 1;
            return;
        }
        if self.rle_count >= Self::MINIMUM_RLE_COUNT {
            self.write_run(out);
            self.rle_value = value;
            self.rle_count = 1;
            return;
        }
        for _ in 0..self.rle_count {
            self.bp_block[self.bp_block_count] = self.rle_value;
            self.bp_block_count += 1;
        }
        self.bp_block[self.bp_block_count] = value;
        self.bp_block_count += 1;
        self.rle_count = 0;
    }

    pub fn write_many(&mut self, out: &mut Vec<u8>, value: u32, mut count: usize) {
        if self.rle_count != 0 {
            self.write_value(out, value);
            count -= 1;
        }
        while self.bp_block_count != 0 && count > 0 {
            self.write_value(out, value);
            count -= 1;
        }
        self.rle_value = value;
        self.rle_count += count;
    }

    pub fn finish_write(&mut self, out: &mut Vec<u8>) {
        self.write_run(out);
    }

    fn write_run(&mut self, out: &mut Vec<u8>) {
        if self.rle_count != 0 {
            varint(out, (self.rle_count as u64) << 1);
            let bytes = self.rle_value.to_le_bytes();
            out.extend_from_slice(&bytes[..self.byte_width as usize]);
            self.rle_count = 0;
        } else if self.bp_block_count != 0 {
            varint(out, ((Self::BP_BLOCK_SIZE / 8) as u64) << 1 | 1);
            let values: Vec<u64> = self.bp_block.iter().map(|&v| u64::from(v)).collect();
            bit_pack(&values, self.bit_width, out);
            self.bp_block_count = 0;
        }
    }
}

/// `DbpEncoder`: DELTA_BINARY_PACKED, in blocks of 2048 values and eight miniblocks.
pub struct DbpEncoder {
    total_value_count: u64,
    count: u64,
    previous_value: i64,
    min_delta: i64,
    data: Vec<i64>,
}

impl DbpEncoder {
    const BLOCK_SIZE_IN_VALUES: usize = 2048;
    const MINIBLOCKS: usize = 8;
    const VALUES_IN_MINIBLOCK: usize = Self::BLOCK_SIZE_IN_VALUES / Self::MINIBLOCKS;

    pub fn new(total_value_count: u64) -> Self {
        Self {
            total_value_count,
            count: 0,
            previous_value: 0,
            min_delta: i64::MAX,
            data: Vec::with_capacity(Self::BLOCK_SIZE_IN_VALUES),
        }
    }

    pub fn begin_write(&mut self, out: &mut Vec<u8>, first_value: i64) {
        varint(out, Self::BLOCK_SIZE_IN_VALUES as u64);
        varint(out, Self::MINIBLOCKS as u64);
        varint(out, self.total_value_count);
        varint(out, zigzag(first_value));
        if self.total_value_count != 0 {
            self.count += 1;
        }
        self.previous_value = first_value;
        self.min_delta = i64::MAX;
        self.data.clear();
    }

    pub fn write_value(&mut self, out: &mut Vec<u8>, value: i64) {
        let delta = value.wrapping_sub(self.previous_value);
        self.previous_value = value;
        self.min_delta = self.min_delta.min(delta);
        self.data.push(delta);
        if self.data.len() == Self::BLOCK_SIZE_IN_VALUES {
            self.write_block(out);
        }
    }

    pub fn finish_write(&mut self, out: &mut Vec<u8>) {
        if !self.data.is_empty() {
            self.write_block(out);
        }
    }

    fn write_block(&mut self, out: &mut Vec<u8>) {
        let block_count = self.data.len();
        let miniblocks = block_count.div_ceil(Self::VALUES_IN_MINIBLOCK);
        let mut values = vec![0u64; miniblocks * Self::VALUES_IN_MINIBLOCK];
        for (i, delta) in self.data.iter().enumerate() {
            values[i] = delta.wrapping_sub(self.min_delta) as u64;
        }
        let mut widths = [0u8; Self::MINIBLOCKS];
        for (miniblock, width) in widths.iter_mut().enumerate().take(miniblocks) {
            let part = &values[miniblock * Self::VALUES_IN_MINIBLOCK..(miniblock + 1) * Self::VALUES_IN_MINIBLOCK];
            let max = part.iter().copied().max().unwrap_or(0);
            // `BitpackingPrimitives::GetEffectiveWidth`: past 56 bits, all 64
            let bits = bit_width_of_max(max);
            *width = if bits > 56 { 64 } else { bits as u8 };
        }
        varint(out, zigzag(self.min_delta));
        out.extend_from_slice(&widths);
        for (miniblock, width) in widths.iter().enumerate().take(miniblocks) {
            let part = &values[miniblock * Self::VALUES_IN_MINIBLOCK..(miniblock + 1) * Self::VALUES_IN_MINIBLOCK];
            bit_pack(part, u32::from(*width), out);
        }
        self.count += block_count as u64;
        self.min_delta = i64::MAX;
        self.data.clear();
    }
}

/// `DlbaEncoder`: DELTA_LENGTH_BYTE_ARRAY, the lengths delta encoded, then the bytes.
pub struct DlbaEncoder {
    lengths: DbpEncoder,
    bytes: Vec<u8>,
}

impl DlbaEncoder {
    pub fn new(total_value_count: u64) -> Self {
        Self {
            lengths: DbpEncoder::new(total_value_count),
            bytes: Vec::new(),
        }
    }

    pub fn begin_write(&mut self, out: &mut Vec<u8>, first_value: &[u8]) {
        self.lengths.begin_write(out, first_value.len() as i64);
        self.bytes.extend_from_slice(first_value);
    }

    pub fn write_value(&mut self, out: &mut Vec<u8>, value: &[u8]) {
        self.lengths.write_value(out, value.len() as i64);
        self.bytes.extend_from_slice(value);
    }

    pub fn finish_write(&mut self, out: &mut Vec<u8>) {
        self.lengths.finish_write(out);
        out.extend_from_slice(&self.bytes);
    }
}

/// `BssEncoder`: BYTE_STREAM_SPLIT, the first bytes of all values, then the second bytes, and so on.
pub struct BssEncoder {
    total_value_count: usize,
    width: usize,
    count: usize,
    buffer: Vec<u8>,
}

impl BssEncoder {
    pub fn new(total_value_count: usize, width: usize) -> Self {
        Self {
            total_value_count,
            width,
            count: 0,
            buffer: vec![0; total_value_count * width],
        }
    }

    pub fn write_value(&mut self, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate().take(self.width) {
            self.buffer[i * self.total_value_count + self.count] = byte;
        }
        self.count += 1;
    }

    pub fn finish_write(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.buffer);
    }
}

/// `ParquetBloomFilter`: a split block bloom filter of 32-byte blocks.
pub struct BloomFilter {
    pub data: Vec<u8>,
}

impl BloomFilter {
    const BLOCK: usize = 32;
    const SALT: [u32; 8] = [
        0x47b6_137b,
        0x4497_4d91,
        0x8824_ad5b,
        0xa2b7_289d,
        0x7054_95c7,
        0x2df1_424b,
        0x9efc_4947,
        0x5c6b_fb31,
    ];

    /// `BloomFilterSize`: bytes for `entries` values at false positive rate `target`, `None` past 128 MiB.
    pub fn size(entries: u64, target: f64) -> Option<usize> {
        let bytes = -(entries as f64) / (-target.powf(1.0 / 8.0)).ln_1p();
        if !bytes.is_finite() || bytes > (128 * 1024 * 1024) as f64 {
            return None;
        }
        Some((bytes as usize).next_power_of_two().max(Self::BLOCK))
    }

    pub fn create(entries: u64, target: f64) -> Option<Self> {
        Self::size(entries, target).map(|size| Self { data: vec![0; size] })
    }

    /// `OneRatio`: how many of the bits are set.
    fn one_ratio(&self) -> f64 {
        let ones: u32 = self.data.iter().map(|b| b.count_ones()).sum();
        f64::from(ones) / (self.data.len() as f64 * 8.0)
    }

    /// `FoldToTarget`: halves the filter, ORing its blocks together, while it stays below the false positive
    /// rate `target`.
    pub fn fold_to_target(&mut self, target: f64) {
        let blocks = self.data.len() / Self::BLOCK;
        let mut zero_ratio = 1.0 - self.one_ratio();
        let mut new_blocks = blocks;
        while new_blocks > 1 {
            zero_ratio *= zero_ratio;
            if (1.0 - zero_ratio).powi(8) > target {
                break;
            }
            new_blocks /= 2;
        }
        if new_blocks == blocks {
            return;
        }
        let factor = blocks / new_blocks;
        let mut folded = vec![0u8; new_blocks * Self::BLOCK];
        for (i, target_block) in folded.chunks_mut(Self::BLOCK).enumerate() {
            for source in self.data[i * factor * Self::BLOCK..(i + 1) * factor * Self::BLOCK].chunks(Self::BLOCK) {
                for (a, b) in target_block.iter_mut().zip(source) {
                    *a |= b;
                }
            }
        }
        self.data = folded;
    }

    /// `FilterCheck`: whether a value of this hash may be in the filter.
    pub fn contains(&self, hash: u64) -> bool {
        let blocks = (self.data.len() / Self::BLOCK) as u64;
        if blocks == 0 {
            return true;
        }
        let block = (((hash >> 32) * blocks) >> 32) as usize;
        let key = hash as u32;
        Self::SALT.iter().enumerate().all(|(word, salt)| {
            let bit = key.wrapping_mul(*salt) >> 27;
            let at = block * Self::BLOCK + word * 4;
            let value = u32::from_le_bytes(self.data[at..at + 4].try_into().expect("four bytes"));
            value & (1 << bit) != 0
        })
    }

    /// `FilterInsert` of a value's XXH64.
    pub fn insert(&mut self, hash: u64) {
        let blocks = (self.data.len() / Self::BLOCK) as u64;
        let block = (((hash >> 32) * blocks) >> 32) as usize;
        let key = hash as u32;
        for (word, salt) in Self::SALT.iter().enumerate() {
            let bit = key.wrapping_mul(*salt) >> 27;
            let at = block * Self::BLOCK + word * 4;
            let mut value = u32::from_le_bytes(self.data[at..at + 4].try_into().expect("four bytes"));
            value |= 1 << bit;
            self.data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// `XXH64` with seed 0, as Parquet's bloom filters hash the plain encoding of a value.
pub fn xxhash64(bytes: &[u8]) -> u64 {
    twox_hash::XxHash64::oneshot(0, bytes)
}

/// Thrift's compact protocol, as the generated `write` methods of `parquet_types.cpp` use it.
pub struct Thrift {
    pub out: Vec<u8>,
    last_field: Vec<i16>,
}

pub mod ttype {
    pub const BOOL_TRUE: u8 = 1;
    pub const BOOL_FALSE: u8 = 2;
    pub const I32: u8 = 5;
    pub const I64: u8 = 6;
    pub const BINARY: u8 = 8;
    pub const LIST: u8 = 9;
    pub const STRUCT: u8 = 12;
}

impl Default for Thrift {
    fn default() -> Self {
        Self::new()
    }
}

impl Thrift {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            last_field: vec![0],
        }
    }

    /// `writeStructBegin`.
    pub fn begin(&mut self) {
        self.last_field.push(0);
    }

    /// `writeFieldStop` and `writeStructEnd`.
    pub fn end(&mut self) {
        self.out.push(0);
        self.last_field.pop();
    }

    fn field(&mut self, id: i16, kind: u8) {
        let last = self.last_field.last_mut().expect("in a struct");
        let delta = id - *last;
        if delta > 0 && delta <= 15 {
            self.out.push(((delta as u8) << 4) | kind);
        } else {
            self.out.push(kind);
            varint(&mut self.out, zigzag(i64::from(id)));
        }
        *last = id;
    }

    pub fn i32(&mut self, id: i16, value: i32) {
        self.field(id, ttype::I32);
        varint(&mut self.out, zigzag(i64::from(value)));
    }

    pub fn i64(&mut self, id: i16, value: i64) {
        self.field(id, ttype::I64);
        varint(&mut self.out, zigzag(value));
    }

    /// A byte field (`i8`).
    pub fn i8(&mut self, id: i16, value: i8) {
        self.field(id, 3);
        self.out.push(value as u8);
    }

    pub fn bool(&mut self, id: i16, value: bool) {
        self.field(id, if value { ttype::BOOL_TRUE } else { ttype::BOOL_FALSE });
    }

    pub fn binary(&mut self, id: i16, value: &[u8]) {
        self.field(id, ttype::BINARY);
        self.raw_binary(value);
    }

    /// A struct field: `write` fills it in.
    pub fn structure(&mut self, id: i16, write: impl FnOnce(&mut Self)) {
        self.field(id, ttype::STRUCT);
        self.begin();
        write(self);
        self.end();
    }

    /// An empty struct (the members of unions such as `TimeUnit`).
    pub fn empty(&mut self, id: i16) {
        self.structure(id, |_| {});
    }

    pub fn list(&mut self, id: i16, element: u8, size: usize) {
        self.field(id, ttype::LIST);
        if size < 15 {
            self.out.push(((size as u8) << 4) | element);
        } else {
            self.out.push(0xf0 | element);
            varint(&mut self.out, size as u64);
        }
    }

    /// An `i32` element of a list.
    pub fn element_i32(&mut self, value: i32) {
        varint(&mut self.out, zigzag(i64::from(value)));
    }

    /// A binary element of a list, or the bytes of a binary field.
    pub fn raw_binary(&mut self, value: &[u8]) {
        varint(&mut self.out, value.len() as u64);
        self.out.extend_from_slice(value);
    }

    /// A struct element of a list.
    pub fn element_struct(&mut self, write: impl FnOnce(&mut Self)) {
        self.begin();
        write(self);
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_runs_and_bit_packing() {
        let mut out = Vec::new();
        let mut encoder = RleBpEncoder::new(1);
        encoder.begin_write();
        encoder.write_many(&mut out, 1, 10);
        encoder.finish_write(&mut out);
        assert_eq!(out, [20, 1]);
    }

    #[test]
    fn varints_and_zigzag() {
        let mut out = Vec::new();
        varint(&mut out, 300);
        assert_eq!(out, [0xac, 0x02]);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(bit_width_of_count(10), 4);
    }

    #[test]
    fn thrift_field_deltas() {
        let mut t = Thrift::new();
        t.begin();
        t.i32(1, 2);
        t.i64(3, -1);
        t.end();
        assert_eq!(t.out, [0x15, 4, 0x26, 1, 0]);
    }
}
