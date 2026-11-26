// Ported from duckdb/extension/tpcds/dsdgen/dsdgen.cpp and dsdgen_helpers.cpp @ rust-port-baseline

//! `DSDGenWrapper::DSDGen`: generates the rows of every table, in order, as `DataChunk`s.

use crate::Table;
use crate::append_info::AppendInfo;
use crate::dsdgen_c::Ctx;
use crate::dsdgen_c::genrand::initial_streams;
use crate::dsdgen_c::tables::DBGEN_VERSION;
use crate::dsdgen_c::tdef_functions::{BuilderFn, builder};
use crate::dsdgen_c::tdefs::{FL_CHILD, column_tables, initial_w_tdefs};
use crate::queries::MAX_SCALE_FACTOR;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};

/// `InitializeDSDgen(scale)`: a fresh generator state for scale factor `scale`.
fn initialize_dsdgen(scale: f64) -> Box<Ctx> {
    let mut ctx = Box::<Ctx>::default();
    // set_str("SCALE", std::to_string(scale)), read back with atof
    ctx.params.scale = format!("{scale:.6}").parse().expect("a formatted number parses");
    ctx.w_tdefs = initial_w_tdefs();
    ctx.column_tables = column_tables();
    ctx.streams = initial_streams();
    ctx.init_rand();
    ctx
}

/// The rows of all TPC-DS tables at one scale factor, as `(table, chunk)` pairs: see [`dsdgen`].
pub struct DsdGen {
    ctx: Box<Ctx>,
    out: AppendInfo,
    table_id: i32,
    table_started: bool,
    current_row: i64,
    current_table_remaining: i64,
    builder: Option<BuilderFn>,
    finished: bool,
}

/// `DSDGenWrapper::DSDGen(sf)`: the rows of all 24 tables at scale factor `sf`, in chunks of at
/// most `STANDARD_VECTOR_SIZE` rows.
///
/// The tables are generated in table number order ([`Table::ALL`]). A sales table generates its
/// returns table along with it, so the chunks of the two are interleaved; the chunks of each
/// table are in row order. A scale factor of 0 or less generates nothing, as in C++.
pub fn dsdgen(sf: f64) -> Result<DsdGen> {
    if sf.is_nan() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "DSDGen requires a valid scale factor",
        ));
    }
    let finished = sf <= 0.0;
    if !finished && sf > MAX_SCALE_FACTOR {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "DSDGen does not support a scale factor exceeding {}.",
                MAX_SCALE_FACTOR as i64
            ),
        ));
    }
    let ctx = initialize_dsdgen(if finished { 1.0 } else { sf });
    let out = AppendInfo::new(|t| ctx.tdef(t as i32).n_first_column);
    Ok(DsdGen {
        ctx,
        out,
        table_id: 0,
        table_started: false,
        current_row: 1,
        current_table_remaining: 0,
        builder: None,
        finished,
    })
}

impl DsdGen {
    /// The number of rows dsdgen makes of `table` (`get_rowcount`): orders and tickets for the
    /// sales tables, which have several lines each, and 0 for the returns tables, which their
    /// sales tables make.
    pub fn row_count(&mut self, table: Table) -> i64 {
        if table.is_child() {
            0
        } else {
            self.ctx.get_rowcount(table as i32)
        }
    }

    /// `StartNextTable`
    fn start_next_table(&mut self) -> bool {
        while self.table_id < DBGEN_VERSION {
            if self.ctx.tdef(self.table_id).flags & FL_CHILD != 0 {
                self.table_id += 1;
                continue;
            }
            self.current_row = 1;
            self.current_table_remaining = self.ctx.get_rowcount(self.table_id);
            // FL_SMALL tables call ResetCountCount, which resets counters nothing reads
            self.builder = builder(self.table_id);
            self.table_started = true;
            return true;
        }
        false
    }

    /// Ends the current table: its rows and those of its child table go out in chunks.
    fn finish_table(&mut self) {
        self.out.flush(self.table_id);
        if let Some(child) = Table::from_id(self.table_id).child() {
            self.out.flush(child as i32);
        }
        self.table_started = false;
        self.table_id += 1;
    }

    /// `GenerateSequential`, one row at a time.
    fn generate_row(&mut self) {
        if !self.table_started && !self.start_next_table() {
            self.finished = true;
            return;
        }
        if self.current_table_remaining == 0 {
            self.finish_table();
            return;
        }
        let builder = self.builder.expect("every parent table has a builder");
        builder(&mut self.ctx, &mut self.out, self.current_row);
        self.ctx.row_stop(self.table_id);
        self.current_row += 1;
        self.current_table_remaining -= 1;
    }
}

impl Iterator for DsdGen {
    type Item = (Table, DataChunk);

    fn next(&mut self) -> Option<(Table, DataChunk)> {
        loop {
            if let Some(chunk) = self.out.ready.pop_front() {
                return Some(chunk);
            }
            if self.finished {
                return None;
            }
            self.generate_row();
        }
    }
}
