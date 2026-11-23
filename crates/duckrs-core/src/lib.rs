//! The duckrs engine (DuckDB's `src/` beyond `common` and `parser`): the catalog, functions,
//! the binder and planner, execution and storage. It starts as one crate with a module per C++
//! directory; the modules split into crates once their boundaries settle.

pub mod catalog;
pub mod execution;
pub mod function;
pub mod logging;
pub mod optimizer;
pub mod planner;
pub mod storage;

/// `NotImplementedException` with DuckDB's own wording.
pub(crate) fn error_not_implemented(message: &str) -> duckrs_common::Error {
    duckrs_common::Error::not_implemented(message)
}

/// The error for SQL that DuckDB supports but duckrs does not run yet. The wording is kept
/// apart from DuckDB's own messages, so that a test expecting an error never passes because
/// of a feature that is missing here.
pub fn unported(what: impl std::fmt::Display) -> duckrs_common::Error {
    duckrs_common::Error::not_implemented(format!("{what} is not yet ported to duckrs"))
}
