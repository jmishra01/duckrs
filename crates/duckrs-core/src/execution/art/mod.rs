// Ported from duckdb/src/execution/index/art @ rust-port-baseline
//
//! The Adaptive Radix Tree behind keys and indexes. Its nodes live in fixed-size buffers that go into
//! the database file as they are, so the port keeps their layout byte for byte.

pub mod allocator;
pub mod key;
pub mod pointer;
pub mod tree;
