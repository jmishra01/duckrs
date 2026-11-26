// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/join.cpp @ rust-port-baseline

//! Foreign keys: `mk_join` picks the row of another table that a column refers to.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, dttoj, is_leap, strtodt};
use super::tables::*;
use super::tdefs::FL_TYPE_2;

/// The statics of `date_join`, `cp_join` and `web_join`.
#[derive(Default)]
pub(crate) struct State {
    j_today: i32,
    n_page_per_catalog: i32,
    cp_start: DateT,
    n_concurrent_sites: i32,
    n_site_duration: i32,
    n_offset: i32,
}

impl Ctx {
    /// `date_join`
    fn date_join(&mut self, from_tbl: i32, from_col: i32, join_count: i64, n_year: i32) -> i64 {
        if !self.init.date_join_init {
            self.join.j_today = dttoj(&strtodt(TODAYS_DATE));
            self.init.date_join_init = true;
        }
        let n_day = match from_tbl {
            STORE_SALES | CATALOG_SALES | WEB_SALES => {
                self.pick_distribution_int("calendar", 1, CALENDAR_SALES + is_leap(n_year), from_col)
                    .0
            }
            STORE_RETURNS | CATALOG_RETURNS | WEB_RETURNS => {
                let (n_min, n_max) = match from_tbl {
                    STORE_RETURNS => (SS_MIN_SHIP_DELAY, SS_MAX_SHIP_DELAY),
                    CATALOG_RETURNS => (CS_MIN_SHIP_DELAY, CS_MAX_SHIP_DELAY),
                    _ => (WS_MIN_SHIP_DELAY, WS_MAX_SHIP_DELAY),
                };
                let n_temp = self.genrand_integer(DIST_UNIFORM, n_min * 2, n_max * 2, 0, from_col);
                return join_count + i64::from(n_temp);
            }
            WEB_SITE | WEB_PAGE => return self.web_join(from_col, join_count),
            _ => {
                self.pick_distribution_int("calendar", 1, 1 + is_leap(n_year), from_col)
                    .0
            }
        };
        let temp = DateT {
            year: n_year,
            month: 1,
            day: 1,
            julian: 0,
        };
        let n_result = dttoj(&temp) + n_day;
        if n_result > self.join.j_today {
            -1
        } else {
            i64::from(n_result)
        }
    }

    /// `time_join`
    fn time_join(&mut self, to_tbl: i32, to_col: i32, _join_count: i64) -> i64 {
        let weight_set = match to_tbl {
            STORE_SALES | STORE_RETURNS => 2,
            CATALOG_SALES | WEB_SALES | CATALOG_RETURNS | WEB_RETURNS => 3,
            _ => 1,
        };
        let hour = self.pick_distribution_int("hours", 1, weight_set, to_col).0;
        let secs = self.genrand_integer(DIST_UNIFORM, 0, 3599, 0, to_col);
        i64::from(hour * 3600 + secs)
    }

    /// `cp_join`: the catalog page of a catalog that is current on `j_date`.
    fn cp_join(&mut self, _tbl: i32, col: i32, j_date: i64) -> i64 {
        if !self.init.cp_join_init {
            self.join.n_page_per_catalog =
                ((self.get_rowcount(CATALOG_PAGE) as i32) / CP_CATALOGS_PER_YEAR) / (YEAR_MAXIMUM - YEAR_MINIMUM + 2);
            self.join.cp_start = strtodt(DATA_START_DATE);
            self.init.cp_join_init = true;
        }
        let n_type = self.pick_distribution_str("catalog_page_type", 1, 2, col).1;
        let n_page = self.genrand_integer(DIST_UNIFORM, 1, self.join.n_page_per_catalog, 0, col);
        let mut n_offset = j_date as i32 - self.join.cp_start.julian - 1;
        let mut n_count = (n_offset / 365) * CP_CATALOGS_PER_YEAR;
        n_offset %= 365;
        match n_type {
            1 => {
                if n_offset > 183 {
                    n_count += 1;
                }
            }
            2 => n_count += n_offset / 91,
            3 => n_count += n_offset / 31,
            _ => {}
        }
        // CP_SK(c, s, p)
        i64::from(n_count * self.join.n_page_per_catalog + n_page)
    }

    /// `web_join`: the dates of web sites and pages, and the pages of web sales and returns.
    fn web_join(&mut self, col: i32, join_key: i64) -> i64 {
        let ws_param = i64::from(self.tdef(WEB_SITE).n_param);
        let wp_param = i64::from(self.tdef(WEB_PAGE).n_param);
        if !self.init.web_join_init {
            let site_close = strtodt(WEB_END_DATE);
            let mut n_site_duration = site_close.julian;
            self.join.n_concurrent_sites = self.get_rowcount(CONCURRENT_WEB_SITES) as i32;
            let site_open = strtodt(WEB_START_DATE);
            n_site_duration -= site_open.julian;
            n_site_duration *= self.join.n_concurrent_sites;
            self.join.n_site_duration = n_site_duration;
            self.join.n_offset = (site_close.julian - site_open.julian) / (2 * n_site_duration);
            self.init.web_join_init = true;
        }
        let duration = i64::from(self.join.n_site_duration);
        let site_open = i64::from(strtodt(DATE_MINIMUM).julian);
        let mut res = -1;
        match col {
            WEB_OPEN_DATE => {
                res = site_open - ((join_key * WEB_DATE_STAGGER) % duration / 2);
                if web_is_replaced(join_key) && web_is_replacement(join_key) {
                    res += i64::from(self.join.n_offset) * duration;
                }
            }
            WEB_CLOSE_DATE => {
                res = site_open - ((join_key * WEB_DATE_STAGGER) % duration / 2);
                res += ws_param * duration;
                if web_is_replaced(join_key) && !web_is_replacement(join_key) {
                    res -= ws_param * duration / 2;
                }
            }
            WEB_REC_START_DATE_ID => {
                res = site_open - (((join_key - 1) * WEB_DATE_STAGGER) % duration / 2);
                res += (join_key % ws_param) * duration;
            }
            WEB_REC_END_DATE_ID => {
                res = site_open - ((join_key * WEB_DATE_STAGGER) % duration / 2);
                res += ((join_key + 1) % ws_param) * duration * 5 - 1;
            }
            WP_REC_START_DATE_ID => {
                res = site_open - (((join_key - 1) * WEB_DATE_STAGGER) % duration / 2);
                res += (join_key % wp_param) * duration * 5;
            }
            WP_REC_END_DATE_ID => {
                res = site_open - ((join_key * WEB_DATE_STAGGER) % duration / 2);
                res += ((join_key + 1) % wp_param) * duration - 1;
            }
            WP_CREATION_DATE_SK => {
                let k_site = join_key / i64::from(WEB_PAGES_PER_SITE) + 1;
                let site = k_site as i32;
                res =
                    site_open - i64::from((site.wrapping_mul(WEB_DATE_STAGGER as i32)) % self.join.n_site_duration / 2);
                if i64::from(site) % wp_param == 0 {
                    res = i64::from(self.genrand_integer(DIST_UNIFORM, res as i32, site_open as i32, 0, col));
                }
            }
            WR_WEB_PAGE_SK | WS_WEB_PAGE_SK => {
                res = i64::from(self.genrand_integer(DIST_UNIFORM, 1, WEB_PAGES_PER_SITE, 0, col));
            }
            _ => {}
        }
        res
    }

    /// `mk_join`: the key of a row of `to_tbl` that column `from_col` refers to; `join_count`
    /// says which (a date, or a row number) where the join depends on it.
    pub(crate) fn mk_join(&mut self, from_col: i32, to_tbl: i32, join_count: i64) -> i64 {
        let n_from_table = self.get_table_from_column(from_col);
        match to_tbl {
            CATALOG_PAGE => self.cp_join(n_from_table, from_col, join_count),
            DATET => {
                let n_year = self.genrand_integer(DIST_UNIFORM, YEAR_MINIMUM, YEAR_MAXIMUM, 0, from_col);
                self.date_join(n_from_table, from_col, join_count, n_year)
            }
            TIME => self.time_join(n_from_table, from_col, join_count),
            _ => {
                if self.tdef(to_tbl).flags & FL_TYPE_2 != 0 {
                    return self.scd_join(to_tbl, from_col, join_count);
                }
                let rows = self.get_rowcount(to_tbl);
                self.genrand_key(DIST_UNIFORM, 1, rows, 0, from_col)
            }
        }
    }
}
