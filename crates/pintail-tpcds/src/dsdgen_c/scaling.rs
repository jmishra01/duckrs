// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/scaling.cpp @ rust-port-baseline

//! Row counts by scale factor, and the number of sales per day.

use super::Ctx;
use super::constants::{CALENDAR_SALES, CALENDAR_UNIFORM};
use super::date::{day_number, is_leap, jtodt};
use super::tables::*;
use super::tdefs::FL_TYPE_2;
use super::w_inventory::sc_w_inventory;

const AR_SCALE_VOLUME: [i32; 9] = [1, 10, 100, 300, 1000, 3000, 10000, 30000, 100000];

/// The statics of `scaling.cpp`.
#[derive(Default)]
pub(crate) struct State {
    /// `arRowcount[].kBaseRowcount`
    base_rowcount: Vec<i64>,
}

/// `getScaleSlot`
fn get_scale_slot(n_target_gb: i32) -> usize {
    let mut i = 0;
    while n_target_gb > AR_SCALE_VOLUME[i] {
        i += 1;
    }
    i
}

impl Ctx {
    /// `LogScale`
    fn log_scale(&self, n_table: i32, n_target_gb: i32) -> i64 {
        let i = get_scale_slot(n_target_gb);
        let n_delta = self.dist_weight("rowcounts", n_table + 1, i as i32 + 1)
            - self.dist_weight("rowcounts", n_table + 1, i as i32);
        let f_offset =
            (n_target_gb - AR_SCALE_VOLUME[i - 1]) as f32 / (AR_SCALE_VOLUME[i] - AR_SCALE_VOLUME[i - 1]) as f32;
        let mut hg_rowcount = i64::from((f_offset * n_delta as f32) as i32);
        hg_rowcount += i64::from(self.dist_weight("rowcounts", n_table + 1, i as i32));
        hg_rowcount
    }

    /// `StaticScale`
    fn static_scale(&self, n_table: i32) -> i64 {
        i64::from(self.dist_weight("rowcounts", n_table + 1, 1))
    }

    /// `LinearScale`
    fn linear_scale(&self, n_table: i32, mut n_target_gb: i32) -> i64 {
        let mut hg_rowcount = 0i64;
        for i in (0..=8).rev() {
            while n_target_gb >= AR_SCALE_VOLUME[i] {
                hg_rowcount += i64::from(self.dist_weight("rowcounts", n_table + 1, i as i32 + 1));
                n_target_gb -= AR_SCALE_VOLUME[i];
            }
        }
        hg_rowcount
    }

    /// `getIDCount`: the number of distinct business keys of a table.
    pub(crate) fn get_id_count(&mut self, n_table: i32) -> i64 {
        let k_rowcount = self.get_rowcount(n_table);
        if n_table >= PSEUDO_TABLE_START {
            return k_rowcount;
        }
        if self.tdef(n_table).flags & FL_TYPE_2 != 0 {
            let mut k_unique_count = (k_rowcount / 6) * 3;
            match k_rowcount % 6 {
                1 => k_unique_count += 1,
                2 | 3 => k_unique_count += 2,
                4 | 5 => k_unique_count += 3,
                _ => {}
            }
            k_unique_count
        } else {
            k_rowcount
        }
    }

    /// `get_rowcount`
    pub(crate) fn get_rowcount(&mut self, table: i32) -> i64 {
        if !self.init.get_rowcount_init {
            let n_scale = self.params.scale;
            if n_scale > 100000.0 {
                panic!("dsdgen: the scale factor exceeds 100000");
            }
            let mut rowcounts = vec![0i64; MAX_TABLE as usize + 1];
            let i_scale = if n_scale < 1.0 { 1 } else { n_scale as i32 };
            for n_table in CALL_CENTER..=MAX_TABLE {
                let weight_set = match i_scale {
                    100000 => Some(9),
                    30000 => Some(8),
                    10000 => Some(7),
                    3000 => Some(6),
                    1000 => Some(5),
                    300 => Some(4),
                    100 => Some(3),
                    10 => Some(2),
                    1 => Some(1),
                    _ => None,
                };
                let mut base = match weight_set {
                    Some(set) => i64::from(self.dist_weight("rowcounts", n_table + 1, set)),
                    None => match self.dist_member_int("rowcounts", n_table + 1, 3) {
                        2 => self.linear_scale(n_table, n_scale as i32),
                        1 => self.static_scale(n_table),
                        3 => self.log_scale(n_table, n_scale as i32),
                        _ => 0,
                    },
                };
                let mut n_multiplier: i64 = 1;
                if n_table < PSEUDO_TABLE_START {
                    n_multiplier = if self.tdef(n_table).flags & FL_TYPE_2 != 0 {
                        2
                    } else {
                        1
                    };
                }
                for _ in 1..=self.dist_member_int("rowcounts", n_table + 1, 2) {
                    n_multiplier *= 10;
                }
                base *= n_multiplier;
                if base >= 0 && n_scale < 1.0 {
                    let mem = self.dist_member_int("rowcounts", n_table + 1, 3);
                    if !(mem == 1 && n_multiplier == 1) {
                        base = i64::from((base as f64 * n_scale) as i32);
                    }
                    if base == 0 {
                        base = 1;
                    }
                }
                rowcounts[n_table as usize] = base;
            }
            self.scaling.base_rowcount = rowcounts;
            self.init.get_rowcount_init = true;
        }
        if table == INVENTORY {
            return sc_w_inventory(self);
        }
        if table == S_INVENTORY {
            return self.get_id_count(ITEM) * self.get_rowcount(WAREHOUSE) * 6;
        }
        self.scaling.base_rowcount[table as usize]
    }

    /// `dateScaling`: the number of rows of a fact table sold on Julian day `j_date`.
    pub(crate) fn date_scaling(&mut self, n_table: i32, j_date: i64) -> i64 {
        let date = jtodt(j_date as i32);
        let (mut k_row_count, mut n_date_weight) = match n_table {
            STORE_SALES | CATALOG_SALES | WEB_SALES => (self.get_rowcount(n_table), CALENDAR_SALES),
            S_CATALOG_ORDER => (self.get_rowcount(CATALOG_SALES), CALENDAR_SALES),
            S_PURCHASE => (self.get_rowcount(STORE_SALES), CALENDAR_SALES),
            S_WEB_ORDER => (self.get_rowcount(WEB_SALES), CALENDAR_SALES),
            S_INVENTORY | INVENTORY => {
                let warehouses = self.get_rowcount(WAREHOUSE);
                (warehouses * self.get_id_count(ITEM), CALENDAR_UNIFORM)
            }
            _ => panic!("dsdgen: table {} does not support dateScaling", self.tdef(n_table).name),
        };
        if n_table != INVENTORY {
            if is_leap(date.year) != 0 {
                n_date_weight += 1;
            }
            let n_calendar_total = i64::from(self.dist_max("calendar", n_date_weight)) * 5;
            let n_day_weight = self.dist_weight("calendar", day_number(&date), n_date_weight);
            k_row_count *= i64::from(n_day_weight);
            k_row_count += n_calendar_total / 2;
            k_row_count /= n_calendar_total;
        }
        k_row_count
    }
}
