// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/tdefs.cpp @ rust-port-baseline

//! Table definitions (`tdef`) and the lookups of `tdefs.cpp`.

use super::Ctx;
use super::columns::MAX_COLUMN;
use super::tables::{MAX_TABLE, S_BRAND};
use super::tdef_data::{S_TDEFS, W_TDEFS};

pub(crate) const FL_SMALL: i32 = 0x0040;
pub(crate) const FL_CHILD: i32 = 0x0004;
pub(crate) const FL_TYPE_2: i32 = 0x0020;
pub(crate) const FL_PARENT: i32 = 0x1000;

/// `tdef`, without the output file and the sparse keys, which no TPC-DS table uses.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct Tdef {
    pub(crate) name: &'static str,
    pub(crate) abreviation: &'static str,
    pub(crate) flags: i32,
    pub(crate) n_first_column: i32,
    pub(crate) n_last_column: i32,
    pub(crate) n_table_index: i32,
    pub(crate) n_param: i32,
    pub(crate) n_update_size: i32,
    pub(crate) n_new_row_pct: i32,
    pub(crate) n_null_pct: i32,
    pub(crate) k_null_bit_map: i64,
    pub(crate) k_not_null_bit_map: i64,
}

/// The warehouse table definitions a context starts from.
pub(crate) fn initial_w_tdefs() -> Vec<Tdef> {
    W_TDEFS.to_vec()
}

/// The table of every column, as `getTableFromColumn` finds it: the first table, in table number
/// order, whose column range holds the column.
pub(crate) fn column_tables() -> Vec<i32> {
    (0..=MAX_COLUMN)
        .map(|column| {
            (0..=MAX_TABLE)
                .find(|&table| {
                    simple_tdef(&W_TDEFS, table)
                        .is_some_and(|t| column >= t.n_first_column && column <= t.n_last_column)
                })
                .unwrap_or(-1)
        })
        .collect()
}

fn simple_tdef(w_tdefs: &[Tdef], table: i32) -> Option<&Tdef> {
    if table >= S_BRAND {
        S_TDEFS.get((table - S_BRAND) as usize)
    } else {
        w_tdefs.get(table as usize)
    }
}

impl Ctx {
    /// `getSimpleTdefsByNumber`
    pub(crate) fn tdef(&self, table: i32) -> &Tdef {
        simple_tdef(&self.w_tdefs, table).unwrap_or_else(|| panic!("dsdgen: no table {table}"))
    }

    /// `getSimpleTdefsByNumber`, for writing (the null bitmap).
    pub(crate) fn tdef_mut(&mut self, table: i32) -> &mut Tdef {
        &mut self.w_tdefs[table as usize]
    }

    /// `getTableFromColumn`
    pub(crate) fn get_table_from_column(&self, column: i32) -> i32 {
        self.column_tables.get(column as usize).copied().unwrap_or(-1)
    }
}
