//! duckrs: an analytical SQL database engine, ported from DuckDB.
//!
//! Phase 0 only provides the public API surface; every query returns a
//! `Not implemented` error until the engine lands in later phases.

pub use duckrs_common::{Error, ErrorKind, Result};

use std::path::{Path, PathBuf};

/// A database instance (`DuckDB` / `DatabaseInstance` in C++).
#[derive(Debug)]
pub struct Database {
    path: Option<PathBuf>,
    read_only: bool,
}

impl Database {
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            path: None,
            read_only: false,
        })
    }

    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self> {
        Ok(Self {
            path: Some(path.as_ref().to_path_buf()),
            read_only,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection { _private: () })
    }
}

/// A connection to a [`Database`].
#[derive(Debug)]
pub struct Connection {
    _private: (),
}

impl Connection {
    pub fn query(&mut self, _sql: &str) -> Result<QueryResult> {
        Err(Error::not_implemented("duckrs cannot execute queries yet"))
    }
}

/// A fully materialized query result.
#[derive(Debug, Default)]
pub struct QueryResult {
    pub names: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_reports_not_implemented() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        let err = con.query("SELECT 42").unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotImplemented);
        assert!(err.to_string().starts_with("Not implemented Error: "));
    }
}
