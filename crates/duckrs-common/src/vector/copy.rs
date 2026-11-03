// Ported from duckdb/src/common/vector_operations/vector_copy.cpp (VectorOperations::Copy)
// @ rust-port-baseline

use super::{SelectionVector, Vector};
use crate::types::{Interval, ListEntry, PhysicalType, StringT};

/// How copied strings keep their bytes alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringCopy {
    /// Copy the bytes of long strings into the target's own string buffer. The target then owns
    /// everything it references (as a `ColumnDataCollection` must).
    Deep,
    /// Copy only the 16-byte strings and keep the source's buffer alive from the target.
    Reference,
}

impl Vector {
    /// Copies rows `sel[0..count]` of `source` (any vector type) into rows
    /// `target_offset..target_offset + count` of this flat vector, growing it if needed
    /// (`VectorOperations::Copy`). LIST children are appended to this vector's child.
    pub fn copy_from(
        &mut self,
        source: &Vector,
        sel: &SelectionVector,
        count: usize,
        target_offset: usize,
        strings: StringCopy,
    ) {
        assert_eq!(
            source.logical_type(),
            self.logical_type(),
            "copy between vectors of different types"
        );
        if count == 0 {
            return;
        }
        let end = target_offset + count;
        if self.capacity() < end {
            self.resize(end.next_power_of_two());
        }
        // Struct and array rows live in their children; their unified format carries only
        // validity, so resolve rows through the dictionary/constant layers first.
        let mut flat_source;
        let source = match source.physical_type() {
            PhysicalType::Struct | PhysicalType::Array => {
                flat_source = source.clone();
                flat_source.slice(sel, count);
                flat_source.flatten(count);
                return self.copy_nested(&flat_source, count, target_offset, strings);
            }
            _ => source,
        };
        // The unified view must cover the source rows the selection reads, not the copy count.
        let u = source.to_unified(sel_len(sel, count));
        for i in 0..count {
            let valid = u.validity.is_valid(u.sel.get(sel.get(i)));
            self.validity_mut().set(target_offset + i, valid);
        }
        macro_rules! copy {
            ($t:ty) => {{
                let input = u.data::<$t>();
                let out = self.data_mut::<$t>();
                for i in 0..count {
                    out[target_offset + i] = input[u.sel.get(sel.get(i))];
                }
            }};
        }
        match self.physical_type() {
            PhysicalType::Bool => copy!(bool),
            PhysicalType::Int8 => copy!(i8),
            PhysicalType::Int16 => copy!(i16),
            PhysicalType::Int32 => copy!(i32),
            PhysicalType::Int64 => copy!(i64),
            PhysicalType::Int128 => copy!(i128),
            PhysicalType::UInt8 => copy!(u8),
            PhysicalType::UInt16 => copy!(u16),
            PhysicalType::UInt32 => copy!(u32),
            PhysicalType::UInt64 => copy!(u64),
            PhysicalType::UInt128 => copy!(u128),
            PhysicalType::Float => copy!(f32),
            PhysicalType::Double => copy!(f64),
            PhysicalType::Interval => copy!(Interval),
            PhysicalType::Varchar => match strings {
                StringCopy::Deep => {
                    for i in 0..count {
                        if u.is_valid(sel.get(i)) {
                            self.set_str(target_offset + i, u.str_at(sel.get(i)));
                        }
                    }
                }
                StringCopy::Reference => {
                    self.keep_strings_alive(source);
                    let input = u.data::<StringT>();
                    // SAFETY: the copied strings point into `source`'s string buffer (or are
                    // inlined), which keep_strings_alive just registered with this vector.
                    let out = unsafe { self.string_data_mut() };
                    for i in 0..count {
                        out[target_offset + i] = input[u.sel.get(sel.get(i))];
                    }
                }
            },
            PhysicalType::List => {
                let mut flat = source.clone();
                flat.flatten(sel_len(sel, count));
                let entries: Vec<ListEntry> = (0..count)
                    .map(|i| {
                        let row = sel.get(i);
                        if flat.is_valid(row) {
                            flat.data::<ListEntry>()[row]
                        } else {
                            ListEntry::default()
                        }
                    })
                    .collect();
                let mut offset = self.list_size();
                let child_sel = SelectionVector::from_vec(
                    entries
                        .iter()
                        .flat_map(|e| (e.offset..e.offset + e.length).map(|r| r as u32))
                        .collect(),
                );
                let child_count: usize = entries.iter().map(|e| e.length as usize).sum();
                let start = offset;
                self.set_list_size(start + child_count);
                self.list_child_mut()
                    .copy_from(flat.list_child(), &child_sel, child_count, start, strings);
                let out = self.data_mut::<ListEntry>();
                for (i, e) in entries.iter().enumerate() {
                    out[target_offset + i] = ListEntry::new(offset as u64, e.length);
                    offset += e.length as usize;
                }
            }
            other => unreachable!("vectors of {other} hold no values"),
        }
    }

    fn copy_nested(&mut self, source: &Vector, count: usize, target_offset: usize, strings: StringCopy) {
        for i in 0..count {
            let valid = source.is_valid(i);
            self.validity_mut().set(target_offset + i, valid);
        }
        let identity = SelectionVector::Identity;
        if self.physical_type() == PhysicalType::Struct {
            let sources = source.struct_children();
            for (target, child) in self.struct_children_mut().iter_mut().zip(sources) {
                target.copy_from(child, &identity, count, target_offset, strings);
            }
        } else {
            let size = self.logical_type().array_size().expect("ARRAY has a size") as usize;
            self.array_child_mut().copy_from(
                source.array_child(),
                &identity,
                count * size,
                target_offset * size,
                strings,
            );
        }
    }
}

/// Source rows a selection touches, so non-flat sources are resolved far enough.
fn sel_len(sel: &SelectionVector, count: usize) -> usize {
    match sel {
        SelectionVector::Identity => count,
        _ => (0..count).map(|i| sel.get(i) + 1).max().unwrap_or(0),
    }
}
