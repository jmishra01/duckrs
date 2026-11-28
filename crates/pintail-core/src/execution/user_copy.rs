// Ported from duckdb/src/execution/operator/persistent/physical_copy_to_file.cpp (the parts a copy function of
// duckdb/src/main/capi/v1/copy_function-c.cpp uses: one global state, sink, finalize) @ rust-port-baseline

//! `PhysicalCopyToFile` of a copy function a client registered ([`crate::function::udf::UserCopyTo`]).

use super::physical::{PhysicalOperator, SourceState};
use crate::function::udf::{CopyToSink, UserCopyTo};
use parking_lot::Mutex;
use pintail_common::Result;
use pintail_common::types::LogicalType;
use pintail_common::vector::{DataChunk, Vector};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes the rows of its child with the copy function's sink, and gives how many there were.
pub struct PhysicalUserCopyTo {
    types: Vec<LogicalType>,
    copy: UserCopyTo,
    children: Vec<Box<dyn PhysicalOperator>>,
    /// `CopyToFunctionGlobalState`: made by the function's global init before the first rows (or at the end, when
    /// there are none), so that planning alone (EXPLAIN) does not run it
    state: Mutex<Option<Box<dyn CopyToSink>>>,
    rows: AtomicU64,
}

impl fmt::Debug for PhysicalUserCopyTo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhysicalUserCopyTo").field("copy", &self.copy).finish()
    }
}

impl PhysicalUserCopyTo {
    pub fn new(copy: UserCopyTo, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        Self {
            types: vec![LogicalType::BIGINT],
            copy,
            children,
            state: Mutex::new(None),
            rows: AtomicU64::new(0),
        }
    }

    /// `GetGlobalSinkState`: the function's global state, made the first time.
    fn with_state(&self, run: impl FnOnce(&mut dyn CopyToSink) -> Result<()>) -> Result<()> {
        let mut state = self.state.lock();
        if state.is_none() {
            *state = Some(self.copy.bound.initialize(&self.copy.path)?);
        }
        run(state.as_mut().expect("initialized above").as_mut())
    }
}

impl PhysicalOperator for PhysicalUserCopyTo {
    fn name(&self) -> &'static str {
        "COPY_TO_FILE"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn is_sink(&self) -> bool {
        true
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        self.with_state(|state| state.sink(chunk))?;
        self.rows.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        self.with_state(|state| state.finalize())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("copy state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let mut count = Vector::new(LogicalType::BIGINT, 1);
        count.data_mut::<i64>()[0] = self.rows.load(Ordering::Relaxed) as i64;
        Ok(Some(DataChunk::from_columns(vec![count], 1)))
    }
}
