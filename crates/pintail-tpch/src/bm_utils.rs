// Ported from duckdb/extension/tpch/dbgen/bm_utils.cpp @ rust-port-baseline
//
// Copyright owned by the Transaction Processing Performance Council; see LICENSE in this crate.
//
// Distributions, value selections and date helpers. The routines the DuckDB generator does not
// call (`tpch_a_rnd`, `e_str`, `agg_str`, `julian`, `mk_ascdate`, `set_state`, `getopt`, file
// helpers) are not ported: dbgen.cpp has its own appending versions of the ones it needs.

use crate::dss::{STARTDATE, Seed, leap};
use crate::rnd::dss_random;

/// The distribution file dbgen embeds (`dists_dss` of dists_dss.h), as plain text.
pub const DISTS_DSS: &str = include_str!("../data/dists.dss");

/// One member of a distribution (`set_member`); `weight` is the cumulative weight.
#[derive(Debug, Clone, Copy)]
pub struct SetMember {
    pub weight: i64,
    pub text: &'static str,
}

/// A weighted set of strings (`distribution`).
#[derive(Debug, Clone, Default)]
pub struct Distribution {
    pub count: usize,
    pub max: i64,
    pub list: Vec<SetMember>,
}

impl Distribution {
    /// `DIST_MEMBER(d, i)`
    pub fn member(&self, i: usize) -> &'static str {
        self.list[i].text
    }

    /// The total weight, which `pick_str` draws from.
    pub fn total_weight(&self) -> i64 {
        self.list[self.count - 1].weight
    }
}

/// `pick_str`: the index of the member that a uniform draw in `[1, total weight]` selects.
#[inline]
pub fn pick_str(s: &Distribution, seed: &mut Seed) -> usize {
    let j = dss_random(1, s.total_weight(), seed);
    let mut i = 0;
    while s.list[i].weight < j {
        i += 1;
    }
    i
}

/// `unjulian`: `date - STARTDATE` in days, for a `yyddd` date.
pub fn unjulian(date: i64) -> i64 {
    let mut res = 0;
    for i in STARTDATE / 1000..date / 1000 {
        res += 365 + leap(i);
    }
    res + date % 1000 - 1
}

/// `read_line_into_buffer`: the next line of `src` with its newline, and whether anything follows it.
fn read_line_into_buffer<'a>(src: &mut &'a str) -> (&'a str, bool) {
    let end = src.find('\n').map_or(src.len(), |i| i + 1);
    let (line, rest) = src.split_at(end);
    *src = rest;
    (line, !rest.is_empty())
}

/// `%ld` of `sscanf`: an optionally signed decimal number after leading white space.
fn scan_long(s: &str) -> Option<i64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits_start = usize::from(s.starts_with(['+', '-']));
    let digits = s[digits_start..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    s[..digits_start + digits].parse().ok()
}

/// `read_dist`: loads the distribution `name` from the embedded dists.dss.
pub fn read_dist(name: &str) -> Distribution {
    let mut src = DISTS_DSS;
    let mut target = Distribution::default();
    let mut name_set = false;
    loop {
        let (line, more) = read_line_into_buffer(&mut src);
        // the C++ stops once the line read reaches the end of the text
        if !more {
            break;
        }
        let line = line.split('\n').next().unwrap_or_default();
        let line = line.split('#').next().unwrap_or_default();
        if line.is_empty() {
            continue;
        }
        if !name_set {
            let mut tokens = line.split(['\n', '\t', ' ']).filter(|t| !t.is_empty());
            if !tokens.next().is_some_and(|t| t.eq_ignore_ascii_case("BEGIN")) {
                continue;
            }
            if !tokens.next().is_some_and(|t| t.eq_ignore_ascii_case(name)) {
                continue;
            }
            name_set = true;
            continue;
        } else if line.len() >= 3 && line[..3].eq_ignore_ascii_case("END") {
            return target;
        }
        // sscanf(line, "%[^|]|%ld", token, &weight)
        let Some(bar) = line.find('|') else { continue };
        if bar == 0 {
            continue;
        }
        let token = &line[..bar];
        let Some(weight) = scan_long(&line[bar + 1..]) else {
            continue;
        };
        if token.eq_ignore_ascii_case("count") {
            target.count = weight as usize;
            target.list = Vec::with_capacity(target.count);
            target.max = 0;
            continue;
        }
        target.max += weight;
        target.list.push(SetMember {
            weight: target.max,
            text: token,
        });
    }
    assert_eq!(target.list.len(), target.count, "Read error on dist '{name}'");
    target
}

/// A random member of `set` (`PickDistribution` of dbgen.cpp).
#[inline]
pub fn pick_distribution(set: &Distribution, seed: &mut Seed) -> &'static str {
    set.member(pick_str(set, seed))
}
