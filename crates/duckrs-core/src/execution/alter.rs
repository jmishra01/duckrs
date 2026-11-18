// Ported from the constructors of DataTable in duckdb/src/storage/data_table.cpp that ALTER uses
// @ rust-port-baseline
//
//! Puts an ALTER into effect: the rows become what the plan says, and the new entry takes the
//! place of the old one, which keeps its rows for a rollback.

use super::constraints;
use super::expression_executor::execute;
use crate::catalog::{IndexCatalogEntry, RowIds, TableCatalogEntry, TableSource};
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
    let old = alter.table.clone();
    let adding_key = alter.constraints.len() > old.constraints.len();
    let before: Vec<_> = old.types();
    // rows still in a file stay there: only the columns that the ALTER makes are read and written
    let layered = layered_alter(&old, &alter, &before)?;
    let empty;
    let loaded;
    let source = match layered {
        Some(_) => {
            empty = ColumnDataCollection::new(before.clone());
            &empty
        }
        None => {
            loaded = old.rows()?;
            &*loaded
        }
    };
    let row_ids = old.row_ids(old.row_count());
    let rows = match &alter.projection {
        _ if layered.is_some() => ColumnDataCollection::new(alter.columns.iter().map(|c| c.ty.clone()).collect()),
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
    let mut table = TableCatalogEntry::new(old.schema.clone(), alter.name, alter.columns, rows, alter.constraints);
    if let Some(layered) = layered {
        table.keep_in_file(layered);
    }
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
    log_as_alter(&old, &table, alter.logged.as_ref());
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

/// `WriteAlter`: the log says an ALTER of a column as such, if it knows the table as it was and nothing of its rows
/// is waiting to be logged. The new entry goes on from the log state of that table, its rows in the same places.
/// Otherwise the log makes the table again with its rows. Tables with indexes are made again, which logs those.
fn log_as_alter(
    old: &Arc<TableCatalogEntry>,
    table: &Arc<TableCatalogEntry>,
    kind: Option<&duckrs_parser::ast::AlterKind>,
) {
    let (Some(kind), Some(base)) = (kind, old.durable_base()) else {
        return;
    };
    if !base.indexes.read().is_empty() {
        return;
    }
    let ids = base.row_ids(base.row_count());
    let log = base.log.lock();
    let same_ids =
        |a: &crate::catalog::RowIds, b: &crate::catalog::RowIds| a.next() == b.next() && a.ranges().eq(b.ranges());
    let settled = log.updated.is_empty() && (Arc::ptr_eq(&log.logged, &ids) || same_ids(&log.logged, &ids));
    // an ALTER TYPE gives the rows of a table with gaps other ids, which the log's ALTER doesn't
    let renumbered = !same_ids(&table.row_ids(table.row_count()), &old.row_ids(old.row_count()));
    if !settled || renumbered {
        return;
    }
    let Ok(info) = crate::storage::log_writer::alter_record(old, table, kind) else {
        return;
    };
    let mut alters = base.alters.lock().clone();
    alters.push(crate::catalog::LoggedAlter {
        from: Arc::downgrade(&base),
        info,
    });
    *table.alters.lock() = alters;
    *table.log.lock() = crate::catalog::TableLog {
        logged: log.logged.clone(),
        replayed: log.replayed.clone(),
        ..crate::catalog::TableLog::default()
    };
}

/// `RowGroup::AddColumn`, `RemoveColumn` and `AlterType` of a table whose rows are in a file: the rows stay, and of
/// each column either that of the table, which stays unread, or one the ALTER computes from the columns it reads.
/// `None` where the rows have to be read: they are in memory, the ALTER checks them, or a new type gives them new
/// ids.
fn layered_alter(
    old: &TableCatalogEntry,
    alter: &BoundTableAlter,
    before: &[duckrs_common::types::LogicalType],
) -> Result<Option<Arc<dyn TableSource>>> {
    use crate::planner::expression::BoundExprKind;
    use crate::storage::layered::{Layer, LayeredTable};
    let Some(source) = old.source() else {
        return Ok(None);
    };
    if alter.verify {
        return Ok(None);
    }
    let types: Vec<_> = alter.columns.iter().map(|c| c.ty.clone()).collect();
    let Some(projection) = &alter.projection else {
        return Ok(Some(source));
    };
    let ids = old.row_ids(old.row_count());
    let dense = ids.ranges().count() <= 1 && ids.ranges().next().is_none_or(|(first, _)| first == 0);
    if alter.retyped.is_some() && !dense {
        return Ok(None);
    }
    let (base, old_layers) = LayeredTable::parts(&source, before.len());
    let mut layers = Vec::with_capacity(projection.len());
    for (index, (expression, ty)) in projection.iter().zip(&types).enumerate() {
        if let BoundExprKind::Reference(at) = expression.kind
            && before[at] == *ty
            && alter.retyped != Some(index)
        {
            layers.push(old_layers[at].clone());
            continue;
        }
        // the columns it reads, in the order it reads them
        let mut read = Vec::new();
        let mut expression = expression.clone();
        expression.walk_mut(&mut |e| {
            if let BoundExprKind::Reference(at) = e.kind {
                let position = read.iter().position(|c| *c == at).unwrap_or_else(|| {
                    read.push(at);
                    read.len() - 1
                });
                e.kind = BoundExprKind::Reference(position);
            }
        });
        let mut groups = Vec::with_capacity(source.group_count());
        for group in 0..source.group_count() {
            let rows = source.group_rows(group);
            let chunk = if read.is_empty() {
                DataChunk::from_columns(Vec::new(), rows)
            } else {
                source.read_group(group, &read)?
            };
            let mut vector = execute(&expression, &chunk)?;
            vector.flatten(rows);
            groups.push(Some(vector));
        }
        layers.push(Layer::Memory {
            groups: Arc::new(groups),
            from: None,
        });
    }
    Ok(Some(Arc::new(LayeredTable::new(base, layers, types))))
}
