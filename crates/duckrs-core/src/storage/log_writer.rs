//! Ported from duckdb/src/storage/write_ahead_log.cpp and the `Serialize` of duckdb/src/common/types/
//! {data_chunk,vector}.cpp @ rust-port-baseline.
//!
//! Makes commits durable. The engine has no undo log, so a commit looks at how the catalog differs
//! from what is durable and writes that, in the records of C++, which replays them too. This way
//! whatever a statement does comes to the same few records, and what a transaction undid never
//! shows up.
//!
//! Tables change by being rewritten, which says nothing of which rows changed. So a commit that
//! did more than add rows deletes all rows of the table and inserts them again.

use super::block_manager::DB_IDENTIFIER_LEN;
use super::checksum::checksum;
use super::index_data::TableIndex;
use super::metadata::MetaBlockPointer;
use super::serialization::custom::QualifiedName;
use super::serialization::generated::enums::wal_type;
use super::serialization::generated::{self as g};
use super::serialization::to_stored::write_not_ported;
use super::table_data::fixed_width;
use super::writer::{
    index_info, macro_info, parents_first, raw_bytes, sequence_info, table_info, trigger_info, type_info, view_info,
};
use crate::catalog::{
    Catalog, IndexCatalogEntry, MacroCatalogEntry, RowIds, SchemaCatalogEntry, SequenceCatalogEntry, TEMP_SCHEMA,
    TableCatalogEntry, TableConstraint, TableLog, TableOrView, ViewCatalogEntry,
};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::serializer::{BinarySerializer, SerializationOptions, Serialize, StorageVersion};
use duckrs_common::types::{ListEntry, LogicalType, LogicalTypeId, PhysicalType, StringT};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `WAL_VERSION_NUMBER`.
const WAL_VERSION: u64 = 2;
/// `WAL_ENCRYPTED_VERSION_NUMBER`: the log of an encrypted file.
const WAL_ENCRYPTED_VERSION: u64 = 3;

type Key = (String, String);

fn key(schema: &str, name: &str) -> Key {
    (schema.to_lowercase(), name.to_lowercase())
}

use crate::catalog::SCHEMA_PATH_SEPARATOR;

/// The schemas of a schema's name here: a nested one is named by its path.
fn schema_path(schema: &str) -> Vec<String> {
    schema.split(SCHEMA_PATH_SEPARATOR).map(str::to_string).collect()
}

/// The name of a schema here: a nested one by its path, so that the log can say where it is.
fn schema_ident(schema: &SchemaCatalogEntry) -> String {
    schema.path.join(&SCHEMA_PATH_SEPARATOR.to_string())
}

fn path_of((schema, name): &Key) -> Option<QualifiedName> {
    let mut path = schema_path(schema);
    path.push(name.clone());
    Some(QualifiedName { path })
}

/// The entries that are durable, each with its original name.
#[derive(Default)]
struct Durable {
    schemas: BTreeMap<String, String>,
    tables: BTreeMap<Key, (Key, Arc<TableCatalogEntry>)>,
    /// Each with the table it is on, as a table that goes takes them along.
    indexes: BTreeMap<Key, (Key, Arc<TableCatalogEntry>, Arc<IndexCatalogEntry>)>,
    views: BTreeMap<Key, (Key, Arc<ViewCatalogEntry>)>,
    macros: [BTreeMap<Key, (Key, Arc<MacroCatalogEntry>)>; 2],
    sequences: BTreeMap<Key, (Key, Arc<SequenceCatalogEntry>, u64)>,
    /// with what `COMMENT ON TYPE` said, which a change of logs again
    types: BTreeMap<Key, (Key, (LogicalType, Option<String>))>,
    /// By the table they are on and their name, which is unique per table.
    triggers: BTreeMap<Key, (Key, Arc<crate::catalog::TriggerCatalogEntry>)>,
}

impl Durable {
    fn of(catalog: &Catalog) -> Self {
        let mut result = Self::default();
        for schema in catalog
            .schemas()
            .iter()
            .filter(|s| !s.name.eq_ignore_ascii_case(TEMP_SCHEMA))
        {
            let ident = schema_ident(schema);
            let named = |name: &str| (ident.clone(), name.to_string());
            result.schemas.insert(ident.to_lowercase(), ident.clone());
            for entry in schema.tables_and_views() {
                match entry {
                    TableOrView::Table(table) => {
                        for index in table.indexes.read().iter() {
                            let entry = (named(&index.name), table.clone(), index.clone());
                            result.indexes.insert(key(&ident, &index.name), entry);
                        }
                        result
                            .tables
                            .insert(key(&ident, &table.name), (named(&table.name), table));
                    }
                    TableOrView::View(view) => {
                        result.views.insert(key(&ident, &view.name), (named(&view.name), view));
                    }
                }
            }
            for table_macro in [false, true] {
                for entry in schema.macro_entries(table_macro) {
                    let name = named(&entry.name);
                    result.macros[usize::from(table_macro)].insert(key(&ident, &entry.name), (name, entry));
                }
            }
            for sequence in schema.sequence_entries() {
                let usage = sequence.data().usage_count;
                let name = named(&sequence.name);
                result
                    .sequences
                    .insert(key(&ident, &sequence.name), (name, sequence, usage));
            }
            for (name, ty) in schema.type_entries() {
                let comment = schema.type_comment(&name);
                result.types.insert(key(&ident, &name), (named(&name), (ty, comment)));
            }
            for trigger in schema.trigger_entries() {
                let on = format!("{}{SCHEMA_PATH_SEPARATOR}{}", trigger.table, trigger.name);
                result
                    .triggers
                    .insert(key(&ident, &on), (named(&trigger.name), trigger));
            }
        }
        result
    }
}

/// The records of one commit, as the version of the file has them.
/// The bytes of some log records, the version they are in, how many there are, and how the file is encrypted.
struct Records(Vec<u8>, StorageVersion, u64, Option<super::encryption::FileEncryption>);

impl Records {
    fn entry(&mut self, wal: u64, body: impl FnOnce(&mut BinarySerializer)) {
        let mut s = BinarySerializer::new(SerializationOptions {
            target: self.1,
            ..Default::default()
        });
        s.write_object_value(|s| {
            s.write_property(100, "wal_type", &wal);
            body(s);
        });
        let payload = s.into_bytes();
        self.0.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        match &self.3 {
            // `ChecksumWriter::FlushEncrypted`
            Some(encryption) => {
                let framed = encryption
                    .encrypt_log_entry(checksum(&payload), &payload)
                    .expect("random bytes for a nonce");
                self.0.extend_from_slice(&framed);
            }
            None => {
                self.0.extend_from_slice(&checksum(&payload).to_le_bytes());
                self.0.extend_from_slice(&payload);
            }
        }
        self.2 += 1;
    }

    /// DROP of a kind that all have the same fields.
    /// `WALDropTable` and the like: before v2.0.0 by schema and name, from then on by the path.
    fn drop_entry(&mut self, wal: u64, name: &Key) {
        self.entry(wal, |s| {
            g::WALDropTable {
                schema: name.0.clone(),
                name: name.1.clone(),
                qualified_name: path_of(name),
            }
            .write_members(s);
        });
    }

    fn use_table(&mut self, name: &Key) {
        self.entry(wal_type::USE_TABLE, |s| {
            g::WALUseTable {
                schema: name.0.clone(),
                table: name.1.clone(),
                qualified_name: path_of(name),
            }
            .write_members(s);
        });
    }

    /// `WALCreateSchema` and `WALDropSchema`.
    fn schema_entry(&mut self, wal: u64, schema: &str) {
        self.entry(wal, |s| {
            g::WALDropSchema {
                schema: schema.to_string(),
                qualified_name: Some(QualifiedName {
                    path: schema_path(schema),
                }),
            }
            .write_members(s);
        });
    }
}

fn write_validity(s: &mut BinarySerializer, vector: &Vector, count: usize) -> bool {
    let has_mask = (0..count).any(|row| !vector.is_valid(row));
    s.write_property(100, "has_validity_mask", &has_mask);
    if has_mask {
        let mut mask = vec![0u8; count.div_ceil(64) * 8];
        for row in (0..count).filter(|&row| vector.is_valid(row)) {
            mask[row / 8] |= 1 << (row % 8);
        }
        s.write_property(101, "validity", mask.as_slice());
    }
    has_mask
}

/// `Vector::Serialize`, of a flat vector, as v2.0.0 has it.
fn write_vector(s: &mut BinarySerializer, vector: &Vector, ty: &LogicalType, count: usize) -> Result<()> {
    // `Vector::Serialize` of a GEOMETRY: its format, then the WKB as a list of blobs
    if ty.id() == LogicalTypeId::Geometry {
        // before v1.5.0 (`Geometry::VERSION_ADDED`) without a format: the spatial extension's
        let spatial = !s.should_serialize(StorageVersion::V1_5_0);
        if !spatial {
            const WKB: u8 = 1;
            s.write_property(99, "geometry_format", &WKB);
        }
        write_validity(s, vector, count);
        let mut blobs = Vec::with_capacity(count);
        for row in 0..count {
            let value = vector.get_value(row);
            blobs.push(match value.data() {
                duckrs_common::value::ValueData::Bytes(bytes) if vector.is_valid(row) => {
                    if spatial {
                        duckrs_common::geometry::to_spatial(bytes)?
                    } else {
                        bytes.to_vec()
                    }
                }
                _ => Vec::new(),
            });
        }
        s.write_list(102, "data", count, |s, row| {
            s.write_varint_unsigned(blobs[row].len() as u64);
            s.write_raw(&blobs[row]);
        });
        return Ok(());
    }
    if ty.id() == LogicalTypeId::Variant {
        let stored = duckrs_common::variant::to_unshredded_vector(vector, count)?;
        return write_vector(s, &stored, &duckrs_common::variant::unshredded_type(), count);
    }
    write_validity(s, vector, count);
    match ty.physical_type() {
        PhysicalType::Varchar => {
            let data = vector.data::<StringT>();
            let mut lengths = Vec::with_capacity(count * 4);
            let mut bytes = Vec::new();
            for (row, string) in data.iter().enumerate().take(count) {
                let string: &[u8] = if vector.is_valid(row) {
                    // SAFETY: the row is valid, so its string lives in the heap of the vector
                    unsafe { string.as_bytes() }
                } else {
                    &[]
                };
                lengths.extend_from_slice(&(string.len() as u32).to_le_bytes());
                bytes.extend_from_slice(string);
            }
            s.write_property(107, "byte_data_length", &(bytes.len() as u64));
            s.write_property(108, "length_data", lengths.as_slice());
            s.write_property(109, "byte_data", bytes.as_slice());
        }
        PhysicalType::Struct => {
            let fields = ty.struct_children();
            let mut result = Ok(());
            s.write_list(103, "children", fields.len(), |s, i| {
                let child = flat(&vector.struct_children()[i], count);
                s.write_object_value(|s| {
                    if result.is_ok() {
                        result = write_vector(s, &child, &fields[i].1, count);
                    }
                });
            });
            result?;
        }
        PhysicalType::List => {
            // the children of the rows, in order and nothing else: a chunk can have more
            let mut selection = Vec::new();
            let mut entries = Vec::with_capacity(count);
            for (row, entry) in vector.data::<ListEntry>().iter().enumerate().take(count) {
                let offset = selection.len() as u64;
                if vector.is_valid(row) {
                    selection.extend((entry.offset..entry.offset + entry.length).map(|i| i as u32));
                    entries.push((offset, entry.length));
                } else {
                    entries.push((offset, 0));
                }
            }
            let list_size = selection.len();
            s.write_property(104, "list_size", &(list_size as u64));
            s.write_list(105, "entries", count, |s, row| {
                s.write_object_value(|s| {
                    s.write_property(100, "offset", &entries[row].0);
                    s.write_property(101, "length", &entries[row].1);
                });
            });
            let mut child = vector.list_child().clone();
            child.slice(&SelectionVector::from_vec(selection), list_size);
            child.flatten(list_size);
            let child_type = ty.child_type().expect("a list has a child type");
            let mut result = Ok(());
            s.write_object(106, "child", |s| {
                result = write_vector(s, &child, child_type, list_size)
            });
            result?;
        }
        PhysicalType::Array => {
            let size = ty.array_size().expect("an array has a size") as usize;
            s.write_property(103, "array_size", &(size as u64));
            let child = flat(vector.array_child(), size * count);
            let child_type = ty.child_type().expect("an array has a child type");
            let mut result = Ok(());
            s.write_object(104, "child", |s| {
                result = write_vector(s, &child, child_type, size * count)
            });
            result?;
        }
        physical => {
            if fixed_width(physical).is_none() {
                return Err(write_not_ported(&format!("a column of type {ty}")));
            }
            s.write_property(102, "data", raw_bytes(vector, count)?.as_slice());
        }
    }
    Ok(())
}

fn flat(vector: &Vector, count: usize) -> Vector {
    let mut vector = vector.clone();
    vector.flatten(count);
    vector
}

/// A chunk in the bytes of `DataChunk::Serialize`, for temporary files.
pub(crate) fn serialize_chunk(chunk: &DataChunk) -> Result<Vec<u8>> {
    let mut s = BinarySerializer::new(SerializationOptions::default());
    write_chunk_as(&mut s, 100, chunk)?;
    Ok(s.into_bytes())
}

/// Whether a chunk of these types can be serialized.
pub(crate) fn can_serialize(types: &[LogicalType]) -> bool {
    fn can(ty: &LogicalType) -> bool {
        ty.child_type().is_none_or(can) && ty.struct_children().iter().all(|(_, child)| can(child))
    }
    types.iter().all(can)
}

fn write_chunk(s: &mut BinarySerializer, chunk: &DataChunk) -> Result<()> {
    write_chunk_as(s, 101, chunk)
}

/// `DataChunk::Serialize`.
fn write_chunk_as(s: &mut BinarySerializer, field: u16, chunk: &DataChunk) -> Result<()> {
    let count = chunk.len();
    let types = chunk.types();
    let mut result = Ok(());
    s.write_object(field, "chunk", |s| {
        s.write_property(100, "rows", &(count as u32));
        s.write_property(101, "types", &types);
        s.write_list(102, "columns", types.len(), |s, i| {
            let vector = flat(chunk.column(i), count);
            s.write_object_value(|s| {
                if result.is_ok() {
                    result = write_vector(s, &vector, &types[i], count);
                }
            });
        });
    });
    result
}

/// Rows by the ids that a replay gives them.
fn log_deletes(records: &mut Records, ids: &[u64]) -> Result<()> {
    for ids in ids.chunks(STANDARD_VECTOR_SIZE) {
        let mut vector = Vector::new(LogicalType::BIGINT, ids.len());
        for (target, id) in vector.data_mut::<i64>().iter_mut().zip(ids) {
            *target = *id as i64;
        }
        let chunk = DataChunk::from_columns(vec![vector], ids.len());
        let mut result = Ok(());
        records.entry(wal_type::DELETE_TUPLE, |s| result = write_chunk(s, &chunk));
        result?;
    }
    Ok(())
}

/// `WriteAheadLog::WriteUpdate` of a column for the rows at `positions`, in order, whose ids in a
/// replay are `ids`.
fn log_update(
    records: &mut Records,
    chunk_column: &mut dyn FnMut(usize) -> Result<Vector>,
    column: usize,
    positions: &[usize],
    ids: &[u64],
) -> Result<()> {
    let mut done = 0;
    for part in positions.chunk_by(|a, b| a / STANDARD_VECTOR_SIZE == b / STANDARD_VECTOR_SIZE) {
        let selection = part.iter().map(|at| (at % STANDARD_VECTOR_SIZE) as u32).collect();
        let mut values = chunk_column(part[0] / STANDARD_VECTOR_SIZE)?;
        values.slice(&SelectionVector::from_vec(selection), part.len());
        let mut row_ids = Vector::new(LogicalType::BIGINT, part.len());
        for (target, id) in row_ids.data_mut::<i64>().iter_mut().zip(&ids[done..]) {
            *target = *id as i64;
        }
        done += part.len();
        let update = DataChunk::from_columns(vec![values, row_ids], part.len());
        let mut result = Ok(());
        records.entry(wal_type::UPDATE_TUPLE, |s| {
            s.write_property(101, "column_indexes", &vec![column as u64]);
            result = write_chunk_as(s, 102, &update);
        });
        result?;
    }
    Ok(())
}

/// What changed of the rows of a table, in records.
/// What writes the rows of a large append to blocks ahead of a checkpoint (`OptimisticDataWriter`): of a table, its
/// rows in memory, the position of the first new one and its id.
pub type WriteAhead<'a> =
    &'a dyn Fn(&TableCatalogEntry, &ColumnDataCollection, usize, u64) -> Result<Vec<super::writer::WrittenGroup>>;

fn log_rows(records: &mut Records, name: &Key, table: &TableCatalogEntry, ahead: Option<WriteAhead>) -> Result<()> {
    let mut log = table.log.lock();
    let count = table.row_count();
    let ids = table.row_ids(count);
    if Arc::ptr_eq(&ids, &log.logged) && log.updated.is_empty() {
        return Ok(());
    }
    let first_new = ids.first_from(log.logged.next());
    if first_new == log.logged.len() && first_new == count && log.updated.is_empty() {
        let gone = log.logged.missing_from(&ids);
        if gone.is_empty() {
            log.logged = ids;
            return Ok(());
        }
    }
    records.use_table(name);
    let mut replayed = (*log.replayed).clone();
    let columns: Vec<usize> = log.updated_columns.iter().copied().collect();
    // a nested column takes more than a column in an update, and a replay sees it as a new row
    let nested = columns
        .iter()
        .any(|c| table.columns[*c].ty.physical_type() == PhysicalType::Struct);
    // rows still in a file, of which nothing is new, need only the columns that were updated
    // rows in a file, with those appended after them, are read where they are
    let in_file = table.source().filter(|_| !nested);
    let storage = match &in_file {
        Some(_) => None,
        None => Some(table.rows()?),
    };
    let first_written = if nested {
        let all: Vec<u64> = replayed
            .ranges()
            .flat_map(|(id, count)| id..id + count as u64)
            .collect();
        log_deletes(records, &all)?;
        replayed = RowIds::starting_at(replayed.next());
        0
    } else {
        // rows that come after these go in with their new values anyway
        let (positions, updated): (Vec<usize>, Vec<u64>) = log
            .updated
            .iter()
            .filter_map(|id| Some((ids.position(id)?, log.replayed.id(log.logged.position(id)?))))
            .unzip();
        if !positions.is_empty() {
            for column in &columns {
                let mut chunk_column: Box<dyn FnMut(usize) -> Result<Vector>> = match (&in_file, &storage) {
                    (Some(source), _) => {
                        let ty = table.columns[*column].ty.clone();
                        let mut cursor = crate::catalog::TableCursor::new(
                            source.clone(),
                            vec![*column],
                            vec![ty],
                            table.row_group_size(),
                        );
                        Box::new(move |chunk| Ok(cursor.chunk(chunk)?.column(0).clone()))
                    }
                    (None, Some(rows)) => Box::new(|chunk| Ok(rows.chunk(chunk).column(*column).clone())),
                    (None, None) => unreachable!("the rows are read where they aren't in a file"),
                };
                log_update(records, &mut *chunk_column, *column, &positions, &updated)?;
            }
        }
        let gone = log.logged.missing_from(&ids);
        let gone_ids: Vec<u64> = gone.iter().map(|at| log.replayed.id(*at)).collect();
        log_deletes(records, &gone_ids)?;
        replayed.remove(&gone);
        first_new
    };
    if first_written < count {
        let source = match (&in_file, &storage) {
            (Some(source), _) => source.clone(),
            (None, Some(rows)) => {
                let parts = vec![super::appended::AppendedPart::Memory(Arc::new((**rows).clone()))];
                let appended =
                    super::appended::AppendedTable::new(parts, (*ids).clone(), table.types(), table.row_group_size());
                Arc::new(appended) as Arc<dyn crate::catalog::TableSource>
            }
            (None, None) => unreachable!("the rows are read where they aren't in a file"),
        };
        drop(storage);
        log_new_rows(records, table, &source, &ids, first_written, count, ahead)?;
    }
    replayed.fill(count);
    log.replayed = Arc::new(replayed);
    log.logged = ids;
    log.updated.clear();
    log.updated_columns.clear();
    Ok(())
}

/// `DataTable::WriteToLog` of the rows of `table` from `first` on, of `source`, which has all its rows: row groups an
/// append wrote to blocks go in as where they are (`WriteRowGroupData`), and so do the rows in memory after them where
/// the append is a bulk one, of at least a row group (`LocalStorage::Flush`), which a commit writes to blocks now;
/// other rows go in as rows. The table keeps its rows where the log says they are.
fn log_new_rows(
    records: &mut Records,
    table: &TableCatalogEntry,
    source: &Arc<dyn crate::catalog::TableSource>,
    ids: &RowIds,
    first: usize,
    count: usize,
    ahead: Option<WriteAhead>,
) -> Result<()> {
    use super::appended::{AppendedPart, AppendedTable, collection_between};
    let types = table.types();
    // the parts in front of the new rows stay; of those after, the row groups in blocks and the rows in memory
    let mut kept = Vec::new();
    let mut written = Vec::new();
    let mut pending = ColumnDataCollection::new(types.clone());
    let mut position = 0;
    for part in AppendedTable::parts_of(source) {
        let rows = match &part {
            AppendedPart::Source(source) => source.row_count(),
            AppendedPart::Written(group) => group.rows,
            AppendedPart::Memory(memory) => memory.len(),
        };
        let end = position + rows;
        if end <= first {
            kept.push(part);
        } else {
            match part {
                AppendedPart::Written(group) if position >= first && pending.is_empty() => written.push(group),
                AppendedPart::Memory(memory) => {
                    let from = first.saturating_sub(position);
                    if from > 0 {
                        kept.push(AppendedPart::Memory(Arc::new(collection_between(&memory, 0, from))));
                    }
                    pending.append_collection(&collection_between(&memory, from, memory.len()));
                }
                // new rows in a file or behind rows in memory: as rows, which the table has in memory then
                part => {
                    let rows = match &part {
                        AppendedPart::Source(source) => source.read_all()?,
                        AppendedPart::Written(group) => {
                            let one = AppendedTable::new(
                                vec![AppendedPart::Written(group.clone())],
                                RowIds::of_ranges([(group.row_start, group.rows)], group.row_start + group.rows as u64),
                                types.clone(),
                                table.row_group_size(),
                            );
                            crate::catalog::TableSource::read_all(&one)?
                        }
                        AppendedPart::Memory(_) => unreachable!("handled above"),
                    };
                    let from = first.saturating_sub(position);
                    if from > 0 {
                        kept.push(AppendedPart::Memory(Arc::new(collection_between(&rows, 0, from))));
                    }
                    pending.append_collection(&collection_between(&rows, from, rows.len()));
                }
            }
        }
        position = end;
    }
    let first_pending = first + written.iter().map(|group| group.rows).sum::<usize>();
    let bulk = count - first >= table.row_group_size();
    let consecutive =
        pending.is_empty() || ids.id(count - 1) - ids.id(first_pending) == (count - 1 - first_pending) as u64;
    if let (true, true, Some(ahead)) = (bulk, consecutive, ahead)
        && !pending.is_empty()
    {
        let groups = ahead(table, &pending, 0, ids.id(first_pending))?;
        if !groups.is_empty() {
            written.extend(groups.into_iter().map(Arc::new));
            pending = ColumnDataCollection::new(types.clone());
        }
    }
    // `FlushBlocks`: the blocks the append left open are written by now, so that the log can say where its row groups are
    if let (false, Some(ahead)) = (written.is_empty(), ahead) {
        ahead(table, &ColumnDataCollection::new(types.clone()), 0, 0)?;
    }
    if !written.is_empty() {
        records.entry(wal_type::ROW_GROUP_DATA, |s| {
            super::writer::write_row_group_data(s, &types, &written)
        });
    }
    for chunk in pending.chunks() {
        let mut result = Ok(());
        records.entry(wal_type::INSERT_TUPLE, |s| result = write_chunk(s, chunk));
        result?;
    }
    // where the rows are now: the row groups in blocks are kept out of memory
    if !written.is_empty() {
        kept.extend(written.into_iter().map(AppendedPart::Written));
        if !pending.is_empty() {
            kept.push(AppendedPart::Memory(Arc::new(pending)));
        }
        let appended = AppendedTable::new(kept, ids.clone(), types, table.row_group_size());
        table.set_appended(Arc::new(appended));
    }
    Ok(())
}

/// `SerializeIndex`: an index on what the log holds of its table, whose rows have the ids they get
/// when the log is replayed.
fn log_index(
    records: &mut Records,
    info: g::CreateInfo,
    table: &TableCatalogEntry,
    index: &Arc<IndexCatalogEntry>,
    block_size: usize,
) -> Result<()> {
    let mut tree = TableIndex::of_index(table, index, block_size, records.1);
    {
        let log = table.log.lock();
        let mut first = 0;
        for chunk in table.rows()?.chunks() {
            tree.add(chunk, |row| log.replayed.id(first + row) as i64)?;
            first += chunk.len();
        }
    }
    let (storage, buffers) = tree.storage_info_for_log()?;
    records.entry(wal_type::CREATE_INDEX, |s| {
        s.write_property(101, "index_catalog_entry", &Some(Box::new(info)));
        s.write_property(102, "index_storage_info", &storage);
        s.write_list(103, "index_storage", buffers.len(), |s, allocator| {
            for buffer in &buffers[allocator] {
                s.write_varint_unsigned(buffer.len() as u64);
                s.write_raw(buffer);
            }
        });
    });
    Ok(())
}

/// The log of a database file.
#[derive(Default)]
pub struct WriteAheadLog {
    durable: Durable,
    file: Option<File>,
    /// What is in the file.
    size: u64,
    /// The version of the database file, which its log is written in.
    version: StorageVersion,
    /// `wal_entries_count`: the records since the last checkpoint.
    entries: u64,
    /// Of a log that was replayed and isn't written to: where what it committed ends
    replayed: Option<u64>,
    /// of an encrypted file, whose log is encrypted as well
    encryption: Option<super::encryption::FileEncryption>,
}

impl std::fmt::Debug for WriteAheadLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteAheadLog").field("size", &self.size).finish()
    }
}

/// How much the log of the file at `path` holds, which is nothing if there is none.
pub fn log_size(path: &Path) -> u64 {
    std::fs::metadata(log_path(path)).map_or(0, |metadata| metadata.len())
}

fn log_path(path: &Path) -> PathBuf {
    let mut result = path.as_os_str().to_owned();
    result.push(".wal");
    result.into()
}

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{}: {error}", path.display()))
}

impl WriteAheadLog {
    /// All of `catalog` is durable.
    pub fn new(catalog: &Catalog) -> Self {
        Self {
            durable: Durable::of(catalog),
            ..Self::default()
        }
    }

    /// The log of a file in `version`.
    pub fn with_version(mut self, version: StorageVersion) -> Self {
        self.version = version;
        self
    }

    /// The log of a file encrypted as `encryption` says.
    pub fn with_encryption(mut self, encryption: Option<super::encryption::FileEncryption>) -> Self {
        self.encryption = encryption;
        self
    }

    /// `WAL_VERSION_NUMBER`, or `WAL_ENCRYPTED_VERSION_NUMBER` of an encrypted file.
    fn wal_version(&self) -> u64 {
        if self.encryption.is_some() {
            WAL_ENCRYPTED_VERSION
        } else {
            WAL_VERSION
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// A log that was replayed ends with what it committed at `end`, of entries of `version`: a checkpoint marks
    /// it there (`WriteAheadLog::Initialize` of the size replay left), so that a crash between the end of the
    /// checkpoint and the removal of the log leaves a log that says it is in the file.
    pub fn with_replayed(mut self, replayed: Option<(u64, u64)>) -> Self {
        let wanted = self.wal_version();
        self.replayed = replayed.filter(|(_, version)| *version == wanted).map(|(end, _)| end);
        self
    }

    /// `GetWALEntriesCount`.
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// `WriteVersion`: starts a log, the only part of it without a checksum.
    fn open(&mut self, path: &Path, identifier: &[u8; DB_IDENTIFIER_LEN], iteration: u64) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let wal = log_path(path);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&wal)
            .map_err(|error| io_error(&wal, error))?;
        let mut s = BinarySerializer::new(SerializationOptions {
            target: self.version,
            ..Default::default()
        });
        let wal_version = self.wal_version();
        s.write_object_value(|s| {
            s.write_property(100, "wal_type", &wal_type::WAL_VERSION);
            s.write_property(101, "version", &wal_version);
            // which file the log is of, from v1.3.0 on
            if s.should_serialize(StorageVersion::V1_3_0) {
                s.write_list(102, "db_identifier", DB_IDENTIFIER_LEN, |s, i| {
                    identifier[i].write_value(s)
                });
                s.write_property(103, "checkpoint_iteration", &iteration);
            }
        });
        let header = s.into_bytes();
        file.write_all(&header).map_err(|error| io_error(&wal, error))?;
        self.size = header.len() as u64;
        self.file = Some(file);
        Ok(())
    }

    fn append(&mut self, path: &Path, data: &[u8]) -> Result<()> {
        let wal = log_path(path);
        let file = self.file.as_mut().expect("opened first");
        crate::execution::profiler::time_metric("storage.write_to_wal_latency", || {
            file.write_all(data).map_err(|error| io_error(&wal, error))?;
            file.sync_data().map_err(|error| io_error(&wal, error))
        })?;
        self.size += data.len() as u64;
        Ok(())
    }

    /// Fails for what can't be logged. This comes first, as `log_rows` moves on what is durable of a
    /// table: with one that fails after that, the rows of the others would never reach the log.
    fn check_storable(&self, catalog: &Catalog, name: &str, current: &Durable) -> Result<()> {
        fn check_type(ty: &LogicalType) -> Result<()> {
            match ty.physical_type() {
                PhysicalType::Struct => ty.struct_children().iter().try_for_each(|(_, child)| check_type(child)),
                PhysicalType::List | PhysicalType::Array => ty.child_type().map_or(Ok(()), check_type),
                _ => Ok(()),
            }
        }
        for (k, (original, table)) in &current.tables {
            table.types().iter().try_for_each(check_type)?;
            if !self
                .durable
                .tables
                .get(k)
                .is_some_and(|(_, old)| Arc::ptr_eq(old, table))
            {
                table_info(name, &*catalog.schema_for(Some(&original.0))?, table)?;
            }
        }
        for (k, (original, view)) in &current.views {
            if !self.durable.views.get(k).is_some_and(|(_, old)| Arc::ptr_eq(old, view)) {
                view_info(name, &*catalog.schema_for(Some(&original.0))?, view)?;
            }
        }
        for table_macro in [0, 1] {
            for (k, (original, entry)) in &current.macros[table_macro] {
                if !self.durable.macros[table_macro]
                    .get(k)
                    .is_some_and(|(_, old)| Arc::ptr_eq(old, entry))
                {
                    macro_info(name, &*catalog.schema_for(Some(&original.0))?, entry, catalog)?;
                }
            }
        }
        Ok(())
    }

    /// The tables that go and come back: each that changed its entry, as there are no records for
    /// ALTER, and what references one of those, as that keeps it from being dropped.
    fn written_again(&self, current: &Durable) -> BTreeSet<Key> {
        // a table that ALTERs made of this one is the same table to the log
        let changed = |k: &Key, table: &Arc<TableCatalogEntry>| {
            current
                .tables
                .get(k)
                .is_some_and(|(_, new)| !Arc::ptr_eq(new, table) && alters_since(new, table).is_none())
        };
        let mut again: BTreeSet<Key> = self
            .durable
            .tables
            .iter()
            .filter(|(k, (_, table))| changed(k, table))
            .map(|(k, _)| k.clone())
            .collect();
        loop {
            let referencing: Vec<Key> = current
                .tables
                .iter()
                .filter(|(k, (_, table))| {
                    !again.contains(*k)
                        && self.durable.tables.contains_key(*k)
                        && table.constraints.iter().any(|constraint| match constraint {
                            TableConstraint::ForeignKey(foreign) => foreign
                                .referenced
                                .as_ref()
                                .is_some_and(|parent| again.contains(&key(&table.schema, &parent.name))),
                            _ => false,
                        })
                })
                .map(|(k, _)| k.clone())
                .collect();
            if referencing.is_empty() {
                return again;
            }
            again.extend(referencing);
        }
    }

    /// What differs from what is durable, in records. Drops go first, as a name can come back.
    fn differences(
        &self,
        catalog: &Catalog,
        name: &str,
        current: &Durable,
        block_size: usize,
        ahead: Option<WriteAhead>,
    ) -> Result<Records> {
        let mut records = Records(Vec::new(), self.version, 0, self.encryption.clone());
        let schema_of = |key: &Key| -> Result<Arc<SchemaCatalogEntry>> { catalog.schema_for(Some(&key.0)) };
        let gone = |old_same: bool, new_exists: bool| !new_exists || !old_same;
        // before the tables they are on go
        for (k, (original, trigger)) in &self.durable.triggers {
            let same = current
                .triggers
                .get(k)
                .is_some_and(|(_, new)| Arc::ptr_eq(new, trigger));
            if !same {
                records.entry(wal_type::DROP_TRIGGER, |s| {
                    g::WALDropTrigger {
                        schema: original.0.clone(),
                        name: original.1.clone(),
                        table: trigger.table.clone(),
                        qualified_name: path_of(original),
                    }
                    .write_members(s);
                });
            }
        }
        for (k, (original, view)) in &self.durable.views {
            let same = current.views.get(k).is_some_and(|(_, new)| Arc::ptr_eq(new, view));
            if gone(same, current.views.contains_key(k)) {
                records.drop_entry(wal_type::DROP_VIEW, original);
            }
        }
        for (table_macro, wal) in [(0, wal_type::DROP_MACRO), (1, wal_type::DROP_TABLE_MACRO)] {
            for (k, (original, entry)) in &self.durable.macros[table_macro] {
                let same = current.macros[table_macro]
                    .get(k)
                    .is_some_and(|(_, new)| Arc::ptr_eq(new, entry));
                if !same {
                    records.drop_entry(wal, original);
                }
            }
        }
        let again = self.written_again(current);
        let same_table = |k: &Key, table: &Arc<TableCatalogEntry>, tables: &BTreeMap<Key, _>| {
            let tables: &BTreeMap<Key, (Key, Arc<TableCatalogEntry>)> = tables;
            !again.contains(k) && tables.get(k).is_some_and(|(_, other)| Arc::ptr_eq(other, table))
        };
        // `WriteAlter`: the tables that ALTERs made of those the log knows, with the ALTERs since
        let mut altered: BTreeMap<Key, Vec<g::AlterInfo>> = BTreeMap::new();
        for (k, (_, table)) in &current.tables {
            let Some((_, durable)) = self.durable.tables.get(k).filter(|_| !again.contains(k)) else {
                continue;
            };
            if let Some(alters) = alters_since(table, durable) {
                altered.insert(k.clone(), alters);
            }
        }
        for (k, (original, table, index)) in &self.durable.indexes {
            let same = current
                .indexes
                .get(k)
                .is_some_and(|(_, _, new)| Arc::ptr_eq(new, index));
            let table_key = key(&original.0, &table.name);
            if !same && same_table(&table_key, table, &current.tables) {
                records.drop_entry(wal_type::DROP_INDEX, original);
            }
        }
        // those that have a foreign key go first, as what they reference can't go before them
        let tables: Vec<_> = self.durable.tables.iter().collect();
        for position in parents_first(tables.iter().map(|(_, (_, table))| table))
            .into_iter()
            .rev()
        {
            let (k, (original, table)) = tables[position];
            if !same_table(k, table, &current.tables) && !altered.contains_key(k) {
                records.drop_entry(wal_type::DROP_TABLE, original);
            }
        }
        for (k, (original, sequence, _)) in &self.durable.sequences {
            let same = current
                .sequences
                .get(k)
                .is_some_and(|(_, new, _)| Arc::ptr_eq(new, sequence));
            if !same {
                records.drop_entry(wal_type::DROP_SEQUENCE, original);
            }
        }
        for (k, (original, ty)) in &self.durable.types {
            if current.types.get(k).is_none_or(|(_, new)| new != ty) {
                records.drop_entry(wal_type::DROP_TYPE, original);
            }
        }
        for (k, original) in &self.durable.schemas {
            if !current.schemas.contains_key(k) {
                records.schema_entry(wal_type::DROP_SCHEMA, original);
            }
        }
        for (k, original) in &current.schemas {
            if !self.durable.schemas.contains_key(k) {
                // `WriteCreateSchema`: the schema's path
                records.schema_entry(wal_type::CREATE_SCHEMA, original);
            }
        }
        for (k, (original, ty)) in &current.types {
            if self.durable.types.get(k).is_none_or(|(_, old)| old != ty) {
                let info = type_info(name, &*schema_of(original)?, original.1.clone(), ty.0.clone());
                let entry = g::WALCreateType {
                    type_: Some(Box::new(info)),
                };
                records.entry(wal_type::CREATE_TYPE, |s| entry.write_members(s));
            }
        }
        for (k, (original, sequence, usage)) in &current.sequences {
            match self.durable.sequences.get(k) {
                Some((_, old, old_usage)) if Arc::ptr_eq(old, sequence) => {
                    if usage != old_usage {
                        let data = sequence.data();
                        let entry = g::WALSequenceValue {
                            schema: original.0.clone(),
                            name: original.1.clone(),
                            usage_count: Some(data.usage_count),
                            counter: Some(data.counter),
                            last_value: data.last_value,
                            qualified_name: path_of(original),
                        };
                        records.entry(wal_type::SEQUENCE_VALUE, |s| entry.write_members(s));
                    }
                }
                _ => {
                    let info = sequence_info(name, &*schema_of(original)?, sequence);
                    let entry = g::WALCreateSequence {
                        sequence: Some(Box::new(info)),
                    };
                    records.entry(wal_type::CREATE_SEQUENCE, |s| entry.write_members(s));
                }
            }
        }
        for table_macro in [0, 1] {
            for (k, (original, entry)) in &current.macros[table_macro] {
                let same = self.durable.macros[table_macro]
                    .get(k)
                    .is_some_and(|(_, old)| Arc::ptr_eq(old, entry));
                if same {
                    continue;
                }
                let info = Some(Box::new(macro_info(name, &*schema_of(original)?, entry, catalog)?));
                if table_macro == 1 {
                    let entry = g::WALCreateTableMacro { table_macro: info };
                    records.entry(wal_type::CREATE_TABLE_MACRO, |s| entry.write_members(s));
                } else {
                    let entry = g::WALCreateMacro { macro_: info };
                    records.entry(wal_type::CREATE_MACRO, |s| entry.write_members(s));
                }
            }
        }
        let tables: Vec<_> = current.tables.iter().collect();
        for position in parents_first(tables.iter().map(|(_, (_, table))| table)) {
            let (k, (original, table)) = tables[position];
            if let Some(alters) = altered.remove(k) {
                for info in alters {
                    let info = g::ParseInfo {
                        info_type: super::serialization::generated::enums::parse_info_type::ALTER_INFO,
                        kind: g::ParseInfoKind::AlterInfo(info),
                    };
                    records.entry(wal_type::ALTER_INFO, |s| {
                        s.write_property(101, "info", &Some(Box::new(info)))
                    });
                }
            } else if !same_table(k, table, &self.durable.tables) {
                if again.contains(k) {
                    // its rows go in again, under the ids of a new table
                    *table.log.lock() = TableLog::default();
                }
                let info = table_info(name, &*schema_of(original)?, table)?;
                let entry = g::WALCreateTable {
                    table: Some(Box::new(info)),
                };
                records.entry(wal_type::CREATE_TABLE, |s| entry.write_members(s));
            }
            log_rows(&mut records, original, table, ahead)?;
            table.alters.lock().clear();
        }
        for (k, (original, table, index)) in &current.indexes {
            let same = self
                .durable
                .indexes
                .get(k)
                .is_some_and(|(_, old_table, old)| Arc::ptr_eq(old, index) && Arc::ptr_eq(old_table, table))
                && !again.contains(&key(&original.0, &table.name));
            if !same {
                let info = index_info(name, &*schema_of(original)?, table, index)?;
                log_index(&mut records, info, table, index, block_size)?;
            }
        }
        for (k, (original, view)) in &current.views {
            let same = self.durable.views.get(k).is_some_and(|(_, old)| Arc::ptr_eq(old, view));
            if !same {
                let info = view_info(name, &*schema_of(original)?, view)?;
                let entry = g::WALCreateView {
                    view: Some(Box::new(info)),
                };
                records.entry(wal_type::CREATE_VIEW, |s| entry.write_members(s));
            }
        }
        // after the tables they are on, and again with a table that goes and comes back
        for (k, (original, trigger)) in &current.triggers {
            let same = self
                .durable
                .triggers
                .get(k)
                .is_some_and(|(_, old)| Arc::ptr_eq(old, trigger))
                && !again.contains(&key(&original.0, &trigger.table));
            if !same {
                let info = trigger_info(name, &*schema_of(original)?, trigger)?;
                let entry = g::WALCreateTrigger {
                    trigger: Some(Box::new(info)),
                };
                records.entry(wal_type::CREATE_TRIGGER, |s| entry.write_members(s));
            }
        }
        Ok(records)
    }

    /// Makes what `catalog` holds durable: whether there was anything to do.
    #[allow(clippy::too_many_arguments)]
    pub fn commit(
        &mut self,
        path: &Path,
        catalog: &Catalog,
        name: &str,
        identifier: &[u8; DB_IDENTIFIER_LEN],
        iteration: u64,
        block_size: usize,
        ahead: Option<WriteAhead>,
    ) -> Result<bool> {
        let current = Durable::of(catalog);
        self.check_storable(catalog, name, &current)?;
        let mut records = crate::execution::profiler::time_metric("storage.commit_local_storage_latency", || {
            self.differences(catalog, name, &current, block_size, ahead)
        })?;
        self.durable = current;
        if records.0.is_empty() {
            return Ok(false);
        }
        records.entry(wal_type::WAL_FLUSH, |_| {});
        self.open(path, identifier, iteration)?;
        self.append(path, &records.0)?;
        self.entries += records.2;
        Ok(true)
    }

    /// `WriteCheckpoint`: before a checkpoint, for a crash during it to be told from one after it.
    /// Returns what the log held, to go back to if the checkpoint fails.
    pub fn mark_checkpoint(&mut self, path: &Path, root: MetaBlockPointer) -> Result<u64> {
        if self.file.is_none()
            && let Some(end) = self.replayed.take()
        {
            // what follows `end` was never committed, or is a marker of a checkpoint that didn't finish
            let wal = log_path(path);
            let mut file = OpenOptions::new()
                .write(true)
                .open(&wal)
                .map_err(|error| io_error(&wal, error))?;
            file.set_len(end).map_err(|error| io_error(&wal, error))?;
            file.seek(std::io::SeekFrom::End(0))
                .map_err(|error| io_error(&wal, error))?;
            self.file = Some(file);
            self.size = end;
        }
        let size = self.size;
        if self.file.is_some() {
            let mut records = Records(Vec::new(), self.version, 0, self.encryption.clone());
            records.entry(wal_type::CHECKPOINT, |s| s.write_property(101, "meta_block", &root));
            records.entry(wal_type::WAL_FLUSH, |_| {});
            self.append(path, &records.0)?;
        }
        Ok(size)
    }

    /// A checkpoint that failed leaves no marker behind, which only makes sense at the end.
    pub fn unmark_checkpoint(&mut self, path: &Path, size: u64) -> Result<()> {
        if let Some(file) = &self.file {
            file.set_len(size).map_err(|error| io_error(&log_path(path), error))?;
            self.size = size;
        }
        Ok(())
    }

    /// All of `catalog` is in the file now, and the log file is gone.
    pub fn checkpointed(&mut self, catalog: &Catalog) {
        // the file is in the version it was
        let version = self.version;
        let encryption = self.encryption.take();
        *self = Self::new(catalog).with_version(version).with_encryption(encryption);
    }
}

/// `AlterInfo` of an ALTER of a column of `table`, now `altered`, as `WriteAlter` logs it: what the statement said,
/// with the column it added as the table has it, and a cast to the new type for a type changed without USING.
pub(crate) fn alter_record(
    table: &TableCatalogEntry,
    altered: &TableCatalogEntry,
    kind: &duckrs_parser::ast::AlterKind,
) -> Result<g::AlterInfo> {
    use super::serialization::generated::enums::{alter_table_type, alter_type, on_entry_not_found};
    use duckrs_parser::ast::AlterKind;
    let column = |path: &[String]| path.first().cloned().unwrap_or_default();
    let expression = |expr: &duckrs_parser::ast::Expr| -> Result<Option<Box<g::ParsedExpression>>> {
        Ok(Some(Box::new(super::serialization::to_stored::expression(expr)?)))
    };
    let (alter_table_type, kind) = match kind {
        AlterKind::RenameColumn { path, new_name } => (
            alter_table_type::RENAME_COLUMN,
            g::AlterTableInfoKind::RenameColumnInfo(g::RenameColumnInfo {
                old_name: column(path),
                new_name: new_name.clone(),
            }),
        ),
        AlterKind::AddColumn {
            column: added,
            if_not_exists,
            not_null,
            unique,
        } => {
            let columns = altered.logical_columns();
            let stored = columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&added.name))
                .ok_or_else(|| write_not_ported("an ADD COLUMN of a column the table doesn't have"))?;
            (
                alter_table_type::ADD_COLUMN,
                g::AlterTableInfoKind::AddColumnInfo(g::AddColumnInfo {
                    new_column: super::writer::stored_column(stored)?,
                    if_column_not_exists: *if_not_exists,
                    add_not_null: *not_null,
                    add_unique: *unique,
                }),
            )
        }
        AlterKind::RemoveColumn {
            path,
            if_exists,
            cascade,
        } => (
            alter_table_type::REMOVE_COLUMN,
            g::AlterTableInfoKind::RemoveColumnInfo(g::RemoveColumnInfo {
                removed_column: column(path),
                if_column_exists: *if_exists,
                cascade: *cascade,
            }),
        ),
        AlterKind::ChangeColumnType { path, using, .. } => {
            let name = column(path);
            let target = altered
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&name))
                .map(|c| c.ty.clone())
                .ok_or_else(|| write_not_ported("an ALTER TYPE of a column the table doesn't have"))?;
            // `BindAlterTypes`: without USING, the column cast to the type
            let using = match using {
                Some(using) => (**using).clone(),
                None => super::writer::parse_expression(&format!(
                    "CAST({} AS {target})",
                    duckrs_parser::ast::sql_identifier(&name)
                ))?,
            };
            (
                alter_table_type::ALTER_COLUMN_TYPE,
                g::AlterTableInfoKind::ChangeColumnTypeInfo(g::ChangeColumnTypeInfo {
                    column_name: name,
                    target_type: target,
                    expression: expression(&using)?,
                    column_path: Vec::new(),
                }),
            )
        }
        AlterKind::SetDefault { path, expression: set } => (
            alter_table_type::SET_DEFAULT,
            g::AlterTableInfoKind::SetDefaultInfo(g::SetDefaultInfo {
                column_name: column(path),
                expression: match set {
                    Some(set) => expression(set)?,
                    None => None,
                },
                column_path: Vec::new(),
            }),
        ),
        AlterKind::SetNotNull(path) => (
            alter_table_type::SET_NOT_NULL,
            g::AlterTableInfoKind::SetNotNullInfo(g::SetNotNullInfo {
                column_name: column(path),
                column_path: Vec::new(),
            }),
        ),
        AlterKind::DropNotNull(path) => (
            alter_table_type::DROP_NOT_NULL,
            g::AlterTableInfoKind::DropNotNullInfo(g::DropNotNullInfo {
                column_name: column(path),
                column_path: Vec::new(),
            }),
        ),
        _ => return Err(write_not_ported("this ALTER in a write-ahead log")),
    };
    let mut path = vec![table.catalog.clone()];
    path.extend(schema_path(&table.schema));
    path.push(table.name.clone());
    Ok(g::AlterInfo {
        type_: alter_type::ALTER_TABLE,
        catalog: table.catalog.clone(),
        schema: table.schema.clone(),
        name: table.name.clone(),
        if_not_found: on_entry_not_found::THROW_EXCEPTION,
        allow_internal: false,
        qualified_name: Some(QualifiedName { path }),
        kind: g::AlterInfoKind::AlterTableInfo(g::AlterTableInfo { alter_table_type, kind }),
    })
}

/// The ALTERs that made `table` of `durable`, oldest first, if it was made of it that way.
fn alters_since(table: &Arc<TableCatalogEntry>, durable: &Arc<TableCatalogEntry>) -> Option<Vec<g::AlterInfo>> {
    if Arc::ptr_eq(table, durable) {
        return None;
    }
    let alters = table.alters.lock();
    let from = alters
        .iter()
        .position(|alter| std::ptr::eq(alter.from.as_ptr(), Arc::as_ptr(durable)))?;
    Some(alters[from..].iter().map(|alter| alter.info.clone()).collect())
}
