// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/nulls.cpp @ rust-port-baseline

//! Which columns of a row are NULL.

use super::Ctx;
use super::constants::{DIST_UNIFORM, MAXINT};

impl Ctx {
    /// `nullSet(&getSimpleTdefsByNumber(table)->kNullBitMap, stream)`: draws the null bitmap of the
    /// next row of `table`. The bits are column numbers counted from the table's first column.
    pub(crate) fn null_set(&mut self, table: i32, stream: i32) {
        let last_table = self.get_table_from_column(stream);
        let (n_null_pct, k_not_null_bit_map) = {
            let t = self.tdef(last_table);
            (t.n_null_pct, t.k_not_null_bit_map)
        };
        let n_threshold = self.genrand_integer(DIST_UNIFORM, 0, 9999, 0, stream);
        let k_bit_map = self.genrand_key(DIST_UNIFORM, 1, MAXINT, 0, stream);
        let mut dest = 0;
        if n_threshold < n_null_pct {
            dest = k_bit_map & !k_not_null_bit_map;
        }
        self.tdef_mut(table).k_null_bit_map = dest;
    }
}
