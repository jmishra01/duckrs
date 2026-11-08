// Ported from duckdb/src/catalog/{catalog,catalog_set}.cpp,
// duckdb/src/catalog/catalog_entry/{duck_schema_entry,macro_catalog_entry,sequence_catalog_entry,
// table_catalog_entry,view_catalog_entry}.cpp and
// duckdb/src/common/exception/catalog_exception.cpp @ rust-port-baseline
//
//! The catalog: schemas with their tables and views, in memory. Names are case-insensitive and keep the
//! case they were created with. A transaction works on a copy of all of it: see `fork`.

use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::string_util::similarity_rating;
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{Expr, MacroFunction, SelectStatement, sql_identifier, sql_quoted_identifier};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Weak};

pub mod default_types;
pub mod defaults;
mod defaults_table;
mod fork;
mod row_ids;
pub use fork::{CatalogVersions, Fork, RowVersions, TableOrigin, tick};
pub use row_ids::{IdSet, MAX_ROW_ID, RowIds};

/// The name of the in-memory database's catalog.
pub const DEFAULT_CATALOG: &str = "memory";
/// The schema used when a name is not qualified (`DEFAULT_SCHEMA`).
pub const DEFAULT_SCHEMA: &str = "main";
/// Where temporary objects live, ahead of the other schemas on the search path. DuckDB has a
/// `temp` catalog per connection; here it is one schema, which connections share.
pub const TEMP_SCHEMA: &str = "temp";
/// `DEFAULT_ROW_GROUP_SIZE`.
const ROW_GROUP_ROWS: usize = 122_880;

/// A column of a table (`ColumnDefinition`).
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    pub name: String,
    pub ty: LogicalType,
    /// The value of the column when an INSERT leaves it out; NULL without one.
    pub default_value: Option<BoundExpr>,
    /// A `NOT NULL` constraint.
    pub not_null: bool,
    /// The default as it was written.
    pub default_text: Option<String>,
}

/// A generated column: not stored, only an expression over the other columns.
#[derive(Debug, Clone)]
pub struct GeneratedColumn {
    pub name: String,
    pub ty: LogicalType,
    /// Reads columns by bare name.
    pub expr: Expr,
    /// The expression over the stored columns, by index.
    pub bound: BoundExpr,
    /// Among all the columns of the table.
    pub position: usize,
}

/// A column of a table as a query sees it (`ColumnList::Logical`).
pub struct LogicalColumn<'a> {
    pub name: &'a str,
    pub ty: &'a LogicalType,
    pub definition: Option<&'a ColumnDefinition>,
    pub generated: Option<&'a Expr>,
}

impl ColumnDefinition {
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self {
            name: name.into(),
            ty,
            default_value: None,
            not_null: false,
            default_text: None,
        }
    }
}

/// The rows of a table that stay in a database file: row groups that can be read one at a time,
/// and only the columns that are needed.
pub trait TableSource: std::fmt::Debug + Send + Sync {
    /// Its rows that aren't deleted.
    fn row_count(&self) -> usize;
    /// Their ids.
    fn row_ids(&self) -> RowIds;
    fn group_count(&self) -> usize;
    /// The rows of a group that aren't deleted.
    fn group_rows(&self, group: usize) -> usize;
    /// The rows of a group, with `columns` of the table in that order.
    fn read_group(&self, group: usize, columns: &[usize]) -> Result<DataChunk>;
    /// All of it.
    fn read_all(&self) -> Result<ColumnDataCollection>;
    /// What the file says about the values of the table's columns, without reading them.
    fn column_statistics(&self) -> Option<Vec<ColumnStatistics>>;
    /// What the file says about a column per group, without reading its values.
    fn zones(&self, column: usize) -> Result<Vec<ColumnZone>>;
    /// `pragma_storage_info`: the segments, in the order that C++ lists them. Reading what fills
    /// `segment_info` takes their blocks, so that is only done on request.
    fn segments(&self, include_info: bool) -> Result<Vec<SegmentInfo>>;
}

/// A row of `pragma_storage_info`.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub row_group: u64,
    pub column: usize,
    pub path: String,
    pub segment: u64,
    pub segment_type: String,
    pub start: u64,
    pub count: u64,
    pub compression: &'static str,
    pub block_id: i64,
    pub block_offset: u64,
    /// `get_segment_info`, for the codecs that have one and if it was asked for.
    pub info: Option<String>,
}

/// A database as `duckdb_databases()` and `pragma_database_size()` list it.
#[derive(Debug, Clone)]
pub struct DatabaseListing {
    pub name: String,
    pub path: Option<String>,
    pub read_only: bool,
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub wal_size: u64,
}

/// What knows of all databases of a connection, as a catalog only knows of itself.
pub trait DatabaseList: Send + Sync + std::fmt::Debug {
    fn databases(&self) -> Vec<DatabaseListing>;
    /// Hands over the catalog of the database of that name, if there is one.
    fn with_catalog(&self, name: &str, use_it: &mut dyn FnMut(&Catalog));
}

/// Reads a `TableSource` chunk by chunk, numbered as those of a table in memory: all but the last
/// are full. That isn't how a file has them when groups are small or rows deleted, so groups go
/// through a buffer that holds what the next chunk needs and no more.
#[derive(Debug)]
pub struct TableCursor {
    source: Arc<dyn TableSource>,
    columns: Vec<usize>,
    types: Vec<LogicalType>,
    next_group: usize,
    /// Whether groups and chunks line up, which lets a scan skip the groups it needn't read.
    aligned: bool,
    buffer: ColumnDataCollection,
    /// The number of the first chunk of `buffer`.
    first_chunk: usize,
}

impl TableCursor {
    pub fn new(source: Arc<dyn TableSource>, columns: Vec<usize>, types: Vec<LogicalType>) -> Self {
        let aligned = Self::is_aligned(&*source);
        let buffer = ColumnDataCollection::new(types.clone());
        Self {
            source,
            columns,
            types,
            next_group: 0,
            aligned,
            buffer,
            first_chunk: 0,
        }
    }

    /// Whether the groups of a source are the groups of a table in memory.
    pub fn is_aligned(source: &dyn TableSource) -> bool {
        let groups = source.group_count();
        (0..groups).all(|g| {
            let rows = source.group_rows(g);
            rows == ROW_GROUP_ROWS || (g + 1 == groups && rows <= ROW_GROUP_ROWS)
        })
    }

    /// Chunk `index`, of chunks asked for in order.
    pub fn chunk(&mut self, index: usize) -> Result<DataChunk> {
        let group = index / (ROW_GROUP_ROWS / STANDARD_VECTOR_SIZE);
        if self.aligned && group >= self.next_group {
            self.buffer = ColumnDataCollection::new(self.types.clone());
            self.next_group = group;
            self.first_chunk = group * (ROW_GROUP_ROWS / STANDARD_VECTOR_SIZE);
        }
        loop {
            let local = index - self.first_chunk;
            let chunks = self.buffer.chunk_count();
            let complete =
                local + 1 < chunks || (local < chunks && self.buffer.chunk(local).len() == STANDARD_VECTOR_SIZE);
            if complete || (self.next_group == self.source.group_count() && local < chunks) {
                return Ok(self.buffer.chunk(local).clone());
            }
            if self.next_group == self.source.group_count() {
                return Err(Error::new(ErrorKind::Internal, "a table scan ran past its rows"));
            }
            // what the chunks before this one held isn't needed any more, but a chunk that isn't
            // full goes on in the next group, even if a scan skips that one
            let full = chunks - usize::from(chunks > 0 && self.buffer.chunk(chunks - 1).len() < STANDARD_VECTOR_SIZE);
            let skipped = local.min(full);
            let mut kept = ColumnDataCollection::new(self.types.clone());
            for chunk in self.buffer.chunks().skip(skipped) {
                kept.append(chunk);
            }
            self.first_chunk += skipped;
            kept.append(&self.source.read_group(self.next_group, &self.columns)?);
            self.buffer = kept;
            self.next_group += 1;
        }
    }
}

/// What is durable of the rows of a table, for the next commit to log the difference. Records name
/// rows by the ids that they get when the log is replayed, which are the ids that they have here
/// unless the table had to go into the log again, as replaying that leaves no gaps.
#[derive(Debug, Default)]
pub struct TableLog {
    /// The rows that are durable.
    pub logged: Arc<RowIds>,
    /// The same rows, as a replay numbers them.
    pub replayed: Arc<RowIds>,
    /// Those that changed in place since, by id here, and which columns did.
    pub updated: IdSet,
    pub updated_columns: BTreeSet<usize>,
}

/// A table (`TableCatalogEntry`) and its rows.
#[derive(Debug)]
pub struct TableCatalogEntry {
    pub schema: String,
    pub name: String,
    /// The columns that are stored.
    pub columns: Vec<ColumnDefinition>,
    /// The columns that are not, in order of `position`.
    pub generated: Vec<GeneratedColumn>,
    /// Empty while the rows are still in `source`: go through `rows` and `write_storage`.
    storage: RwLock<ColumnDataCollection>,
    /// Where the rows are if they are in a database file and nothing needed all of them so far.
    source: RwLock<Option<Arc<dyn TableSource>>>,
    /// Where they still are too if they were only read from there, for what looks at the file.
    read_from: RwLock<Option<Arc<dyn TableSource>>>,
    /// Beyond the NOT NULLs, which are in `columns`.
    pub constraints: Vec<TableConstraint>,
    /// What the write-ahead log knows of the rows.
    pub log: Mutex<TableLog>,
    /// The tables with a foreign key to this one.
    pub referenced_by: Mutex<Vec<Weak<TableCatalogEntry>>>,
    /// The keys of each `TableConstraint::Unique`, in order, when somebody needed them since
    /// the rows last moved. Whoever rewrites `storage` must reset this.
    pub unique_keys: Mutex<Option<Vec<UniqueKeys>>>,
    pub indexes: RwLock<Vec<Arc<IndexCatalogEntry>>>,
    /// Of `columns`, when somebody needed them since the rows last changed.
    pub statistics: Mutex<TableStatistics>,
    /// The tree of each key that a scan used, as `index_trees` left it.
    pub index_trees: Mutex<HashMap<String, IndexTree>>,
    /// How often rows changed places in a way that a tree can't follow.
    rewrites: std::sync::atomic::AtomicU64,
    /// What this is a copy of, for a transaction.
    origin: Option<TableOrigin>,
    /// Which row is which. Rows added at the end are in here once somebody asks.
    row_ids: Mutex<Arc<RowIds>>,
    versions: Arc<Mutex<RowVersions>>,
}

/// The statistics of a table's columns, if they are up to date. Working them out takes a scan, and
/// `version` tells whether the rows changed in the meantime, as then they must not end up here.
#[derive(Debug, Default)]
pub struct TableStatistics {
    pub version: u64,
    pub columns: Option<Arc<Vec<ColumnStatistics>>>,
    /// The rows and each column's distinct values, for the join order.
    pub distinct: Option<Arc<TableDistinctCounts>>,
    /// Per column that a filter asked about, a zone per row group, as far as rows stayed the same.
    pub zones: std::collections::HashMap<usize, Arc<Vec<ColumnZone>>>,
    /// How many rows at the start `columns` covers. As in C++ rows that go leave it as it is, and
    /// it only says what values can be.
    pub covered: usize,
    /// Rows that changed in place since, which it doesn't cover, in no particular order.
    pub updated: Vec<usize>,
    /// Rows were written, and nobody said which yet. If nobody does that was all of them.
    pub unreported: bool,
}

impl TableStatistics {
    /// Nothing that was known holds any more.
    pub fn forget(&mut self) {
        self.columns = None;
        self.covered = 0;
        self.updated.clear();
        self.zones.clear();
        self.unreported = false;
    }

    /// Rows from this one on aren't what zones were made of.
    fn rows_changed_from(&mut self, position: usize) {
        self.distinct = None;
        let groups = position / crate::execution::zone_map::ROW_GROUP_SIZE;
        for zones in self.zones.values_mut() {
            if zones.len() > groups {
                Arc::make_mut(zones).truncate(groups);
            }
        }
    }

    /// `removed`, in order, are gone.
    fn rows_removed(&mut self, removed: &[usize]) {
        self.unreported = false;
        let Some(first) = removed.first() else { return };
        self.rows_changed_from(*first);
        self.covered -= removed.partition_point(|position| *position < self.covered);
        self.updated
            .retain_mut(|position| match removed.binary_search(position) {
                Ok(_) => false,
                Err(before) => {
                    *position -= before;
                    true
                }
            });
    }
}

/// What a row group holds of a column (the zone map of `SegmentStatistics`).
#[derive(Debug, Clone, Default)]
pub struct ColumnZone {
    /// the bounds of its values that are not NULL; `None` without any
    pub min: Option<duckrs_common::value::Value>,
    pub max: Option<duckrs_common::value::Value>,
    pub has_null: bool,
    /// the bounds could not be worked out
    pub unknown: bool,
}

/// How many rows a table has and how many distinct values each column holds, approximately.
#[derive(Debug)]
pub struct TableDistinctCounts {
    pub rows: u64,
    /// `None` for a column of a type that has no count
    pub columns: Vec<Option<u64>>,
}

/// What is known about the values of a column (`BaseStatistics`).
#[derive(Debug, Clone)]
pub struct ColumnStatistics {
    /// the bounds of its values that are not NULL, if known
    pub min: Option<duckrs_common::value::Value>,
    pub max: Option<duckrs_common::value::Value>,
    pub can_be_null: bool,
    /// whether there can be values that are not NULL
    pub can_be_valid: bool,
}

/// `DuckIndexEntry`. Scans read a table through one with `constraints::index_tree`, and the UNIQUE
/// ones are checked through `TableCatalogEntry::unique_keys`.
#[derive(Debug, Clone)]
pub struct IndexCatalogEntry {
    pub name: String,
    pub unique: bool,
    /// They read the columns of the table by index.
    pub expressions: Vec<BoundExpr>,
    /// How errors name the expressions.
    pub names: Vec<String>,
    /// The columns the expressions read.
    pub columns: Vec<usize>,
    /// The expressions as they were written, which is what a file holds.
    pub parsed: Vec<Expr>,
    /// `ART`, or what an extension added, which only a file can bring along.
    pub index_type: String,
    pub sql: String,
    /// `COMMENT ON INDEX`, which makes a new entry, for a transaction that rolls back to lose it.
    pub comment: Option<String>,
}

/// The tree of a key, for scans.
#[derive(Debug, Clone)]
pub struct IndexTree {
    /// `TableCatalogEntry::rewrite_count` when it was built.
    pub rewrites: u64,
    /// All rows with a lower id are in the tree, or were.
    pub frontier: u64,
    /// How many rows it has, of which `gone` have left the table since.
    pub rows: usize,
    pub gone: usize,
    pub tree: Arc<crate::execution::art::tree::Art>,
}

impl IndexTree {
    /// Rows go by here so that a tree needn't be built again for each DELETE.
    const MOST_GONE: usize = 4;

    /// Rows that it may have went: `false` if the tree has too many that are no more.
    fn rows_gone(&mut self, ids: &RowIds, removed: &[usize]) -> bool {
        self.gone += removed
            .iter()
            .filter(|position| ids.id(**position) < self.frontier)
            .count();
        self.gone * Self::MOST_GONE <= self.rows
    }
}

/// A key that has to be unique: of a PRIMARY KEY, a UNIQUE or a UNIQUE index.
#[derive(Debug, Clone)]
pub struct TableKey {
    pub columns: Vec<usize>,
    /// Of an index; a constraint's key is its columns.
    pub expressions: Option<Vec<BoundExpr>>,
    pub names: Vec<String>,
    pub primary_key: bool,
}

pub use crate::execution::art::tree::UniqueKeys;

/// A bound constraint of a table (`BoundConstraint`).
#[derive(Debug, Clone)]
pub enum TableConstraint {
    /// The expression reads the columns of the table by index.
    Check {
        expression: BoundExpr,
        parsed: Expr,
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
    pub referenced: Option<ReferencedTable>,
    pub referenced_columns: Vec<usize>,
}

/// The table of a foreign key, by name: ALTER replaces entries. A referenced table can't go away.
#[derive(Debug, Clone)]
pub struct ReferencedTable {
    pub schema: Weak<SchemaCatalogEntry>,
    pub name: String,
}

impl ReferencedTable {
    pub fn table(&self) -> Arc<TableCatalogEntry> {
        let schema = self.schema.upgrade().expect("a table's schema outlives it");
        match schema.get_entry(&self.name) {
            Some(TableOrView::Table(table)) => table,
            _ => unreachable!("a referenced table can't be dropped"),
        }
    }
}

impl TableCatalogEntry {
    pub fn new(
        schema: String,
        name: String,
        columns: Vec<ColumnDefinition>,
        rows: ColumnDataCollection,
        constraints: Vec<TableConstraint>,
    ) -> Self {
        Self {
            schema,
            name,
            columns,
            generated: Vec::new(),
            storage: RwLock::new(rows),
            source: RwLock::default(),
            read_from: RwLock::default(),
            log: Mutex::default(),
            constraints,
            referenced_by: Mutex::default(),
            unique_keys: Mutex::default(),
            indexes: RwLock::default(),
            statistics: Mutex::default(),
            index_trees: Mutex::default(),
            rewrites: Default::default(),
            origin: None,
            row_ids: Mutex::default(),
            versions: Arc::default(),
        }
    }

    /// The rows, to change them. A scan that starts after this sees the change, as it waits for
    /// the lock, and one that started before it has an old `version`.
    /// How many rows there are, which takes no reading.
    pub fn row_count(&self) -> usize {
        match &*self.source.read() {
            Some(source) => source.row_count(),
            None => self.storage.read().len(),
        }
    }

    /// Where the rows are, if they are still in a file.
    pub fn source(&self) -> Option<Arc<dyn TableSource>> {
        self.source.read().clone()
    }

    /// Leaves the rows in a file until something needs them all.
    /// What is in the file, with the rows being the same.
    pub fn stored(&self) -> Option<Arc<dyn TableSource>> {
        self.source().or_else(|| self.read_from.read().clone())
    }

    pub fn set_source(&self, source: Arc<dyn TableSource>) {
        self.set_row_ids(source.row_ids());
        *self.source.write() = Some(source);
    }

    /// One column of each chunk in turn, of rows in memory or in a file, which stay there.
    pub fn for_each_chunk(&self, column: usize, mut body: impl FnMut(usize, &Vector, usize)) -> Result<()> {
        if let Some(source) = self.source() {
            let ty = self.columns[column].ty.clone();
            let chunks = self.row_count().div_ceil(STANDARD_VECTOR_SIZE);
            let mut cursor = TableCursor::new(source, vec![column], vec![ty]);
            for index in 0..chunks {
                let chunk = cursor.chunk(index)?;
                body(index, chunk.column(0), chunk.len());
            }
            return Ok(());
        }
        for (index, chunk) in self.storage.read().chunks().enumerate() {
            body(index, chunk.column(column), chunk.len());
        }
        Ok(())
    }

    /// A checkpoint put the rows in a file: what is in memory of them goes.
    pub fn leave_in_file(&self, source: Arc<dyn TableSource>) {
        let types = self.types();
        let ids = source.row_ids();
        let old = self.row_ids(self.row_count());
        if ids.next() != old.next() || !ids.ranges().eq(old.ranges()) {
            self.rows_rewritten();
        }
        self.set_row_ids(ids);
        *self.write_loaded_storage() = ColumnDataCollection::new(types);
        *self.source.write() = Some(source);
        self.statistics.lock().forget();
    }

    /// Reads rows that are still in a file into memory.
    fn load_rows(&self) -> Result<()> {
        if self.source.read().is_none() {
            return Ok(());
        }
        let mut source = self.source.write();
        if let Some(rows) = source.as_ref() {
            *self.storage.write() = rows.read_all()?;
            *self.read_from.write() = source.take();
        }
        Ok(())
    }

    /// All rows, in memory. Scans don't need this: they read `source` as they go.
    pub fn rows(&self) -> Result<parking_lot::RwLockReadGuard<'_, ColumnDataCollection>> {
        self.load_rows()?;
        Ok(self.storage.read())
    }

    /// The ids of the rows, of which there are `count`.
    pub fn row_ids(&self, count: usize) -> Arc<RowIds> {
        let mut ids = self.row_ids.lock();
        if ids.len() < count {
            Arc::make_mut(&mut ids).fill(count);
        }
        ids.clone()
    }

    pub fn set_row_ids(&self, ids: RowIds) {
        *self.row_ids.lock() = Arc::new(ids);
    }

    /// Rows are somewhere else now, in a way that `rows_moved` doesn't say.
    pub fn rows_rewritten(&self) {
        self.rewrites.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Changes each time that rows go or move.
    pub fn rewrite_count(&self) -> u64 {
        self.rewrites.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// All that can have a tree, in the order that a scan tries them: the keys of constraints and
    /// then the indexes, as `TableIndexList` has them.
    pub fn scan_keys(&self) -> Vec<TableKey> {
        let mut keys: Vec<TableKey> = self
            .constraints
            .iter()
            .filter_map(|constraint| {
                let columns = match constraint {
                    TableConstraint::Unique { columns, .. } => columns,
                    TableConstraint::ForeignKey(foreign) => &foreign.columns,
                    TableConstraint::Check { .. } => return None,
                };
                Some(TableKey {
                    columns: columns.clone(),
                    expressions: None,
                    names: columns.iter().map(|c| self.columns[*c].name.clone()).collect(),
                    primary_key: false,
                })
            })
            .collect();
        keys.extend(self.indexes.read().iter().map(|index| TableKey {
            columns: index.columns.clone(),
            expressions: Some(index.expressions.clone()),
            names: index.names.clone(),
            primary_key: false,
        }));
        keys
    }

    /// Replaces rows that were in a file, which the caller has read.
    pub fn set_rows(&self, rows: ColumnDataCollection) {
        self.rows_rewritten();
        *self.source.write() = None;
        *self.storage.write() = rows;
        self.statistics.lock().forget();
    }

    /// Whoever writes to `storage` comes through here, so that nothing goes on using statistics of
    /// other rows.
    pub fn write_storage(&self) -> Result<parking_lot::RwLockWriteGuard<'_, ColumnDataCollection>> {
        self.load_rows()?;
        Ok(self.write_loaded_storage())
    }

    /// `write_storage` for what only adds rows at the end.
    pub fn append_storage(&self) -> Result<parking_lot::RwLockWriteGuard<'_, ColumnDataCollection>> {
        let storage = self.write_storage()?;
        self.rows_appended(storage.len());
        Ok(storage)
    }

    /// All rows are durable as they are, in a file or in a log that was replayed.
    pub fn reset_log(&self) {
        let ids = self.row_ids(self.row_count());
        *self.log.lock() = TableLog {
            logged: ids.clone(),
            replayed: ids,
            ..TableLog::default()
        };
    }

    /// Whether an UPDATE of `columns` is a DELETE and an INSERT, which gives the rows new ids and
    /// moves them to the end (`LogicalUpdate::BindExtraColumns`).
    pub fn update_moves_rows(&self, columns: &[usize]) -> bool {
        fn regular(ty: &LogicalType) -> bool {
            match ty.id() {
                LogicalTypeId::List
                | LogicalTypeId::Array
                | LogicalTypeId::Map
                | LogicalTypeId::Union
                | LogicalTypeId::Variant
                | LogicalTypeId::Geometry => false,
                LogicalTypeId::Struct => ty.struct_children().iter().all(|(_, ty)| regular(ty)),
                _ => true,
            }
        }
        let keyed = self.constraints.iter().any(|constraint| match constraint {
            TableConstraint::Check { .. } => false,
            TableConstraint::Unique { columns: keys, .. } => keys.iter().any(|c| columns.contains(c)),
            TableConstraint::ForeignKey(key) => key.columns.iter().any(|c| columns.contains(c)),
        });
        keyed
            || self
                .indexes
                .read()
                .iter()
                .any(|index| index.columns.iter().any(|c| columns.contains(c)))
            || columns.iter().any(|c| !regular(&self.columns[*c].ty))
    }

    fn write_loaded_storage(&self) -> parking_lot::RwLockWriteGuard<'_, ColumnDataCollection> {
        let storage = self.storage.write();
        *self.read_from.write() = None;
        let mut statistics = self.statistics.lock();
        statistics.version += 1;
        statistics.unreported = true;
        statistics.distinct = None;
        storage
    }

    /// Whoever calls `write_storage` says what became of the rows, for what is known of them to
    /// stay: the rows at these positions changed, and only their values.
    pub fn rows_updated_in_place(&self, positions: impl IntoIterator<Item = usize>) {
        let mut statistics = self.statistics.lock();
        statistics.unreported = false;
        let positions: Vec<usize> = positions.into_iter().collect();
        if let Some(first) = positions.iter().min() {
            statistics.rows_changed_from(*first);
        }
        // without any statistics they'd only pile up
        if statistics.columns.is_some() {
            statistics.updated.extend(positions);
        }
    }

    /// All that happened is that rows were added at the end, of which there were `first_new_row`.
    pub fn rows_appended(&self, first_new_row: usize) {
        let mut statistics = self.statistics.lock();
        statistics.unreported = false;
        statistics.rows_changed_from(first_new_row);
    }

    /// What `unique_keys` holds the keys of, in that order.
    /// The columns in the order a query sees them.
    pub fn logical_columns(&self) -> Vec<LogicalColumn<'_>> {
        let mut result: Vec<LogicalColumn<'_>> = self
            .columns
            .iter()
            .map(|c| LogicalColumn {
                name: &c.name,
                ty: &c.ty,
                definition: Some(c),
                generated: None,
            })
            .collect();
        for column in &self.generated {
            let position = column.position.min(result.len());
            result.insert(
                position,
                LogicalColumn {
                    name: &column.name,
                    ty: &column.ty,
                    definition: None,
                    generated: Some(&column.expr),
                },
            );
        }
        result
    }

    pub fn is_generated(&self, name: &str) -> bool {
        self.generated.iter().any(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn keys(&self) -> Vec<TableKey> {
        let mut keys: Vec<TableKey> = self
            .constraints
            .iter()
            .filter_map(|constraint| match constraint {
                TableConstraint::Unique { columns, primary_key } => Some(TableKey {
                    columns: columns.clone(),
                    expressions: None,
                    names: columns.iter().map(|c| self.columns[*c].name.clone()).collect(),
                    primary_key: *primary_key,
                }),
                _ => None,
            })
            .collect();
        keys.extend(
            self.indexes
                .read()
                .iter()
                .filter(|index| index.unique)
                .map(|index| TableKey {
                    columns: index.columns.clone(),
                    expressions: Some(index.expressions.clone()),
                    names: index.names.clone(),
                    primary_key: false,
                }),
        );
        keys
    }

    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.ty.clone()).collect()
    }

    /// Drops the rows from `rows` on, which a statement that failed added.
    pub fn truncate(&self, rows: usize) {
        // rows that are still in a file weren't added to
        if self.source.read().is_some() {
            return;
        }
        self.rows_rewritten();
        {
            let mut storage = self.write_loaded_storage();
            storage.truncate(rows);
            let mut statistics = self.statistics.lock();
            statistics.unreported = false;
            statistics.rows_changed_from(rows);
            statistics.covered = statistics.covered.min(rows);
            statistics.updated.retain(|position| *position < rows);
        }
        Arc::make_mut(&mut self.row_ids.lock()).truncate(rows);
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

impl ViewCatalogEntry {
    /// `ViewCatalogEntry::ToSQL`.
    pub fn to_sql(&self) -> String {
        let mut sql = String::from("CREATE VIEW ");
        if !self.schema.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            sql += &sql_identifier(&self.schema);
            sql.push('.');
        }
        sql += &sql_identifier(&self.name);
        if !self.aliases.is_empty() {
            let aliases: Vec<String> = self.aliases.iter().map(|a| sql_identifier(a)).collect();
            sql += &format!(" ({})", aliases.join(", "));
        }
        format!("{sql} AS {};", self.query)
    }
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
    /// How often it was used, by which logs tell the last state.
    pub usage_count: u64,
    /// the value `nextval` returns next
    pub counter: i64,
    pub last_value: Option<i64>,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    /// what it started from (`start_value`); a file keeps the counter as that
    pub start_value: i64,
}

/// A sequence (`SequenceCatalogEntry`). Its values are not transactional, as in DuckDB.
#[derive(Debug)]
pub struct SequenceCatalogEntry {
    pub name: String,
    data: Mutex<SequenceData>,
}

impl SequenceCatalogEntry {
    /// Its state, for a checkpoint.
    pub fn data(&self) -> SequenceData {
        self.data.lock().clone()
    }

    /// The state that a write-ahead log recorded (`ReplayValue`).
    pub fn replay_value(&self, usage_count: u64, counter: i64, last_value: Option<i64>) {
        let mut data = self.data.lock();
        data.usage_count = usage_count;
        data.counter = counter;
        data.last_value = last_value;
    }

    /// `DeriveLastValue`: storage versions before v2.0.0 only have the counter. The caller knows that
    /// the sequence was used.
    pub fn derive_last_value(&self) {
        let mut data = self.data.lock();
        if data.last_value.is_some() {
            return;
        }
        let Some(last_value) = data.counter.checked_sub(data.increment) else {
            return;
        };
        // a counter that wrapped around in a cycle does not tell
        if (data.min_value..=data.max_value).contains(&last_value) {
            data.last_value = Some(last_value);
        }
    }

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
        data.usage_count += 1;
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
            data.usage_count += 1;
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
    /// The schemas from the outermost down to this one (`GetSchemaPath`): only the name for a
    /// top-level schema
    pub path: Vec<String>,
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

/// What separates the schemas of a nested schema's path in its key: no name can have it.
pub const SCHEMA_PATH_SEPARATOR: char = '\u{1f}';

/// The key of the schema at `path`: a top-level schema's is its name.
pub fn schema_key<S: AsRef<str>>(path: &[S]) -> String {
    let parts: Vec<String> = path.iter().map(|p| key(p.as_ref())).collect();
    parts.join(&SCHEMA_PATH_SEPARATOR.to_string())
}

/// A schema's key as SQL writes the path: `s1.child`.
pub fn display_schema(schema: &str) -> String {
    schema.replace(SCHEMA_PATH_SEPARATOR, ".")
}

impl SchemaCatalogEntry {
    fn new(name: &str) -> Self {
        Self::at_path(vec![name.to_string()])
    }

    /// The schema at `path`, nested in the one before it if there is one.
    fn at_path(path: Vec<String>) -> Self {
        Self {
            name: path.last().cloned().unwrap_or_default(),
            path,
            tables: RwLock::new(BTreeMap::new()),
            sequences: RwLock::new(BTreeMap::new()),
            macros: [RwLock::new(BTreeMap::new()), RwLock::new(BTreeMap::new())],
            types: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn get_type(&self, name: &str) -> Option<LogicalType> {
        self.types.read().get(&key(name)).cloned()
    }

    /// How an entry names the schema it is in: a nested one by its path, which looks it up as
    /// its key does.
    pub fn ident(&self) -> String {
        self.path.join(&SCHEMA_PATH_SEPARATOR.to_string())
    }

    /// The key of the schema in its catalog.
    pub fn key(&self) -> String {
        schema_key(&self.path)
    }

    /// The key of the schema it is nested in (`GetParentSchema`), if it is.
    pub fn parent_key(&self) -> Option<String> {
        (self.path.len() > 1).then(|| schema_key(&self.path[..self.path.len() - 1]))
    }

    /// An empty schema at the same place.
    pub(crate) fn empty_like(&self) -> Self {
        Self::at_path(self.path.clone())
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

    /// The custom types, by name.
    pub fn type_entries(&self) -> Vec<(String, LogicalType)> {
        self.types.read().iter().map(|(n, t)| (n.clone(), t.clone())).collect()
    }

    pub fn sequence_entries(&self) -> Vec<Arc<SequenceCatalogEntry>> {
        self.sequences.read().values().cloned().collect()
    }

    pub fn macro_entries(&self, table_macro: bool) -> Vec<Arc<MacroCatalogEntry>> {
        self.macros[usize::from(table_macro)].read().values().cloned().collect()
    }

    /// Drops a type, returning whether it existed.
    pub fn drop_type(&self, name: &str) -> bool {
        self.types.write().remove(&key(name)).is_some()
    }

    /// All the tables and views, by name.
    pub fn tables_and_views(&self) -> Vec<TableOrView> {
        self.tables.read().values().cloned().collect()
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

    fn register_foreign_keys(tables: &BTreeMap<String, TableOrView>, table: &Arc<TableCatalogEntry>) {
        for constraint in &table.constraints {
            if let TableConstraint::ForeignKey(ForeignKey {
                referenced: Some(parent),
                ..
            }) = constraint
                && let Some(TableOrView::Table(parent)) = tables.get(&key(&parent.name))
            {
                parent.referenced_by.lock().push(Arc::downgrade(table));
            }
        }
    }

    /// Whether a table with a foreign key to `table` exists.
    pub fn is_referenced(&self, table: &TableCatalogEntry) -> bool {
        let tables = self.tables.read();
        table
            .referenced_by
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|child| matches!(tables.get(&key(&child.name)), Some(TableOrView::Table(t)) if Arc::ptr_eq(t, &child)))
    }

    /// The index with this name, and its table.
    pub fn get_index(&self, name: &str) -> Option<(Arc<TableCatalogEntry>, Arc<IndexCatalogEntry>)> {
        self.tables.read().values().find_map(|entry| {
            let TableOrView::Table(table) = entry else {
                return None;
            };
            let index = table
                .indexes
                .read()
                .iter()
                .find(|i| i.name.eq_ignore_ascii_case(name))
                .cloned()?;
            Some((table.clone(), index))
        })
    }

    /// Drops an index, returning whether it existed.
    pub fn drop_index(&self, name: &str) -> bool {
        let Some((table, index)) = self.get_index(name) else {
            return false;
        };
        table.indexes.write().retain(|other| !Arc::ptr_eq(other, &index));
        *table.unique_keys.lock() = None;
        true
    }

    /// `AlterEntry`: `table` takes the place of the entry called `old_name`.
    pub fn replace_table(&self, old_name: &str, table: Arc<TableCatalogEntry>) {
        let mut tables = self.tables.write();
        tables.remove(&key(old_name));
        Self::register_foreign_keys(&tables, &table);
        tables.insert(key(&table.name), TableOrView::Table(table));
    }

    pub fn rename_view(&self, view: &ViewCatalogEntry, new_name: &str) -> Result<()> {
        let mut tables = self.tables.write();
        tables.remove(&key(&view.name));
        let view = ViewCatalogEntry {
            schema: view.schema.clone(),
            name: new_name.to_string(),
            query: view.query.clone(),
            aliases: view.aliases.clone(),
        };
        tables.insert(key(new_name), TableOrView::View(Arc::new(view)));
        Ok(())
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
        generated: Vec<GeneratedColumn>,
        constraints: Vec<TableConstraint>,
        on_conflict: OnCreateConflict,
    ) -> Result<Option<Arc<TableCatalogEntry>>> {
        let mut tables = self.tables.write();
        if !Self::check_conflict(&tables, name, "Table", on_conflict)? {
            return Ok(None);
        }
        let types = columns.iter().map(|c| c.ty.clone()).collect();
        let mut table = TableCatalogEntry::new(
            self.ident(),
            name.to_string(),
            columns,
            ColumnDataCollection::new(types),
            constraints,
        );
        table.generated = generated;
        let table = Arc::new(table);
        Self::register_foreign_keys(&tables, &table);
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
                schema: self.ident(),
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

    /// `CatalogSet::SimilarEntry`: the score and the name of the closest table or view.
    fn similar_entry(&self, name: &str) -> Option<(f64, String)> {
        let mut best: Option<(f64, String)> = None;
        for entry in self.tables.read().values() {
            let score = similarity_rating(entry.name(), name);
            if score > best.as_ref().map_or(0.0, |(best, _)| *best) {
                best = Some((score, entry.name().to_string()));
            }
        }
        best
    }

    /// The tables whose defaults use the sequence `name`.
    fn tables_using_sequence(&self, name: &str) -> Vec<String> {
        let Some(current) = self.get_sequence(name) else {
            return Vec::new();
        };
        let mut users = Vec::new();
        for entry in self.tables.read().values() {
            let TableOrView::Table(table) = entry else {
                continue;
            };
            let mut uses = false;
            for column in &table.columns {
                let Some(default_value) = &column.default_value else {
                    continue;
                };
                default_value.clone().walk_mut(&mut |e| {
                    if let BoundExprKind::Sequence { sequence, .. } = &e.kind {
                        uses |= Arc::ptr_eq(sequence, &current);
                    }
                });
            }
            for constraint in &table.constraints {
                if let TableConstraint::Check { expression, .. } = constraint {
                    expression.clone().walk_mut(&mut |e| {
                        if let BoundExprKind::Sequence { sequence, .. } = &e.kind {
                            uses |= Arc::ptr_eq(sequence, &current);
                        }
                    });
                }
            }
            if uses {
                users.push(table.name.clone());
            }
        }
        users
    }

    /// An index whose expressions call the macro `name`, which keeps it from going
    /// (`DependencyManager`).
    pub fn index_using_macro(&self, name: &str) -> Option<String> {
        for entry in self.tables.read().values() {
            let TableOrView::Table(table) = entry else { continue };
            for index in table.indexes.read().iter() {
                let mut found = false;
                for expression in &index.parsed {
                    duckrs_parser::transformer::visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| {
                        if let duckrs_parser::ast::ExprKind::Function(function) = &e.kind {
                            found |= function.name.name.eq_ignore_ascii_case(name);
                        }
                    });
                }
                if found {
                    return Some(index.name.clone());
                }
            }
        }
        None
    }

    /// The tables with a column of the ENUM `name`, however deep in a nested type. A column does
    /// not remember the name of its type, so an ENUM is known by its type, and other user types
    /// are not tracked yet.
    fn tables_using_type(&self, name: &str) -> Vec<String> {
        fn mentions(ty: &LogicalType, target: &LogicalType, name: &str) -> bool {
            (target.id() == LogicalTypeId::Enum && ty == target)
                || ty.alias().is_some_and(|alias| alias.eq_ignore_ascii_case(name))
                || ty.child_type().is_some_and(|child| mentions(child, target, name))
                || ty
                    .struct_children()
                    .iter()
                    .any(|(_, child)| mentions(child, target, name))
        }
        let Some(target) = self.get_type(name) else {
            return Vec::new();
        };
        self.tables
            .read()
            .values()
            .filter_map(|entry| match entry {
                TableOrView::Table(table) if table.columns.iter().any(|c| mentions(&c.ty, &target, name)) => {
                    Some(table.name.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// `DROP SEQUENCE`: the tables that use it stop it, or go with it under CASCADE.
    pub fn drop_sequence_checked(&self, name: &str, cascade: bool) -> Result<bool> {
        if self.get_sequence(name).is_none() {
            return Ok(false);
        }
        let users = self.tables_using_sequence(name);
        if !users.is_empty() && !cascade {
            return Err(entry_in_use(name, "sequence", &users));
        }
        for table in &users {
            self.drop_entry(table, "Table")?;
        }
        Ok(self.drop_sequence(name))
    }

    /// `DROP TYPE`: the tables that use it stop it, or go with it under CASCADE.
    pub fn drop_type_checked(&self, name: &str, cascade: bool) -> Result<bool> {
        if self.get_type(name).is_none() {
            return Ok(false);
        }
        let users = self.tables_using_type(name);
        if !users.is_empty() && !cascade {
            return Err(entry_in_use(name, "type", &users));
        }
        for table in &users {
            self.drop_entry(table, "Table")?;
        }
        Ok(self.drop_type(name))
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
    database_list: RwLock<Option<std::sync::Weak<dyn DatabaseList>>>,
    versions: Mutex<CatalogVersions>,
    /// Whether this is the copy of a transaction, which has temporary objects of its own already.
    forked: bool,
}

thread_local! {
    /// The temporary objects of the connection that this thread is running a statement of.
    static SESSION_TEMP: std::cell::RefCell<Option<Arc<SchemaCatalogEntry>>> = const { std::cell::RefCell::new(None) };
}

/// For as long as it lives, statements on this thread have these temporary objects: as in C++ they
/// are the connection's, whereas the catalog is shared by all of them.
pub struct SessionTemp(Option<Arc<SchemaCatalogEntry>>);

impl SessionTemp {
    pub fn enter(temp: &Arc<SchemaCatalogEntry>) -> Self {
        Self(SESSION_TEMP.with_borrow_mut(|current| current.replace(temp.clone())))
    }

    pub fn new_schema() -> Arc<SchemaCatalogEntry> {
        Arc::new(SchemaCatalogEntry::new(TEMP_SCHEMA))
    }
}

impl Drop for SessionTemp {
    fn drop(&mut self) {
        SESSION_TEMP.with_borrow_mut(|current| *current = self.0.take());
    }
}

/// What a catalog holds at a moment, to go back to when a statement turns out to conflict.
#[derive(Debug)]
pub struct CatalogSnapshot {
    schemas: BTreeMap<String, Arc<SchemaCatalogEntry>>,
    entries: Vec<SchemaEntries>,
    /// sharing their buffers with the tables, whose writes copy what they change
    table_rows: Vec<TableRows>,
}

/// The rows and indexes of a table in a [`CatalogSnapshot`].
#[derive(Debug)]
struct TableRows {
    table: Arc<TableCatalogEntry>,
    rows: ColumnDataCollection,
    source: Option<Arc<dyn TableSource>>,
    indexes: Vec<Arc<IndexCatalogEntry>>,
    ids: Arc<RowIds>,
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
    /// Says who lists the databases that this one is among.
    pub fn set_database_list(&self, list: std::sync::Weak<dyn DatabaseList>) {
        *self.database_list.write() = Some(list);
    }

    /// All databases, or none if this one is on its own.
    pub fn databases(&self) -> Vec<DatabaseListing> {
        let list = self.database_list.read().as_ref().and_then(std::sync::Weak::upgrade);
        list.map_or_else(Vec::new, |list| list.databases())
    }

    /// `use_it` with the catalog of the database called `name`, if there is one.
    pub fn with_database(&self, name: &str, use_it: &mut dyn FnMut(&Catalog)) {
        let list = self.database_list.read().as_ref().and_then(std::sync::Weak::upgrade);
        if let Some(list) = list {
            list.with_catalog(name, use_it);
        }
    }

    /// A snapshot to [`Catalog::restore`] later.
    pub fn snapshot(&self) -> CatalogSnapshot {
        let schemas = self.schema_map();
        let mut entries = Vec::with_capacity(schemas.len());
        let mut table_rows = Vec::new();
        for schema in schemas.values() {
            let tables = schema.tables.read().clone();
            for entry in tables.values() {
                if let TableOrView::Table(table) = entry {
                    // a table that is still in a file comes back as it is, without reading it
                    let rows = table.storage.read().clone();
                    let source = table.source();
                    let indexes = table.indexes.read().clone();
                    table_rows.push(TableRows {
                        table: table.clone(),
                        rows,
                        source,
                        indexes,
                        ids: table.row_ids.lock().clone(),
                    });
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
        let mut schemas = snapshot.schemas;
        if self.session_temp().is_some()
            && let Some(shared) = self.schemas.read().get(TEMP_SCHEMA)
        {
            schemas.insert(key(TEMP_SCHEMA), shared.clone());
        }
        *self.schemas.write() = schemas;
        for saved in snapshot.entries {
            *saved.schema.tables.write() = saved.tables;
            *saved.schema.sequences.write() = saved.sequences;
            let [scalar, table] = saved.macros;
            *saved.schema.macros[0].write() = scalar;
            *saved.schema.macros[1].write() = table;
            *saved.schema.types.write() = saved.types;
        }
        for TableRows {
            table,
            rows,
            source,
            indexes,
            ids,
        } in snapshot.table_rows
        {
            table.rows_rewritten();
            *table.write_loaded_storage() = rows;
            *table.source.write() = source;
            *table.indexes.write() = indexes;
            *table.row_ids.lock() = ids;
            *table.unique_keys.lock() = None;
        }
    }

    /// A catalog with the default `main` schema.
    pub fn new(name: &str) -> Self {
        let catalog = Self {
            name: name.to_string(),
            schemas: RwLock::new(BTreeMap::new()),
            database_list: RwLock::new(None),
            versions: Mutex::default(),
            forked: false,
        };
        catalog
            .schemas
            .write()
            .insert(key(DEFAULT_SCHEMA), Arc::new(SchemaCatalogEntry::new(DEFAULT_SCHEMA)));
        catalog
            .schemas
            .write()
            .insert(key(TEMP_SCHEMA), Arc::new(SchemaCatalogEntry::new(TEMP_SCHEMA)));
        catalog
    }

    /// `DuckCatalog::CreateSchema`.
    pub fn create_schema(&self, name: &str, on_conflict: OnCreateConflict) -> Result<()> {
        self.create_schema_path(&[name.to_string()], on_conflict)
    }

    /// `DuckCatalog::CreateSchemaInternal`: a schema at `path`, nested in the schemas before it,
    /// which have to exist.
    pub fn create_schema_path(&self, path: &[String], on_conflict: OnCreateConflict) -> Result<()> {
        let mut schemas = self.schemas.write();
        let name = path.last().expect("a schema has a name");
        if path.len() > 1 && !schemas.contains_key(&schema_key(&path[..path.len() - 1])) {
            // `LookupSchemaPath`: the root that is no schema, else the first parent that is missing
            if !schemas.contains_key(&key(&path[0])) {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("\"{}\" is not a catalog or schema", path[0]),
                ));
            }
            let missing = (2..path.len())
                .find(|end| !schemas.contains_key(&schema_key(&path[..*end])))
                .expect("a parent is missing");
            return Err(missing_entry("Schema", &path[missing - 1]));
        }
        let schema_key = schema_key(path);
        if schemas.contains_key(&schema_key) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Schema with name \"{name}\" already exists!"),
                    ));
                }
                OnCreateConflict::Ignore => return Ok(()),
                OnCreateConflict::Replace => dependency_check(&schemas, &schemas[&schema_key])?,
            }
        }
        schemas.insert(schema_key, Arc::new(SchemaCatalogEntry::at_path(path.to_vec())));
        Ok(())
    }

    /// `DuckCatalog::DropSchema`: a schema with entries, or with schemas in it, only goes with
    /// CASCADE, which drops what is nested in it too.
    pub fn drop_schema(&self, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        let mut schemas = self.schemas.write();
        let Some(schema) = schemas.get(&key(name)) else {
            return if if_exists {
                Ok(())
            } else {
                Err(self.missing_schema(&schemas, name))
            };
        };
        if name.eq_ignore_ascii_case(DEFAULT_SCHEMA) {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Cannot drop entry {name} because it is an internal system entry"),
            ));
        }
        if !cascade {
            dependency_check(&schemas, schema)?;
        }
        let dropped = key(name);
        let nested = format!("{dropped}{SCHEMA_PATH_SEPARATOR}");
        schemas.retain(|schema, _| *schema != dropped && !schema.starts_with(&nested));
        Ok(())
    }

    /// `CatalogException::MissingEntry` of a schema: of a nested one, the first schema of its path
    /// that is missing.
    fn missing_schema(&self, schemas: &BTreeMap<String, Arc<SchemaCatalogEntry>>, schema: &str) -> Error {
        let path: Vec<&str> = schema.split(SCHEMA_PATH_SEPARATOR).collect();
        let missing = (1..=path.len())
            .find(|end| !schemas.contains_key(&schema_key(&path[..*end])))
            .unwrap_or(path.len());
        missing_entry("Schema", path[missing - 1])
    }

    /// The schemas nested right in `schema`.
    pub fn nested_schemas(&self, schema: &SchemaCatalogEntry) -> Vec<Arc<SchemaCatalogEntry>> {
        let parent = schema.key();
        self.schemas()
            .into_iter()
            .filter(|nested| nested.parent_key().as_deref() == Some(parent.as_str()))
            .collect()
    }

    /// All the schemas, by name.
    pub fn schemas(&self) -> Vec<Arc<SchemaCatalogEntry>> {
        self.schema_map().into_values().collect()
    }

    pub fn get_schema(&self, name: &str) -> Option<Arc<SchemaCatalogEntry>> {
        let name = key(name);
        if name == TEMP_SCHEMA
            && let Some(temp) = self.session_temp()
        {
            return Some(temp);
        }
        self.schemas.read().get(&name).cloned()
    }

    pub(crate) fn session_temp(&self) -> Option<Arc<SchemaCatalogEntry>> {
        if self.forked {
            return None;
        }
        SESSION_TEMP.with_borrow(Clone::clone)
    }

    /// The schemas as the connection on this thread sees them.
    fn schema_map(&self) -> BTreeMap<String, Arc<SchemaCatalogEntry>> {
        let mut schemas = self.schemas.read().clone();
        if let Some(temp) = self.session_temp() {
            schemas.insert(key(TEMP_SCHEMA), temp);
        }
        schemas
    }

    /// The schema an object qualified with `schema` (or not, with `None`) lives in.
    pub fn schema_for(&self, schema: Option<&str>) -> Result<Arc<SchemaCatalogEntry>> {
        let name = schema.unwrap_or(DEFAULT_SCHEMA);
        self.get_schema(name)
            .ok_or_else(|| self.missing_schema(&self.schemas.read(), name))
    }

    /// `schema_for`, of what is called `name`: without a schema that can be a temporary one.
    pub fn schema_with_entry(&self, schema: Option<&str>, name: &str) -> Result<Arc<SchemaCatalogEntry>> {
        let temporary = self
            .get_schema(TEMP_SCHEMA)
            .filter(|temporary| schema.is_none() && temporary.get_entry(name).is_some());
        temporary.map_or_else(|| self.schema_for(schema), Ok)
    }

    /// `Catalog::GetEntry`: looks the name up in `schema`, or without one along the search path:
    /// `search_first` (the schema of the view being bound), then the default schema.
    pub fn get_entry(&self, schema: Option<&str>, search_first: Option<&str>, name: &str) -> Result<TableOrView> {
        let entry = match schema {
            Some(schema) => {
                if self.get_schema(schema).is_none() {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!(
                            "Table with name \"{0}.{name}\" does not exist because schema \"{0}\" does not exist.",
                            display_schema(schema)
                        ),
                    ));
                }
                self.schema_for(Some(schema))?.get_entry(name)
            }
            None => [TEMP_SCHEMA]
                .iter()
                .chain(search_first.iter())
                .chain([&DEFAULT_SCHEMA])
                .find_map(|schema| self.get_schema(schema)?.get_entry(name)),
        };
        entry.ok_or_else(|| self.missing_table(schema, search_first, name))
    }

    /// `Catalog::CreateMissingEntryException`: suggests the most similar table, and one in a schema
    /// off the search path, qualified, when that one is much closer.
    fn missing_table(&self, schema: Option<&str>, search_first: Option<&str>, name: &str) -> Error {
        const UNSEEN_PENALTY: f64 = 0.2;
        let searched: Vec<&str> = match schema {
            Some(schema) => vec![schema],
            None => [TEMP_SCHEMA]
                .into_iter()
                .chain(search_first)
                .chain([DEFAULT_SCHEMA])
                .collect(),
        };
        let schemas = self.schemas();
        let mut seen: Option<(f64, String)> = None;
        let mut unseen: Vec<(f64, String)> = Vec::new();
        for entry in &schemas {
            let Some((score, similar)) = entry.similar_entry(name) else {
                continue;
            };
            if searched.iter().any(|s| s.eq_ignore_ascii_case(&entry.name)) {
                if seen.as_ref().is_none_or(|(best, _)| score > *best) {
                    seen = Some((score, similar));
                }
                continue;
            }
            let qualified = format!("{}.{similar}", entry.name);
            match unseen.first().map(|(best, _)| *best) {
                Some(best) if score < best => {}
                Some(best) if score > best => unseen = vec![(score, qualified)],
                _ => unseen.push((score, qualified)),
            }
        }
        let best_seen = seen.as_ref().map_or(0.0, |(score, _)| *score);
        let mut suggestions: Vec<String> = match unseen.first() {
            Some((score, _)) if *score == 1.0 || score - UNSEEN_PENALTY > best_seen => {
                unseen.into_iter().map(|(_, name)| name).collect()
            }
            _ => seen.into_iter().map(|(_, name)| name).collect(),
        };
        suggestions.sort();
        let message = format!("Table with name {name} does not exist!");
        Error::new(
            ErrorKind::Catalog,
            match suggestions.as_slice() {
                [] => message,
                [.., last] if suggestions.len() > 2 => {
                    let rest = suggestions[..suggestions.len() - 1].join(", ");
                    format!("{message}\nDid you mean \"{rest}, or {last}\"?")
                }
                _ => format!("{message}\nDid you mean \"{}\"?", suggestions.join(" or ")),
            },
        )
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
            None => [TEMP_SCHEMA]
                .iter()
                .chain(search_first.iter())
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
            None => [TEMP_SCHEMA]
                .iter()
                .chain(search_first.iter())
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
            None => [TEMP_SCHEMA]
                .iter()
                .chain(search_first.iter())
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

/// `DependencyManager::CollectDependents`: why `name` cannot be dropped.
fn entry_in_use(name: &str, kind: &str, tables: &[String]) -> Error {
    let mut message = format!("Cannot drop entry \"{name}\" because there are entries that depend on it.\n");
    for table in tables {
        message += &format!("table \"{table}\" depends on {kind} \"{name}\".\n");
    }
    message += "Use DROP...CASCADE to drop all dependents.";
    Error::new(ErrorKind::Dependency, message)
}

/// `DependencyManager::VerifyExistence`: a schema with tables cannot just go away.
fn dependency_check(schemas: &BTreeMap<String, Arc<SchemaCatalogEntry>>, schema: &SchemaCatalogEntry) -> Result<()> {
    let mut entries = schema.entries();
    let own = schema.key();
    for nested in schemas.values() {
        if nested.parent_key().as_deref() == Some(own.as_str()) {
            entries.push(("Schema".to_string(), nested.name.clone()));
        }
    }
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

/// `CatalogException::MissingEntry`.
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
            .create_table("Tbl", columns.clone(), Vec::new(), Vec::new(), OnCreateConflict::Error)
            .unwrap();
        assert_eq!(catalog.get_table(None, "TBL").unwrap().name, "Tbl");
        let err = schema
            .create_table("tbl", columns.clone(), Vec::new(), Vec::new(), OnCreateConflict::Error)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Catalog Error: Table with name \"tbl\" already exists!"
        );
        assert!(
            schema
                .create_table("tbl", columns, Vec::new(), Vec::new(), OnCreateConflict::Ignore)
                .unwrap()
                .is_none()
        );
        let err = catalog.get_table(None, "nope").unwrap_err();
        assert_eq!(err.to_string(), "Catalog Error: Table with name nope does not exist!");
    }
}
