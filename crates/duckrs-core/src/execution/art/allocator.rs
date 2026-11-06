// Ported from duckdb/src/execution/index/fixed_size_allocator.cpp and fixed_size_buffer.cpp
// @ rust-port-baseline
//
//! `FixedSizeAllocator`: buffers the size of a block, each a bitmask followed by segments of one
//! size. A buffer goes into the file as it is, so the bitmask has to come out as in C++.

use super::pointer::{IndexPointer, NodeType};
use duckrs_common::types::PhysicalType;
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::{BTreeMap, BTreeSet};

const BITS_PER_VALUE: usize = u64::BITS as usize;
const VALIDITY_SIZE: usize = size_of::<u64>();

/// `sizeof` the nodes, which are structs in C++ with its padding.
pub const LEAF_SIZE: usize = 48;
pub const NODE_4_SIZE: usize = 40;
pub const NODE_16_SIZE: usize = 152;
pub const NODE_48_SIZE: usize = 648;
pub const NODE_256_SIZE: usize = 2056;
pub const NODE_7_LEAF_SIZE: usize = 8;
pub const NODE_15_LEAF_SIZE: usize = 16;
pub const NODE_256_LEAF_SIZE: usize = 40;
/// `LegacyNode256Leaf`: what files from before v2.0.0 have.
pub const LEGACY_NODE_256_LEAF_SIZE: usize = 264;
/// `Prefix::METADATA_SIZE`: a child pointer and a count after the bytes of a prefix.
pub const PREFIX_METADATA_SIZE: usize = 9;

/// `ART::SetPrefixCount`: what a prefix can hold, so that its segments align. `types` are those of
/// the columns of a key.
pub fn prefix_count(types: &[PhysicalType]) -> usize {
    /// `AlignValueFloor(255 - METADATA_SIZE) - 1`.
    const MAX_ALIGNED: usize = 239;
    let compound: usize = types.iter().copied().map(key_type_size).sum();
    (compound.next_multiple_of(8) - 1).min(MAX_ALIGNED)
}

/// `GetTypeIdSize`: of what can be in a key.
fn key_type_size(ty: PhysicalType) -> usize {
    match ty {
        PhysicalType::Bool | PhysicalType::Int8 | PhysicalType::UInt8 => 1,
        PhysicalType::Int16 | PhysicalType::UInt16 => 2,
        PhysicalType::Int32 | PhysicalType::UInt32 | PhysicalType::Float => 4,
        PhysicalType::Int64 | PhysicalType::UInt64 | PhysicalType::Double => 8,
        // `sizeof(string_t)`, as the bytes of a string are not what this is about
        PhysicalType::Int128 | PhysicalType::UInt128 | PhysicalType::Varchar => 16,
        other => unreachable!("{other:?} can't be in an index key: the binder refuses it"),
    }
}

/// What a segment of a node takes, by the index of its allocator.
pub fn segment_sizes(prefix_count: usize) -> [usize; 9] {
    [
        prefix_count + PREFIX_METADATA_SIZE,
        LEAF_SIZE,
        NODE_4_SIZE,
        NODE_16_SIZE,
        NODE_48_SIZE,
        NODE_256_SIZE,
        NODE_7_LEAF_SIZE,
        NODE_15_LEAF_SIZE,
        NODE_256_LEAF_SIZE,
    ]
}

/// `FixedSizeBuffer`.
#[derive(Debug, Clone)]
struct Buffer {
    data: Vec<u8>,
    segment_count: usize,
}

impl Buffer {
    fn entry(&self, index: usize) -> u64 {
        let at = index * VALIDITY_SIZE;
        u64::from_le_bytes(self.data[at..at + VALIDITY_SIZE].try_into().expect("8 bytes"))
    }

    fn set_entry(&mut self, index: usize, value: u64) {
        let at = index * VALIDITY_SIZE;
        self.data[at..at + VALIDITY_SIZE].copy_from_slice(&value.to_le_bytes());
    }

    /// A set bit is a free segment.
    fn is_free(&self, segment: usize) -> bool {
        self.entry(segment / BITS_PER_VALUE) >> (segment % BITS_PER_VALUE) & 1 != 0
    }

    fn set_free(&mut self, segment: usize, free: bool) {
        let index = segment / BITS_PER_VALUE;
        let bit = 1 << (segment % BITS_PER_VALUE);
        let entry = self.entry(index);
        self.set_entry(index, if free { entry | bit } else { entry & !bit });
    }

    /// `FixedSizeBuffer::AllocateSegment`: fills up a buffer in order before looking for gaps.
    fn allocate(&mut self, bitmask_count: usize) -> usize {
        let segment = if self.is_free(self.segment_count) {
            self.segment_count
        } else {
            let entry = (0..bitmask_count)
                .find(|&index| self.entry(index) != 0)
                .expect("the allocator only picks a buffer with room");
            entry * BITS_PER_VALUE + self.entry(entry).trailing_zeros() as usize
        };
        self.set_free(segment, false);
        self.segment_count += 1;
        segment
    }
}

/// A buffer as a file has it.
#[derive(Debug)]
pub struct StoredBuffer<'a> {
    pub id: u64,
    pub data: &'a [u8],
    pub segment_count: usize,
    pub has_free_space: bool,
}

/// `FixedSizeAllocator`.
#[derive(Debug, Clone)]
pub struct FixedSizeAllocator {
    segment_size: usize,
    block_size: usize,
    bitmask_count: usize,
    available_segments_per_buffer: usize,
    /// Where the segments of a buffer start.
    bitmask_offset: usize,
    total_segment_count: usize,
    buffers: BTreeMap<u64, Buffer>,
    buffers_with_free_space: BTreeSet<u64>,
    /// The one to fill next.
    buffer_with_free_space: Option<u64>,
}

impl FixedSizeAllocator {
    /// `block_size` is what a block holds, without its checksum.
    pub fn new(segment_size: usize, block_size: usize) -> Self {
        assert!(
            segment_size <= block_size - VALIDITY_SIZE,
            "The maximum segment size of fixed-size allocators is {}",
            block_size - VALIDITY_SIZE
        );
        // each full mask is followed by as many segments as it has bits: all of those at once
        let full_value_bytes = VALIDITY_SIZE + BITS_PER_VALUE * segment_size;
        let full_value_count = block_size / full_value_bytes;
        let mut byte_count = full_value_count * full_value_bytes;
        let mut bitmask_count = full_value_count;
        let mut available = full_value_count * BITS_PER_VALUE;
        while byte_count < block_size {
            if bitmask_count == 0 || (bitmask_count * BITS_PER_VALUE).is_multiple_of(available) {
                // room for another mask, for what is left to be segments
                bitmask_count += 1;
                byte_count += VALIDITY_SIZE;
            }
            let remaining = ((block_size - byte_count) / segment_size).min(BITS_PER_VALUE);
            if remaining == 0 {
                break;
            }
            available += remaining;
            byte_count += remaining * segment_size;
        }
        Self {
            segment_size,
            block_size,
            bitmask_count,
            available_segments_per_buffer: available,
            bitmask_offset: bitmask_count * VALIDITY_SIZE,
            total_segment_count: 0,
            buffers: BTreeMap::new(),
            buffers_with_free_space: BTreeSet::new(),
            buffer_with_free_space: None,
        }
    }

    /// An allocator per type of node, in the order of `NodeType::allocator_index`.
    pub fn for_nodes(prefix_count: usize, block_size: usize) -> Vec<Self> {
        let sizes = segment_sizes(prefix_count);
        sizes.into_iter().map(|size| Self::new(size, block_size)).collect()
    }

    pub fn segment_size(&self) -> usize {
        self.segment_size
    }

    pub fn available_segments_per_buffer(&self) -> usize {
        self.available_segments_per_buffer
    }

    pub fn bitmask_offset(&self) -> usize {
        self.bitmask_offset
    }

    pub fn segment_count(&self) -> usize {
        self.total_segment_count
    }

    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// `GetAvailableBufferId`: ids of freed buffers come back, from the top.
    fn available_buffer_id(&self) -> u64 {
        let mut id = self.buffers.len() as u64;
        while self.buffers.contains_key(&id) {
            id -= 1;
        }
        id
    }

    /// `FixedSizeAllocator::New`.
    pub fn allocate(&mut self) -> IndexPointer {
        let id = self.buffer_with_free_space.unwrap_or_else(|| {
            let id = self.available_buffer_id();
            let mut buffer = Buffer {
                data: vec![0; self.block_size],
                segment_count: 0,
            };
            // `ValidityMask::SetAllValid`: the bits past the last segment stay clear
            let available = self.available_segments_per_buffer;
            let last = available.div_ceil(BITS_PER_VALUE) - 1;
            for index in 0..last {
                buffer.set_entry(index, u64::MAX);
            }
            let bits = available % BITS_PER_VALUE;
            buffer.set_entry(last, if bits == 0 { u64::MAX } else { !(u64::MAX << bits) });
            self.buffers.insert(id, buffer);
            self.buffers_with_free_space.insert(id);
            self.buffer_with_free_space = Some(id);
            id
        });
        let buffer = self.buffers.get_mut(&id).expect("it was just found");
        let segment = buffer.allocate(self.bitmask_count);
        let full = buffer.segment_count == self.available_segments_per_buffer;
        self.total_segment_count += 1;
        if full {
            self.buffers_with_free_space.remove(&id);
            self.next_buffer_with_free_space();
        }
        IndexPointer::new(id as u32, segment as u32)
    }

    fn next_buffer_with_free_space(&mut self) {
        self.buffer_with_free_space = self.buffers_with_free_space.first().copied();
    }

    fn invalid_pointer(pointer: IndexPointer) -> Error {
        Error::new(
            ErrorKind::DataCorruption,
            format!(
                "Index pointer to buffer {}, segment {} does not point to an allocated segment",
                pointer.buffer_id(),
                pointer.offset()
            ),
        )
    }

    /// `FixedSizeAllocator::Free`.
    pub fn free(&mut self, pointer: IndexPointer) -> Result<()> {
        let (id, segment) = (pointer.buffer_id(), pointer.offset() as usize);
        if segment >= self.available_segments_per_buffer {
            return Err(Self::invalid_pointer(pointer));
        }
        let buffer = self
            .buffers
            .get_mut(&id)
            .ok_or_else(|| Self::invalid_pointer(pointer))?;
        if buffer.is_free(segment) {
            return Err(Self::invalid_pointer(pointer));
        }
        buffer.set_free(segment, true);
        buffer.segment_count -= 1;
        let empty = buffer.segment_count == 0;
        self.total_segment_count -= 1;
        if !empty {
            self.buffers_with_free_space.insert(id);
            self.buffer_with_free_space.get_or_insert(id);
            return Ok(());
        }
        // an empty buffer stays if it is the only one with room, so that one row going in and out
        // doesn't keep making and dropping a buffer
        self.buffers_with_free_space.insert(id);
        if self.buffers_with_free_space.len() == 1 {
            self.buffer_with_free_space = Some(id);
            return Ok(());
        }
        self.buffers_with_free_space.remove(&id);
        self.buffers.remove(&id);
        if self.buffer_with_free_space == Some(id) {
            self.next_buffer_with_free_space();
        }
        Ok(())
    }

    fn range(&self, pointer: IndexPointer) -> Result<std::ops::Range<usize>> {
        let segment = pointer.offset() as usize;
        if segment >= self.available_segments_per_buffer {
            return Err(Self::invalid_pointer(pointer));
        }
        let start = self.bitmask_offset + segment * self.segment_size;
        Ok(start..start + self.segment_size)
    }

    /// The bytes of a segment.
    pub fn get(&self, pointer: IndexPointer) -> Result<&[u8]> {
        let range = self.range(pointer)?;
        let buffer = self.buffers.get(&pointer.buffer_id());
        Ok(&buffer.ok_or_else(|| Self::invalid_pointer(pointer))?.data[range])
    }

    pub fn get_mut(&mut self, pointer: IndexPointer) -> Result<&mut [u8]> {
        let range = self.range(pointer)?;
        let buffer = self.buffers.get_mut(&pointer.buffer_id());
        Ok(&mut buffer.ok_or_else(|| Self::invalid_pointer(pointer))?.data[range])
    }

    /// `FixedSizeAllocator::Init` for one buffer: `stored` is what a file has for it.
    pub fn load_buffer(&mut self, id: u64, stored: &[u8], segment_count: usize) {
        let mut data = vec![0; self.block_size];
        let length = stored.len().min(self.block_size);
        data[..length].copy_from_slice(&stored[..length]);
        self.buffers.insert(id, Buffer { data, segment_count });
        self.total_segment_count += segment_count;
        if segment_count < self.available_segments_per_buffer {
            self.buffers_with_free_space.insert(id);
            self.buffer_with_free_space.get_or_insert(id);
        }
    }

    /// `Persist` and `GetInfo`: the buffers that hold something, each cut off after its last segment.
    pub fn stored_buffers(&self) -> Vec<StoredBuffer<'_>> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.segment_count > 0)
            .map(|(&id, buffer)| {
                let last = (0..self.available_segments_per_buffer)
                    .rev()
                    .find(|&segment| !buffer.is_free(segment))
                    .expect("a buffer with segments has one that is taken");
                StoredBuffer {
                    id,
                    data: &buffer.data[..(last + 1) * self.segment_size + self.bitmask_offset],
                    segment_count: buffer.segment_count,
                    has_free_space: self.buffers_with_free_space.contains(&id),
                }
            })
            .collect()
    }

    /// How many segments in a stored buffer of this allocator are taken, as its bitmask says.
    pub fn taken_segments(&self, buffer: &[u8]) -> usize {
        let free: u32 = buffer[..self.bitmask_offset]
            .chunks_exact(VALIDITY_SIZE)
            .map(|entry| u64::from_le_bytes(entry.try_into().expect("8 bytes")).count_ones())
            .sum();
        self.available_segments_per_buffer - free as usize
    }
}

/// The allocator a node of a type comes from.
pub fn allocator_of(allocators: &mut [FixedSizeAllocator], ty: NodeType) -> Option<&mut FixedSizeAllocator> {
    allocators.get_mut(ty.allocator_index()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// `DEFAULT_BLOCK_ALLOC_SIZE` less the checksum.
    const BLOCK_SIZE: usize = 262_136;

    #[test]
    fn segments_and_masks_fill_a_buffer() {
        for segment_size in [8, 16, 40, 152, 648, 2056] {
            let allocator = FixedSizeAllocator::new(segment_size, BLOCK_SIZE);
            let used = allocator.bitmask_offset() + allocator.available_segments_per_buffer() * segment_size;
            assert!(used <= BLOCK_SIZE, "{segment_size}");
            assert!(BLOCK_SIZE - used < segment_size + VALIDITY_SIZE, "{segment_size}");
            let bits = allocator.bitmask_offset() / VALIDITY_SIZE * BITS_PER_VALUE;
            assert!(allocator.available_segments_per_buffer() <= bits, "{segment_size}");
        }
    }

    #[test]
    fn prefixes_are_sized_by_their_key() {
        assert_eq!(prefix_count(&[PhysicalType::Int32]), 7);
        assert_eq!(prefix_count(&[PhysicalType::Int64]), 7);
        assert_eq!(prefix_count(&[PhysicalType::Int64, PhysicalType::Int32]), 15);
        assert_eq!(prefix_count(&[PhysicalType::Varchar]), 15);
        assert_eq!(prefix_count(&[PhysicalType::Varchar; 40]), 239);
    }

    #[test]
    fn segments_are_handed_out_in_order_and_come_back() {
        let mut allocator = FixedSizeAllocator::new(40, BLOCK_SIZE);
        let first = allocator.allocate();
        let second = allocator.allocate();
        assert_eq!((first.buffer_id(), first.offset()), (0, 0));
        assert_eq!((second.buffer_id(), second.offset()), (0, 1));
        allocator.get_mut(second).unwrap().fill(7);
        assert_eq!(allocator.get(second).unwrap(), [7; 40]);
        assert_eq!(allocator.get(first).unwrap(), [0; 40]);
        allocator.free(first).unwrap();
        assert_eq!(allocator.segment_count(), 1);
        // as many are held as there are before the next in order, which is taken: so the mask decides
        let again = allocator.allocate();
        assert_eq!(again.offset(), 0);
        assert_eq!(allocator.allocate().offset(), 2);
        allocator.free(again).unwrap();
        assert!(allocator.free(again).is_err(), "it was freed already");
    }

    #[test]
    fn a_full_buffer_makes_way_for_another() {
        let mut allocator = FixedSizeAllocator::new(2056, BLOCK_SIZE);
        let available = allocator.available_segments_per_buffer();
        let pointers: Vec<_> = (0..=available).map(|_| allocator.allocate()).collect();
        assert_eq!(pointers[available].buffer_id(), 1);
        assert_eq!(allocator.buffer_count(), 2);
        // the buffer being filled goes on, and the gap in the first one waits
        allocator.free(pointers[3]).unwrap();
        let next = allocator.allocate();
        assert_eq!((next.buffer_id(), next.offset()), (1, 1));
    }

    #[test]
    fn an_empty_buffer_goes_unless_it_is_the_last_with_room() {
        let mut allocator = FixedSizeAllocator::new(2056, BLOCK_SIZE);
        let available = allocator.available_segments_per_buffer();
        let pointers: Vec<_> = (0..available * 2).map(|_| allocator.allocate()).collect();
        for pointer in &pointers[available..] {
            allocator.free(*pointer).unwrap();
        }
        assert_eq!(allocator.buffer_count(), 2, "the only one with room stays");
        allocator.free(pointers[0]).unwrap();
        for pointer in &pointers[1..available] {
            allocator.free(*pointer).unwrap();
        }
        assert_eq!(allocator.buffer_count(), 1);
        assert_eq!(allocator.segment_count(), 0);
    }

    #[test]
    fn pointers_that_go_nowhere_are_errors() {
        let mut allocator = FixedSizeAllocator::new(40, BLOCK_SIZE);
        allocator.allocate();
        assert!(allocator.get(IndexPointer::new(9, 0)).is_err());
        assert!(allocator.get(IndexPointer::new(0, 0xFF_FFFF)).is_err());
        assert!(allocator.free(IndexPointer::new(0, 0xFF_FFFF)).is_err());
    }

    proptest! {
        /// Whatever goes in and out, what's held stays apart and the counts add up.
        #[test]
        fn segments_never_overlap(steps in proptest::collection::vec(any::<(bool, u8)>(), 1..600)) {
            let mut allocator = FixedSizeAllocator::new(2056, BLOCK_SIZE);
            let mut held: Vec<(IndexPointer, u8)> = Vec::new();
            for (take, value) in steps {
                if take || held.is_empty() {
                    let pointer = allocator.allocate();
                    allocator.get_mut(pointer).unwrap().fill(value);
                    held.push((pointer, value));
                } else {
                    let (pointer, _) = held.swap_remove(usize::from(value) % held.len());
                    allocator.free(pointer).unwrap();
                }
                prop_assert_eq!(allocator.segment_count(), held.len());
            }
            for (pointer, value) in held {
                prop_assert!(allocator.get(pointer).unwrap().iter().all(|byte| *byte == value));
            }
        }
    }
}
