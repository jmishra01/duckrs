// Ported from duckdb/src/include/duckdb/common/types/validity_mask.hpp @ rust-port-baseline

use std::sync::Arc;

/// Per-row NULL tracking: one bit per row, 1 = valid. A mask without a bitmap means "all rows
/// valid", which is the common case and costs nothing.
#[derive(Debug, Clone, Default)]
pub struct ValidityMask {
    bits: Option<Arc<Vec<u64>>>,
    capacity: usize,
}

const BITS_PER_ENTRY: usize = 64;

fn entry_count(capacity: usize) -> usize {
    capacity.div_ceil(BITS_PER_ENTRY)
}

impl ValidityMask {
    /// An all-valid mask for `capacity` rows.
    pub fn new(capacity: usize) -> Self {
        Self { bits: None, capacity }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// True if no bitmap is allocated (every row is valid). A mask with a bitmap may still
    /// have all bits set; use [`Self::count_valid`] to check.
    pub fn all_valid(&self) -> bool {
        self.bits.is_none()
    }

    pub fn is_valid(&self, row: usize) -> bool {
        match &self.bits {
            None => true,
            Some(bits) => bits[row / BITS_PER_ENTRY] & (1 << (row % BITS_PER_ENTRY)) != 0,
        }
    }

    /// The raw 64-row entries, if a bitmap is allocated.
    pub fn entries(&self) -> Option<&[u64]> {
        self.bits.as_deref().map(Vec::as_slice)
    }

    fn bits_mut(&mut self) -> &mut Vec<u64> {
        let capacity = self.capacity;
        let bits = self
            .bits
            .get_or_insert_with(|| Arc::new(vec![u64::MAX; entry_count(capacity)]));
        Arc::make_mut(bits)
    }

    pub fn set_invalid(&mut self, row: usize) {
        assert!(
            row < self.capacity,
            "row {row} out of range for capacity {}",
            self.capacity
        );
        self.bits_mut()[row / BITS_PER_ENTRY] &= !(1 << (row % BITS_PER_ENTRY));
    }

    pub fn set_valid(&mut self, row: usize) {
        if self.bits.is_some() {
            self.bits_mut()[row / BITS_PER_ENTRY] |= 1 << (row % BITS_PER_ENTRY);
        }
    }

    pub fn set(&mut self, row: usize, valid: bool) {
        if valid {
            self.set_valid(row);
        } else {
            self.set_invalid(row);
        }
    }

    /// Marks rows `0..count` invalid.
    pub fn set_all_invalid(&mut self, count: usize) {
        assert!(count <= self.capacity);
        let bits = self.bits_mut();
        let full = count / BITS_PER_ENTRY;
        bits[..full].fill(0);
        if !count.is_multiple_of(BITS_PER_ENTRY) {
            bits[full] &= !((1u64 << (count % BITS_PER_ENTRY)) - 1);
        }
    }

    /// Drops the bitmap, making every row valid.
    pub fn set_all_valid(&mut self) {
        self.bits = None;
    }

    /// Valid rows among `0..count`.
    pub fn count_valid(&self, count: usize) -> usize {
        let Some(bits) = &self.bits else {
            return count;
        };
        let full = count / BITS_PER_ENTRY;
        let mut valid: usize = bits[..full].iter().map(|e| e.count_ones() as usize).sum();
        if !count.is_multiple_of(BITS_PER_ENTRY) {
            let tail = bits[full] & ((1u64 << (count % BITS_PER_ENTRY)) - 1);
            valid += tail.count_ones() as usize;
        }
        valid
    }

    /// A mask for rows `offset..offset + count` of this one.
    pub fn slice(&self, offset: usize, count: usize) -> Self {
        let Some(bits) = &self.bits else {
            return Self::new(count);
        };
        if offset.is_multiple_of(BITS_PER_ENTRY) {
            // Word-aligned: copy whole entries.
            let first = offset / BITS_PER_ENTRY;
            let mut words = vec![u64::MAX; entry_count(count)];
            let available = bits.len().saturating_sub(first).min(words.len());
            words[..available].copy_from_slice(&bits[first..first + available]);
            return Self {
                bits: Some(Arc::new(words)),
                capacity: count,
            };
        }
        let mut result = Self::new(count);
        for i in 0..count {
            if !self.is_valid(offset + i) {
                result.set_invalid(i);
            }
        }
        result
    }

    /// Keeps a row valid only if it is valid in both masks (word-wise AND).
    pub fn and_with(&mut self, other: &ValidityMask) {
        let Some(other_bits) = other.entries() else {
            return;
        };
        let bits = self.bits_mut();
        for (a, b) in bits.iter_mut().zip(other_bits) {
            *a &= *b;
        }
    }

    /// Grows the mask to `capacity` rows; new rows are valid.
    pub fn resize(&mut self, capacity: usize) {
        if capacity <= self.capacity {
            return;
        }
        if let Some(bits) = &mut self.bits {
            Arc::make_mut(bits).resize(entry_count(capacity), u64::MAX);
        }
        self.capacity = capacity;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_count() {
        let mut m = ValidityMask::new(130);
        assert!(m.all_valid());
        assert_eq!(m.count_valid(130), 130);
        m.set_invalid(0);
        m.set_invalid(64);
        m.set_invalid(129);
        assert!(!m.is_valid(64) && m.is_valid(65));
        assert_eq!(m.count_valid(130), 127);
        assert_eq!(m.count_valid(64), 63);
        m.set_valid(64);
        assert_eq!(m.count_valid(130), 128);
    }

    #[test]
    fn copies_are_independent() {
        let mut a = ValidityMask::new(10);
        a.set_invalid(1);
        let mut b = a.clone();
        b.set_invalid(2);
        assert!(a.is_valid(2));
        assert!(!b.is_valid(1) && !b.is_valid(2));
    }

    #[test]
    fn aligned_slice_and_intersection() {
        let mut a = ValidityMask::new(200);
        a.set_invalid(3);
        a.set_invalid(130);
        let s = a.slice(128, 70);
        assert_eq!(s.count_valid(70), 69);
        assert!(!s.is_valid(2));
        let mut b = ValidityMask::new(200);
        b.set_invalid(4);
        b.and_with(&a);
        assert_eq!(b.count_valid(200), 197);
        let mut c = ValidityMask::new(200);
        c.and_with(&ValidityMask::new(200));
        assert!(c.all_valid());
    }

    #[test]
    fn all_invalid_slice_resize() {
        let mut m = ValidityMask::new(100);
        m.set_all_invalid(70);
        assert_eq!(m.count_valid(100), 30);
        let s = m.slice(60, 20);
        assert_eq!(s.count_valid(20), 10);
        m.resize(200);
        assert!(m.is_valid(150));
        assert_eq!(m.count_valid(200), 130);
    }
}
