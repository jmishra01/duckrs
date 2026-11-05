//! duckrs: an analytical SQL database engine, ported from DuckDB.
//!
//! The engine runs in memory and covers tables, views, sequences, macros, INSERT, SELECT with
//! joins and prepared statements so far; other statements return a `Not implemented` error
//! until later phases.

pub use duckrs_common::types::{LogicalType, LogicalTypeId};
pub use duckrs_common::value::Value;
pub use duckrs_common::{Error, ErrorKind, Result};
pub use duckrs_core::client::MaterializedQueryResult as QueryResult;

use duckrs_core::client::{ClientContext, DatabaseInstance, PreparedStatementData};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A database instance (`DuckDB` / `DatabaseInstance` in C++).
#[derive(Debug)]
pub struct Database {
    instance: Arc<DatabaseInstance>,
    path: Option<PathBuf>,
    read_only: bool,
}

impl Database {
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            instance: DatabaseInstance::new(),
            path: None,
            read_only: false,
        })
    }

    /// Opens a database file. What is in the file is read into memory, and it takes no writes
    /// until storing them is ported. If there is no file, the database lives in memory and
    /// nothing is written to `path` yet.
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self> {
        Ok(Self {
            instance: DatabaseInstance::open_file(path.as_ref(), read_only)?,
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
        Ok(Connection {
            context: ClientContext::new(self.instance.clone()),
        })
    }
}

/// A connection to a [`Database`].
#[derive(Debug)]
pub struct Connection {
    context: ClientContext,
}

impl Connection {
    /// Runs one or more `;`-separated statements and returns the result of the last one.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult> {
        self.context.query(sql)
    }

    /// Prepares one statement, with `?` or `$1` for values given when it runs.
    pub fn prepare(&mut self, sql: &str) -> Result<PreparedStatement> {
        Ok(PreparedStatement {
            data: self.context.prepare(sql)?,
        })
    }

    /// Runs a prepared statement with values for its parameters, in order.
    pub fn execute(&mut self, statement: &PreparedStatement, values: &[Value]) -> Result<QueryResult> {
        self.context.execute_prepared(&statement.data, values)
    }
}

/// A statement prepared on a [`Connection`].
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    data: Arc<PreparedStatementData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_run() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        let result = con.query("SELECT 42 AS answer").unwrap();
        assert_eq!(result.names, vec!["answer"]);
        assert_eq!(result.types, vec![LogicalType::INTEGER]);
        assert_eq!(result.value(0, 0), Value::integer(42));
        // connections share the database
        con.query("CREATE TABLE t (i INTEGER); INSERT INTO t VALUES (1), (2)")
            .unwrap();
        let mut other = db.connect().unwrap();
        assert_eq!(other.query("SELECT i FROM t").unwrap().row_count(), 2);
        assert_eq!(con.query("SELECT * FROM t, t AS u").unwrap().row_count(), 4);
        let prepared = con.prepare("SELECT i FROM t WHERE i = $1").unwrap();
        let result = con.execute(&prepared, &[Value::integer(2)]).unwrap();
        assert_eq!(result.value(0, 0), Value::integer(2));
        let result = con.query("SELECT * FROM t WHERE i IN (SELECT i FROM t)").unwrap();
        assert_eq!(result.row_count(), 2);
        let err = con.query("ATTACH 'x.db' AS x").unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotImplemented);
    }
}
