// Ported from duckdb/src/common/types/vector_buffer.cpp @ rust-port-baseline

use crate::types::{NativeType, StringHeap, StringT};
use parking_lot::Mutex;
use std::fmt;
use std::sync::Arc;

// `DataBuffer` relies on u128 words being 16-byte aligned.
const _: () = assert!(align_of::<u128>() == 16);

/// Zero-initialized storage for a flat vector's values, aligned to 16 bytes so it can be viewed
/// as a slice of any [`NativeType`].
#[derive(Clone, Default)]
pub(crate) struct DataBuffer {
    words: Vec<u128>,
    bytes: usize,
}

impl fmt::Debug for DataBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DataBuffer({} bytes)", self.bytes)
    }
}

impl DataBuffer {
    pub(crate) fn zeroed(bytes: usize) -> Self {
        Self {
            words: vec![0; bytes.div_ceil(16)],
            bytes,
        }
    }

    pub(crate) fn as_slice<T: NativeType>(&self) -> &[T] {
        let len = self.bytes.checked_div(size_of::<T>()).unwrap_or(0);
        // SAFETY: `words` is 16-byte aligned (align_of::<u128>() == 16 on all supported
        // targets) and holds at least `bytes` initialized bytes. NativeType guarantees T is POD
        // with alignment <= 16 and that every bit pattern written through `as_mut_slice::<T>`
        // or zeroing is a valid T.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<T>(), len) }
    }

    pub(crate) fn as_mut_slice<T: NativeType>(&mut self) -> &mut [T] {
        let len = self.bytes.checked_div(size_of::<T>()).unwrap_or(0);
        // SAFETY: as for `as_slice`; `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<T>(), len) }
    }

    /// Grows to `bytes`, zero-filling the new space.
    pub(crate) fn grow(&mut self, bytes: usize) {
        if bytes > self.bytes {
            self.words.resize(bytes.div_ceil(16), 0);
            self.bytes = bytes;
        }
    }
}

/// Owns the bytes of a vector's non-inlined strings (`VectorStringBuffer`). Strings copied from
/// other vectors without copying their bytes keep those vectors' buffers alive through
/// `keep_alive` (`StringVector::AddHeapReference`).
#[derive(Debug, Default)]
pub struct StringBuffer {
    heap: StringHeap,
    keep_alive: Mutex<Vec<Arc<StringBuffer>>>,
}

impl StringBuffer {
    /// Bytes its strings took so far, for memory accounting.
    pub fn allocated(&self) -> usize {
        self.heap.allocated()
    }

    /// Copies `bytes` into this buffer. The result stays readable while the buffer is alive.
    pub fn add(&self, bytes: &[u8]) -> StringT {
        self.heap.add(bytes)
    }

    /// Takes `bytes` into this buffer without copying them ([`StringHeap::adopt`]).
    pub fn adopt(&self, bytes: Vec<u8>) -> *const u8 {
        self.heap.adopt(bytes)
    }

    /// Keeps `other` alive as long as this buffer, so strings pointing into it stay valid.
    pub(crate) fn add_reference(&self, other: &Arc<StringBuffer>) {
        // A buffer referencing itself would be a reference cycle (a leak).
        if std::ptr::eq(self, Arc::as_ptr(other)) {
            return;
        }
        let mut refs = self.keep_alive.lock();
        if !refs.iter().any(|r| Arc::ptr_eq(r, other)) {
            refs.push(other.clone());
        }
    }
}
