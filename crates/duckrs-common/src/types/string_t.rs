// Ported from duckdb/src/include/duckdb/common/types/string_type.hpp and string_heap.cpp
// @ rust-port-baseline

use parking_lot::Mutex;
use std::cmp::Ordering;
use std::fmt;

/// Bytes 8..16: the tail of an inlined string, or the pointer to a longer string's bytes.
#[derive(Clone, Copy)]
#[repr(C)]
union Tail {
    bytes: [u8; 8],
    ptr: *const u8,
}

/// `string_t`: DuckDB's 16-byte string. Bytes 0..4 hold the length and bytes 4..8 the first
/// four bytes of the string. Strings of up to 12 bytes continue inline in bytes 8..16; longer
/// strings store a pointer there to bytes owned elsewhere, usually a [`StringHeap`] held by
/// the vector the string lives in.
///
/// A `StringT` is a view and does not keep its bytes alive, so reading a non-inlined string is
/// `unsafe`. Use the vector accessors, which tie the bytes to the vector's lifetime.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct StringT {
    len: u32,
    prefix: [u8; 4],
    tail: Tail,
}

// SAFETY: a StringT is an immutable view. The bytes it points to are never mutated after the
// string is created, and their owner (a vector buffer) is itself Send + Sync.
unsafe impl Send for StringT {}
// SAFETY: see above.
unsafe impl Sync for StringT {}

impl Default for StringT {
    fn default() -> Self {
        Self::inlined(b"")
    }
}

impl StringT {
    pub const INLINE_LENGTH: usize = 12;
    pub const PREFIX_LENGTH: usize = 4;
    pub const MAX_LENGTH: usize = u32::MAX as usize;

    /// An inlined string. Panics if `bytes` is longer than [`Self::INLINE_LENGTH`].
    pub fn inlined(bytes: &[u8]) -> Self {
        assert!(bytes.len() <= Self::INLINE_LENGTH, "string too long to inline");
        // Zero the padding so equal strings have equal prefixes and bytes.
        let mut inlined = [0u8; 12];
        inlined[..bytes.len()].copy_from_slice(bytes);
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&inlined[..4]);
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&inlined[4..]);
        Self {
            len: bytes.len() as u32,
            prefix,
            tail: Tail { bytes: tail },
        }
    }

    /// A string pointing at `len` bytes at `ptr` (inlined when short enough).
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for reads of `len` bytes for as long as this value, or any copy of
    /// it, is read through [`Self::as_bytes`].
    pub unsafe fn from_raw_parts(ptr: *const u8, len: usize) -> Self {
        assert!(len <= Self::MAX_LENGTH, "string exceeds the maximum string length");
        // SAFETY: the caller guarantees ptr is valid for len bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        if len <= Self::INLINE_LENGTH {
            return Self::inlined(bytes);
        }
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&bytes[..Self::PREFIX_LENGTH]);
        Self {
            len: len as u32,
            prefix,
            tail: Tail { ptr },
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_inlined(&self) -> bool {
        self.len() <= Self::INLINE_LENGTH
    }

    /// The first (up to) four bytes, zero-padded. Valid without dereferencing.
    pub fn prefix(&self) -> [u8; 4] {
        self.prefix
    }

    /// The string's bytes.
    ///
    /// # Safety
    ///
    /// For a non-inlined string, the buffer it was created from must still be alive.
    pub unsafe fn as_bytes(&self) -> &[u8] {
        if self.is_inlined() {
            // SAFETY: repr(C) places `prefix` at offset 4 and `tail` at offset 8 with no padding,
            // so an inlined string's len <= 12 bytes are contiguous inside `self`.
            unsafe { std::slice::from_raw_parts(self.prefix.as_ptr(), self.len()) }
        } else {
            // SAFETY: the caller guarantees the pointee is alive; it holds len bytes.
            unsafe { std::slice::from_raw_parts(self.tail.ptr, self.len()) }
        }
    }

    /// Byte-wise equality with the prefix fast path (`StringComparisonOperators::Equals`).
    ///
    /// # Safety
    ///
    /// As for [`Self::as_bytes`], for both strings.
    pub unsafe fn equals(&self, other: &StringT) -> bool {
        if self.len != other.len || self.prefix() != other.prefix() {
            return false;
        }
        // SAFETY: forwarded to the caller.
        unsafe { self.as_bytes() == other.as_bytes() }
    }

    /// Lexicographic byte order, shorter strings first on a tie (`memcmp` then length).
    ///
    /// # Safety
    ///
    /// As for [`Self::as_bytes`], for both strings.
    pub unsafe fn compare(&self, other: &StringT) -> Ordering {
        match self.prefix().cmp(&other.prefix()) {
            Ordering::Equal => {}
            unequal if self.len() >= 4 && other.len() >= 4 => return unequal,
            _ => {}
        }
        // SAFETY: forwarded to the caller.
        unsafe { self.as_bytes().cmp(other.as_bytes()) }
    }
}

impl fmt::Debug for StringT {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_inlined() {
            // SAFETY: inlined strings never dereference a pointer.
            let bytes = unsafe { self.as_bytes() };
            write!(f, "StringT({:?})", String::from_utf8_lossy(bytes))
        } else {
            write!(f, "StringT(len={}, prefix={:?})", self.len, self.prefix())
        }
    }
}

const CHUNK_SIZE: usize = 16 * 1024;

#[derive(Default)]
struct Arena {
    chunks: Vec<Box<[u8]>>,
    used: usize,
    /// buffers of bytes the heap took as they are ([`StringHeap::adopt`])
    adopted: Vec<Vec<u8>>,
}

/// An append-only arena for string bytes (`StringHeap`). Bytes never move once added, so
/// strings that point into the heap stay valid until it is dropped.
#[derive(Default)]
pub struct StringHeap {
    arena: Mutex<Arena>,
}

impl fmt::Debug for StringHeap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arena = self.arena.lock();
        write!(f, "StringHeap({} chunks)", arena.chunks.len())
    }
}

impl StringHeap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copies `bytes` into the heap (unless short enough to inline) and returns a string for it.
    /// The result may only be read while this heap is alive.
    pub fn add(&self, bytes: &[u8]) -> StringT {
        if bytes.len() <= StringT::INLINE_LENGTH {
            return StringT::inlined(bytes);
        }
        let mut arena = self.arena.lock();
        let fits = arena.chunks.last().is_some_and(|c| c.len() - arena.used >= bytes.len());
        if !fits {
            let size = bytes.len().max(CHUNK_SIZE);
            arena.chunks.push(vec![0u8; size].into_boxed_slice());
            arena.used = 0;
        }
        let used = arena.used;
        let chunk = arena.chunks.last_mut().expect("chunk was just ensured");
        chunk[used..used + bytes.len()].copy_from_slice(bytes);
        let ptr = chunk[used..].as_ptr();
        arena.used += bytes.len();
        // SAFETY: the bytes live in a boxed chunk that is never moved or freed before the heap.
        unsafe { StringT::from_raw_parts(ptr, bytes.len()) }
    }

    /// Takes `bytes` into the heap as they are, without copying them, and gives where they now are: strings may
    /// point into them while the heap is alive.
    pub fn adopt(&self, bytes: Vec<u8>) -> *const u8 {
        // the bytes of a vector stay where they are when the vector moves
        let ptr = bytes.as_ptr();
        self.arena.lock().adopted.push(bytes);
        ptr
    }

    /// Bytes allocated so far, for memory accounting.
    pub fn allocated(&self) -> usize {
        let arena = self.arena.lock();
        arena.chunks.iter().map(|c| c.len()).sum::<usize>() + arena.adopted.iter().map(Vec::capacity).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_cpp() {
        assert_eq!(size_of::<StringT>(), 16);
        assert_eq!(std::mem::offset_of!(StringT, prefix), 4);
        assert_eq!(std::mem::offset_of!(StringT, tail), 8);
    }

    #[test]
    fn inline_and_heap_strings() {
        let heap = StringHeap::new();
        let short = heap.add(b"hello");
        let long = heap.add(b"a much longer string value");
        assert!(short.is_inlined() && !long.is_inlined());
        // SAFETY: heap is alive.
        unsafe {
            assert_eq!(short.as_bytes(), b"hello");
            assert_eq!(long.as_bytes(), b"a much longer string value");
            assert_eq!(long.prefix(), *b"a mu");
        }
        assert_eq!(StringT::inlined(b"ab").prefix(), [b'a', b'b', 0, 0]);
    }

    #[test]
    fn heap_survives_many_chunks() {
        let heap = StringHeap::new();
        let strings: Vec<(StringT, String)> = (0..5000)
            .map(|i| {
                let s = format!("string number {i:06} with padding");
                (heap.add(s.as_bytes()), s)
            })
            .collect();
        for (st, s) in &strings {
            // SAFETY: heap is alive.
            assert_eq!(unsafe { st.as_bytes() }, s.as_bytes());
        }
        let big = vec![b'x'; CHUNK_SIZE * 2];
        let big_string = heap.add(&big);
        // SAFETY: heap is alive.
        assert_eq!(unsafe { big_string.as_bytes() }, &big[..]);
    }

    #[test]
    fn comparisons() {
        let heap = StringHeap::new();
        let cases: [&[u8]; 7] = [b"", b"a", b"ab", b"abc", b"abcdefghijklmnop", b"abcdefghijklmnoq", b"b"];
        for a in cases {
            for b in cases {
                let (sa, sb) = (heap.add(a), heap.add(b));
                // SAFETY: heap is alive.
                unsafe {
                    assert_eq!(sa.compare(&sb), a.cmp(b), "{a:?} vs {b:?}");
                    assert_eq!(sa.equals(&sb), a == b);
                }
            }
        }
    }
}
