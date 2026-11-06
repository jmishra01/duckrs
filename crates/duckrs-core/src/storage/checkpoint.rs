//! Ported from duckdb/src/storage/checkpoint_manager.cpp, checkpoint/table_data_reader.cpp and
//! table/row_group.cpp @ rust-port-baseline.
//!
//! Reads what the last checkpoint left: the catalog and, per table, where its row groups are.
//! A serialized object can't be skipped, so what is not ported (indexes and so on) stops the whole
//! load with an error instead of going missing silently.

use super::block_manager::corrupt;
use super::buffer_manager::BufferManager;
use super::metadata::{MetaBlockPointer, MetadataChains};
use super::serialization::convert;
use super::serialization::generated::{self as g, enums::catalog_type};
use super::serialization::{not_ported, read_type_expression};
use super::statistics::{read_column_statistics, skip_table_sample};
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, Result};
use duckrs_parser::ast;

/// `StorageVersion::V2_0_0`. Older files have a smaller number here.
const STORAGE_VERSION_V2_0_0: u64 = 69;

fn not_ported_entry(entry_type: u64) -> Error {
    not_ported(match entry_type {
        catalog_type::TRIGGER_ENTRY => "triggers",
        _ => "this kind of catalog entry",
    })
}

/// A stored column of a table in a file.
#[derive(Debug, Clone)]
pub struct StoredColumn {
    pub name: String,
    pub ty: LogicalType,
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
            // which metadata blocks a row group owns, packed: only a rewrite needs to know
            d.read_property_with_default::<Vec<u64>>(107, "per_column_metadata_blocks")?;
            Ok(result)
        })
    }
}

/// A table in a file (`BoundCreateTableInfo` with its `PersistentTableData`).
#[derive(Debug, Clone)]
pub struct StoredTable {
    pub schema: String,
    pub name: String,
    /// What a statement that creates the table would say, generated columns included.
    pub definition: ast::CreateTableInfo,
    /// The columns with data.
    pub columns: Vec<StoredColumn>,
    pub total_rows: u64,
    pub row_groups: Vec<RowGroupPointer>,
}

/// `CreateSequenceInfo`, which is what C++ checkpoints of a sequence.
#[derive(Debug, Clone)]
pub struct StoredSequence {
    pub counter: i64,
    pub last_value: Option<i64>,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
}

/// An entry of the catalog of a file. The file has them in an order in which each comes after what
/// it depends on, so they are kept in it.
#[derive(Debug, Clone)]
pub enum StoredEntry {
    Schema(String),
    Table(Box<StoredTable>),
    View {
        schema: String,
        name: String,
        query: Box<ast::SelectStatement>,
        aliases: Vec<String>,
    },
    Macro {
        schema: String,
        name: String,
        table_macro: bool,
        macros: Vec<ast::MacroFunction>,
    },
    Sequence {
        schema: String,
        name: String,
        data: StoredSequence,
    },
    Type {
        schema: String,
        name: String,
        ty: LogicalType,
    },
    Index(Box<ast::CreateInfo>),
}

/// The catalog of a file.
#[derive(Debug, Clone, Default)]
pub struct StoredCatalog {
    pub entries: Vec<StoredEntry>,
}

/// The schema of an entry. Files from v2.0.0 on can have a path in place of `schema`.
fn schema_of(info: &g::CreateInfo) -> Result<String> {
    match info.qualified_name.as_ref().map_or(&[][..], |name| &name.path) {
        [] => Ok(info.schema.clone()),
        [_, schema, _] => Ok(schema.clone()),
        _ => Err(not_ported("an entry in a nested schema")),
    }
}

/// `CreateTableInfo::Deserialize`: what it says of a table.
fn table_definition(schema: &str, stored: g::CreateTableInfo) -> Result<(ast::CreateTableInfo, Vec<StoredColumn>)> {
    if stored.query.is_some() {
        return Err(not_ported("CREATE TABLE AS queries"));
    }
    if !stored.partition_keys.is_empty() {
        return Err(not_ported("partition keys"));
    }
    if !stored.sort_keys.is_empty() {
        return Err(not_ported("sort keys"));
    }
    if !stored.options.is_empty() {
        return Err(not_ported("table options"));
    }
    let mut columns = Vec::with_capacity(stored.columns.columns.len());
    let mut physical = Vec::new();
    for column in stored.columns.columns {
        let (definition, stored) = convert::column(column)?;
        columns.push(definition);
        physical.extend(stored.map(|(name, ty)| StoredColumn { name, ty }));
    }
    let mut constraints = Vec::with_capacity(stored.constraints.len());
    for constraint in stored.constraints {
        let constraint = constraint.ok_or_else(|| corrupt("a constraint is missing"))?;
        constraints.extend(convert::constraint(*constraint)?);
    }
    let definition = ast::CreateTableInfo {
        name: ast::QualifiedName {
            qualification: vec![schema.to_string()],
            name: stored.table,
        },
        on_conflict: ast::OnCreateConflict::Error,
        temporary: false,
        columns,
        constraints,
        query: None,
        partition_keys: Vec::new(),
        sort_keys: Vec::new(),
        options: Vec::new(),
    };
    Ok((definition, physical))
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
    let mut d = BinaryDeserializer::new(&root_bytes)
        .with_type_expressions(read_type_expression)
        .from_before_v2(buffer.block_manager().header().storage_compatibility < STORAGE_VERSION_V2_0_0);
    let mut pending = Vec::new();
    d.read_object_value(|d| {
        d.read_list(100, "catalog_entries", |d| d.read_object_value(read_entry))
            .map(|entries| pending = entries)
    })?;
    for entry in pending {
        catalog.entries.push(match entry {
            Pending::Done(entry) => entry,
            Pending::Table(table, pointer) => {
                let mut table = *table;
                table.row_groups = read_row_groups(buffer, &mut chains, pointer, &table.columns)?;
                StoredEntry::Table(Box::new(table))
            }
        });
    }
    Ok(catalog)
}

/// An entry of the root, which for a table says where to go on reading.
enum Pending {
    Done(StoredEntry),
    Table(Box<StoredTable>, MetaBlockPointer),
}

/// `CheckpointReader::ReadEntry`.
fn read_entry(d: &mut BinaryDeserializer<'_>) -> Result<Pending> {
    let entry_type: u64 = d.read_property(99, "type")?;
    if ![
        catalog_type::SCHEMA_ENTRY,
        catalog_type::TABLE_ENTRY,
        catalog_type::VIEW_ENTRY,
        catalog_type::SEQUENCE_ENTRY,
        catalog_type::TYPE_ENTRY,
        catalog_type::INDEX_ENTRY,
        catalog_type::MACRO_ENTRY,
        catalog_type::TABLE_MACRO_ENTRY,
    ]
    .contains(&entry_type)
    {
        return Err(not_ported_entry(entry_type));
    }
    let info: Option<Box<g::CreateInfo>> = d.read_property(100, "info")?;
    let info = *info.ok_or_else(|| corrupt("a catalog entry has no create info"))?;
    if info.type_ != entry_type {
        return Err(corrupt("catalog entry type mismatch"));
    }
    // the path of a schema is its name and an empty name for the entry (`SchemaCatalogEntry::GetInfo`)
    if let g::CreateInfoKind::CreateSchemaInfo(stored) = &info.kind {
        if !stored.options.is_empty() {
            return Err(not_ported("schema options"));
        }
        let path = info.qualified_name.map(|name| name.path).unwrap_or_default();
        return match <[String; 2]>::try_from(path) {
            Ok([name, _]) => Ok(Pending::Done(StoredEntry::Schema(name))),
            Err(path) if path.is_empty() => Ok(Pending::Done(StoredEntry::Schema(info.schema))),
            Err(_) => Err(not_ported("a nested schema")),
        };
    }
    let schema = schema_of(&info)?;
    Ok(Pending::Done(match info.kind {
        g::CreateInfoKind::CreateSchemaInfo(_) => unreachable!("returned above"),
        g::CreateInfoKind::CreateTableInfo(stored) => {
            let name = stored.table.clone();
            let (definition, columns) = table_definition(&schema, stored)?;
            let pointer = d.read_property(101, "table_pointer")?;
            let total_rows = d.read_property(102, "total_rows")?;
            // the ART of each index, which only lookups and writes need: C++ doesn't read them at
            // load either, and the rows are all that `duckrs` keeps keys unique from
            d.read_property_with_default::<Vec<g::BlockPointer>>(103, "index_pointers")?;
            d.read_property_with_default::<Vec<g::IndexStorageInfo>>(104, "index_storage_infos")?;
            d.read_optional_property::<u64>(105, "next_row_id")?;
            let table = StoredTable {
                schema,
                name,
                definition,
                columns,
                total_rows,
                row_groups: Vec::new(),
            };
            return Ok(Pending::Table(Box::new(table), pointer));
        }
        g::CreateInfoKind::CreateIndexInfo(stored) => {
            // where the ART is in files from before there were `index_storage_infos`
            d.read_optional_property::<g::BlockPointer>(101, "root_block_pointer")?;
            StoredEntry::Index(Box::new(convert::create_index(&schema, stored)?))
        }
        g::CreateInfoKind::CreateViewInfo(stored) => StoredEntry::View {
            schema,
            name: stored.view_name,
            query: Box::new(convert::select_statement(stored.query)?),
            aliases: stored.aliases,
        },
        g::CreateInfoKind::CreateMacroInfo(stored) => {
            let mut macros = Vec::with_capacity(1 + stored.extra_functions.len());
            for function in std::iter::once(stored.function).chain(stored.extra_functions) {
                let function = function.ok_or_else(|| corrupt("a macro has no function"))?;
                macros.push(convert::macro_function(*function)?);
            }
            StoredEntry::Macro {
                schema,
                name: stored.name,
                table_macro: entry_type == catalog_type::TABLE_MACRO_ENTRY,
                macros,
            }
        }
        g::CreateInfoKind::CreateSequenceInfo(stored) => StoredEntry::Sequence {
            schema,
            name: stored.name,
            data: StoredSequence {
                counter: stored.start_value,
                last_value: stored.last_value,
                increment: stored.increment,
                min_value: stored.min_value,
                max_value: stored.max_value,
                cycle: stored.cycle,
            },
        },
        g::CreateInfoKind::CreateTypeInfo(stored) => StoredEntry::Type {
            schema,
            name: stored.name,
            ty: stored.logical_type,
        },
    }))
}
