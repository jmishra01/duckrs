// Ported from duckdb/src/common/types/row/tuple_data_collection.cpp and
// tuple_data_scatter_gather.cpp @ rust-port-baseline
//
// In-memory only for now: rows live in fixed blocks that never move, so row pointers stay valid
// until the collection is reset or dropped. Spilling (pointer swizzling through the buffer
// manager) comes with the storage engine. LIST/ARRAY columns are not supported yet.

use super::layout::TupleDataLayout;
use crate::STANDARD_VECTOR_SIZE;
use crate::types::{Interval, PhysicalType, StringT};
use crate::vector::{DataChunk, StringBuffer, Vector};
use crate::{Error, Result};
use std::ptr::NonNull;
use std::sync::Arc;

const BLOCK_SIZE: usize = 256 * 1024;

/// The address of a row in a [`TupleDataCollection`]. Hash tables store these. A `RowPtr` is
/// only valid while its collection is alive and has not been reset; reading through one is
/// therefore `unsafe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowPtr(NonNull<u8>);

// SAFETY: a RowPtr is an address; every access through it goes through `unsafe` functions whose
// callers guarantee the row is alive and not mutated concurrently.
unsafe impl Send for RowPtr {}
// SAFETY: see above.
unsafe impl Sync for RowPtr {}

impl RowPtr {
    pub(crate) fn from_raw(ptr: *mut u8) -> RowPtr {
        RowPtr(NonNull::new(ptr).expect("row pointers are non-null"))
    }

    pub fn as_ptr(self) -> *mut u8 {
        self.0.as_ptr()
    }

    /// The aggregate state `i` of this row.
    ///
    /// # Safety
    ///
    /// The row must be alive and laid out with `layout`.
    pub unsafe fn aggregate_state(self, layout: &TupleDataLayout, i: usize) -> *mut u8 {
        // SAFETY: the offset lies within the row, which the caller guarantees is alive.
        unsafe { self.as_ptr().add(layout.aggregate_offset(i)) }
    }
}

/// Reads validity bit `column` of the row at `row`.
///
/// # Safety
///
/// `row` must point at a live row whose layout has at least `column + 1` validity bits.
#[inline]
pub(crate) unsafe fn row_is_valid(row: *const u8, column: usize) -> bool {
    // SAFETY: the validity bytes are at the start of the row.
    unsafe { *row.add(column / 8) & (1 << (column % 8)) != 0 }
}

/// Rows stored in DuckDB's row format (`TupleDataCollection`): the input to hash tables,
/// aggregation and sorting.
pub struct TupleDataCollection {
    layout: TupleDataLayout,
    blocks: Vec<Box<[u64]>>,
    rows_in_last: usize,
    rows_per_block: usize,
    count: usize,
    strings: Arc<StringBuffer>,
}

impl std::fmt::Debug for TupleDataCollection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TupleDataCollection({} rows, {} blocks)",
            self.count,
            self.blocks.len()
        )
    }
}

fn check_supported(layout: &TupleDataLayout) -> Result<()> {
    for ty in layout.types() {
        match ty.physical_type() {
            PhysicalType::List | PhysicalType::Array => {
                return Err(Error::not_implemented(format!(
                    "{ty} columns in rows are not supported yet"
                )));
            }
            PhysicalType::Struct => {
                let children: Vec<_> = ty.struct_children().iter().map(|(_, t)| t.clone()).collect();
                check_supported(&TupleDataLayout::new(children, vec![], super::RowValidity::CanHaveNull))?;
            }
            _ => {}
        }
    }
    Ok(())
}

impl TupleDataCollection {
    pub fn new(layout: TupleDataLayout) -> Result<Self> {
        check_supported(&layout)?;
        let rows_per_block = (BLOCK_SIZE / layout.row_width().max(1)).max(1);
        Ok(Self {
            layout,
            blocks: Vec::new(),
            rows_in_last: 0,
            rows_per_block,
            count: 0,
            strings: Arc::default(),
        })
    }

    pub fn layout(&self) -> &TupleDataLayout {
        &self.layout
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Reserves space for one row and returns its address; the row's bytes are zeroed.
    fn allocate_row(&mut self) -> RowPtr {
        if self.blocks.is_empty() || self.rows_in_last == self.rows_per_block {
            let words = (self.rows_per_block * self.layout.row_width()).div_ceil(8);
            self.blocks.push(vec![0u64; words.max(1)].into_boxed_slice());
            self.rows_in_last = 0;
        }
        let block = self.blocks.last_mut().expect("just ensured");
        let offset = self.rows_in_last * self.layout.row_width();
        self.rows_in_last += 1;
        self.count += 1;
        // SAFETY: offset + row_width fits within the block, which was sized for rows_per_block rows.
        let ptr = unsafe { block.as_mut_ptr().cast::<u8>().add(offset) };
        RowPtr(NonNull::new(ptr).expect("block pointers are non-null"))
    }

    /// Appends the rows of `chunk` (whose columns must match the layout's types) and returns
    /// their addresses, in order (`TupleDataCollection::Append`). Aggregate states are zeroed.
    pub fn append(&mut self, chunk: &DataChunk) -> Vec<RowPtr> {
        assert_eq!(chunk.types(), self.layout.types(), "appended chunk has the wrong types");
        let rows: Vec<RowPtr> = (0..chunk.len()).map(|_| self.allocate_row()).collect();
        let strings = self.strings.clone();
        for row in &rows {
            // SAFETY: freshly allocated rows of this layout.
            unsafe { std::ptr::write_bytes(row.as_ptr(), 0xFF, self.layout.flag_width()) };
        }
        for (column, vector) in chunk.columns().iter().enumerate() {
            // SAFETY: the rows were just allocated with this collection's layout.
            unsafe { scatter(&self.layout, column, vector, &rows, chunk.len(), &strings) };
        }
        rows
    }

    /// Every row address, in insertion order.
    pub fn row_pointers(&self) -> Vec<RowPtr> {
        let mut out = Vec::with_capacity(self.count);
        let width = self.layout.row_width();
        for (b, block) in self.blocks.iter().enumerate() {
            let rows = if b + 1 == self.blocks.len() {
                self.rows_in_last
            } else {
                self.rows_per_block
            };
            let base = block.as_ptr().cast::<u8>().cast_mut();
            for r in 0..rows {
                // SAFETY: r < rows, so the offset lies within the block.
                let ptr = unsafe { base.add(r * width) };
                out.push(RowPtr(NonNull::new(ptr).expect("block pointers are non-null")));
            }
        }
        out
    }

    /// Reads column `column` of `rows` into `target` (`TupleDataCollection::Gather`). `target`
    /// becomes a flat vector of `rows.len()` values.
    ///
    /// # Safety
    ///
    /// Every row must belong to this collection (so it is alive and uses its layout).
    pub unsafe fn gather(&self, rows: &[RowPtr], column: usize, target: &mut Vector) {
        target.prepare_flat(rows.len());
        // SAFETY: forwarded to the caller.
        unsafe { gather(&self.layout, column, rows, target, &self.strings) };
    }

    /// Reads every row back as chunks of up to [`STANDARD_VECTOR_SIZE`] rows.
    pub fn scan(&self) -> impl Iterator<Item = DataChunk> + '_ {
        let rows = self.row_pointers();
        let types = self.layout.types().to_vec();
        (0..rows.len().div_ceil(STANDARD_VECTOR_SIZE)).map(move |c| {
            let batch = &rows[c * STANDARD_VECTOR_SIZE..((c + 1) * STANDARD_VECTOR_SIZE).min(rows.len())];
            let mut chunk = DataChunk::with_capacity(&types, batch.len().max(1));
            for column in 0..types.len() {
                // SAFETY: the rows came from row_pointers() of this collection.
                unsafe { self.gather(batch, column, chunk.column_mut(column)) };
            }
            chunk.set_len(batch.len());
            chunk
        })
    }

    /// Frees every row. Previously returned [`RowPtr`]s become dangling.
    pub fn reset(&mut self) {
        self.blocks.clear();
        self.rows_in_last = 0;
        self.count = 0;
        self.strings = Arc::default();
    }
}

/// Writes column `column` of `vector` into `rows` (`TupleDataScatter`).
///
/// # Safety
///
/// Every row must be alive and laid out with `layout`, with its validity bytes initialized.
unsafe fn scatter(
    layout: &TupleDataLayout,
    column: usize,
    vector: &Vector,
    rows: &[RowPtr],
    count: usize,
    strings: &StringBuffer,
) {
    let offset = layout.offsets()[column];
    let ty = &layout.types()[column];
    if ty.physical_type() == PhysicalType::Struct {
        let mut flat = vector.clone();
        flat.flatten(count);
        let child_layout = layout.struct_layout(column);
        // SAFETY: the struct occupies child_layout.row_width() bytes at `offset` in each row.
        let child_rows = unsafe { nested_rows(rows, offset) };
        for (i, row) in rows.iter().enumerate() {
            // SAFETY: rows are alive (caller) and the bytes are in bounds.
            unsafe {
                std::ptr::write_bytes(child_rows[i].as_ptr(), 0xFF, child_layout.flag_width());
                if !flat.is_valid(i) {
                    set_invalid(row.as_ptr(), column);
                }
            }
        }
        for (c, child) in flat.struct_children().iter().enumerate() {
            // SAFETY: child rows are nested rows of child_layout.
            unsafe { scatter(child_layout, c, child, &child_rows, count, strings) };
        }
        return;
    }
    let u = vector.to_unified(count);
    macro_rules! write {
        ($t:ty) => {{
            let data = u.data::<$t>();
            for (i, row) in rows.iter().enumerate() {
                let idx = u.sel.get(i);
                // SAFETY: `offset + size_of::<$t>()` lies within the row; rows need not be aligned.
                unsafe {
                    if u.validity.is_valid(idx) {
                        row.as_ptr().add(offset).cast::<$t>().write_unaligned(data[idx]);
                    } else {
                        set_invalid(row.as_ptr(), column);
                    }
                }
            }
        }};
    }
    match ty.physical_type() {
        PhysicalType::Bool => write!(bool),
        PhysicalType::Int8 => write!(i8),
        PhysicalType::Int16 => write!(i16),
        PhysicalType::Int32 => write!(i32),
        PhysicalType::Int64 => write!(i64),
        PhysicalType::Int128 => write!(i128),
        PhysicalType::UInt8 => write!(u8),
        PhysicalType::UInt16 => write!(u16),
        PhysicalType::UInt32 => write!(u32),
        PhysicalType::UInt64 => write!(u64),
        PhysicalType::UInt128 => write!(u128),
        PhysicalType::Float => write!(f32),
        PhysicalType::Double => write!(f64),
        PhysicalType::Interval => write!(Interval),
        PhysicalType::Varchar => {
            for (i, row) in rows.iter().enumerate() {
                // SAFETY: as above; long strings are copied into the collection's own buffer.
                unsafe {
                    if u.is_valid(i) {
                        let s = strings.add(u.str_at(i));
                        row.as_ptr().add(offset).cast::<StringT>().write_unaligned(s);
                    } else {
                        set_invalid(row.as_ptr(), column);
                    }
                }
            }
        }
        other => unreachable!("{other} columns are rejected by check_supported"),
    }
}

/// The nested rows of a STRUCT column stored at `offset` in each of `rows`.
///
/// # Safety
///
/// `offset` must lie within every row.
unsafe fn nested_rows(rows: &[RowPtr], offset: usize) -> Vec<RowPtr> {
    rows.iter()
        // SAFETY: forwarded to the caller.
        .map(|r| RowPtr::from_raw(unsafe { r.as_ptr().add(offset) }))
        .collect()
}

/// Clears validity bit `column`.
///
/// # Safety
///
/// `row` must point at a live row with at least `column + 1` validity bits.
unsafe fn set_invalid(row: *mut u8, column: usize) {
    // SAFETY: forwarded to the caller.
    unsafe { *row.add(column / 8) &= !(1 << (column % 8)) };
}

/// Reads column `column` of `rows` into the flat vector `target`, which has room for them.
///
/// # Safety
///
/// Every row must be alive and laid out with `layout`; its long strings must live in `strings`.
unsafe fn gather(
    layout: &TupleDataLayout,
    column: usize,
    rows: &[RowPtr],
    target: &mut Vector,
    strings: &Arc<StringBuffer>,
) {
    let offset = layout.offsets()[column];
    let ty = layout.types()[column].clone();
    for (i, row) in rows.iter().enumerate() {
        // SAFETY: forwarded to the caller.
        let valid = !layout.can_have_null() || unsafe { row_is_valid(row.as_ptr(), column) };
        if !valid {
            target.set_null(i, true);
        }
    }
    if ty.physical_type() == PhysicalType::Struct {
        let child_layout = layout.struct_layout(column);
        // SAFETY: the struct occupies child_layout.row_width() bytes at `offset` in each row.
        let child_rows = unsafe { nested_rows(rows, offset) };
        for (c, child) in target.struct_children_mut().iter_mut().enumerate() {
            child.prepare_flat(rows.len());
            // SAFETY: nested rows of child_layout.
            unsafe { gather(child_layout, c, &child_rows, child, strings) };
        }
        return;
    }
    macro_rules! read {
        ($t:ty) => {{
            let out = target.data_mut::<$t>();
            for (i, row) in rows.iter().enumerate() {
                // SAFETY: in-bounds unaligned read of a value the scatter wrote (or zeroes).
                out[i] = unsafe { row.as_ptr().add(offset).cast::<$t>().read_unaligned() };
            }
        }};
    }
    match ty.physical_type() {
        PhysicalType::Bool => read!(bool),
        PhysicalType::Int8 => read!(i8),
        PhysicalType::Int16 => read!(i16),
        PhysicalType::Int32 => read!(i32),
        PhysicalType::Int64 => read!(i64),
        PhysicalType::Int128 => read!(i128),
        PhysicalType::UInt8 => read!(u8),
        PhysicalType::UInt16 => read!(u16),
        PhysicalType::UInt32 => read!(u32),
        PhysicalType::UInt64 => read!(u64),
        PhysicalType::UInt128 => read!(u128),
        PhysicalType::Float => read!(f32),
        PhysicalType::Double => read!(f64),
        PhysicalType::Interval => read!(Interval),
        PhysicalType::Varchar => {
            target.keep_buffer_alive(strings);
            // SAFETY: the strings are inlined or point into `strings`, now kept alive by target.
            let out = unsafe { target.string_data_mut() };
            for (i, row) in rows.iter().enumerate() {
                // SAFETY: in-bounds unaligned read; zeroed bytes are a valid empty string.
                out[i] = unsafe { row.as_ptr().add(offset).cast::<StringT>().read_unaligned() };
            }
        }
        other => unreachable!("{other} columns are rejected by check_supported"),
    }
}

/// Reads one non-nested value of column `column` from a row, for the row matcher.
///
/// # Safety
///
/// `row` must be alive and laid out with `layout`, and `T` must be the column's storage type.
#[inline]
pub(crate) unsafe fn load<T: Copy>(layout: &TupleDataLayout, row: RowPtr, column: usize) -> T {
    // SAFETY: forwarded to the caller.
    unsafe { row.as_ptr().add(layout.offsets()[column]).cast::<T>().read_unaligned() }
}
