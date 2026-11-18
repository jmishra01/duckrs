// Ported from the constructors of DataTable in duckdb/src/storage/data_table.cpp that ALTER uses
// @ rust-port-baseline
//
//! Puts an ALTER into effect: the rows become what the plan says, and the new entry takes the
//! place of the old one, which keeps its rows for a rollback.

use super::constraints;
use super::expression_executor::execute;
use crate::catalog::{IndexCatalogEntry, RowIds, TableCatalogEntry};
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
    let source = old.rows()?;
    let row_ids = old.row_ids(source.len());
    let before: Vec<_> = old.types();
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
    let mut table = TableCatalogEntry::new(old.schema.clone(), alter.name, alter.columns, rows, alter.constraints);
    table.generated = alter.generated;
    table.catalog = old.catalog.clone();
    *table.comment.lock() = alter.comment;
    table
        .row_group_rows
        .store(old.row_group_size(), std::sync::atomic::Ordering::Relaxed);
    // the columns it leaves as they were keep what the last checkpoint wrote of them
    if let Some(checkpointed) = old.checkpointed() {
        let stored = old.checkpointed_columns();
        let columns = match &alter.projection {
            Some(projection) => projection
                .iter()
                .zip(&table.columns)
                .enumerate()
                .map(|(index, (expression, column))| match expression.kind {
                    _ if alter.retyped == Some(index) => None,
                    crate::planner::expression::BoundExprKind::Reference(at) if before[at] == column.ty => stored[at],
                    _ => None,
                })
                .collect(),
            None => stored,
        };
        table.set_checkpointed_columns(Some(checkpointed), columns);
    }
    let table = Arc::new(table);
    // C++ writes a column of a new type to new row groups, without the gaps
    let retyped =
        before.len() == table.columns.len() && table.columns.iter().zip(&before).any(|(new, old)| new.ty != *old);
    table.set_row_ids(if retyped {
        let rows = table.row_count();
        RowIds::of_ranges([(0, rows)], rows as u64)
    } else {
        (*row_ids).clone()
    });
    if alter.verify {
        let rows = table.rows()?;
        for chunk in rows.chunks() {
            constraints::verify_not_null(&table, chunk)?;
        }
        // a new key checks the rows as CREATE INDEX does
        // and a DEFERRED one the rows that are committed: those of the transaction wait for its commit
        let ids = table.row_ids(rows.len());
        let committed = |position: usize| ids.id(position) < crate::catalog::MAX_ROW_ID;
        let deferred = if adding_key && table.has_deferred_keys() {
            *table.deleted_before_key.lock() = old.deleted_committed_rows()?;
            constraints::check_deferred_keys(&table, &rows, committed)
        } else {
            Ok(())
        };
        deferred
            .and_then(|()| constraints::current_keys(&table, &rows).map(drop))
            .map_err(|error| {
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
    if table.has_outstanding_updates() {
        return Err(Error::new(
            ErrorKind::Transaction,
            "Cannot create index with outstanding updates",
        ));
    }
    let index = Arc::new(index);
    table.indexes.write().push(index.clone());
    *table.unique_keys.lock() = None;
    let rows = table.rows()?;
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
