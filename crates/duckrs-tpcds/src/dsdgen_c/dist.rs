// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/dist.cpp @ rust-port-baseline

//! The distributions of `tpcds.idx`: named tables of values with sets of weights, from which the
//! generator picks values at random (`pick_distribution`) or by row (`dist_member`).

use super::Ctx;
use super::constants::DIST_UNIFORM;
use super::r_params::atoi;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The binary distribution file the generator reads its distributions from.
static TPCDS_IDX: &[u8] = include_bytes!("../../data/tpcds.idx");

const D_NAME_LEN: usize = 20;
/// `IDX_SIZE`: a name and seven 32-bit integers.
const IDX_SIZE: usize = D_NAME_LEN + 7 * 4;
const TKN_INT: i32 = 7;

/// A loaded distribution (`d_idx_t` with its `dist_t`).
pub(crate) struct Dist {
    length: i32,
    type_vector: Vec<i32>,
    /// The weights of each weight set, accumulated, as `load_dist` keeps them.
    weight_sets: Vec<Vec<i32>>,
    maximums: Vec<i32>,
    /// The values of each value set, by row, without their terminating NUL.
    values: Vec<Vec<&'static [u8]>>,
    /// The values of each value set read as integers (`atoi`).
    int_values: Vec<Vec<i32>>,
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

fn c_str(bytes: &'static [u8]) -> &'static [u8] {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// `find_dist`'s index and `load_dist`, for every distribution at once. Names compare without case,
/// as `strcasecmp` does, so they are kept in lower case.
fn load_distributions() -> HashMap<String, Dist> {
    let idx = TPCDS_IDX;
    let entry_count = read_i32(idx, 0) as usize;
    let mut result = HashMap::with_capacity(entry_count);
    let mut entry = idx.len() - entry_count * IDX_SIZE;
    for _ in 0..entry_count {
        let name = c_str(&idx[entry..entry + D_NAME_LEN]);
        let mut p = entry + D_NAME_LEN;
        let mut next = || {
            let v = read_i32(idx, p);
            p += 4;
            v
        };
        let _index = next();
        let offset = next() as usize;
        let str_space = next() as usize;
        let length = next();
        let w_width = next();
        let v_width = next();
        let name_space = next() as usize;
        entry += IDX_SIZE;

        let mut read = offset;
        let mut take = || {
            let v = read_i32(idx, read);
            read += 4;
            v
        };
        let type_vector: Vec<i32> = (0..v_width).map(|_| take()).collect();
        let mut maximums = Vec::with_capacity(w_width as usize);
        let weight_sets: Vec<Vec<i32>> = (0..w_width)
            .map(|_| {
                let mut total = 0;
                let set = (0..length)
                    .map(|_| {
                        total += take();
                        total
                    })
                    .collect();
                maximums.push(total);
                set
            })
            .collect();
        let value_offsets: Vec<Vec<usize>> = (0..v_width)
            .map(|_| (0..length).map(|_| take() as usize).collect())
            .collect();
        let strings_start = read + name_space;
        let strings = &idx[strings_start..strings_start + str_space];
        let values: Vec<Vec<&'static [u8]>> = value_offsets
            .iter()
            .map(|set| set.iter().map(|&o| c_str(&strings[o..])).collect())
            .collect();
        let int_values = values.iter().map(|set| set.iter().map(|v| atoi(v)).collect()).collect();
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        result.insert(
            name,
            Dist {
                length,
                type_vector,
                weight_sets,
                maximums,
                values,
                int_values,
            },
        );
    }
    result
}

/// `find_dist`
pub(crate) fn find_dist(name: &str) -> &'static Dist {
    static DISTRIBUTIONS: OnceLock<HashMap<String, Dist>> = OnceLock::new();
    let distributions = DISTRIBUTIONS.get_or_init(load_distributions);
    distributions
        .get(name)
        .or_else(|| distributions.get(&name.to_ascii_lowercase()))
        .unwrap_or_else(|| panic!("dsdgen: Invalid distribution name '{name}'"))
}

impl Dist {
    fn int_or_one(&self, set: usize, index: usize) -> i32 {
        // dist_op returns the integer of an int value, and 1 for other types
        if self.type_vector[set] == TKN_INT {
            self.int_values[set][index]
        } else {
            1
        }
    }
}

impl Ctx {
    /// The row `pick_distribution` draws: by weight set `wset`, counting from 0.
    fn pick_index(&mut self, dist: &Dist, wset: i32, stream: i32) -> usize {
        let weights = &dist.weight_sets[(wset - 1) as usize];
        let level = self.genrand_integer(DIST_UNIFORM, 1, dist.maximums[(wset - 1) as usize], 0, stream);
        let mut index = 0usize;
        while (index as i32) < dist.length && level > weights[index] {
            index += 1;
        }
        if index as i32 >= dist.length {
            panic!("dsdgen: Distribution overrun");
        }
        index
    }

    /// `pick_distribution` of a string value: the value of value set `vset` in a row drawn by
    /// weight set `wset`, and the row (counting from 1).
    pub(crate) fn pick_distribution_str(
        &mut self,
        name: &str,
        vset: i32,
        wset: i32,
        stream: i32,
    ) -> (&'static [u8], i32) {
        let dist = find_dist(name);
        let index = self.pick_index(dist, wset, stream);
        (dist.values[(vset - 1) as usize][index], index as i32 + 1)
    }

    /// `pick_distribution` of an integer value, and the row (counting from 1).
    pub(crate) fn pick_distribution_int(&mut self, name: &str, vset: i32, wset: i32, stream: i32) -> (i32, i32) {
        let dist = find_dist(name);
        let index = self.pick_index(dist, wset, stream);
        (dist.int_values[(vset - 1) as usize][index], index as i32 + 1)
    }

    /// `dist_member` of a string value: value set `vset` of row `index` (counting from 1).
    pub(crate) fn dist_member_str(&self, name: &str, index: i32, vset: i32) -> &'static [u8] {
        let dist = find_dist(name);
        check_member(dist, index);
        dist.values[(vset - 1) as usize][(index - 1) as usize]
    }

    /// `dist_member` of an integer value, or `dist_member(NULL, ...)`: the integer of an int
    /// value, and 1 for other types.
    pub(crate) fn dist_member_int(&self, name: &str, index: i32, vset: i32) -> i32 {
        let dist = find_dist(name);
        check_member(dist, index);
        dist.int_or_one((vset - 1) as usize, (index - 1) as usize)
    }

    /// `dist_weight`: the weight of row `index` (counting from 1) in weight set `wset`.
    pub(crate) fn dist_weight(&self, name: &str, index: i32, wset: i32) -> i32 {
        let dist = find_dist(name);
        let set = &dist.weight_sets[(wset - 1) as usize];
        let mut res = set[(index - 1) as usize];
        if index > 1 {
            res -= set[(index - 2) as usize];
        }
        res
    }

    /// `distsize`
    pub(crate) fn distsize(&self, name: &str) -> i32 {
        find_dist(name).length
    }

    /// `dist_max(dist, w)`: the total of weight set `wset`.
    pub(crate) fn dist_max(&self, name: &str, wset: i32) -> i32 {
        find_dist(name).maximums[(wset - 1) as usize]
    }
}

fn check_member(dist: &Dist, index: i32) {
    if index > dist.length || index < 1 {
        panic!("dsdgen: Distribution over-run/under-run");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_load() {
        let d = find_dist("calendar");
        assert_eq!(d.length, 366);
        assert_eq!(find_dist("Syllables").length, find_dist("syllables").length);
    }
}
