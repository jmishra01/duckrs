// Ported from duckdb/extension/tpch/tpch_extension.cpp (DbgenFunction), DBGenWrapper::CreateTPCHSchema of
// duckdb/extension/tpch/dbgen/dbgen.cpp, duckdb/extension/tpcds/tpcds_extension.cpp (DsdgenFunction) and
// DSDGenWrapper::CreateTPCDSSchema of duckdb/extension/tpcds/dsdgen/dsdgen.cpp @ rust-port-baseline
//
//! `dbgen(...)` and `dsdgen(...)` in a FROM clause: when the scan runs, it creates the benchmark's tables and
//! appends the generated rows to them, and gives no rows itself. (`CALL dbgen(...)` runs in the client.)

use crate::catalog::{
    ColumnDefinition, OnCreateConflict, SchemaCatalogEntry, TableCatalogEntry, TableConstraint, TableOrView,
};
use crate::function::udf::{TableUdfRun, TableUdfScan};
use crate::planner::binder::tpc::{TpcBenchmark, TpcGeneration};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::ConstraintCheckMode;
use std::collections::HashMap;
use std::sync::Arc;

/// The chunks of a table that one INSERT appends (`DBGEN_ROW_BATCH_SIZE` rows and more).
const APPEND_CHUNKS: usize = 60;

/// The scan of a `dbgen(...)` / `dsdgen(...)` call, which [`crate::planner::binder::tpc::TpcGenerator`] gives the
/// binder.
pub fn scan(generation: TpcGeneration) -> Arc<dyn TableUdfScan> {
    Arc::new(GenerateScan { generation })
}

struct GenerateScan {
    generation: TpcGeneration,
}

impl TableUdfScan for GenerateScan {
    fn init(&self, _column_ids: &[usize]) -> Result<Box<dyn TableUdfRun>> {
        Ok(Box::new(GenerateRun {
            generation: Some(self.generation.clone()),
        }))
    }
}

/// `DBGenGlobalState`: the data is generated once per run of the scan, with its first chunk.
struct GenerateRun {
    generation: Option<TpcGeneration>,
}

impl TableUdfRun for GenerateRun {
    fn next(&mut self, _output: &mut DataChunk) -> Result<()> {
        if let Some(generation) = self.generation.take() {
            generate(&generation)?;
        }
        Ok(())
    }
}

/// `DbgenFunction` / `DsdgenFunction`: the schema, then the data.
fn generate(generation: &TpcGeneration) -> Result<()> {
    let schema = &generation.schema;
    let suffix = &generation.suffix;
    let mut tables: HashMap<&'static str, Arc<TableCatalogEntry>> = HashMap::new();
    match generation.benchmark {
        TpcBenchmark::Tpch => {
            let mut generator =
                duckrs_tpch::DBGenGenerator::new(generation.sf, generation.children, generation.step)?;
            // `DBGenWrapper::CreateTPCHSchema`: the tables, if they are not there, with NOT NULL columns
            for table in duckrs_tpch::TpchTable::ALL {
                let spec = table.schema();
                let name = format!("{}{suffix}", spec.name);
                let columns: Vec<(String, LogicalType)> =
                    spec.columns.iter().map(|c| (c.name.to_string(), c.ty.clone())).collect();
                create_table(schema, &name, &columns, true, None, OnCreateConflict::Ignore)?;
                let entry = table_named(schema, &name)?;
                // `ValidateTPCHTableSchema`: a table that was there must have the columns dbgen fills
                let found: Vec<(&str, &LogicalType)> =
                    entry.columns.iter().map(|c| (c.name.as_str(), &c.ty)).collect();
                duckrs_tpch::validate_tpch_table_schema(table, &name, &found)?;
                tables.insert(spec.name, entry);
            }
            append_all(
                &tables,
                std::iter::from_fn(|| generator.next_chunk()).map(|(table, chunk)| (table.name(), chunk)),
            )
        }
        TpcBenchmark::Tpcds => {
            let generator = duckrs_tpcds::dsdgen(generation.sf)?;
            // `DSDGenWrapper::CreateTPCDSSchema`: an existing table is an error, or replaced with `overwrite`
            let conflict = if generation.overwrite {
                OnCreateConflict::Replace
            } else {
                OnCreateConflict::Error
            };
            for definition in duckrs_tpcds::tpcds_schema("", generation.keys) {
                let base = definition.table.name();
                let name = format!("{base}{suffix}");
                let key = definition.primary_key.as_ref().map(|key| {
                    key.iter()
                        .filter_map(|column| definition.columns.iter().position(|(c, _)| c == column))
                        .collect::<Vec<usize>>()
                });
                create_table(schema, &name, &definition.columns, false, key, conflict)?;
                tables.insert(base, table_named(schema, &name)?);
            }
            append_all(&tables, generator.map(|(table, chunk)| (table.name(), chunk)))
        }
    }
}

fn create_table(
    schema: &SchemaCatalogEntry,
    name: &str,
    columns: &[(String, LogicalType)],
    not_null: bool,
    primary_key: Option<Vec<usize>>,
    on_conflict: OnCreateConflict,
) -> Result<()> {
    let definitions = columns
        .iter()
        .map(|(column, ty)| ColumnDefinition {
            name: column.clone(),
            ty: ty.clone(),
            default_value: None,
            not_null,
            default_text: None,
            comment: None,
            compression: None,
        })
        .collect();
    let mut constraints: Vec<TableConstraint> = if not_null {
        (0..columns.len()).map(TableConstraint::NotNull).collect()
    } else {
        Vec::new()
    };
    if let Some(columns) = primary_key {
        constraints.push(TableConstraint::Unique {
            columns,
            primary_key: true,
            column_level: false,
            check_mode: ConstraintCheckMode::Default,
        });
    }
    schema
        .create_table(name, definitions, Vec::new(), constraints, on_conflict)
        .map(drop)
}

fn table_named(schema: &SchemaCatalogEntry, name: &str) -> Result<Arc<TableCatalogEntry>> {
    match schema.get_entry(name) {
        Some(TableOrView::Table(table)) => Ok(table),
        _ => Err(Error::new(
            ErrorKind::Catalog,
            format!("Table with name {name} does not exist!"),
        )),
    }
}

/// The generated chunks into their tables, a batch of each table's chunks to an INSERT (the generators' appenders).
fn append_all(
    tables: &HashMap<&'static str, Arc<TableCatalogEntry>>,
    chunks: impl Iterator<Item = (&'static str, DataChunk)>,
) -> Result<()> {
    let mut pending: HashMap<&'static str, Vec<DataChunk>> = HashMap::new();
    let mut order: Vec<&'static str> = Vec::new();
    for (table, chunk) in chunks {
        crate::execution::interrupt::check()?;
        let batch = pending.entry(table).or_insert_with(|| {
            order.push(table);
            Vec::new()
        });
        batch.push(chunk);
        if batch.len() >= APPEND_CHUNKS {
            let batch = pending.remove(table).expect("just added");
            order.retain(|t| *t != table);
            insert(&tables[table], batch)?;
        }
    }
    for table in order {
        let batch = pending.remove(table).expect("pending");
        insert(&tables[table], batch)?;
    }
    Ok(())
}

/// An INSERT of `chunks` into `table`, as the query's own INSERTs run (constraints, local storage, the log).
fn insert(table: &Arc<TableCatalogEntry>, chunks: Vec<DataChunk>) -> Result<()> {
    let types: Vec<LogicalType> = table.columns.iter().map(|c| c.ty.clone()).collect();
    let source = LogicalOperator::new(
        LogicalOperatorKind::ChunkGet {
            table_index: 1,
            types,
            chunks: Arc::new(chunks),
        },
        vec![],
    );
    let plan = LogicalOperator::new(
        LogicalOperatorKind::Insert {
            table: table.clone(),
            column_index_map: (0..table.columns.len()).map(Some).collect(),
            table_index: 0,
            returning: false,
            on_conflict: None,
        },
        vec![source],
    );
    crate::execution::run_plan(plan).map(drop)
}
