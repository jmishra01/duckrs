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
mod wal;
pub mod writer;

use crate::catalog::Catalog;
use block_manager::SingleFileBlockManager;
use buffer_manager::BufferManager;
use duckrs_common::{Error, ErrorKind, Result};
use metadata::MetaBlockPointer;
use std::path::Path;
use std::sync::Arc;

/// The blocks of a file that stay in memory. Rows stay in the file, and a scan decodes one row group
/// at a time, so this is what bounds the memory that a large file takes.
const BUFFER_MEMORY: usize = 256 << 20;

/// Whether there is a database in the file: a new database starts as a missing or empty file.
pub fn is_database_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// Puts what the last checkpoint of the file at `path` left into `catalog`, with what its write-ahead
/// log committed since. The tables stay in the file, which is what the result reads them through.
pub fn load_database(path: &Path, catalog: &Catalog, read_only: bool) -> Result<Arc<BufferManager>> {
    let blocks = Arc::new(SingleFileBlockManager::open(path, read_only)?);
    let buffer = Arc::new(BufferManager::new(blocks, BUFFER_MEMORY));
    metadata::load_free_list(&buffer)?;
    let records = wal::read_records(path, &buffer)?;
    let stored = checkpoint::read_catalog(&buffer)?;
    let mut replay = wal::Replay::new(buffer.clone(), catalog);
    for entry in stored.entries {
        replay.add_entry(entry)?;
    }
    replay.replay(records)?;
    Ok(buffer)
}

/// A new database file, for `load_database`.
pub fn create_database(path: &Path) -> Result<()> {
    if std::fs::metadata(path).is_ok() {
        std::fs::remove_file(path).map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", path.display())))?;
    }
    SingleFileBlockManager::create(path, block_manager::DEFAULT_BLOCK_ALLOC_SIZE).map(drop)
}

/// Whether checkpoints can go into this file: they write v2.0.0, which an older file isn't.
pub fn takes_checkpoints(buffer: &BufferManager) -> bool {
    let header = buffer.block_manager().header();
    !checkpoint::is_before_v2(&header) || !MetaBlockPointer::new(header.meta_block, 0).is_valid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{TableCatalogEntry, TableConstraint};
    use crate::client::{ClientContext, DatabaseInstance};
    use crate::execution::constraints::current_keys;

    const FILES: [&str; 3] = ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"];

    fn load(file: &str) -> Catalog {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../duckrs/tests/fixtures")
            .join(file);
        let catalog = Catalog::new("test");
        load_database(&path, &catalog, true).unwrap();
        catalog
    }

    fn keys(table: &TableCatalogEntry) -> Vec<(Vec<String>, bool)> {
        table
            .keys()
            .into_iter()
            .map(|key| (key.names, key.primary_key))
            .collect()
    }

    fn names(names: &[&str], primary_key: bool) -> (Vec<String>, bool) {
        (names.iter().map(ToString::to_string).collect(), primary_key)
    }

    #[test]
    fn primary_keys_and_unique_constraints_come_along() {
        for file in FILES {
            let catalog = load(file);
            let parent = catalog.get_table(None, "parent").unwrap();
            assert_eq!(
                keys(&parent),
                [names(&["id"], true), names(&["code"], false), names(&["a", "b"], false)],
                "{file}"
            );
            let compound = catalog.get_table(None, "compound").unwrap();
            assert_eq!(keys(&compound), [names(&["x", "y"], true)], "{file}");
            assert!(parent.columns[0].not_null, "{file}");
            // the keys come from the rows, as the file doesn't give them
            let rows = parent.rows().unwrap();
            let keys = current_keys(&parent, &rows).unwrap();
            let sizes: Vec<usize> = keys.as_ref().unwrap().iter().map(|k| k.len()).collect();
            assert_eq!(sizes, [2, 2, 2], "{file}");
        }
    }

    #[test]
    fn foreign_keys_come_along() {
        for file in FILES {
            let catalog = load(file);
            let child = catalog.get_table(None, "child").unwrap();
            let foreign: Vec<_> = child
                .constraints
                .iter()
                .filter_map(|constraint| match constraint {
                    TableConstraint::ForeignKey(key) => Some(key),
                    _ => None,
                })
                .collect();
            assert_eq!(foreign.len(), 2, "{file}");
            let parent = foreign[0].referenced.as_ref().expect("another table");
            assert_eq!(parent.name, "parent", "{file}");
            assert!(
                foreign[1].referenced.is_none(),
                "{file}: a table that references itself"
            );
            let referenced = catalog.get_table(None, "parent").unwrap();
            assert_eq!(referenced.referenced_by.lock().len(), 1, "{file}");
        }
    }

    /// A query reads what it needs of a file: all of this runs without the rows in memory.
    #[test]
    fn tables_stay_in_the_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../duckrs/tests/fixtures/scan_v122880.db");
        let db = DatabaseInstance::open_file(&path, true).unwrap();
        let mut context = ClientContext::new(db.clone());
        let in_file = |table: &str| db.catalog.get_table(None, table).unwrap().source().is_some();
        for sql in [
            "SELECT count(*) FROM big",
            "SELECT i, s FROM big WHERE i = 7",
            "SELECT tiny, sum(i) FROM big GROUP BY tiny",
            "SELECT count(*) FROM big a JOIN holes b ON a.i = b.i",
            "SELECT * FROM big ORDER BY i LIMIT 5",
            "BEGIN",
            "SELECT count(*) FROM holes",
            "ROLLBACK",
            "DESCRIBE big",
            "SELECT * FROM duckdb_tables()",
        ] {
            context.query(sql).unwrap();
            assert!(in_file("big"), "{sql}");
            assert!(in_file("holes"), "{sql}");
        }
    }

    /// The row groups a filter skips come from the metadata, so skipping them takes no reading.
    #[test]
    fn filters_skip_row_groups_of_a_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../duckrs/tests/fixtures/scan_v122880.db");
        let mut context = ClientContext::new(DatabaseInstance::open_file(&path, true).unwrap());
        for (filter, groups) in [("i < 100", "1 / 2"), ("i >= 125000", "1 / 2"), ("i >= 100", "2 / 2")] {
            let sql = format!("EXPLAIN ANALYZE SELECT count(*) FROM big WHERE {filter}");
            let plan = context.query(&sql).unwrap().value(1, 0).try_to_string().unwrap();
            let line = plan
                .lines()
                .find(|line| line.contains("Row Groups Scanned"))
                .unwrap_or(&plan);
            assert!(line.contains(groups), "{filter}: {plan}");
        }
    }

    /// Only the columns that a scan reads are decoded.
    #[test]
    fn a_group_is_read_a_column_at_a_time() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../duckrs/tests/fixtures/scan_v122880.db");
        let catalog = Catalog::new("test");
        load_database(&path, &catalog, true).unwrap();
        let table = catalog.get_table(None, "big").unwrap();
        let source = table.source().unwrap();
        let chunk = source.read_group(1, &[0]).unwrap();
        assert_eq!(chunk.column_count(), 1);
        assert_eq!(chunk.len(), 130_000 - 122_880);
        assert_eq!(table.row_count(), 130_000);
        assert!(table.source().is_some());
    }

    #[test]
    fn indexes_come_along() {
        for file in FILES {
            let catalog = load(file);
            let table = catalog.get_table(None, "indexed").unwrap();
            let indexes = table.indexes.read();
            let found: Vec<(&str, bool)> = indexes.iter().map(|i| (i.name.as_str(), i.unique)).collect();
            assert_eq!(
                found,
                [
                    ("expression_index", false),
                    ("plain_index", false),
                    ("unique_index", true)
                ],
                "{file}"
            );
            let schema = catalog.schema_for(Some("other_schema")).unwrap();
            assert!(schema.get_index("other_index").is_some(), "{file}");
        }
    }
}
