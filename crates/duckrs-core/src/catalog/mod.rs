// Ported from duckdb/src/catalog/{catalog,catalog_set}.cpp,
// duckdb/src/catalog/catalog_entry/{duck_schema_entry,table_catalog_entry}.cpp and
// duckdb/src/common/exception/catalog_exception.cpp @ rust-port-baseline
//
//! The catalog: schemas and their tables, in memory. Names are case-insensitive and keep the
//! case they were created with. There are no transactions yet, so changes are visible at once;
//! MVCC visibility arrives with Phase 10, and table storage moves to `DataTable` with Phase 9.

use crate::planner::expression::BoundExpr;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The name of the in-memory database's catalog.
pub const DEFAULT_CATALOG: &str = "memory";
/// The schema used when a name is not qualified (`DEFAULT_SCHEMA`).
pub const DEFAULT_SCHEMA: &str = "main";

/// A column of a table (`ColumnDefinition`).
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    pub name: String,
    pub ty: LogicalType,
    /// The value of the column when an INSERT leaves it out; NULL without one.
    pub default_value: Option<BoundExpr>,
    /// A `NOT NULL` constraint.
    pub not_null: bool,
}

impl ColumnDefinition {
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self {
            name: name.into(),
            ty,
            default_value: None,
            not_null: false,
        }
    }
}

/// A table (`TableCatalogEntry`) and its rows.
#[derive(Debug)]
pub struct TableCatalogEntry {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnDefinition>,
    pub storage: RwLock<ColumnDataCollection>,
}

impl TableCatalogEntry {
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.ty.clone()).collect()
    }

    /// The column with this name (case-insensitive), by index.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }
}

/// A schema (`SchemaCatalogEntry`).
#[derive(Debug)]
pub struct SchemaCatalogEntry {
    pub name: String,
    tables: RwLock<BTreeMap<String, Arc<TableCatalogEntry>>>,
}

/// What to do when an entry being created already exists (`OnCreateConflict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnCreateConflict {
    Error,
    Ignore,
    Replace,
}

fn key(name: &str) -> String {
    name.to_lowercase()
}

impl SchemaCatalogEntry {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            tables: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn get_table(&self, name: &str) -> Option<Arc<TableCatalogEntry>> {
        self.tables.read().get(&key(name)).cloned()
    }

    /// `DuckSchemaEntry::CreateTable`: `None` when the table exists and the conflict is ignored.
    pub fn create_table(
        &self,
        name: &str,
        columns: Vec<ColumnDefinition>,
        on_conflict: OnCreateConflict,
    ) -> Result<Option<Arc<TableCatalogEntry>>> {
        let mut tables = self.tables.write();
        if tables.contains_key(&key(name)) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Table with name \"{name}\" already exists!"),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(None),
                OnCreateConflict::Replace => {}
            }
        }
        let types = columns.iter().map(|c| c.ty.clone()).collect();
        let table = Arc::new(TableCatalogEntry {
            schema: self.name.clone(),
            name: name.to_string(),
            columns,
            storage: RwLock::new(ColumnDataCollection::new(types)),
        });
        tables.insert(key(name), table.clone());
        Ok(Some(table))
    }

    /// Drops a table, returning whether it existed.
    pub fn drop_table(&self, name: &str) -> bool {
        self.tables.write().remove(&key(name)).is_some()
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.read().values().map(|t| t.name.clone()).collect()
    }
}

/// A database's catalog (`DuckCatalog`).
#[derive(Debug)]
pub struct Catalog {
    pub name: String,
    schemas: RwLock<BTreeMap<String, Arc<SchemaCatalogEntry>>>,
}

impl Catalog {
    /// A catalog with the default `main` schema.
    pub fn new(name: &str) -> Self {
        let catalog = Self {
            name: name.to_string(),
            schemas: RwLock::new(BTreeMap::new()),
        };
        catalog
            .schemas
            .write()
            .insert(key(DEFAULT_SCHEMA), Arc::new(SchemaCatalogEntry::new(DEFAULT_SCHEMA)));
        catalog
    }

    /// `DuckCatalog::CreateSchema`.
    pub fn create_schema(&self, name: &str, on_conflict: OnCreateConflict) -> Result<()> {
        let mut schemas = self.schemas.write();
        if schemas.contains_key(&key(name)) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Schema with name \"{name}\" already exists!"),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(()),
                OnCreateConflict::Replace => dependency_check(&schemas[&key(name)])?,
            }
        }
        schemas.insert(key(name), Arc::new(SchemaCatalogEntry::new(name)));
        Ok(())
    }

    /// `DuckCatalog::DropSchema`: a schema with tables only goes with CASCADE.
    pub fn drop_schema(&self, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        let mut schemas = self.schemas.write();
        let Some(schema) = schemas.get(&key(name)) else {
            return if if_exists {
                Ok(())
            } else {
                Err(missing_entry("Schema", name))
            };
        };
        if name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Cannot drop entry {name} because it is an internal system entry"),
            ));
        }
        if !cascade {
            dependency_check(schema)?;
        }
        schemas.remove(&key(name));
        Ok(())
    }

    pub fn get_schema(&self, name: &str) -> Option<Arc<SchemaCatalogEntry>> {
        self.schemas.read().get(&key(name)).cloned()
    }

    /// The schema an object qualified with `schema` (or not, with `None`) lives in.
    pub fn schema_for(&self, schema: Option<&str>) -> Result<Arc<SchemaCatalogEntry>> {
        let name = schema.unwrap_or(DEFAULT_SCHEMA);
        self.get_schema(name).ok_or_else(|| missing_entry("Schema", name))
    }

    /// `Catalog::GetEntry` for a table: looks the name up in `schema` (or the default schema).
    pub fn get_table(&self, schema: Option<&str>, name: &str) -> Result<Arc<TableCatalogEntry>> {
        let entry = match schema {
            Some(schema) => self.schema_for(Some(schema))?.get_table(name),
            None => self.get_schema(DEFAULT_SCHEMA).and_then(|s| s.get_table(name)),
        };
        entry.ok_or_else(|| missing_entry("Table", name))
    }
}

/// `DependencyManager::VerifyExistence`: a schema with tables cannot just go away.
fn dependency_check(schema: &SchemaCatalogEntry) -> Result<()> {
    let names = schema.table_names();
    if names.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "Cannot drop entry \"{}\" because there are entries that depend on it.\n",
        schema.name
    );
    for table in names {
        message += &format!("table \"{table}\" depends on schema \"{}\".\n", schema.name);
    }
    message += "Use DROP...CASCADE to drop all dependents.";
    Err(Error::new(ErrorKind::Dependency, message))
}

/// `CatalogException::MissingEntry`, without the "Did you mean" suggestion yet.
pub fn missing_entry(kind: &str, name: &str) -> Error {
    Error::new(ErrorKind::Catalog, format!("{kind} with name {name} does not exist!"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_case_insensitive() {
        let catalog = Catalog::new(DEFAULT_CATALOG);
        let schema = catalog.schema_for(None).unwrap();
        let columns = vec![ColumnDefinition::new("i", LogicalType::INTEGER)];
        schema
            .create_table("Tbl", columns.clone(), OnCreateConflict::Error)
            .unwrap();
        assert_eq!(catalog.get_table(None, "TBL").unwrap().name, "Tbl");
        let err = schema
            .create_table("tbl", columns.clone(), OnCreateConflict::Error)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Catalog Error: Table with name \"tbl\" already exists!"
        );
        assert!(
            schema
                .create_table("tbl", columns, OnCreateConflict::Ignore)
                .unwrap()
                .is_none()
        );
        let err = catalog.get_table(None, "nope").unwrap_err();
        assert_eq!(err.to_string(), "Catalog Error: Table with name nope does not exist!");
    }
}
