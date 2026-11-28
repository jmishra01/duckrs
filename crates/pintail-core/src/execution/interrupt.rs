// Ported from duckdb/src/main/client_context.cpp (`ClientContext::InterruptCheck`) @ rust-port-baseline
//
//! The deadline of the query running on this thread (`max_execution_time`), which the pipelines check as they
//! push their chunks, as C++ checks it at its `InterruptCheck` call sites.

use pintail_common::{Error, ErrorKind, Result};
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The deadline of the query running on this thread, if it has one.
pub fn current() -> Option<Instant> {
    DEADLINE.with(Cell::get)
}

/// Runs `body` with `deadline` the deadline of what runs on this thread.
pub fn with_deadline<T>(deadline: Option<Instant>, body: impl FnOnce() -> T) -> T {
    let previous = DEADLINE.with(|current| current.replace(deadline));
    let result = body();
    DEADLINE.with(|current| current.set(previous));
    result
}

/// `ClientContext::InterruptCheck`: an error once the deadline has passed.
pub fn check() -> Result<()> {
    match current() {
        Some(deadline) if Instant::now() >= deadline => Err(Error::new(
            ErrorKind::Interrupt,
            "Query exceeded maximum execution time",
        )),
        _ => Ok(()),
    }
}
