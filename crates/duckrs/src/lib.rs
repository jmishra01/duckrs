//! duckrs: an analytical SQL database engine, ported from DuckDB.
//!
//! A [`Database`] is in memory or in a file; a [`Connection`] runs SQL on it ([`Connection::query`],
//! [`Connection::prepare`]), adds rows with an [`Appender`], gives results as Arrow record batches
//! ([`Connection::query_arrow`]) or a chunk at a time ([`Connection::stream`]), and runs [`Relation`]s, queries built a
//! step at a time.

/// The Arrow crate the record batches of [`Connection::query_arrow`] are of.
pub use ::arrow;
pub use duckrs_common::types::{LogicalType, LogicalTypeId};
pub use duckrs_common::value::Value;
pub use duckrs_common::vector::DataChunk;
pub use duckrs_common::{Error, ErrorKind, Result};
pub use duckrs_core::client::MaterializedQueryResult as QueryResult;

mod arrow_export;
pub use arrow_export::{arrow_schema, arrow_type, record_batches};
mod relation;
pub use relation::{JoinKind, Relation};

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

    /// Opens a database file, or makes it if there is none and `read_only` isn't set. Tables stay in
    /// the file. A commit goes to its write-ahead log, which survives a crash, and the log goes
    /// into the file on `CHECKPOINT`, when it gets large and when the database goes away.
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self> {
        Ok(Self {
            instance: DatabaseInstance::open_file(path.as_ref(), read_only)?,
            path: Some(path.as_ref().to_path_buf()),
            read_only,
        })
    }

    /// [`Database::open`], making a new file in the storage version named `version` (e.g. `v1.0.0`),
    /// as the `storage_compatibility_version` of C++'s config does.
    pub fn open_with_storage_version(path: impl AsRef<Path>, read_only: bool, version: &str) -> Result<Self> {
        Ok(Self {
            instance: DatabaseInstance::open_file_with_version(path.as_ref(), read_only, version)?,
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

    /// Runs `sql` and gives its rows as Arrow record batches of up to `batch_rows` rows
    /// (`QueryResult::FetchArrowTable`).
    pub fn query_arrow(&mut self, sql: &str, batch_rows: usize) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        let result = self.query(sql)?;
        record_batches(&result, batch_rows)
    }

    /// Runs `sql` and gives its rows a chunk at a time ([`ChunkedResult::fetch`], as C++'s `QueryResult::Fetch`).
    /// The rows are all there when it returns: the chunks are those of the materialized result.
    pub fn stream(&mut self, sql: &str) -> Result<ChunkedResult> {
        Ok(ChunkedResult {
            result: self.query(sql)?,
            next: 0,
        })
    }

    /// Adds a replacement scan (`DBConfig::replacement_scans`): for a table name the catalog does not have,
    /// `scan` may give the rows to read instead, as column names, types and chunks.
    pub fn add_replacement_scan(
        &mut self,
        scan: impl Fn(&str) -> Option<Result<(Vec<String>, Vec<LogicalType>, std::sync::Arc<Vec<DataChunk>>)>>
        + Send
        + Sync
        + 'static,
    ) {
        self.context.add_replacement_scan(std::sync::Arc::new(scan));
    }

    /// Runs a prepared statement with values for its parameters, in order.
    pub fn execute(&mut self, statement: &PreparedStatement, values: &[Value]) -> Result<QueryResult> {
        self.context.execute_prepared(&statement.data, values)
    }
}

impl Connection {
    /// An [`Appender`] that adds rows to the table `table` of the default schema.
    pub fn appender(&mut self, table: &str) -> Result<Appender<'_>> {
        self.appender_in(None, table)
    }

    /// An [`Appender`] that adds rows to `schema.table`.
    pub fn appender_in(&mut self, schema: Option<&str>, table: &str) -> Result<Appender<'_>> {
        let columns = self.context.table_columns(schema, table)?;
        Ok(Appender {
            connection: self,
            schema: schema.map(str::to_string),
            table: table.to_string(),
            columns,
            rows: Vec::new(),
            row: Vec::new(),
        })
    }
}

/// Adds rows to a table, many at a time (`Appender`): rows are kept until [`Appender::flush`], which inserts them
/// as one INSERT would, and which happens on its own when enough have been added and when it goes away.
pub struct Appender<'c> {
    connection: &'c mut Connection,
    schema: Option<String>,
    table: String,
    columns: Vec<(String, LogicalType)>,
    rows: Vec<Vec<Value>>,
    row: Vec<Value>,
}

/// `Appender::FLUSH_COUNT`: the rows it keeps before it flushes them.
const APPENDER_FLUSH_ROWS: usize = 204_800;

impl Appender<'_> {
    /// The types of the columns, in order.
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|(_, ty)| ty.clone()).collect()
    }

    /// The next value of the current row, cast to the type of its column.
    pub fn append(&mut self, value: impl Into<Value>) -> Result<()> {
        let value = value.into();
        let Some((_, ty)) = self.columns.get(self.row.len()) else {
            return Err(Error::new(ErrorKind::InvalidInput, "Too many appends for chunk!"));
        };
        let cast = if value.is_null() {
            Value::null(ty.clone())
        } else if value.logical_type() == ty {
            value
        } else {
            duckrs_core::execution::parquet::try_cast_value(&value, ty).ok_or_else(|| {
                Error::new(
                    ErrorKind::Conversion,
                    format!(
                        "Could not convert '{}' to {ty}",
                        value.try_to_string().unwrap_or_default()
                    ),
                )
            })?
        };
        self.row.push(cast);
        Ok(())
    }

    /// `Appender::EndRow`: the current row is complete.
    pub fn end_row(&mut self) -> Result<()> {
        if self.row.len() != self.columns.len() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Call to EndRow before all columns have been appended to!",
            ));
        }
        self.rows.push(std::mem::take(&mut self.row));
        if self.rows.len() >= APPENDER_FLUSH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    /// A whole row at once.
    pub fn append_row(&mut self, values: impl IntoIterator<Item = Value>) -> Result<()> {
        for value in values {
            self.append(value)?;
        }
        self.end_row()
    }

    /// `Appender::Flush`: inserts the rows added so far.
    pub fn flush(&mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let types = self.types();
        let rows = std::mem::take(&mut self.rows);
        let mut chunks = Vec::new();
        for batch in rows.chunks(duckrs_common::STANDARD_VECTOR_SIZE) {
            let mut columns = Vec::with_capacity(types.len());
            for (column, ty) in types.iter().enumerate() {
                let mut vector = duckrs_common::vector::Vector::new(ty.clone(), batch.len());
                for (at, row) in batch.iter().enumerate() {
                    vector.set_value(at, &row[column])?;
                }
                columns.push(vector);
            }
            chunks.push(duckrs_common::vector::DataChunk::from_columns(columns, batch.len()));
        }
        self.connection
            .context
            .append(self.schema.as_deref(), &self.table, self.columns.clone(), chunks)
    }

    /// `Appender::Close`: flushes what is left.
    pub fn close(mut self) -> Result<()> {
        self.flush()
    }
}

impl Drop for Appender<'_> {
    /// Like C++'s destructor, a last flush whose error goes nowhere.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// The rows of a query, a chunk of up to `STANDARD_VECTOR_SIZE` rows at a time.
#[derive(Debug)]
pub struct ChunkedResult {
    result: QueryResult,
    next: usize,
}

impl ChunkedResult {
    pub fn names(&self) -> &[String] {
        &self.result.names
    }

    pub fn types(&self) -> &[LogicalType] {
        &self.result.types
    }

    /// `QueryResult::Fetch`: the next chunk, or `None` after the last.
    pub fn fetch(&mut self) -> Option<DataChunk> {
        let collection = &self.result.collection;
        while self.next < collection.chunk_count() {
            let chunk = collection.chunk(self.next);
            self.next += 1;
            if !chunk.is_empty() {
                return Some(chunk.clone());
            }
        }
        None
    }
}

impl Iterator for ChunkedResult {
    type Item = DataChunk;

    fn next(&mut self) -> Option<DataChunk> {
        self.fetch()
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
        let err = con.query("ATTACH ':memory:' AS x (ENCRYPTION_KEY 'key')").unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotImplemented);
    }

    #[test]
    fn results_become_record_batches() {
        use arrow::array::{Array, AsArray};
        use arrow::datatypes::DataType;
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        let batches = con
            .query_arrow(
                "SELECT i::INTEGER AS i, 'v' || i AS s, (i * 1.5)::DECIMAL(9, 2) AS d, DATE '2024-01-01' + i::INTEGER AS \
                 day, [i, NULL] AS l, {'a': i, 'b': 'x'} AS st, MAP {'k': i} AS m, CASE WHEN i % 2 = 0 THEN NULL ELSE \
                 i::HUGEINT END AS h, INTERVAL 1 MONTH AS iv FROM range(5) t(i)",
                3,
            )
            .unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), 3);
        assert_eq!(batches[1].num_rows(), 2);
        let schema = batches[0].schema();
        let types: Vec<&DataType> = schema.fields().iter().map(|f| f.data_type()).collect();
        assert_eq!(types[0], &DataType::Int32);
        assert_eq!(types[1], &DataType::Utf8);
        assert_eq!(types[2], &DataType::Decimal128(9, 2));
        assert_eq!(types[3], &DataType::Date32);
        assert!(matches!(types[4], DataType::List(_)));
        assert!(matches!(types[5], DataType::Struct(_)));
        assert!(matches!(types[6], DataType::Map(_, _)));
        assert_eq!(types[7], &DataType::Decimal128(38, 0));
        let ints = batches[1].column(0).as_primitive::<arrow::datatypes::Int32Type>();
        assert_eq!(ints.values().to_vec(), vec![3, 4]);
        assert_eq!(batches[0].column(1).as_string::<i32>().value(2), "v2");
        assert_eq!(
            batches[0]
                .column(2)
                .as_primitive::<arrow::datatypes::Decimal128Type>()
                .value(1),
            150
        );
        assert!(batches[0].column(7).is_null(0) && !batches[0].column(7).is_null(1));
        let list = batches[0].column(4).as_list::<i32>();
        assert_eq!(list.value(1).len(), 2);
        // an empty result is one empty batch
        let empty = con.query_arrow("SELECT 1 AS x WHERE false", 10).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].num_rows(), 0);
    }

    #[test]
    fn relations_run_their_queries() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        con.query("CREATE TABLE t AS SELECT i, i % 3 AS g FROM range(10) r(i)")
            .unwrap();
        let rel = Relation::table("t")
            .filter("i >= 2")
            .aggregate(&["g", "count(*) AS n", "sum(i) AS total"], &["g"])
            .order(&["g"]);
        let result = rel.execute(&mut con).unwrap();
        assert_eq!(result.row_count(), 3);
        assert_eq!(result.names, vec!["g", "n", "total"]);
        // g = 0 holds 3, 6 and 9
        assert_eq!(result.value(1, 0), Value::bigint(3));
        let joined = Relation::table("t")
            .join(
                &Relation::values(&[&["0", "'zero'"], &["1", "'one'"]], &["k", "name"]),
                "a",
                "b",
                "a.g = b.k",
                JoinKind::Inner,
            )
            .project(&["name", "i"])
            .order(&["i"])
            .limit(2, 1);
        let result = joined.execute(&mut con).unwrap();
        // the rows of g 0 and 1 by i, past the first: 1 ('one'), then 3 ('zero')
        assert_eq!(result.value(0, 0), Value::varchar("one"));
        assert_eq!(result.value(1, 0), Value::bigint(1));
        assert_eq!(result.value(0, 1), Value::varchar("zero"));
        assert_eq!(result.value(1, 1), Value::bigint(3));
        let distinct = Relation::table("t")
            .project(&["g"])
            .distinct()
            .union(&Relation::from_sql("SELECT 7"));
        assert_eq!(distinct.execute(&mut con).unwrap().row_count(), 4);
        Relation::table("t")
            .filter("i < 3")
            .create_table(&mut con, "small")
            .unwrap();
        assert_eq!(
            con.query("SELECT count(*) FROM small").unwrap().value(0, 0),
            Value::bigint(3)
        );
        assert!(Relation::table("missing").execute(&mut con).is_err());
    }

    #[test]
    fn results_come_in_chunks() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        let mut result = con.stream("SELECT i FROM range(5000) t(i)").unwrap();
        assert_eq!(result.names(), ["i"]);
        let sizes: Vec<usize> = std::iter::from_fn(|| result.fetch()).map(|chunk| chunk.len()).collect();
        assert_eq!(sizes.iter().sum::<usize>(), 5000);
        assert!(sizes.iter().all(|size| *size <= duckrs_common::STANDARD_VECTOR_SIZE));
        assert!(result.fetch().is_none());
        assert_eq!(con.stream("SELECT 1 WHERE false").unwrap().count(), 0);
    }

    #[test]
    fn appenders_add_rows() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        con.query("CREATE TABLE t (i INTEGER, s VARCHAR, d DOUBLE)").unwrap();
        let mut appender = con.appender("t").unwrap();
        for i in 0..5000 {
            appender.append(Value::bigint(i)).unwrap();
            appender.append(Value::varchar(format!("row {i}"))).unwrap();
            appender.append(Value::null(LogicalType::DOUBLE)).unwrap();
            appender.end_row().unwrap();
        }
        appender
            .append_row([Value::integer(-1), Value::varchar("last"), Value::double(1.5)])
            .unwrap();
        // a row of too few or too many values is an error
        appender.append(Value::integer(1)).unwrap();
        assert_eq!(
            appender.end_row().unwrap_err().message,
            "Call to EndRow before all columns have been appended to!"
        );
        appender.append(Value::varchar("x")).unwrap();
        appender.append(Value::double(0.0)).unwrap();
        assert_eq!(
            appender.append(Value::integer(1)).unwrap_err().message,
            "Too many appends for chunk!"
        );
        appender.end_row().unwrap();
        appender.close().unwrap();
        let result = con.query("SELECT count(*), sum(i), count(d), max(s) FROM t").unwrap();
        assert_eq!(result.value(0, 0), Value::bigint(5002));
        assert_eq!(result.value(1, 0), Value::hugeint(12_497_500));
        assert_eq!(result.value(2, 0), Value::bigint(2));
        assert_eq!(result.value(3, 0), Value::varchar("x"));
        // a value that doesn't cast, and a table that isn't there
        let mut appender = con.appender("t").unwrap();
        assert_eq!(
            appender.append(Value::varchar("abc")).unwrap_err().kind,
            ErrorKind::Conversion
        );
        drop(appender);
        assert!(con.appender("nope").is_err());
        // a constraint fails the flush, and nothing of it stays
        con.query("CREATE TABLE k (i INTEGER PRIMARY KEY)").unwrap();
        let mut appender = con.appender("k").unwrap();
        appender.append_row([Value::integer(1)]).unwrap();
        appender.append_row([Value::integer(1)]).unwrap();
        assert!(appender.flush().is_err());
        drop(appender);
        assert_eq!(
            con.query("SELECT count(*) FROM k").unwrap().value(0, 0),
            Value::bigint(0)
        );
    }
}
