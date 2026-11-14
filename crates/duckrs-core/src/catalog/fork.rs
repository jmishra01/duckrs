//! Ported in spirit from duckdb/src/transaction/{duck_transaction,duck_transaction_manager,
//! commit_state,cleanup_state}.cpp and storage/table/row_version_manager.cpp @ rust-port-baseline.
//!
//! C++ keeps versions in the rows and in the catalog sets, with an undo buffer to get back. Here
//! tables change by being rewritten, so a transaction gets a copy of the catalog of its own, a
//! [`Fork`], whose tables share their buffers with those of the database. Everything that binds or
//! runs a statement only has to be given that catalog: what it sees is what the fork saw when it
//! started, and rolling back is dropping it. A commit brings what changed to the database.
//!
//! What conflicts is found when it happens, as in C++, by the ids of the rows: what a transaction
//! changed is in [`RowVersions`], shared by the table and its forks. Without any fork that's empty.

use super::TEMP_SCHEMA;
use super::row_ids::{MAX_ROW_ID, RowIds};
use super::{
    Catalog, CatalogSnapshot, ForeignKey, IndexCatalogEntry, ReferencedTable, SchemaCatalogEntry, SchemaEntries,
    TableCatalogEntry, TableConstraint, TableOrView, key,
};
use crate::execution::constraints;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Commits and starts of transactions are in one order. Across databases too, which does no harm.
static CLOCK: AtomicU64 = AtomicU64::new(1);

/// The next moment. Each is unique, which also makes it an id for a transaction.
pub fn tick() -> u64 {
    CLOCK.fetch_add(1, Ordering::SeqCst)
}

/// What is known of the rows of a table while forks of it are around.
#[derive(Debug, Default)]
pub struct RowVersions {
    forks: usize,
    /// The rows that an open transaction changed, which one, and whether it was an update.
    owners: HashMap<u64, (u64, bool)>,
    /// When rows were last changed: a transaction that started before that can't change them.
    changed: HashMap<u64, u64>,
    /// Whether an update was committed that a transaction doesn't see. A checkpoint would fold it
    /// into the rows, where a delete is one that it leaves for later.
    unseen_update: bool,
}

/// What a table of a fork came from.
#[derive(Debug)]
pub struct TableOrigin {
    table: Arc<TableCatalogEntry>,
    /// Of the table of the database, to tell whether others changed it.
    version: u64,
    rows: usize,
    transaction: u64,
    start: u64,
    /// The rows that this transaction deleted or updated, by id.
    claimed: Mutex<HashSet<u64>>,
    updated_columns: Mutex<BTreeSet<usize>>,
    /// What the table had when the transaction started.
    indexes: Vec<Arc<IndexCatalogEntry>>,
}

fn conflict(update: bool) -> Error {
    Error::new(
        ErrorKind::Transaction,
        if update {
            "Conflict on update!"
        } else {
            "Conflict on tuple deletion!"
        },
    )
}

impl TableCatalogEntry {
    /// Whether rows of this table have to keep track of who they are.
    fn tracks_rows(&self) -> bool {
        self.origin.is_some() || self.versions.lock().forks > 0
    }

    /// Before rows change: fails if another transaction got to them first (`CheckForConflicts`).
    /// A statement outside of a transaction has nothing to hold on to, as it commits right away.
    pub fn claim_rows(&self, positions: impl IntoIterator<Item = usize>, count: usize, update: bool) -> Result<()> {
        if !self.tracks_rows() {
            return Ok(());
        }
        let ids = self.row_ids(count);
        let mut versions = self.versions.lock();
        let (transaction, start) = self.origin.as_ref().map_or((0, u64::MAX), |o| (o.transaction, o.start));
        let mut claimed = Vec::new();
        for position in positions {
            let id = ids.id(position);
            if id >= MAX_ROW_ID {
                continue;
            }
            let taken = versions.owners.get(&id).is_some_and(|(owner, _)| *owner != transaction);
            if taken || versions.changed.get(&id).is_some_and(|at| *at > start) {
                return Err(conflict(update));
            }
            claimed.push(id);
        }
        if let Some(origin) = &self.origin {
            for id in &claimed {
                versions.owners.insert(*id, (transaction, update));
            }
            origin.claimed.lock().extend(claimed);
        }
        Ok(())
    }

    /// Rows that changed in place. A transaction brings them along when it commits.
    pub fn rows_updated(&self, positions: impl IntoIterator<Item = usize>, columns: &[usize], count: usize) {
        if let Some(origin) = &self.origin {
            origin.updated_columns.lock().extend(columns);
            return;
        }
        let ids = self.row_ids(count);
        let changed: Vec<u64> = positions.into_iter().map(|position| ids.id(position)).collect();
        {
            let mut log = self.log.lock();
            changed.iter().for_each(|id| log.updated.insert(*id));
            log.updated_columns.extend(columns);
        }
        if self.tracks_rows() {
            let now = tick();
            let mut versions = self.versions.lock();
            versions.unseen_update = true;
            versions.changed.extend(changed.into_iter().map(|id| (id, now)));
        }
    }

    /// The rows at the positions in `removed`, of `count`, in order, went away. What is added at the
    /// end gets its id when something asks for it.
    pub fn rows_moved(&self, removed: &[usize], count: usize) {
        let ids = self.row_ids(count);
        let group_rows = self.row_group_size();
        self.statistics.lock().rows_removed(removed, group_rows);
        {
            let rewrites = self.rewrite_count();
            let mut trees = self.index_trees.lock();
            trees.retain(|_, tree| tree.rewrites == rewrites && tree.rows_gone(&ids, removed));
        }
        if self.origin.is_none() && self.tracks_rows() {
            let now = tick();
            let mut versions = self.versions.lock();
            versions
                .changed
                .extend(removed.iter().map(|position| (ids.id(*position), now)));
        }
        let mut left = (*ids).clone();
        left.remove(removed);
        *self.row_ids.lock() = Arc::new(left);
    }

    /// Whether a transaction other than the one of this table has updated rows it hasn't committed.
    pub fn has_outstanding_updates(&self) -> bool {
        let transaction = self.origin.as_ref().map_or(0, |origin| origin.transaction);
        let versions = self.versions.lock();
        // one that is committed is still in the rows' versions in C++ while older transactions run
        versions.unseen_update
            || versions
                .owners
                .values()
                .any(|(owner, update)| *owner != transaction && *update)
    }

    /// Where the rows start that the transaction of this table added, at most `first_new_row`.
    pub fn first_local_row(&self, first_new_row: usize, count: usize) -> usize {
        if self.origin.is_none() {
            return first_new_row;
        }
        first_new_row.min(self.row_ids(count).first_from(MAX_ROW_ID))
    }

    /// The table of the database, as long as this copy has the same rows in the same places.
    pub(crate) fn same_as_origin(&self, rows: usize) -> Option<&Arc<Self>> {
        let origin = self.origin.as_ref()?;
        let same = rows == origin.rows
            && self.statistics.lock().version == 0
            && origin.table.statistics.lock().version == origin.version;
        same.then_some(&origin.table)
    }

    /// The keys of the table this one is a fork of, when they are those of this table too.
    pub(crate) fn inherited_keys(&self, rows: usize) -> Option<Vec<super::UniqueKeys>> {
        let origin = self.origin.as_ref()?;
        if rows != origin.rows
            || !origin.claimed.lock().is_empty()
            || origin.table.statistics.lock().version != origin.version
            || self.indexes.read().len() != origin.table.indexes.read().len()
        {
            return None;
        }
        origin.table.unique_keys.lock().clone()
    }

    /// A copy for `schema`, whose rows are shared until either changes them.
    fn copy_for(&self, schema: &Arc<SchemaCatalogEntry>, origin: Option<TableOrigin>) -> Arc<Self> {
        let constraints = self
            .constraints
            .iter()
            .map(|constraint| match constraint {
                TableConstraint::ForeignKey(foreign) => TableConstraint::ForeignKey(ForeignKey {
                    referenced: foreign.referenced.as_ref().map(|parent| ReferencedTable {
                        schema: Arc::downgrade(schema),
                        name: parent.name.clone(),
                    }),
                    ..foreign.clone()
                }),
                other => other.clone(),
            })
            .collect();
        let mut table = Self::new(
            self.schema.clone(),
            self.name.clone(),
            self.columns.clone(),
            self.storage.read().clone(),
            constraints,
        );
        table.generated.clone_from(&self.generated);
        table.from_query.store(
            self.from_query.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        *table.source.write() = self.source();
        {
            // what is known of the rows stays known, other than how often they changed
            let theirs = self.statistics.lock();
            let mut own = table.statistics.lock();
            if !theirs.unreported {
                own.columns.clone_from(&theirs.columns);
                own.covered = theirs.covered;
                own.updated.clone_from(&theirs.updated);
                own.zones.clone_from(&theirs.zones);
            }
        }
        let ids = self.row_ids(self.row_count());
        *table.row_ids.lock() = Arc::new(match &origin {
            Some(_) => ids.for_transaction(),
            None => ids.committed(ids.next_committed()),
        });
        *table.indexes.write() = self.indexes.read().clone();
        if origin.is_some() {
            table.versions = self.versions.clone();
        }
        table.origin = origin;
        Arc::new(table)
    }
}

impl SchemaCatalogEntry {
    /// Who references what, after the tables changed.
    fn link_foreign_keys(&self) {
        let tables = self.tables.read();
        for entry in tables.values() {
            if let TableOrView::Table(table) = entry {
                table.referenced_by.lock().clear();
            }
        }
        for entry in tables.values() {
            if let TableOrView::Table(table) = entry {
                Self::register_foreign_keys(&tables, table);
            }
        }
    }
}

/// An entry of a catalog that differs from what it was.
#[derive(Debug)]
struct ChangedEntry {
    key: String,
    schema: String,
    name: String,
    created: bool,
}

/// Whether `new` is what `old` is, changed rows aside.
fn same_table_entry(old: &TableOrView, new: &TableOrView) -> bool {
    match (old, new) {
        (TableOrView::View(old), TableOrView::View(new)) => Arc::ptr_eq(old, new),
        (TableOrView::Table(old), TableOrView::Table(new)) => {
            Arc::ptr_eq(old, new) || new.origin.as_ref().is_some_and(|o| Arc::ptr_eq(&o.table, old))
        }
        _ => false,
    }
}

fn map_changes<V>(
    prefix: &str,
    schema: &str,
    old: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
    same: impl Fn(&V, &V) -> bool,
    found: &mut Vec<ChangedEntry>,
) {
    for (name, value) in new {
        if !old.get(name).is_some_and(|other| same(other, value)) {
            found.push(ChangedEntry {
                key: format!("{prefix}{name}"),
                schema: schema.to_string(),
                name: name.clone(),
                created: !old.contains_key(name),
            });
        }
    }
    for name in old.keys().filter(|name| !new.contains_key(*name)) {
        found.push(ChangedEntry {
            key: format!("{prefix}{name}"),
            schema: schema.to_string(),
            name: name.clone(),
            created: false,
        });
    }
}

fn changed_entries(base: &[SchemaEntries], catalog: &Catalog) -> Vec<ChangedEntry> {
    let mut found = Vec::new();
    let schemas = catalog.schemas.read();
    for saved in base {
        let name = saved.schema.key();
        let Some(schema) = schemas.get(&name) else {
            found.push(ChangedEntry {
                key: format!("schema {name}"),
                schema: name.clone(),
                name,
                created: false,
            });
            continue;
        };
        let tables = schema.tables.read();
        map_changes(
            &format!("{name}."),
            &name,
            &saved.tables,
            &tables,
            same_table_entry,
            &mut found,
        );
        for entry in tables.values() {
            if let TableOrView::Table(table) = entry
                && let Some(origin) = &table.origin
            {
                let same = |a: &Arc<IndexCatalogEntry>, b: &Arc<IndexCatalogEntry>| a.name == b.name;
                let (own, before) = (table.indexes.read(), &origin.indexes);
                for (list, other, created) in [(&*own, before, true), (before, &*own, false)] {
                    for index in list.iter().filter(|index| !other.iter().any(|o| same(o, index))) {
                        found.push(ChangedEntry {
                            key: format!("index {name}.{}", key(&index.name)),
                            schema: name.clone(),
                            name: index.name.clone(),
                            created,
                        });
                    }
                }
            }
        }
        let sequences = schema.sequences.read();
        map_changes(
            &format!("{name}."),
            &name,
            &saved.sequences,
            &sequences,
            Arc::ptr_eq,
            &mut found,
        );
        for (index, prefix) in ["macro", "table macro"].iter().enumerate() {
            let macros = schema.macros[index].read();
            let prefix = format!("{prefix} {name}.");
            map_changes(&prefix, &name, &saved.macros[index], &macros, Arc::ptr_eq, &mut found);
        }
        let types = schema.types.read();
        map_changes(
            &format!("type {name}."),
            &name,
            &saved.types,
            &types,
            |a, b| a == b,
            &mut found,
        );
        let triggers = schema.triggers.read();
        map_changes(
            &format!("trigger {name}."),
            &name,
            &saved.triggers,
            &triggers,
            Arc::ptr_eq,
            &mut found,
        );
    }
    for name in schemas.keys() {
        if !base.iter().any(|saved| saved.schema.key() == *name) {
            found.push(ChangedEntry {
                key: format!("schema {name}"),
                schema: name.clone(),
                name: name.clone(),
                created: true,
            });
        }
    }
    found
}

/// Who changed which entries of a catalog (`CatalogSet`'s versions, as far as conflicts go).
#[derive(Debug, Default)]
pub struct CatalogVersions {
    forks: usize,
    /// Those of them that wrote what keeps a checkpoint from running.
    writers: usize,
    owners: HashMap<String, u64>,
    changed: HashMap<String, u64>,
}

impl CatalogVersions {
    fn claim(&mut self, entries: &[ChangedEntry], transaction: u64, start: u64) -> Result<()> {
        for entry in entries {
            let taken = self.owners.get(&entry.key).is_some_and(|owner| *owner != transaction);
            if taken || self.changed.get(&entry.key).is_some_and(|at| *at > start) {
                let what = if entry.created { "create" } else { "alter" };
                return Err(Error::new(
                    ErrorKind::Transaction,
                    format!("Catalog write-write conflict on {what} with \"{}\"", entry.name),
                ));
            }
        }
        if transaction != 0 {
            for entry in entries {
                self.owners.insert(entry.key.clone(), transaction);
            }
        }
        Ok(())
    }
}

impl Catalog {
    /// Whether a transaction has changes that a checkpoint wouldn't have.
    pub fn has_writers(&self) -> bool {
        let tables = self.schemas().into_iter().flat_map(|schema| schema.tables_and_views());
        self.versions.lock().writers > 0
            || tables
                .into_iter()
                .any(|entry| matches!(entry, TableOrView::Table(table) if table.versions.lock().unseen_update))
    }

    /// Whether what changes here can conflict with a transaction.
    pub fn has_forks(&self) -> bool {
        self.versions.lock().forks > 0
    }

    /// For a statement outside of a transaction: what changed since `before` is free to change.
    pub fn claim_changes(&self, before: &CatalogSnapshot) -> Result<()> {
        let entries = changed_entries(&before.entries, self);
        let mut versions = self.versions.lock();
        versions.claim(&entries, 0, u64::MAX)?;
        let now = tick();
        for entry in entries {
            versions.changed.insert(entry.key, now);
        }
        Ok(())
    }

    /// What a transaction sees from now on. Whoever calls this keeps writers out.
    pub fn fork(self: &Arc<Self>) -> Fork {
        let start = tick();
        let transaction = start;
        let mut catalog = Catalog::new(&self.name);
        catalog.row_group_rows = self.row_group_rows.clone();
        catalog.forked = true;
        let catalog = Arc::new(catalog);
        *catalog.database_list.write() = self.database_list.read().clone();
        let schemas = self.schema_map();
        let mut forked = BTreeMap::new();
        let mut base = Vec::with_capacity(schemas.len());
        let mut tables = Vec::new();
        for (name, schema) in &schemas {
            let copy = Arc::new(schema.empty_like());
            let saved = schema.tables.read().clone();
            let mut copies = BTreeMap::new();
            for (table_name, entry) in &saved {
                copies.insert(
                    table_name.clone(),
                    match entry {
                        TableOrView::View(view) => TableOrView::View(view.clone()),
                        TableOrView::Table(table) => {
                            let copy = table.fork_for(&copy, transaction, start);
                            tables.push(copy.clone());
                            TableOrView::Table(copy)
                        }
                    },
                );
            }
            *copy.tables.write() = copies;
            let sequences = schema.sequences.read().clone();
            let macros = [schema.macros[0].read().clone(), schema.macros[1].read().clone()];
            let types = schema.types.read().clone();
            let triggers = schema.triggers.read().clone();
            copy.triggers.write().clone_from(&triggers);
            copy.sequences.write().clone_from(&sequences);
            copy.macros[0].write().clone_from(&macros[0]);
            copy.macros[1].write().clone_from(&macros[1]);
            copy.types.write().clone_from(&types);
            copy.link_foreign_keys();
            base.push(SchemaEntries {
                schema: schema.clone(),
                tables: saved,
                sequences,
                macros,
                types,
                triggers,
            });
            forked.insert(name.clone(), copy);
        }
        *catalog.schemas.write() = forked;
        self.versions.lock().forks += 1;
        Fork {
            catalog,
            database: self.clone(),
            base,
            tables,
            transaction,
            start,
            dirty: false,
            stops_checkpoints: false,
            held: Mutex::default(),
        }
    }
}

impl TableCatalogEntry {
    fn fork_for(self: &Arc<Self>, schema: &Arc<SchemaCatalogEntry>, transaction: u64, start: u64) -> Arc<Self> {
        let rows = self.row_count();
        self.versions.lock().forks += 1;
        let origin = TableOrigin {
            table: self.clone(),
            version: self.statistics.lock().version,
            rows,
            transaction,
            start,
            claimed: Mutex::default(),
            updated_columns: Mutex::default(),
            indexes: self.indexes.read().clone(),
        };
        self.copy_for(schema, Some(origin))
    }
}

/// What a commit does to a table of the database.
enum TableChange {
    /// Rows at the end, by themselves or ahead of keys that are known.
    Append(Vec<DataChunk>),
    Replace(ColumnDataCollection, Arc<RowIds>),
}

struct PreparedTable {
    fork: Arc<TableCatalogEntry>,
    change: Option<TableChange>,
}

/// A transaction's view of a database.
#[derive(Debug)]
pub struct Fork {
    pub catalog: Arc<Catalog>,
    database: Arc<Catalog>,
    base: Vec<SchemaEntries>,
    /// All tables that it started with.
    tables: Vec<Arc<TableCatalogEntry>>,
    transaction: u64,
    start: u64,
    /// Whether a statement that writes ran here.
    pub dirty: bool,
    /// Whether one of them was more than an INSERT or a DELETE.
    stops_checkpoints: bool,
    /// The entries it changed.
    held: Mutex<HashSet<String>>,
}

impl Fork {
    /// A statement that writes runs here. Rows added and deleted go along with a checkpoint, unlike
    /// what else can be written (`DuckTransaction::ModifyDatabase`).
    pub fn mark_dirty(&mut self, stops_checkpoints: bool) {
        self.dirty = true;
        if stops_checkpoints && !std::mem::replace(&mut self.stops_checkpoints, true) {
            self.database.versions.lock().writers += 1;
        }
    }

    pub fn is_of(&self, database: &Arc<Catalog>) -> bool {
        Arc::ptr_eq(&self.database, database)
    }

    /// After a statement that can change the catalog: fails if another transaction got there first.
    pub fn claim_changes(&self) -> Result<()> {
        let entries = changed_entries(&self.base, &self.catalog);
        self.database
            .versions
            .lock()
            .claim(&entries, self.transaction, self.start)?;
        self.held.lock().extend(entries.into_iter().map(|entry| entry.key));
        Ok(())
    }

    /// The change to `table`, whose rows were `origin.table`'s, if it has one.
    fn prepare_table(table: &Arc<TableCatalogEntry>, origin: &TableOrigin) -> Result<Option<TableChange>> {
        let forked_version = table.statistics.lock().version;
        let same_indexes = {
            let (own, before) = (table.indexes.read(), &origin.indexes);
            own.len() == before.len() && own.iter().zip(before.iter()).all(|(a, b)| Arc::ptr_eq(a, b))
        };
        if forked_version == 0 && same_indexes {
            return Ok(None);
        }
        let theirs_changed = origin.table.statistics.lock().version != origin.version;
        let claimed = origin.claimed.lock();
        if claimed.is_empty() {
            let rows = table.rows()?;
            let mut added = Vec::new();
            let mut row = 0;
            for chunk in rows.chunks() {
                let count = chunk.len();
                let skip = origin.rows.saturating_sub(row);
                row += count;
                if skip >= count {
                    continue;
                }
                let mut part = chunk.clone();
                if skip > 0 {
                    let kept = (skip as u32..count as u32).collect();
                    part.slice(&duckrs_common::vector::SelectionVector::from_vec(kept), count - skip);
                }
                added.push(part);
            }
            if theirs_changed {
                check_appended(table, origin, &added)?;
            }
            return Ok(Some(TableChange::Append(added)));
        }
        let rows = table.rows()?.clone();
        let ids = table.row_ids(rows.len());
        if !theirs_changed {
            return Ok(Some(TableChange::Replace(rows, ids)));
        }
        let merged = merge(table, origin, &claimed, &rows, &ids)?;
        constraints::check_all_keys(table, &merged.0).map_err(failed_commit)?;
        Ok(Some(TableChange::Replace(merged.0, Arc::new(merged.1))))
    }

    /// `DuckTransactionManager::CommitTransaction`. Whoever calls this keeps everyone else out.
    pub fn commit(&self) -> Result<()> {
        let entries = changed_entries(&self.base, &self.catalog);
        let changed: HashSet<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
        self.check_drops(&entries)?;
        // first what can fail, for a commit to do all or nothing
        let mut prepared = Vec::new();
        for table in &self.tables {
            let Some(origin) = &table.origin else { continue };
            let schema = key(&table.schema);
            let entry_key = format!("{schema}.{}", key(&table.name));
            if changed.contains(entry_key.as_str()) {
                continue;
            }
            let change = Self::prepare_table(table, origin)?;
            if change.is_none() {
                continue;
            }
            let live = self.database.get_schema(&schema);
            let current = live.and_then(|schema| schema.tables.read().get(&key(&table.name)).cloned());
            match current {
                Some(TableOrView::Table(current)) if Arc::ptr_eq(&current, &origin.table) => {}
                // ALTER gives a new entry, as does dropping a table and creating it again
                Some(TableOrView::Table(current)) if current.columns.len() != origin.table.columns.len() => {
                    return Err(gone_table(&table.name, "altered"));
                }
                _ => return Err(gone_table(&table.name, "dropped")),
            }
            prepared.push(PreparedTable {
                fork: table.clone(),
                change,
            });
        }
        let now = tick();
        self.apply_entries(&entries, now);
        for PreparedTable { fork, change } in prepared {
            let origin = fork.origin.as_ref().expect("only tables of the database get here");
            origin.apply(&fork, change.expect("only changes get here"), now)?;
        }
        Ok(())
    }

    /// `CleanupDependencies`: a schema that this transaction dropped got an entry from another one.
    fn check_drops(&self, changes: &[ChangedEntry]) -> Result<()> {
        let own = self.catalog.schemas.read();
        let live = self.database.schema_map();
        for saved in &self.base {
            let name = saved.schema.key();
            if own.contains_key(&name) {
                if !live.contains_key(&name) && changes.iter().any(|entry| entry.schema == name) {
                    return Err(Error::new(
                        ErrorKind::Transaction,
                        format!(
                            "Failed to commit: Schema \"{}\" was dropped by another transaction",
                            saved.schema.name
                        ),
                    ));
                }
                continue;
            }
            let Some(schema) = live.get(&name) else { continue };
            if schema
                .tables
                .read()
                .keys()
                .any(|table| !saved.tables.contains_key(table))
            {
                return Err(Error::new(
                    ErrorKind::Transaction,
                    format!(
                        "Failed to commit: Could not commit DROP of \"{}\" because a dependency was created after \
                         the transaction started",
                        saved.schema.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// What was created, dropped or altered goes to the database.
    fn apply_entries(&self, entries: &[ChangedEntry], now: u64) {
        if entries.is_empty() {
            return;
        }
        let own = self.catalog.schemas.read();
        let mut schemas = self.database.schemas.write();
        for saved in &self.base {
            let name = saved.schema.key();
            if !own.contains_key(&name) {
                schemas.remove(&name);
            }
        }
        for (name, schema) in own.iter() {
            let saved = self.base.iter().find(|saved| saved.schema.key() == *name);
            // one that another transaction dropped has nothing of this one: `check_drops`
            if saved.is_some() && !schemas.contains_key(name) {
                continue;
            }
            let live = match self.database.session_temp().filter(|_| name == TEMP_SCHEMA) {
                Some(temp) => temp,
                None => schemas
                    .entry(name.clone())
                    .or_insert_with(|| Arc::new(schema.empty_like()))
                    .clone(),
            };
            {
                let new = schema.tables.read();
                let mut tables = live.tables.write();
                let old = saved.map(|saved| &saved.tables);
                tables.retain(|name, _| new.contains_key(name) || !old.is_some_and(|old| old.contains_key(name)));
                for (table_name, entry) in new.iter() {
                    if old
                        .and_then(|old| old.get(table_name))
                        .is_some_and(|old| same_table_entry(old, entry))
                    {
                        continue;
                    }
                    let entry = match entry {
                        TableOrView::View(view) => TableOrView::View(view.clone()),
                        TableOrView::Table(table) => TableOrView::Table(table.copy_for(&live, None)),
                    };
                    tables.insert(table_name.clone(), entry);
                }
            }
            copy_changes(
                &schema.sequences,
                &live.sequences,
                saved.map(|saved| &saved.sequences),
                Arc::ptr_eq,
            );
            for index in [0, 1] {
                copy_changes(
                    &schema.macros[index],
                    &live.macros[index],
                    saved.map(|saved| &saved.macros[index]),
                    Arc::ptr_eq,
                );
            }
            copy_changes(&schema.types, &live.types, saved.map(|saved| &saved.types), |a, b| {
                a == b
            });
            copy_changes(
                &schema.triggers,
                &live.triggers,
                saved.map(|saved| &saved.triggers),
                Arc::ptr_eq,
            );
            live.link_foreign_keys();
        }
        let mut versions = self.database.versions.lock();
        for entry in entries {
            versions.changed.insert(entry.key.clone(), now);
        }
    }
}

/// `to` gets what `from` has that `old` didn't, and loses what `from` no longer has.
fn copy_changes<V: Clone>(
    from: &RwLock<BTreeMap<String, V>>,
    to: &RwLock<BTreeMap<String, V>>,
    old: Option<&BTreeMap<String, V>>,
    same: impl Fn(&V, &V) -> bool,
) {
    let from = from.read();
    let mut to = to.write();
    if let Some(old) = old {
        for name in old.keys().filter(|name| !from.contains_key(*name)) {
            to.remove(name);
        }
    }
    for (name, value) in from.iter() {
        if !old.and_then(|old| old.get(name)).is_some_and(|old| same(old, value)) {
            to.insert(name.clone(), value.clone());
        }
    }
}

/// `CommitState`.
fn gone_table(table: &str, how: &str) -> Error {
    Error::new(
        ErrorKind::Transaction,
        format!(
            "Failed to commit: Attempting to modify table \"{table}\" but another transaction has {how} this table"
        ),
    )
}

fn failed_commit(error: Error) -> Error {
    if error.kind == ErrorKind::Constraint {
        Error::new(ErrorKind::Transaction, format!("Failed to commit: {}", error.message))
    } else {
        error
    }
}

/// Rows added to a table that others added to as well: the keys only held among the older rows.
fn check_appended(table: &Arc<TableCatalogEntry>, origin: &TableOrigin, added: &[DataChunk]) -> Result<()> {
    if table.keys().is_empty() {
        return Ok(());
    }
    let mut rows = origin.table.rows()?.clone();
    for chunk in added {
        rows.append(chunk);
    }
    constraints::check_all_keys(table, &rows).map_err(failed_commit)
}

/// The rows of the database as they are now, with what this transaction did to them, and their ids.
fn merge(
    table: &TableCatalogEntry,
    origin: &TableOrigin,
    claimed: &HashSet<u64>,
    rows: &ColumnDataCollection,
    ids: &RowIds,
) -> Result<(ColumnDataCollection, RowIds)> {
    let mut own: HashMap<u64, usize> = HashMap::new();
    let mut local = Vec::new();
    for position in 0..rows.len() {
        let id = ids.id(position);
        if id >= MAX_ROW_ID {
            local.push(position);
        } else if claimed.contains(&id) {
            own.insert(id, position);
        }
    }
    let value_row = |position: usize| -> Vec<Value> {
        let chunk = rows.chunk(position / STANDARD_VECTOR_SIZE);
        let row = position % STANDARD_VECTOR_SIZE;
        (0..chunk.column_count())
            .map(|c| chunk.column(c).get_value(row))
            .collect()
    };
    let current = origin.table.rows()?;
    let list = origin.table.row_ids(current.len());
    let types: Vec<LogicalType> = table.types();
    let mut merged = ColumnDataCollection::new(types.clone());
    let mut merged_ids = Vec::with_capacity(current.len() + local.len());
    for (index, chunk) in current.chunks().enumerate() {
        let first = index * STANDARD_VECTOR_SIZE;
        let chunk_ids: Vec<u64> = (0..chunk.len()).map(|row| list.id(first + row)).collect();
        if !chunk_ids.iter().any(|id| claimed.contains(id)) {
            merged.append(chunk);
            merged_ids.extend_from_slice(&chunk_ids);
            continue;
        }
        let mut output = DataChunk::new(&types);
        for (row, &id) in chunk_ids.iter().enumerate() {
            if !claimed.contains(&id) {
                let values: Vec<Value> = (0..types.len()).map(|c| chunk.column(c).get_value(row)).collect();
                output.append_row(&values)?;
                merged_ids.push(id);
            } else if let Some(position) = own.get(&id) {
                output.append_row(&value_row(*position))?;
                merged_ids.push(id);
            }
        }
        merged.append(&output);
    }
    for (index, position) in local.into_iter().enumerate() {
        let mut single = DataChunk::new(&types);
        single.append_row(&value_row(position))?;
        merged.append(&single);
        merged_ids.push(MAX_ROW_ID + index as u64);
    }
    let next = list.next();
    Ok((merged, RowIds::of_list(&merged_ids, next)))
}

impl TableOrigin {
    /// Installs what was prepared. All that can fail here is reading what was read before.
    fn apply(&self, fork: &TableCatalogEntry, change: TableChange, now: u64) -> Result<()> {
        let table = &self.table;
        let claimed = self.claimed.lock();
        let same_as_fork = table.statistics.lock().version == self.version;
        match change {
            TableChange::Append(chunks) => {
                let mut storage = table.append_storage()?;
                for chunk in &chunks {
                    storage.append(chunk);
                }
            }
            TableChange::Replace(rows, ids) => {
                table.rows_rewritten();
                let next = table.row_ids(table.row_count()).next();
                // who reads the rows mustn't get ids of others
                let mut storage = table.write_storage()?;
                *storage = rows;
                table.set_row_ids(ids.committed(next));
            }
        }
        // others can have changed them too: only what this transaction did goes
        {
            let own = fork.indexes.read();
            let mut indexes = table.indexes.write();
            indexes.retain(|index| {
                own.iter().any(|o| Arc::ptr_eq(o, index)) || !self.indexes.iter().any(|b| Arc::ptr_eq(b, index))
            });
            for index in own
                .iter()
                .filter(|index| !self.indexes.iter().any(|b| Arc::ptr_eq(b, index)))
            {
                indexes.push(index.clone());
            }
        }
        *table.unique_keys.lock() = if same_as_fork {
            fork.unique_keys.lock().take()
        } else {
            None
        };
        // what it updated is still there, and what it deleted isn't
        {
            let ids = table.row_ids(table.row_count());
            let mut log = table.log.lock();
            for id in claimed.iter().filter(|id| ids.position(**id).is_some()) {
                log.updated.insert(*id);
            }
            log.updated_columns.extend(self.updated_columns.lock().iter());
        }
        let mut versions = table.versions.lock();
        // deletes a checkpoint leaves for later: what isn't there any more was one of those
        let updated = claimed
            .iter()
            .any(|id| versions.owners.get(id).is_some_and(|(_, update)| *update));
        versions.unseen_update |= versions.forks > 1 && updated;
        versions.changed.extend(claimed.iter().map(|id| (*id, now)));
        Ok(())
    }
}

impl Drop for Fork {
    /// `RemoveTransaction`: what was held goes, and without forks there are no versions to keep.
    fn drop(&mut self) {
        for table in &self.tables {
            let Some(origin) = &table.origin else { continue };
            let mut versions = origin.table.versions.lock();
            for id in origin.claimed.lock().iter() {
                versions.owners.remove(id);
            }
            versions.forks -= 1;
            if versions.forks == 0 {
                versions.changed.clear();
                versions.unseen_update = false;
            }
        }
        let mut versions = self.database.versions.lock();
        for entry in self.held.lock().iter() {
            versions.owners.remove(entry);
        }
        versions.forks -= 1;
        versions.writers -= usize::from(self.stops_checkpoints);
        if versions.forks == 0 {
            versions.changed.clear();
        }
    }
}
