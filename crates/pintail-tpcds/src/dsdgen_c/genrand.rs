// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/genrand.cpp @ rust-port-baseline

//! The random number streams: a Lehmer generator per column, which can be skipped ahead.

use super::Ctx;
use super::columns::MAX_COLUMN;
use super::constants::{ALPHANUM, DIST_EXPONENTIAL, DIST_RETURNS, DIST_SALES, DIST_UNIFORM, MAXINT};
use super::date::{DateT, dttoj, is_leap, jtodt};
use super::decimal::Decimal;
use super::streams::STREAMS;

const MULT: i64 = 16807;
const N_Q: i64 = 127773;
const N_R: i64 = 2836;

/// `rng_t`, without the column, table and duplicate numbers, which only `row_skip` reads.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Rng {
    pub(crate) n_used: i32,
    pub(crate) n_used_per_row: i32,
    pub(crate) n_seed: i64,
    pub(crate) n_initial_seed: i64,
    pub(crate) n_total: i64,
}

/// The streams as `streams.h` declares them, before `init_rand` seeds them.
pub(crate) fn initial_streams() -> Vec<Rng> {
    STREAMS
        .iter()
        .map(|&(n_used_per_row, _n_column, _n_table, _n_duplicate_of)| Rng {
            n_used_per_row,
            ..Rng::default()
        })
        .collect()
}

impl Ctx {
    /// `next_random`
    pub(crate) fn next_random(&mut self, stream: i32) -> i64 {
        let r = &mut self.streams[stream as usize];
        let mut s = r.n_seed;
        let div_res = s / N_Q;
        let mod_res = s - N_Q * div_res;
        s = MULT * mod_res - div_res * N_R;
        if s < 0 {
            s += MAXINT;
        }
        r.n_seed = s;
        r.n_used += 1;
        r.n_total += 1;
        s
    }

    /// `genrand_integer`, returning the value.
    pub(crate) fn genrand_integer(&mut self, dist: i32, min: i32, max: i32, _mean: i32, stream: i32) -> i32 {
        match dist {
            DIST_UNIFORM => {
                let mut res = self.next_random(stream) as i32;
                res %= max.wrapping_sub(min).wrapping_add(1);
                res.wrapping_add(min)
            }
            DIST_EXPONENTIAL => {
                let mut fres = 0f64;
                for _ in 0..12 {
                    fres += (self.next_random(stream) / MAXINT) as f64 - 0.5;
                }
                min.wrapping_add((f64::from(max.wrapping_sub(min).wrapping_add(1)) * fres) as i32)
            }
            _ => panic!("dsdgen: Undefined distribution"),
        }
    }

    /// `genrand_key`, returning the value.
    pub(crate) fn genrand_key(&mut self, dist: i32, min: i64, max: i64, _mean: i64, stream: i32) -> i64 {
        let res = match dist {
            DIST_UNIFORM => {
                let mut res = self.next_random(stream) as i32;
                res %= (max - min + 1) as i32;
                res.wrapping_add(min as i32)
            }
            DIST_EXPONENTIAL => {
                let mut fres = 0f64;
                for _ in 0..12 {
                    fres += (self.next_random(stream) / MAXINT) as f64 - 0.5;
                }
                (min as i32).wrapping_add(((max - min + 1) as f64 * fres) as i32)
            }
            _ => panic!("dsdgen: Undefined distribution"),
        };
        i64::from(res)
    }

    /// `genrand_decimal`: sets the precision, number and scale of `dest`.
    pub(crate) fn genrand_decimal(
        &mut self,
        dest: &mut Decimal,
        dist: i32,
        min: &Decimal,
        max: &Decimal,
        mean: Option<&Decimal>,
        stream: i32,
    ) {
        dest.precision = if min.precision < max.precision {
            min.precision
        } else {
            max.precision
        };
        let mut number = match dist {
            DIST_UNIFORM => {
                let mut n = self.next_random(stream);
                n %= max.number - min.number + 1;
                n + min.number
            }
            DIST_EXPONENTIAL => {
                let mut fres = 0f64;
                for _ in 0..12 {
                    fres /= 2.0;
                    fres += self.next_random(stream) as f64 / MAXINT as f64 - 0.5;
                }
                let mean = mean.expect("dsdgen: an exponential decimal needs a mean");
                mean.number + i64::from(((max.number - min.number + 1) as f64 * fres) as i32)
            }
            _ => panic!("dsdgen: Undefined distribution"),
        };
        dest.number = number;
        let mut i = 0;
        while number > 10 {
            number /= 10;
            i += 1;
        }
        dest.scale = i;
    }

    /// `gen_charset`: `min..=max` characters drawn from `set`; draws `max` + 1 numbers.
    pub(crate) fn gen_charset(&mut self, dest: &mut Vec<u8>, set: &[u8], min: i32, max: i32, stream: i32) {
        let len = self.genrand_integer(DIST_UNIFORM, min, max, 0, stream);
        dest.clear();
        for i in 0..max {
            let temp = self.genrand_integer(DIST_UNIFORM, 0, set.len() as i32 - 1, 0, stream);
            if i < len {
                dest.push(set[temp as usize]);
            }
        }
    }

    /// `genrand_date`
    pub(crate) fn genrand_date(
        &mut self,
        dist: i32,
        min: &DateT,
        max: &DateT,
        mean: Option<&DateT>,
        stream: i32,
    ) -> DateT {
        let mut idt = dttoj(min);
        let mut range = dttoj(max);
        range -= idt;
        let mut imean = 0;
        match dist {
            DIST_SALES | DIST_RETURNS => {
                let mut n_day_count = min.day;
                let mut n_year = min.year;
                let mut n_total_weight = 0;
                loop {
                    range -= 1;
                    if range == 0 {
                        break;
                    }
                    n_total_weight += self.dist_weight("calendar", n_day_count, dist + is_leap(n_year));
                    if n_day_count == 365 + is_leap(n_year) {
                        n_year += 1;
                        n_day_count = 1;
                    } else {
                        n_day_count += 1;
                    }
                }
                let mut temp = self.genrand_integer(DIST_UNIFORM, 1, n_total_weight, 0, stream);
                n_day_count = min.day;
                idt = min.julian;
                n_year = min.year;
                while temp >= 0 {
                    temp -= self.dist_weight("calendar", n_day_count, dist + is_leap(n_year));
                    n_day_count += 1;
                    idt += 1;
                    if n_day_count > 365 + is_leap(n_year) {
                        n_year += 1;
                        n_day_count = 1;
                    }
                }
            }
            DIST_EXPONENTIAL | DIST_UNIFORM => {
                if dist == DIST_EXPONENTIAL {
                    imean = dttoj(mean.expect("dsdgen: an exponential date needs a mean"));
                    imean -= idt;
                }
                let temp = self.genrand_integer(dist, 0, range, imean, stream);
                idt += temp;
            }
            _ => {}
        }
        jtodt(idt)
    }

    /// `init_rand`: seeds every stream, `MAXINT / MAX_COLUMN` apart.
    pub(crate) fn init_rand(&mut self) {
        if self.init.init_rand_init {
            return;
        }
        self.streams = initial_streams();
        let n_seed = self.params.rng_seed;
        let skip = MAXINT / i64::from(MAX_COLUMN);
        for i in 0..i64::from(MAX_COLUMN) {
            let seed = if i != 0 && (i64::from(i32::MAX) - n_seed) / i < skip {
                let mut val = n_seed + skip * i;
                val %= MAXINT;
                val -= MAXINT;
                val -= 2;
                val
            } else {
                n_seed + skip * i
            };
            let r = &mut self.streams[i as usize];
            r.n_initial_seed = seed;
            r.n_seed = seed;
            r.n_used = 0;
            r.n_total = 0;
        }
        self.init.init_rand_init = true;
    }

    /// `genrand_email`: `first.last@company.domain`.
    pub(crate) fn genrand_email(&mut self, dest: &mut Vec<u8>, first: &[u8], last: &[u8], column: i32) {
        let (domain, _) = self.pick_distribution_str("top_domains", 1, 1, column);
        let company_length = self.genrand_integer(DIST_UNIFORM, 10, 20, 0, column);
        let mut company = Vec::new();
        self.gen_charset(&mut company, ALPHANUM, 1, 20, column);
        company.truncate(company_length as usize);
        dest.clear();
        dest.extend_from_slice(first);
        dest.push(b'.');
        dest.extend_from_slice(last);
        dest.push(b'@');
        dest.extend_from_slice(&company);
        dest.push(b'.');
        dest.extend_from_slice(domain);
    }

    /// `genrand_url`
    pub(crate) fn genrand_url(&mut self, dest: &mut Vec<u8>, _column: i32) {
        dest.clear();
        dest.extend_from_slice(b"http://www.foo.com");
    }
}
