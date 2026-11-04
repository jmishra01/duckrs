// Ported from the constructors of DataTable in duckdb/src/storage/data_table.cpp that ALTER uses
// @ rust-port-baseline
//
//! Puts an ALTER into effect: the rows become what the plan says, and the new entry takes the
//! place of the old one, which keeps its rows for a rollback.

use super::constraints;
use super::expression_executor::execute;
use crate::catalog::{IndexCatalogEntry, TableCatalogEntry};
use crate::planner::binder::{BoundAlter, BoundTableAlter};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

pub fn execute_alter(alter: BoundAlter) -> Result<()> {
    match alter {
        BoundAlter::Nothing => Ok(()),
        BoundAlter::RenameView { schema, view, new_name } => schema.rename_view(&view, &new_name),
        BoundAlter::Table(alter) => execute_table_alter(alter),
    }
}

fn execute_table_alter(alter: BoundTableAlter) -> Result<()> {
    let old = alter.table;
    let adding_key = alter.constraints.len() > old.constraints.len();
    let source = old.storage.read();
    let rows = match &alter.projection {
        Some(projection) => {
            let types: Vec<_> = alter.columns.iter().map(|c| c.ty.clone()).collect();
            let mut rows = ColumnDataCollection::new(types);
            for chunk in source.chunks() {
                let columns = projection
                    .iter()
                    .map(|expression| execute(expression, chunk))
                    .collect::<Result<Vec<_>>>()?;
                rows.append(&DataChunk::from_columns(columns, chunk.len()));
            }
            rows
        }
        None => source.clone(),
    };
    drop(source);
    let table = Arc::new(TableCatalogEntry::new(
        old.schema.clone(),
        alter.name,
        alter.columns,
        rows,
        alter.constraints,
    ));
    if alter.verify {
        let rows = table.storage.read();
        for chunk in rows.chunks() {
            constraints::verify_not_null(&table, chunk)?;
        }
        // a new key checks the rows as CREATE INDEX does
        constraints::current_keys(&table, &rows).map(drop).map_err(|error| {
            if adding_key && error.kind == ErrorKind::Constraint {
                Error::new(ErrorKind::Constraint, "Data contains duplicates on indexed column(s)")
            } else {
                error
            }
        })?;
    }
    if let Some(error) = alter.rejected {
        return Err(error);
    }
    *table.referenced_by.lock() = old.referenced_by.lock().clone();
    *table.indexes.write() = old.indexes.read().clone();
    alter.schema.replace_table(&old.name, table);
    Ok(())
}

/// `PhysicalCreateARTIndex`: the keys of the rows have to be unique for a UNIQUE index.
pub fn execute_create_index(table: &Arc<TableCatalogEntry>, index: IndexCatalogEntry) -> Result<()> {
    let index = Arc::new(index);
    table.indexes.write().push(index.clone());
    *table.unique_keys.lock() = None;
    let rows = table.storage.read();
    let result = constraints::current_keys(table, &rows).map(drop);
    drop(rows);
    result.map_err(|error| {
        table.indexes.write().retain(|other| !Arc::ptr_eq(other, &index));
        *table.unique_keys.lock() = None;
        if error.kind == ErrorKind::Constraint {
            Error::new(ErrorKind::Constraint, "Data contains duplicates on indexed column(s)")
        } else {
            error
        }
    })
}
