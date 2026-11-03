// Ported from duckdb/src/common/types/row/tuple_data_layout.cpp @ rust-port-baseline

use crate::types::{LogicalType, PhysicalType};
use std::collections::HashMap;

/// Rounds up to a multiple of 8 (`AlignValue`).
pub fn align8(value: usize) -> usize {
    value.div_ceil(8) * 8
}

/// Whether the layout reserves validity bits (`TupleDataValidityType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowValidity {
    CanHaveNull,
    CannotHaveNull,
}

/// Where each column and aggregate state lives inside a row (`TupleDataLayout`):
///
/// ```text
/// [validity bits][heap size?][column 0][column 1]...[pad][aggregate 0][aggregate 1]...[pad]
/// ```
///
/// One validity bit per column (1 = valid) comes first. Rows with variable-size columns
/// reserve 8 bytes for their heap size (used when rows spill). Columns are packed without
/// alignment; aggregate states are 8-byte aligned. A STRUCT column is stored inline using a
/// nested layout of its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleDataLayout {
    types: Vec<LogicalType>,
    aggregate_sizes: Vec<usize>,
    flag_width: usize,
    data_width: usize,
    aggr_width: usize,
    row_width: usize,
    offsets: Vec<usize>,
    all_constant: bool,
    heap_size_offset: Option<usize>,
    variable_columns: Vec<usize>,
    struct_layouts: HashMap<usize, TupleDataLayout>,
    validity: RowValidity,
}

impl TupleDataLayout {
    /// A layout for `types` followed by aggregate states of the given (8-byte multiple) sizes.
    pub fn new(types: Vec<LogicalType>, aggregate_sizes: Vec<usize>, validity: RowValidity) -> Self {
        Self::build(types, aggregate_sizes, validity, true)
    }

    fn build(types: Vec<LogicalType>, aggregate_sizes: Vec<usize>, validity: RowValidity, top_level: bool) -> Self {
        let flag_width = match validity {
            RowValidity::CanHaveNull => types.len().div_ceil(8),
            RowValidity::CannotHaveNull => 0,
        };
        let mut row_width = flag_width;
        let mut all_constant = true;
        let mut variable_columns = Vec::new();
        let mut struct_layouts = HashMap::new();
        for (i, ty) in types.iter().enumerate() {
            match ty.physical_type() {
                PhysicalType::Struct => {
                    let children = ty.struct_children().iter().map(|(_, t)| t.clone()).collect();
                    let child = Self::build(children, Vec::new(), RowValidity::CanHaveNull, false);
                    if !child.all_constant {
                        all_constant = false;
                        variable_columns.push(i);
                    }
                    struct_layouts.insert(i, child);
                }
                p if !p.is_constant_size() => {
                    all_constant = false;
                    variable_columns.push(i);
                }
                _ => {}
            }
        }
        let mut heap_size_offset = None;
        if top_level && !all_constant {
            heap_size_offset = Some(row_width);
            row_width += 8;
        }
        let mut offsets = Vec::with_capacity(types.len() + aggregate_sizes.len());
        for (i, ty) in types.iter().enumerate() {
            offsets.push(row_width);
            let physical = ty.physical_type();
            row_width += if physical.is_constant_size() || physical == PhysicalType::Varchar {
                physical.size()
            } else if physical == PhysicalType::Struct {
                struct_layouts[&i].row_width
            } else {
                // LIST/ARRAY: a pointer to the heap.
                8
            };
        }
        let align = !aggregate_sizes.is_empty();
        if align {
            row_width = align8(row_width);
        }
        let data_width = row_width - flag_width;
        for &size in &aggregate_sizes {
            assert_eq!(size, align8(size), "aggregate state sizes must be multiples of 8");
            offsets.push(row_width);
            row_width += size;
        }
        let aggr_width = row_width - data_width - flag_width;
        if align {
            row_width = align8(row_width);
        }
        Self {
            types,
            aggregate_sizes,
            flag_width,
            data_width,
            aggr_width,
            row_width,
            offsets,
            all_constant,
            heap_size_offset,
            variable_columns,
            struct_layouts,
            validity,
        }
    }

    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    pub fn column_count(&self) -> usize {
        self.types.len()
    }

    pub fn aggregate_count(&self) -> usize {
        self.aggregate_sizes.len()
    }

    /// Byte offsets of each column, then each aggregate state.
    pub fn offsets(&self) -> &[usize] {
        &self.offsets
    }

    /// The offset of aggregate state `i`.
    pub fn aggregate_offset(&self, i: usize) -> usize {
        self.offsets[self.types.len() + i]
    }

    pub fn flag_width(&self) -> usize {
        self.flag_width
    }

    pub fn data_width(&self) -> usize {
        self.data_width
    }

    pub fn aggr_width(&self) -> usize {
        self.aggr_width
    }

    pub fn row_width(&self) -> usize {
        self.row_width
    }

    pub fn all_constant(&self) -> bool {
        self.all_constant
    }

    pub fn heap_size_offset(&self) -> Option<usize> {
        self.heap_size_offset
    }

    pub fn variable_columns(&self) -> &[usize] {
        &self.variable_columns
    }

    pub fn can_have_null(&self) -> bool {
        self.validity == RowValidity::CanHaveNull
    }

    /// The nested layout of STRUCT column `column`.
    pub fn struct_layout(&self, column: usize) -> &TupleDataLayout {
        &self.struct_layouts[&column]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_offsets() {
        let l = TupleDataLayout::new(
            vec![LogicalType::INTEGER, LogicalType::BIGINT, LogicalType::BOOLEAN],
            vec![],
            RowValidity::CanHaveNull,
        );
        assert_eq!(l.flag_width(), 1);
        assert_eq!(l.offsets(), [1, 5, 13]);
        assert_eq!(l.row_width(), 14);
        assert!(l.all_constant() && l.heap_size_offset().is_none());

        let with_strings = TupleDataLayout::new(
            vec![LogicalType::VARCHAR, LogicalType::INTEGER],
            vec![16, 8],
            RowValidity::CanHaveNull,
        );
        // validity (1) + heap size (8) + string (16) + int (4) = 29, aligned to 32.
        assert_eq!(with_strings.heap_size_offset(), Some(1));
        assert_eq!(with_strings.offsets(), [9, 25, 32, 48]);
        assert_eq!(with_strings.aggregate_offset(1), 48);
        assert_eq!(with_strings.row_width(), 56);
        assert_eq!(with_strings.data_width(), 31);
        assert_eq!(with_strings.aggr_width(), 24);

        let s = LogicalType::struct_type(vec![
            ("a".into(), LogicalType::INTEGER),
            ("b".into(), LogicalType::DOUBLE),
        ]);
        let nested = TupleDataLayout::new(vec![s], vec![], RowValidity::CannotHaveNull);
        assert_eq!(nested.flag_width(), 0);
        assert_eq!(nested.struct_layout(0).row_width(), 1 + 4 + 8);
        assert_eq!(nested.row_width(), 13);
    }
}
