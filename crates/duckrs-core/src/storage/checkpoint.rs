//! Ported from duckdb/src/storage/checkpoint_manager.cpp, checkpoint/table_data_reader.cpp and
//! table/row_group.cpp @ rust-port-baseline.
//!
//! Reads what the last checkpoint left: the catalog and, per table, where its row groups are.
//! A serialized object can't be skipped, so what is not ported (indexes and so on) stops the whole
//! load with an error instead of going missing silently.

use super::block_manager::{DatabaseHeader, corrupt};
use super::buffer_manager::BufferManager;
use super::metadata::{MetaBlockPointer, MetadataChains};
use super::serialization::convert;
use super::serialization::generated::{self as g, enums::catalog_type};
use super::serialization::{not_ported, read_type_expression};
use super::statistics::{BaseStatistics, read_column_statistics, skip_table_sample};
use duckrs_common::serializer::{BinaryDeserializer, Deserialize};
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;

/// `StorageVersion::V2_0_0`. Older files have a smaller number here.
pub(super) const STORAGE_VERSION_V2_0_0: u64 = 69;

pub(super) fn is_before_v2(header: &DatabaseHeader) -> bool {
    header.storage_compatibility < STORAGE_VERSION_V2_0_0
}

/// For what a file holds of its catalog, in its checkpoints and logs.
pub(super) fn catalog_deserializer(data: &[u8], before_v2: bool) -> BinaryDeserializer<'_> {
    BinaryDeserializer::new(data)
        .with_type_expressions(read_type_expression)
        .from_before_v2(before_v2)
}

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
    /// What the last checkpoint knew of each column.
    pub column_statistics: Vec<Option<BaseStatistics>>,
    /// The id of the next row, past the gaps that a vacuum leaves.
    pub next_row_id: u64,
    /// Where the ART of each index is, for what reads one.
    pub indexes: Vec<g::IndexStorageInfo>,
}

/// `CreateSequenceInfo`, which is what C++ checkpoints of a sequence.
#[derive(Debug, Clone)]
pub struct StoredSequence {
    pub usage_count: u64,
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
    /// the schemas from the outermost down to it
    Schema(Vec<String>),
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
    /// With what `COMMENT ON INDEX` said.
    Index(Box<ast::CreateInfo>, Option<String>),
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
        // [catalog, schema path..., name] of an entry in a nested schema
        [_, path @ .., _] if !path.is_empty() => Ok(crate::catalog::schema_key(path)),
        _ => Err(corrupt("an entry has no schema")),
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
) -> Result<(Vec<RowGroupPointer>, Vec<Option<BaseStatistics>>)> {
    let mut d = BinaryDeserializer::new(chains.at(buffer, pointer)?);
    let statistics = d.read_object_value(|d| {
        let mut column = columns.iter();
        let statistics = d.read_list(100, "column_stats", |d| {
            let column = column
                .next()
                .ok_or_else(|| corrupt("a table has more statistics than columns"))?;
            read_column_statistics(d, &column.ty)
        })?;
        if d.has_property(101)? {
            d.read_object_field(101, "table_sample", skip_table_sample)?;
        }
        Ok(statistics)
    })?;
    let count = u64::from_le_bytes(d.read_raw(8)?.try_into().expect("8 bytes"));
    let row_groups = (0..count)
        .map(|_| RowGroupPointer::read_value(&mut d))
        .collect::<Result<_>>()?;
    Ok((row_groups, statistics))
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
    let mut d = catalog_deserializer(&root_bytes, is_before_v2(&buffer.block_manager().header()));
    let mut pending = Vec::new();
    d.read_object_value(|d| {
        d.read_list(100, "catalog_entries", |d| d.read_object_value(read_entry))
            .map(|entries| pending = entries)
    })?;
    for entry in pending {
        catalog.entries.push(match entry {
            Pending::Done(entry) => entry,
            Pending::Table(mut table, pointer) => {
                (table.row_groups, table.column_statistics) =
                    read_row_groups(buffer, &mut chains, pointer, &table.columns)?;
                StoredEntry::Table(table)
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
    let info = *info.ok_or_else(|| {
        let name = if entry_type == catalog_type::SCHEMA_ENTRY {
            "schema"
        } else {
            "catalog"
        };
        Error::new(
            ErrorKind::DataCorruption,
            format!("corrupt database file - {name} entry without create info"),
        )
    })?;
    if info.type_ != entry_type {
        return Err(corrupt("catalog entry type mismatch"));
    }
    let is_index = matches!(info.kind, g::CreateInfoKind::CreateIndexInfo(_));
    let converted = convert_entry(info)?;
    if is_index {
        // where the ART is in files from before there were `index_storage_infos`
        d.read_optional_property::<g::BlockPointer>(101, "root_block_pointer")?;
    }
    match converted {
        Converted::Entry(entry) => Ok(Pending::Done(entry)),
        Converted::Table(mut table) => {
            let pointer = d.read_property(101, "table_pointer")?;
            table.total_rows = d.read_property(102, "total_rows")?;
            // the ART of each index, which only lookups and writes need: C++ doesn't read them at
            // load either, and the rows are all that `duckrs` keeps keys unique from
            d.read_property_with_default::<Vec<g::BlockPointer>>(103, "index_pointers")?;
            table.indexes = d.read_property_with_default(104, "index_storage_infos")?;
            table.next_row_id = d
                .read_optional_property(105, "next_row_id")?
                .unwrap_or(table.total_rows);
            Ok(Pending::Table(table, pointer))
        }
    }
}

/// An entry of a checkpoint or a log. Where the rows of a table are is for the caller to find out.
pub(super) enum Converted {
    Entry(StoredEntry),
    Table(Box<StoredTable>),
}

/// What a `CreateInfo` says, in the terms of the engine.
pub(super) fn convert_entry(info: g::CreateInfo) -> Result<Converted> {
    let entry_type = info.type_;
    let comment = info.comment.clone();
    // the path of a schema is its name and an empty name for the entry (`SchemaCatalogEntry::GetInfo`)
    if let g::CreateInfoKind::CreateSchemaInfo(stored) = &info.kind {
        if !stored.options.is_empty() {
            return Err(not_ported("schema options"));
        }
        let path = info.qualified_name.map(|name| name.path).unwrap_or_default();
        return match path.as_slice() {
            [] => Ok(Converted::Entry(StoredEntry::Schema(vec![info.schema]))),
            [name] | [name, _] => Ok(Converted::Entry(StoredEntry::Schema(vec![name.clone()]))),
            // `CheckpointReader::ReadSchema`: [catalog, schema path..., ""] of a nested one
            [_, schemas @ .., _] => Ok(Converted::Entry(StoredEntry::Schema(schemas.to_vec()))),
        };
    }
    let schema = schema_of(&info)?;
    Ok(Converted::Entry(match info.kind {
        g::CreateInfoKind::CreateSchemaInfo(_) => unreachable!("returned above"),
        g::CreateInfoKind::CreateTableInfo(stored) => {
            let name = stored.table.clone();
            let (definition, columns) = table_definition(&schema, stored)?;
            return Ok(Converted::Table(Box::new(StoredTable {
                schema,
                name,
                definition,
                columns,
                total_rows: 0,
                row_groups: Vec::new(),
                column_statistics: Vec::new(),
                next_row_id: 0,
                indexes: Vec::new(),
            })));
        }
        g::CreateInfoKind::CreateIndexInfo(stored) => {
            let comment = comment.filter(|value| !value.is_null());
            let comment = comment.map(|value| value.try_to_string()).transpose()?;
            StoredEntry::Index(Box::new(convert::create_index(&schema, stored)?), comment)
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
                usage_count: stored.usage_count,
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
