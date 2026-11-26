// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/scd.cpp @ rust-port-baseline

//! Slowly changing dimensions: the type 2 tables keep up to three revisions of each business key,
//! with the date range each revision is current in.

use super::Ctx;
use super::build_support::mk_bkey;
use super::constants::{DATA_END_DATE, DATA_START_DATE, DIST_UNIFORM};
use super::date::{mk_date, strtodt};
use super::tables::MAX_TABLE;

/// `arBKeys`: the last business key of each table.
pub(crate) struct State {
    ar_bkeys: Vec<Vec<u8>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            ar_bkeys: vec![Vec::new(); MAX_TABLE as usize],
        }
    }
}

/// The dates the revisions change on: `(jMinimumDataDate, jMaximumDataDate, jH1DataDate,
/// jT1DataDate, jT2DataDate)`.
fn data_dates() -> (i64, i64, i64, i64, i64) {
    // DATA_START_DATE and DATA_END_DATE, without parsing them on every row
    debug_assert_eq!(strtodt(DATA_START_DATE), mk_date(1998, 1, 1));
    debug_assert_eq!(strtodt(DATA_END_DATE), mk_date(2003, 12, 31));
    let minimum = i64::from(mk_date(1998, 1, 1).julian);
    let maximum = i64::from(mk_date(2003, 12, 31).julian);
    let h1 = minimum + (maximum - minimum) / 2;
    let mut t2 = (maximum - minimum) / 3;
    let t1 = minimum + t2;
    t2 += t1;
    (minimum, maximum, h1, t1, t2)
}

/// `changeSCD`: a field of a revision that is not new keeps the previous revision's value when
/// the next bit of `flags` is set; otherwise the value becomes the one to keep.
pub(crate) fn change_scd<T: Clone>(new: &mut T, old: &mut T, flags: &mut i32, first: bool) {
    if *flags != (*flags / 2) * 2 && !first {
        new.clone_from(old);
    } else {
        old.clone_from(new);
    }
    *flags /= 2;
}

/// `changeSCD(SCD_PTR, ...)`: the C code assigns the pointer parameters, not what they point to,
/// so only the flags change.
pub(crate) fn change_scd_ptr(flags: &mut i32) {
    *flags /= 2;
}

impl Ctx {
    /// `setSCDKeys`: the business key and date range of row `k_index` of a type 2 table; returns
    /// whether the row starts a new business key.
    pub(crate) fn set_scd_keys(
        &mut self,
        n_column_id: i32,
        k_index: i64,
        bkey: &mut Vec<u8>,
        begin_date_key: &mut i64,
        end_date_key: &mut i64,
    ) -> bool {
        let (j_min, j_max, j_h1, j_t1, j_t2) = data_dates();
        let n_table_id = self.get_table_from_column(n_column_id);
        let offset = i64::from(n_table_id) * 6;
        let key = &mut self.scd.ar_bkeys[n_table_id as usize];
        let mut new_bkey = false;
        match (k_index % 6) as i32 {
            1 => {
                mk_bkey(key, k_index, n_column_id);
                new_bkey = true;
                *begin_date_key = j_min - offset;
                *end_date_key = -1;
            }
            2 => {
                mk_bkey(key, k_index, n_column_id);
                new_bkey = true;
                *begin_date_key = j_min - offset;
                *end_date_key = j_h1 - offset;
            }
            3 => {
                mk_bkey(key, k_index - 1, n_column_id);
                *begin_date_key = j_h1 - offset + 1;
                *end_date_key = -1;
            }
            4 => {
                mk_bkey(key, k_index, n_column_id);
                new_bkey = true;
                *begin_date_key = j_min - offset;
                *end_date_key = j_t1 - offset;
            }
            5 => {
                mk_bkey(key, k_index - 1, n_column_id);
                *begin_date_key = j_t1 - offset + 1;
                *end_date_key = j_t2 - offset;
            }
            0 => {
                mk_bkey(key, k_index - 2, n_column_id);
                *begin_date_key = j_t2 - offset + 1;
                *end_date_key = -1;
            }
            _ => {}
        }
        if *end_date_key > j_max {
            *end_date_key = -1;
        }
        bkey.clone_from(key);
        new_bkey
    }

    /// `scd_join`: a random revision of a type 2 table that is current on `j_date`.
    pub(crate) fn scd_join(&mut self, tbl: i32, col: i32, j_date: i64) -> i64 {
        let (_, j_max, _, _, _) = data_dates();
        let k_rowcount = self.get_id_count(tbl);
        let mut res = self.genrand_key(DIST_UNIFORM, 1, k_rowcount, 0, col);
        res = self.match_scd_sk(res, j_date, tbl);
        if j_date > j_max {
            res = -1;
        }
        if res > self.get_rowcount(tbl) { -1 } else { res }
    }

    /// `matchSCDSK`: the surrogate key of the revision of business key `k_unique` current on
    /// `j_date`.
    pub(crate) fn match_scd_sk(&mut self, k_unique: i64, j_date: i64, n_table: i32) -> i64 {
        let (_, _, j_h1, j_t1, j_t2) = data_dates();
        let mut k_return = -1;
        match k_unique % 3 {
            1 => {
                k_return = (k_unique / 3) * 6;
                k_return += 1;
            }
            2 => {
                k_return = (k_unique / 3) * 6;
                k_return += 2;
                if j_date > j_h1 {
                    k_return += 1;
                }
            }
            0 => {
                k_return = (k_unique / 3) * 6;
                k_return += -2;
                if j_date > j_t1 {
                    k_return += 1;
                }
                if j_date > j_t2 {
                    k_return += 1;
                }
            }
            _ => {}
        }
        let rowcount = self.get_rowcount(n_table);
        if k_return > rowcount {
            k_return = rowcount;
        }
        k_return
    }
}
