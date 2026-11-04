// Ported from duckdb/src/catalog/{catalog,catalog_set}.cpp,
// duckdb/src/catalog/catalog_entry/{duck_schema_entry,macro_catalog_entry,sequence_catalog_entry,
// table_catalog_entry,view_catalog_entry}.cpp and
// duckdb/src/common/exception/catalog_exception.cpp @ rust-port-baseline
//
//! The catalog: schemas with their tables and views, in memory. Names are case-insensitive and keep the
//! case they were created with. There are no transactions yet, so changes are visible at once;
//! MVCC visibility arrives with Phase 10, and table storage moves to `DataTable` with Phase 9.

use crate::planner::expression::BoundExpr;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{MacroFunction, SelectStatement, sql_quoted_identifier};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};

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
    /// Beyond the NOT NULLs, which are in `columns`.
    pub constraints: Vec<TableConstraint>,
    /// The tables with a foreign key to this one.
    pub referenced_by: Mutex<Vec<Weak<TableCatalogEntry>>>,
    /// The keys of each `TableConstraint::Unique`, in order, when somebody needed them since
    /// the rows last moved. Whoever rewrites `storage` must reset this.
    pub unique_keys: Mutex<Option<Vec<UniqueKeys>>>,
}

/// The keys of one UNIQUE or PRIMARY KEY, with the position of their rows.
pub type UniqueKeys = HashMap<Vec<u8>, usize>;

/// A bound constraint of a table (`BoundConstraint`).
#[derive(Debug, Clone)]
pub enum TableConstraint {
    /// The expression reads the columns of the table by index.
    Check {
        expression: BoundExpr,
        text: String,
    },
    Unique {
        columns: Vec<usize>,
        primary_key: bool,
    },
    ForeignKey(ForeignKey),
}

/// A foreign key, on the table that has it. `columns` match `referenced_columns` one by one,
/// which are the columns of a UNIQUE or PRIMARY KEY of the referenced table in its order.
#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub columns: Vec<usize>,
    /// `None` for a table that references itself.
    pub referenced: Option<Arc<TableCatalogEntry>>,
    pub referenced_columns: Vec<usize>,
}

impl TableCatalogEntry {
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.ty.clone()).collect()
    }

    /// Drops the rows from `rows` on, which a statement that failed added.
    pub fn truncate(&self, rows: usize) {
        self.storage.write().truncate(rows);
        *self.unique_keys.lock() = None;
    }

    /// The column with this name (case-insensitive), by index.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }
}

/// A view (`ViewCatalogEntry`): a query, bound anew each time the view is read.
#[derive(Debug)]
pub struct ViewCatalogEntry {
    pub schema: String,
    pub name: String,
    pub query: SelectStatement,
    /// the names listed in `CREATE VIEW v (a, b)`
    pub aliases: Vec<String>,
}

/// A macro with its overloads (`ScalarMacroCatalogEntry`, `TableMacroCatalogEntry`).
#[derive(Debug)]
pub struct MacroCatalogEntry {
    pub name: String,
    pub table_macro: bool,
    pub macros: Vec<MacroFunction>,
}

impl MacroCatalogEntry {
    /// `CatalogTypeToString`.
    pub fn type_name(&self) -> &'static str {
        macro_type_name(self.table_macro)
    }
}

/// `CatalogTypeToString` of a macro entry.
pub fn macro_type_name(table_macro: bool) -> &'static str {
    if table_macro {
        "Table Macro Function"
    } else {
        "Macro Function"
    }
}

/// The state of a sequence (`SequenceData`).
#[derive(Debug, Clone)]
pub struct SequenceData {
    /// the value `nextval` returns next
    pub counter: i64,
    pub last_value: Option<i64>,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
}

/// A sequence (`SequenceCatalogEntry`). Its values are not transactional, as in DuckDB.
#[derive(Debug)]
pub struct SequenceCatalogEntry {
    pub name: String,
    data: Mutex<SequenceData>,
}

impl SequenceCatalogEntry {
    /// `SequenceCatalogEntry::CurrentValue`.
    pub fn current_value(&self) -> Result<i64> {
        self.data.lock().last_value.ok_or_else(|| {
            Error::new(
                ErrorKind::Sequence,
                "currval: sequence is not yet defined in this session",
            )
        })
    }

    /// `SequenceCatalogEntry::NextValue`.
    pub fn next_value(&self) -> Result<i64> {
        let mut data = self.data.lock();
        let result = data.counter;
        let next = data.counter.checked_add(data.increment);
        let next_counter = if data.cycle {
            match next {
                None if data.increment < 0 => data.max_value,
                None => data.min_value,
                Some(next) if next < data.min_value => data.max_value,
                Some(next) if next > data.max_value => data.min_value,
                Some(next) => next,
            }
        } else {
            // an exhausted sequence keeps a counter outside of its range
            let mut exhausted = result < data.min_value || result > data.max_value;
            let mut counter = next.unwrap_or(0);
            if !exhausted && next.is_none() {
                // the counter moves to an int64 outside of the range, if there is one
                let outside = [i64::MAX, i64::MIN]
                    .into_iter()
                    .find(|c| *c < data.min_value || *c > data.max_value);
                match outside {
                    Some(outside) => counter = outside,
                    None => exhausted = true,
                }
            }
            if exhausted {
                let (bound, value) = if data.increment < 0 {
                    ("minimum", data.min_value)
                } else {
                    ("maximum", data.max_value)
                };
                return Err(Error::new(
                    ErrorKind::Sequence,
                    format!(
                        "nextval: reached {bound} value of sequence {} ({value})",
                        sql_quoted_identifier(&self.name)
                    ),
                ));
            }
            counter
        };
        data.counter = next_counter;
        data.last_value = Some(result);
        Ok(result)
    }

    /// `SequenceCatalogEntry::SetValue`.
    pub fn set_value(&self, value: i64, is_called: bool) -> Result<i64> {
        {
            let mut data = self.data.lock();
            if value < data.min_value || value > data.max_value {
                return Err(Error::new(
                    ErrorKind::Sequence,
                    format!(
                        "setval: value {value} is out of bounds for sequence {} ({}..{})",
                        sql_quoted_identifier(&self.name),
                        data.min_value,
                        data.max_value
                    ),
                ));
            }
            data.counter = value;
            if !is_called {
                return Ok(value);
            }
        }
        // as if nextval had just returned `value`
        self.next_value()
    }
}

/// An entry of a schema's table set, which tables and views share, so that a table and a view
/// cannot have the same name.
#[derive(Debug, Clone)]
pub enum TableOrView {
    Table(Arc<TableCatalogEntry>),
    View(Arc<ViewCatalogEntry>),
}

impl TableOrView {
    pub fn name(&self) -> &str {
        match self {
            Self::Table(table) => &table.name,
            Self::View(view) => &view.name,
        }
    }

    /// `CatalogTypeToString`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Table(_) => "Table",
            Self::View(_) => "View",
        }
    }
}

/// A schema (`SchemaCatalogEntry`).
#[derive(Debug)]
pub struct SchemaCatalogEntry {
    pub name: String,
    tables: RwLock<BTreeMap<String, TableOrView>>,
    sequences: RwLock<BTreeMap<String, Arc<SequenceCatalogEntry>>>,
    /// the scalar macros and the table macros, which can share a name
    macros: [RwLock<BTreeMap<String, Arc<MacroCatalogEntry>>>; 2],
    /// the types of CREATE TYPE, by name
    types: RwLock<BTreeMap<String, LogicalType>>,
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
            sequences: RwLock::new(BTreeMap::new()),
            macros: [RwLock::new(BTreeMap::new()), RwLock::new(BTreeMap::new())],
            types: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn get_type(&self, name: &str) -> Option<LogicalType> {
        self.types.read().get(&key(name)).cloned()
    }

    /// `DuckSchemaEntry::CreateType`.
    pub fn create_type(&self, name: &str, ty: LogicalType, on_conflict: OnCreateConflict) -> Result<()> {
        let mut types = self.types.write();
        if types.contains_key(&key(name)) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Type with name \"{name}\" already exists!"),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(()),
                OnCreateConflict::Replace => {}
            }
        }
        types.insert(key(name), ty);
        Ok(())
    }

    /// Drops a type, returning whether it existed.
    pub fn drop_type(&self, name: &str) -> bool {
        self.types.write().remove(&key(name)).is_some()
    }

    /// The table or view with this name.
    pub fn get_entry(&self, name: &str) -> Option<TableOrView> {
        self.tables.read().get(&key(name)).cloned()
    }

    /// `DuckSchemaEntry::AddEntryInternal` up to adding the entry: whether to add it, given what
    /// exists under its name.
    fn check_conflict(
        tables: &BTreeMap<String, TableOrView>,
        name: &str,
        type_name: &str,
        on_conflict: OnCreateConflict,
    ) -> Result<bool> {
        let Some(existing) = tables.get(&key(name)) else {
            return Ok(true);
        };
        match on_conflict {
            OnCreateConflict::Error => Err(Error::new(
                ErrorKind::Catalog,
                format!("{} with name \"{name}\" already exists!", existing.type_name()),
            )),
            OnCreateConflict::Ignore => Ok(false),
            OnCreateConflict::Replace if existing.type_name() != type_name => Err(Error::new(
                ErrorKind::Catalog,
                format!(
                    "Existing object {} is of type {}, trying to replace with type {type_name}",
                    sql_quoted_identifier(name),
                    existing.type_name()
                ),
            )),
            OnCreateConflict::Replace => {
                Self::check_not_referenced(tables, existing)?;
                Ok(true)
            }
        }
    }

    /// `AlterFKInfo` of a DROP: a table that a foreign key points at has to stay.
    fn check_not_referenced(tables: &BTreeMap<String, TableOrView>, entry: &TableOrView) -> Result<()> {
        let TableOrView::Table(table) = entry else {
            return Ok(());
        };
        for child in table.referenced_by.lock().iter().filter_map(Weak::upgrade) {
            if matches!(tables.get(&key(&child.name)), Some(TableOrView::Table(t)) if Arc::ptr_eq(t, &child)) {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!(
                        "Could not drop the table because this table is main key table of the table {}",
                        sql_quoted_identifier(&child.name)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `DuckSchemaEntry::CreateTable`: `None` when the table exists and the conflict is ignored.
    pub fn create_table(
        &self,
        name: &str,
        columns: Vec<ColumnDefinition>,
        constraints: Vec<TableConstraint>,
        on_conflict: OnCreateConflict,
    ) -> Result<Option<Arc<TableCatalogEntry>>> {
        let mut tables = self.tables.write();
        if !Self::check_conflict(&tables, name, "Table", on_conflict)? {
            return Ok(None);
        }
        let types = columns.iter().map(|c| c.ty.clone()).collect();
        let table = Arc::new(TableCatalogEntry {
            schema: self.name.clone(),
            name: name.to_string(),
            columns,
            storage: RwLock::new(ColumnDataCollection::new(types)),
            constraints,
            referenced_by: Mutex::default(),
            unique_keys: Mutex::default(),
        });
        for constraint in &table.constraints {
            if let TableConstraint::ForeignKey(ForeignKey {
                referenced: Some(parent),
                ..
            }) = constraint
            {
                parent.referenced_by.lock().push(Arc::downgrade(&table));
            }
        }
        tables.insert(key(name), TableOrView::Table(table.clone()));
        Ok(Some(table))
    }

    /// `DuckSchemaEntry::CreateView`.
    pub fn create_view(
        &self,
        name: &str,
        query: SelectStatement,
        aliases: Vec<String>,
        on_conflict: OnCreateConflict,
    ) -> Result<()> {
        let mut tables = self.tables.write();
        if Self::check_conflict(&tables, name, "View", on_conflict)? {
            let view = ViewCatalogEntry {
                schema: self.name.clone(),
                name: name.to_string(),
                query,
                aliases,
            };
            tables.insert(key(name), TableOrView::View(Arc::new(view)));
        }
        Ok(())
    }

    /// `DuckSchemaEntry::DropEntry`, for `type_name` entries: whether there was one to drop.
    pub fn drop_entry(&self, name: &str, type_name: &str) -> Result<bool> {
        let mut tables = self.tables.write();
        let Some(existing) = tables.get(&key(name)) else {
            return Ok(false);
        };
        if existing.type_name() != type_name {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!(
                    "Existing object {} is of type {}, trying to drop type {type_name}",
                    sql_quoted_identifier(name),
                    existing.type_name()
                ),
            ));
        }
        Self::check_not_referenced(&tables, existing)?;
        tables.remove(&key(name));
        Ok(true)
    }

    pub fn get_sequence(&self, name: &str) -> Option<Arc<SequenceCatalogEntry>> {
        self.sequences.read().get(&key(name)).cloned()
    }

    /// `DuckSchemaEntry::CreateSequence`.
    pub fn create_sequence(&self, name: &str, data: SequenceData, on_conflict: OnCreateConflict) -> Result<()> {
        let mut sequences = self.sequences.write();
        if sequences.contains_key(&key(name)) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Sequence with name \"{name}\" already exists!"),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(()),
                OnCreateConflict::Replace => {}
            }
        }
        let sequence = SequenceCatalogEntry {
            name: name.to_string(),
            data: Mutex::new(data),
        };
        sequences.insert(key(name), Arc::new(sequence));
        Ok(())
    }

    /// Drops a sequence, returning whether it existed.
    pub fn drop_sequence(&self, name: &str) -> bool {
        self.sequences.write().remove(&key(name)).is_some()
    }

    pub fn get_macro(&self, name: &str, table_macro: bool) -> Option<Arc<MacroCatalogEntry>> {
        self.macros[usize::from(table_macro)].read().get(&key(name)).cloned()
    }

    /// `DuckSchemaEntry::CreateFunction` for a macro.
    pub fn create_macro(
        &self,
        name: &str,
        table_macro: bool,
        macros: Vec<MacroFunction>,
        on_conflict: OnCreateConflict,
    ) -> Result<()> {
        let mut entries = self.macros[usize::from(table_macro)].write();
        if entries.contains_key(&key(name)) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("{} with name \"{name}\" already exists!", macro_type_name(table_macro)),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(()),
                OnCreateConflict::Replace => {}
            }
        }
        let entry = MacroCatalogEntry {
            name: name.to_string(),
            table_macro,
            macros,
        };
        entries.insert(key(name), Arc::new(entry));
        Ok(())
    }

    /// Drops a macro of this kind: whether there was one to drop.
    pub fn drop_macro(&self, name: &str, table_macro: bool) -> bool {
        self.macros[usize::from(table_macro)]
            .write()
            .remove(&key(name))
            .is_some()
    }

    /// The type and name of each entry, for "depends on" messages.
    fn entries(&self) -> Vec<(String, String)> {
        let tables = self.tables.read();
        let sequences = self.sequences.read();
        let types = self.types.read();
        let (scalar_macros, table_macros) = (self.macros[0].read(), self.macros[1].read());
        tables
            .values()
            .map(|e| (e.type_name().to_lowercase(), e.name().to_string()))
            .chain(sequences.values().map(|s| ("sequence".to_string(), s.name.clone())))
            .chain(types.keys().map(|t| ("type".to_string(), t.clone())))
            .chain(
                scalar_macros
                    .values()
                    .chain(table_macros.values())
                    .map(|m| (m.type_name().to_lowercase(), m.name.clone())),
            )
            .collect()
    }
}

/// A database's catalog (`DuckCatalog`).
#[derive(Debug)]
pub struct Catalog {
    pub name: String,
    schemas: RwLock<BTreeMap<String, Arc<SchemaCatalogEntry>>>,
}

/// What a catalog holds at a moment, to go back to on ROLLBACK, until the undo buffer of Phase 10.
#[derive(Debug)]
pub struct CatalogSnapshot {
    schemas: BTreeMap<String, Arc<SchemaCatalogEntry>>,
    entries: Vec<SchemaEntries>,
    /// sharing their buffers with the tables, whose writes copy what they change
    table_rows: Vec<(Arc<TableCatalogEntry>, ColumnDataCollection)>,
}

/// The contents of a schema in a [`CatalogSnapshot`].
#[derive(Debug)]
struct SchemaEntries {
    schema: Arc<SchemaCatalogEntry>,
    tables: BTreeMap<String, TableOrView>,
    sequences: BTreeMap<String, Arc<SequenceCatalogEntry>>,
    macros: [BTreeMap<String, Arc<MacroCatalogEntry>>; 2],
    types: BTreeMap<String, LogicalType>,
}

impl Catalog {
    /// A snapshot to [`Catalog::restore`] later.
    pub fn snapshot(&self) -> CatalogSnapshot {
        let schemas = self.schemas.read().clone();
        let mut entries = Vec::with_capacity(schemas.len());
        let mut table_rows = Vec::new();
        for schema in schemas.values() {
            let tables = schema.tables.read().clone();
            for entry in tables.values() {
                if let TableOrView::Table(table) = entry {
                    let rows = table.storage.read().clone();
                    table_rows.push((table.clone(), rows));
                }
            }
            entries.push(SchemaEntries {
                schema: schema.clone(),
                tables,
                sequences: schema.sequences.read().clone(),
                macros: [schema.macros[0].read().clone(), schema.macros[1].read().clone()],
                types: schema.types.read().clone(),
            });
        }
        CatalogSnapshot {
            schemas,
            entries,
            table_rows,
        }
    }

    /// Goes back to a snapshot: what was created since is gone, and what was dropped is back.
    pub fn restore(&self, snapshot: CatalogSnapshot) {
        *self.schemas.write() = snapshot.schemas;
        for saved in snapshot.entries {
            *saved.schema.tables.write() = saved.tables;
            *saved.schema.sequences.write() = saved.sequences;
            let [scalar, table] = saved.macros;
            *saved.schema.macros[0].write() = scalar;
            *saved.schema.macros[1].write() = table;
            *saved.schema.types.write() = saved.types;
        }
        for (table, rows) in snapshot.table_rows {
            *table.storage.write() = rows;
            *table.unique_keys.lock() = None;
        }
    }

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

    /// `Catalog::GetEntry`: looks the name up in `schema`, or without one along the search path:
    /// `search_first` (the schema of the view being bound), then the default schema.
    pub fn get_entry(&self, schema: Option<&str>, search_first: Option<&str>, name: &str) -> Result<TableOrView> {
        let entry = match schema {
            Some(schema) => self.schema_for(Some(schema))?.get_entry(name),
            None => search_first
                .iter()
                .chain([&DEFAULT_SCHEMA])
                .find_map(|schema| self.get_schema(schema)?.get_entry(name)),
        };
        entry.ok_or_else(|| missing_entry("Table", name))
    }

    /// `Catalog::GetEntry` for a sequence, with the lookup of [`Self::get_entry`].
    pub fn get_sequence(
        &self,
        schema: Option<&str>,
        search_first: Option<&str>,
        name: &str,
    ) -> Result<Arc<SequenceCatalogEntry>> {
        let entry = match schema {
            Some(schema) => self.schema_for(Some(schema))?.get_sequence(name),
            None => search_first
                .iter()
                .chain([&DEFAULT_SCHEMA])
                .find_map(|schema| self.get_schema(schema)?.get_sequence(name)),
        };
        entry.ok_or_else(|| missing_entry("Sequence", name))
    }

    /// `Catalog::GetEntry` for a macro, with the lookup of [`Self::get_entry`].
    pub fn get_macro(
        &self,
        schema: Option<&str>,
        search_first: Option<&str>,
        name: &str,
        table_macro: bool,
    ) -> Result<Option<Arc<MacroCatalogEntry>>> {
        Ok(match schema {
            Some(schema) => self.schema_for(Some(schema))?.get_macro(name, table_macro),
            None => search_first
                .iter()
                .chain([&DEFAULT_SCHEMA])
                .find_map(|schema| self.get_schema(schema)?.get_macro(name, table_macro)),
        })
    }

    /// `Catalog::GetEntry` for a type.
    pub fn get_type(
        &self,
        schema: Option<&str>,
        search_first: Option<&str>,
        name: &str,
    ) -> Result<Option<LogicalType>> {
        Ok(match schema {
            Some(schema) => self.schema_for(Some(schema))?.get_type(name),
            None => search_first
                .iter()
                .chain([&DEFAULT_SCHEMA])
                .find_map(|schema| self.get_schema(schema)?.get_type(name)),
        })
    }

    /// `Catalog::GetEntry` for a table: a view by that name is an error.
    pub fn get_table(&self, schema: Option<&str>, name: &str) -> Result<Arc<TableCatalogEntry>> {
        match self.get_entry(schema, None, name)? {
            TableOrView::Table(table) => Ok(table),
            TableOrView::View(_) => Err(Error::new(ErrorKind::Catalog, format!("{name} is not an table"))),
        }
    }
}

/// `DependencyManager::VerifyExistence`: a schema with tables cannot just go away.
fn dependency_check(schema: &SchemaCatalogEntry) -> Result<()> {
    let entries = schema.entries();
    if entries.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "Cannot drop entry \"{}\" because there are entries that depend on it.\n",
        schema.name
    );
    for (type_name, name) in entries {
        message += &format!("{type_name} \"{name}\" depends on schema \"{}\".\n", schema.name);
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
            .create_table("Tbl", columns.clone(), Vec::new(), OnCreateConflict::Error)
            .unwrap();
        assert_eq!(catalog.get_table(None, "TBL").unwrap().name, "Tbl");
        let err = schema
            .create_table("tbl", columns.clone(), Vec::new(), OnCreateConflict::Error)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Catalog Error: Table with name \"tbl\" already exists!"
        );
        assert!(
            schema
                .create_table("tbl", columns, Vec::new(), OnCreateConflict::Ignore)
                .unwrap()
                .is_none()
        );
        let err = catalog.get_table(None, "nope").unwrap_err();
        assert_eq!(err.to_string(), "Catalog Error: Table with name nope does not exist!");
    }
}
