// Ported from duckdb/src/common/types/vector.cpp and duckdb/src/include/duckdb/common/types/vector.hpp
// @ rust-port-baseline

mod buffer;
mod chunk;
mod copy;
mod selection;
mod validity;

pub use buffer::StringBuffer;
pub use chunk::DataChunk;
pub use copy::StringCopy;
pub use selection::SelectionVector;
pub use validity::ValidityMask;

use crate::types::{Interval, ListEntry, LogicalType, NativeType, PhysicalType, StringT};
use crate::value::{Value, ValueData};
use crate::{Error, ErrorKind, Result};
use buffer::DataBuffer;
use std::sync::Arc;

/// How a vector stores its rows (`VectorType`). FSST and shredded vectors are storage-level
/// encodings that arrive with the storage engine (Phase 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorType {
    /// One value per row.
    Flat,
    /// A single value that every row shares.
    Constant,
    /// Rows select values from a child vector through a [`SelectionVector`].
    Dictionary,
    /// `start + i * increment` (integral types only).
    Sequence,
}

/// The values of a flat or constant vector, plus whatever nested or string data they reference.
#[derive(Debug, Clone)]
struct Flat {
    data: Arc<DataBuffer>,
    validity: ValidityMask,
    aux: Aux,
    capacity: usize,
}

#[derive(Debug, Clone)]
enum Aux {
    None,
    Strings(Arc<StringBuffer>),
    /// LIST/MAP: the entries index into `child`, whose first `size` rows are in use.
    List {
        child: Box<Vector>,
        size: usize,
    },
    Struct(Vec<Vector>),
    /// ARRAY: row `i` is child rows `i * size .. (i + 1) * size`.
    Array(Box<Vector>),
}

#[derive(Debug, Clone)]
enum Repr {
    Flat(Flat),
    Constant(Flat),
    Dictionary { sel: SelectionVector, child: Box<Vector> },
    Sequence { start: i64, increment: i64 },
}

/// A column of values (`Vector`). Cloning is cheap and shares buffers; mutating a shared buffer
/// copies it first, so clones never observe each other's writes.
#[derive(Debug, Clone)]
pub struct Vector {
    ty: LogicalType,
    repr: Repr,
}

impl Flat {
    fn new(ty: &LogicalType, capacity: usize) -> Self {
        let physical = ty.physical_type();
        let aux = match physical {
            PhysicalType::Varchar => Aux::Strings(Arc::default()),
            PhysicalType::List => Aux::List {
                child: Box::new(Vector::new(
                    ty.child_type().expect("LIST has a child type").clone(),
                    capacity,
                )),
                size: 0,
            },
            PhysicalType::Struct => Aux::Struct(
                ty.struct_children()
                    .iter()
                    .map(|(_, t)| Vector::new(t.clone(), capacity))
                    .collect(),
            ),
            PhysicalType::Array => {
                let size = ty.array_size().expect("ARRAY has a size") as usize;
                let child = ty.child_type().expect("ARRAY has a child type").clone();
                Aux::Array(Box::new(Vector::new(child, capacity * size)))
            }
            _ => Aux::None,
        };
        Self {
            data: Arc::new(DataBuffer::zeroed(capacity * physical.size())),
            validity: ValidityMask::new(capacity),
            aux,
            capacity,
        }
    }

    fn grow(&mut self, ty: &LogicalType, capacity: usize) {
        if capacity <= self.capacity {
            return;
        }
        Arc::make_mut(&mut self.data).grow(capacity * ty.physical_type().size());
        self.validity.resize(capacity);
        match &mut self.aux {
            Aux::Struct(children) => children.iter_mut().for_each(|c| c.resize(capacity)),
            Aux::Array(child) => {
                let size = ty.array_size().expect("ARRAY has a size") as usize;
                child.resize(capacity * size);
            }
            Aux::None | Aux::Strings(_) | Aux::List { .. } => {}
        }
        self.capacity = capacity;
    }
}

fn type_error(message: String) -> Error {
    Error::new(ErrorKind::Internal, message)
}

impl Vector {
    /// A flat vector of `capacity` rows, all valid and zeroed.
    pub fn new(ty: LogicalType, capacity: usize) -> Self {
        let flat = Flat::new(&ty, capacity);
        Self {
            ty,
            repr: Repr::Flat(flat),
        }
    }

    /// A constant vector holding `value` (`Vector(Value)`).
    pub fn constant(value: &Value) -> Self {
        let mut v = Self::new(value.logical_type().clone(), 1);
        v.set_value(0, value)
            .expect("a value always fits a vector of its own type");
        v.repr = match v.repr {
            Repr::Flat(f) => Repr::Constant(f),
            other => other,
        };
        v
    }

    /// A constant NULL of type `ty`.
    pub fn constant_null(ty: LogicalType) -> Self {
        Self::constant(&Value::null(ty))
    }

    /// `start + i * increment` for an integral type (`Vector::Sequence`).
    pub fn sequence(ty: LogicalType, start: i64, increment: i64) -> Result<Self> {
        if !ty.is_integral() || ty.physical_type().size() > 8 {
            return Err(type_error(format!("sequence vectors need an integer type, not {ty}")));
        }
        Ok(Self {
            ty,
            repr: Repr::Sequence { start, increment },
        })
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.ty
    }

    pub fn physical_type(&self) -> PhysicalType {
        self.ty.physical_type()
    }

    pub fn vector_type(&self) -> VectorType {
        match self.repr {
            Repr::Flat(_) => VectorType::Flat,
            Repr::Constant(_) => VectorType::Constant,
            Repr::Dictionary { .. } => VectorType::Dictionary,
            Repr::Sequence { .. } => VectorType::Sequence,
        }
    }

    fn flat_ref(&self) -> &Flat {
        match &self.repr {
            Repr::Flat(f) | Repr::Constant(f) => f,
            _ => panic!("expected a flat or constant vector, got {:?}", self.vector_type()),
        }
    }

    fn flat_mut(&mut self) -> &mut Flat {
        match &mut self.repr {
            Repr::Flat(f) | Repr::Constant(f) => f,
            _ => panic!("expected a flat or constant vector"),
        }
    }

    fn check_native<T: NativeType>(&self) {
        assert_eq!(
            T::PHYSICAL,
            self.physical_type(),
            "type mismatch: {} is stored as {}",
            self.ty,
            self.physical_type()
        );
    }

    /// Rows a flat vector can hold without growing.
    pub fn capacity(&self) -> usize {
        self.flat_ref().capacity
    }

    /// Grows a flat vector to hold at least `capacity` rows.
    pub fn resize(&mut self, capacity: usize) {
        let ty = self.ty.clone();
        self.flat_mut().grow(&ty, capacity);
    }

    /// The values of a flat or constant vector (`FlatVector::GetData`).
    pub fn data<T: NativeType>(&self) -> &[T] {
        self.check_native::<T>();
        self.flat_ref().data.as_slice()
    }

    /// Mutable values of a flat or constant vector; copies the buffer first if it is shared.
    ///
    /// Not available for strings: a string written here could point at bytes this vector does
    /// not keep alive. Use [`Self::set_str`], or [`Self::string_data_mut`] in audited code.
    pub fn data_mut<T: NativeType>(&mut self) -> &mut [T] {
        self.check_native::<T>();
        assert!(
            T::PHYSICAL != PhysicalType::Varchar,
            "use set_str or string_data_mut to write strings"
        );
        Arc::make_mut(&mut self.flat_mut().data).as_mut_slice()
    }

    /// Mutable strings of a flat VARCHAR/BLOB vector.
    ///
    /// # Safety
    ///
    /// Every non-inlined string written must point into this vector's [`Self::string_buffer`],
    /// or into a buffer registered with [`Self::keep_strings_alive`].
    pub unsafe fn string_data_mut(&mut self) -> &mut [StringT] {
        self.check_native::<StringT>();
        Arc::make_mut(&mut self.flat_mut().data).as_mut_slice()
    }

    /// Keeps the string bytes of `other` alive for as long as this vector's string buffer, so
    /// strings copied from `other` without copying their bytes stay valid.
    pub fn keep_strings_alive(&mut self, other: &Vector) {
        let target = self.string_buffer();
        let mut source = other;
        while let Repr::Dictionary { child, .. } = &source.repr {
            source = child;
        }
        if let Repr::Flat(f) | Repr::Constant(f) = &source.repr
            && let Aux::Strings(buffer) = &f.aux
        {
            target.add_reference(buffer);
        }
    }

    pub fn validity(&self) -> &ValidityMask {
        &self.flat_ref().validity
    }

    pub fn validity_mut(&mut self) -> &mut ValidityMask {
        &mut self.flat_mut().validity
    }

    /// Whether row `i` is non-NULL, for any vector type.
    pub fn is_valid(&self, i: usize) -> bool {
        match &self.repr {
            Repr::Flat(f) => f.validity.is_valid(i),
            Repr::Constant(f) => f.validity.is_valid(0),
            Repr::Dictionary { sel, child } => child.is_valid(sel.get(i)),
            Repr::Sequence { .. } => true,
        }
    }

    /// Marks row `i` of a flat vector NULL or valid. For STRUCT and ARRAY the children are
    /// marked as well, as `FlatVector::SetNull` does.
    pub fn set_null(&mut self, i: usize, null: bool) {
        let ty = self.ty.clone();
        let flat = self.flat_mut();
        flat.validity.set(i, !null);
        if null {
            match &mut flat.aux {
                Aux::Struct(children) => children.iter_mut().for_each(|c| c.set_null(i, true)),
                Aux::Array(child) => {
                    let size = ty.array_size().expect("ARRAY has a size") as usize;
                    (i * size..(i + 1) * size).for_each(|j| child.set_null(j, true));
                }
                _ => {}
            }
        }
    }

    pub fn is_constant_null(&self) -> bool {
        matches!(&self.repr, Repr::Constant(f) if !f.validity.is_valid(0))
    }

    /// Reinterprets row 0 of a flat vector as a constant (`SetVectorType(CONSTANT_VECTOR)`).
    pub fn set_constant(&mut self) {
        let placeholder = Repr::Sequence { start: 0, increment: 0 };
        match std::mem::replace(&mut self.repr, placeholder) {
            Repr::Flat(f) | Repr::Constant(f) => self.repr = Repr::Constant(f),
            other => {
                self.repr = other;
                panic!("only flat vectors can become constant");
            }
        }
    }

    /// Makes this a writable flat vector of at least `capacity` rows, all valid, ready to be
    /// filled by an executor. Reuses the existing buffer when possible.
    pub fn prepare_flat(&mut self, capacity: usize) {
        let reusable = self.physical_type().is_constant_size()
            && matches!(&self.repr, Repr::Flat(f) | Repr::Constant(f) if Arc::strong_count(&f.data) == 1);
        if !reusable {
            *self = Vector::new(self.ty.clone(), capacity);
            return;
        }
        let placeholder = Repr::Sequence { start: 0, increment: 0 };
        let (Repr::Flat(mut f) | Repr::Constant(mut f)) = std::mem::replace(&mut self.repr, placeholder) else {
            unreachable!("checked above")
        };
        f.validity.set_all_valid();
        f.grow(&self.ty, capacity);
        self.repr = Repr::Flat(f);
    }

    /// Restricts the vector to rows `sel[0..count]` (`Vector::Slice`). Constant vectors stay
    /// constant; dictionaries compose their selections.
    pub fn slice(&mut self, sel: &SelectionVector, count: usize) {
        match &mut self.repr {
            Repr::Constant(_) => {}
            Repr::Dictionary { sel: current, .. } => *current = current.compose(sel, count),
            _ => {
                let child = std::mem::replace(self, Vector::new(LogicalType::SQLNULL, 0));
                self.ty = child.ty.clone();
                self.repr = Repr::Dictionary {
                    sel: sel.clone(),
                    child: Box::new(child),
                };
            }
        }
    }

    /// Converts the first `count` rows to a flat vector in place (`Vector::Flatten`).
    pub fn flatten(&mut self, count: usize) {
        if matches!(self.repr, Repr::Flat(_)) {
            return;
        }
        // `gather` resolves dictionaries and constants itself.
        let flat = gather(self, &SelectionVector::Identity, count);
        self.repr = Repr::Flat(flat);
    }

    /// A row-addressable view of any vector (`Vector::ToUnifiedFormat`). Row `i` lives at
    /// `data[sel.get(i)]` and is NULL unless `validity.is_valid(sel.get(i))`.
    pub fn to_unified(&self, count: usize) -> UnifiedFormat {
        match &self.repr {
            Repr::Flat(f) => UnifiedFormat::from_flat(f, self.physical_type(), SelectionVector::Identity),
            Repr::Constant(f) => UnifiedFormat::from_flat(f, self.physical_type(), SelectionVector::Zero),
            Repr::Dictionary { sel, child } => {
                let child_count = (0..count).map(|i| sel.get(i) + 1).max().unwrap_or(0);
                let mut unified = child.to_unified(child_count);
                unified.sel = unified.sel.compose(sel, count);
                unified
            }
            Repr::Sequence { .. } => {
                let mut flat = self.clone();
                flat.flatten(count);
                flat.to_unified(count)
            }
        }
    }

    /// The string at row `i` of a VARCHAR/BLOB vector, or `None` for NULL.
    pub fn str_at(&self, i: usize) -> Option<&[u8]> {
        match &self.repr {
            Repr::Flat(f) | Repr::Constant(f) => {
                let row = if matches!(self.repr, Repr::Constant(_)) { 0 } else { i };
                if !f.validity.is_valid(row) {
                    return None;
                }
                self.check_native::<StringT>();
                let s = &f.data.as_slice::<StringT>()[row];
                // SAFETY: the string's bytes are inlined or live in a StringBuffer that this
                // vector keeps alive (directly or via keep_alive), and `&self` borrows it.
                Some(unsafe { s.as_bytes() })
            }
            Repr::Dictionary { sel, child } => child.str_at(sel.get(i)),
            Repr::Sequence { .. } => panic!("sequence vectors hold integers"),
        }
    }

    /// The string buffer of a flat VARCHAR/BLOB vector, for writing strings into it.
    pub fn string_buffer(&self) -> Arc<StringBuffer> {
        match &self.flat_ref().aux {
            Aux::Strings(buffer) => buffer.clone(),
            _ => panic!("{} vectors have no string buffer", self.ty),
        }
    }

    /// Copies `bytes` into this vector's string buffer and stores the string at row `i`.
    pub fn set_str(&mut self, i: usize, bytes: &[u8]) {
        let s = self.string_buffer().add(bytes);
        // SAFETY: `s` is inlined or points into this vector's own string buffer.
        unsafe { self.string_data_mut()[i] = s };
        self.validity_mut().set_valid(i);
    }

    /// [`Self::set_str`] of rows `0..` from `strings`, with the buffer and the data taken once.
    pub fn set_strs<'a>(&mut self, strings: impl IntoIterator<Item = &'a [u8]>) {
        let buffer = self.string_buffer();
        let values: Vec<StringT> = strings.into_iter().map(|bytes| buffer.add(bytes)).collect();
        if self.capacity() < values.len() {
            self.resize(values.len());
        }
        // SAFETY: the strings are inlined or point into this vector's own string buffer.
        unsafe { self.string_data_mut()[..values.len()].copy_from_slice(&values) };
        if !self.validity().all_valid() {
            for row in 0..values.len() {
                self.validity_mut().set_valid(row);
            }
        }
    }

    /// Keeps `buffer` alive for as long as this vector's string buffer, so strings pointing into
    /// it may be stored here (through `string_data_mut`).
    pub(crate) fn keep_buffer_alive(&mut self, buffer: &Arc<StringBuffer>) {
        self.string_buffer().add_reference(buffer);
    }

    /// The child of a flat LIST/MAP vector (`ListVector::GetEntry`).
    pub fn list_child(&self) -> &Vector {
        match &self.flat_ref().aux {
            Aux::List { child, .. } => child,
            _ => panic!("{} is not a LIST", self.ty),
        }
    }

    pub fn list_child_mut(&mut self) -> &mut Vector {
        match &mut self.flat_mut().aux {
            Aux::List { child, .. } => child,
            _ => panic!("not a LIST"),
        }
    }

    /// Child rows in use (`ListVector::GetListSize`).
    pub fn list_size(&self) -> usize {
        match &self.flat_ref().aux {
            Aux::List { size, .. } => *size,
            _ => panic!("{} is not a LIST", self.ty),
        }
    }

    /// Sets the child rows in use, growing the child if needed (`ListVector::SetListSize`).
    pub fn set_list_size(&mut self, new_size: usize) {
        match &mut self.flat_mut().aux {
            Aux::List { child, size } => {
                if child.capacity() < new_size {
                    child.resize(new_size.next_power_of_two());
                }
                *size = new_size;
            }
            _ => panic!("not a LIST"),
        }
    }

    /// The children of a flat STRUCT vector (`StructVector::GetEntries`).
    pub fn struct_children(&self) -> &[Vector] {
        match &self.flat_ref().aux {
            Aux::Struct(children) => children,
            _ => panic!("{} is not a STRUCT", self.ty),
        }
    }

    pub fn struct_children_mut(&mut self) -> &mut [Vector] {
        match &mut self.flat_mut().aux {
            Aux::Struct(children) => children,
            _ => panic!("not a STRUCT"),
        }
    }

    /// The child of a flat ARRAY vector (`ArrayVector::GetEntry`).
    pub fn array_child(&self) -> &Vector {
        match &self.flat_ref().aux {
            Aux::Array(child) => child,
            _ => panic!("{} is not an ARRAY", self.ty),
        }
    }

    pub fn array_child_mut(&mut self) -> &mut Vector {
        match &mut self.flat_mut().aux {
            Aux::Array(child) => child,
            _ => panic!("not an ARRAY"),
        }
    }

    /// Reads row `i` as a [`Value`] (`Vector::GetValue`).
    pub fn get_value(&self, i: usize) -> Value {
        let (flat, row) = match &self.repr {
            Repr::Flat(f) => (f, i),
            Repr::Constant(f) => (f, 0),
            Repr::Dictionary { sel, child } => return child.get_value(sel.get(i)),
            Repr::Sequence { start, increment } => {
                let v = start + increment * i as i64;
                let data = match self.physical_type() {
                    PhysicalType::Int8 => ValueData::Int8(v as i8),
                    PhysicalType::Int16 => ValueData::Int16(v as i16),
                    PhysicalType::Int32 => ValueData::Int32(v as i32),
                    PhysicalType::Int64 => ValueData::Int64(v),
                    PhysicalType::UInt8 => ValueData::UInt8(v as u8),
                    PhysicalType::UInt16 => ValueData::UInt16(v as u16),
                    PhysicalType::UInt32 => ValueData::UInt32(v as u32),
                    PhysicalType::UInt64 => ValueData::UInt64(v as u64),
                    other => unreachable!("sequence of {other}"),
                };
                return Value::new(self.ty.clone(), data).expect("matching physical type");
            }
        };
        if !flat.validity.is_valid(row) {
            return Value::null(self.ty.clone());
        }
        let d = &flat.data;
        let data = match self.physical_type() {
            PhysicalType::Bool => ValueData::Bool(d.as_slice::<bool>()[row]),
            PhysicalType::Int8 => ValueData::Int8(d.as_slice::<i8>()[row]),
            PhysicalType::Int16 => ValueData::Int16(d.as_slice::<i16>()[row]),
            PhysicalType::Int32 => ValueData::Int32(d.as_slice::<i32>()[row]),
            PhysicalType::Int64 => ValueData::Int64(d.as_slice::<i64>()[row]),
            PhysicalType::Int128 => ValueData::Int128(d.as_slice::<i128>()[row]),
            PhysicalType::UInt8 => ValueData::UInt8(d.as_slice::<u8>()[row]),
            PhysicalType::UInt16 => ValueData::UInt16(d.as_slice::<u16>()[row]),
            PhysicalType::UInt32 => ValueData::UInt32(d.as_slice::<u32>()[row]),
            PhysicalType::UInt64 => ValueData::UInt64(d.as_slice::<u64>()[row]),
            PhysicalType::UInt128 => ValueData::UInt128(d.as_slice::<u128>()[row]),
            PhysicalType::Float => ValueData::Float(d.as_slice::<f32>()[row]),
            PhysicalType::Double => ValueData::Double(d.as_slice::<f64>()[row]),
            PhysicalType::Interval => ValueData::Interval(d.as_slice::<Interval>()[row]),
            PhysicalType::Varchar => ValueData::Bytes(self.str_at(i).expect("row is valid").to_vec()),
            PhysicalType::List => {
                let entry = d.as_slice::<ListEntry>()[row];
                let child = self.list_child();
                ValueData::List(
                    (entry.offset..entry.offset + entry.length)
                        .map(|j| child.get_value(j as usize))
                        .collect(),
                )
            }
            PhysicalType::Struct => {
                ValueData::Struct(self.struct_children().iter().map(|c| c.get_value(row)).collect())
            }
            PhysicalType::Array => {
                let size = self.ty.array_size().expect("ARRAY has a size") as usize;
                let child = self.array_child();
                ValueData::List((row * size..(row + 1) * size).map(|j| child.get_value(j)).collect())
            }
            other => unreachable!("vectors of {other} hold no values"),
        };
        Value::new(self.ty.clone(), data).expect("data read with the vector's own physical type")
    }

    /// Writes `value` at row `i` of a flat vector (`Vector::SetValue`). The value's type must
    /// equal the vector's type; casting is the caller's job.
    pub fn set_value(&mut self, i: usize, value: &Value) -> Result<()> {
        if value.is_null() {
            self.set_null(i, true);
            return Ok(());
        }
        if value.logical_type() != &self.ty {
            return Err(type_error(format!(
                "cannot store a {} value in a {} vector",
                value.logical_type(),
                self.ty
            )));
        }
        macro_rules! put {
            ($t:ty, $v:expr) => {
                self.data_mut::<$t>()[i] = $v
            };
        }
        match value.data() {
            ValueData::Null => unreachable!("handled above"),
            ValueData::Bool(v) => put!(bool, *v),
            ValueData::Int8(v) => put!(i8, *v),
            ValueData::Int16(v) => put!(i16, *v),
            ValueData::Int32(v) => put!(i32, *v),
            ValueData::Int64(v) => put!(i64, *v),
            ValueData::Int128(v) => put!(i128, *v),
            ValueData::UInt8(v) => put!(u8, *v),
            ValueData::UInt16(v) => put!(u16, *v),
            ValueData::UInt32(v) => put!(u32, *v),
            ValueData::UInt64(v) => put!(u64, *v),
            ValueData::UInt128(v) => put!(u128, *v),
            ValueData::Float(v) => put!(f32, *v),
            ValueData::Double(v) => put!(f64, *v),
            ValueData::Interval(v) => put!(Interval, *v),
            ValueData::Bytes(b) => {
                self.set_str(i, b);
                return Ok(());
            }
            ValueData::List(items) if self.physical_type() == PhysicalType::Array => {
                let size = self.ty.array_size().expect("ARRAY has a size") as usize;
                let child = self.array_child_mut();
                for (k, item) in items.iter().enumerate() {
                    child.set_value(i * size + k, item)?;
                }
            }
            ValueData::List(items) => {
                let offset = self.list_size();
                self.set_list_size(offset + items.len());
                let child = self.list_child_mut();
                for (k, item) in items.iter().enumerate() {
                    child.set_value(offset + k, item)?;
                }
                put!(ListEntry, ListEntry::new(offset as u64, items.len() as u64));
            }
            ValueData::Struct(fields) => {
                for (child, field) in self.struct_children_mut().iter_mut().zip(fields) {
                    child.set_value(i, field)?;
                }
            }
        }
        self.validity_mut().set_valid(i);
        Ok(())
    }
}

/// Copies the rows `sel[0..count]` of `source` into a new flat buffer.
fn gather(source: &Vector, sel: &SelectionVector, count: usize) -> Flat {
    let ty = &source.ty;
    if let Repr::Sequence { start, increment } = source.repr {
        let mut flat = Flat::new(ty, count);
        let data = Arc::make_mut(&mut flat.data);
        macro_rules! fill {
            ($t:ty) => {{
                let out = data.as_mut_slice::<$t>();
                for i in 0..count {
                    out[i] = (start + increment * sel.get(i) as i64) as $t;
                }
            }};
        }
        match ty.physical_type() {
            PhysicalType::Int8 => fill!(i8),
            PhysicalType::Int16 => fill!(i16),
            PhysicalType::Int32 => fill!(i32),
            PhysicalType::Int64 => fill!(i64),
            PhysicalType::UInt8 => fill!(u8),
            PhysicalType::UInt16 => fill!(u16),
            PhysicalType::UInt32 => fill!(u32),
            PhysicalType::UInt64 => fill!(u64),
            other => unreachable!("sequence of {other}"),
        }
        return flat;
    }
    // Resolve dictionaries down to the flat/constant vector holding the values.
    let (base, sel) = resolve(source, sel, count);
    let src = base.flat_ref();
    let row_sel = if matches!(base.repr, Repr::Constant(_)) {
        SelectionVector::Zero
    } else {
        sel
    };
    let mut flat = Flat::new(&LogicalType::SQLNULL, 0);
    flat.capacity = count;
    flat.validity = ValidityMask::new(count);
    if !src.validity.all_valid() {
        for i in 0..count {
            if !src.validity.is_valid(row_sel.get(i)) {
                flat.validity.set_invalid(i);
            }
        }
    }
    let physical = ty.physical_type();
    let mut data = DataBuffer::zeroed(count * physical.size());
    macro_rules! copy {
        ($t:ty) => {{
            let input = src.data.as_slice::<$t>();
            let out = data.as_mut_slice::<$t>();
            for i in 0..count {
                out[i] = input[row_sel.get(i)];
            }
        }};
    }
    match physical {
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
        // The copied strings point into the source's buffer, which the shared Aux keeps alive.
        PhysicalType::Varchar => copy!(StringT),
        // Entries keep indexing the same (shared) child vector.
        PhysicalType::List => copy!(ListEntry),
        PhysicalType::Struct | PhysicalType::Array => {}
        other => unreachable!("vectors of {other} hold no values"),
    }
    flat.data = Arc::new(data);
    flat.aux = match &src.aux {
        Aux::Struct(children) => Aux::Struct(
            children
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    if matches!(base.repr, Repr::Constant(_)) {
                        c.slice(&SelectionVector::Zero, count);
                    } else {
                        c.slice(&row_sel, count);
                    }
                    c.flatten(count);
                    c
                })
                .collect(),
        ),
        Aux::Array(child) => {
            let size = ty.array_size().expect("ARRAY has a size") as usize;
            let child_sel = SelectionVector::from_vec(
                (0..count * size)
                    .map(|j| (row_sel.get(j / size) * size + j % size) as u32)
                    .collect(),
            );
            let mut c = (**child).clone();
            c.slice(&child_sel, count * size);
            c.flatten(count * size);
            Aux::Array(Box::new(c))
        }
        other => other.clone(),
    };
    flat
}

/// Follows dictionary vectors down to the vector that holds the values, composing selections.
fn resolve<'a>(v: &'a Vector, sel: &SelectionVector, count: usize) -> (&'a Vector, SelectionVector) {
    match &v.repr {
        Repr::Dictionary { sel: inner, child } => resolve(child, &inner.compose(sel, count), count),
        _ => (v, sel.clone()),
    }
}

/// A row-addressable view of a vector, whatever its [`VectorType`] (`UnifiedVectorFormat`).
#[derive(Debug, Clone)]
pub struct UnifiedFormat {
    pub sel: SelectionVector,
    pub validity: ValidityMask,
    physical: PhysicalType,
    data: Arc<DataBuffer>,
    // Keeps string bytes alive while the format is in use.
    _strings: Option<Arc<StringBuffer>>,
}

impl UnifiedFormat {
    fn from_flat(f: &Flat, physical: PhysicalType, sel: SelectionVector) -> Self {
        Self {
            sel,
            validity: f.validity.clone(),
            physical,
            data: f.data.clone(),
            _strings: match &f.aux {
                Aux::Strings(s) => Some(s.clone()),
                _ => None,
            },
        }
    }

    /// The underlying values; index them with `sel.get(i)`.
    pub fn data<T: NativeType>(&self) -> &[T] {
        assert_eq!(T::PHYSICAL, self.physical, "type mismatch reading a unified format");
        self.data.as_slice()
    }

    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        self.validity.is_valid(self.sel.get(i))
    }

    /// The string at logical row `i` (which must be valid) of a VARCHAR/BLOB format.
    pub fn str_at(&self, i: usize) -> &[u8] {
        let s = &self.data::<StringT>()[self.sel.get(i)];
        // SAFETY: the format holds the vector's string buffer (`_strings`), which keeps every
        // string's bytes alive, and `&self` borrows it.
        unsafe { s.as_bytes() }
    }
}

#[cfg(test)]
mod tests;
