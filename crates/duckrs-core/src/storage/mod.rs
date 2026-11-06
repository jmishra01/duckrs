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

use crate::catalog::Catalog;
use block_manager::SingleFileBlockManager;
use buffer_manager::BufferManager;
use duckrs_common::Result;
use std::path::Path;
use std::sync::Arc;

/// What of the file stays in memory while it loads. Rows are copied out, so this only bounds the
/// blocks read ahead of that.
const LOAD_BUFFER_MEMORY: usize = 256 << 20;

/// Whether there is a database in the file: a new database starts as a missing or empty file.
pub fn is_database_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// Puts what the last checkpoint of the file at `path` left into `catalog`, with what its write-ahead
/// log committed since. The file is opened read-only: it and the log stay as they are.
pub fn load_database(path: &Path, catalog: &Catalog) -> Result<()> {
    let blocks = Arc::new(SingleFileBlockManager::open(path, true)?);
    let buffer = BufferManager::new(blocks, LOAD_BUFFER_MEMORY);
    metadata::load_free_list(&buffer)?;
    let records = wal::read_records(path, &buffer)?;
    let stored = checkpoint::read_catalog(&buffer)?;
    let mut replay = wal::Replay::new(&buffer, catalog, !records.is_empty());
    for entry in stored.entries {
        replay.add_entry(entry)?;
    }
    replay.replay(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{TableCatalogEntry, TableConstraint};
    use crate::execution::constraints::current_keys;

    const FILES: [&str; 3] = ["parsed_v100.db", "parsed_v150.db", "parsed_v200.db"];

    fn load(file: &str) -> Catalog {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../duckrs/tests/fixtures")
            .join(file);
        let catalog = Catalog::new("test");
        load_database(&path, &catalog).unwrap();
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
            let rows = parent.storage.read();
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
