//! The storage engine: DuckDB's `src/storage`, in step with its file format (see Phase 9 in
//! `docs/ARCHITECTURE.md`). For now it reads database files, into memory.

pub mod block_manager;
pub mod buffer_manager;
pub mod checkpoint;
pub mod checksum;
pub mod compression;
pub mod metadata;
pub mod serialization;
pub mod statistics;
pub mod table_data;

use crate::catalog::{Catalog, OnCreateConflict, SequenceData};
use crate::planner::binder::Binder;
use crate::planner::logical::LogicalOperatorKind;
use block_manager::SingleFileBlockManager;
use buffer_manager::BufferManager;
use checkpoint::{StoredEntry, StoredTable};
use duckrs_common::{Error, Result};
use std::path::Path;
use std::sync::Arc;

/// What of the file stays in memory while it loads. Rows are copied out, so this only bounds the
/// blocks read ahead of that.
const LOAD_BUFFER_MEMORY: usize = 256 << 20;

/// Whether there is a database in the file: a new database starts as a missing or empty file.
pub fn is_database_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// Whether a write-ahead log waits to be replayed: without it the file is out of date.
fn check_write_ahead_log(path: &Path) -> Result<()> {
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    if std::fs::metadata(wal).is_ok_and(|m| m.len() > 0) {
        return Err(Error::not_implemented(
            "replaying the write-ahead log of a database file is not yet ported to duckrs",
        ));
    }
    Ok(())
}

/// Puts what the last checkpoint of the file at `path` left into `catalog`. The file is opened
/// read-only: it stays as it is.
pub fn load_database(path: &Path, catalog: &Catalog) -> Result<()> {
    check_write_ahead_log(path)?;
    let blocks = Arc::new(SingleFileBlockManager::open(path, true)?);
    let buffer = BufferManager::new(blocks, LOAD_BUFFER_MEMORY);
    metadata::load_free_list(&buffer)?;
    let stored = checkpoint::read_catalog(&buffer)?;
    for entry in stored.entries {
        match entry {
            StoredEntry::Schema(name) => catalog.create_schema(&name, OnCreateConflict::Ignore)?,
            StoredEntry::Table(table) => load_table(&buffer, catalog, &table)?,
            StoredEntry::View {
                schema,
                name,
                query,
                aliases,
            } => {
                let schema = catalog.schema_for(Some(&schema))?;
                schema.create_view(&name, *query, aliases, OnCreateConflict::Error)?;
            }
            StoredEntry::Macro {
                schema,
                name,
                table_macro,
                macros,
            } => {
                let schema = catalog.schema_for(Some(&schema))?;
                schema.create_macro(&name, table_macro, macros, OnCreateConflict::Error)?;
            }
            StoredEntry::Sequence { schema, name, data } => {
                let data = SequenceData {
                    counter: data.counter,
                    last_value: data.last_value,
                    increment: data.increment,
                    min_value: data.min_value,
                    max_value: data.max_value,
                    cycle: data.cycle,
                };
                let schema = catalog.schema_for(Some(&schema))?;
                schema.create_sequence(&name, data, OnCreateConflict::Error)?;
            }
            StoredEntry::Type { schema, name, ty } => {
                let schema = catalog.schema_for(Some(&schema))?;
                schema.create_type(&name, ty, OnCreateConflict::Error)?;
            }
        }
    }
    Ok(())
}

/// Binds the definition of a table as `CREATE TABLE` does, for its defaults, generated columns and
/// constraints, and copies its rows in.
fn load_table(buffer: &BufferManager, catalog: &Catalog, table: &StoredTable) -> Result<()> {
    let plan = Binder::new(catalog).bind_stored_table(&table.definition, &table.schema)?;
    let LogicalOperatorKind::CreateTable {
        schema,
        name,
        columns,
        generated,
        constraints,
        on_conflict,
    } = plan.kind
    else {
        unreachable!("CREATE TABLE without a query binds to CreateTable");
    };
    let rows = table_data::read_table(buffer, table)?;
    let entry = schema
        .create_table(&name, columns, generated, constraints, on_conflict)?
        .expect("OnCreateConflict::Error fails or creates");
    *entry.write_storage() = rows;
    Ok(())
}
