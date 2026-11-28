// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/parallel.cpp @ rust-port-baseline

//! Keeping the random streams in step from row to row, and splitting a table between parallel
//! children.

use super::Ctx;
use super::constants::DIST_UNIFORM;
use super::tdefs::FL_PARENT;

impl Ctx {
    /// `split_work`: the first row and the row count of this child's part of a table, and whether
    /// the table is generated (0 when it is too small to split and this is not the first child).
    pub(crate) fn split_work(&mut self, tnum: i32) -> (i64, i64, i32) {
        let k_total_rows = self.get_rowcount(tnum);
        let n_parallel = i64::from(self.params.parallel);
        let n_child = i64::from(self.params.child);
        if k_total_rows < 1000000 {
            if n_child > 1 {
                return (1, 0, 0);
            }
            return (1, k_total_rows, 1);
        }
        if !self.params.parallel_set {
            return (1, k_total_rows, 1);
        }
        let k_extra_rows = k_total_rows % n_parallel;
        let k_rowset_size = (k_total_rows - k_extra_rows) / n_parallel;
        let mut first_row = 1 + (n_child - 1) * k_rowset_size;
        if k_extra_rows != 0 && (n_child - 1) != 0 {
            first_row += if (n_child - 1) < k_extra_rows {
                n_child - 1
            } else {
                k_extra_rows
            };
        }
        let mut row_count = k_rowset_size;
        if k_extra_rows != 0 && n_child <= k_extra_rows {
            row_count += 1;
        }
        (first_row, row_count, 1)
    }

    /// `checkSeeds`: draws from every stream of a table until it has used its numbers for the row.
    fn check_seeds(&mut self, first_column: i32, last_column: i32) {
        for i in first_column..=last_column {
            while self.streams[i as usize].n_used < self.streams[i as usize].n_used_per_row {
                self.genrand_integer(DIST_UNIFORM, 1, 100, 0, i);
            }
            self.streams[i as usize].n_used = 0;
        }
    }

    /// `row_stop`: ends a row of `tbl` and of its child tables.
    pub(crate) fn row_stop(&mut self, tbl: i32) {
        let mut t = *self.tdef(tbl);
        self.check_seeds(t.n_first_column, t.n_last_column);
        if t.flags & FL_PARENT != 0 {
            t = *self.tdef(t.n_param);
            self.check_seeds(t.n_first_column, t.n_last_column);
            if t.flags & FL_PARENT != 0 {
                t = *self.tdef(t.n_param);
                self.check_seeds(t.n_first_column, t.n_last_column);
            }
        }
    }
}
