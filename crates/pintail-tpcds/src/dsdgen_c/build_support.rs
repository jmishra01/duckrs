// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/build_support.cpp @ rust-port-baseline

//! Helpers the table builders share: item hierarchies, words made of syllables, business keys.

use super::Ctx;
use super::columns::{CC_COMPANY_NAME, I_BRAND, I_CATEGORY, I_CLASS};
use super::constants::DIST_UNIFORM;

/// The statics of `hierarchy_item`.
pub(crate) struct HierarchyState {
    n_last_category: i32,
    n_last_class: i32,
    n_brand_base: i32,
    sz_class_dist_name: &'static [u8],
}

impl Default for HierarchyState {
    fn default() -> Self {
        Self {
            n_last_category: -1,
            n_last_class: -1,
            n_brand_base: 0,
            sz_class_dist_name: b"",
        }
    }
}

const SZ_XLATE: &[u8; 16] = b"ABCDEFGHIJKLMNOP";

/// `ltoc`: eight letters for the 32 bits of `n_val`, lowest nibble first.
fn ltoc(dest: &mut Vec<u8>, mut n_val: u64) {
    for _ in 0..8 {
        dest.push(SZ_XLATE[(n_val & 0xF) as usize]);
        n_val >>= 4;
    }
}

/// `mk_bkey`: the 16-letter business key of a surrogate key.
pub(crate) fn mk_bkey(dest: &mut Vec<u8>, k_primary: i64, _stream: i32) {
    dest.clear();
    ltoc(dest, (k_primary >> 32) as u64);
    ltoc(dest, (k_primary & 0xFFFF_FFFF) as u64);
}

fn dist_name(name: &'static [u8]) -> &'static str {
    std::str::from_utf8(name).expect("dsdgen: a distribution name is ASCII")
}

impl Ctx {
    /// `hierarchy_item` for `I_CATEGORY` and `I_CLASS`: the id and name of the item's category,
    /// then of its class within that category.
    pub(crate) fn hierarchy_item(&mut self, h_level: i32) -> (i64, &'static [u8]) {
        match h_level {
            I_CATEGORY => {
                let (name, index) = self.pick_distribution_str("categories", 1, 1, h_level);
                let h = &mut self.hierarchy;
                h.n_last_category = index;
                h.n_brand_base = index;
                h.n_last_class = -1;
                (i64::from(index), name)
            }
            I_CLASS => {
                if self.hierarchy.n_last_category == -1 {
                    panic!("dsdgen: I_CLASS before I_CATEGORY");
                }
                let class_dist = self.dist_member_str("categories", self.hierarchy.n_last_category, 2);
                self.hierarchy.sz_class_dist_name = class_dist;
                let (name, index) = self.pick_distribution_str(dist_name(class_dist), 1, 1, h_level);
                self.hierarchy.n_last_class = index;
                self.hierarchy.n_last_category = -1;
                (i64::from(index), name)
            }
            _ => panic!("dsdgen: Invalid call to hierarchy_item"),
        }
    }

    /// `hierarchy_item` for `I_BRAND`: writes the brand name into `name` and returns its id.
    pub(crate) fn hierarchy_item_brand(&mut self, name: &mut Vec<u8>, k_index: i64) -> i64 {
        let h = &self.hierarchy;
        if h.n_last_class == -1 {
            panic!("dsdgen: I_BRAND before I_CLASS");
        }
        let (class_dist, n_last_class, n_brand_base) =
            (dist_name(h.sz_class_dist_name), h.n_last_class, h.n_brand_base);
        let n_brand_count = self.dist_member_int(class_dist, n_last_class, 2);
        let mut id = k_index % i64::from(n_brand_count) + 1;
        self.mk_word(
            name,
            "brand_syllables",
            i64::from(n_brand_base * 10 + n_last_class),
            45,
            I_BRAND,
        );
        name.extend_from_slice(format!(" #{}", id as i32).as_bytes());
        id += i64::from((n_brand_base * 1000 + n_last_class) * 1000);
        id
    }

    /// `mk_companyname`
    pub(crate) fn mk_companyname(&mut self, dest: &mut Vec<u8>, _n_table: i32, n_company: i32) {
        self.mk_word(dest, "syllables", i64::from(n_company), 10, CC_COMPANY_NAME);
    }

    /// `bitmap_to_dist` of a string value: picks row `modulus % size + 1` and divides `modulus` by
    /// the size.
    pub(crate) fn bitmap_to_dist_str(&self, distname: &str, modulus: &mut i64, vset: i32) -> &'static [u8] {
        let s = i64::from(self.distsize(distname));
        let m = ((*modulus % s) + 1) as i32;
        *modulus /= s;
        self.dist_member_str(distname, m, vset)
    }

    /// `bitmap_to_dist` of an integer value.
    pub(crate) fn bitmap_to_dist_int(&self, distname: &str, modulus: &mut i64, vset: i32) -> i32 {
        let s = i64::from(self.distsize(distname));
        let m = ((*modulus % s) + 1) as i32;
        *modulus /= s;
        self.dist_member_int(distname, m, vset)
    }

    /// `mk_word`: the syllables of `src`'s digits in base (number of syllables), as long as they
    /// fit in `char_cnt` characters.
    pub(crate) fn mk_word(&self, dest: &mut Vec<u8>, syl_set: &str, src: i64, char_cnt: i32, _col: i32) {
        dest.clear();
        let mut i = src;
        while i > 0 {
            let n_syllable_count = i64::from(self.distsize(syl_set));
            let cp = self.dist_member_str(syl_set, (i % n_syllable_count) as i32 + 1, 1);
            i /= n_syllable_count;
            if (dest.len() + cp.len()) as i32 <= char_cnt {
                dest.extend_from_slice(cp);
            } else {
                break;
            }
        }
    }

    /// `embed_string`: overwrites `dest` at a random position with a word from a distribution.
    pub(crate) fn embed_string(&mut self, dest: &mut [u8], sz_dist: &str, n_value: i32, n_weight: i32, n_stream: i32) {
        let (word, _) = self.pick_distribution_str(sz_dist, n_value, n_weight, n_stream);
        let max = dest.len() as i64 - word.len() as i64 - 1;
        let n_position = self.genrand_integer(DIST_UNIFORM, 0, max as i32, 0, n_stream) as usize;
        dest[n_position..n_position + word.len()].copy_from_slice(word);
    }
}
