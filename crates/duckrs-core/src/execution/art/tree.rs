// Ported from duckdb/src/execution/index/art/{node,base_node,base_leaf,node48,node256,node256_leaf,prefix,
// prefix_handle,leaf}.cpp and duckdb/src/include/duckdb/execution/index/art/art_operator.hpp
// @ rust-port-baseline
//
//! The tree itself. C++ passes references to the pointers inside nodes around; here a [`Slot`] says
//! where such a pointer is, as the nodes are bytes in the buffers of the allocators.
//!
//! A key holds one row, in a leaf that is in the pointer to it. Rows that share a key go below a
//! gate: a nested tree, keyed by row id, whose last byte is in a leaf node that only has bytes.

use super::allocator::{FixedSizeAllocator, PREFIX_METADATA_SIZE};
use super::key::ArtKey;
use super::pointer::{IndexPointer, NodePtr, NodeType};
use duckrs_common::{Error, ErrorKind, Result};

const NODE_4_CAPACITY: u8 = 4;
const NODE_16_CAPACITY: u8 = 16;
const NODE_48_CAPACITY: u8 = 48;
const NODE_7_LEAF_CAPACITY: u8 = 7;
const NODE_15_LEAF_CAPACITY: u8 = 15;
/// `Node48::EMPTY_MARKER`.
const EMPTY_MARKER: u8 = 48;
/// `Node48::SHRINK_THRESHOLD`.
const NODE_48_SHRINK: u16 = 12;
/// `Node256::SHRINK_THRESHOLD`.
const NODE_256_SHRINK: u16 = 36;
/// `Prefix::ROW_ID_COUNT`: the bytes of a row id above the one in a leaf node.
const ROW_ID_COUNT: usize = 7;
/// Where a `Node16` has its children: `count` and `key[16]`, then padding.
const NODE_16_CHILDREN: usize = 24;
const NODE_48_CHILDREN: usize = 264;
/// Of `Node4`, `Node256` and `Node256Leaf`, whose first field is padded.
const ALIGNED_FIELD: usize = 8;

/// Where a node pointer is kept.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    /// Above the root, which nothing has to change.
    Nowhere,
    Root,
    /// `at` bytes into a node.
    In {
        node: NodePtr,
        at: usize,
    },
}

/// `ARTConflictType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    None,
    /// The key of a unique index is there already.
    Constraint,
}

/// `IndexAppendMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendMode {
    Default,
    /// Leaves what is there: `ON CONFLICT DO NOTHING`.
    IgnoreDuplicates,
    /// Adds a row next to the one that is there, in a unique index too.
    InsertDuplicates,
}

/// The bytes of a key, and whether it is part of the range that it ends.
pub type KeyBound<'a> = Option<(&'a [u8], bool)>;

fn corrupted(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::DataCorruption, message)
}

/// `ART`, without what binds it to a table.
#[derive(Debug, Clone)]
pub struct Art {
    allocators: Vec<FixedSizeAllocator>,
    root: NodePtr,
    prefix_count: usize,
    unique: bool,
}

impl Art {
    pub fn new(prefix_count: usize, block_size: usize, unique: bool) -> Self {
        Self::from_parts(
            FixedSizeAllocator::for_nodes(prefix_count, block_size),
            NodePtr::EMPTY,
            unique,
        )
    }

    /// A tree over allocators that hold its nodes, as a file has them.
    pub fn from_parts(allocators: Vec<FixedSizeAllocator>, root: NodePtr, unique: bool) -> Self {
        let prefix_count = allocators[0].segment_size() - PREFIX_METADATA_SIZE;
        Self {
            allocators,
            root,
            prefix_count,
            unique,
        }
    }

    /// Where the tree starts, as a file says it.
    pub fn root_bits(&self) -> u64 {
        self.root.0.bits()
    }

    pub fn allocators(&self) -> &[FixedSizeAllocator] {
        &self.allocators
    }

    pub fn is_empty(&self) -> bool {
        !self.root.is_set()
    }

    /// How many segments are in use, over all types of node.
    pub fn segment_count(&self) -> usize {
        self.allocators.iter().map(FixedSizeAllocator::segment_count).sum()
    }

    fn node_type(node: NodePtr) -> Result<NodeType> {
        match node.node_type() {
            Some(NodeType::Leaf) => Err(crate::unported("leaves of an index from before v1.2.0")),
            Some(ty) => Ok(ty),
            None => Err(corrupted("an ART has a node of no known type")),
        }
    }

    fn is_prefix(node: NodePtr) -> bool {
        node.node_type() == Some(NodeType::Prefix)
    }

    fn allocator(&self, node: NodePtr) -> Result<&FixedSizeAllocator> {
        let index = Self::node_type(node)?.allocator_index();
        let index = index.ok_or_else(|| corrupted("an inlined leaf has no node"))?;
        Ok(&self.allocators[index])
    }

    fn bytes(&self, node: NodePtr) -> Result<&[u8]> {
        self.allocator(node)?.get(node.0)
    }

    fn bytes_mut(&mut self, node: NodePtr) -> Result<&mut [u8]> {
        let index = Self::node_type(node)?.allocator_index();
        let index = index.ok_or_else(|| corrupted("an inlined leaf has no node"))?;
        self.allocators[index].get_mut(node.0)
    }

    fn read(&self, slot: Slot) -> Result<NodePtr> {
        Ok(match slot {
            Slot::Nowhere => NodePtr::EMPTY,
            Slot::Root => self.root,
            Slot::In { node, at } => {
                let bytes = self.bytes(node)?;
                let word = bytes[at..at + 8].try_into().expect("8 bytes");
                NodePtr(IndexPointer::from_bits(u64::from_le_bytes(word)))
            }
        })
    }

    fn write(&mut self, slot: Slot, value: NodePtr) -> Result<()> {
        match slot {
            Slot::Nowhere => return Err(Error::new(ErrorKind::Internal, "writing above the root of an ART")),
            Slot::Root => self.root = value,
            Slot::In { node, at } => {
                let bytes = self.bytes_mut(node)?;
                bytes[at..at + 8].copy_from_slice(&value.0.bits().to_le_bytes());
            }
        }
        Ok(())
    }

    // Nodes

    /// `NodePtr::New`, and `Prefix` has its own.
    fn new_node(&mut self, ty: NodeType) -> Result<NodePtr> {
        let index = ty.allocator_index().expect("only nodes with a segment");
        let node = NodePtr::new(self.allocators[index].allocate(), ty);
        let bytes = self.bytes_mut(node)?;
        // segments come back with what they held
        bytes.fill(0);
        if ty == NodeType::Node48 {
            bytes[1..257].fill(EMPTY_MARKER);
        }
        Ok(node)
    }

    /// `NodePtr::FreeNode`.
    fn free_node(&mut self, node: NodePtr) -> Result<()> {
        let index = Self::node_type(node)?.allocator_index();
        let index = index.ok_or_else(|| corrupted("an inlined leaf has no node"))?;
        self.allocators[index].free(node.0)
    }

    /// `NodePtr::FreeTree`.
    fn free_tree(&mut self, node: NodePtr) -> Result<()> {
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            if !node.is_set() {
                continue;
            }
            match Self::node_type(node)? {
                NodeType::LeafInlined => continue,
                NodeType::Prefix => stack.push(self.prefix_child(node)?),
                NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
                    stack.extend(self.children(node)?.into_iter().map(|(_, child)| child));
                }
                _ => {}
            }
            self.free_node(node)?;
        }
        Ok(())
    }

    /// The bytes and children of a node, in order.
    fn children(&self, node: NodePtr) -> Result<Vec<(u8, NodePtr)>> {
        let mut children = Vec::new();
        for byte in self.bytes_of(node)? {
            let slot = self.child_slot(node, byte)?;
            children.push((
                byte,
                self.read(slot.ok_or_else(|| corrupted("a byte of an ART has no child"))?)?,
            ));
        }
        Ok(children)
    }

    /// The bytes that lead out of a node, in order.
    fn bytes_of(&self, node: NodePtr) -> Result<Vec<u8>> {
        let bytes = self.bytes(node)?;
        Ok(match Self::node_type(node)? {
            NodeType::Node4 | NodeType::Node16 | NodeType::Node7Leaf | NodeType::Node15Leaf => {
                let count = usize::from(bytes[0]);
                bytes
                    .get(1..=count)
                    .ok_or_else(|| corrupted("an ART node has more children than room"))?
                    .to_vec()
            }
            NodeType::Node48 => (0..=255u8)
                .filter(|&b| bytes[1 + usize::from(b)] != EMPTY_MARKER)
                .collect(),
            NodeType::Node256 => {
                let mut present = Vec::new();
                for byte in 0..=255u8 {
                    let at = ALIGNED_FIELD + usize::from(byte) * 8;
                    if bytes[at + 7] != 0 {
                        present.push(byte);
                    }
                }
                present
            }
            NodeType::Node256Leaf => (0..=255u8).filter(|&b| leaf_mask_has(bytes, b)).collect(),
            _ => return Err(corrupted("an ART node of this type has no children")),
        })
    }

    /// `NodePtr::GetChildMutable`.
    fn child_slot(&self, node: NodePtr, byte: u8) -> Result<Option<Slot>> {
        let bytes = self.bytes(node)?;
        let at = match Self::node_type(node)? {
            NodeType::Node4 | NodeType::Node16 => {
                let children = if Self::node_type(node)? == NodeType::Node4 {
                    ALIGNED_FIELD
                } else {
                    NODE_16_CHILDREN
                };
                let count = usize::from(bytes[0]);
                let keys = bytes
                    .get(1..=count)
                    .ok_or_else(|| corrupted("an ART node has more children than room"))?;
                keys.iter()
                    .position(|&key| key == byte)
                    .map(|index| children + index * 8)
            }
            NodeType::Node48 => {
                let index = bytes[1 + usize::from(byte)];
                if index >= NODE_48_CAPACITY && index != EMPTY_MARKER {
                    return Err(corrupted("a Node48 has an index past its children"));
                }
                (index != EMPTY_MARKER).then(|| NODE_48_CHILDREN + usize::from(index) * 8)
            }
            NodeType::Node256 => {
                let at = ALIGNED_FIELD + usize::from(byte) * 8;
                (bytes[at + 7] != 0).then_some(at)
            }
            _ => return Err(corrupted("an ART node of this type has no children")),
        };
        Ok(at.map(|at| Slot::In { node, at }))
    }

    fn has_byte(&self, node: NodePtr, byte: u8) -> Result<bool> {
        let bytes = self.bytes(node)?;
        Ok(match Self::node_type(node)? {
            NodeType::Node256Leaf => leaf_mask_has(bytes, byte),
            _ => bytes[1..=usize::from(bytes[0])].contains(&byte),
        })
    }

    /// `InsertChildInternal`: keys stay in order.
    fn insert_sorted(&mut self, node: NodePtr, byte: u8, child: NodePtr, children: usize) -> Result<()> {
        let bytes = self.bytes_mut(node)?;
        let count = usize::from(bytes[0]);
        let position = bytes[1..=count].iter().take_while(|&&key| key < byte).count();
        bytes.copy_within(1 + position..1 + count, 2 + position);
        bytes[1 + position] = byte;
        let at = children + position * 8;
        bytes.copy_within(at..children + count * 8, at + 8);
        bytes[at..at + 8].copy_from_slice(&child.0.bits().to_le_bytes());
        bytes[0] += 1;
        Ok(())
    }

    /// `DeleteChildInternal`, but for the child, which is freed here too.
    fn delete_sorted(&mut self, node: NodePtr, byte: u8, children: usize) -> Result<u8> {
        let position = {
            let bytes = self.bytes(node)?;
            let count = usize::from(bytes[0]);
            bytes[1..=count]
                .iter()
                .position(|&key| key == byte)
                .ok_or_else(|| corrupted("an ART node doesn't have the byte to delete"))?
        };
        let child = self.read(Slot::In {
            node,
            at: children + position * 8,
        })?;
        self.free_tree(child)?;
        let bytes = self.bytes_mut(node)?;
        let count = usize::from(bytes[0]);
        bytes.copy_within(2 + position..1 + count, 1 + position);
        bytes.copy_within(
            children + (position + 1) * 8..children + count * 8,
            children + position * 8,
        );
        bytes[0] -= 1;
        Ok(bytes[0])
    }

    /// Replaces a node with a new one of the same children, and the same gate.
    fn rebuild(&mut self, old: NodePtr, ty: NodeType) -> Result<NodePtr> {
        let children = self.children(old)?;
        let mut new = self.new_node(ty)?;
        new.set_gate(old.is_gate());
        for (byte, child) in children {
            self.place_child(new, byte, child)?;
        }
        self.free_node(old)?;
        Ok(new)
    }

    /// Adds to a node that has room, for what `rebuild` does.
    fn place_child(&mut self, node: NodePtr, byte: u8, child: NodePtr) -> Result<()> {
        match Self::node_type(node)? {
            NodeType::Node4 => self.insert_sorted(node, byte, child, ALIGNED_FIELD),
            NodeType::Node16 => self.insert_sorted(node, byte, child, NODE_16_CHILDREN),
            NodeType::Node48 => {
                let bytes = self.bytes_mut(node)?;
                let count = bytes[0];
                // what was deleted left a gap somewhere
                let mut position = usize::from(count);
                let taken = |bytes: &[u8], position: usize| {
                    let at = NODE_48_CHILDREN + position * 8;
                    bytes[at + 7] != 0
                };
                if taken(bytes, position) {
                    position = (0..usize::from(NODE_48_CAPACITY))
                        .find(|&position| !taken(bytes, position))
                        .ok_or_else(|| corrupted("a Node48 has no room for what its count says"))?;
                }
                bytes[1 + usize::from(byte)] = position as u8;
                bytes[0] += 1;
                let at = NODE_48_CHILDREN + position * 8;
                bytes[at..at + 8].copy_from_slice(&child.0.bits().to_le_bytes());
                Ok(())
            }
            NodeType::Node256 => {
                let bytes = self.bytes_mut(node)?;
                let count = u16::from_le_bytes([bytes[0], bytes[1]]) + 1;
                bytes[..2].copy_from_slice(&count.to_le_bytes());
                let at = ALIGNED_FIELD + usize::from(byte) * 8;
                bytes[at..at + 8].copy_from_slice(&child.0.bits().to_le_bytes());
                Ok(())
            }
            _ => Err(corrupted("an ART node of this type has no children")),
        }
    }

    /// `NodePtr::InsertChild`, for a node or a leaf node. What to point at the node comes back, as
    /// a full node grows into another.
    fn insert_child(&mut self, node: NodePtr, byte: u8, child: NodePtr) -> Result<NodePtr> {
        let ty = Self::node_type(node)?;
        let count = self.bytes(node)?[0];
        match ty {
            NodeType::Node4 if count == NODE_4_CAPACITY => {
                let grown = self.rebuild(node, NodeType::Node16)?;
                self.place_child(grown, byte, child)?;
                Ok(grown)
            }
            NodeType::Node16 if count == NODE_16_CAPACITY => {
                let grown = self.rebuild(node, NodeType::Node48)?;
                self.place_child(grown, byte, child)?;
                Ok(grown)
            }
            NodeType::Node48 if count == NODE_48_CAPACITY => {
                let grown = self.rebuild(node, NodeType::Node256)?;
                self.place_child(grown, byte, child)?;
                Ok(grown)
            }
            NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
                self.place_child(node, byte, child)?;
                Ok(node)
            }
            NodeType::Node7Leaf if count == NODE_7_LEAF_CAPACITY => {
                let grown = self.rebuild_leaf(node, NodeType::Node15Leaf)?;
                self.place_byte(grown, byte)?;
                Ok(grown)
            }
            NodeType::Node15Leaf if count == NODE_15_LEAF_CAPACITY => {
                let grown = self.rebuild_leaf(node, NodeType::Node256Leaf)?;
                self.place_byte(grown, byte)?;
                Ok(grown)
            }
            NodeType::Node7Leaf | NodeType::Node15Leaf | NodeType::Node256Leaf => {
                self.place_byte(node, byte)?;
                Ok(node)
            }
            _ => Err(corrupted("an ART node of this type has no children")),
        }
    }

    fn rebuild_leaf(&mut self, old: NodePtr, ty: NodeType) -> Result<NodePtr> {
        let bytes = self.bytes_of(old)?;
        let mut new = self.new_node(ty)?;
        new.set_gate(old.is_gate());
        for byte in bytes {
            self.place_byte(new, byte)?;
        }
        self.free_node(old)?;
        Ok(new)
    }

    /// `InsertByteInternal` and `Node256Leaf::InsertByte`.
    fn place_byte(&mut self, node: NodePtr, byte: u8) -> Result<()> {
        let ty = Self::node_type(node)?;
        let bytes = self.bytes_mut(node)?;
        if ty == NodeType::Node256Leaf {
            let count = u16::from_le_bytes([bytes[0], bytes[1]]) + 1;
            bytes[..2].copy_from_slice(&count.to_le_bytes());
            bytes[ALIGNED_FIELD + usize::from(byte / 8)] |= 1 << (byte % 8);
            return Ok(());
        }
        let count = usize::from(bytes[0]);
        let position = bytes[1..=count].iter().take_while(|&&key| key < byte).count();
        bytes.copy_within(1 + position..1 + count, 2 + position);
        bytes[1 + position] = byte;
        bytes[0] += 1;
        Ok(())
    }

    /// `NodePtr::DeleteChild`.
    fn delete_child(&mut self, slot: Slot, parent: Slot, byte: u8, in_gate: bool, row_id: i64) -> Result<()> {
        let node = self.read(slot)?;
        match Self::node_type(node)? {
            NodeType::Node4 => {
                if self.delete_sorted(node, byte, ALIGNED_FIELD)? != 1 {
                    return Ok(());
                }
                // one way down: the node goes, and what's left joins what is above it
                let (remaining, child) = self.children(node)?[0];
                self.free_node(node)?;
                self.write(slot, NodePtr::EMPTY)?;
                self.concat(parent, slot, child, remaining, node.is_gate(), in_gate)
            }
            NodeType::Node16 => {
                if self.delete_sorted(node, byte, NODE_16_CHILDREN)? < NODE_4_CAPACITY {
                    let shrunk = self.rebuild(node, NodeType::Node4)?;
                    self.write(slot, shrunk)?;
                }
                Ok(())
            }
            NodeType::Node48 => {
                let child = self.child_slot(node, byte)?;
                let child = child.ok_or_else(|| corrupted("an ART node doesn't have the byte to delete"))?;
                let held = self.read(child)?;
                self.free_tree(held)?;
                self.write(child, NodePtr::EMPTY)?;
                let bytes = self.bytes_mut(node)?;
                bytes[1 + usize::from(byte)] = EMPTY_MARKER;
                bytes[0] -= 1;
                if u16::from(bytes[0]) < NODE_48_SHRINK {
                    let shrunk = self.rebuild(node, NodeType::Node16)?;
                    self.write(slot, shrunk)?;
                }
                Ok(())
            }
            NodeType::Node256 => {
                let child = self.child_slot(node, byte)?;
                let child = child.ok_or_else(|| corrupted("an ART node doesn't have the byte to delete"))?;
                let held = self.read(child)?;
                self.free_tree(held)?;
                self.write(child, NodePtr::EMPTY)?;
                let bytes = self.bytes_mut(node)?;
                let count = u16::from_le_bytes([bytes[0], bytes[1]]) - 1;
                bytes[..2].copy_from_slice(&count.to_le_bytes());
                if count <= NODE_256_SHRINK {
                    let shrunk = self.rebuild(node, NodeType::Node48)?;
                    self.write(slot, shrunk)?;
                }
                Ok(())
            }
            NodeType::Node7Leaf => {
                if self.remove_byte(node, byte)? != 1 {
                    return Ok(());
                }
                // the one that's left goes back into a pointer
                let remaining = self.bytes(node)?[1];
                let row = (row_id as u64 & !0xFF) | u64::from(remaining);
                let leaf = NodePtr::inlined_leaf(row as i64);
                let above = self.read(parent)?;
                if Self::is_prefix(above) {
                    self.free_tree(above)?;
                    return self.write(parent, leaf);
                }
                self.free_node(node)?;
                self.write(slot, leaf)
            }
            NodeType::Node15Leaf => {
                if self.remove_byte(node, byte)? < NODE_7_LEAF_CAPACITY {
                    let shrunk = self.rebuild_leaf(node, NodeType::Node7Leaf)?;
                    self.write(slot, shrunk)?;
                }
                Ok(())
            }
            NodeType::Node256Leaf => {
                let bytes = self.bytes_mut(node)?;
                let count = u16::from_le_bytes([bytes[0], bytes[1]]) - 1;
                bytes[..2].copy_from_slice(&count.to_le_bytes());
                bytes[ALIGNED_FIELD + usize::from(byte / 8)] &= !(1 << (byte % 8));
                if count <= NODE_48_SHRINK {
                    let shrunk = self.rebuild_leaf(node, NodeType::Node15Leaf)?;
                    self.write(slot, shrunk)?;
                }
                Ok(())
            }
            _ => Err(corrupted("an ART node of this type has no children")),
        }
    }

    /// `DeleteByteInternal`.
    fn remove_byte(&mut self, node: NodePtr, byte: u8) -> Result<u8> {
        let bytes = self.bytes_mut(node)?;
        let count = usize::from(bytes[0]);
        let position = bytes[1..=count]
            .iter()
            .position(|&key| key == byte)
            .ok_or_else(|| corrupted("an ART leaf doesn't have the byte to delete"))?;
        bytes.copy_within(2 + position..1 + count, 1 + position);
        bytes[0] -= 1;
        Ok(bytes[0])
    }

    // Prefixes

    fn prefix_slot(&self, prefix: NodePtr) -> Slot {
        Slot::In {
            node: prefix,
            at: self.prefix_count + 1,
        }
    }

    fn prefix_child(&self, prefix: NodePtr) -> Result<NodePtr> {
        self.read(self.prefix_slot(prefix))
    }

    fn prefix_bytes(&self, prefix: NodePtr) -> Result<&[u8]> {
        let bytes = self.bytes(prefix)?;
        let count = usize::from(bytes[self.prefix_count]);
        bytes
            .get(..count)
            .filter(|_| count <= self.prefix_count)
            .ok_or_else(|| corrupted("an ART prefix has more bytes than room"))
    }

    /// `PrefixHandle::NewInternal`.
    fn new_prefix(&mut self, data: &[u8]) -> Result<NodePtr> {
        let node = NodePtr::new(self.allocators[0].allocate(), NodeType::Prefix);
        let prefix_count = self.prefix_count;
        let bytes = self.bytes_mut(node)?;
        bytes.fill(0);
        bytes[..data.len()].copy_from_slice(data);
        bytes[prefix_count] = data.len() as u8;
        Ok(node)
    }

    /// `PrefixHandle::New`: as many as it takes. The first and the last come back.
    fn new_prefix_chain(&mut self, data: &[u8]) -> Result<(NodePtr, NodePtr)> {
        let mut parts = data.chunks(self.prefix_count);
        let first = parts
            .next()
            .ok_or_else(|| Error::new(ErrorKind::Internal, "an empty ART prefix"))?;
        let root = self.new_prefix(first)?;
        let mut tail = root;
        for part in parts {
            let next = self.new_prefix(part)?;
            self.write(self.prefix_slot(tail), next)?;
            tail = next;
        }
        Ok((root, tail))
    }

    /// `PrefixHandle::AppendByte`: the prefix that has the byte comes back.
    fn append_byte(&mut self, prefix: NodePtr, byte: u8) -> Result<NodePtr> {
        let prefix_count = self.prefix_count;
        let bytes = self.bytes_mut(prefix)?;
        let count = usize::from(bytes[prefix_count]);
        if count != prefix_count {
            bytes[count] = byte;
            bytes[prefix_count] += 1;
            return Ok(prefix);
        }
        let next = self.new_prefix(&[byte])?;
        self.write(self.prefix_slot(prefix), next)?;
        Ok(next)
    }

    /// `PrefixHandle::Append`: what prefixes `other` starts with are merged into this one.
    fn append(&mut self, mut prefix: NodePtr, mut other: NodePtr) -> Result<()> {
        while Self::is_prefix(other) {
            if other.is_gate() {
                return self.write(self.prefix_slot(prefix), other);
            }
            let data = self.prefix_bytes(other)?.to_vec();
            for byte in data {
                prefix = self.append_byte(prefix, byte)?;
            }
            let next = self.prefix_child(other)?;
            self.write(self.prefix_slot(prefix), next)?;
            self.free_node(other)?;
            other = next;
        }
        self.write(self.prefix_slot(prefix), other)
    }

    /// `Prefix::GetTail`.
    fn prefix_tail(&self, mut prefix: NodePtr) -> Result<NodePtr> {
        loop {
            let child = self.prefix_child(prefix)?;
            if !Self::is_prefix(child) {
                return Ok(prefix);
            }
            prefix = child;
        }
    }

    /// `PrefixHandle::Split`: what comes after byte `position` comes back, for `branch` to hold.
    fn split(&mut self, slot: Slot, branch: &mut NodePtr, position: usize) -> Result<NodePtr> {
        let prefix = self.read(slot)?;
        let data = self.prefix_bytes(prefix)?.to_vec();
        let old_child = self.prefix_child(prefix)?;
        let child = if position + 1 < data.len() {
            let suffix = self.new_prefix(&data[position + 1..])?;
            self.append(suffix, old_child)?;
            suffix
        } else {
            old_child
        };
        if position != 0 {
            // a gate stays with this prefix
            let prefix_count = self.prefix_count;
            self.bytes_mut(prefix)?[prefix_count] = position as u8;
            self.write(self.prefix_slot(prefix), *branch)?;
            return Ok(child);
        }
        branch.set_gate(prefix.is_gate());
        self.free_node(prefix)?;
        self.write(slot, *branch)?;
        Ok(child)
    }

    /// `Prefix::Concat`: a `Node4` went, and left `child` under `byte`.
    fn concat(
        &mut self,
        parent: Slot,
        node4: Slot,
        child: NodePtr,
        byte: u8,
        node4_gate: bool,
        in_gate: bool,
    ) -> Result<()> {
        if node4_gate {
            return self.concat_node4_was_gate(node4, child, byte);
        }
        let above = self.read(parent)?;
        if child.is_gate() {
            if !Self::is_prefix(above) {
                let prefix = self.new_prefix(&[byte])?;
                self.write(self.prefix_slot(prefix), child)?;
                return self.write(node4, prefix);
            }
            let tail = self.prefix_tail(above)?;
            let tail = self.append_byte(tail, byte)?;
            return self.write(self.prefix_slot(tail), child);
        }
        let inlined = child.node_type() == Some(NodeType::LeafInlined);
        if inlined && in_gate {
            if Self::is_prefix(above) {
                // the tree below the gate is down to one row, so there is no gate any more
                let mut current = above;
                while Self::is_prefix(current) {
                    let next = self.prefix_child(current)?;
                    self.free_node(current)?;
                    current = next;
                }
                return self.write(parent, child);
            }
            // row ids below a gate aren't prefixed
            return self.write(node4, child);
        }
        let tail = if Self::is_prefix(above) {
            let tail = self.prefix_tail(above)?;
            self.append_byte(tail, byte)?
        } else {
            let prefix = self.new_prefix(&[byte])?;
            self.write(node4, prefix)?;
            prefix
        };
        if Self::is_prefix(child) {
            return self.append(tail, child);
        }
        self.write(self.prefix_slot(tail), child)
    }

    /// `Prefix::ConcatNode4WasGate`.
    fn concat_node4_was_gate(&mut self, node4: Slot, child: NodePtr, byte: u8) -> Result<()> {
        if child.node_type() == Some(NodeType::LeafInlined) {
            return self.write(node4, child);
        }
        let mut prefix = self.new_prefix(&[byte])?;
        if Self::is_prefix(child) {
            self.append(prefix, child)?;
        } else {
            self.write(self.prefix_slot(prefix), child)?;
        }
        prefix.set_gate(true);
        self.write(node4, prefix)
    }

    // What a tree is for

    /// `ARTOperator::Insert`. `key` must not be empty: NULLs aren't in an index.
    pub fn insert(&mut self, key: &ArtKey, row_id: i64, mode: AppendMode) -> Result<Conflict> {
        if key.is_empty() {
            return Err(Error::new(ErrorKind::Internal, "an empty key in an ART"));
        }
        let row_key = ArtKey::of(row_id);
        if self.is_empty() {
            let (root, tail) = self.new_prefix_chain(key.as_bytes())?;
            self.write(self.prefix_slot(tail), NodePtr::inlined_leaf(row_id))?;
            self.root = root;
            return Ok(Conflict::None);
        }
        let mut active = Slot::Root;
        let mut active_key = key.as_bytes();
        let mut depth = 0;
        let mut in_gate = false;
        loop {
            let node = self.read(active)?;
            if !node.is_set() {
                return Err(corrupted("an ART has a child pointer to nothing"));
            }
            if !in_gate && node.is_gate() {
                if self.unique && mode != AppendMode::InsertDuplicates {
                    // two rows of a key in a unique index: only a commit that replaces one does that
                    return Ok(Conflict::Constraint);
                }
                active_key = row_key.as_bytes();
                depth = 0;
                in_gate = true;
                continue;
            }
            match Self::node_type(node)? {
                NodeType::LeafInlined => {
                    if self.unique && mode == AppendMode::Default {
                        return Ok(Conflict::Constraint);
                    }
                    if self.unique && mode == AppendMode::IgnoreDuplicates {
                        return Ok(Conflict::None);
                    }
                    self.merge_inlined(active, row_id, in_gate, depth)?;
                    return Ok(Conflict::None);
                }
                NodeType::Node7Leaf | NodeType::Node15Leaf | NodeType::Node256Leaf => {
                    let grown = self.insert_child(node, active_key[ROW_ID_COUNT], NodePtr::EMPTY)?;
                    self.write(active, grown)?;
                    return Ok(Conflict::None);
                }
                NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
                    if let Some(child) = self.child_slot(node, active_key[depth])? {
                        active = child;
                        depth += 1;
                        continue;
                    }
                    let grown = self.insert_into_node(node, key.as_bytes(), row_id, depth, in_gate)?;
                    self.write(active, grown)?;
                    return Ok(Conflict::None);
                }
                NodeType::Prefix => {
                    let data = self.prefix_bytes(node)?;
                    let same = data
                        .iter()
                        .zip(&active_key[depth..])
                        .take_while(|(have, want)| have == want)
                        .count();
                    if same == data.len() {
                        depth += same;
                        active = self.prefix_slot(node);
                        continue;
                    }
                    let byte = data[same];
                    depth += same;
                    let mut branch = self.new_node(NodeType::Node4)?;
                    let child = self.split(active, &mut branch, same)?;
                    self.place_child(branch, byte, child)?;
                    self.insert_into_node(branch, active_key, row_id, depth, in_gate)?;
                    return Ok(Conflict::None);
                }
                NodeType::Leaf => unreachable!("`node_type` refuses these"),
            }
        }
    }

    /// `InsertIntoNode`: the node holds the rest of the key from here on.
    fn insert_into_node(
        &mut self,
        node: NodePtr,
        key: &[u8],
        row_id: i64,
        depth: usize,
        in_gate: bool,
    ) -> Result<NodePtr> {
        let leaf = NodePtr::inlined_leaf(row_id);
        if in_gate {
            // what's left of a row id is in the leaf
            let byte = ArtKey::of(row_id).as_bytes()[depth];
            return self.insert_child(node, byte, leaf);
        }
        let child = if depth + 1 < key.len() {
            let (root, tail) = self.new_prefix_chain(&key[depth + 1..])?;
            self.write(self.prefix_slot(tail), leaf)?;
            root
        } else {
            leaf
        };
        self.insert_child(node, key[depth], child)
    }

    /// `Leaf::MergeInlined`: two rows where there was one.
    fn merge_inlined(&mut self, slot: Slot, right: i64, in_gate: bool, depth: usize) -> Result<()> {
        let left = self.read(slot)?.row_id();
        let depth = if in_gate { depth } else { 0 };
        let (left_key, right_key) = (ArtKey::of(left), ArtKey::of(right));
        let position = left_key.mismatch_position(&right_key, depth);
        if position >= left_key.len() {
            return Err(Error::new(ErrorKind::Internal, "a row twice in an ART"));
        }
        let (left_byte, right_byte) = (left_key.as_bytes()[position], right_key.as_bytes()[position]);
        let merged = if position == ROW_ID_COUNT {
            let leaf = self.new_node(NodeType::Node7Leaf)?;
            self.place_byte(leaf, left_byte)?;
            self.place_byte(leaf, right_byte)?;
            leaf
        } else {
            let node = self.new_node(NodeType::Node4)?;
            self.place_child(node, left_byte, NodePtr::inlined_leaf(left))?;
            self.place_child(node, right_byte, NodePtr::inlined_leaf(right))?;
            node
        };
        let mut root = merged;
        if position != depth {
            let (first, tail) = self.new_prefix_chain(&left_key.as_bytes()[depth..position])?;
            self.write(self.prefix_slot(tail), merged)?;
            root = first;
        }
        root.set_gate(!in_gate);
        self.write(slot, root)
    }

    /// `ARTOperator::Delete`: whether the key had that row.
    pub fn erase(&mut self, key: &ArtKey, row_id: i64) -> Result<bool> {
        let row_key = ArtKey::of(row_id);
        let mut great_grandparent = Slot::Nowhere;
        let mut grandparent = Slot::Nowhere;
        let mut parent = Slot::Root;
        let mut current = Slot::Root;
        let mut current_key = key.as_bytes();
        let (mut grandparent_depth, mut parent_depth, mut depth) = (0, 0, 0);
        let mut in_gate = false;
        let mut passed_internal_node = false;
        loop {
            let node = self.read(current)?;
            if !node.is_set() {
                return Ok(false);
            }
            if !in_gate && node.is_gate() {
                in_gate = true;
                current_key = row_key.as_bytes();
                depth = 0;
                continue;
            }
            match Self::node_type(node)? {
                NodeType::LeafInlined => {
                    if node.row_id() != row_id {
                        return Ok(false);
                    }
                    let above = self.read(parent)?;
                    if Self::is_prefix(above) {
                        if !passed_internal_node {
                            // all that's left is this key
                            self.free_tree(above)?;
                            self.write(parent, NodePtr::EMPTY)?;
                            return Ok(true);
                        }
                        // the Node4 above this prefix may go, for which the prefix above it counts
                        let byte = current_key[grandparent_depth];
                        self.delete_child(grandparent, great_grandparent, byte, in_gate, row_id)?;
                        return Ok(true);
                    }
                    self.delete_child(parent, grandparent, current_key[parent_depth], in_gate, row_id)?;
                    return Ok(true);
                }
                NodeType::Prefix => {
                    great_grandparent = grandparent;
                    grandparent = parent;
                    parent = current;
                    grandparent_depth = parent_depth;
                    parent_depth = depth;
                    let mut walking = node;
                    while Self::is_prefix(walking) {
                        let data = self.prefix_bytes(walking)?;
                        if !current_key[depth..].starts_with(data) {
                            return Ok(false);
                        }
                        depth += data.len();
                        current = self.prefix_slot(walking);
                        walking = self.read(current)?;
                        if walking.is_gate() {
                            break;
                        }
                    }
                }
                NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
                    passed_internal_node = true;
                    great_grandparent = grandparent;
                    grandparent = parent;
                    parent = current;
                    grandparent_depth = parent_depth;
                    parent_depth = depth;
                    let Some(child) = self.child_slot(node, current_key[depth])? else {
                        return Ok(false);
                    };
                    current = child;
                    depth += 1;
                }
                NodeType::Node7Leaf | NodeType::Node15Leaf | NodeType::Node256Leaf => {
                    let byte = current_key[depth];
                    if !self.has_byte(node, byte)? {
                        return Ok(false);
                    }
                    self.delete_child(current, parent, byte, in_gate, row_id)?;
                    return Ok(true);
                }
                NodeType::Leaf => unreachable!("`node_type` refuses these"),
            }
        }
    }

    /// `ARTOperator::Lookup` and what follows it: the rows of a key, in order.
    pub fn lookup(&self, key: &ArtKey) -> Result<Vec<i64>> {
        let bytes = key.as_bytes();
        let mut node = self.root;
        let mut depth = 0;
        let mut rows = Vec::new();
        while node.is_set() {
            if node.is_gate() {
                self.rows_below(node, &mut Vec::new(), &mut rows)?;
                break;
            }
            match Self::node_type(node)? {
                NodeType::LeafInlined => {
                    rows.push(node.row_id());
                    break;
                }
                NodeType::Prefix => {
                    let data = self.prefix_bytes(node)?;
                    if !bytes[depth..].starts_with(data) {
                        break;
                    }
                    depth += data.len();
                    node = self.prefix_child(node)?;
                }
                _ => {
                    let Some(&byte) = bytes.get(depth) else { break };
                    let Some(slot) = self.child_slot(node, byte)? else {
                        break;
                    };
                    node = self.read(slot)?;
                    depth += 1;
                }
            }
        }
        Ok(rows)
    }

    /// All rows below a gate, by the bytes of a row id that led here.
    fn rows_below(&self, node: NodePtr, path: &mut Vec<u8>, rows: &mut Vec<i64>) -> Result<()> {
        let length = path.len();
        match Self::node_type(node)? {
            NodeType::LeafInlined => rows.push(node.row_id()),
            NodeType::Prefix => {
                path.extend_from_slice(self.prefix_bytes(node)?);
                self.rows_below(self.prefix_child(node)?, path, rows)?;
            }
            NodeType::Node7Leaf | NodeType::Node15Leaf | NodeType::Node256Leaf => {
                for byte in self.bytes_of(node)? {
                    let mut id = [0; 8];
                    let start = path
                        .get(..ROW_ID_COUNT)
                        .ok_or_else(|| corrupted("a leaf node of an ART is above the last byte of a row id"))?;
                    id[..ROW_ID_COUNT].copy_from_slice(start);
                    id[ROW_ID_COUNT] = byte;
                    rows.push(i64::from_be_bytes(id) ^ i64::MIN);
                }
            }
            _ => {
                for (byte, child) in self.children(node)? {
                    path.push(byte);
                    self.rows_below(child, path, rows)?;
                    path.pop();
                }
            }
        }
        path.truncate(length);
        Ok(())
    }

    /// `ART::Scan` of a range: the rows of the keys between two bounds, each with whether it is
    /// part of it, or `None` if they are more than `limit`.
    pub fn range(&self, low: KeyBound<'_>, high: KeyBound<'_>, limit: usize) -> Result<Option<Vec<i64>>> {
        let mut rows = Vec::new();
        let within = if self.root.is_set() {
            self.range_below(self.root, &mut Vec::new(), (low, high), limit, &mut rows)?
        } else {
            true
        };
        Ok(within.then_some(rows))
    }

    /// `false` if there are too many rows.
    fn range_below(
        &self,
        node: NodePtr,
        path: &mut Vec<u8>,
        bounds: (KeyBound<'_>, KeyBound<'_>),
        limit: usize,
        rows: &mut Vec<i64>,
    ) -> Result<bool> {
        let (low, high) = bounds;
        // all keys below here start with `path`
        let below_low = |path: &[u8]| low.is_some_and(|(key, _)| path < &key[..path.len().min(key.len())]);
        let above_high = |path: &[u8]| high.is_some_and(|(key, _)| path > &key[..path.len().min(key.len())]);
        if below_low(path) || above_high(path) {
            return Ok(true);
        }
        if node.is_gate() || node.node_type() == Some(NodeType::LeafInlined) {
            let after_low = low.is_none_or(|(key, inclusive)| match path.as_slice().cmp(key) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => inclusive,
                std::cmp::Ordering::Less => false,
            });
            let before_high = high.is_none_or(|(key, inclusive)| match path.as_slice().cmp(key) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => inclusive,
                std::cmp::Ordering::Greater => false,
            });
            if after_low && before_high {
                self.rows_below(node, &mut Vec::new(), rows)?;
            }
            return Ok(rows.len() <= limit);
        }
        let length = path.len();
        let mut within = true;
        if Self::is_prefix(node) {
            path.extend_from_slice(self.prefix_bytes(node)?);
            within = self.range_below(self.prefix_child(node)?, path, bounds, limit, rows)?;
        } else {
            for (byte, child) in self.children(node)? {
                path.push(byte);
                within = self.range_below(child, path, bounds, limit, rows)?;
                path.pop();
                if !within {
                    break;
                }
            }
        }
        path.truncate(length);
        Ok(within)
    }

    /// Every key and its rows, in the order of the keys.
    pub fn scan(&self) -> Result<Vec<(Vec<u8>, Vec<i64>)>> {
        let mut result = Vec::new();
        if self.root.is_set() {
            self.scan_below(self.root, &mut Vec::new(), &mut result)?;
        }
        Ok(result)
    }

    fn scan_below(&self, node: NodePtr, path: &mut Vec<u8>, result: &mut Vec<(Vec<u8>, Vec<i64>)>) -> Result<()> {
        if node.is_gate() || node.node_type() == Some(NodeType::LeafInlined) {
            let mut rows = Vec::new();
            self.rows_below(node, &mut Vec::new(), &mut rows)?;
            result.push((path.clone(), rows));
            return Ok(());
        }
        let length = path.len();
        if Self::is_prefix(node) {
            path.extend_from_slice(self.prefix_bytes(node)?);
            self.scan_below(self.prefix_child(node)?, path, result)?;
        } else {
            for (byte, child) in self.children(node)? {
                path.push(byte);
                self.scan_below(child, path, result)?;
                path.pop();
            }
        }
        path.truncate(length);
        Ok(())
    }
}

/// A key of a table and where its row is: the ART of a PRIMARY KEY or a UNIQUE.
#[derive(Debug, Clone)]
pub struct UniqueKeys {
    art: Art,
}

impl UniqueKeys {
    /// `DEFAULT_BLOCK_ALLOC_SIZE` less a checksum, which is all that needs to fit in a block.
    pub const BLOCK_SIZE: usize = 262_136;

    pub fn new(prefix_count: usize) -> Self {
        Self {
            art: Art::new(prefix_count, Self::BLOCK_SIZE, true),
        }
    }

    /// Adds a key, unless it's there: then where its row is comes back.
    pub fn insert(&mut self, key: &ArtKey, row: usize) -> Result<Option<usize>> {
        Ok(match self.art.insert(key, row as i64, AppendMode::Default)? {
            Conflict::None => None,
            Conflict::Constraint => self.get(key)?,
        })
    }

    pub fn get(&self, key: &ArtKey) -> Result<Option<usize>> {
        Ok(self.art.lookup(key)?.first().map(|row| *row as usize))
    }

    pub fn contains(&self, key: &ArtKey) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.art.scan()?.len())
    }

    pub fn is_empty(&self) -> bool {
        self.art.is_empty()
    }
}

/// `Node256Leaf::HasByte`: a mask of 256 bits.
fn leaf_mask_has(bytes: &[u8], byte: u8) -> bool {
    bytes[ALIGNED_FIELD + usize::from(byte / 8)] >> (byte % 8) & 1 != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    const BLOCK_SIZE: usize = 262_136;

    fn tree(unique: bool) -> Art {
        Art::new(7, BLOCK_SIZE, unique)
    }

    #[test]
    fn a_key_finds_its_row() {
        let mut art = tree(true);
        let key = ArtKey::of(42i32);
        assert_eq!(art.lookup(&key).unwrap(), Vec::<i64>::new());
        assert_eq!(art.insert(&key, 7, AppendMode::Default).unwrap(), Conflict::None);
        assert_eq!(art.lookup(&key).unwrap(), [7]);
        assert_eq!(art.lookup(&ArtKey::of(43i32)).unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn a_unique_index_refuses_a_second_row() {
        let mut art = tree(true);
        let key = ArtKey::of(1i32);
        art.insert(&key, 1, AppendMode::Default).unwrap();
        assert_eq!(art.insert(&key, 2, AppendMode::Default).unwrap(), Conflict::Constraint);
        assert_eq!(
            art.insert(&key, 2, AppendMode::IgnoreDuplicates).unwrap(),
            Conflict::None
        );
        assert_eq!(art.lookup(&key).unwrap(), [1]);
        assert_eq!(
            art.insert(&key, 2, AppendMode::InsertDuplicates).unwrap(),
            Conflict::None
        );
        assert_eq!(art.lookup(&key).unwrap(), [1, 2]);
    }

    #[test]
    fn nodes_grow_and_shrink_through_every_size() {
        let mut art = tree(true);
        let keys: Vec<ArtKey> = (0..=255u8)
            .map(|byte| ArtKey::of([byte, 9].iter().fold(0u16, |a, b| a << 8 | u16::from(*b))))
            .collect();
        for (row, key) in keys.iter().enumerate() {
            art.insert(key, row as i64, AppendMode::Default).unwrap();
            let expected: Vec<usize> = (0..=row).collect();
            let found: Vec<usize> = art.scan().unwrap().iter().map(|(_, rows)| rows[0] as usize).collect();
            assert_eq!(found, expected);
        }
        for (row, key) in keys.iter().enumerate().rev() {
            assert!(art.erase(key, row as i64).unwrap());
            assert_eq!(art.scan().unwrap().len(), row);
        }
        assert!(art.is_empty());
        assert_eq!(art.segment_count(), 0);
    }

    #[test]
    fn rows_of_a_key_go_under_a_gate_and_come_back_out() {
        let mut art = tree(false);
        let key = ArtKey::of_string(b"same");
        let rows: Vec<i64> = (0..600).map(|row| row * 3).collect();
        for &row in &rows {
            art.insert(&key, row, AppendMode::Default).unwrap();
        }
        assert_eq!(art.lookup(&key).unwrap(), rows);
        for &row in &rows[1..] {
            assert!(art.erase(&key, row).unwrap());
        }
        assert_eq!(art.lookup(&key).unwrap(), [0]);
        assert!(art.erase(&key, 0).unwrap());
        assert_eq!(art.segment_count(), 0, "nothing stays behind");
    }

    #[test]
    fn row_ids_that_differ_in_the_last_byte_share_a_leaf_node() {
        let mut art = tree(false);
        let key = ArtKey::of(1i32);
        for row in 0..300 {
            art.insert(&key, row, AppendMode::Default).unwrap();
        }
        assert_eq!(art.lookup(&key).unwrap().len(), 300);
        for row in 2..300 {
            assert!(art.erase(&key, row).unwrap());
        }
        assert_eq!(art.lookup(&key).unwrap(), [0, 1]);
    }

    #[test]
    fn what_is_not_there_is_not_erased() {
        let mut art = tree(false);
        let key = ArtKey::of(1i32);
        art.insert(&key, 1, AppendMode::Default).unwrap();
        assert!(!art.erase(&key, 2).unwrap());
        assert!(!art.erase(&ArtKey::of(2i32), 1).unwrap());
        assert_eq!(art.lookup(&key).unwrap(), [1]);
    }

    #[test]
    fn long_keys_take_chains_of_prefixes() {
        let mut art = tree(true);
        let long = [b'x'; 100];
        let (a, b) = (
            ArtKey::of_string(&[&long[..], b"a"].concat()),
            ArtKey::of_string(&[&long[..], b"b"].concat()),
        );
        art.insert(&a, 1, AppendMode::Default).unwrap();
        art.insert(&b, 2, AppendMode::Default).unwrap();
        assert_eq!(art.lookup(&a).unwrap(), [1]);
        assert_eq!(art.lookup(&b).unwrap(), [2]);
        assert!(art.erase(&a, 1).unwrap());
        assert_eq!(art.lookup(&b).unwrap(), [2]);
        assert!(art.erase(&b, 2).unwrap());
        assert_eq!(art.segment_count(), 0);
    }

    /// Takes keys that share a lot, as those are what make prefixes split and join.
    fn key_of(number: u16) -> ArtKey {
        let text = format!("key/{}/{}", number % 7, number % 301);
        ArtKey::of_string(text.as_bytes())
    }

    fn matches_model(art: &Art, model: &BTreeMap<Vec<u8>, BTreeSet<i64>>) {
        let found = art.scan().unwrap();
        let expected: Vec<(Vec<u8>, Vec<i64>)> = model
            .iter()
            .map(|(key, rows)| (key.clone(), rows.iter().copied().collect()))
            .collect();
        assert_eq!(found, expected);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Whatever goes in and comes out, it behaves as a map from keys to sets of rows, and
        /// leaves no node behind.
        #[test]
        fn a_tree_behaves_as_a_map(steps in proptest::collection::vec(any::<(bool, u16, u16)>(), 1..1200)) {
            let mut art = tree(false);
            let mut model: BTreeMap<Vec<u8>, BTreeSet<i64>> = BTreeMap::new();
            for (insert, number, row) in steps {
                let key = key_of(number);
                let row = i64::from(row % 2000) * 251;
                let held = model.get(key.as_bytes()).is_some_and(|rows| rows.contains(&row));
                if insert && !held {
                    art.insert(&key, row, AppendMode::Default).unwrap();
                    model.entry(key.as_bytes().to_vec()).or_default().insert(row);
                } else {
                    prop_assert_eq!(art.erase(&key, row).unwrap(), held);
                    if let Some(rows) = model.get_mut(key.as_bytes()) {
                        rows.remove(&row);
                        if rows.is_empty() {
                            model.remove(key.as_bytes());
                        }
                    }
                }
            }
            matches_model(&art, &model);
            for (key, rows) in &model {
                let found = art.lookup(&key_from_bytes(key)).unwrap();
                prop_assert_eq!(&found, &rows.iter().copied().collect::<Vec<_>>());
            }
            for (key, rows) in model {
                let key = key_from_bytes(&key);
                for row in rows {
                    prop_assert!(art.erase(&key, row).unwrap());
                }
            }
            prop_assert!(art.is_empty());
            prop_assert_eq!(art.segment_count(), 0);
        }

        /// A unique tree, whose keys are integers.
        #[test]
        fn a_unique_tree_behaves_as_a_map(steps in proptest::collection::vec(any::<(bool, u32)>(), 1..1500)) {
            let mut art = tree(true);
            let mut model: BTreeMap<i32, i64> = BTreeMap::new();
            for (row, (insert, number)) in steps.into_iter().enumerate() {
                let number = (number % 5000) as i32 * 257 - 400_000;
                let key = ArtKey::of(number);
                match (insert, model.get(&number).copied()) {
                    (true, Some(_)) => {
                        prop_assert_eq!(art.insert(&key, row as i64, AppendMode::Default).unwrap(), Conflict::Constraint);
                    }
                    (true, None) => {
                        prop_assert_eq!(art.insert(&key, row as i64, AppendMode::Default).unwrap(), Conflict::None);
                        model.insert(number, row as i64);
                    }
                    (false, Some(held)) => {
                        prop_assert!(art.erase(&key, held).unwrap());
                        model.remove(&number);
                    }
                    (false, None) => prop_assert!(!art.erase(&key, 0).unwrap()),
                }
            }
            let rows: Vec<i64> = art.scan().unwrap().into_iter().map(|(_, rows)| rows[0]).collect();
            prop_assert_eq!(rows, model.values().copied().collect::<Vec<_>>());
        }
    }

    /// The key that has these bytes, which a test made with `key_of`.
    fn key_from_bytes(bytes: &[u8]) -> ArtKey {
        ArtKey::of_string(&bytes[..bytes.len() - 1])
    }
}
