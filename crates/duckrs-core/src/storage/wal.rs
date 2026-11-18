//! Ported from duckdb/src/storage/wal_replay.cpp and the `Deserialize` of duckdb/src/common/types/
//! {data_chunk,vector}.cpp @ rust-port-baseline.
//!
//! Reads the write-ahead log of a database file and applies what was committed. Files are only
//! opened read-only for now, so of the C++ replay this is the read-only path: logs stay as they are
//! and a checkpoint that didn't finish is not completed, only read around.
//!
//! All records are read before any is applied. What follows the last WAL_FLUSH was never committed,
//! and a log that stops halfway through a record is what a crash leaves, so neither is an error.

use super::block_manager::{DB_IDENTIFIER_LEN, corrupt};
use super::buffer_manager::BufferManager;
use super::checkpoint::{self, Converted, StoredEntry, StoredSequence, StoredTable};
use super::checksum::checksum;
use super::metadata::MetaBlockPointer;
use super::serialization::custom::QualifiedName;
use super::serialization::generated::enums::{vector_type, wal_type};
use super::serialization::generated::{self as g};
use super::serialization::not_ported;
use super::table_data::{self, DiskTable, PersistentRowGroup, fill_vector, fixed_width};
use crate::catalog::{Catalog, OnCreateConflict, RowIds, SequenceData};
use crate::planner::binder::Binder;
use crate::planner::logical::LogicalOperatorKind;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::{ListEntry, LogicalType, PhysicalType};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `WAL_VERSION_NUMBER`: entries have their size and a checksum in front.
const FRAMED_VERSION: u64 = 2;
/// `WAL_ENCRYPTED_VERSION_NUMBER`.
const ENCRYPTED_VERSION: u64 = 3;
const FRAME_HEADER_SIZE: usize = 16;
const STANDARD_VECTOR_SIZE: usize = 2048;

/// Schema and name of an entry, as a record gives them.
type EntryName = (String, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DropKind {
    Table,
    View,
    Sequence,
    Macro,
    TableMacro,
    Type,
    Index,
    /// a trigger, on the table of this name
    Trigger(String),
}

/// A record that changes the database.
pub(super) enum Record {
    Create(Box<StoredEntry>),
    /// the schemas from the outermost down to it
    CreateSchema(Vec<String>),
    DropSchema(Vec<String>),
    Drop(DropKind, EntryName),
    SequenceValue {
        name: EntryName,
        usage_count: u64,
        counter: i64,
        last_value: Option<i64>,
    },
    UseTable(EntryName),
    Insert(DataChunk),
    RowGroups(Vec<PersistentRowGroup>),
    Delete(DataChunk),
    /// C++ logs an update twice: the values, and with `validity` which of them are NULL.
    Update {
        column: usize,
        validity: bool,
        chunk: DataChunk,
    },
}

/// What reading an entry leaves besides a `Record`.
enum Entry {
    Record(Record),
    Version,
    Checkpoint(MetaBlockPointer),
    Flush,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Main,
    Checkpoint,
}

/// What a file says that a log has to agree with.
struct FileState {
    db_identifier: [u8; DB_IDENTIFIER_LEN],
    iteration: u64,
    before_v2: bool,
}

/// `ThrowVersionError`.
fn version_error(wal_iteration: u64, file_iteration: u64) -> Error {
    let relation = if wal_iteration < file_iteration {
        "an older"
    } else {
        "a newer"
    };
    Error::new(
        ErrorKind::Io,
        format!(
            "WAL file does not match the database file. That means the WAL was created for {relation} version of \
             this database. File checkpoint iteration: {file_iteration}, WAL checkpoint iteration: {wal_iteration}"
        ),
    )
}

/// `QualifiedName` of a record, or the schema and name of logs from before v2.0.0.
fn entry_name(schema: String, name: String, path: Option<QualifiedName>) -> Result<EntryName> {
    let path = path.map(|name| name.path).unwrap_or_default();
    match path.as_slice() {
        [] => Ok((schema, name)),
        [schema, name] => Ok((schema.clone(), name.clone())),
        // [schema path..., name] of an entry in a nested schema
        [schemas @ .., name] if !schemas.is_empty() => Ok((crate::catalog::schema_key(schemas), name.clone())),
        _ => Err(corrupt("a log entry names no schema")),
    }
}

fn create(info: Option<Box<g::CreateInfo>>) -> Result<Record> {
    let info = *info.ok_or_else(|| corrupt("a log entry has no create info"))?;
    Ok(Record::Create(Box::new(match checkpoint::convert_entry(info)? {
        Converted::Entry(entry) => entry,
        Converted::Table(table) => StoredEntry::Table(table),
    })))
}

/// `ReplayCreateSchema`: the path of the schema, of which old logs only have the name.
fn schema_name(schema: String, path: Option<QualifiedName>) -> Result<Vec<String>> {
    let path = path.map(|name| name.path).unwrap_or_default();
    Ok(if path.is_empty() { vec![schema] } else { path })
}

/// The value of each row of a vector with an unknown number of them: the mask comes first.
fn is_valid(mask: Option<&[u8]>, row: usize) -> bool {
    mask.is_none_or(|mask| mask[row / 8] >> (row % 8) & 1 != 0)
}

/// `Vector::Deserialize`, always to a flat vector.
fn read_vector(d: &mut BinaryDeserializer<'_>, ty: &LogicalType, count: usize) -> Result<Vector> {
    let kind = d
        .read_optional_property::<u64>(90, "vector_type")?
        .unwrap_or(vector_type::FLAT_VECTOR);
    match kind {
        vector_type::FLAT_VECTOR => read_flat_vector(d, ty, count),
        vector_type::CONSTANT_VECTOR => {
            let mut vector = read_flat_vector(d, ty, 1)?;
            vector.slice(&SelectionVector::from_vec(vec![0; count]), count);
            vector.flatten(count);
            Ok(vector)
        }
        vector_type::DICTIONARY_VECTOR => {
            let selection = d.read_blob_property(91)?;
            if selection.len() != count * 4 {
                return Err(corrupt("a dictionary has a selection of the wrong size"));
            }
            let dict_count: u64 = d.read_property(92, "dict_count")?;
            let selection: Vec<u32> = selection
                .chunks_exact(4)
                .map(|index| u32::from_le_bytes(index.try_into().expect("4 bytes")))
                .collect();
            if selection.iter().any(|&index| u64::from(index) >= dict_count) {
                return Err(corrupt("a dictionary selection is out of range"));
            }
            let mut vector = read_vector(d, ty, dict_count as usize)?;
            vector.slice(&SelectionVector::from_vec(selection), count);
            vector.flatten(count);
            Ok(vector)
        }
        vector_type::SEQUENCE_VECTOR => {
            let start: i64 = d.read_property(91, "seq_start")?;
            let increment: i64 = d.read_property(92, "seq_increment")?;
            let width = fixed_width(ty.physical_type()).ok_or_else(|| corrupt("a sequence of another type"))?;
            let mut data = Vec::with_capacity(count * width);
            for row in 0..count as i128 {
                let value = i128::from(start) + i128::from(increment) * row;
                data.extend_from_slice(&value.to_le_bytes()[..width]);
            }
            let mut vector = Vector::new(ty.clone(), count);
            fill_vector(&mut vector, &data)?;
            Ok(vector)
        }
        other => Err(not_ported(&format!("a vector of type {other}"))),
    }
}

/// Lengths and bytes of strings, or a list of them in logs of old versions.
fn read_strings(d: &mut BinaryDeserializer<'_>, ty: &LogicalType, count: usize, mask: Option<&[u8]>) -> Result<Vector> {
    let mut strings: Vec<Option<&[u8]>> = Vec::with_capacity(count.min(STANDARD_VECTOR_SIZE));
    if d.has_property(107)? {
        let byte_length: u64 = d.read_property(107, "byte_data_length")?;
        let lengths = d.read_blob_property(108)?;
        let mut bytes = d.read_blob_property(109)?;
        if lengths.len() != count * 4 || bytes.len() as u64 != byte_length {
            return Err(corrupt("strings of the wrong size"));
        }
        for (row, length) in lengths.chunks_exact(4).enumerate() {
            if !is_valid(mask, row) {
                strings.push(None);
                continue;
            }
            let length = u32::from_le_bytes(length.try_into().expect("4 bytes")) as usize;
            let (string, rest) = bytes
                .split_at_checked(length)
                .ok_or_else(|| corrupt("string lengths exceed the string data"))?;
            strings.push(Some(string));
            bytes = rest;
        }
    } else {
        let mut row = 0;
        let read = d.read_list(102, "data", |d| {
            let string = d.read_blob()?;
            row += 1;
            Ok(is_valid(mask, row - 1).then_some(string))
        })?;
        strings = read;
    }
    if strings.len() != count {
        return Err(corrupt("a vector has the wrong number of strings"));
    }
    let mut vector = Vector::new(ty.clone(), count);
    for (row, string) in strings.into_iter().enumerate() {
        if let Some(string) = string {
            vector.set_str(row, string);
        }
    }
    Ok(vector)
}

fn read_list_vector(
    d: &mut BinaryDeserializer<'_>,
    ty: &LogicalType,
    count: usize,
    mask: Option<&[u8]>,
) -> Result<Vector> {
    let list_size: u64 = d.read_property(104, "list_size")?;
    let mut entries = Vec::with_capacity(count.min(STANDARD_VECTOR_SIZE));
    let mut row = 0;
    d.read_list(105, "entries", |d| {
        let (offset, length): (u64, u64) =
            d.read_object_value(|d| Ok((d.read_property(100, "offset")?, d.read_property(101, "length")?)))?;
        let valid = is_valid(mask, row);
        row += 1;
        if valid && (offset > list_size || length > list_size - offset) {
            return Err(corrupt("a list entry is out of range"));
        }
        entries.push(if valid {
            ListEntry { offset, length }
        } else {
            ListEntry { offset: 0, length: 0 }
        });
        Ok(())
    })?;
    if entries.len() != count {
        return Err(corrupt("a vector has the wrong number of list entries"));
    }
    let child_type = ty.child_type().ok_or_else(|| corrupt("a list has no child type"))?;
    let child = d.read_object(106, "child", |d| read_vector(d, child_type, list_size as usize))?;
    let mut vector = Vector::new(ty.clone(), count);
    vector.data_mut::<ListEntry>()[..count].copy_from_slice(&entries);
    vector.set_list_size(list_size as usize);
    *vector.list_child_mut() = child;
    Ok(vector)
}

fn read_flat_vector(d: &mut BinaryDeserializer<'_>, ty: &LogicalType, count: usize) -> Result<Vector> {
    if ty.id() == duckrs_common::types::LogicalTypeId::Variant {
        let stored = read_flat_vector(d, &duckrs_common::variant::unshredded_type(), count)?;
        return duckrs_common::variant::from_unshredded_vector(&stored, ty, count);
    }
    // `Vector::Deserialize` of a GEOMETRY: WKB, or the spatial extension's format before v1.5.0
    if ty.id() == duckrs_common::types::LogicalTypeId::Geometry {
        const WKB: u8 = 1;
        const SPATIAL: u8 = 0;
        match d
            .read_optional_property::<u8>(99, "geometry_format")?
            .unwrap_or(SPATIAL)
        {
            WKB => {}
            SPATIAL => {
                // `Geometry::FromSpatialGeometry` of each blob
                let blobs = read_flat_vector(d, &LogicalType::BLOB, count)?;
                let mut result = Vector::new(ty.clone(), count.max(1));
                for row in 0..count {
                    let value = blobs.get_value(row);
                    match value.data() {
                        duckrs_common::value::ValueData::Bytes(blob) if !value.is_null() => {
                            let wkb = duckrs_common::geometry::from_spatial(blob)?;
                            result.set_value(
                                row,
                                &Value::new(ty.clone(), duckrs_common::value::ValueData::Bytes(wkb))?,
                            )?;
                        }
                        _ => result.set_null(row, true),
                    }
                }
                return Ok(result);
            }
            _ => return Err(not_ported("a shredded GEOMETRY in a write-ahead log")),
        }
    } else if d.has_property(99)? {
        return Err(not_ported("extra vector data in a write-ahead log"));
    }
    let has_mask: bool = d.read_property(100, "has_validity_mask")?;
    let mask = if has_mask {
        let mask = d.read_blob_property(101)?;
        if mask.len() < count.div_ceil(8) {
            return Err(corrupt("a validity mask is too small"));
        }
        Some(mask)
    } else {
        None
    };
    let physical = ty.physical_type();
    let mut vector = match physical {
        PhysicalType::Varchar => read_strings(d, ty, count, mask)?,
        PhysicalType::List => read_list_vector(d, ty, count, mask)?,
        PhysicalType::Struct => {
            let mut fields = ty.struct_children().iter();
            let children = d.read_list(103, "children", |d| {
                let (_, field) = fields.next().ok_or_else(|| corrupt("a struct has too many children"))?;
                d.read_object_value(|d| read_vector(d, field, count))
            })?;
            if fields.next().is_some() {
                return Err(corrupt("a struct has too few children"));
            }
            let mut vector = Vector::new(ty.clone(), count);
            for (target, child) in vector.struct_children_mut().iter_mut().zip(children) {
                *target = child;
            }
            vector
        }
        PhysicalType::Array => {
            let size: u64 = d.read_property(103, "array_size")?;
            if ty.array_size().map(u64::from) != Some(size) {
                return Err(corrupt("an array of the wrong size"));
            }
            let child_rows = (size as usize)
                .checked_mul(count)
                .ok_or_else(|| corrupt("an array is too large"))?;
            let child_type = ty.child_type().ok_or_else(|| corrupt("an array has no child type"))?;
            let child = d.read_object(104, "child", |d| read_vector(d, child_type, child_rows))?;
            let mut vector = Vector::new(ty.clone(), count);
            *vector.array_child_mut() = child;
            vector
        }
        _ => {
            let width = fixed_width(physical).ok_or_else(|| not_ported(&format!("{ty} in a write-ahead log")))?;
            let data = d.read_blob_property(102)?;
            if Some(data.len()) != count.checked_mul(width) {
                return Err(corrupt("a vector has the wrong size"));
            }
            let mut vector = Vector::new(ty.clone(), count);
            fill_vector(&mut vector, data)?;
            vector
        }
    };
    for row in (0..count).filter(|&row| !is_valid(mask, row)) {
        vector.set_null(row, true);
    }
    Ok(vector)
}

/// A chunk from the bytes of [`super::log_writer::serialize_chunk`].
pub(crate) fn deserialize_chunk(bytes: &[u8]) -> Result<DataChunk> {
    read_chunk(&mut BinaryDeserializer::new(bytes), 100)
}

/// `DataChunk::Deserialize`.
fn read_chunk(d: &mut BinaryDeserializer<'_>, field_id: u16) -> Result<DataChunk> {
    d.read_object(field_id, "chunk", |d| {
        let rows: u32 = d.read_property(100, "rows")?;
        let types = d.read_list(101, "types", LogicalType::read_value)?;
        let mut types_left = types.iter();
        let columns = d.read_list(102, "columns", |d| {
            let ty = types_left
                .next()
                .ok_or_else(|| corrupt("a chunk has too many columns"))?;
            d.read_object_value(|d| read_vector(d, ty, rows as usize))
        })?;
        if columns.len() != types.len() || columns.is_empty() {
            return Err(corrupt("a chunk has the wrong number of columns"));
        }
        Ok(DataChunk::from_columns(columns, rows as usize))
    })
}

/// `ReplayIndexData`: the ART, which isn't needed here (see "Indexes" in `docs/ARCHITECTURE.md`).
fn skip_index_data(d: &mut BinaryDeserializer<'_>, info: &g::IndexStorageInfo) -> Result<()> {
    let mut allocators = info.allocator_infos.iter();
    d.read_list(103, "index_storage", |d| {
        let allocator = allocators.next().ok_or_else(|| corrupt("too much index data"))?;
        for _ in &allocator.allocation_sizes {
            d.read_blob()?;
        }
        Ok(())
    })
    .map(drop)
}

struct Reader<'a> {
    file: &'a FileState,
    kind: LogKind,
    version: u64,
    /// `expected_checkpoint_id`: the log is one checkpoint old, which has to be the crash between
    /// the end of a checkpoint and the truncation of its log.
    expected_iteration: Option<u64>,
}

impl Reader<'_> {
    /// `ReplayVersion`.
    fn read_version(&mut self, d: &mut BinaryDeserializer<'_>) -> Result<()> {
        self.version = d.read_property(101, "version")?;
        if self.version == ENCRYPTED_VERSION {
            return Err(not_ported("an encrypted write-ahead log"));
        }
        if self.version > ENCRYPTED_VERSION {
            let version = self.version;
            return Err(Error::new(
                ErrorKind::Io,
                format!("Failed to read WAL of version {version} - can only read version 1, 2 and 3 (encrypted)"),
            ));
        }
        let identifier: Vec<u8> = d.read_property_with_default(102, "db_identifier")?;
        let iteration: Option<u64> = d.read_optional_property(103, "checkpoint_iteration")?;
        let Some(iteration) = iteration.filter(|_| !identifier.is_empty()) else {
            return Ok(());
        };
        if identifier != self.file.db_identifier {
            return Err(Error::new(ErrorKind::Io, "WAL does not match database file."));
        }
        let expected = self.file.iteration;
        if iteration == expected {
            return Ok(());
        }
        if iteration + 1 == expected {
            self.expected_iteration = Some(expected);
            return Ok(());
        }
        if self.kind == LogKind::Checkpoint && iteration == expected + 1 {
            return Ok(());
        }
        Err(version_error(iteration, expected))
    }

    /// `ReplayEntry`.
    fn read_entry(&mut self, d: &mut BinaryDeserializer<'_>) -> Result<Entry> {
        d.read_object_value(|d| {
            let ty: u64 = d.read_property(100, "wal_type")?;
            self.read_body(d, ty)
        })
    }

    fn read_body(&mut self, d: &mut BinaryDeserializer<'_>, ty: u64) -> Result<Entry> {
        use wal_type::*;
        let record = match ty {
            WAL_VERSION => {
                self.read_version(d)?;
                return Ok(Entry::Version);
            }
            WAL_FLUSH => return Ok(Entry::Flush),
            CHECKPOINT => return Ok(Entry::Checkpoint(d.read_property(101, "meta_block")?)),
            CREATE_TABLE => create(g::WALCreateTable::read_members(d)?.table)?,
            CREATE_VIEW => create(g::WALCreateView::read_members(d)?.view)?,
            CREATE_SEQUENCE => create(g::WALCreateSequence::read_members(d)?.sequence)?,
            CREATE_MACRO => create(g::WALCreateMacro::read_members(d)?.macro_)?,
            CREATE_TABLE_MACRO => create(g::WALCreateTableMacro::read_members(d)?.table_macro)?,
            CREATE_TYPE => create(g::WALCreateType::read_members(d)?.type_)?,
            CREATE_TRIGGER => create(g::WALCreateTrigger::read_members(d)?.trigger)?,
            DROP_TRIGGER => {
                let e = g::WALDropTrigger::read_members(d)?;
                Record::Drop(
                    DropKind::Trigger(e.table),
                    entry_name(e.schema, e.name, e.qualified_name)?,
                )
            }
            CREATE_SCHEMA => {
                let entry = g::WALCreateSchema::read_members(d)?;
                Record::CreateSchema(schema_name(entry.schema, entry.qualified_name)?)
            }
            DROP_SCHEMA => {
                let entry = g::WALDropSchema::read_members(d)?;
                Record::DropSchema(schema_name(entry.schema, entry.qualified_name)?)
            }
            CREATE_INDEX => {
                let info: Option<Box<g::CreateInfo>> = d.read_property(101, "index_catalog_entry")?;
                let storage: g::IndexStorageInfo = d.read_property(102, "index_storage_info")?;
                skip_index_data(d, &storage)?;
                create(info)?
            }
            DROP_TABLE => {
                let e = g::WALDropTable::read_members(d)?;
                Record::Drop(DropKind::Table, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_VIEW => {
                let e = g::WALDropView::read_members(d)?;
                Record::Drop(DropKind::View, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_SEQUENCE => {
                let e = g::WALDropSequence::read_members(d)?;
                Record::Drop(DropKind::Sequence, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_MACRO => {
                let e = g::WALDropMacro::read_members(d)?;
                Record::Drop(DropKind::Macro, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_TABLE_MACRO => {
                let e = g::WALDropTableMacro::read_members(d)?;
                Record::Drop(DropKind::TableMacro, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_TYPE => {
                let e = g::WALDropType::read_members(d)?;
                Record::Drop(DropKind::Type, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            DROP_INDEX => {
                let e = g::WALDropIndex::read_members(d)?;
                Record::Drop(DropKind::Index, entry_name(e.schema, e.name, e.qualified_name)?)
            }
            SEQUENCE_VALUE => {
                let e = g::WALSequenceValue::read_members(d)?;
                Record::SequenceValue {
                    name: entry_name(e.schema, e.name, e.qualified_name)?,
                    usage_count: e.usage_count.unwrap_or(0),
                    counter: e.counter.unwrap_or(0),
                    last_value: e.last_value,
                }
            }
            USE_TABLE => {
                let e = g::WALUseTable::read_members(d)?;
                Record::UseTable(entry_name(e.schema, e.table, e.qualified_name)?)
            }
            INSERT_TUPLE => Record::Insert(read_chunk(d, 101)?),
            ROW_GROUP_DATA => Record::RowGroups(table_data::read_collection_data(d, 101)?),
            DELETE_TUPLE => Record::Delete(read_chunk(d, 101)?),
            UPDATE_TUPLE => {
                let path: Vec<u64> = d.read_property(101, "column_indexes")?;
                let chunk = read_chunk(d, 102)?;
                let (column, validity) = match *path.as_slice() {
                    [column] => (column, false),
                    // the first child of a column is its validity
                    [column, 0] => (column, true),
                    _ => return Err(not_ported("an update of a nested column in a write-ahead log")),
                };
                if chunk.column_count() != 2 {
                    return Err(corrupt("an update needs the values and the row ids"));
                }
                Record::Update {
                    column: column as usize,
                    validity,
                    chunk,
                }
            }
            ALTER_INFO => return Err(not_ported("ALTER in a write-ahead log")),
            other => return Err(corrupt(format!("unknown write-ahead log entry {other}"))),
        };
        Ok(Entry::Record(record))
    }

    fn deserializer<'b>(&self, data: &'b [u8]) -> BinaryDeserializer<'b> {
        checkpoint::catalog_deserializer(data, self.file.before_v2)
    }

    /// `GetEntryDeserializer` and `ReplayEntry`: the next entry, and where the one after it starts.
    fn next(&mut self, data: &[u8], position: usize) -> Result<(Entry, usize)> {
        if self.version < FRAMED_VERSION {
            let mut d = self.deserializer(&data[position..]);
            let entry = self.read_entry(&mut d)?;
            return Ok((entry, position + d.position()));
        }
        let header = data
            .get(position..position + FRAME_HEADER_SIZE)
            .ok_or_else(|| torn("a log ends inside the header of an entry"))?;
        let size = u64::from_le_bytes(header[..8].try_into().expect("8 bytes"));
        let stored = u64::from_le_bytes(header[8..].try_into().expect("8 bytes"));
        let start = position + FRAME_HEADER_SIZE;
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .filter(|&end| end <= data.len() && size != 0)
            .ok_or_else(|| torn("an entry exceeds the data in the log"))?;
        let frame = &data[start..end];
        let computed = checksum(frame);
        if stored != computed {
            return Err(Error::new(
                ErrorKind::DataCorruption,
                format!(
                    "Corrupt WAL file: entry at byte position {start} computed checksum {computed} does not match \
                     stored checksum {stored}"
                ),
            ));
        }
        let mut d = self.deserializer(frame);
        let entry = self.read_entry(&mut d)?;
        Ok((entry, end))
    }
}

/// What C++ takes for a log that a crash cut off.
fn torn(message: &str) -> Error {
    Error::new(ErrorKind::Serialization, message)
}

/// The records of one log that were committed.
struct Scan {
    records: Vec<Record>,
    /// With where it ended, if the marker of a checkpoint was read in full.
    checkpoint: Option<(MetaBlockPointer, usize)>,
    /// Where the last entry started, or would have.
    last_position: usize,
    expected_iteration: Option<u64>,
}

fn scan(data: &[u8], file: &FileState, kind: LogKind) -> Result<Scan> {
    let mut reader = Reader {
        file,
        kind,
        version: 1,
        expected_iteration: None,
    };
    let mut records = Vec::new();
    let mut committed = 0;
    let mut position = 0;
    let mut last_position;
    let mut checkpoint = None;
    loop {
        last_position = position;
        match reader.next(data, position) {
            Ok((entry, next)) => {
                position = next;
                match entry {
                    Entry::Record(record) => records.push(record),
                    Entry::Version => {}
                    Entry::Checkpoint(_) if checkpoint.is_some() => {
                        return Err(corrupt("a log has more than one checkpoint marker"));
                    }
                    Entry::Checkpoint(pointer) => checkpoint = Some((pointer, next)),
                    Entry::Flush => {
                        committed = records.len();
                        if next == data.len() {
                            break;
                        }
                    }
                }
            }
            // what a crash leaves at the end of a log
            Err(error) if error.kind == ErrorKind::Serialization => break,
            Err(error) => return Err(error),
        }
    }
    records.truncate(committed);
    Ok(Scan {
        records,
        checkpoint,
        last_position,
        expected_iteration: reader.expected_iteration,
    })
}

fn read_log(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(data) if data.is_empty() => Ok(None),
        Ok(data) => Ok(Some(data)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::new(ErrorKind::Io, format!("{}: {error}", path.display()))),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut result = path.as_os_str().to_owned();
    result.push(suffix);
    result.into()
}

/// `ReplayLog` of a connection that can't change the log.
fn replay_log(
    data: &[u8],
    path: &Path,
    buffer: &BufferManager,
    file: &FileState,
    kind: LogKind,
) -> Result<Vec<Record>> {
    let scanned = scan(data, file, kind)?;
    let mut after = None;
    if let Some((marker, end)) = scanned.checkpoint {
        if kind == LogKind::Checkpoint {
            return Err(corrupt("a checkpoint WAL cannot contain a checkpoint marker"));
        }
        if scanned.last_position != end {
            return Err(corrupt("WAL checkpoint marker must be at the end of the WAL"));
        }
        let clean = marker.block_pointer == buffer.block_manager().header().meta_block;
        let checkpoint_log = read_log(&with_suffix(path, ".wal.checkpoint"))?;
        match (checkpoint_log, clean) {
            (None, true) => return Ok(Vec::new()),
            (None, false) => {}
            (Some(log), true) => return replay_log(&log, path, buffer, file, LogKind::Checkpoint),
            (Some(log), false) => after = Some(log),
        }
    }
    if let Some(expected) = scanned.expected_iteration {
        return Err(version_error(expected - 1, expected));
    }
    let mut records = scanned.records;
    if let Some(log) = after {
        records.extend(replay_log(&log, path, buffer, file, LogKind::Checkpoint)?);
    }
    Ok(records)
}

/// `WriteAheadLog::Replay`: the committed records of the log of the file at `path`.
pub(super) fn read_records(path: &Path, buffer: &BufferManager) -> Result<Vec<Record>> {
    let Some(data) = read_log(&with_suffix(path, ".wal"))? else {
        return Ok(Vec::new());
    };
    let header = buffer.block_manager().header();
    let file = FileState {
        db_identifier: buffer.block_manager().main_header().db_identifier,
        iteration: header.iteration,
        before_v2: checkpoint::is_before_v2(&header),
    };
    replay_log(&data, path, buffer, &file, LogKind::Main)
}

/// A table while a log is replayed. Row ids are what records name rows by, and deleting rows in
/// the file leaves gaps: `row_ids[position]` is the id of the row at that position of the table.
#[derive(Default)]
struct LiveTable {
    row_ids: Vec<u64>,
    next_row_id: u64,
    deleted: HashSet<usize>,
    /// By position, then column.
    updates: HashMap<usize, HashMap<usize, UpdatedCell>>,
}

/// The last update of a cell. The value of a NULL is whatever the vector held, so `null` decides.
#[derive(Default)]
struct UpdatedCell {
    value: Option<Value>,
    null: Option<bool>,
}

impl LiveTable {
    fn position(&self, row_id: i64) -> Result<usize> {
        u64::try_from(row_id)
            .ok()
            .and_then(|row_id| self.row_ids.binary_search(&row_id).ok())
            .ok_or_else(|| corrupt("a log names a row that does not exist"))
    }

    fn add_rows(&mut self, count: usize) {
        let next = self.next_row_id + count as u64;
        self.row_ids.extend(self.next_row_id..next);
        self.next_row_id = next;
    }
}

fn key(name: &EntryName) -> EntryName {
    (name.0.to_lowercase(), name.1.to_lowercase())
}

/// Puts entries in the catalog: those of a checkpoint, then those of a log.
pub(super) struct Replay<'a> {
    buffer: Arc<BufferManager>,
    catalog: &'a Catalog,
    before_v2: bool,
    /// The tables of the file, of which only those that a log changes get read.
    disk: HashMap<EntryName, Arc<DiskTable>>,
    tables: HashMap<EntryName, LiveTable>,
    sequence_usage: HashMap<EntryName, u64>,
    current: Option<EntryName>,
    /// The `storage_version` of each index whose ART the file has, by schema and name.
    index_versions: HashMap<EntryName, Option<u64>>,
}

impl<'a> Replay<'a> {
    pub(super) fn new(buffer: Arc<BufferManager>, catalog: &'a Catalog) -> Self {
        let before_v2 = checkpoint::is_before_v2(&buffer.block_manager().header());
        Self {
            buffer,
            catalog,
            before_v2,
            disk: HashMap::new(),
            tables: HashMap::new(),
            sequence_usage: HashMap::new(),
            current: None,
            index_versions: HashMap::new(),
        }
    }

    /// Binds the definition of a table as `CREATE TABLE` does, for its defaults, generated columns
    /// and constraints. Its rows stay in the file.
    fn load_table(&mut self, table: StoredTable) -> Result<()> {
        // `ART::ART` of an index with storage: the version it has, if it says
        for index in &table.indexes {
            let version = index
                .options
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("storage_version"))
                .and_then(|(_, value)| value.as_i128())
                .map(|version| version as u64);
            self.index_versions
                .insert(key(&(table.schema.clone(), index.name.clone())), version);
        }
        let plan = Binder::new(self.catalog).bind_stored_table(&table.definition, &table.schema)?;
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
        let table_key = key(&(table.schema.clone(), table.name.clone()));
        let (comment, column_comments) = (table.comment.clone(), table.column_comments.clone());
        let mut columns = columns;
        for column in &mut columns {
            column.comment = column_comments
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&column.name))
                .map(|(_, c)| c.clone());
        }
        let disk = Arc::new(DiskTable::new(self.buffer.clone(), table)?);
        let entry = schema
            .create_table(&name, columns, generated, constraints, on_conflict)?
            .expect("OnCreateConflict::Error fails or creates");
        *entry.comment.lock() = comment;
        entry.set_source(disk.clone());
        // the deleted rows of the file are known only by scanning it
        let ids = crate::catalog::TableSource::row_ids(&*disk);
        let stored: usize = ids.stored_groups().iter().map(|&(_, rows)| rows).sum();
        entry
            .unloaded_deletes
            .store(stored > ids.len(), std::sync::atomic::Ordering::Relaxed);
        entry.reset_log();
        self.disk.insert(table_key, disk);
        Ok(())
    }

    /// A table that a log changes, whose rows that takes in memory.
    fn live_table(&mut self, name: &EntryName) -> Result<&mut LiveTable> {
        let table_key = key(name);
        if !self.tables.contains_key(&table_key) {
            let disk = self
                .disk
                .get(&table_key)
                .ok_or_else(|| corrupt("a log names a table that does not exist"))?;
            let mut row_ids = Vec::new();
            let rows = disk.read_rows(Some(&mut row_ids))?;
            let live = LiveTable {
                row_ids,
                next_row_id: disk.next_row_id(),
                ..LiveTable::default()
            };
            self.catalog.get_table(Some(&name.0), &name.1)?.set_rows(rows);
            self.tables.insert(table_key.clone(), live);
        }
        Ok(self.tables.get_mut(&table_key).expect("just ensured"))
    }

    fn create_sequence(&mut self, schema: &str, name: &str, stored: StoredSequence) -> Result<()> {
        let data = SequenceData {
            usage_count: stored.usage_count,
            counter: stored.counter,
            last_value: stored.last_value,
            increment: stored.increment,
            min_value: stored.min_value,
            max_value: stored.max_value,
            cycle: stored.cycle,
            start_value: stored.counter,
        };
        let entry = self.catalog.schema_for(Some(schema))?;
        entry.create_sequence(name, data, OnCreateConflict::Error)?;
        self.sequence_usage
            .insert(key(&(schema.to_string(), name.to_string())), stored.usage_count);
        if self.before_v2 && stored.usage_count > 0 {
            let sequence = entry.get_sequence(name).expect("just created");
            sequence.derive_last_value();
        }
        Ok(())
    }

    /// Puts an entry in the catalog.
    pub(super) fn add_entry(&mut self, entry: StoredEntry) -> Result<()> {
        match entry {
            StoredEntry::Schema(path) => self.catalog.create_schema_path(&path, OnCreateConflict::Ignore)?,
            StoredEntry::Table(table) => self.load_table(*table)?,
            StoredEntry::View {
                schema,
                name,
                query,
                aliases,
                comment,
                column_comments,
                secure,
            } => {
                let schema = self.catalog.schema_for(Some(&schema))?;
                schema.create_view(&name, *query, aliases, OnCreateConflict::Error, Vec::new(), secure)?;
                if comment.is_some() || !column_comments.is_empty() {
                    schema.set_view_comments(&name, comment, column_comments);
                }
            }
            StoredEntry::Macro {
                schema,
                name,
                table_macro,
                macros,
                comment,
            } => {
                let schema = self.catalog.schema_for(Some(&schema))?;
                schema.create_macro(&name, table_macro, macros, OnCreateConflict::Error)?;
                if comment.is_some() {
                    schema.set_macro_comment(&name, table_macro, comment);
                }
            }
            StoredEntry::Sequence {
                schema,
                name,
                data,
                comment,
            } => {
                self.create_sequence(&schema, &name, data)?;
                if comment.is_some() {
                    self.catalog.schema_for(Some(&schema))?.set_sequence_comment(&name, comment);
                }
            }
            StoredEntry::Type {
                schema,
                name,
                ty,
                comment,
            } => {
                // `ReplayCreateType` takes IGNORE_ON_CONFLICT: a type can be logged twice
                let schema = self.catalog.schema_for(Some(&schema))?;
                schema.create_type(&name, ty, OnCreateConflict::Ignore)?;
                if comment.is_some() {
                    schema.set_type_comment(&name, comment);
                }
            }
            StoredEntry::Trigger { schema, trigger } => {
                let schema = self.catalog.schema_for(Some(&schema))?;
                schema.create_trigger(*trigger, OnCreateConflict::Error)?;
            }
            StoredEntry::Index(mut info, comment) => {
                let index_type = match &mut info.kind {
                    duckrs_parser::ast::CreateKind::Index { index_type, .. } => {
                        std::mem::replace(index_type, "ART".to_string())
                    }
                    _ => unreachable!("an index has the info of an index"),
                };
                // the file has these rows under the index already, so they aren't checked again
                if let Some((table, mut index)) = Binder::new(self.catalog).bind_create_index(&info)? {
                    let name = key(&(table.schema.clone(), index.name.clone()));
                    index.storage_version = self.index_versions.get(&name).copied();
                    index.index_type = index_type;
                    index.comment = comment;
                    table.indexes.write().push(Arc::new(index));
                }
            }
        }
        Ok(())
    }

    fn drop_entry(&mut self, kind: DropKind, (schema, name): &EntryName) -> Result<()> {
        let entry = self.catalog.schema_for(Some(schema))?;
        let dropped = match kind {
            DropKind::Table => {
                let table_key = key(&(schema.clone(), name.clone()));
                self.tables.remove(&table_key);
                self.disk.remove(&table_key);
                // its triggers go with it
                entry.drop_triggers_on(name);
                entry.drop_entry(name, "Table")?
            }
            DropKind::View => entry.drop_entry(name, "View")?,
            DropKind::Sequence => entry.drop_sequence(name),
            DropKind::Macro => entry.drop_macro(name, false),
            DropKind::TableMacro => entry.drop_macro(name, true),
            DropKind::Type => entry.drop_type(name),
            DropKind::Index => entry.drop_index(name),
            DropKind::Trigger(table) => entry.drop_trigger(&table, name),
        };
        if !dropped {
            return Err(corrupt(format!("a log drops {name}, which does not exist")));
        }
        Ok(())
    }

    fn sequence_value(
        &mut self,
        name: &EntryName,
        usage_count: u64,
        counter: i64,
        last_value: Option<i64>,
    ) -> Result<()> {
        let sequence = self
            .catalog
            .schema_for(Some(&name.0))?
            .get_sequence(&name.1)
            .ok_or_else(|| corrupt("a log names a sequence that does not exist"))?;
        // `SequenceCatalogEntry::ReplayValue`: transactions can log them out of order
        let usage = self.sequence_usage.entry(key(name)).or_default();
        if usage_count > *usage {
            *usage = usage_count;
            sequence.replay_value(usage_count, counter, last_value);
        }
        if self.before_v2 {
            sequence.derive_last_value();
        }
        Ok(())
    }

    fn current_table(&mut self) -> Result<&mut LiveTable> {
        let name = self
            .current
            .clone()
            .ok_or_else(|| corrupt("a log changes rows without a table"))?;
        self.live_table(&name)
    }

    fn append(&mut self, chunk: &DataChunk) -> Result<()> {
        let live = self.current_table()?;
        live.add_rows(chunk.len());
        let (schema, name) = self.current.as_ref().expect("the table above");
        let table = self.catalog.get_table(Some(schema), name)?;
        if chunk.types() != table.types() {
            return Err(corrupt("a log inserts rows of other types"));
        }
        table.write_storage()?.append(chunk);
        Ok(())
    }

    /// Applies a record, in the order of the log.
    fn apply(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Create(entry) => {
                if let StoredEntry::Table(table) = &*entry {
                    // rows of a table of a log come from that log
                    debug_assert!(table.row_groups.is_empty());
                }
                self.add_entry(*entry)
            }
            Record::CreateSchema(path) => self.catalog.create_schema_path(&path, OnCreateConflict::Ignore),
            Record::DropSchema(path) => self
                .catalog
                .drop_schema(&crate::catalog::schema_key(&path), false, true),
            Record::Drop(kind, name) => self.drop_entry(kind, &name),
            Record::SequenceValue {
                name,
                usage_count,
                counter,
                last_value,
            } => self.sequence_value(&name, usage_count, counter, last_value),
            Record::UseTable(name) => {
                if !self.disk.contains_key(&key(&name)) {
                    return Err(corrupt("a log uses a table that does not exist"));
                }
                self.current = Some(name);
                Ok(())
            }
            Record::Insert(chunk) => self.append(&chunk),
            Record::RowGroups(row_groups) => {
                for row_group in &row_groups {
                    let chunk = table_data::scan_row_group(&self.buffer, row_group)?;
                    self.append(&chunk)?;
                }
                Ok(())
            }
            Record::Delete(chunk) => {
                let table = self.current_table()?;
                for row in 0..chunk.len() {
                    let position = table.position(chunk.column(0).data::<i64>()[row])?;
                    table.deleted.insert(position);
                }
                Ok(())
            }
            Record::Update {
                column,
                validity,
                chunk,
            } => {
                let table = self.current_table()?;
                for row in 0..chunk.len() {
                    let position = table.position(chunk.column(1).data::<i64>()[row])?;
                    let value = chunk.get_value(0, row);
                    let cell = table.updates.entry(position).or_default().entry(column).or_default();
                    if validity {
                        cell.null = Some(value.is_null());
                    } else {
                        cell.value = Some(value);
                    }
                }
                Ok(())
            }
        }
    }

    /// Rewrites the tables that a log deleted or updated rows of.
    fn finish(self) -> Result<()> {
        for ((schema, name), live) in self.tables {
            let table = self.catalog.get_table(Some(&schema), &name)?;
            let ids: Vec<u64> = (0..live.row_ids.len())
                .filter(|position| !live.deleted.contains(position))
                .map(|position| live.row_ids[position])
                .collect();
            table.set_row_ids(RowIds::of_list(&ids, live.next_row_id));
            if live.deleted.is_empty() && live.updates.is_empty() {
                table.reset_log();
                continue;
            }
            let mut storage = table.write_storage()?;
            let types = storage.types().to_vec();
            let mut result = ColumnDataCollection::new(types.clone());
            let mut chunk = DataChunk::new(&types);
            for position in (0..storage.len()).filter(|position| !live.deleted.contains(position)) {
                let mut row = storage.row(position);
                for (&column, cell) in live.updates.get(&position).into_iter().flatten() {
                    let target = row
                        .get_mut(column)
                        .ok_or_else(|| corrupt("a log updates a column that does not exist"))?;
                    if cell.null == Some(true) {
                        *target = Value::null(types[column].clone());
                    } else if let Some(value) = &cell.value {
                        *target = value.clone();
                    }
                }
                chunk.append_row(&row)?;
                if chunk.len() == STANDARD_VECTOR_SIZE {
                    result.append(&chunk);
                    chunk.reset();
                }
            }
            if !chunk.is_empty() {
                result.append(&chunk);
            }
            *storage = result;
            drop(storage);
            table.reset_log();
        }
        Ok(())
    }

    /// Applies what a log committed.
    pub(super) fn replay(mut self, records: Vec<Record>) -> Result<()> {
        if records.iter().any(|record| matches!(record, Record::RowGroups(_))) {
            self.buffer.block_manager().include_blocks_past_checkpoint()?;
        }
        for record in records {
            self.apply(record)?;
        }
        self.finish()
    }
}
