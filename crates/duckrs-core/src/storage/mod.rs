//! The storage engine: DuckDB's `src/storage`, in step with its file format (see Phase 9 in
//! `docs/ARCHITECTURE.md`). For now it reads database files, into memory.

pub mod block_manager;
pub mod buffer_manager;
pub mod checkpoint;
pub mod checksum;
pub mod compression;
pub mod metadata;
pub mod statistics;
pub mod table_data;

use crate::catalog::{Catalog, ColumnDefinition, OnCreateConflict};
use block_manager::SingleFileBlockManager;
use buffer_manager::BufferManager;
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
    for schema in &stored.schemas {
        catalog.create_schema(schema, OnCreateConflict::Ignore)?;
    }
    for table in &stored.tables {
        let schema = catalog.schema_for(Some(&table.schema))?;
        let columns = table
            .columns
            .iter()
            .map(|column| {
                let mut definition = ColumnDefinition::new(column.name.clone(), column.ty.clone());
                definition.not_null = column.not_null;
                definition
            })
            .collect();
        let rows = table_data::read_table(&buffer, table)?;
        let entry = schema
            .create_table(&table.name, columns, Vec::new(), Vec::new(), OnCreateConflict::Error)?
            .expect("OnCreateConflict::Error fails or creates");
        *entry.write_storage() = rows;
    }
    Ok(())
}
