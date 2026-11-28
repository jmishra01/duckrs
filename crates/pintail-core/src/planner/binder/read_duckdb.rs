// Ported from duckdb/src/function/table/read_duckdb.cpp @ rust-port-baseline
//
//! `read_duckdb`: a table of each of the database files a pattern names, attached read-only and hidden for
//! the statement, read as the union of their scans.

use super::binder_error;
use super::copy::strings;
use super::table_functions::{named_argument, parse_select};
use super::{Binder, Binding};
use crate::catalog::{Catalog, TableCatalogEntry, TableOrView};
use crate::planner::logical::LogicalOperator;
use pintail_common::types::LogicalType;
use pintail_common::{Error, ErrorKind, Result};
use pintail_parser::ast::{FunctionExpr, TableRef};
use std::sync::Arc;

/// A database file opened for `read_duckdb`: its catalog, and what has to stay alive while it is read.
#[derive(Clone)]
pub struct ForeignDatabase {
    pub name: String,
    pub catalog: Arc<Catalog>,
    pub keep: Arc<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for ForeignDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignDatabase").field("name", &self.name).finish()
    }
}

/// `DuckDBReadBindData`'s options.
#[derive(Default)]
struct Selection {
    schema_name: Option<String>,
    table_name: Option<String>,
}

impl Selection {
    fn matches(&self, table: &TableCatalogEntry) -> bool {
        self.schema_name.as_ref().is_none_or(|s| *s == table.schema)
            && self.table_name.as_ref().is_none_or(|t| *t == table.name)
    }

    fn has_selection(&self) -> bool {
        self.schema_name.is_some() || self.table_name.is_some()
    }

    /// `DuckDBFileReaderOptions::PrintOptions`.
    fn print(&self) -> String {
        let mut options = String::new();
        if let Some(schema) = &self.schema_name {
            options += &format!("schema_name=\"{schema}\"");
        }
        if !options.is_empty() {
            options += ", ";
        }
        if let Some(table) = &self.table_name {
            options += &format!("table_name=\"{table}\"");
        }
        options
    }

    /// `DuckDBFileReaderOptions::GetCandidates`.
    fn candidates(&self, tables: &[Arc<TableCatalogEntry>]) -> String {
        if tables.is_empty() {
            return String::new();
        }
        let list: Vec<String> = tables
            .iter()
            .map(|table| {
                let conflicts = tables.iter().filter(|other| other.name == table.name).count() > 1;
                if conflicts {
                    format!("{}.{}", table.schema, table.name)
                } else {
                    table.name.clone()
                }
            })
            .collect();
        let mut term = self.schema_name.as_ref().map(|s| format!("{s}.")).unwrap_or_default();
        term += self.table_name.as_deref().unwrap_or_default();
        pintail_common::string_util::candidates_error_message(&list, &term, "Candidates")
    }
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Binder<'_> {
    /// `ReadDuckDBTableFunction`.
    pub(super) fn bind_read_duckdb(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut patterns = None;
        let mut selection = Selection::default();
        let mut union_by_name = false;
        for argument in &args {
            let value = self.constant_argument(&argument.expr)?;
            if argument.name.is_empty() {
                if patterns.is_some() {
                    return Err(binder_error(format!("{name} takes one positional parameter")));
                }
                if value.is_null() {
                    return Err(binder_error(format!("{name} cannot take NULL list as parameter")));
                }
                patterns = Some(strings(&value)?);
                continue;
            }
            let option = argument.name.to_lowercase();
            match option.as_str() {
                "schema_name" | "table_name" => {
                    if value.is_null() {
                        return Err(binder_error(format!(
                            "read_duckdb parameter \"{option}\" cannot be NULL"
                        )));
                    }
                    let text = value.try_to_string()?;
                    if option == "schema_name" {
                        selection.schema_name = Some(text);
                    } else {
                        selection.table_name = Some(text);
                    }
                }
                "union_by_name" => union_by_name = super::copy::is_true(&value, &option)?,
                _ => return Err(crate::unported(format!("the {name} option {option}"))),
            }
        }
        let patterns = patterns.ok_or_else(|| binder_error(format!("{name} requires a file name")))?;
        let mut files = Vec::new();
        for pattern in &patterns {
            self.check_file_access(pattern)?;
            let found = crate::execution::file_system::glob(pattern);
            if found.is_empty() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        let open = self
            .session
            .as_ref()
            .map(|session| session.open_database.clone())
            .ok_or_else(|| crate::unported("read_duckdb here"))?;
        // the table of each file (`DuckDBReader::DuckDBReader`)
        let mut readers = Vec::with_capacity(files.len());
        for file in &files {
            let (catalog, keep) = open(file)?;
            let mut tables = Vec::new();
            let mut candidates = Vec::new();
            for schema in catalog.schemas() {
                if schema.key() == crate::catalog::TEMP_SCHEMA {
                    continue;
                }
                for entry in schema.tables_and_views() {
                    let TableOrView::Table(table) = entry else { continue };
                    if selection.matches(&table) {
                        tables.push(table.clone());
                    }
                    candidates.push(table);
                }
            }
            if tables.len() != 1 {
                let problem = if tables.is_empty() {
                    "does not have any tables"
                } else {
                    "has multiple tables"
                };
                let extra = if selection.has_selection() {
                    format!(" matching {}", selection.print())
                } else {
                    "\nSelect a table using `table_name='<name>'".to_string()
                };
                return Err(binder_error(format!(
                    "Database \"{file}\" {problem}{extra}{}",
                    selection.candidates(&candidates)
                )));
            }
            let foreign = ForeignDatabase {
                name: format!("__duckdb_reader_{}", self.foreign_databases.len()),
                catalog,
                keep,
            };
            self.foreign_databases.push(foreign.clone());
            readers.push((file.clone(), foreign, tables.remove(0)));
        }
        // the columns: those of the first file, or with `union_by_name` those of all of them
        let columns_of = |table: &TableCatalogEntry| -> Vec<(String, LogicalType, bool)> {
            table
                .logical_columns()
                .iter()
                .map(|c| (c.name.to_string(), c.ty.clone(), c.generated.is_some()))
                .collect()
        };
        let mut columns = columns_of(&readers[0].2);
        if union_by_name {
            for (_, _, table) in &readers[1..] {
                for (column, ty, generated) in columns_of(table) {
                    if !columns.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&column)) {
                        columns.push((column, ty, generated));
                    }
                }
            }
        }
        let mut parts = Vec::with_capacity(readers.len());
        for (file, foreign, table) in &readers {
            let own = columns_of(table);
            let mut select = Vec::with_capacity(columns.len() + 2);
            for (column, ty, _) in &columns {
                match own.iter().find(|(n, ..)| n.eq_ignore_ascii_case(column)) {
                    // `TryInitializeScan`: a generated column is not read
                    Some((n, _, true)) => select.push(format!(
                        "error({})::{ty} AS {}",
                        literal(&format!("Unsupported: read_duckdb cannot read generated column {n}")),
                        quoted(column)
                    )),
                    Some((n, ..)) => select.push(format!("{} AS {}", quoted(n), quoted(column))),
                    None if union_by_name => select.push(format!("NULL::{ty} AS {}", quoted(column))),
                    None => {
                        let names: Vec<&str> = own.iter().map(|(n, ..)| n.as_str()).collect();
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "Failed to read file \"{file}\": schema mismatch in glob: column \"{column}\" was read \
                                 from the original file \"{}\", but could not be found in file \"{file}\".\nCandidate \
                                 names: {}\nIf you are trying to read files with different schemas, try setting \
                                 union_by_name=True",
                                readers[0].0,
                                names.join(", ")
                            ),
                        ));
                    }
                }
            }
            select.push(format!("{} AS filename", literal(file)));
            select.push("rowid AS rowid".to_string());
            parts.push(format!(
                "SELECT {} FROM {}.{}.{}",
                select.join(", "),
                quoted(&foreign.name),
                quoted(&table.schema),
                quoted(&table.name)
            ));
        }
        let select = parse_select(&parts.join(" UNION ALL "), "Expected a single SELECT statement")?;
        let mut binder = self.child_binder();
        let bound = binder.bind_select_statement(&select);
        self.next_table_index = binder.next_table_index;
        let bound = bound?;
        let alias = if table_ref.alias.is_empty() {
            name.clone()
        } else {
            table_ref.alias.clone()
        };
        let count = columns.len();
        let names: Vec<String> = columns.iter().map(|(n, ..)| n.clone()).collect();
        let names = Self::alias_columns(table_ref, &alias, names)?;
        let table_index = bound.plan.column_bindings()[0].table_index;
        // `filename` and `rowid` are virtual columns (`GetVirtualColumns`), which `*` leaves out
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            catalog: None,
            table_index,
            names,
            types: bound.types[..count].to_vec(),
            row_id: None,
            generated: Vec::new(),
            virtual_columns: vec![
                ("filename".to_string(), LogicalType::VARCHAR, count),
                ("rowid".to_string(), LogicalType::BIGINT, count + 1),
            ],
        });
        Ok(bound.plan)
    }
}
