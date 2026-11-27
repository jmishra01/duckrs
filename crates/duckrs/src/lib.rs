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
pub use duckrs_main::MaterializedQueryResult as QueryResult;
pub use duckrs_main::with_error_location;
pub use duckrs_main::{StatementProperties, StatementReturnType, StatementType};
pub use duckrs_main::{TableDescription, TableDescriptionColumn};
/// What a replacement scan reads in place of a table ([`Database::add_replacement_scan`]).
pub use duckrs_core::planner::binder::Replacement;

/// The configuration options (`DBConfig::GetOptionByIndex` / `GetOptionByName`).
pub mod settings {
    pub use duckrs_main::settings::{SettingDefinition, all_settings, find_setting};
}

/// `Value::DefaultCastAs`: `value` cast to `ty`, as a SQL `CAST` would.
pub fn cast_value(value: &Value, ty: &LogicalType) -> Result<Value> {
    duckrs_core::function::cast::cast_value(value, ty)
}

/// `VectorOperations::Cast`: the first `count` rows of `source` cast to `ty`, as a SQL `CAST` would.
pub fn cast_vector(source: &duckrs_common::vector::Vector, ty: &LogicalType, count: usize) -> Result<duckrs_common::vector::Vector> {
    let mut result = duckrs_common::vector::Vector::new(ty.clone(), count);
    duckrs_core::function::cast::cast_vector(source, &mut result, count, duckrs_core::function::cast::CastMode::Strict)?;
    Ok(result)
}

/// User-defined functions: scalar, aggregate and table functions a client registers on a database
/// ([`Connection::register_scalar_functions`], ...) and implements through the traits here, which the engine calls
/// on whole chunks.
pub mod udf {
    pub use duckrs_core::function::udf::*;
    /// A bound expression, as a scalar function's bind sees its arguments (`duckdb_expression`).
    pub use duckrs_core::planner::expression::BoundExpr as Expression;

    /// `ExpressionExecutor::EvaluateScalar`: the value of a foldable expression.
    pub fn fold(expression: &Expression) -> crate::Result<crate::Value> {
        duckrs_core::execution::expression_executor::evaluate_scalar(expression)
    }
}

mod arrow_export;
pub use arrow_export::{arrow_schema, arrow_type, record_batches};
mod relation;
pub use relation::{JoinKind, Relation};

use duckrs_main::{ClientContext, DatabaseInstance, PreparedStatementData};
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

    /// Adds a replacement scan to the database (`DBConfig::replacement_scans`): for a table name the catalog does
    /// not have, `scan` may give what to read instead, rows or a table function call. Every connection to the
    /// database, those open now too, consults it after the scans of its own
    /// ([`Connection::add_replacement_scan`]); an error from it fails the query.
    pub fn add_replacement_scan(&self, scan: impl Fn(&str) -> Option<Result<Replacement>> + Send + Sync + 'static) {
        self.instance.add_replacement_scan(Arc::new(scan));
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
        self.context.add_replacement_scan(std::sync::Arc::new(move |name: &str| {
            scan(name).map(|rows| rows.map(Replacement::Rows))
        }));
    }

    /// The current value of setting `name` for this connection (`current_setting`), or `None` for no such setting.
    pub fn setting(&self, name: &str) -> Option<Value> {
        self.context.setting(name)
    }

    /// `ClientContext::GetConnectionId`: what tells this connection apart from the others of the process.
    pub fn connection_id(&self) -> u64 {
        self.context.connection_id()
    }

    /// Registers scalar functions under one name (`duckdb_register_scalar_function_set`): each overload joins
    /// those registered before, taking the place of one with the same parameters. Every connection to the database
    /// can call them.
    pub fn register_scalar_functions(&mut self, set: udf::ScalarUdfSet) -> Result<()> {
        self.context.user_functions().register_scalar(set)
    }

    /// Registers aggregate functions under one name (`duckdb_register_aggregate_function_set`); a name that has
    /// them already is an error.
    pub fn register_aggregate_functions(&mut self, set: udf::AggregateFunctionSet) -> Result<()> {
        self.context.user_functions().register_aggregate(set)
    }

    /// Registers table functions under one name (`duckdb_register_table_function`): each overload joins those
    /// registered before, taking the place of one with the same parameters.
    pub fn register_table_functions(&mut self, set: udf::TableUdfSet) -> Result<()> {
        self.context.user_functions().register_table(set)
    }

    /// Runs a prepared statement with values for its parameters, in order.
    pub fn execute(&mut self, statement: &PreparedStatement, values: &[Value]) -> Result<QueryResult> {
        self.context.execute_prepared(&statement.data, values)
    }

    /// Runs a prepared statement with values for its parameters by name ([`PreparedStatement::parameter_names`]).
    pub fn execute_named(
        &mut self,
        statement: &PreparedStatement,
        values: std::collections::HashMap<String, Value>,
    ) -> Result<QueryResult> {
        self.context.execute_prepared_named(&statement.data, values)
    }

    /// The type of the last statement of `sql`, and what its result holds (`StatementProperties`); `None` when it
    /// doesn't parse.
    pub fn statement_properties(&self, sql: &str) -> Option<StatementProperties> {
        self.context.statement_properties(sql)
    }

    /// The text of each statement of `sql` (`Connection::ExtractStatements`), to prepare them one at a time.
    pub fn extract_statements(&mut self, sql: &str) -> Result<Vec<String>> {
        self.context.extract_statements(sql)
    }

    /// The names and types of the columns of `catalog.schema.table` (`ClientContext::TableInfo`); without a catalog,
    /// of the database the connection uses.
    pub fn table_columns(
        &self,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<(String, LogicalType)>> {
        self.context.table_columns_in(catalog, schema, table)
    }

    /// The DEFAULT of each column of `schema.table` folded to a constant (NULL without one), or `None` for a
    /// default that does not fold, as `Appender::AppendDefault` reads them.
    pub fn table_column_defaults(
        &self,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<Option<Value>>> {
        self.context.table_column_defaults(catalog, schema, table)
    }

    /// `ClientContext::TableInfo`: the table `catalog.schema.table` and its columns (without a catalog, in the
    /// database the connection uses), or `None` when there is no such table, a view not being one.
    pub fn table_info(
        &self,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Option<TableDescription>> {
        self.context.table_info(catalog, schema, table)
    }

    /// `ClientContext::Append`: inserts `chunks`, whose columns are `columns` of `schema.table` (names and types,
    /// as [`Connection::table_columns`] gives them), as one INSERT would.
    pub fn append_chunks(
        &mut self,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: &str,
        columns: Vec<(String, LogicalType)>,
        chunks: Vec<DataChunk>,
    ) -> Result<()> {
        self.context.append_in(catalog, schema, table, columns, chunks)
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

impl PreparedStatement {
    /// The names of its parameters, lowercase, in the order they appear: `1`, `2`, ... for `?` and `$1`.
    pub fn parameter_names(&self) -> &[String] {
        self.data.parameter_names()
    }

    /// The type that preparing found for parameter `name` (as [`PreparedStatement::parameter_names`] gives it), from
    /// what the statement does with it (`TryGetParameterType`): `i = $1` with an INTEGER `i`, `CAST($1 AS BIGINT)`,
    /// an INSERT into a column. `None` when its type is only known once a value is bound.
    pub fn parameter_type(&self, name: &str) -> Option<&LogicalType> {
        self.data.parameter_type(name)
    }

    /// The parameters whose types preparing found, by name (`GetExpectedParameterTypes`).
    pub fn parameter_types(&self) -> &std::collections::HashMap<String, LogicalType> {
        self.data.parameter_types()
    }

    /// The names and types of its result columns (`GetNames`, `GetTypes`), when it was bound as it was prepared.
    pub fn result_columns(&self) -> Option<(&[String], &[LogicalType])> {
        self.data.result_columns()
    }

    /// Its type, and what its result holds (`GetStatementType`, `GetStatementProperties`).
    pub fn properties(&self) -> StatementProperties {
        self.data.properties()
    }
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
        // what is not ported yet is a "not implemented" error (loading extensions comes after v1)
        let err = con.query("LOAD httpfs").unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotImplemented);
    }

    #[test]
    fn prepared_parameters_take_types_from_their_uses() {
        let db = Database::open_in_memory().unwrap();
        let mut con = db.connect().unwrap();
        con.query("CREATE TABLE a (i INTEGER, s VARCHAR)").unwrap();
        let type_of = |con: &mut Connection, sql: &str, name: &str| {
            con.prepare(sql).unwrap().parameter_type(name).map(LogicalType::to_string)
        };
        let integer = Some("INTEGER".to_string());
        assert_eq!(type_of(&mut con, "INSERT INTO a VALUES (?, 'x')", "1"), integer);
        assert_eq!(type_of(&mut con, "SELECT sum(i) FROM a WHERE i > ?", "1"), integer);
        assert_eq!(type_of(&mut con, "SELECT * FROM a WHERE s = $1", "1"), Some("VARCHAR".to_string()));
        assert_eq!(type_of(&mut con, "SELECT CAST($v AS BIGINT)", "v"), Some("BIGINT".to_string()));
        assert_eq!(type_of(&mut con, "SELECT CAST(CAST($1 AS INTEGER) AS VARCHAR)", "1"), integer);
        // uses that disagree leave the parameter without a type, as does a use that gives none
        assert_eq!(type_of(&mut con, "SELECT $1::INTEGER + $1::BIGINT", "1"), None);
        let prepared = con.prepare("SELECT $1::INTEGER, $2").unwrap();
        assert_eq!(prepared.parameter_type("1"), Some(&LogicalType::INTEGER));
        assert_eq!(prepared.parameter_type("2"), None);
        // a result column of a parameter of no type is `unknown` (`ParameterNotResolvedException`)
        let (names, types) = prepared.result_columns().unwrap();
        assert_eq!(names, ["unknown"]);
        assert_eq!(types[0].id(), LogicalTypeId::Unknown);
        // values are still bound when it runs
        let result = con.execute(&prepared, &[Value::integer(1), Value::varchar("b")]).unwrap();
        assert_eq!(result.value(1, 0), Value::varchar("b"));
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
