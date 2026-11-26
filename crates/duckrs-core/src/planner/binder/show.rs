// Ported from duckdb/src/planner/binder/tableref/bind_showref.cpp,
// duckdb/src/planner/binder/statement/bind_summarize.cpp, the column information of
// duckdb/src/function/table/system/pragma_table_info.cpp and the catalog listings of
// duckdb/src/function/table/system/duckdb_{tables,views,columns,schemas,databases,settings}.cpp,
// the binds of duckdb/src/function/table/system/duckdb_{secrets,secret_types,secret_type_parameters,
// which_secret}.cpp @ rust-port-baseline
//
//! SHOW, DESCRIBE, SUMMARIZE and the `duckdb_*` listings of the catalog. They are rows computed
//! while binding, as `LogicalColumnDataGet` holds in C++.

use super::table_functions::parse_select;
use super::{Binder, ShowBehavior, binder_error};
use crate::catalog::{SchemaCatalogEntry, SegmentInfo, TEMP_SCHEMA, TableCatalogEntry, TableConstraint, TableOrView};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{COLUMN_IDENTIFIER_ROW_ID, LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::{ErrorKind, Result};
use duckrs_parser::ast::{
    ConstraintCheckMode, Expr, ExprKind, QualifiedName, QueryNode, SelectNode, SelectStatement, ShowRef, ShowType,
    TableRef, TableRefKind, sql_identifier,
};
use duckrs_parser::transformer::visit_expr_mut;
use std::sync::Arc;

fn text(value: &str) -> Value {
    Value::varchar(value)
}

fn null_text() -> Value {
    Value::null(LogicalType::VARCHAR)
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `CheckConstraints`: whether a column is in a primary key and in a UNIQUE.
fn column_keys(table: &TableCatalogEntry, column: usize) -> (bool, bool) {
    let (mut primary, mut unique) = (false, false);
    for constraint in &table.constraints {
        if let TableConstraint::Unique {
            columns, primary_key, ..
        } = constraint
            && columns.contains(&column)
        {
            if *primary_key {
                primary = true;
            } else {
                unique = true;
            }
        }
    }
    (primary, unique)
}

/// The row of `DESCRIBE` for a column of a table, `name` taking the place of the column's name.
fn column_info(table: &TableCatalogEntry, name: &str, column: &str) -> Vec<Value> {
    let logical = table.logical_columns();
    let found = logical
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(column))
        .expect("a column of the table");
    let (primary, unique) = table
        .column_index(column)
        .map_or((false, false), |i| column_keys(table, i));
    let not_null = found.definition.is_some_and(|d| d.not_null) || primary;
    let key = if primary {
        text("PRI")
    } else if unique {
        text("UNI")
    } else {
        null_text()
    };
    let default = match (found.generated, found.definition.and_then(|d| d.default_text.as_ref())) {
        (Some(expr), _) => text(&expr.to_string()),
        (None, Some(default)) => text(default),
        _ => null_text(),
    };
    vec![
        text(name),
        text(&found.ty.to_string()),
        text(if not_null { "NO" } else { "YES" }),
        key,
        default,
        null_text(),
    ]
}

fn plain_column_info(name: &str, ty: &LogicalType) -> Vec<Value> {
    vec![
        text(name),
        text(&ty.to_string()),
        text("YES"),
        null_text(),
        null_text(),
        null_text(),
    ]
}

/// `FindBaseTableColumn`: the column of a base table that an output column of `op` passes on.
fn base_table_column(op: &LogicalOperator, binding: ColumnBinding) -> Option<(Arc<TableCatalogEntry>, usize)> {
    match &op.kind {
        LogicalOperatorKind::Get {
            table,
            table_index,
            column_ids,
            ..
        } => {
            if *table_index != binding.table_index {
                return None;
            }
            let column = column_ids[binding.column_index];
            (column != COLUMN_IDENTIFIER_ROW_ID).then(|| (table.clone(), column))
        }
        LogicalOperatorKind::Projection {
            table_index,
            expressions,
        } => {
            if *table_index != binding.table_index {
                return None;
            }
            match &expressions[binding.column_index].kind {
                BoundExprKind::ColumnRef { binding, .. } => base_table_column(&op.children[0], *binding),
                _ => None,
            }
        }
        LogicalOperatorKind::Limit { .. }
        | LogicalOperatorKind::Order { .. }
        | LogicalOperatorKind::TopN { .. }
        | LogicalOperatorKind::Distinct { .. }
        | LogicalOperatorKind::Filter { .. }
        | LogicalOperatorKind::Sample { .. }
        | LogicalOperatorKind::LimitPercent { .. }
        | LogicalOperatorKind::Join { .. }
        | LogicalOperatorKind::CrossProduct => op.children.iter().find_map(|c| base_table_column(c, binding)),
        _ => None,
    }
}

/// `StringUtil::BytesToHumanReadableString`.
pub fn human_readable_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut parts = [bytes, 0, 0, 0, 0, 0];
    for index in 1..parts.len() {
        parts[index] = parts[index - 1] / 1024;
        parts[index - 1] %= 1024;
    }
    for index in (1..parts.len()).rev() {
        if parts[index] != 0 {
            return format!("{}.{} {}", parts[index], parts[index - 1] * 10 / 1024, UNITS[index]);
        }
    }
    format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" })
}

/// `DuckDB::ReleaseCodename`.
fn release_codename(version: &str) -> &'static str {
    // `VersioningUtils::IsReleaseVersion`
    if version.contains("-dev") {
        return "Development Version";
    }
    const NAMES: [(&str, &str); 6] = [
        ("v1.2.", "Histrionicus"),
        ("v1.3.", "Ossivalis"),
        ("v1.4.", "Andium"),
        ("v1.5.", "Variegata"),
        ("v2.0.", "Cyanoptera"),
        ("v2.1.", "Islandica"),
    ];
    NAMES
        .iter()
        .find(|(prefix, _)| version.starts_with(prefix))
        .map_or("Unknown Version", |(_, name)| name)
}

/// `DuckDBPlatform`: the operating system and architecture, as extension repositories name them.
pub(crate) fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        "windows" => "windows",
        "freebsd" => "freebsd",
        _ => "linux",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "loongarch64" => "loong64",
        "riscv64" => "riscv64",
        "x86" => "i686",
        _ => "amd64",
    };
    let postfix = if os == "windows" && cfg!(target_env = "gnu") {
        "_mingw"
    } else {
        ""
    };
    format!("{os}_{arch}{postfix}")
}

/// `internal_extensions` of `extension_helper.cpp`.
const DEFAULT_EXTENSIONS: [(&str, &str); 30] = [
    ("core_functions", "Core function library"),
    (
        "icu",
        "Adds support for time zones and collations using the ICU library",
    ),
    ("excel", "Adds support for Excel-like format strings"),
    ("parquet", "Adds support for reading and writing parquet files"),
    ("tpch", "Adds TPC-H data generation and query support"),
    ("tpcds", "Adds TPC-DS data generation and query support"),
    (
        "httpfs",
        "Adds support for reading and writing files over a HTTP(S) connection",
    ),
    ("json", "Adds support for JSON operations"),
    ("autocomplete", "Adds support for autocomplete in the shell"),
    ("motherduck", "Enables motherduck integration with the system"),
    ("mysql_scanner", "Adds support for connecting to a MySQL database"),
    (
        "odbc_scanner",
        "Adds support for connecting to remote databases over ODBC",
    ),
    (
        "sqlite_scanner",
        "Adds support for reading and writing SQLite database files",
    ),
    ("postgres_scanner", "Adds support for connecting to a Postgres database"),
    ("inet", "Adds support for IP-related data types and functions"),
    (
        "spatial",
        "Geospatial extension that adds support for working with spatial data and functions",
    ),
    ("aws", "Provides features that depend on the AWS SDK"),
    (
        "azure",
        "Adds a filesystem abstraction for Azure blob storage to DuckDB",
    ),
    ("encodings", "All unicode encodings to UTF-8"),
    ("iceberg", "Adds support for Apache Iceberg"),
    ("vss", "Adds indexing support to accelerate Vector Similarity Search"),
    ("delta", "Adds support for Delta Lake"),
    ("fts", "Adds support for Full-Text Search Indexes"),
    ("ui", "Adds local UI for DuckDB"),
    ("ducklake", "Adds support for DuckLake, SQL as a Lakehouse Format"),
    ("quack", "The DuckDB 'Quack' Client/Server Protocol"),
    (
        "vortex",
        "Adds support for reading and writing files using the Vortex file format",
    ),
    ("lance", "Adds support for querying Lance datasets"),
    ("avro", "Adds support for reading Avro files"),
    ("unity_catalog", "Adds support for connecting to Unity Catalog"),
];

/// `internal_aliases` of `extension_alias.cpp`: (alias, extension).
const EXTENSION_ALIASES: [(&str, &str); 10] = [
    ("http", "httpfs"),
    ("https", "httpfs"),
    ("md", "motherduck"),
    ("mysql", "mysql_scanner"),
    ("odbc", "odbc_scanner"),
    ("s3", "httpfs"),
    ("postgres", "postgres_scanner"),
    ("sqlite", "sqlite_scanner"),
    ("sqlite3", "sqlite_scanner"),
    ("uc_catalog", "unity_catalog"),
];

/// The extensions duckrs has compiled in, as C++'s `linked_extensions` of a build with them.
const LINKED_EXTENSIONS: [&str; 4] = ["core_functions", "json", "parquet", "tpch"];

const DESCRIBE_NAMES: [&str; 6] = ["column_name", "column_type", "null", "key", "default", "extra"];

const SYSTEM_TABLES: [&str; 21] = [
    "duckdb_functions",
    "duckdb_prepared_statements",
    "pragma_collations",
    "duckdb_extensions",
    "pragma_user_agent",
    "duckdb_dialects",
    "duckdb_keywords",
    "pragma_version",
    "pragma_platform",
    "duckdb_triggers",
    "duckdb_types",
    "duckdb_sequences",
    "duckdb_optimizers",
    "pragma_database_size",
    "duckdb_databases",
    "duckdb_schemas",
    "duckdb_tables",
    "duckdb_views",
    "duckdb_columns",
    "duckdb_indexes",
    "duckdb_constraints",
];

/// What `pragma_storage_info` lists of rows that no checkpoint wrote: per row group each column and
/// its validity, in one segment.
fn memory_segments(table: &TableCatalogEntry) -> Vec<SegmentInfo> {
    let rows = table.row_count() as u64;
    let size = table.row_group_size() as u64;
    let mut segments = Vec::new();
    for (group, start) in (0..rows).step_by(table.row_group_size().max(1)).enumerate() {
        for (column, definition) in table.columns.iter().enumerate() {
            let parts = [
                (format!("[{column}]"), definition.ty.to_string()),
                (format!("[{column}, 0]"), "VALIDITY".to_string()),
            ];
            for (path, segment_type) in parts {
                segments.push(SegmentInfo {
                    row_group: group as u64,
                    column,
                    path,
                    segment: 0,
                    segment_type,
                    start,
                    count: size.min(rows - start),
                    compression: "Uncompressed",
                    block_id: -1,
                    block_offset: 0,
                    info: None,
                });
            }
        }
    }
    segments
}

/// `internal_optimizer_types` of duckdb/src/common/enums/optimizer_type.cpp: what
/// `duckdb_optimizers()` lists (`ListAllOptimizers`).
const OPTIMIZERS: [&str; 49] = [
    "expression_rewriter",
    "filter_pullup",
    "filter_pushdown",
    "empty_result_pullup",
    "cte_filter_pusher",
    "regex_range",
    "in_clause",
    "join_order",
    "deliminator",
    "unnest_rewriter",
    "unused_columns",
    "statistics_propagation",
    "common_subexpressions",
    "common_aggregate",
    "column_lifetime",
    "limit_pushdown",
    "row_group_pruner",
    "top_n",
    "top_n_window_elimination",
    "build_side_probe_side",
    "compressed_materialization",
    "duplicate_groups",
    "reorder_filter",
    "sampling_pushdown",
    "join_filter_pushdown",
    "extension",
    "materialized_cte",
    "aggregate_function_rewriter",
    "late_materialization",
    "cte_inlining",
    "common_subplan",
    "join_elimination",
    "window_self_join",
    "window_join_pushdown",
    "projection_pullup",
    "outer_join_simplification",
    "window_rewriter",
    "partitioned_execution",
    "partial_aggregate_pushdown",
    "remote_pushdown",
    "grouping_sets",
    "type_pushdown",
    "scalar_fn_pushdown",
    "distinct_aggregate_rewrite",
    "aggregate_reuse",
    "projection_placement",
    "redundant_order_keys",
    "disjunctive_join_rewriter",
    "redundant_order",
];

/// The columns and rows of a system table.
type SystemRows = (Vec<(&'static str, LogicalType)>, Vec<Vec<Value>>);

/// A table of the listing, with the ids the listings share.
struct ListedSchema {
    database: String,
    database_oid: i64,
    /// the schema's name as listed
    display: String,
    schema: Arc<SchemaCatalogEntry>,
    oid: i64,
}

struct ListedEntry {
    database: String,
    database_oid: i64,
    schema: Arc<SchemaCatalogEntry>,
    schema_oid: i64,
    oid: i64,
    entry: TableOrView,
}

impl Binder<'_> {
    /// Rows known while binding as a table called `default_alias`.
    pub(super) fn bind_rows(
        &mut self,
        table_ref: &TableRef,
        default_alias: &str,
        names: Vec<String>,
        types: Vec<LogicalType>,
        rows: Vec<Vec<Value>>,
    ) -> Result<LogicalOperator> {
        let alias = if table_ref.alias.is_empty() {
            default_alias.to_string()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(super::Binding {
            alias,
            schema: None,
            catalog: None,
            table_index,
            names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let expressions = rows
            .into_iter()
            .map(|row| row.into_iter().map(BoundExpr::constant).collect())
            .collect();
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ExpressionGet {
                function: None,
                volatile: false,
                materialized: true,
                table_index,
                types,
                expressions,
            },
            vec![],
        ))
    }

    fn bind_describe_rows(&mut self, table_ref: &TableRef, rows: Vec<Vec<Value>>) -> Result<LogicalOperator> {
        let names = DESCRIBE_NAMES.iter().map(|n| n.to_string()).collect();
        self.bind_rows(table_ref, "__show_select", names, vec![LogicalType::VARCHAR; 6], rows)
    }

    /// `Binder::BindDescribeQuery`.
    fn bind_describe_query(&mut self, table_ref: &TableRef, query: &QueryNode) -> Result<LogicalOperator> {
        let mut binder = self.child_binder();
        let bound = binder.bind_query_node(query);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let bindings = bound.plan.column_bindings();
        let mut rows = Vec::with_capacity(bound.names.len());
        for (i, name) in bound.names.iter().enumerate() {
            rows.push(match base_table_column(&bound.plan, bindings[i]) {
                Some((table, column)) => column_info(&table, name, &table.columns[column].name),
                None => plain_column_info(name, &bound.types[i]),
            });
        }
        self.bind_describe_rows(table_ref, rows)
    }

    /// The columns of a table or view as `DESCRIBE name` shows them.
    fn describe_entry(&mut self, name: &QualifiedName) -> Result<Vec<Vec<Value>>> {
        if name.qualification.is_empty()
            && let Some(index) = self.ctes.iter().rposition(|(n, ..)| n.eq_ignore_ascii_case(&name.name))
        {
            let reference = TableRef::from(TableRefKind::BaseTable {
                name: name.clone(),
                at_clause: None,
            });
            let mut binder = self.child_binder();
            let _ = index;
            binder.bind_table_ref(&reference)?;
            self.next_table_index = binder.next_table_index;
            let binding = &binder.bind_context.bindings[0];
            return Ok(binding
                .names
                .iter()
                .zip(&binding.types)
                .map(|(n, t)| plain_column_info(n, t))
                .collect());
        }
        let entry = match self.catalog.get_entry(
            super::schema_of(name)?.as_deref(),
            self.search_schema.as_deref(),
            &name.name,
        ) {
            Ok(entry) => entry,
            // the views of the system catalog (`information_schema`, `pg_catalog`) bind as a reference does
            Err(error) => {
                let reference = TableRef::from(TableRefKind::BaseTable {
                    name: name.clone(),
                    at_clause: None,
                });
                let mut binder = self.child_binder();
                if binder.bind_table_ref(&reference).is_err() {
                    return Err(error);
                }
                self.next_table_index = binder.next_table_index;
                let binding = &binder.bind_context.bindings[0];
                return Ok(binding
                    .names
                    .iter()
                    .zip(&binding.types)
                    .map(|(n, t)| plain_column_info(n, t))
                    .collect());
            }
        };
        Ok(match entry {
            TableOrView::Table(table) => table
                .logical_columns()
                .iter()
                .map(|c| column_info(&table, c.name, c.name))
                .collect(),
            TableOrView::View(view) => {
                let (names, types) = self.view_columns(&view)?;
                names.iter().zip(&types).map(|(n, t)| plain_column_info(n, t)).collect()
            }
        })
    }

    fn view_columns(&mut self, view: &crate::catalog::ViewCatalogEntry) -> Result<(Vec<String>, Vec<LogicalType>)> {
        let reference = TableRef::from(TableRefKind::BaseTable {
            name: QualifiedName {
                qualification: vec![view.schema.clone()],
                ..QualifiedName::new(view.name.clone())
            },
            at_clause: None,
        });
        let mut binder = self.child_binder();
        binder.bind_table_ref(&reference)?;
        self.next_table_index = binder.next_table_index;
        let binding = &binder.bind_context.bindings[0];
        Ok((binding.names.clone(), binding.types.clone()))
    }

    /// `PragmaShowTables`: the names on the search path.
    fn show_tables(&mut self, table_ref: &TableRef, schema: Option<&str>) -> Result<LogicalOperator> {
        self.show_tables_matching(table_ref, &|binder, listed| match schema {
            Some(schema) => listed.schema.name.eq_ignore_ascii_case(schema),
            // `in_search_path(database_name, schema_name)`
            None => binder.in_search_path(&listed.database, &listed.schema.name),
        })
    }

    /// `PragmaShowTables(database, schema)`: the tables and views of a database, or of one of its schemas.
    fn show_tables_from(
        &mut self,
        table_ref: &TableRef,
        database: &str,
        schema: Option<&str>,
    ) -> Result<LogicalOperator> {
        self.show_tables_matching(table_ref, &|_, listed| {
            listed.database.eq_ignore_ascii_case(database)
                && schema.is_none_or(|schema| {
                    // a nested schema goes by its whole path
                    let nested = listed.schema.path.len() > 1;
                    let name = if nested { listed.schema.key() } else { listed.schema.name.clone() };
                    name.eq_ignore_ascii_case(schema)
                })
        })
    }

    fn show_tables_matching(
        &mut self,
        table_ref: &TableRef,
        wanted: &dyn Fn(&Self, &ListedEntry) -> bool,
    ) -> Result<LogicalOperator> {
        let mut names: Vec<String> = Vec::new();
        for listed in self.listed_entries() {
            let wanted = wanted(self, &listed);
            if wanted {
                names.push(listed.entry.name().to_string());
            }
        }
        names.sort();
        let rows = names.into_iter().map(|n| vec![Value::varchar(n)]).collect();
        self.bind_rows(
            table_ref,
            "__show_select",
            vec!["name".to_string()],
            vec![LogicalType::VARCHAR],
            rows,
        )
    }

    /// `PragmaShowTablesExpanded`.
    fn show_all_tables(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        let text_list = LogicalType::list(LogicalType::VARCHAR);
        let mut rows = Vec::new();
        for listed in self.listed_entries() {
            let (names, types) = match &listed.entry {
                TableOrView::Table(table) => {
                    let columns = table.logical_columns();
                    (
                        columns.iter().map(|c| c.name.to_string()).collect(),
                        columns.iter().map(|c| c.ty.clone()).collect(),
                    )
                }
                TableOrView::View(view) => self.view_columns(view)?,
            };
            let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
            rows.push(vec![
                text(&listed.database),
                text(if temporary { "main" } else { &listed.schema.name }),
                text(listed.entry.name()),
                Value::list(LogicalType::VARCHAR, names.into_iter().map(Value::varchar).collect())?,
                Value::list(
                    LogicalType::VARCHAR,
                    types.iter().map(|t| Value::varchar(t.to_string())).collect(),
                )?,
                Value::boolean(temporary),
            ]);
        }
        rows.sort_by_key(|row| {
            row[..3]
                .iter()
                .map(|v| v.try_to_string().unwrap_or_default())
                .collect::<Vec<_>>()
        });
        let names = [
            "database",
            "schema",
            "name",
            "column_names",
            "column_types",
            "temporary",
        ];
        let types = vec![
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            text_list.clone(),
            text_list,
            LogicalType::BOOLEAN,
        ];
        self.bind_rows(
            table_ref,
            "__show_select",
            names.iter().map(|n| n.to_string()).collect(),
            types,
            rows,
        )
    }

    /// `Binder::TryBindShowSetting`.
    fn bind_show_setting(&mut self, table_ref: &TableRef, name: &str) -> Result<Option<LogicalOperator>> {
        let Some(session) = self.session.clone() else {
            return Ok(None);
        };
        let Ok(value) = (session.setting)(name) else {
            return Ok(None);
        };
        let ty = value.logical_type().clone();
        Ok(Some(self.bind_rows(
            table_ref,
            "__show_setting",
            vec![name.to_string()],
            vec![ty],
            vec![vec![value]],
        )?))
    }

    fn bind_describe(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        if let Some(query) = &show.query {
            return self.bind_describe_query(table_ref, query);
        }
        if show.show_type == ShowType::ShowFrom {
            let current = self
                .session
                .as_ref()
                .map_or_else(|| self.catalog.name.clone(), |session| session.database.clone());
            let (mut catalog, mut schema) = (show.catalog.clone(), show.schema.clone());
            if schema.is_empty() {
                std::mem::swap(&mut catalog, &mut schema);
            }
            let databases: Vec<String> = self.catalog.databases().iter().map(|d| d.name.clone()).collect();
            let is_database = |name: &str| databases.iter().any(|d| d.eq_ignore_ascii_case(name));
            // `BindSchemaOrCatalog`: a lone name that is a database and no schema of this one names the database
            if catalog.is_empty() && self.catalog.get_schema(&schema).is_none() && is_database(&schema) {
                catalog = std::mem::take(&mut schema);
            }
            let schema_exists = |binder: &Self, database: &str, schema: &str| {
                if database.eq_ignore_ascii_case(&current) {
                    return binder.catalog.get_schema(schema).is_some();
                }
                let mut found = false;
                binder
                    .catalog
                    .with_database(database, &mut |c| found = c.get_schema(schema).is_some());
                found
            };
            let not_found = |database: &str, schema: &str| {
                duckrs_common::Error::new(
                    ErrorKind::Catalog,
                    format!("SHOW TABLES FROM: No catalog + schema named \"{database}.{schema}\" found."),
                )
            };
            if catalog.is_empty() {
                if !schema_exists(self, &current, &schema) {
                    return Err(not_found(&current, &schema));
                }
                catalog = current.clone();
            } else if !schema.is_empty() && (!is_database(&catalog) || !schema_exists(self, &catalog, &schema)) {
                // "a.b" can also name a nested schema of the current database
                let nested = crate::catalog::schema_key(&[catalog.clone(), schema.clone()]);
                if !schema_exists(self, &current, &nested) {
                    return Err(not_found(&catalog, &schema));
                }
                (catalog, schema) = (current.clone(), nested);
            }
            let schema = (!schema.is_empty()).then_some(schema);
            return self.show_tables_from(table_ref, &catalog, schema.as_deref());
        }
        let Some(name) = &show.name else {
            return self.show_all_tables(table_ref);
        };
        match name.name.as_str() {
            // `PragmaShowDatabases`
            "\"databases\"" => {
                let select = "SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name";
                let select = parse_select(select, "Expected a single SELECT statement")?;
                self.bind_rewritten(&select, table_ref, "__show_select")
            }
            "\"schemas\"" => {
                let select = "SELECT schema.database_name, schema.schema_name, ((SELECT current_schema() = \
                              schema.schema_name) AND (SELECT current_database() = schema.database_name)) \"current\" \
                              FROM duckdb_schemas() schema JOIN duckdb_databases() dbs USING (database_oid) \
                              WHERE dbs.internal = false ORDER BY ALL";
                let select = parse_select(select, "Expected a single SELECT statement")?;
                self.bind_rewritten(&select, table_ref, "__show_select")
            }
            "\"tables\"" => self.show_tables(table_ref, None),
            "__show_tables_expanded" => self.show_all_tables(table_ref),
            "\"variables\"" => Err(crate::unported("SHOW VARIABLES")),
            _ => {
                let name = name.clone();
                let rows = self.describe_entry(&name)?;
                self.bind_describe_rows(table_ref, rows)
            }
        }
    }

    /// `Binder::BindSummarize`.
    fn bind_summarize(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        let query: QueryNode = match (&show.query, &show.name) {
            (Some(query), _) => (**query).clone(),
            (None, Some(name)) => SelectNode {
                select_list: vec![Expr::star()],
                from_table: TableRefKind::BaseTable {
                    name: name.clone(),
                    at_clause: None,
                }
                .into(),
                ..SelectNode::default()
            }
            .into(),
            _ => return Err(binder_error("SUMMARIZE requires a table or a query")),
        };
        let select = SelectStatement { node: query };
        let mut binder = self.child_binder();
        let bound = binder.bind_select_statement(&select);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let mut columns: [Vec<String>; 12] = Default::default();
        for (name, ty) in bound.names.iter().zip(&bound.types) {
            let column = sql_identifier(name);
            let type_name = ty.to_string();
            let temporal = type_name.starts_with("DATE") || type_name.starts_with("TIME");
            let numeric = ty.is_numeric();
            let aggregate = |function: &str, modifier: &str| format!("CAST({function}({column}{modifier}) AS VARCHAR)");
            let unless = |condition: bool, value: String| if condition { value } else { "NULL".to_string() };
            columns[0].push(sql_string(name));
            columns[1].push(sql_string(&type_name));
            columns[2].push(aggregate("min", ""));
            columns[3].push(aggregate("max", ""));
            columns[4].push(format!("CAST({} AS BIGINT)", aggregate("approx_count_distinct", "")));
            columns[5].push(unless(numeric || temporal, aggregate("avg", "")));
            columns[6].push(unless(numeric, aggregate("stddev", "")));
            for (slot, quantile) in [(7, "0.25"), (8, "0.50"), (9, "0.75")] {
                let modifier = format!(", CAST({quantile} AS FLOAT)");
                columns[slot].push(unless(numeric || temporal, aggregate("approx_quantile", &modifier)));
            }
            columns[10].push("count_star()".to_string());
            columns[11].push(format!(
                "CAST(CASE WHEN count_star() > 0 THEN (CAST(1 AS DOUBLE) - CAST({} AS DOUBLE) / \
                 CAST(count_star() AS DOUBLE)) * CAST(100 AS DOUBLE) ELSE NULL END AS DECIMAL(9, 2))",
                aggregate("count", "")
            ));
        }
        const ALIASES: [&str; 12] = [
            "column_name",
            "column_type",
            "min",
            "max",
            "approx_unique",
            "avg",
            "std",
            "q25",
            "q50",
            "q75",
            "count",
            "null_percentage",
        ];
        let list: Vec<String> = ALIASES
            .iter()
            .zip(&columns)
            .map(|(alias, values)| format!("unnest(list_value({})) AS {}", values.join(", "), sql_identifier(alias)))
            .collect();
        let mut parsed = parse_select(&format!("SELECT {}", list.join(", ")), "Expected a SELECT")?;
        let mut source = TableRef::subquery(select);
        source.alias = "summarize_tbl".to_string();
        if let duckrs_parser::ast::QueryNodeKind::Select(node) = &mut parsed.node.kind {
            node.from_table = source;
        }
        self.bind_rewritten(&parsed, table_ref, "unnamed_subquery")
    }

    /// `Binder::Bind(ShowRef)`.
    pub(super) fn bind_show_ref(&mut self, table_ref: &TableRef, show: &ShowRef) -> Result<LogicalOperator> {
        match show.show_type {
            ShowType::Summary => self.bind_summarize(table_ref, show),
            ShowType::Show if show.query.is_none() => {
                let name = show.name.as_ref();
                let can_be_setting = name.is_some_and(|n| n.qualification.is_empty());
                if self.options.show_behavior == ShowBehavior::Setting {
                    let setting = name.map(|n| n.to_string()).unwrap_or_default();
                    if can_be_setting
                        && let Some(plan) = self.bind_show_setting(table_ref, &name.expect("checked").name)?
                    {
                        return Ok(plan);
                    }
                    return Err(duckrs_common::Error::new(
                        ErrorKind::Catalog,
                        format!("Setting with name \"{setting}\" does not exist"),
                    ));
                }
                if self.options.show_behavior == ShowBehavior::Table {
                    return self.bind_describe(table_ref, show);
                }
                match self.bind_describe(table_ref, show) {
                    Err(error) if error.kind == ErrorKind::Catalog && can_be_setting => {
                        let setting = name.expect("checked above").name.clone();
                        match self.bind_show_setting(table_ref, &setting)? {
                            Some(plan) => Ok(plan),
                            None => Err(error),
                        }
                    }
                    result => result,
                }
            }
            _ => self.bind_describe(table_ref, show),
        }
    }

    /// `pragma_table_info` and `pragma_show`.
    pub(super) fn bind_pragma_table_info(
        &mut self,
        table_ref: &TableRef,
        function: &str,
        name: &QualifiedName,
    ) -> Result<LogicalOperator> {
        // `PragmaTableInfoBind`: of a view, which is bound again when it runs, or a table that is not the catalog's
        // own, the rows can differ between runs
        let native_table = !(name.qualification.is_empty()
            && self.ctes.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&name.name)))
            && matches!(
                self.catalog.get_entry(
                    super::schema_of(name)?.as_deref(),
                    self.search_schema.as_deref(),
                    &name.name
                ),
                Ok(TableOrView::Table(_))
            );
        let described = self.describe_entry(name)?;
        let mark = |mut op: LogicalOperator| {
            if let LogicalOperatorKind::ExpressionGet { volatile, .. } = &mut op.kind {
                *volatile = !native_table;
            }
            op
        };
        if function.eq_ignore_ascii_case("pragma_show") {
            return self.bind_describe_rows(table_ref, described).map(mark);
        }
        let mut rows = Vec::with_capacity(described.len());
        for (cid, row) in described.into_iter().enumerate() {
            let [column, ty, null, key, default, _] = <[Value; 6]>::try_from(row).expect("six columns");
            let not_null = null.try_to_string()? == "NO";
            let primary = !key.is_null() && key.try_to_string()? == "PRI";
            rows.push(vec![
                Value::integer(cid as i32),
                column,
                ty,
                Value::boolean(not_null),
                default,
                Value::boolean(primary),
            ]);
        }
        let names = ["cid", "name", "type", "notnull", "dflt_value", "pk"];
        let types = vec![
            LogicalType::INTEGER,
            LogicalType::VARCHAR,
            LogicalType::VARCHAR,
            LogicalType::BOOLEAN,
            LogicalType::VARCHAR,
            LogicalType::BOOLEAN,
        ];
        self.bind_rows(
            table_ref,
            function,
            names.iter().map(|n| n.to_string()).collect(),
            types,
            rows,
        )
        .map(mark)
    }

    /// Everything in the catalog, with stable ids.
    /// `DuckDBConstraintsFunction`: per table the NOT NULLs and then the others, as the entry has them.
    fn constraint_rows(&self) -> Result<SystemRows> {
        let list = LogicalType::list(LogicalType::VARCHAR);
        let indexes = LogicalType::list(LogicalType::BIGINT);
        let mut rows = Vec::new();
        for listed in self.listed_entries() {
            let TableOrView::Table(table) = &listed.entry else {
                continue;
            };
            let names_of =
                |columns: &[usize]| -> Vec<String> { columns.iter().map(|c| table.columns[*c].name.clone()).collect() };
            // type, text, expression, columns, referenced table and columns, the end of the name, the
            // check mode
            type Row = (
                &'static str,
                String,
                Option<String>,
                Vec<usize>,
                Option<String>,
                Vec<String>,
                &'static str,
                &'static str,
            );
            // in the order the table has them (`NotNullConstraint::ToString` is "NOT NULL")
            let mut constraints: Vec<Row> = Vec::new();
            for constraint in &table.constraints {
                let quoted = |columns: &[usize]| {
                    columns
                        .iter()
                        .map(|c| sql_identifier(&table.columns[*c].name))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                constraints.push(match constraint {
                    TableConstraint::NotNull(column) => (
                        "NOT NULL",
                        "NOT NULL".to_string(),
                        None,
                        vec![*column],
                        None,
                        Vec::new(),
                        "not_null",
                        "DEFAULT",
                    ),
                    TableConstraint::Check { parsed, .. } => {
                        let mut columns = Vec::new();
                        visit_expr_mut(&mut parsed.clone(), &mut |e: &mut Expr| {
                            // a lambda's parameters are no columns (an arrow is the JSON operator here,
                            // as a CHECK refuses the arrow lambdas that read columns)
                            if let ExprKind::Lambda {
                                lhs,
                                expr: body,
                                syntax: duckrs_parser::ast::LambdaSyntax::Keyword,
                            } = &mut e.kind
                            {
                                let mut parameters = Vec::new();
                                visit_expr_mut(lhs, &mut |p: &mut Expr| {
                                    if let ExprKind::ColumnRef(names) = &p.kind {
                                        parameters.push(names[0].clone());
                                    }
                                });
                                visit_expr_mut(body, &mut |inner: &mut Expr| {
                                    if let ExprKind::ColumnRef(names) = &inner.kind
                                        && parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0]))
                                    {
                                        inner.kind = ExprKind::Default;
                                    }
                                });
                                **lhs = Expr::from(ExprKind::Default);
                            }
                            if let ExprKind::ColumnRef(parts) = &e.kind {
                                let at = usize::from(parts.len() > 1 && parts[0].eq_ignore_ascii_case(&table.name));
                                columns.extend(parts.get(at).and_then(|name| table.column_index(name)));
                            }
                        });
                        let text = format!("CHECK({parsed})");
                        (
                            "CHECK",
                            text,
                            Some(parsed.to_string()),
                            columns,
                            None,
                            Vec::new(),
                            "check",
                            "DEFAULT",
                        )
                    }
                    TableConstraint::Unique {
                        columns,
                        primary_key,
                        check_mode,
                        ..
                    } => {
                        let (kind, end) = if *primary_key {
                            ("PRIMARY KEY", "pkey")
                        } else {
                            ("UNIQUE", "key")
                        };
                        // `UniqueConstraint::ToString` and `EnumUtil::ToString(check_mode)`
                        let mode = match check_mode {
                            ConstraintCheckMode::Default => "DEFAULT",
                            ConstraintCheckMode::Immediate => "IMMEDIATE",
                            ConstraintCheckMode::Deferred => "DEFERRED",
                        };
                        let mut text = format!("{kind}({})", quoted(columns));
                        if *check_mode != ConstraintCheckMode::Default {
                            text = format!("{text} {mode}");
                        }
                        (kind, text, None, columns.clone(), None, Vec::new(), end, mode)
                    }
                    TableConstraint::ForeignKey(key) => {
                        let (parent, referenced) = match &key.referenced {
                            Some(parent) => {
                                let parent = parent.table();
                                let columns = key.referenced_columns.iter();
                                let names = columns.map(|c| parent.columns[*c].name.clone()).collect();
                                (parent.name.clone(), names)
                            }
                            None => (table.name.clone(), names_of(&key.referenced_columns)),
                        };
                        let text = format!(
                            "FOREIGN KEY ({}) REFERENCES {}({})",
                            quoted(&key.columns),
                            sql_identifier(&parent),
                            referenced
                                .iter()
                                .map(|name| sql_identifier(name))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        (
                            "FOREIGN KEY",
                            text,
                            None,
                            key.columns.clone(),
                            Some(parent),
                            referenced,
                            "fkey",
                            "DEFAULT",
                        )
                    }
                });
            }
            for (kind, text, expression, columns, parent, referenced, end, mode) in constraints {
                let names = names_of(&columns);
                let mut name = format!("{}_", table.name);
                for column in names.iter().chain(&referenced) {
                    name += &format!("{}_", column.to_lowercase());
                }
                // `GetConstraintName`: names can repeat
                name += end;
                let index = rows.len() as i64;
                let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                let varchars = |names: Vec<String>| {
                    Value::list(LogicalType::VARCHAR, names.into_iter().map(Value::varchar).collect())
                };
                rows.push(vec![
                    Value::varchar(listed.database.as_str()),
                    Value::bigint(listed.database_oid),
                    Value::varchar(if temporary { "main" } else { listed.schema.name.as_str() }),
                    Value::bigint(listed.schema_oid),
                    Value::varchar(table.name.as_str()),
                    Value::bigint(listed.oid),
                    Value::bigint(index),
                    Value::varchar(kind),
                    Value::varchar(text),
                    expression.map_or_else(|| Value::null(LogicalType::VARCHAR), Value::varchar),
                    Value::list(
                        LogicalType::BIGINT,
                        columns.iter().map(|c| Value::bigint(*c as i64)).collect(),
                    )?,
                    varchars(names)?,
                    Value::varchar(name),
                    parent.map_or_else(|| Value::null(LogicalType::VARCHAR), Value::varchar),
                    varchars(referenced)?,
                    Value::varchar(mode),
                ]);
            }
        }
        let (bigint, varchar) = (LogicalType::BIGINT, LogicalType::VARCHAR);
        Ok((
            vec![
                ("database_name", varchar.clone()),
                ("database_oid", bigint.clone()),
                ("schema_name", varchar.clone()),
                ("schema_oid", bigint.clone()),
                ("table_name", varchar.clone()),
                ("table_oid", bigint.clone()),
                ("constraint_index", bigint),
                ("constraint_type", varchar.clone()),
                ("constraint_text", varchar.clone()),
                ("expression", varchar.clone()),
                ("constraint_column_indexes", indexes),
                ("constraint_column_names", list.clone()),
                ("constraint_name", varchar.clone()),
                ("referenced_table", varchar.clone()),
                ("referenced_column_names", list),
                ("constraint_check_mode", varchar),
            ],
            rows,
        ))
    }

    fn listed_entries(&self) -> Vec<ListedEntry> {
        let mut result = Vec::new();
        let mut oid = 1000;
        for listed in self.listed_schemas() {
            for entry in listed.schema.tables_and_views() {
                oid += 1;
                result.push(ListedEntry {
                    database: listed.database.clone(),
                    database_oid: listed.database_oid,
                    schema: listed.schema.clone(),
                    schema_oid: listed.oid,
                    oid,
                    entry,
                });
            }
        }
        result
    }

    /// `CatalogSearchPath::SchemaInSearchPath`: `temp.main`, the schemas that were set, and `main` of the database
    /// statements run in.
    fn in_search_path(&self, database: &str, schema: &str) -> bool {
        if database.eq_ignore_ascii_case("temp") || schema.eq_ignore_ascii_case(TEMP_SCHEMA) {
            return true;
        }
        let current = self
            .session
            .as_ref()
            .map_or_else(|| self.catalog.name.clone(), |session| session.database.clone());
        let set = self.session.as_ref().map(|s| s.search_path.clone()).unwrap_or_default();
        let in_current = database.eq_ignore_ascii_case(&current);
        if in_current && schema.eq_ignore_ascii_case("main") {
            return true;
        }
        set.iter().any(|path| match path.split_once('.') {
            Some((db, s)) => db.eq_ignore_ascii_case(database) && s.eq_ignore_ascii_case(schema),
            // a schema of the database statements run in, or a database whose default schema it is
            None => {
                (in_current && path.eq_ignore_ascii_case(schema))
                    || (path.eq_ignore_ascii_case(database) && schema.eq_ignore_ascii_case("main"))
            }
        })
    }

    /// `Catalog::GetAllSchemas`: the schemas of all databases, by database and then name, with
    /// the oids of both. The temporary schema is `main` of the database `temp`.
    fn listed_schemas(&self) -> Vec<ListedSchema> {
        let mut result = Vec::new();
        let databases = self.catalog.databases();
        let mut add = |database: &str, database_oid: i64, catalog: &crate::catalog::Catalog, current: bool| {
            for schema in catalog.schemas() {
                let temporary = schema.key() == TEMP_SCHEMA;
                if temporary && !current {
                    continue;
                }
                result.push(ListedSchema {
                    database: if temporary { "temp".into() } else { database.to_string() },
                    database_oid: if temporary { 3 } else { database_oid },
                    display: if temporary { "main".into() } else { schema.name.clone() },
                    schema,
                    oid: 0,
                });
            }
        };
        if databases.is_empty() {
            add(&self.catalog.name, 1, self.catalog, true);
        }
        // the catalog this binds against is that of the database statements run in, whose name only
        // the session knows
        let current = self
            .session
            .as_ref()
            .map_or_else(|| self.catalog.name.clone(), |session| session.database.clone());
        for (index, database) in databases.iter().enumerate() {
            let oid = if index == 0 { 1 } else { index as i64 + 3 };
            if database.name.eq_ignore_ascii_case(&current) {
                add(&database.name, oid, self.catalog, true);
            } else {
                self.catalog
                    .with_database(&database.name, &mut |catalog| add(&database.name, oid, catalog, false));
            }
        }
        result.sort_by(|a, b| (&a.database, &a.display).cmp(&(&b.database, &b.display)));
        for (index, listed) in result.iter_mut().enumerate() {
            listed.oid = index as i64 + 1;
        }
        result
    }

    /// `duckdb_logs()` (`DuckDBLogBindReplace`), and `duckdb_log_contexts()`: what the log
    /// storage holds, the entries alone or, with `denormalized_table`, each with its context.
    pub(super) fn bind_duckdb_logs(
        &mut self,
        name: &str,
        call: &duckrs_parser::ast::FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let mut denormalized = false;
        for argument in call.args.iter().map(super::table_functions::named_argument) {
            if name == "duckdb_logs" && argument.name.eq_ignore_ascii_case("denormalized_table") {
                let value = self.constant_argument(&argument.expr)?;
                if value.is_null() {
                    return Err(duckrs_common::Error::new(
                        ErrorKind::InvalidInput,
                        "denormalized_table cannot be NULL",
                    ));
                }
                let value = crate::function::cast::cast_value(&value, &LogicalType::BOOLEAN)?;
                denormalized = value.try_to_string()? == "true";
            } else if argument.name.is_empty() {
                return Err(binder_error(format!("{name} takes no positional arguments")));
            } else {
                return Err(binder_error(format!(
                    "Invalid named parameter \"{}\" for function {name}",
                    argument.name
                )));
            }
        }
        self.bind_log_table(name, denormalized, table_ref)
    }

    /// The rows of `duckdb_logs()`, `duckdb_logs(denormalized_table = true)` (the `duckdb_logs`
    /// view) or `duckdb_log_contexts()`.
    pub(super) fn bind_log_table(
        &mut self,
        name: &str,
        denormalized: bool,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let Some(session) = self.session.clone() else {
            return Err(crate::unported(format!("{name} here")));
        };
        // `FileLogStorage::BindReplace`: the files are read as CSV; the lock is let go first, as
        // the query reads the log again
        let file_sql = {
            use crate::logging::LogTable;
            let mut manager = session.log_manager.lock().expect("the log manager is not poisoned");
            if manager.is_file_storage() {
                let table = match (name, denormalized) {
                    ("duckdb_log_contexts", _) => LogTable::Contexts,
                    (_, true) => LogTable::All,
                    _ => LogTable::Entries,
                };
                match manager.file_scan_sql(table)? {
                    Some(sql) => Some(sql),
                    None if table == LogTable::All => Some(crate::logging::DENORMALIZED_JOIN.to_string()),
                    None => {
                        return Err(duckrs_common::Error::new(
                            ErrorKind::InvalidConfiguration,
                            format!(
                                "Log storage '{}' does not support this query. Select a queryable storage, such as \
                                 'memory' or 'file', before generating logs.",
                                manager.config().storage
                            ),
                        ));
                    }
                }
            } else {
                None
            }
        };
        if let Some(sql) = file_sql {
            let select = parse_select(&sql, "Expected a single SELECT statement")?;
            return self.bind_rewritten(&select, table_ref, name);
        }
        let (ubigint, varchar) = (LogicalType::UBIGINT, LogicalType::VARCHAR);
        let context_columns = vec![
            ("context_id", ubigint.clone()),
            ("scope", varchar.clone()),
            ("connection_id", ubigint.clone()),
            ("transaction_id", ubigint.clone()),
            ("query_id", ubigint.clone()),
            ("thread_id", ubigint.clone()),
        ];
        let entry_columns = vec![
            ("context_id", ubigint),
            (
                "timestamp",
                LogicalType::new(duckrs_common::types::LogicalTypeId::TimestampTz),
            ),
            ("type", varchar.clone()),
            ("log_level", varchar.clone()),
            ("message", varchar),
        ];
        let manager = session.log_manager.lock().expect("the log manager is not poisoned");
        let (columns, rows) = if name == "duckdb_log_contexts" {
            let (_, contexts) = manager.scan()?;
            (
                context_columns,
                contexts.iter().map(crate::logging::context_values).collect(),
            )
        } else if denormalized {
            let mut columns = context_columns;
            columns.extend(entry_columns.into_iter().skip(1));
            (columns, manager.denormalized_rows()?)
        } else {
            let (entries, _) = manager.scan()?;
            (entry_columns, entries.iter().map(|e| e.values()).collect())
        };
        drop(manager);
        let (names, types) = columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, name, names, types, rows)
    }

    /// `duckdb_settings()` (`DuckDBSettingsBind`, `DuckDBSettingsInit`): the settings, without
    /// the debug and deprecated ones unless asked for.
    pub(super) fn bind_duckdb_settings(
        &mut self,
        call: &duckrs_parser::ast::FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let (mut debug, mut deprecated, mut include_all) = (None, None, None);
        for argument in call.args.iter().map(super::table_functions::named_argument) {
            let name = argument.name.to_lowercase();
            let target = match name.as_str() {
                "debug" => &mut debug,
                "deprecated" => &mut deprecated,
                "include_all" => &mut include_all,
                "" => return Err(binder_error("duckdb_settings takes no positional arguments")),
                _ => {
                    return Err(binder_error(format!(
                        "Invalid named parameter \"{}\" for function duckdb_settings",
                        argument.name
                    )));
                }
            };
            let value = self.constant_argument(&argument.expr)?;
            if value.is_null() {
                return Err(duckrs_common::Error::new(
                    ErrorKind::InvalidInput,
                    format!("Cannot use NULL as argument for {name}"),
                ));
            }
            let value = crate::function::cast::cast_value(&value, &LogicalType::BOOLEAN)?;
            *target = Some(value.try_to_string()? == "true");
        }
        if include_all == Some(true) && (debug.is_some() || deprecated.is_some()) {
            return Err(duckrs_common::Error::new(
                ErrorKind::InvalidInput,
                "Cannot combine include_all := true with the debug or deprecated filters",
            ));
        }
        let (debug, deprecated, include_all) = (
            debug.unwrap_or(false),
            deprecated.unwrap_or(false),
            include_all.unwrap_or(false),
        );
        let Some(session) = self.session.clone() else {
            return Err(crate::unported("duckdb_settings here"));
        };
        let variant = LogicalType::new(duckrs_common::types::LogicalTypeId::Variant);
        let mut rows = Vec::new();
        for setting in (session.settings)() {
            if !include_all && (setting.is_debug != debug || setting.is_deprecated != deprecated) {
                continue;
            }
            let (value, typed_value) = match &setting.value {
                Some(value) if !value.is_null() => (
                    crate::function::cast::cast_value(value, &LogicalType::VARCHAR)?,
                    crate::function::cast::cast_value(value, &variant)?,
                ),
                _ => (null_text(), Value::null(variant.clone())),
            };
            rows.push(vec![
                text(setting.name),
                value,
                text(setting.description),
                text(setting.input_type),
                text(setting.scope),
                Value::list(
                    LogicalType::VARCHAR,
                    setting.aliases.iter().map(|alias| text(alias)).collect(),
                )?,
                typed_value,
            ]);
        }
        let varchar = LogicalType::VARCHAR;
        let columns = [
            ("name", varchar.clone()),
            ("value", varchar.clone()),
            ("description", varchar.clone()),
            ("input_type", varchar.clone()),
            ("scope", varchar.clone()),
            ("aliases", LogicalType::list(varchar)),
            ("typed_value", variant),
        ];
        let (names, types) = columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, "duckdb_settings", names, types, rows)
    }

    /// `duckdb_secrets()`, `duckdb_secret_types()`, `duckdb_secret_type_parameters()` and
    /// `which_secret(path, type)`: the rows the client's secret manager gives.
    pub(super) fn bind_secret_function(
        &mut self,
        name: &str,
        call: &duckrs_parser::ast::FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let arguments: Vec<_> = call.args.iter().map(super::table_functions::named_argument).collect();
        let positional = arguments.iter().filter(|argument| argument.name.is_empty()).count();
        let expected = if name == "which_secret" { 2 } else { 0 };
        if positional != expected {
            return Err(binder_error(format!(
                "Function \"{name}\" expects {expected} positional argument{}, got {positional}",
                if expected == 1 { "" } else { "s" }
            )));
        }
        // `DuckDBSecretsBind`: `redact` is its only named parameter
        let mut redact = true;
        let mut values = Vec::new();
        for argument in &arguments {
            let parameter = argument.name.to_lowercase();
            let known = parameter.is_empty() || (name == "duckdb_secrets" && parameter == "redact");
            if !known {
                return Err(binder_error(format!(
                    "Invalid named parameter \"{}\" for function {name}",
                    argument.name
                )));
            }
            let value = self.constant_argument(&argument.expr)?;
            if parameter == "redact" {
                if value.is_null() {
                    return Err(duckrs_common::Error::new(
                        ErrorKind::InvalidInput,
                        "Cannot use NULL as argument for redact",
                    ));
                }
                let value = crate::function::cast::cast_value(&value, &LogicalType::BOOLEAN)?;
                redact = value.try_to_string()? == "true";
            } else {
                values.push(crate::function::cast::cast_value(&value, &LogicalType::VARCHAR)?);
            }
        }
        let Some(secrets) = self.session.as_ref().and_then(|session| session.secrets.clone()) else {
            return Err(crate::unported(format!("{name} here")));
        };
        let varchar = LogicalType::VARCHAR;
        let (columns, rows) = match name {
            "duckdb_secrets" => (
                vec![
                    ("name", varchar.clone()),
                    ("type", varchar.clone()),
                    ("provider", varchar.clone()),
                    ("persistent", LogicalType::BOOLEAN),
                    ("storage", varchar.clone()),
                    ("scope", LogicalType::list(varchar.clone())),
                    ("secret_string", varchar.clone()),
                ],
                secrets.secrets(redact)?,
            ),
            "duckdb_secret_types" => (
                vec![
                    ("type", varchar.clone()),
                    ("default_provider", varchar.clone()),
                    ("extension", varchar.clone()),
                ],
                secrets.secret_types()?,
            ),
            "duckdb_secret_type_parameters" => (
                vec![
                    ("type", varchar.clone()),
                    ("provider", varchar.clone()),
                    ("name", varchar.clone()),
                    ("parameter_type", varchar.clone()),
                    ("description", varchar.clone()),
                ],
                secrets.secret_type_parameters()?,
            ),
            _ => {
                // `DuckDBWhichSecretFunction`: `Value::ToString` of the inputs
                let text_of = |value: &Value| -> Result<String> {
                    if value.is_null() {
                        Ok("NULL".to_string())
                    } else {
                        value.try_to_string()
                    }
                };
                let (path, secret_type) = (text_of(&values[0])?, text_of(&values[1])?);
                (
                    vec![
                        ("name", varchar.clone()),
                        ("persistent", varchar.clone()),
                        ("storage", varchar.clone()),
                    ],
                    secrets.which_secret(&path, &secret_type)?.into_iter().collect(),
                )
            }
        };
        let (names, types) = columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, name, names, types, rows)
    }

    /// `pragma_storage_info`: the segments of a table in its file. Rows in memory have none.
    pub(super) fn bind_storage_info(
        &mut self,
        call: &duckrs_parser::ast::FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let (mut include_info, mut loaded_only) = (false, false);
        let arguments: Vec<_> = call.args.iter().map(super::table_functions::named_argument).collect();
        for argument in arguments.iter().filter(|argument| !argument.name.is_empty()) {
            let name = argument.name.to_lowercase();
            let target = match name.as_str() {
                "include_segment_info" => &mut include_info,
                "loaded_segments_only" => &mut loaded_only,
                _ => {
                    return Err(binder_error(format!(
                        "Invalid named parameter \"{}\" for function pragma_storage_info",
                        argument.name
                    )));
                }
            };
            let value = evaluate_scalar(&self.bind_expression(&argument.expr)?)?;
            if value.is_null() {
                return Err(duckrs_common::Error::new(
                    duckrs_common::ErrorKind::InvalidInput,
                    format!("Cannot use NULL as argument for {name}"),
                ));
            }
            *target = value.try_to_string()?.eq_ignore_ascii_case("true");
        }
        let mut positional = arguments.iter().filter(|argument| argument.name.is_empty());
        let (Some(argument), None) = (positional.next(), positional.next()) else {
            return Err(binder_error("pragma_storage_info takes the name of a table"));
        };
        let name = evaluate_scalar(&self.bind_expression(&argument.expr)?)?.try_to_string()?;
        let parts: Vec<&str> = name.split('.').collect();
        // a database comes first if there are three parts, or if no schema here has the name
        let mut elsewhere = None;
        if parts.len() == 3 || (parts.len() == 2 && self.catalog.get_schema(parts[0]).is_none()) {
            let schema = (parts.len() == 3).then_some(parts[1]);
            let table = parts[parts.len() - 1];
            self.catalog.with_database(parts[0], &mut |catalog| {
                elsewhere = Some(catalog.get_table(schema, table));
            });
        }
        let table = match (elsewhere, parts.as_slice()) {
            (Some(table), _) => table?,
            (None, [table]) => self.catalog.get_table(None, table)?,
            (None, [schema, table]) => self.catalog.get_table(Some(schema), table)?,
            _ => return Err(crate::unported("a table name of more than two parts")),
        };
        let stored = table.stored();
        let persistent = stored.is_some();
        let segments = match stored {
            Some(source) => source.segments(include_info, loaded_only)?,
            None => memory_segments(&table),
        };
        let (bigint, varchar, boolean) = (LogicalType::BIGINT, LogicalType::VARCHAR, LogicalType::BOOLEAN);
        let mut columns = vec![
            ("row_group_id", bigint.clone()),
            ("column_name", varchar.clone()),
            ("column_id", bigint.clone()),
            ("column_path", varchar.clone()),
            ("segment_id", bigint.clone()),
            ("segment_type", varchar.clone()),
            ("start", bigint.clone()),
            ("count", bigint.clone()),
            ("compression", varchar.clone()),
            ("stats", varchar.clone()),
            ("has_updates", boolean.clone()),
            ("persistent", boolean),
            ("block_id", bigint.clone()),
            ("block_offset", bigint),
        ];
        // only there if it was asked for
        if include_info {
            columns.push(("segment_info", varchar.clone()));
        }
        columns.push(("additional_block_ids", LogicalType::list(LogicalType::BIGINT)));
        let rows = segments
            .into_iter()
            .map(|s| {
                let mut row = vec![
                    Value::bigint(s.row_group as i64),
                    Value::varchar(table.columns[s.column].name.clone()),
                    Value::bigint(s.column as i64),
                    Value::varchar(s.path),
                    Value::bigint(s.segment as i64),
                    Value::varchar(s.segment_type),
                    Value::bigint(s.start as i64),
                    Value::bigint(s.count as i64),
                    Value::varchar(s.compression.to_string()),
                    Value::null(LogicalType::VARCHAR),
                    Value::boolean(false),
                    Value::boolean(persistent),
                    Value::bigint(s.block_id),
                    Value::bigint(s.block_offset as i64),
                ];
                if include_info {
                    row.push(Value::varchar(s.info.unwrap_or_default()));
                }
                row.push(Value::null(LogicalType::list(LogicalType::BIGINT)));
                row
            })
            .collect();
        let (names, types) = columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, "pragma_storage_info", names, types, rows)
    }

    pub(super) fn is_system_table(name: &str) -> bool {
        SYSTEM_TABLES.iter().any(|t| t.eq_ignore_ascii_case(name))
    }

    /// `duckdb_databases()` and the like.
    pub(super) fn bind_system_table(&mut self, table_ref: &TableRef, name: &str) -> Result<LogicalOperator> {
        let varchar = LogicalType::VARCHAR;
        let bigint = LogicalType::BIGINT;
        let boolean = LogicalType::BOOLEAN;
        let integer = LogicalType::INTEGER;
        let tags = LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR);
        let (columns, rows): (Vec<(&str, LogicalType)>, Vec<Vec<Value>>) = match name.to_lowercase().as_str() {
            // `DuckDBOptimizersFunction`
            "duckdb_optimizers" => (
                vec![("name", varchar.clone())],
                OPTIMIZERS.iter().map(|name| vec![text(name)]).collect(),
            ),
            // `DuckDBFunctionsFunction`: a row per overload, by kind of function
            "duckdb_functions" => {
                use crate::catalog::MacroCatalogEntry;
                use crate::function::BuiltinFunctions;
                use crate::planner::expression::VOLATILE_FUNCTIONS;
                use duckrs_parser::ast::MacroBody;
                let list = LogicalType::list(LogicalType::VARCHAR);
                let strings = |items: Vec<Value>| Value::list(LogicalType::VARCHAR, items);
                let no_tags = Value::new(tags.clone(), duckrs_common::value::ValueData::List(vec![]))?;
                // (CatalogType, database, database oid, schema, name, kind, return type, parameters,
                // parameter types, macro definition, side effects, internal, stability)
                type Row = (
                    u8,
                    String,
                    i64,
                    String,
                    String,
                    &'static str,
                    Value,
                    Value,
                    Value,
                    Value,
                    Value,
                    bool,
                    Value,
                );
                let mut functions: Vec<Row> = Vec::new();
                let builtin = |name: &str,
                               kind: &'static str,
                               catalog_type: u8,
                               arguments: &[LogicalType],
                               ret: &LogicalType|
                 -> Result<Row> {
                    let volatile = VOLATILE_FUNCTIONS.contains(&name);
                    // `GetParameterNames`: without a description the parameters are col0, col1, ...
                    let names = (0..arguments.len()).map(|i| text(&format!("col{i}"))).collect();
                    let types = arguments.iter().map(|t| text(&t.to_string())).collect();
                    Ok((
                        catalog_type,
                        "system".into(),
                        2,
                        "main".into(),
                        name.to_string(),
                        kind,
                        text(&ret.to_string()),
                        strings(names)?,
                        strings(types)?,
                        null_text(),
                        Value::boolean(volatile),
                        true,
                        text(if volatile {
                            "VOLATILE"
                        } else if crate::planner::expression::CONSISTENT_WITHIN_QUERY_FUNCTIONS.contains(&name) {
                            "CONSISTENT_WITHIN_QUERY"
                        } else {
                            "CONSISTENT"
                        }),
                    ))
                };
                let functions_ref = &mut functions;
                for set in BuiltinFunctions::get().scalar_sets() {
                    for function in &set.functions {
                        functions_ref.push(builtin(
                            &set.name,
                            "scalar",
                            26,
                            &function.arguments,
                            &function.return_type,
                        )?);
                    }
                }
                for set in BuiltinFunctions::get().aggregate_sets() {
                    for function in &set.functions {
                        functions_ref.push(builtin(
                            &set.name,
                            "aggregate",
                            27,
                            &function.arguments,
                            &function.return_type,
                        )?);
                    }
                }
                // `WindowFunctionExtractor`: the window functions of `function_list.cpp`, with their signatures
                let match_recognize_result = format!(
                    "{}",
                    LogicalType::list(LogicalType::struct_type(
                        [
                            ("classifier", LogicalType::VARCHAR),
                            ("match_number", LogicalType::UBIGINT),
                            ("is_match_start", LogicalType::BOOLEAN),
                            ("is_match_end", LogicalType::BOOLEAN),
                            ("match_start", LogicalType::UBIGINT),
                            ("match_end", LogicalType::UBIGINT),
                            ("is_excluded", LogicalType::BOOLEAN),
                            ("is_empty", LogicalType::BOOLEAN),
                            ("row_index", LogicalType::UBIGINT),
                        ]
                        .into_iter()
                        .map(|(n, t)| (n.to_string(), t))
                        .collect()
                    ))
                );
                // (name, (parameter, type) pairs, return type)
                type WindowSignature<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
                let windows: [WindowSignature; 14] = [
                    ("cume_dist", &[], "DOUBLE"),
                    ("dense_rank", &[], "BIGINT"),
                    ("fill", &[("expr", "ANY")], "ANY"),
                    ("first_value", &[("expr", "ANY")], "ANY"),
                    ("lag", &[("col", "T"), ("offset", "BIGINT"), ("default", "ANY")], "T"),
                    ("last_value", &[("expr", "ANY")], "ANY"),
                    ("lead", &[("col", "T"), ("offset", "BIGINT"), ("default", "ANY")], "T"),
                    ("match_recognize", &[("columns", "ANY")], ""),
                    ("nth_value", &[("expr", "ANY"), ("n", "BIGINT")], "ANY"),
                    ("ntile", &[("num_buckets", "BIGINT")], "BIGINT"),
                    ("percent_rank", &[], "DOUBLE"),
                    ("rank", &[], "BIGINT"),
                    ("rank_dense", &[], "BIGINT"),
                    ("row_number", &[], "BIGINT"),
                ];
                for (name, parameters, ret) in windows {
                    let ret = if name == "match_recognize" {
                        match_recognize_result.as_str()
                    } else {
                        ret
                    };
                    functions_ref.push((
                        32,
                        "system".into(),
                        2,
                        "main".into(),
                        name.to_string(),
                        "window",
                        text(ret),
                        strings(parameters.iter().map(|(n, _)| text(n)).collect())?,
                        strings(parameters.iter().map(|(_, t)| text(t)).collect())?,
                        null_text(),
                        Value::boolean(false),
                        true,
                        text("CONSISTENT"),
                    ));
                }
                // `MacroExtractor` and `TableMacroExtractor`
                // what `COMMENT ON MACRO` said, by database, schema, name and kind
                let mut macro_comments: std::collections::HashMap<(String, String, String, &'static str), String> =
                    std::collections::HashMap::new();
                let mut macro_rows = |database: &str,
                                      database_oid: i64,
                                      schema: &str,
                                      entry: &MacroCatalogEntry,
                                      internal: bool|
                 -> Result<()> {
                    for function in &entry.macros {
                        let names = function.parameters.iter().map(|p| text(&p.name)).collect();
                        let types = function
                            .parameters
                            .iter()
                            .map(|p| match &p.parameter_type {
                                Some(ty) => self.bind_type(ty).map(|t| text(&t.to_string())),
                                None => Ok(null_text()),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let (kind, catalog_type, definition) = match &function.body {
                            MacroBody::Scalar(body) => ("macro", 30, body.to_string()),
                            MacroBody::Table(query) => ("table_macro", 31, query.to_string()),
                        };
                        let return_type = match (&function.return_type, kind) {
                            (Some(ty), "macro") => text(&self.bind_type(ty)?.to_string()),
                            _ => null_text(),
                        };
                        if let Some(comment) = &entry.comment {
                            macro_comments.insert(
                                (database.to_string(), schema.to_string(), entry.name.clone(), kind),
                                comment.clone(),
                            );
                        }
                        functions_ref.push((
                            catalog_type,
                            database.to_string(),
                            database_oid,
                            schema.to_string(),
                            entry.name.clone(),
                            kind,
                            return_type,
                            strings(names)?,
                            strings(types)?,
                            text(&definition),
                            Value::null(boolean.clone()),
                            internal,
                            null_text(),
                        ));
                    }
                    Ok(())
                };
                for (schema, name, table_macro) in crate::catalog::defaults::default_macro_names() {
                    if let Some(entry) = crate::catalog::defaults::default_macro(Some(schema), name, table_macro) {
                        macro_rows("system", 2, schema, &entry, true)?;
                    }
                }
                // the macros of every attached database (`GetAllSchemas`)
                for listed in self.listed_schemas() {
                    for table_macro in [false, true] {
                        for entry in listed.schema.macro_entries(table_macro) {
                            macro_rows(&listed.database, listed.database_oid, &listed.display, &entry, false)?;
                        }
                    }
                }
                // `DuckDBFunctionsInit`: by kind, then as `GetAllSchemas` orders the schemas
                functions.sort_by(|a, b| (a.0, &a.1, &a.3).cmp(&(b.0, &b.1, &b.3)));
                let mut rows = Vec::with_capacity(functions.len());
                for (
                    oid,
                    (
                        _,
                        database,
                        database_oid,
                        schema,
                        name,
                        kind,
                        ret,
                        parameters,
                        types,
                        definition,
                        volatile,
                        internal,
                        stability,
                    ),
                ) in functions.into_iter().enumerate()
                {
                    let comment = macro_comments.get(&(database.clone(), schema.clone(), name.clone(), kind));
                    rows.push(vec![
                        text(&database),
                        text(&database_oid.to_string()),
                        text(&schema),
                        text(&name),
                        null_text(),
                        text(kind),
                        null_text(),
                        comment.map_or_else(null_text, |c| text(c)),
                        no_tags.clone(),
                        ret,
                        parameters,
                        types,
                        null_text(),
                        definition,
                        volatile,
                        Value::boolean(internal),
                        null_text(),
                        Value::bigint(oid as i64 + 1),
                        strings(vec![])?,
                        stability,
                        strings(vec![])?,
                    ]);
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        // `DuckDBFunctionsBind` declares it VARCHAR, and appends a BIGINT
                        ("database_oid", varchar.clone()),
                        ("schema_name", varchar.clone()),
                        ("function_name", varchar.clone()),
                        ("alias_of", varchar.clone()),
                        ("function_type", varchar.clone()),
                        ("description", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("return_type", varchar.clone()),
                        ("parameters", list.clone()),
                        ("parameter_types", list.clone()),
                        ("varargs", varchar.clone()),
                        ("macro_definition", varchar.clone()),
                        ("has_side_effects", boolean.clone()),
                        ("internal", boolean.clone()),
                        ("extension_name", varchar.clone()),
                        ("function_oid", bigint.clone()),
                        ("examples", list.clone()),
                        ("stability", varchar.clone()),
                        ("categories", list.clone()),
                    ],
                    rows,
                )
            }
            // `DuckDBTypesFunction`: a row per constructor of each type
            "duckdb_types" => {
                use crate::catalog::default_types::{BUILTIN_TYPES, Constructor};
                use duckrs_common::types::LogicalTypeId as L;
                // (database, database oid, schema, schema oid, type oid, name, type, internal, constructors)
                type Entry<'a> = (
                    String,
                    i64,
                    String,
                    i64,
                    i64,
                    String,
                    LogicalType,
                    bool,
                    &'a [Constructor],
                    Option<String>,
                );
                const USER: &[Constructor] = &[Constructor {
                    parameters: &[],
                    varargs: None,
                }];
                let mut builtin: Vec<_> = BUILTIN_TYPES.iter().collect();
                builtin.sort_by_key(|(name, ..)| *name);
                // `DefaultTypeGenerator`: the main schema of every database has the built-in types
                let builtins = |database: &str, database_oid: i64, schema_oid: i64| -> Vec<Entry> {
                    builtin
                        .iter()
                        .map(|(name, id, constructors)| {
                            let (database, schema) = (database.to_string(), "main".to_string());
                            let ty = LogicalType::new(*id);
                            (
                                database,
                                database_oid,
                                schema,
                                schema_oid,
                                *id as i64,
                                name.to_string(),
                                ty,
                                true,
                                *constructors,
                                None,
                            )
                        })
                        .collect()
                };
                let mut entries: Vec<Entry> = builtins("system", 2, 0);
                let mut oid = 3000;
                // the schemas of every database (`GetAllSchemas`)
                for (schema_oid, listed) in self.listed_schemas().iter().enumerate() {
                    let schema = &listed.schema;
                    let database = listed.database.as_str();
                    let database_oid = listed.database_oid;
                    let schema_name = listed.display.as_str();
                    let schema_oid = schema_oid as i64 + 1;
                    if schema_name.eq_ignore_ascii_case("main") {
                        entries.extend(builtins(database, database_oid, schema_oid));
                    }
                    for (name, ty) in schema.type_entries() {
                        oid += 1;
                        let comment = schema.type_comment(&name);
                        let (database, schema) = (database.to_string(), schema_name.to_string());
                        entries.push((
                            database,
                            database_oid,
                            schema,
                            schema_oid,
                            oid,
                            name,
                            ty,
                            false,
                            USER,
                            comment,
                        ));
                    }
                }
                // `GetAllSchemas` orders by database, then schema; a schema's entries are by name
                entries.sort_by(|a, b| (&a.0, &a.2, &a.5).cmp(&(&b.0, &b.2, &b.5)));
                let mut oids = std::collections::HashSet::new();
                let mut rows = Vec::new();
                let list = LogicalType::list(LogicalType::VARCHAR);
                for (database, database_oid, schema, schema_oid, oid, name, ty, internal, constructors, comment) in
                    &entries
                {
                    for constructor in constructors.iter() {
                        let category = match ty.id() {
                            L::TinyInt
                            | L::SmallInt
                            | L::Integer
                            | L::BigInt
                            | L::Decimal
                            | L::Float
                            | L::Double
                            | L::UTinyInt
                            | L::USmallInt
                            | L::UInteger
                            | L::UBigInt
                            | L::HugeInt
                            | L::UHugeInt => Some("NUMERIC"),
                            L::Date
                            | L::Time
                            | L::TimeNs
                            | L::TimestampSec
                            | L::TimestampMs
                            | L::Timestamp
                            | L::TimestampNs
                            | L::Interval
                            | L::TimeTz
                            | L::TimestampTz
                            | L::TimestampTzNs => Some("DATETIME"),
                            L::Char | L::Varchar => Some("STRING"),
                            L::Boolean => Some("BOOLEAN"),
                            L::Struct | L::List | L::Array | L::Map | L::Union => Some("COMPOSITE"),
                            L::Variant => Some("VARIANT"),
                            _ => None,
                        };
                        let physical = ty.physical_type();
                        let labels = if ty.id() == L::Enum && ty.has_params() {
                            Value::list(LogicalType::VARCHAR, ty.enum_values().iter().map(|v| text(v)).collect())?
                        } else {
                            Value::null(list.clone())
                        };
                        let strings =
                            |items: Vec<&str>| Value::list(LogicalType::VARCHAR, items.into_iter().map(text).collect());
                        let parameters = strings(constructor.parameters.iter().map(|(n, _)| *n).collect())?;
                        let parameter_types = strings(constructor.parameters.iter().map(|(_, t)| *t).collect())?;
                        rows.push(vec![
                            text(database),
                            Value::bigint(*database_oid),
                            text(schema),
                            Value::bigint(*schema_oid),
                            // a type id is listed once, by the first name that has it
                            if oids.insert(*oid) {
                                Value::bigint(*oid)
                            } else {
                                Value::null(bigint.clone())
                            },
                            text(name),
                            if physical == duckrs_common::types::PhysicalType::Invalid {
                                Value::null(bigint.clone())
                            } else {
                                Value::bigint(physical.size() as i64)
                            },
                            text(ty.id().name()),
                            category.map_or_else(null_text, text),
                            comment.as_deref().map_or_else(null_text, text),
                            Value::new(tags.clone(), duckrs_common::value::ValueData::List(vec![]))?,
                            Value::boolean(*internal),
                            null_text(),
                            labels,
                            parameters,
                            parameter_types,
                            constructor.varargs.map_or_else(null_text, text),
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("type_oid", bigint.clone()),
                        ("type_name", varchar.clone()),
                        ("type_size", bigint.clone()),
                        ("logical_type", varchar.clone()),
                        ("type_category", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("extension_name", varchar.clone()),
                        ("labels", list.clone()),
                        ("parameters", list.clone()),
                        ("parameter_types", list.clone()),
                        ("varargs", varchar.clone()),
                    ],
                    rows,
                )
            }
            // `DuckDBTriggersFunction`
            "duckdb_triggers" => {
                let mut rows = Vec::new();
                let mut oid = 3000;
                for listed in self.listed_schemas() {
                    let temporary = listed.schema.key() == TEMP_SCHEMA;
                    for trigger in listed.schema.trigger_entries() {
                        oid += 1;
                        let columns = trigger.columns.iter().map(|c| text(c)).collect();
                        rows.push(vec![
                            text(&listed.database),
                            Value::bigint(listed.database_oid),
                            text(&listed.display),
                            Value::bigint(listed.oid),
                            text(&trigger.name),
                            Value::bigint(oid),
                            text(&trigger.table),
                            text(trigger.timing.name()),
                            text(trigger.event.name()),
                            Value::list(varchar.clone(), columns)?,
                            text(trigger.for_each.name()),
                            match &trigger.comment {
                                Some(comment) => text(comment),
                                None => null_text(),
                            },
                            Value::null(tags.clone()),
                            Value::boolean(temporary),
                            text(&trigger.to_sql()),
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("trigger_name", varchar.clone()),
                        ("trigger_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("action_timing", varchar.clone()),
                        ("event_manipulation", varchar.clone()),
                        ("columns", LogicalType::list(varchar.clone())),
                        ("for_each", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("temporary", boolean.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            // `DuckDBSequencesFunction`
            "duckdb_sequences" => {
                let mut rows = Vec::new();
                let mut oid = 2000;
                for (schema_oid, schema) in self.catalog.schemas().iter().enumerate() {
                    let temporary = schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    for sequence in schema.sequence_entries() {
                        oid += 1;
                        let data = sequence.data();
                        let last_value = match data.last_value {
                            Some(value) if data.usage_count > 0 => Value::bigint(value),
                            _ => Value::null(bigint.clone()),
                        };
                        // `SequenceCatalogEntry::ToSQL`
                        let sql = format!(
                            "CREATE SEQUENCE {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START {} {};",
                            sequence.name,
                            data.increment,
                            data.min_value,
                            data.max_value,
                            data.counter,
                            if data.cycle { "CYCLE" } else { "NO CYCLE" }
                        );
                        rows.push(vec![
                            text(if temporary { "temp" } else { self.catalog.name.as_str() }),
                            Value::bigint(if temporary { 3 } else { 1 }),
                            text(if temporary { "main" } else { &schema.name }),
                            Value::bigint(schema_oid as i64 + 1),
                            text(&sequence.name),
                            Value::bigint(oid),
                            sequence.comment.as_deref().map_or_else(null_text, text),
                            Value::null(tags.clone()),
                            Value::boolean(temporary),
                            Value::bigint(data.start_value),
                            Value::bigint(data.min_value),
                            Value::bigint(data.max_value),
                            Value::bigint(data.increment),
                            Value::boolean(data.cycle),
                            last_value,
                            text(&sql),
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("sequence_name", varchar.clone()),
                        ("sequence_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("temporary", boolean.clone()),
                        ("start_value", bigint.clone()),
                        ("min_value", bigint.clone()),
                        ("max_value", bigint.clone()),
                        ("increment_by", bigint.clone()),
                        ("cycle", boolean.clone()),
                        ("last_value", bigint.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            // `DuckDBDialectsFunction`: the dialect extensions, of which there are none
            "duckdb_dialects" => (vec![("dialect_name", varchar.clone())], Vec::new()),
            // `DuckDBKeywordsFunction`
            "duckdb_keywords" => (
                vec![("keyword_name", varchar.clone()), ("keyword_category", varchar.clone())],
                duckrs_parser::keywords::keyword_list()
                    .into_iter()
                    .map(|(word, category)| vec![text(&word), text(category)])
                    .collect(),
            ),
            // `DuckDBExtensionsInit`: the default extensions, installed or not, and those linked into duckrs,
            // which are loaded; duckrs installs no others, so there is no directory of them to list
            "duckdb_extensions" => {
                // `fs.ListFiles` of the extension directory, which the file system can refuse
                if self.options.local_file_system_disabled {
                    return Err(duckrs_common::Error::new(
                        ErrorKind::Permission,
                        "File system LocalFileSystem has been disabled by configuration",
                    ));
                }
                if let Some(access) = &self.options.file_access {
                    let directory = format!(
                        "{}/.duckdb/extensions/v{}/{}",
                        crate::execution::file_system::home_directory(),
                        env!("CARGO_PKG_VERSION"),
                        platform()
                    );
                    access.check(&directory, true)?;
                }
                let list = LogicalType::list(varchar.clone());
                let version = format!("v{}", env!("CARGO_PKG_VERSION"));
                let mut entries: std::collections::BTreeMap<&str, (&str, bool)> = DEFAULT_EXTENSIONS
                    .iter()
                    .map(|(name, description)| (*name, (*description, false)))
                    .collect();
                for name in LINKED_EXTENSIONS {
                    entries.entry(name).or_insert(("", true)).1 = true;
                }
                let rows = entries
                    .into_iter()
                    .map(|(name, (description, linked))| {
                        let aliases = EXTENSION_ALIASES
                            .iter()
                            .filter(|(_, extension)| *extension == name)
                            .map(|(alias, _)| text(alias))
                            .collect();
                        Ok(vec![
                            text(name),
                            Value::boolean(linked),
                            Value::boolean(linked),
                            text(if linked { "(BUILT-IN)" } else { "" }),
                            text(description),
                            Value::list(varchar.clone(), aliases)?,
                            text(if linked { &version } else { "" }),
                            text(if linked { "STATICALLY_LINKED" } else { "NOT_INSTALLED" }),
                            text(""),
                            // a built-in extension is not signed
                            Value::null(varchar.clone()),
                        ])
                    })
                    .collect::<Result<Vec<_>>>()?;
                (
                    vec![
                        ("extension_name", varchar.clone()),
                        ("loaded", boolean.clone()),
                        ("installed", boolean.clone()),
                        ("install_path", varchar.clone()),
                        ("description", varchar.clone()),
                        ("aliases", list),
                        ("extension_version", varchar.clone()),
                        ("install_mode", varchar.clone()),
                        ("installed_from", varchar.clone()),
                        ("signature_key_fingerprint", varchar.clone()),
                    ],
                    rows,
                )
            }
            // `PragmaVersionFunction`
            "pragma_version" => {
                let version = format!("v{}", env!("CARGO_PKG_VERSION"));
                let codename = release_codename(&version);
                (
                    vec![
                        ("library_version", varchar.clone()),
                        ("source_id", varchar.clone()),
                        ("codename", varchar.clone()),
                    ],
                    vec![vec![text(&version), text("duckrs"), text(codename)]],
                )
            }
            // `PragmaUserAgentFunction`: `DBConfig::UserAgent`
            "pragma_user_agent" => {
                let mut agent = format!("duckdb/v{}({})", env!("CARGO_PKG_VERSION"), platform());
                for name in ["duckdb_api", "custom_user_agent"] {
                    let value = self
                        .session
                        .as_ref()
                        .and_then(|session| (session.setting)(name).ok())
                        .filter(|v| !v.is_null())
                        .and_then(|v| v.try_to_string().ok())
                        .unwrap_or_default();
                    if !value.is_empty() {
                        agent = format!("{agent} {value}");
                    }
                }
                (vec![("user_agent", varchar.clone())], vec![vec![text(&agent)]])
            }
            // `DuckDBPreparedStatementsFunction`
            "duckdb_prepared_statements" => {
                let list = LogicalType::list(varchar.clone());
                let types = |types: &[String]| {
                    Value::list(varchar.clone(), types.iter().map(|t| Value::varchar(t.clone())).collect())
                };
                let rows = self
                    .session
                    .as_ref()
                    .map(|session| session.prepared_statements.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|row| {
                        let parameters = match row.parameter_count {
                            0 => Value::null(list.clone()),
                            count => types(&vec!["UNKNOWN".to_string(); count])?,
                        };
                        let results = match row.result_types.as_deref() {
                            Some(result_types) => types(result_types)?,
                            None => Value::null(list.clone()),
                        };
                        Ok(vec![text(&row.name), text(&row.statement), parameters, results])
                    })
                    .collect::<Result<Vec<_>>>()?;
                (
                    vec![
                        ("name", varchar.clone()),
                        ("statement", varchar.clone()),
                        ("parameter_types", list.clone()),
                        ("result_types", list.clone()),
                    ],
                    rows,
                )
            }
            // `PragmaCollateFunction`: the collations of the catalog
            "pragma_collations" => (
                vec![("collname", varchar.clone())],
                crate::planner::collation::collation_names().map(|name| vec![text(name)]).collect(),
            ),
            // `PragmaPlatformFunction`
            "pragma_platform" => (vec![("platform", varchar.clone())], vec![vec![text(&platform())]]),
            "pragma_database_size" => {
                let mut rows = Vec::new();
                for database in self.catalog.databases() {
                    let bytes = database.total_blocks * database.block_size;
                    rows.push(vec![
                        text(&database.name),
                        text(&human_readable_bytes(bytes)),
                        Value::bigint(database.block_size as i64),
                        Value::bigint(database.total_blocks as i64),
                        Value::bigint((database.total_blocks - database.free_blocks) as i64),
                        Value::bigint(database.free_blocks as i64),
                        text(&human_readable_bytes(database.wal_size)),
                        // nothing here accounts for memory yet
                        text(&human_readable_bytes(0)),
                        text("Unlimited"),
                    ]);
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_size", varchar.clone()),
                        ("block_size", bigint.clone()),
                        ("total_blocks", bigint.clone()),
                        ("used_blocks", bigint.clone()),
                        ("free_blocks", bigint.clone()),
                        ("wal_size", varchar.clone()),
                        ("memory_usage", varchar.clone()),
                        ("memory_limit", varchar.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_databases" => {
                let tags_of = |version: Option<&str>| -> Result<Value> {
                    let Some(version) = version else {
                        return Ok(Value::null(tags.clone()));
                    };
                    let entry = Value::struct_value(vec![
                        ("key".to_string(), text("storage_version")),
                        ("value".to_string(), text(version)),
                    ]);
                    Value::new(tags.clone(), duckrs_common::value::ValueData::List(vec![entry]))
                };
                let map_of = |entries: &[(String, String)]| -> Result<Value> {
                    let entries = entries
                        .iter()
                        .map(|(key, value)| {
                            Value::struct_value(vec![
                                ("key".to_string(), text(key)),
                                ("value".to_string(), text(value)),
                            ])
                        })
                        .collect();
                    Value::new(tags.clone(), duckrs_common::value::ValueData::List(entries))
                };
                let database = |name: &str, oid: i64, path: Option<&str>, internal: bool, read_only: bool| {
                    vec![
                        text(name),
                        Value::bigint(oid),
                        path.map_or_else(null_text, text),
                        null_text(),
                        Value::null(tags.clone()),
                        Value::boolean(internal),
                        text("duckdb"),
                        Value::boolean(read_only),
                        Value::boolean(false),
                        null_text(),
                        Value::null(tags.clone()),
                    ]
                };
                let mut rows = vec![
                    database("system", 2, None, true, false),
                    database("temp", 3, None, true, false),
                ];
                let listed = self.catalog.databases();
                if listed.is_empty() {
                    rows.insert(0, database(&self.catalog.name, 1, None, false, false));
                }
                for (index, listed) in listed.iter().enumerate() {
                    let oid = if index == 0 { 1 } else { index as i64 + 3 };
                    let mut row = database(&listed.name, oid, listed.path.as_deref(), false, listed.read_only);
                    row[4] = tags_of(listed.storage_version.as_deref())?;
                    row[10] = map_of(&listed.options)?;
                    if let Some(cipher) = &listed.cipher {
                        row[8] = Value::boolean(true);
                        row[9] = text(cipher);
                    }
                    rows.insert(index.min(rows.len()), row);
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("path", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("type", varchar.clone()),
                        ("readonly", boolean.clone()),
                        ("encrypted", boolean.clone()),
                        ("cipher", varchar.clone()),
                        ("options", tags.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_schemas" => {
                let mut rows = Vec::new();
                let schemas = self.listed_schemas();
                let oid_of = |database: &str, key: &str| {
                    schemas
                        .iter()
                        .find(|s| s.database == database && s.schema.key() == key)
                        .map(|s| s.oid)
                };
                for listed in &schemas {
                    // `DuckDBSchemasFunction`: the schema a nested one is in
                    let schema = &listed.schema;
                    let parent_name = schema.path.len().checked_sub(2).map(|i| schema.path[i].as_str());
                    rows.push(vec![
                        Value::bigint(listed.oid),
                        text(&listed.database),
                        Value::bigint(listed.database_oid),
                        text(&listed.display),
                        null_text(),
                        Value::null(tags.clone()),
                        // `DuckCatalog::Initialize` makes the default schema of each catalog internal
                        Value::boolean(
                            listed.database == "temp"
                                || (schema.path.len() <= 1 && listed.display.eq_ignore_ascii_case("main")),
                        ),
                        null_text(),
                        parent_name.map_or_else(null_text, text),
                        schema
                            .parent_key()
                            .and_then(|parent| oid_of(&listed.database, &parent))
                            .map_or_else(|| Value::null(bigint.clone()), Value::bigint),
                    ]);
                }
                (
                    vec![
                        ("oid", bigint.clone()),
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("sql", varchar.clone()),
                        ("parent_schema", varchar.clone()),
                        ("parent_schema_oid", bigint.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_tables" => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let TableOrView::Table(table) = &listed.entry else {
                        continue;
                    };
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    let primary = table
                        .constraints
                        .iter()
                        .any(|c| matches!(c, TableConstraint::Unique { primary_key: true, .. }));
                    let checks = table
                        .constraints
                        .iter()
                        .filter(|c| matches!(c, TableConstraint::Check { .. }))
                        .count();
                    let backing_indexes = table
                        .constraints
                        .iter()
                        .filter(|c| matches!(c, TableConstraint::Unique { .. } | TableConstraint::ForeignKey(_)))
                        .count();
                    // (not locked while the row is made: its SQL reads the comment too)
                    let comment = table.comment.lock().clone();
                    rows.push(vec![
                        text(&listed.database),
                        Value::bigint(listed.database_oid),
                        text(if temporary { "main" } else { &listed.schema.name }),
                        Value::bigint(listed.schema_oid),
                        text(&table.name),
                        Value::bigint(listed.oid),
                        comment.as_deref().map_or_else(null_text, text),
                        Value::null(tags.clone()),
                        Value::boolean(false),
                        Value::boolean(temporary),
                        Value::boolean(primary),
                        Value::bigint(table.row_count() as i64),
                        Value::bigint(table.logical_columns().len() as i64),
                        // C++ backs each UNIQUE, PRIMARY KEY and FOREIGN KEY with an index
                        Value::bigint((table.indexes.read().len() + backing_indexes) as i64),
                        Value::bigint(checks as i64),
                        // `TableCatalogEntry::ToSQL`
                        crate::storage::table_create_sql(&listed.schema, table)
                            .map_or_else(|_| null_text(), |sql| text(&sql)),
                    ]);
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("table_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("temporary", boolean.clone()),
                        ("has_primary_key", boolean.clone()),
                        ("estimated_size", bigint.clone()),
                        ("column_count", bigint.clone()),
                        ("index_count", bigint.clone()),
                        ("check_constraint_count", bigint.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_indexes" => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let TableOrView::Table(table) = &listed.entry else {
                        continue;
                    };
                    for (position, index) in table.indexes.read().iter().enumerate() {
                        let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                        rows.push(vec![
                            text(&listed.database),
                            Value::bigint(listed.database_oid),
                            text(if temporary { "main" } else { &listed.schema.name }),
                            Value::bigint(listed.schema_oid),
                            text(&index.name),
                            Value::bigint(listed.oid * 1000 + position as i64),
                            text(&table.name),
                            Value::bigint(listed.oid),
                            index.comment.as_deref().map_or_else(&null_text, &text),
                            Value::null(tags.clone()),
                            Value::boolean(index.unique),
                            Value::boolean(false),
                            // `CreateIndexInfo::ExpressionsToList`: what is no bare column is in
                            // parentheses; the list is written as a LIST of VARCHAR is
                            {
                                let items = index
                                    .names
                                    .iter()
                                    .map(|name| {
                                        if table.columns.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
                                            Value::varchar(name.clone())
                                        } else {
                                            Value::varchar(format!("({name})"))
                                        }
                                    })
                                    .collect();
                                text(&Value::list(LogicalType::VARCHAR, items)?.try_to_string()?)
                            },
                            text(&index.sql),
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("index_name", varchar.clone()),
                        ("index_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("table_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("is_unique", boolean.clone()),
                        ("is_primary", boolean.clone()),
                        ("expressions", varchar.clone()),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            "duckdb_constraints" => self.constraint_rows()?,
            "duckdb_views" => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let TableOrView::View(view) = &listed.entry else {
                        continue;
                    };
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    // a view that doesn't bind is still listed, without columns
                    let (names, types) = self.view_columns(view).unwrap_or_default();
                    rows.push(vec![
                        text(&listed.database),
                        Value::bigint(listed.database_oid),
                        text(if temporary { "main" } else { &listed.schema.name }),
                        Value::bigint(listed.schema_oid),
                        text(&view.name),
                        Value::bigint(listed.oid),
                        view.comment.as_deref().map_or_else(null_text, text),
                        Value::null(tags.clone()),
                        Value::boolean(false),
                        Value::boolean(temporary),
                        Value::bigint(names.len() as i64),
                        Value::list(LogicalType::VARCHAR, names.into_iter().map(Value::varchar).collect())?,
                        Value::list(
                            LogicalType::VARCHAR,
                            types.iter().map(|t| Value::varchar(t.to_string())).collect(),
                        )?,
                        text(&view.to_sql()),
                    ]);
                }
                let list = LogicalType::list(LogicalType::VARCHAR);
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("view_name", varchar.clone()),
                        ("view_oid", bigint.clone()),
                        ("comment", varchar.clone()),
                        ("tags", tags.clone()),
                        ("internal", boolean.clone()),
                        ("temporary", boolean.clone()),
                        ("column_count", bigint.clone()),
                        ("column_names", list.clone()),
                        ("column_types", list),
                        ("sql", varchar.clone()),
                    ],
                    rows,
                )
            }
            _ => {
                let mut rows = Vec::new();
                for listed in self.listed_entries() {
                    let temporary = listed.schema.name.eq_ignore_ascii_case(TEMP_SCHEMA);
                    let table_columns: Vec<(Value, LogicalType, bool, Value, Value, bool)> = match &listed.entry {
                        TableOrView::Table(table) => table
                            .logical_columns()
                            .iter()
                            .map(|c| {
                                let not_null = c.definition.is_some_and(|d| d.not_null);
                                let expression = c.generated.map_or_else(null_text, |e| text(&e.to_string()));
                                let default = c
                                    .definition
                                    .and_then(|d| d.default_text.as_deref())
                                    .map_or_else(|| expression.clone(), text);
                                (
                                    text(c.name),
                                    c.ty.clone(),
                                    not_null,
                                    default,
                                    expression,
                                    c.generated.is_some(),
                                )
                            })
                            .collect(),
                        TableOrView::View(view) => match self.view_columns(view) {
                            Ok((names, types)) => names
                                .into_iter()
                                .zip(types)
                                .map(|(n, t)| (Value::varchar(n), t, false, null_text(), null_text(), false))
                                .collect(),
                            // `ViewColumnHelper`: a view that doesn't bind has a placeholder column
                            Err(_) => vec![(
                                null_text(),
                                LogicalType::INVALID,
                                false,
                                null_text(),
                                null_text(),
                                false,
                            )],
                        },
                    };
                    for (index, (column, ty, not_null, default, expression, generated)) in
                        table_columns.into_iter().enumerate()
                    {
                        // (precision, radix, scale)
                        let binary = |bits: i32| (Some(bits), Some(2), Some(0));
                        let (numeric_precision, numeric_radix, numeric_scale) = match ty.id() {
                            LogicalTypeId::Decimal => {
                                let (width, scale) = ty.decimal_properties().unwrap_or((18, 3));
                                (Some(width.into()), Some(10), Some(scale.into()))
                            }
                            LogicalTypeId::HugeInt => binary(128),
                            LogicalTypeId::BigInt => binary(64),
                            LogicalTypeId::Integer => binary(32),
                            LogicalTypeId::SmallInt => binary(16),
                            LogicalTypeId::TinyInt => binary(8),
                            LogicalTypeId::Float => binary(24),
                            LogicalTypeId::Double => binary(53),
                            _ => (None, None, None),
                        };
                        // what `COMMENT ON COLUMN` said
                        let name = column.try_to_string().unwrap_or_default();
                        let comment = match &listed.entry {
                            TableOrView::Table(table) => table
                                .columns
                                .iter()
                                .find(|c| c.name.eq_ignore_ascii_case(&name))
                                .and_then(|c| c.comment.clone()),
                            TableOrView::View(view) => view
                                .column_comments
                                .iter()
                                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                                .map(|(_, c)| c.clone()),
                        };
                        rows.push(vec![
                            text(&listed.database),
                            Value::bigint(listed.database_oid),
                            text(if temporary { "main" } else { &listed.schema.name }),
                            Value::bigint(listed.schema_oid),
                            text(listed.entry.name()),
                            Value::bigint(listed.oid),
                            column,
                            Value::integer(index as i32 + 1),
                            comment.as_deref().map_or_else(null_text, text),
                            Value::boolean(false),
                            default,
                            Value::boolean(!not_null),
                            text(&ty.to_string()),
                            if ty.id() == LogicalTypeId::Invalid {
                                Value::null(bigint.clone())
                            } else {
                                Value::bigint(ty.id() as i64)
                            },
                            Value::null(integer.clone()),
                            numeric_precision.map_or_else(|| Value::null(integer.clone()), Value::integer),
                            numeric_radix.map_or_else(|| Value::null(integer.clone()), Value::integer),
                            numeric_scale.map_or_else(|| Value::null(integer.clone()), Value::integer),
                            Value::null(tags.clone()),
                            Value::boolean(generated),
                            expression,
                        ]);
                    }
                }
                (
                    vec![
                        ("database_name", varchar.clone()),
                        ("database_oid", bigint.clone()),
                        ("schema_name", varchar.clone()),
                        ("schema_oid", bigint.clone()),
                        ("table_name", varchar.clone()),
                        ("table_oid", bigint.clone()),
                        ("column_name", varchar.clone()),
                        ("column_index", integer.clone()),
                        ("comment", varchar.clone()),
                        ("internal", boolean.clone()),
                        ("column_default", varchar.clone()),
                        ("is_nullable", boolean.clone()),
                        ("data_type", varchar.clone()),
                        ("data_type_id", bigint.clone()),
                        ("character_maximum_length", integer.clone()),
                        ("numeric_precision", integer.clone()),
                        ("numeric_precision_radix", integer.clone()),
                        ("numeric_scale", integer.clone()),
                        ("tags", tags.clone()),
                        ("is_generated", boolean.clone()),
                        ("generation_expression", varchar.clone()),
                    ],
                    rows,
                )
            }
        };
        let (names, types): (Vec<String>, Vec<LogicalType>) =
            columns.into_iter().map(|(n, t)| (n.to_string(), t)).unzip();
        self.bind_rows(table_ref, &name.to_lowercase(), names, types, rows)
    }
}
