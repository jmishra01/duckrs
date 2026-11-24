// Ported from duckdb/src/planner/binder/statement/bind_export.cpp,
// duckdb/src/execution/operator/persistent/physical_export.cpp and `PragmaImportDatabase` of
// duckdb/src/function/pragma/pragma_queries.cpp @ rust-port-baseline
//
//! `EXPORT DATABASE`: a COPY of each table to a directory, with `schema.sql` to create the catalog
//! again and `load.sql` to read the tables back; and `IMPORT DATABASE`, which runs the two.

use super::{ClientContext, MaterializedQueryResult};
use duckrs_core::catalog::{Catalog, DEFAULT_SCHEMA, SchemaCatalogEntry, TEMP_SCHEMA, TableCatalogEntry, TableConstraint};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    self, CopyInfo, CreateInfo, CreateKind, Literal, OnCreateConflict, QualifiedName, Statement, sql_identifier,
    sql_string,
};
use std::sync::Arc;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

/// `SanitizeExportIdentifier`: lower case letters, the rest underscores.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '_',
        })
        .collect()
}

/// `FileSystem::JoinPath`.
fn join_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_string()
    } else if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// The options of CSV that a COPY FROM reads (`CSVListCopyOptions`, READ_ONLY and READ_WRITE).
const CSV_READ_OPTIONS: &[&str] = &[
    "auto_detect",
    "sample_size",
    "skip",
    "max_line_size",
    "maximum_line_size",
    "ignore_errors",
    "buffer_size",
    "decimal_separator",
    "null_padding",
    "parallel",
    "allow_quoted_nulls",
    "store_rejects",
    "force_not_null",
    "rejects_table",
    "rejects_scan",
    "rejects_limit",
    "encoding",
    "thousands",
    "new_line",
    "date_format",
    "dateformat",
    "timestamp_format",
    "timestampformat",
    "quote",
    "comment",
    "delim",
    "delimiter",
    "sep",
    "separator",
    "escape",
    "header",
    "nullstr",
    "null",
    "compression",
    "strict_mode",
];

/// Those of Parquet (`ParquetListCopyOptions`).
const PARQUET_READ_OPTIONS: &[&str] = &[
    "compression",
    "codec",
    "encryption_config",
    "debug_use_openssl",
    "binary_as_string",
    "file_row_number",
    "can_have_nan",
    "int96_as",
];

/// `ReorderTableEntries`: the tables without foreign keys, then each after those it references.
fn reorder_tables(
    tables: Vec<(Arc<SchemaCatalogEntry>, Arc<TableCatalogEntry>)>,
) -> Vec<(Arc<SchemaCatalogEntry>, Arc<TableCatalogEntry>)> {
    let references = |table: &TableCatalogEntry| -> Vec<String> {
        table
            .constraints
            .iter()
            .filter_map(|c| match c {
                TableConstraint::ForeignKey(key) => key.referenced.as_ref().map(|r| r.name.to_lowercase()),
                _ => None,
            })
            .collect()
    };
    let mut ordered: Vec<(Arc<SchemaCatalogEntry>, Arc<TableCatalogEntry>)> = Vec::new();
    let mut unordered = tables;
    let mut primary_keys_only = true;
    while !unordered.is_empty() {
        let mut remaining = Vec::new();
        let before = ordered.len();
        for (schema, table) in unordered {
            let referenced = references(&table);
            let ready = if primary_keys_only {
                referenced.is_empty()
            } else {
                referenced.iter().all(|name| {
                    ordered
                        .iter()
                        .any(|(s, t)| Arc::ptr_eq(s, &schema) && t.name.to_lowercase() == *name)
                })
            };
            if ready {
                ordered.push((schema, table));
            } else {
                remaining.push((schema, table));
            }
        }
        // a reference that can't be resolved would loop forever: take the rest as they are
        if !primary_keys_only && ordered.len() == before {
            ordered.extend(remaining);
            break;
        }
        primary_keys_only = false;
        unordered = remaining;
    }
    ordered
}

/// `IsTypeLossy` and `TypeVisitor::VisitReplace`: `ty` with HUGEINT and UHUGEINT as VARCHAR, if it has them.
fn lossless_type(ty: &duckrs_common::types::LogicalType) -> Option<duckrs_common::types::LogicalType> {
    use duckrs_common::types::{LogicalType, LogicalTypeId as T};
    fn replace(ty: &LogicalType) -> LogicalType {
        let children = |ty: &LogicalType| -> Vec<(String, LogicalType)> {
            ty.struct_children()
                .iter()
                .map(|(n, c)| (n.clone(), replace(c)))
                .collect()
        };
        match ty.id() {
            T::HugeInt | T::UHugeInt => LogicalType::VARCHAR,
            T::List => LogicalType::list(replace(ty.child_type().expect("a LIST has a child"))),
            T::Array => LogicalType::array(
                replace(ty.child_type().expect("an ARRAY has a child")),
                ty.array_size().unwrap_or(0),
            ),
            T::Map => {
                let entry = ty.child_type().expect("a MAP has entries");
                let fields = entry.struct_children();
                LogicalType::map(replace(&fields[0].1), replace(&fields[1].1))
            }
            T::Struct => LogicalType::struct_type(children(ty)),
            // past the tag, the members
            T::Union => LogicalType::union_type(children(ty).split_off(1)),
            _ => ty.clone(),
        }
    }
    let replaced = replace(ty);
    (replaced != *ty).then_some(replaced)
}

/// The name of a table to read or write it with, without the default schema.
fn table_name(schema: &SchemaCatalogEntry, table: &str) -> String {
    let mut parts = Vec::new();
    if !schema.name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
        parts.extend(schema.path.iter().map(|p| sql_identifier(p)));
    }
    parts.push(sql_identifier(table));
    parts.join(".")
}

fn qualified(schema: &SchemaCatalogEntry, name: &str) -> QualifiedName {
    QualifiedName {
        qualification: if schema.name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            Vec::new()
        } else {
            schema.path.clone()
        },
        name: name.to_string(),
    }
}

/// `PhysicalExport::GetNaiveExportOrder` and `WriteCatalogEntries`: the statements of `schema.sql`.
fn schema_statements(
    catalog: &Catalog,
    tables: &[(Arc<SchemaCatalogEntry>, Arc<TableCatalogEntry>)],
) -> Result<Vec<String>> {
    let schemas: Vec<Arc<SchemaCatalogEntry>> = catalog
        .schemas()
        .into_iter()
        .filter(|s| !s.name.eq_ignore_ascii_case(TEMP_SCHEMA))
        .collect();
    let mut schema_sql = Vec::new();
    let mut sequences = Vec::new();
    let mut types = Vec::new();
    let mut macros = Vec::new();
    let mut views = Vec::new();
    let mut indexes = Vec::new();
    for schema in &schemas {
        if !schema.name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            let mut path = schema.path.clone();
            let name = path.pop().unwrap_or_else(|| schema.name.clone());
            schema_sql.push(
                CreateInfo {
                    name: QualifiedName {
                        qualification: path,
                        name,
                    },
                    on_conflict: OnCreateConflict::Error,
                    temporary: false,
                    kind: CreateKind::Schema { options: Vec::new() },
                }
                .to_string(),
            );
        }
        for sequence in schema.sequence_entries() {
            let data = sequence.data();
            sequences.push(
                CreateInfo {
                    name: qualified(schema, &sequence.name),
                    on_conflict: OnCreateConflict::Error,
                    temporary: false,
                    kind: CreateKind::Sequence {
                        increment: data.increment,
                        min_value: data.min_value,
                        max_value: data.max_value,
                        start_value: data.counter,
                        cycle: data.cycle,
                    },
                }
                .to_string(),
            );
        }
        for (name, ty) in schema.type_entries() {
            let name = qualified(schema, &name);
            types.push(format!("CREATE TYPE {name} AS {};", ty));
        }
        for table_macro in [false, true] {
            for entry in schema.macro_entries(table_macro) {
                macros.push((
                    schema.ident(),
                    entry.clone(),
                    CreateInfo {
                        name: qualified(schema, &entry.name),
                        on_conflict: OnCreateConflict::Error,
                        temporary: false,
                        kind: CreateKind::Macro {
                            table_macro,
                            macros: entry.macros.clone(),
                        },
                    }
                    .to_string(),
                ));
            }
        }
        for entry in schema.tables_and_views() {
            match entry {
                duckrs_core::catalog::TableOrView::View(view) => views.push(view.to_sql()),
                duckrs_core::catalog::TableOrView::Table(table) => {
                    for index in table.indexes.read().iter() {
                        let sql = index.sql.trim_end().trim_end_matches(';');
                        indexes.push(format!("{sql};"));
                    }
                }
            }
        }
    }
    // `PhysicalExport::ExtractEntries`: tables, then macros; `DependencyManager::ReorderEntries` puts a macro that
    // a table's defaults or generated columns call before the tables
    let mut table_sql = Vec::with_capacity(tables.len());
    for (schema, table) in tables {
        table_sql.push(duckrs_core::storage::table_create_sql(schema, table)?);
    }
    let called_by_tables = |entry: &duckrs_core::catalog::MacroCatalogEntry| {
        let name = entry.name.to_lowercase();
        let quoted = format!("\"{name}\"(");
        let bare = format!("{name}(");
        !entry.table_macro
            && table_sql.iter().any(|sql| {
                let sql = sql.to_lowercase();
                sql.contains(&quoted)
                    || sql.match_indices(&bare).any(|(at, _)| {
                        at == 0 || !sql[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_')
                    })
            })
    };
    let (early, late): (Vec<_>, Vec<_>) = macros.into_iter().partition(|(_, entry, _)| called_by_tables(entry));
    let mut statements = schema_sql;
    statements.extend(sequences);
    statements.extend(types);
    statements.extend(order_macros(early));
    statements.extend(table_sql);
    statements.extend(order_macros(late));
    statements.extend(views);
    statements.extend(indexes);
    Ok(statements)
}

/// `WriteStringStreamToFile` (`FILE_CREATE_NEW` truncates what is there).
fn write_new_file(path: &str, content: &str) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {error}")))?;
    file.write_all(content.as_bytes())
        .map_err(|error| Error::new(ErrorKind::Io, format!("Could not write file \"{path}\": {error}")))
}

impl ClientContext {
    /// `Binder::Bind(ExportStatement)` and `PhysicalExport`.
    pub(super) fn export_database(&mut self, database: &str, info: &CopyInfo) -> Result<MaterializedQueryResult> {
        let format = if info.format_given {
            info.format.to_lowercase()
        } else {
            "csv".to_string()
        };
        match format.as_str() {
            "csv" | "parquet" | "json" => {}
            _ => {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("Copy Function with name {format} does not exist!"),
                ));
            }
        }
        // the COPY statements run on the database that is exported, as a statement naming it would
        let previous = if database.is_empty() {
            None
        } else {
            let found = self
                .find_database(database)
                .ok_or_else(|| Error::new(ErrorKind::Binder, format!("Catalog \"{database}\" does not exist!")))?;
            Some(std::mem::replace(&mut self.db, found))
        };
        let result = self.export_tables(info);
        if let Some(previous) = previous {
            self.db = previous;
        }
        result
    }

    fn export_tables(&mut self, info: &CopyInfo) -> Result<MaterializedQueryResult> {
        let format = if info.format_given {
            info.format.to_lowercase()
        } else {
            "csv".to_string()
        };
        let extension = match format.as_str() {
            "csv" => "csv",
            "parquet" => "parquet",
            _ => "json",
        };
        let catalog = self.catalog();
        let mut tables = Vec::new();
        for schema in catalog.schemas() {
            if schema.name.eq_ignore_ascii_case(TEMP_SCHEMA) {
                continue;
            }
            for entry in schema.tables_and_views() {
                if let duckrs_core::catalog::TableOrView::Table(table) = entry {
                    tables.push((schema.clone(), table));
                }
            }
        }
        let tables = reorder_tables(tables);
        for (_, table) in &tables {
            let self_reference = table
                .constraints
                .iter()
                .any(|c| matches!(c, TableConstraint::ForeignKey(key) if key.referenced.is_none()));
            if self_reference {
                return Err(binder_error(format!(
                    "Failed to export database: table {} has a self-referencing foreign key constraint which is \
                     currently not supported for exporting",
                    table.name
                )));
            }
        }
        let directory = &info.file_path;
        let mut taken = std::collections::HashSet::new();
        let mut exported = Vec::with_capacity(tables.len());
        for (schema, table) in &tables {
            let mut id = 0;
            let path = loop {
                let suffix = if id == 0 { String::new() } else { format!("_{id}") };
                let name = if schema.name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
                    format!("{}{suffix}.{extension}", sanitize(&table.name))
                } else {
                    format!(
                        "{}_{}{suffix}.{extension}",
                        sanitize(&schema.name),
                        sanitize(&table.name)
                    )
                };
                let path = join_path(directory, &name);
                if taken.insert(path.clone()) {
                    break path;
                }
                id += 1;
            };
            exported.push((schema.clone(), table.clone(), path));
        }
        if let Some(access) = self.file_access() {
            access.check(directory, true)?;
        }
        if !std::path::Path::new(directory).is_dir() {
            std::fs::create_dir(directory).map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("Failed to create directory \"{directory}\": {error}"),
                )
            })?;
        }
        for (schema, table, path) in &exported {
            // `ParquetWriteSelect`: EXPORT writes the "lossy" types as VARCHAR, so they come back exactly
            let columns: Vec<String> = table
                .columns
                .iter()
                .map(|c| {
                    let name = sql_identifier(&c.name);
                    match lossless_type(&c.ty).filter(|_| format == "parquet") {
                        Some(ty) => format!("CAST({name} AS {ty}) AS {name}"),
                        None => name,
                    }
                })
                .collect();
            let sql = format!(
                "COPY (SELECT {} FROM {}) TO {}",
                columns.join(", "),
                table_name(schema, &table.name),
                sql_string(path)
            );
            let Statement::Copy(mut copy) = duckrs_parser::parse_ast(&sql)?.remove(0) else {
                unreachable!("the statement is a COPY");
            };
            copy.format.clone_from(&format);
            copy.format_given = true;
            copy.options.clone_from(&info.options);
            copy.parsed_options.clone_from(&info.parsed_options);
            self.execute_statement(&Statement::Copy(copy))?;
        }
        let statements = schema_statements(&catalog, &tables)?;
        let mut schema_sql = String::new();
        for statement in statements {
            schema_sql += &statement;
            schema_sql.push('\n');
        }
        schema_sql.push('\n');
        write_new_file(&join_path(directory, "schema.sql"), &schema_sql)?;
        let load_sql = load_statements(&format, info, &exported);
        write_new_file(&join_path(directory, "load.sql"), &load_sql)?;
        Ok(Self::success())
    }

    /// `Binder::Bind(CopyDatabaseStatement)`: with `schema_only` the entries of `from` created in `to`
    /// (`BindCopyDatabaseSchema`), otherwise the rows of each of its tables inserted into the table of the same name
    /// there (`BindCopyDatabaseData`). A statement runs in one database here, so the rows are read in the one and
    /// appended in the other.
    pub(super) fn copy_database(&mut self, from: &str, to: &str, schema_only: bool) -> Result<MaterializedQueryResult> {
        let find = |context: &Self, name: &str| {
            context
                .find_database(name)
                .ok_or_else(|| Error::new(ErrorKind::Binder, format!("Catalog \"{name}\" does not exist!")))
        };
        let source = find(self, from)?;
        if to.eq_ignore_ascii_case("system") {
            return Err(binder_error("Cannot copy to the system catalog"));
        }
        let target = find(self, to)?;
        if Arc::ptr_eq(&source, &target) {
            return Err(binder_error(format!(
                "Cannot copy from {from} to {to} - FROM and TO databases are the same"
            )));
        }
        // `RegisterDBModify` of the target
        target.check_writable(&Statement::CopyDatabase {
            from: from.to_string(),
            to: to.to_string(),
            schema_only,
        })?;
        let previous = std::mem::replace(&mut self.db, source.clone());
        let result = self.copy_database_from(&target, schema_only);
        self.db = previous;
        result
    }

    /// [`Self::copy_database`], with `self.db` the database copied from.
    fn copy_database_from(
        &mut self,
        target: &Arc<super::DatabaseInstance>,
        schema_only: bool,
    ) -> Result<MaterializedQueryResult> {
        let catalog = self.catalog();
        let mut tables = Vec::new();
        for schema in catalog.schemas() {
            if schema.name.eq_ignore_ascii_case(TEMP_SCHEMA) {
                continue;
            }
            for entry in schema.tables_and_views() {
                if let duckrs_core::catalog::TableOrView::Table(table) = entry {
                    tables.push((schema.clone(), table));
                }
            }
        }
        let tables = reorder_tables(tables);
        let source = std::mem::replace(&mut self.db, target.clone());
        let result = if schema_only {
            // `IGNORE_ON_CONFLICT` for the schemas, as the target may have them already
            schema_statements(&catalog, &tables).and_then(|statements| {
                for statement in statements {
                    let statement = match statement.strip_prefix("CREATE SCHEMA ") {
                        Some(rest) if !rest.starts_with("IF NOT EXISTS") => {
                            format!("CREATE SCHEMA IF NOT EXISTS {rest}")
                        }
                        _ => statement,
                    };
                    self.run_in_statement(&statement)?;
                }
                Ok(())
            })
        } else {
            let mut copied = Ok(());
            for (schema, table) in &tables {
                let columns: Vec<(String, duckrs_common::types::LogicalType)> =
                    table.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                let names: Vec<String> = columns.iter().map(|(name, _)| sql_identifier(name)).collect();
                let select = format!("SELECT {} FROM {}", names.join(", "), table_name(schema, &table.name));
                self.db = source.clone();
                let rows = self.run_in_statement(&select);
                self.db = target.clone();
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(error) => {
                        copied = Err(error);
                        break;
                    }
                };
                let chunks: Vec<_> = rows.collection.chunks().cloned().collect();
                // `ClientContext::Append`, within this statement
                let (names, types): (Vec<String>, Vec<_>) = columns.into_iter().unzip();
                let column_list: Vec<String> = names.iter().map(|name| sql_identifier(name)).collect();
                self.appended = Some((names, types, Arc::new(chunks)));
                let inserted = self.run_in_statement(&format!(
                    "INSERT INTO {} ({}) SELECT * FROM __internal_appender_chunks()",
                    table_name(schema, &table.name),
                    column_list.join(", ")
                ));
                self.appended = None;
                if let Err(error) = inserted {
                    copied = Err(error);
                    break;
                }
            }
            copied
        };
        self.db = source;
        result.map(|()| Self::success())
    }

    /// The statements of `sql`, run as part of the statement that is running.
    fn run_in_statement(&mut self, sql: &str) -> Result<MaterializedQueryResult> {
        let mut result = Self::success();
        for statement in duckrs_parser::parse_ast_with(sql, &self.parser_options())? {
            result = self.execute_statement(&statement)?;
        }
        Ok(result)
    }

    /// `PragmaImportDatabase`: runs `schema.sql`, then the COPY statements of `load.sql` on the
    /// files next to it.
    pub(super) fn import_database(&mut self, directory: &str) -> Result<MaterializedQueryResult> {
        let access = self.file_access();
        let read = |name: &str| -> Result<String> {
            let path = join_path(directory, name);
            if let Some(access) = &access {
                access.check(&path, false)?;
            }
            std::fs::read_to_string(&path)
                .map_err(|error| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {error}")))
        };
        let schema = read("schema.sql")?;
        let load = read("load.sql")?;
        let mut statements = duckrs_parser::parse_ast(&schema)?;
        for statement in duckrs_parser::parse_ast(&load)? {
            let Statement::Copy(mut copy) = statement else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("IMPORT DATABASE: load.sql may only contain COPY statements, found: {statement}"),
                ));
            };
            let name = copy
                .file_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or_default()
                .to_string();
            copy.file_path = join_path(directory, &name);
            statements.push(Statement::Copy(copy));
        }
        let mut result = Self::success();
        for statement in &statements {
            result = self.execute_statement(statement)?;
        }
        Ok(result)
    }
}

/// `WriteCopyStatement` for each table: the options of the EXPORT that a read takes, and CSV's defaults.
fn load_statements(
    format: &str,
    info: &CopyInfo,
    exported: &[(Arc<SchemaCatalogEntry>, Arc<TableCatalogEntry>, String)],
) -> String {
    let mut options: Vec<(String, Vec<Literal>)> = info
        .options
        .iter()
        .map(|(name, values)| (name.to_lowercase(), values.clone()))
        .collect();
    let mut parsed: Vec<(String, Option<ast::Expr>)> = info
        .parsed_options
        .iter()
        .map(|(name, expr)| (name.to_lowercase(), expr.clone()))
        .collect();
    let has = |options: &[(String, Vec<Literal>)], parsed: &[(String, Option<ast::Expr>)], name: &str| {
        options.iter().any(|(n, _)| n == name) || parsed.iter().any(|(n, _)| n == name)
    };
    if format == "csv" {
        if !has(&options, &parsed, "header") {
            options.push(("header".to_string(), vec![Literal::integer(1)]));
        }
        if !["delimiter", "sep", "delim"].iter().any(|n| has(&options, &parsed, n)) {
            options.push(("delimiter".to_string(), vec![Literal::string(",")]));
        }
        if !has(&options, &parsed, "quote") {
            options.push(("quote".to_string(), vec![Literal::string("\"")]));
        }
        options.retain(|(n, _)| n != "force_quote");
        parsed.retain(|(n, _)| n != "force_quote");
    }
    let readable: Option<&[&str]> = match format {
        "csv" => Some(CSV_READ_OPTIONS),
        "parquet" => Some(PARQUET_READ_OPTIONS),
        _ => None,
    };
    if let Some(readable) = readable {
        options.retain(|(n, _)| readable.contains(&n.as_str()));
        parsed.retain(|(n, _)| readable.contains(&n.as_str()));
    }
    let allow_quoted_nulls = has(&options, &parsed, "allow_quoted_nulls");
    let mut out = String::new();
    for (schema, table, path) in exported {
        let mut table_options = options.clone();
        let mut line = format!(
            "COPY {} FROM {} (FORMAT '{format}'",
            table_name(schema, &table.name),
            sql_string(&path.replace('\\', "/"))
        );
        if format == "csv" {
            table_options.retain(|(n, _)| n != "force_not_null");
            let not_null: Vec<Literal> = table
                .columns
                .iter()
                .filter(|c| c.not_null)
                .map(|c| Literal::string(c.name.clone()))
                .collect();
            if !not_null.is_empty() {
                table_options.push(("force_not_null".to_string(), not_null));
            }
            if !allow_quoted_nulls {
                line += ", allow_quoted_nulls false";
            }
        }
        for (name, values) in &table_options {
            let values: Vec<String> = if values.is_empty() {
                vec!["true".to_string()]
            } else {
                values.iter().map(ast::literal_to_sql).collect()
            };
            if values.len() == 1 {
                line += &format!(", {name} {}", values[0]);
            } else {
                line += &format!(", {name} ({})", values.join(", "));
            }
        }
        for (name, expr) in &parsed {
            match expr {
                Some(expr) => line += &format!(", {name} {expr}"),
                None => line += &format!(", {name} true"),
            }
        }
        line += ");\n";
        out += &line;
    }
    out
}

/// `DependencyManager::ReorderEntries` for macros: one that calls another comes after it.
fn order_macros(mut pending: Vec<(String, Arc<duckrs_core::catalog::MacroCatalogEntry>, String)>) -> Vec<String> {
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let waits = |entry: &duckrs_core::catalog::MacroCatalogEntry,
                     pending: &[(String, Arc<duckrs_core::catalog::MacroCatalogEntry>, String)]| {
            entry.dependencies.iter().any(|d| {
                d.table_macro.is_some_and(|kind| {
                    pending.iter().any(|(schema, other, _)| {
                        !std::ptr::eq(&**other, entry)
                            && other.table_macro == kind
                            && schema.eq_ignore_ascii_case(&d.schema)
                            && other.name.eq_ignore_ascii_case(&d.name)
                    })
                })
            })
        };
        // a cycle can't be ordered: what is left goes in the order it was found
        let at = (0..pending.len()).find(|&i| !waits(&pending[i].1, &pending)).unwrap_or(0);
        ordered.push(pending.remove(at).2);
    }
    ordered
}
