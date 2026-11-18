//! The storage engine: DuckDB's `src/storage`, in step with its file format (see Phase 9 in
//! `docs/ARCHITECTURE.md`). For now it reads database files, into memory.

pub mod bitpacking;
pub mod block_manager;
pub mod buffer_manager;
pub mod checkpoint;
pub mod checksum;
pub mod compression;
mod dict_fsst_analyze;
mod float_codecs;
pub mod encode;
mod fsst;
mod index_data;
pub mod log_writer;
pub mod metadata;
pub mod roaring;
pub mod serialization;
pub mod statistics;
pub mod table_data;
pub(crate) mod wal;
pub mod writer;

use crate::catalog::Catalog;
use block_manager::SingleFileBlockManager;
use buffer_manager::BufferManager;
use duckrs_common::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::Arc;

/// The blocks of a file that stay in memory. Rows stay in the file, and a scan decodes one row group
/// at a time, so this is what bounds the memory that a large file takes.
const BUFFER_MEMORY: usize = 256 << 20;

/// Whether there is a database in the file: a new database starts as a missing or empty file.
/// `DBPathAndType::CheckMagicBytes`: a file that is not a DuckDB (or SQLite) database but that one of
/// the replacement scans reads (`parquet`, `csv`, `tsv`, `json`, `jsonl`, `ndjson`, maybe compressed).
pub fn is_open_file(path: &Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 16];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let read = file.read(&mut header).unwrap_or(0);
    if read >= 12 && &header[8..12] == b"DUCK" || read >= 16 && &header == b"SQLite format 3\0" {
        return false;
    }
    if read >= 4 && &header[..4] == b"PAR1" {
        return true;
    }
    let lower = path.to_string_lossy().to_lowercase();
    let lower = [".gz", ".zst"]
        .iter()
        .find_map(|c| lower.strip_suffix(c))
        .unwrap_or(&lower);
    ["parquet", "csv", "tsv", "json", "jsonl", "ndjson"]
        .iter()
        .any(|suffix| lower.ends_with(&format!(".{suffix}")))
}

pub fn is_database_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// Puts what the last checkpoint of the file at `path` left into `catalog`, with what its write-ahead
/// log committed since. The tables stay in the file, which is what the result reads them through.
pub fn load_database(path: &Path, catalog: &Catalog, read_only: bool) -> Result<Arc<BufferManager>> {
    let blocks = Arc::new(SingleFileBlockManager::open(path, read_only)?);
    let buffer = Arc::new(BufferManager::new(blocks, BUFFER_MEMORY));
    use crate::execution::profiler::{add_counter, time_metric};
    metadata::load_free_list(&buffer)?;
    let records = time_metric("storage.attach_replay_wal_latency", || wal::read_records(path, &buffer))?;
    let stored = time_metric("storage.attach_load_storage_latency", || {
        checkpoint::read_catalog(&buffer)
    })?;
    let mut replay = wal::Replay::new(buffer.clone(), catalog);
    for entry in stored.entries {
        replay.add_entry(entry)?;
    }
    if !records.is_empty() {
        add_counter("storage.wal_replay_entry_count", records.len() as u64);
    }
    time_metric("storage.attach_replay_wal_latency", || replay.replay(records))?;
    Ok(buffer)
}

/// [`wal::replayed_end`]: where the replayed log of the file at `path` goes on, and its entries' version.
pub fn replayed_log_end(path: &Path, buffer: &BufferManager) -> Result<wal::ReplayedLog> {
    wal::replayed_end(path, buffer)
}

/// Whether a write-ahead log is next to the file at `path`.
pub fn has_log(path: &Path) -> bool {
    let mut log = path.as_os_str().to_owned();
    log.push(".wal");
    std::fs::metadata(log).is_ok()
}

/// A new database file in `version`, for `load_database`.
pub fn create_database(
    path: &Path,
    block_size: Option<u64>,
    version: duckrs_common::serializer::StorageVersion,
) -> Result<()> {
    if std::fs::metadata(path).is_ok() {
        std::fs::remove_file(path).map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", path.display())))?;
    }
    let block_size = block_size.unwrap_or(block_manager::DEFAULT_BLOCK_ALLOC_SIZE);
    SingleFileBlockManager::create(path, block_size, version).map(drop)
}

/// Whether checkpoints can go into this file: in the version of the file, from v0.10.2 on.
pub fn takes_checkpoints(buffer: &BufferManager) -> bool {
    buffer.block_manager().storage_version().is_ok()
}

/// `TableCatalogEntry::GetInfo()->ToString()`: the CREATE TABLE of `table`, as EXPORT DATABASE writes it.
pub fn table_create_sql(
    schema: &crate::catalog::SchemaCatalogEntry,
    table: &crate::catalog::TableCatalogEntry,
) -> Result<String> {
    let info = writer::table_info("", schema, table)?;
    let serialization::generated::CreateInfoKind::CreateTableInfo(stored) = info.kind else {
        unreachable!("a table's info is a CreateTableInfo");
    };
    let (mut definition, _) = checkpoint::table_definition(&schema.key(), stored)?;
    definition.name.qualification = if schema.name.eq_ignore_ascii_case(crate::catalog::DEFAULT_SCHEMA) {
        Vec::new()
    } else {
        schema.path.clone()
    };
    Ok(definition.to_string())
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

    /// The bitmasks that C++ wrote have to mean what the allocator takes them to mean, at the
    /// sizes C++ gave the segments, or nothing it writes could be read by DuckDB.
    #[test]
    fn stored_arts_are_laid_out_as_the_allocator_lays_them_out() {
        use crate::execution::art::allocator::{FixedSizeAllocator, PREFIX_METADATA_SIZE, segment_sizes};
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../duckrs/tests/fixtures/parsed_v200.db");
        let catalog = Catalog::new("test");
        let buffer = load_database(&path, &catalog, true).unwrap();
        let block_size = buffer.block_manager().block_size();
        assert_eq!(block_size, 16_376);
        let mut buffers = 0;
        for entry in checkpoint::read_catalog(&buffer).unwrap().entries {
            let checkpoint::StoredEntry::Table(table) = entry else {
                continue;
            };
            for index in &table.indexes {
                let allocators = &index.allocator_infos;
                assert_eq!(allocators.len(), 9, "{}", index.name);
                let prefix_count = allocators[0].segment_size as usize - PREFIX_METADATA_SIZE;
                assert!((prefix_count + 1).is_multiple_of(8), "{}", index.name);
                for (info, size) in allocators.iter().zip(segment_sizes(prefix_count)) {
                    assert_eq!(info.segment_size as usize, size, "{}", index.name);
                    let allocator = FixedSizeAllocator::new(size, block_size);
                    for (pointer, count) in info.block_pointers.iter().zip(&info.segment_counts) {
                        let block = buffer.block_manager().read_block(pointer.block_id as u64).unwrap();
                        let stored = &block[pointer.offset as usize..];
                        assert_eq!(allocator.taken_segments(stored), *count as usize, "{}", index.name);
                        buffers += 1;
                    }
                }
            }
        }
        assert!(buffers > 0, "the fixture has to have an ART to compare with");
    }

    /// An ART that DuckDB wrote, read by the tree as it is. `tools/check_art.py` makes the files.
    #[test]
    #[ignore = "needs the files of tools/check_art.py"]
    fn an_art_that_duckdb_wrote_can_be_read() {
        use crate::execution::art::allocator::FixedSizeAllocator;
        use crate::execution::art::key::ArtKey;
        use crate::execution::art::pointer::{IndexPointer, NodePtr};
        use crate::execution::art::tree::{AppendMode, Art, Conflict};
        const ROWS: i64 = 20_000;
        let text = |row: i64| format!("https://example.com/items/{row}");
        let files = std::env::var("DUCKRS_ART_FILES").expect("the files to read");
        for file in files.split(':') {
            let catalog = Catalog::new("test");
            let buffer = load_database(Path::new(file), &catalog, true).unwrap();
            let block_size = buffer.block_manager().block_size();
            let tables = checkpoint::read_catalog(&buffer).unwrap().entries.into_iter();
            let table = tables
                .filter_map(|entry| match entry {
                    checkpoint::StoredEntry::Table(table) => Some(table),
                    _ => None,
                })
                .find(|table| table.name == "t")
                .expect("the table of the script");
            assert_eq!(table.indexes.len(), 4, "{file}");
            for index in &table.indexes {
                let mut allocators = Vec::new();
                for info in &index.allocator_infos {
                    let mut allocator = FixedSizeAllocator::new(info.segment_size as usize, block_size);
                    for ((id, pointer), count) in info
                        .buffer_ids
                        .iter()
                        .zip(&info.block_pointers)
                        .zip(&info.segment_counts)
                    {
                        let block = buffer.block_manager().read_block(pointer.block_id as u64).unwrap();
                        allocator.load_buffer(*id, &block[pointer.offset as usize..], *count as usize);
                    }
                    allocators.push(allocator);
                }
                let root = NodePtr(IndexPointer::from_bits(index.root));
                let unique = !index.name.starts_with("idx_groups") && !index.name.starts_with("idx_pairs");
                let mut art = Art::from_parts(allocators, root, unique);
                let scanned = art.scan().unwrap();
                let rows: usize = scanned.iter().map(|(_, rows)| rows.len()).sum();
                assert_eq!(rows, ROWS as usize, "{file} {}", index.name);
                assert!(scanned.windows(2).all(|pair| pair[0].0 < pair[1].0), "{}", index.name);
                let key_of = |row: i64| -> ArtKey {
                    match index.name.as_str() {
                        name if name.starts_with("idx_groups") => ArtKey::of((row % 7) as i32),
                        name if name.starts_with("idx_pairs") => {
                            let mut key = ArtKey::of_string(format!("group-{}", row % 300).as_bytes());
                            key.concat(&ArtKey::of(row % 11));
                            key
                        }
                        name if name.starts_with("PRIMARY") => ArtKey::of(row as i32),
                        _ => ArtKey::of_string(text(row).as_bytes()),
                    }
                };
                for row in (0..ROWS).step_by(97) {
                    assert!(
                        art.lookup(&key_of(row)).unwrap().contains(&row),
                        "{file} {} {row}",
                        index.name
                    );
                }
                // what it read has to be good for changing, too
                for row in (0..ROWS).step_by(3) {
                    assert!(art.erase(&key_of(row), row).unwrap(), "{file} {} {row}", index.name);
                }
                for row in (0..ROWS).step_by(3) {
                    assert_eq!(
                        art.insert(&key_of(row), row, AppendMode::Default).unwrap(),
                        Conflict::None
                    );
                }
                let again: usize = art.scan().unwrap().iter().map(|(_, rows)| rows.len()).sum();
                assert_eq!(again, ROWS as usize, "{file} {}", index.name);
            }
        }
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
            let sizes: Vec<usize> = keys.as_ref().unwrap().iter().map(|k| k.len().unwrap()).collect();
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
