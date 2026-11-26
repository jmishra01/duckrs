// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/address.cpp @ rust-port-baseline

//! Street addresses.

use super::Ctx;
use super::constants::{DIST_UNIFORM, MAXINT};
use super::tables::{ACTIVE_CITIES, ACTIVE_COUNTIES};
use super::tdefs::FL_SMALL;

/// The statics of `mk_address`.
#[derive(Default)]
pub(crate) struct State {
    n_max_cities: i32,
    n_max_counties: i32,
}

/// `ds_addr_t`
#[derive(Clone, Default, Debug)]
pub(crate) struct DsAddr {
    pub(crate) suite_num: Vec<u8>,
    pub(crate) street_num: i32,
    pub(crate) street_name1: &'static [u8],
    pub(crate) street_name2: &'static [u8],
    pub(crate) street_type: &'static [u8],
    pub(crate) city: &'static [u8],
    pub(crate) county: &'static [u8],
    pub(crate) state: &'static [u8],
    pub(crate) country: &'static [u8],
    pub(crate) zip: i32,
    pub(crate) gmt_offset: i32,
}

impl DsAddr {
    /// The street name the tables print: `street_name1 street_name2` (`street_name2` is never
    /// NULL, so this always has the space).
    pub(crate) fn street_name(&self, dest: &mut Vec<u8>) {
        dest.clear();
        dest.extend_from_slice(self.street_name1);
        dest.push(b' ');
        dest.extend_from_slice(self.street_name2);
    }

    /// The zip code the tables print: `%05d`.
    pub(crate) fn zip_string(&self, dest: &mut Vec<u8>) {
        dest.clear();
        dest.extend_from_slice(format!("{:05}", self.zip).as_bytes());
    }
}

/// `city_hash`
pub(crate) fn city_hash(_n_table: i32, name: &[u8]) -> i32 {
    let mut hash_value: i64 = 0;
    let mut res: i64 = 0;
    let twothirtytwo: i64 = 1 << 32;
    for &c in name {
        hash_value *= 26;
        hash_value -= i64::from(b'A');
        // `char` is signed
        hash_value += i64::from(c as i8);
        if !(-MAXINT..=MAXINT).contains(&hash_value) {
            hash_value = (twothirtytwo * 27 + hash_value) % twothirtytwo;
        }
        if hash_value > MAXINT {
            hash_value -= twothirtytwo;
        }
        if hash_value > 1000000 {
            hash_value %= 10000;
            res += hash_value;
            hash_value = 0;
        }
    }
    hash_value %= 1000;
    res += hash_value;
    res %= 10000;
    res as i32
}

impl Ctx {
    /// `mk_address`
    pub(crate) fn mk_address(&mut self, addr: &mut DsAddr, n_column: i32) {
        if !self.init.mk_address_init {
            self.address.n_max_cities = self.get_rowcount(ACTIVE_CITIES) as i32;
            self.address.n_max_counties = self.get_rowcount(ACTIVE_COUNTIES) as i32;
            self.init.mk_address_init = true;
        }
        addr.street_num = self.genrand_integer(DIST_UNIFORM, 1, 1000, 0, n_column);
        addr.street_name1 = self.pick_distribution_str("street_names", 1, 1, n_column).0;
        addr.street_name2 = self.pick_distribution_str("street_names", 1, 2, n_column).0;
        addr.street_type = self.pick_distribution_str("street_type", 1, 1, n_column).0;
        let i = self.genrand_integer(DIST_UNIFORM, 1, 100, 0, n_column);
        addr.suite_num.clear();
        if i & 0x01 != 0 {
            addr.suite_num
                .extend_from_slice(format!("Suite {}", (i >> 1) * 10).as_bytes());
        } else {
            addr.suite_num.extend_from_slice(b"Suite ");
            addr.suite_num.push(((i >> 1) % 25) as u8 + b'A');
        }
        let table = self.get_table_from_column(n_column);
        let small = self.tdef(table).flags & FL_SMALL != 0;
        if small {
            let rows = self.get_rowcount(table) as i32;
            let max = if self.address.n_max_cities > rows {
                rows
            } else {
                self.address.n_max_cities
            };
            let i = self.genrand_integer(DIST_UNIFORM, 1, max, 0, n_column);
            addr.city = self.dist_member_str("cities", i, 1);
        } else {
            addr.city = self.pick_distribution_str("cities", 1, 6, n_column).0;
        }
        let n_region = if small {
            let rows = self.get_rowcount(table) as i32;
            let max = if self.address.n_max_counties > rows {
                rows
            } else {
                self.address.n_max_counties
            };
            let n_region = self.genrand_integer(DIST_UNIFORM, 1, max, 0, n_column);
            addr.county = self.dist_member_str("fips_county", n_region, 2);
            n_region
        } else {
            let (county, n_region) = self.pick_distribution_str("fips_county", 2, 1, n_column);
            addr.county = county;
            n_region
        };
        addr.state = self.dist_member_str("fips_county", n_region, 3);
        addr.zip = city_hash(0, addr.city);
        let zip_prefix = self.dist_member_str("fips_county", n_region, 5);
        let prefix_digit = i32::from(zip_prefix[0]) - i32::from(b'0');
        if prefix_digit == 0 && addr.zip < 9400 {
            addr.zip += 600;
        }
        addr.zip += prefix_digit * 10000;
        // plus4, the city_hash of the street address, is not printed by any table
        addr.gmt_offset = self.dist_member_int("fips_county", n_region, 6);
        addr.country = b"United States";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes() {
        assert_eq!(city_hash(0, b""), 0);
        assert!(city_hash(0, b"Fairview") < 10000);
    }
}
