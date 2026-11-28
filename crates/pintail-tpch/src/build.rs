// Ported from duckdb/extension/tpch/dbgen/build.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// Only `mk_sparse` is ported: the DuckDB generator writes rows straight from dbgen.cpp
// (`GenerateOrderLine`, `GenerateSupplier`, ...), so the `mk_*` row builders, `gen_phone`,
// `rpb_routine` (dbgen.cpp's `RetailPrice`) and `mk_time` are not used.

use crate::dss::{DssHuge, SPARSE_BITS, SPARSE_KEEP};

/// `mk_sparse`: the sparse order key of order `i` in update sequence `seq`.
pub fn mk_sparse(i: DssHuge, seq: DssHuge) -> DssHuge {
    let low_bits = i & ((1 << SPARSE_KEEP) - 1);
    let mut ok = i;
    ok >>= SPARSE_KEEP;
    ok <<= SPARSE_BITS;
    ok += seq;
    ok <<= SPARSE_KEEP;
    ok + low_bits
}

#[cfg(test)]
mod tests {
    use super::mk_sparse;

    #[test]
    fn sparse_keys() {
        let keys: Vec<_> = (1..=10).map(|i| mk_sparse(i, 0)).collect();
        assert_eq!(keys, [1, 2, 3, 4, 5, 6, 7, 32, 33, 34]);
    }
}
