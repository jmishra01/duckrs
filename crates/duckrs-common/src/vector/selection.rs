// Ported from duckdb/src/include/duckdb/common/types/selection_vector.hpp @ rust-port-baseline

use std::sync::Arc;

/// Maps an output row `i` to a source row `get(i)` (`SelectionVector`, `sel_t = uint32_t`).
#[derive(Debug, Clone, Default)]
pub enum SelectionVector {
    /// `get(i) == i` (`incremental_selection`).
    #[default]
    Identity,
    /// `get(i) == 0`: every row reads the first value, as for a constant vector
    /// (`ConstantVector::ZeroSelectionVector`).
    Zero,
    Explicit(Arc<Vec<u32>>),
}

impl SelectionVector {
    pub fn from_vec(indices: Vec<u32>) -> Self {
        Self::Explicit(Arc::new(indices))
    }

    /// An explicit identity selection of `count` rows, ready to be modified with [`Self::set`].
    pub fn with_capacity(count: usize) -> Self {
        Self::from_vec((0..count as u32).collect())
    }

    #[inline]
    pub fn get(&self, i: usize) -> usize {
        match self {
            Self::Identity => i,
            Self::Zero => 0,
            Self::Explicit(indices) => indices[i] as usize,
        }
    }

    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Identity)
    }

    /// Sets `get(i) = index`, materializing an explicit selection if needed.
    pub fn set(&mut self, i: usize, index: u32) {
        if !matches!(self, Self::Explicit(_)) {
            let current: Vec<u32> = (0..=i).map(|j| self.get(j) as u32).collect();
            *self = Self::Explicit(Arc::new(current));
        }
        let Self::Explicit(indices) = self else { unreachable!() };
        let indices = Arc::make_mut(indices);
        if indices.len() <= i {
            indices.resize(i + 1, 0);
        }
        indices[i] = index;
    }

    /// The explicit indices, if any.
    pub fn indices(&self) -> Option<&[u32]> {
        match self {
            Self::Explicit(indices) => Some(indices),
            _ => None,
        }
    }

    /// `result.get(i) == self.get(inner.get(i))` for `i < count` (`SelectionVector::Slice`):
    /// applying `inner` to rows that `self` already selects.
    pub fn compose(&self, inner: &SelectionVector, count: usize) -> SelectionVector {
        match (self, inner) {
            (Self::Identity, _) => inner.clone(),
            (Self::Zero, _) => Self::Zero,
            (_, Self::Identity) => self.clone(),
            _ => Self::from_vec((0..count).map(|i| self.get(inner.get(i)) as u32).collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_selections() {
        let outer = SelectionVector::from_vec(vec![4, 3, 2, 1, 0]);
        let inner = SelectionVector::from_vec(vec![0, 0, 2]);
        let composed = outer.compose(&inner, 3);
        assert_eq!((0..3).map(|i| composed.get(i)).collect::<Vec<_>>(), [4, 4, 2]);
        assert_eq!(SelectionVector::Identity.compose(&inner, 3).get(2), 2);
        assert_eq!(SelectionVector::Zero.compose(&inner, 3).get(2), 0);
    }

    #[test]
    fn set_materializes() {
        let mut sel = SelectionVector::Identity;
        sel.set(3, 9);
        assert_eq!((0..4).map(|i| sel.get(i)).collect::<Vec<_>>(), [0, 1, 2, 9]);
    }
}
