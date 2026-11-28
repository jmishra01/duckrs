// Ported from duckdb/src/include/duckdb/common/types.hpp (GetTypeId / list_entry_t)
// @ rust-port-baseline

use super::{Date, Interval, PhysicalType, StringT, Time, TimeTz, Timestamp};

/// `list_entry_t`: a list value is a slice `[offset, offset + length)` of the child vector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ListEntry {
    pub offset: u64,
    pub length: u64,
}

impl ListEntry {
    pub const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A Rust type that a flat vector can store directly.
///
/// # Safety
///
/// Implementors must be plain old data: `Copy`, no padding-dependent invariants, and every
/// all-zero bit pattern must be a valid value. Vectors hand out typed slices over zeroed,
/// 16-byte-aligned byte buffers.
pub unsafe trait NativeType: sealed::Sealed + Copy + Default + Send + Sync + 'static {
    /// The physical type this Rust type stores.
    const PHYSICAL: PhysicalType;
}

macro_rules! native {
    ($($t:ty => $p:ident),* $(,)?) => {$(
        impl sealed::Sealed for $t {}
        // SAFETY: primitive (or repr(C) POD) types; all-zero is a valid value.
        unsafe impl NativeType for $t {
            const PHYSICAL: PhysicalType = PhysicalType::$p;
        }
    )*};
}

native! {
    bool => Bool,
    i8 => Int8,
    i16 => Int16,
    i32 => Int32,
    i64 => Int64,
    i128 => Int128,
    u8 => UInt8,
    u16 => UInt16,
    u32 => UInt32,
    u64 => UInt64,
    u128 => UInt128,
    f32 => Float,
    f64 => Double,
    Interval => Interval,
    ListEntry => List,
    StringT => Varchar,
    Date => Int32,
    Time => Int64,
    Timestamp => Int64,
    TimeTz => Int64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_physical_sizes() {
        fn check<T: NativeType>() {
            assert_eq!(size_of::<T>(), T::PHYSICAL.size(), "{}", T::PHYSICAL);
            assert!(align_of::<T>() <= 16);
        }
        check::<bool>();
        check::<i8>();
        check::<i16>();
        check::<i32>();
        check::<i64>();
        check::<i128>();
        check::<u128>();
        check::<f32>();
        check::<f64>();
        check::<Interval>();
        check::<ListEntry>();
        check::<StringT>();
    }
}
