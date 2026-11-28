// Ported from duckdb/src/include/duckdb/common/arrow/arrow.hpp @ rust-port-baseline

//! The structs of the Arrow C data interface as a producer hands them to pintail, and safe views of them.
//!
//! What pintail produces is built with arrow-rs (`arrow::ffi`), whose `FFI_ArrowSchema` and `FFI_ArrowArray` have
//! this layout too. What C hands in is read through the structs here instead: C++ reads such arrays as they are,
//! with buffers that need not be aligned, a struct parent without buffers, or children it validates before reading
//! them, and arrow-rs's import assumes none of that. The views check what can be checked (null pointers, counts,
//! negative lengths) and leave the rest to the contract of the interface: a buffer is as long as its array says.

use std::ffi::{CStr, c_char, c_int, c_void};

/// `struct ArrowSchema`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowSchema {
    pub format: *const c_char,
    pub name: *const c_char,
    pub metadata: *const c_char,
    pub flags: i64,
    pub n_children: i64,
    pub children: *mut *mut ArrowSchema,
    pub dictionary: *mut ArrowSchema,
    pub release: Option<unsafe extern "C" fn(*mut ArrowSchema)>,
    pub private_data: *mut c_void,
}

/// `struct ArrowArray`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowArray {
    pub length: i64,
    pub null_count: i64,
    pub offset: i64,
    pub n_buffers: i64,
    pub n_children: i64,
    pub buffers: *mut *const c_void,
    pub children: *mut *mut ArrowArray,
    pub dictionary: *mut ArrowArray,
    pub release: Option<unsafe extern "C" fn(*mut ArrowArray)>,
    pub private_data: *mut c_void,
}

/// `struct ArrowArrayStream`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowArrayStream {
    pub get_schema: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowSchema) -> c_int>,
    pub get_next: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowArray) -> c_int>,
    pub get_last_error: Option<unsafe extern "C" fn(*mut ArrowArrayStream) -> *const c_char>,
    pub release: Option<unsafe extern "C" fn(*mut ArrowArrayStream)>,
    pub private_data: *mut c_void,
}

impl ArrowSchema {
    /// An all-zero schema (`ArrowSchema::Init`), released.
    pub const fn empty() -> Self {
        Self {
            format: std::ptr::null(),
            name: std::ptr::null(),
            metadata: std::ptr::null(),
            flags: 0,
            n_children: 0,
            children: std::ptr::null_mut(),
            dictionary: std::ptr::null_mut(),
            release: None,
            private_data: std::ptr::null_mut(),
        }
    }
}

impl ArrowArray {
    /// An all-zero array (`ArrowArray::Init`), released.
    pub const fn empty() -> Self {
        Self {
            length: 0,
            null_count: 0,
            offset: 0,
            n_buffers: 0,
            n_children: 0,
            buffers: std::ptr::null_mut(),
            children: std::ptr::null_mut(),
            dictionary: std::ptr::null_mut(),
            release: None,
            private_data: std::ptr::null_mut(),
        }
    }
}

fn invalid(message: impl Into<String>) -> pintail::Error {
    pintail::Error::new(pintail::ErrorKind::InvalidInput, message)
}

/// A C string of a schema as text; null is `None`.
///
/// # Safety
///
/// `ptr` is null or a NUL-terminated string that outlives `'a`.
unsafe fn text<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: a non-null string of a schema is NUL-terminated and lives as long as the schema, by the contract of
    // this function.
    Some(unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or(""))
}

/// A schema C passed, borrowed for `'a`.
#[derive(Clone, Copy)]
pub struct SchemaView<'a>(&'a ArrowSchema);

impl<'a> SchemaView<'a> {
    /// # Safety
    ///
    /// `schema` points to a schema that is valid by the Arrow C data interface and stays alive and unchanged for
    /// `'a`; it may be released (`release` null), which the view lets its user check.
    pub unsafe fn new(schema: &'a ArrowSchema) -> Self {
        Self(schema)
    }

    pub fn is_released(&self) -> bool {
        self.0.release.is_none()
    }

    pub fn format(&self) -> &'a str {
        // SAFETY: the format of a valid schema is a NUL-terminated string that lives as long as the schema.
        unsafe { text(self.0.format) }.unwrap_or("")
    }

    pub fn name(&self) -> &'a str {
        // SAFETY: as for the format; the name may be null.
        unsafe { text(self.0.name) }.unwrap_or("")
    }

    pub fn n_children(&self) -> usize {
        usize::try_from(self.0.n_children).unwrap_or(0)
    }

    /// Child `index`, or an error for a child that is not there.
    pub fn child(&self, index: usize) -> pintail::Result<SchemaView<'a>> {
        if index >= self.n_children() || self.0.children.is_null() {
            return Err(invalid("Arrow schema is missing a child"));
        }
        // SAFETY: `children` of a valid schema points to `n_children` pointers, and `index` is below that.
        let child = unsafe { *self.0.children.add(index) };
        // SAFETY: a non-null child of a valid schema is a valid schema that lives as long as its parent.
        match unsafe { child.as_ref() } {
            Some(child) => Ok(SchemaView(child)),
            None => Err(invalid("Arrow schema has a null child")),
        }
    }

    pub fn dictionary(&self) -> Option<SchemaView<'a>> {
        // SAFETY: the dictionary of a valid schema is null or a valid schema that lives as long as its parent.
        unsafe { self.0.dictionary.as_ref() }.map(SchemaView)
    }

    /// The key-value pairs of the metadata (`ArrowSchemaMetadata`): an int32 count, then each key and value as an
    /// int32 length and its bytes.
    pub fn metadata(&self) -> Vec<(String, String)> {
        let ptr = self.0.metadata.cast::<u8>();
        if ptr.is_null() {
            return Vec::new();
        }
        let read = |at: usize, len: usize| -> &'a [u8] {
            // SAFETY: the metadata of a valid schema holds the lengths and bytes it announces, read in order here.
            unsafe { std::slice::from_raw_parts(ptr.add(at), len) }
        };
        let int = |at: usize| i32::from_ne_bytes(read(at, 4).try_into().unwrap_or([0; 4]));
        let mut pairs = Vec::new();
        let count = int(0).max(0);
        let mut at = 4;
        for _ in 0..count {
            let mut entry = [String::new(), String::new()];
            for part in &mut entry {
                let len = int(at).max(0) as usize;
                at += 4;
                *part = String::from_utf8_lossy(read(at, len)).into_owned();
                at += len;
            }
            let [key, value] = entry;
            pairs.push((key, value));
        }
        pairs
    }
}

/// An array C passed, borrowed for `'a`.
#[derive(Clone, Copy)]
pub struct ArrayView<'a>(&'a ArrowArray);

impl<'a> ArrayView<'a> {
    /// # Safety
    ///
    /// `array` points to an array that is valid by the Arrow C data interface (its buffers as long as its length
    /// and type say) and stays alive and unchanged for `'a`.
    pub unsafe fn new(array: &'a ArrowArray) -> Self {
        Self(array)
    }

    pub fn raw(&self) -> &'a ArrowArray {
        self.0
    }

    pub fn len(&self) -> usize {
        usize::try_from(self.0.length).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn offset(&self) -> usize {
        usize::try_from(self.0.offset).unwrap_or(0)
    }

    pub fn null_count(&self) -> i64 {
        self.0.null_count
    }

    pub fn n_children(&self) -> usize {
        usize::try_from(self.0.n_children).unwrap_or(0)
    }

    pub fn n_buffers(&self) -> usize {
        usize::try_from(self.0.n_buffers).unwrap_or(0)
    }

    /// Rejects a negative length or offset.
    pub fn check(&self) -> pintail::Result<()> {
        if self.0.length < 0 || self.0.offset < 0 {
            return Err(invalid("Arrow array has a negative length or offset"));
        }
        Ok(())
    }

    /// Child `index`, which must be there and not be released.
    pub fn child(&self, index: usize) -> pintail::Result<ArrayView<'a>> {
        if index >= self.n_children() || self.0.children.is_null() {
            return Err(invalid("Arrow array is missing a child"));
        }
        // SAFETY: `children` of a valid array points to `n_children` pointers, and `index` is below that.
        let child = unsafe { *self.0.children.add(index) };
        // SAFETY: a non-null child of a valid array is a valid array that lives as long as its parent.
        match unsafe { child.as_ref() } {
            Some(child) if child.release.is_some() => {
                let view = ArrayView(child);
                view.check()?;
                Ok(view)
            }
            _ => Err(invalid("Arrow array child is null or already released")),
        }
    }

    /// Whether child `index` is there, not null and not released (`ValidateArrowArrayShape`).
    pub fn has_child(&self, index: usize) -> bool {
        self.child(index).is_ok()
    }

    pub fn has_children_pointer(&self) -> bool {
        !self.0.children.is_null()
    }

    pub fn dictionary(&self) -> pintail::Result<ArrayView<'a>> {
        // SAFETY: the dictionary of a valid array is null or a valid array that lives as long as its parent.
        match unsafe { self.0.dictionary.as_ref() } {
            Some(dictionary) => {
                let view = ArrayView(dictionary);
                view.check()?;
                Ok(view)
            }
            None => Err(invalid("Arrow dictionary array is missing its dictionary")),
        }
    }

    /// The first `len` bytes of buffer `index`, or `None` for a null buffer (a validity buffer may be null, and any
    /// buffer of no bytes).
    pub fn buffer(&self, index: usize, len: usize) -> pintail::Result<Option<&'a [u8]>> {
        if index >= self.n_buffers() || self.0.buffers.is_null() {
            if len == 0 || index == 0 {
                return Ok(None);
            }
            return Err(invalid(format!("Arrow array is missing buffer {index}")));
        }
        // SAFETY: `buffers` of a valid array points to `n_buffers` pointers, and `index` is below that.
        let ptr = unsafe { *self.0.buffers.add(index) }.cast::<u8>();
        if ptr.is_null() {
            if len == 0 || index == 0 {
                return Ok(None);
            }
            return Err(invalid(format!("Arrow array buffer {index} is null")));
        }
        // SAFETY: a buffer of a valid array holds at least the bytes its length and type call for, which is what the
        // callers ask for; the bytes need no alignment, as they are read as bytes.
        Ok(Some(unsafe { std::slice::from_raw_parts(ptr, len) }))
    }

    /// [`ArrayView::buffer`] that must be there.
    pub fn data(&self, index: usize, len: usize) -> pintail::Result<&'a [u8]> {
        Ok(self.buffer(index, len)?.unwrap_or(&[]))
    }
}
