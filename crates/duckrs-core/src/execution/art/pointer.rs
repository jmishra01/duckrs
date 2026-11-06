// Ported from duckdb/src/include/duckdb/execution/index/index_pointer.hpp and
// duckdb/src/include/duckdb/execution/index/art/node.hpp @ rust-port-baseline
//
//! The 8 bytes that point at a node. They go into the file as they are.

/// `NType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NodeType {
    Prefix = 1,
    /// Deprecated: only in old files.
    Leaf = 2,
    Node4 = 3,
    Node16 = 4,
    Node48 = 5,
    Node256 = 6,
    LeafInlined = 7,
    Node7Leaf = 8,
    Node15Leaf = 9,
    Node256Leaf = 10,
}

impl NodeType {
    pub const ALL: [NodeType; 10] = [
        NodeType::Prefix,
        NodeType::Leaf,
        NodeType::Node4,
        NodeType::Node16,
        NodeType::Node48,
        NodeType::Node256,
        NodeType::LeafInlined,
        NodeType::Node7Leaf,
        NodeType::Node15Leaf,
        NodeType::Node256Leaf,
    ];

    fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| *ty as u8 == byte)
    }

    /// `NodePtr::GetAllocatorIdx`: an inlined leaf is in its pointer, and has no allocator.
    pub fn allocator_index(self) -> Option<usize> {
        match self {
            NodeType::Prefix => Some(0),
            NodeType::Leaf => Some(1),
            NodeType::Node4 => Some(2),
            NodeType::Node16 => Some(3),
            NodeType::Node48 => Some(4),
            NodeType::Node256 => Some(5),
            NodeType::Node7Leaf => Some(6),
            NodeType::Node15Leaf => Some(7),
            NodeType::Node256Leaf => Some(8),
            NodeType::LeafInlined => None,
        }
    }
}

/// `IndexPointer`: a buffer in bits 0-31, a segment of it in bits 32-55, and metadata on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexPointer(u64);

const SHIFT_OFFSET: u32 = 32;
const SHIFT_METADATA: u32 = 56;
const AND_OFFSET: u64 = 0x0000_0000_00FF_FFFF;
const AND_BUFFER_ID: u64 = 0x0000_0000_FFFF_FFFF;
const AND_METADATA: u64 = 0xFF00_0000_0000_0000;

impl IndexPointer {
    pub fn new(buffer_id: u32, offset: u32) -> Self {
        Self(u64::from(offset) << SHIFT_OFFSET | u64::from(buffer_id))
    }

    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub fn bits(self) -> u64 {
        self.0
    }

    pub fn has_metadata(self) -> bool {
        self.0 & AND_METADATA != 0
    }

    pub fn metadata(self) -> u8 {
        (self.0 >> SHIFT_METADATA) as u8
    }

    pub fn set_metadata(&mut self, metadata: u8) {
        self.0 = self.0 & !AND_METADATA | u64::from(metadata) << SHIFT_METADATA;
    }

    pub fn offset(self) -> u64 {
        self.0 >> SHIFT_OFFSET & AND_OFFSET
    }

    pub fn buffer_id(self) -> u64 {
        self.0 & AND_BUFFER_ID
    }
}

/// `NodePtr`: the metadata says the type of the node, and a set top bit makes it a gate, below
/// which a key goes on with row ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodePtr(pub IndexPointer);

const AND_GATE: u8 = 0x80;
const AND_ROW_ID: u64 = 0x00FF_FFFF_FFFF_FFFF;

impl NodePtr {
    /// Pointing at nothing.
    pub const EMPTY: NodePtr = NodePtr(IndexPointer(0));

    pub fn new(pointer: IndexPointer, ty: NodeType) -> Self {
        let mut node = Self(pointer);
        node.0.set_metadata(ty as u8);
        node
    }

    /// `Leaf::New`: a leaf of one row is in its pointer.
    pub fn inlined_leaf(row_id: i64) -> Self {
        let mut node = Self(IndexPointer(row_id as u64 & AND_ROW_ID));
        node.0.set_metadata(NodeType::LeafInlined as u8);
        node
    }

    pub fn is_set(self) -> bool {
        self.0.has_metadata()
    }

    /// `None` for nothing, or for metadata that no node has, which only a damaged file can do.
    pub fn node_type(self) -> Option<NodeType> {
        NodeType::from_byte(self.0.metadata() & !AND_GATE)
    }

    pub fn is_gate(self) -> bool {
        self.0.metadata() & AND_GATE != 0
    }

    pub fn set_gate(&mut self, gate: bool) {
        let metadata = self.0.metadata() & !AND_GATE;
        self.0.set_metadata(metadata | if gate { AND_GATE } else { 0 });
    }

    pub fn row_id(self) -> i64 {
        (self.0.bits() & AND_ROW_ID) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_keeps_its_parts_apart() {
        let mut pointer = IndexPointer::new(0xDEAD_BEEF, 0x12_3456);
        assert_eq!(pointer.bits(), 0x0012_3456_DEAD_BEEF);
        assert_eq!((pointer.buffer_id(), pointer.offset()), (0xDEAD_BEEF, 0x12_3456));
        assert!(!pointer.has_metadata());
        pointer.set_metadata(0xAB);
        assert_eq!(pointer.bits(), 0xAB12_3456_DEAD_BEEF);
        assert_eq!((pointer.buffer_id(), pointer.offset()), (0xDEAD_BEEF, 0x12_3456));
    }

    #[test]
    fn a_node_has_a_type_and_maybe_a_gate() {
        let mut node = NodePtr::new(IndexPointer::new(1, 2), NodeType::Node48);
        assert_eq!(node.node_type(), Some(NodeType::Node48));
        assert!(!node.is_gate());
        node.set_gate(true);
        assert_eq!(node.0.metadata(), 0x85);
        assert_eq!(node.node_type(), Some(NodeType::Node48));
        node.set_gate(false);
        assert_eq!(node.0.metadata(), 5);
        assert_eq!(NodePtr::EMPTY.node_type(), None);
        assert!(!NodePtr::EMPTY.is_set());
    }

    #[test]
    fn a_row_id_fits_in_a_leaf() {
        let leaf = NodePtr::inlined_leaf(0x00AB_CDEF_0123_4567);
        assert_eq!(leaf.node_type(), Some(NodeType::LeafInlined));
        assert_eq!(leaf.row_id(), 0x00AB_CDEF_0123_4567);
    }
}
