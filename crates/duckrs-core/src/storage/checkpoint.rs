//! Ported from duckdb/src/storage/checkpoint_manager.cpp, checkpoint/table_data_reader.cpp and
//! table/row_group.cpp @ rust-port-baseline.
//!
//! Reads what the last checkpoint left: the catalog and, per table, where its row groups are.
//! A serialized object can't be skipped, so what is not ported (views, macros, default values,
//! indexes and so on) stops the whole load with an error instead of going missing silently.

use super::block_manager::corrupt;
use super::buffer_manager::BufferManager;
use super::metadata::{MetaBlockPointer, MetadataChains};
use super::statistics::{read_column_statistics, skip_table_sample};
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, Result};

/// `CatalogType`.
const TABLE_ENTRY: u8 = 1;
const SCHEMA_ENTRY: u8 = 2;
/// `TableColumnType::GENERATED`.
const GENERATED_COLUMN: u8 = 1;
/// `ConstraintType::NOT_NULL`.
const NOT_NULL_CONSTRAINT: u8 = 1;

fn not_ported(what: &str) -> Error {
    Error::not_implemented(format!(
        "reading {what} from a database file is not yet ported to duckrs"
    ))
}

/// An index of a table, which fails the load if a file has one.
struct IndexNotPorted;

impl Deserialize for IndexNotPorted {
    fn read_value(_: &mut BinaryDeserializer<'_>) -> Result<Self> {
        Err(not_ported("indexes"))
    }
}

/// Fails when the next field is `field_id`.
fn reject(d: &mut BinaryDeserializer<'_>, field_id: u16, what: &str) -> Result<()> {
    if d.has_property(field_id)? {
        return Err(not_ported(what));
    }
    Ok(())
}

/// A column of a table in a file.
#[derive(Debug, Clone)]
pub struct StoredColumn {
    pub name: String,
    pub ty: LogicalType,
    pub not_null: bool,
}

/// `RowGroupPointer`.
#[derive(Debug, Clone)]
pub struct RowGroupPointer {
    pub row_start: u64,
    pub tuple_count: u64,
    pub data_pointers: Vec<MetaBlockPointer>,
    pub deletes_pointers: Vec<MetaBlockPointer>,
}

impl Deserialize for RowGroupPointer {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let result = Self {
                row_start: d.read_property(100, "row_start")?,
                tuple_count: d.read_property(101, "tuple_count")?,
                data_pointers: d.read_property(102, "data_pointers")?,
                deletes_pointers: d.read_property(103, "delete_pointers")?,
            };
            d.read_optional_property::<bool>(104, "has_metadata_blocks")?;
            d.read_property_with_default::<Vec<u64>>(105, "extra_metadata_blocks")?;
            d.read_optional_property::<bool>(106, "has_per_column_metadata_blocks")?;
            reject(d, 107, "per column metadata blocks")?;
            Ok(result)
        })
    }
}

/// A table in a file (`BoundCreateTableInfo` with its `PersistentTableData`).
#[derive(Debug, Clone)]
pub struct StoredTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<StoredColumn>,
    pub total_rows: u64,
    pub row_groups: Vec<RowGroupPointer>,
}

/// The catalog of a file. Tables come after the schemas they need.
#[derive(Debug, Clone, Default)]
pub struct StoredCatalog {
    pub schemas: Vec<String>,
    pub tables: Vec<StoredTable>,
}

/// The part of a `CreateInfo` that all entries share.
struct CreateInfo {
    catalog_type: u8,
    schema: String,
    /// `qualified_name`, which files from v2.0.0 on have in place of `schema`.
    qualified_name: Vec<String>,
}

fn read_create_info(d: &mut BinaryDeserializer<'_>) -> Result<CreateInfo> {
    let catalog_type: u8 = d.read_property(100, "type")?;
    let _catalog: String = d.read_property_with_default(101, "catalog")?;
    let schema: String = d.read_property_with_default(102, "schema")?;
    d.read_property_with_default::<bool>(103, "temporary")?;
    d.read_property_with_default::<bool>(104, "internal")?;
    d.read_property::<u8>(105, "on_conflict")?;
    d.read_property_with_default::<String>(106, "sql")?;
    d.read_optional_property::<Value>(107, "comment")?;
    d.read_property_with_default::<Vec<(String, String)>>(108, "tags")?;
    reject(d, 109, "dependencies between catalog entries")?;
    d.read_property_with_default::<String>(110, "extension_name")?;
    let qualified_name = if d.has_property(111)? {
        d.read_object(111, "qualified_name", |d| d.read_property_with_default(100, "path"))?
    } else {
        Vec::new()
    };
    Ok(CreateInfo {
        catalog_type,
        schema,
        qualified_name,
    })
}

fn read_column(d: &mut BinaryDeserializer<'_>) -> Result<Option<StoredColumn>> {
    d.read_object_value(|d| {
        let name: String = d.read_property_with_default(100, "name")?;
        let ty: LogicalType = d.read_property(101, "type")?;
        reject(d, 102, "default values and generated columns")?;
        let category: u8 = d.read_property(103, "category")?;
        d.read_property::<u8>(104, "compression_type")?;
        d.read_optional_property::<Value>(105, "comment")?;
        d.read_property_with_default::<Vec<(String, String)>>(106, "tags")?;
        Ok((category != GENERATED_COLUMN).then_some(StoredColumn {
            name,
            ty,
            not_null: false,
        }))
    })
}

/// `Constraint::Deserialize`: the index of the column of a NOT NULL.
struct NotNullConstraint(u64);

impl Deserialize for NotNullConstraint {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            let constraint_type: u8 = d.read_property(100, "type")?;
            if constraint_type != NOT_NULL_CONSTRAINT {
                return Err(not_ported("CHECK, UNIQUE and FOREIGN KEY constraints"));
            }
            Ok(Self(d.read_property(200, "index")?))
        })
    }
}

/// What `CreateTableInfo::Deserialize` says of a table: its name and its columns.
fn read_table_info(d: &mut BinaryDeserializer<'_>) -> Result<(String, Vec<StoredColumn>)> {
    let name: String = d.read_property_with_default(200, "table")?;
    let mut columns = d.read_object(201, "columns", |d| {
        if !d.has_property(100)? {
            return Ok(Vec::new());
        }
        d.read_list(100, "columns", read_column)
    })?;
    let constraints: Vec<Option<NotNullConstraint>> = d.read_property_with_default(202, "constraints")?;
    reject(d, 203, "CREATE TABLE AS queries")?;
    reject(d, 204, "partition keys")?;
    reject(d, 205, "sort keys")?;
    reject(d, 206, "table options")?;
    for constraint in constraints.into_iter().flatten() {
        match columns.get_mut(constraint.0 as usize).and_then(Option::as_mut) {
            Some(column) => column.not_null = true,
            None => return Err(not_ported("a NOT NULL constraint on a generated column")),
        }
    }
    Ok((name, columns.into_iter().flatten().collect()))
}

/// `TableDataReader::ReadTableData` and the row group pointers after it.
fn read_row_groups(
    buffer: &BufferManager,
    chains: &mut MetadataChains,
    pointer: MetaBlockPointer,
    columns: &[StoredColumn],
) -> Result<Vec<RowGroupPointer>> {
    let mut d = BinaryDeserializer::new(chains.at(buffer, pointer)?);
    d.read_object_value(|d| {
        let mut column = columns.iter();
        d.read_list(100, "column_stats", |d| {
            let column = column
                .next()
                .ok_or_else(|| corrupt("a table has more statistics than columns"))?;
            read_column_statistics(d, &column.ty)
        })?;
        if d.has_property(101)? {
            d.read_object_field(101, "table_sample", skip_table_sample)?;
        }
        Ok(())
    })?;
    let count = u64::from_le_bytes(d.read_raw(8)?.try_into().expect("8 bytes"));
    (0..count).map(|_| RowGroupPointer::read_value(&mut d)).collect()
}

/// `CheckpointReader::LoadCheckpoint`.
pub fn read_catalog(buffer: &BufferManager) -> Result<StoredCatalog> {
    let mut catalog = StoredCatalog::default();
    let root = MetaBlockPointer::new(buffer.block_manager().header().meta_block, 0);
    if !root.is_valid() {
        return Ok(catalog);
    }
    let mut chains = MetadataChains::default();
    // the tables need `chains` for more of the file, so the entries are taken first
    let root_bytes = chains.at(buffer, root)?.to_vec();
    let mut d = BinaryDeserializer::new(&root_bytes);
    let mut pending = Vec::new();
    d.read_object_value(|d| {
        d.read_list(100, "catalog_entries", |d| {
            d.read_object_value(|d| read_entry(d, &mut catalog.schemas, &mut pending))
        })
        .map(drop)
    })?;
    for table in pending {
        let row_groups = read_row_groups(buffer, &mut chains, table.pointer, &table.columns)?;
        catalog.tables.push(StoredTable {
            schema: table.schema,
            name: table.name,
            columns: table.columns,
            total_rows: table.total_rows,
            row_groups,
        });
    }
    Ok(catalog)
}

struct PendingTable {
    schema: String,
    name: String,
    columns: Vec<StoredColumn>,
    pointer: MetaBlockPointer,
    total_rows: u64,
}

/// `CheckpointReader::ReadEntry`.
fn read_entry(d: &mut BinaryDeserializer<'_>, schemas: &mut Vec<String>, tables: &mut Vec<PendingTable>) -> Result<()> {
    let entry_type: u8 = d.read_property(99, "type")?;
    if entry_type != TABLE_ENTRY && entry_type != SCHEMA_ENTRY {
        return Err(not_ported(match entry_type {
            3 => "views",
            4 => "indexes",
            6 => "sequences",
            8 => "types",
            11 => "triggers",
            30 | 31 => "macros",
            _ => "this kind of catalog entry",
        }));
    }
    // `ReadProperty<unique_ptr<CreateInfo>>`: a pointer that can be null, then the object
    let (info, table) = d.read_object_field(100, "info", |d| {
        if !bool::read_value(d)? {
            return Err(corrupt("a catalog entry has no create info"));
        }
        d.read_object_value(|d| {
            let info = read_create_info(d)?;
            if info.catalog_type != entry_type {
                return Err(corrupt("catalog entry type mismatch"));
            }
            if entry_type == TABLE_ENTRY {
                read_table_info(d).map(|table| (info, Some(table)))
            } else {
                reject(d, 200, "schema options")?;
                Ok((info, None))
            }
        })
    })?;
    let path = &info.qualified_name;
    match table {
        None => {
            let name = path.last().unwrap_or(&info.schema).clone();
            schemas.push(name);
        }
        Some((name, columns)) => {
            let schema = match path.len() {
                0 => info.schema,
                3 => path[1].clone(),
                _ => return Err(not_ported("a table in a nested schema")),
            };
            let pointer = d.read_property(101, "table_pointer")?;
            let total_rows = d.read_property(102, "total_rows")?;
            d.read_property_with_default::<Vec<IndexNotPorted>>(103, "index_pointers")?;
            d.read_property_with_default::<Vec<IndexNotPorted>>(104, "index_storage_infos")?;
            d.read_optional_property::<u64>(105, "next_row_id")?;
            tables.push(PendingTable {
                schema,
                name,
                columns,
                pointer,
                total_rows,
            });
        }
    }
    Ok(())
}
