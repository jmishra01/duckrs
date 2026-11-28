// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/skip_days.cpp @ rust-port-baseline

//! The sale date of the first row of a fact table (or of this child's part of it).

use super::Ctx;
use super::constants::DATA_START_DATE;
use super::date::strtodt;

impl Ctx {
    /// `skipDays`: the Julian day of this child's first row of `n_table`; sets `remainder` to the
    /// row number of the last row of that day, plus one.
    pub(crate) fn skip_days(&mut self, n_table: i32, remainder: &mut i64) -> i64 {
        let base_date = strtodt(DATA_START_DATE);
        if !self.init.skip_days_init {
            self.init.skip_days_init = true;
            *remainder = 0;
        }
        let mut j_date = i64::from(base_date.julian);
        let mut index: i64 = 1;
        *remainder = self.date_scaling(n_table, j_date) + index;
        let (k_first_row, _, _) = self.split_work(n_table);
        while index < k_first_row {
            let k_day_count = self.date_scaling(n_table, j_date);
            index += k_day_count;
            j_date += 1;
            *remainder = index;
        }
        if j_date > i64::from(base_date.julian) && index >= k_first_row {
            j_date -= 1;
        }
        j_date
    }
}
