//! Ported from duckdb/src/storage/metadata/metadata_reader.cpp and metadata_manager.cpp
//! @ rust-port-baseline.
//!
//! Metadata lives in chains of sub-blocks: 64 to a block, each starting with a pointer to the next.
//! C++ reads such a chain as a stream. `BinaryDeserializer` reads from a slice, so here a chain is
//! read into memory first, without those pointers, and a position in it is found by the sub-block.

use super::block_manager::{INVALID_BLOCK, corrupt};
use super::buffer_manager::BufferManager;
use duckrs_common::Result;
use duckrs_common::serializer::{BinaryDeserializer, BinarySerializer, Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

/// `METADATA_BLOCK_COUNT`.
const METADATA_BLOCK_COUNT: u64 = 64;
/// `sizeof(block_id_t)`: the pointer in front of a sub-block.
const NEXT_POINTER_SIZE: usize = 8;

/// `MetaBlockPointer`: the block in the low 56 bits, the sub-block in the high 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetaBlockPointer {
    pub block_pointer: u64,
    pub offset: u32,
}

impl MetaBlockPointer {
    pub fn new(block_pointer: u64, offset: u32) -> Self {
        Self { block_pointer, offset }
    }

    pub fn is_valid(self) -> bool {
        self.block_pointer != INVALID_BLOCK
    }

    pub fn block_id(self) -> u64 {
        self.block_pointer & !(0xFF << 56)
    }

    pub fn block_index(self) -> u64 {
        self.block_pointer >> 56
    }
}

impl Deserialize for MetaBlockPointer {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            Ok(Self {
                block_pointer: d.read_property_with_default(100, "block_pointer")?,
                offset: d.read_property_with_default(101, "offset")?,
            })
        })
    }
}

impl Serialize for MetaBlockPointer {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| {
            s.write_property_with_default(100, "block_pointer", &self.block_pointer);
            s.write_property_with_default(101, "offset", &self.offset);
        });
    }
}

/// `GetMetadataBlockSize`.
pub fn metadata_block_size(block_size: usize) -> usize {
    (block_size / METADATA_BLOCK_COUNT as usize) & !7
}

/// The chains read so far of one reader. The metadata of a table's columns is one long chain with
/// a pointer to each column, so reading each from its pointer on would take quadratic time.
#[derive(Debug, Default)]
pub struct MetadataChains {
    data: Vec<u8>,
    /// where in `data` the payload of a sub-block starts
    starts: HashMap<u64, usize>,
}

impl MetadataChains {
    /// The chain from `pointer` on, and with it what follows in memory.
    pub fn at(&mut self, buffer: &BufferManager, pointer: MetaBlockPointer) -> Result<&[u8]> {
        if !self.starts.contains_key(&pointer.block_pointer) {
            self.read_chain(buffer, pointer.block_pointer)?;
        }
        let payload = metadata_block_size(buffer.block_manager().block_size()) - NEXT_POINTER_SIZE;
        let offset = (pointer.offset as usize).max(NEXT_POINTER_SIZE) - NEXT_POINTER_SIZE;
        if offset > payload {
            return Err(corrupt(format!("metadata offset {} is out of bounds", pointer.offset)));
        }
        Ok(&self.data[self.starts[&pointer.block_pointer] + offset..])
    }

    fn read_chain(&mut self, buffer: &BufferManager, first: u64) -> Result<()> {
        let size = metadata_block_size(buffer.block_manager().block_size());
        let mut next = first;
        let mut seen = HashSet::new();
        while next != INVALID_BLOCK {
            if !seen.insert(next) {
                return Err(corrupt(format!("the metadata chain runs into sub-block {next} again")));
            }
            let pointer = MetaBlockPointer::new(next, 0);
            if pointer.block_index() >= METADATA_BLOCK_COUNT {
                return Err(corrupt(format!(
                    "metadata block index {} exceeds metadata block count {METADATA_BLOCK_COUNT}",
                    pointer.block_index()
                )));
            }
            let block = buffer.pin(pointer.block_id())?;
            let start = pointer.block_index() as usize * size;
            let sub_block = &block[start..start + size];
            self.starts.insert(next, self.data.len());
            self.data.extend_from_slice(&sub_block[NEXT_POINTER_SIZE..]);
            next = u64::from_le_bytes(sub_block[..NEXT_POINTER_SIZE].try_into().expect("8 bytes"));
        }
        Ok(())
    }
}

pub(super) fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N]> {
    let (head, tail) = data
        .split_first_chunk::<N>()
        .ok_or_else(|| corrupt("metadata ends early"))?;
    *data = tail;
    Ok(*head)
}

/// `LoadFreeList`, without the free sub-blocks at its end, which only writing needs.
pub fn load_free_list(buffer: &BufferManager) -> Result<()> {
    let blocks = buffer.block_manager();
    let pointer = MetaBlockPointer::new(blocks.header().free_list, 0);
    if !pointer.is_valid() {
        return Ok(());
    }
    let mut chains = MetadataChains::default();
    let mut data = chains.at(buffer, pointer)?;
    let count = u64::from_le_bytes(take(&mut data)?);
    let mut free_list = BTreeSet::new();
    for _ in 0..count {
        free_list.insert(u64::from_le_bytes(take(&mut data)?));
    }
    let count = u64::from_le_bytes(take(&mut data)?);
    let mut multi_use_blocks = HashMap::new();
    for _ in 0..count {
        let block = u64::from_le_bytes(take(&mut data)?);
        multi_use_blocks.insert(block, u32::from_le_bytes(take(&mut data)?));
    }
    blocks.set_free_list(free_list, multi_use_blocks);
    Ok(())
}
