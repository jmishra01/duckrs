//! Ported from duckdb/src/execution/index/art/art.cpp (`Checkpoint`, `SerializeToWAL`) and
//! duckdb/src/catalog/catalog_entry/duck_table_entry.cpp (`GetIndexInfo`) @ rust-port-baseline.
//!
//! The ARTs of a file. C++ keeps them with the table and writes out what changed; here tables
//! change by being rewritten, so each checkpoint builds them from the rows it writes. Nothing reads
//! them back: `duckrs` builds its keys from the rows, and only C++ needs them to be there.

use super::checkpoint::STORAGE_VERSION_V2_0_0;
use super::serialization::generated::{self as g};
use crate::catalog::{IndexCatalogEntry, TableCatalogEntry, TableConstraint, TableKey};
use crate::execution::art::tree::{AppendMode, Art, Conflict};
use crate::execution::constraints::{key_prefix_count, keys_of};
use duckrs_common::value::Value;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

/// An index of a table and the tree it has so far.
pub(super) struct TableIndex {
    pub name: String,
    key: TableKey,
    art: Art,
}

impl TableIndex {
    fn new(table: &TableCatalogEntry, name: String, key: TableKey, unique: bool, block_size: usize) -> Self {
        let art = Art::new(key_prefix_count(table, &key), block_size, unique);
        Self { name, key, art }
    }

    /// `AddIndex` of a table: its keys in the order of its constraints, then its indexes. C++ reads
    /// back the first ones by position and the last ones by name.
    pub fn all_of(table: &TableCatalogEntry, block_size: usize) -> Vec<Self> {
        // the NOT NULL go first in what a table says of its constraints
        let not_null = table
            .logical_columns()
            .iter()
            .filter(|column| column.definition.is_some_and(|d| d.not_null))
            .count();
        let mut indexes = Vec::new();
        for (position, constraint) in table.constraints.iter().enumerate() {
            let position = not_null + position;
            let (kind, columns, unique) = match constraint {
                TableConstraint::Check { .. } => continue,
                TableConstraint::Unique { columns, primary_key } => {
                    (if *primary_key { "PRIMARY" } else { "UNIQUE" }, columns, true)
                }
                TableConstraint::ForeignKey(key) => ("FOREIGN", &key.columns, false),
            };
            let key = TableKey {
                columns: columns.clone(),
                expressions: None,
                names: Vec::new(),
                primary_key: false,
            };
            let name = format!("{kind}_{}_{position}", table.name);
            indexes.push(Self::new(table, name, key, unique, block_size));
        }
        for index in table.indexes.read().iter() {
            indexes.push(Self::of_index(table, index, block_size));
        }
        indexes
    }

    pub fn of_index(table: &TableCatalogEntry, index: &Arc<IndexCatalogEntry>, block_size: usize) -> Self {
        let key = TableKey {
            columns: index.columns.clone(),
            expressions: Some(index.expressions.clone()),
            names: index.names.clone(),
            primary_key: false,
        };
        Self::new(table, index.name.clone(), key, index.unique, block_size)
    }

    /// Adds the rows of `chunk`, of which `row_id` says what they are.
    pub fn add(&mut self, chunk: &DataChunk, row_id: impl Fn(usize) -> i64) -> Result<()> {
        for (row, key) in keys_of(&self.key, chunk)?.into_iter().enumerate() {
            let Some(key) = key else { continue };
            if self.art.insert(&key, row_id(row), AppendMode::Default)? != Conflict::None {
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!("Duplicate key in a checkpoint of index {}", self.name),
                ));
            }
        }
        Ok(())
    }

    /// `PrepareSerialize` and `GetInfo`. What `place` says of each buffer is where it is.
    pub fn storage_info(&self, mut place: impl FnMut(&[u8]) -> Result<g::BlockPointer>) -> Result<g::IndexStorageInfo> {
        let mut allocator_infos = Vec::new();
        for allocator in self.art.allocators() {
            let mut info = g::FixedSizeAllocatorInfo {
                segment_size: allocator.segment_size() as u64,
                buffer_ids: Vec::new(),
                block_pointers: Vec::new(),
                segment_counts: Vec::new(),
                allocation_sizes: Vec::new(),
                buffers_with_free_space: Vec::new(),
            };
            for buffer in allocator.stored_buffers() {
                info.buffer_ids.push(buffer.id);
                info.block_pointers.push(place(buffer.data)?);
                info.segment_counts.push(buffer.segment_count as u64);
                info.allocation_sizes.push(buffer.data.len() as u64);
                if buffer.has_free_space {
                    info.buffers_with_free_space.push(buffer.id);
                }
            }
            allocator_infos.push(info);
        }
        Ok(g::IndexStorageInfo {
            name: self.name.clone(),
            root: self.art.root_bits(),
            allocator_infos,
            // without it C++ takes the keys to be from before v1.5.0
            options: vec![("storage_version".to_string(), Value::ubigint(STORAGE_VERSION_V2_0_0))],
        })
    }

    /// `SerializeToWAL`: buffers go in the log, so they have no place in a file yet.
    pub fn storage_info_for_log(&self) -> Result<(g::IndexStorageInfo, Vec<Vec<&[u8]>>)> {
        let info = self.storage_info(|_| {
            Ok(g::BlockPointer {
                block_id: -1, // `INVALID_BLOCK`, which replaying the log replaces
                offset: 0,
            })
        })?;
        let buffers = self
            .art
            .allocators()
            .iter()
            .map(|allocator| allocator.stored_buffers().into_iter().map(|b| b.data).collect())
            .collect();
        Ok((info, buffers))
    }
}
