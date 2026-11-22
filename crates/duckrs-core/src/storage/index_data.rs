//! Ported from duckdb/src/execution/index/art/art.cpp (`Checkpoint`, `SerializeToWAL`) and
//! duckdb/src/catalog/catalog_entry/duck_table_entry.cpp (`GetIndexInfo`) @ rust-port-baseline.
//!
//! The ARTs of a file. C++ keeps them with the table and writes out what changed; here tables
//! change by being rewritten, so each checkpoint builds them from the rows it writes. Nothing reads
//! them back: `duckrs` builds its keys from the rows, and only C++ needs them to be there.

use super::serialization::generated::{self as g};
use crate::catalog::{IndexCatalogEntry, TableCatalogEntry, TableConstraint, TableKey};
use crate::execution::art::allocator::{LEGACY_NODE_256_LEAF_SIZE, NODE_256_LEAF_SIZE};
use crate::execution::art::tree::{AppendMode, Art, Conflict};
use crate::execution::constraints::{key_prefix_count, keys_of_converted};
use duckrs_common::storage_version::StorageVersion;
use duckrs_common::types::LogicalTypeId;
use duckrs_common::value::Value;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

/// `ARTSerializationFormat`: what files from before v1.2.0 have is `V1_0_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerializationFormat {
    V1_0_0,
    Current,
}

/// `ART::DEPRECATED_ALLOCATOR_COUNT`: the allocators of `V1_0_0`, which has no leaf nodes.
const DEPRECATED_ALLOCATOR_COUNT: usize = 6;

/// An index of a table and the tree it has so far, for a file of `version`.
pub(super) struct TableIndex {
    pub name: String,
    key: TableKey,
    art: Art,
    version: StorageVersion,
    /// `ART::storage_version`: what its keys are made as, `None` for `INVALID` (before v1.5.0)
    key_version: Option<StorageVersion>,
    /// [`Self::has_legacy_geometry_keys`]
    pub legacy_geometry: bool,
    /// Whether the tree is in the `V1_0_0` format already.
    deprecated: bool,
}

impl TableIndex {
    fn new(
        table: &TableCatalogEntry,
        name: String,
        key: TableKey,
        unique: bool,
        block_size: usize,
        version: StorageVersion,
    ) -> Self {
        // `GetNode256LeafSegmentSize` of a new index
        let leaf_size = if version >= StorageVersion::V2_0_0 {
            NODE_256_LEAF_SIZE
        } else {
            LEGACY_NODE_256_LEAF_SIZE
        };
        let art = Art::with_node_256_leaf_size(key_prefix_count(table, &key), leaf_size, block_size, unique);
        Self {
            name,
            key,
            art,
            version,
            key_version: Some(version),
            legacy_geometry: false,
            deprecated: false,
        }
    }

    /// `ART::HasLegacyGeometryKeys` (`KeyInputNeedConversion`): the keys of GEOMETRYs are of the spatial
    /// extension's format, as before v1.5.0, so the index can't be remapped.
    pub fn has_legacy_geometry_keys(&self, table: &TableCatalogEntry) -> bool {
        let old = self.key_version.is_none_or(|version| version < StorageVersion::V1_5_0);
        let geometry = match &self.key.expressions {
            None => self
                .key
                .columns
                .iter()
                .any(|c| table.columns[*c].ty.id() == LogicalTypeId::Geometry),
            Some(expressions) => expressions
                .iter()
                .any(|e| e.return_type.id() == LogicalTypeId::Geometry),
        };
        old && geometry
    }

    /// `ART::GetSerializationFormat`.
    fn format(&self) -> SerializationFormat {
        if self.version < StorageVersion::V1_2_0 {
            SerializationFormat::V1_0_0
        } else {
            SerializationFormat::Current
        }
    }

    /// `ART::GetAllocatorCount`.
    fn allocator_count(&self) -> usize {
        match self.format() {
            SerializationFormat::V1_0_0 => DEPRECATED_ALLOCATOR_COUNT,
            SerializationFormat::Current => self.art.allocators().len(),
        }
    }

    /// `AddIndex` of a table: its keys in the order of its constraints, then its indexes. C++ reads
    /// back the first ones by position and the last ones by name.
    pub fn all_of(table: &TableCatalogEntry, block_size: usize, version: StorageVersion) -> Vec<Self> {
        let mut indexes = Vec::new();
        for (position, constraint) in table.constraints.iter().enumerate() {
            let (kind, columns, unique) = match constraint {
                TableConstraint::Check { .. } | TableConstraint::NotNull(_) => continue,
                TableConstraint::Unique {
                    columns, primary_key, ..
                } => (if *primary_key { "PRIMARY" } else { "UNIQUE" }, columns, true),
                TableConstraint::ForeignKey(key) => ("FOREIGN", &key.columns, false),
            };
            let key = TableKey {
                columns: columns.clone(),
                expressions: None,
                names: Vec::new(),
                primary_key: false,
                deferred: false,
            };
            let name = format!("{kind}_{}_{position}", table.name);
            indexes.push(Self::new(table, name, key, unique, block_size, version));
        }
        for index in table.indexes.read().iter() {
            indexes.push(Self::of_index(table, index, block_size, version));
        }
        indexes
    }

    pub fn of_index(
        table: &TableCatalogEntry,
        index: &Arc<IndexCatalogEntry>,
        block_size: usize,
        version: StorageVersion,
    ) -> Self {
        let key = TableKey {
            columns: index.columns.clone(),
            expressions: Some(index.expressions.clone()),
            names: index.names.clone(),
            primary_key: false,
            deferred: false,
        };
        let mut result = Self::new(table, index.name.clone(), key, index.unique, block_size, version);
        result.key_version = match index.storage_version {
            None => Some(version),
            Some(stored) => stored.map(StorageVersion),
        };
        result.legacy_geometry = result.has_legacy_geometry_keys(table);
        result
    }

    /// Adds the rows of `chunk`, of which `row_id` says what they are. Not after [`Self::storage_info`].
    pub fn add(&mut self, chunk: &DataChunk, row_id: impl Fn(usize) -> i64) -> Result<()> {
        // `ART::ConvertKeyInput`
        let keys = keys_of_converted(&self.key, chunk, self.legacy_geometry)?;
        for (row, key) in keys.into_iter().enumerate() {
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

    /// `PrepareSerialize`: the format of the file, into which the tree may have to change.
    fn prepare(&mut self) -> Result<()> {
        if self.format() == SerializationFormat::V1_0_0 && !self.deprecated {
            self.art.transform_to_deprecated()?;
            self.deprecated = true;
        }
        Ok(())
    }

    /// `PrepareSerialize` and `GetInfo`. What `place` says of each buffer is where it is.
    pub fn storage_info(&mut self, place: impl FnMut(&[u8]) -> Result<g::BlockPointer>) -> Result<g::IndexStorageInfo> {
        self.prepare()?;
        self.info(place)
    }

    fn info(&self, mut place: impl FnMut(&[u8]) -> Result<g::BlockPointer>) -> Result<g::IndexStorageInfo> {
        let mut allocator_infos = Vec::new();
        for allocator in &self.art.allocators()[..self.allocator_count()] {
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
            // without it C++ takes the keys to be from before v1.5.0; a new index has the version of its file
            options: self
                .key_version
                .map(|version| ("storage_version".to_string(), Value::ubigint(version.0)))
                .into_iter()
                .collect(),
        })
    }

    /// `SerializeToWAL`: buffers go in the log, so they have no place in a file yet.
    pub fn storage_info_for_log(&mut self) -> Result<(g::IndexStorageInfo, Vec<Vec<&[u8]>>)> {
        self.prepare()?;
        let info = self.info(|_| {
            Ok(g::BlockPointer {
                block_id: -1, // `INVALID_BLOCK`, which replaying the log replaces
                offset: 0,
            })
        })?;
        let buffers = self.art.allocators()[..self.allocator_count()]
            .iter()
            .map(|allocator| allocator.stored_buffers().into_iter().map(|b| b.data).collect())
            .collect();
        Ok((info, buffers))
    }
}
