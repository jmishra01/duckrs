// Ported from the constraint checks of duckdb/src/storage/data_table.cpp and the error messages
// of duckdb/src/execution/index/art/art.cpp @ rust-port-baseline
//
//! Checks that writes keep a table's constraints. PRIMARY KEY, UNIQUE and FOREIGN KEY need to
//! look keys up, which C++ does with ART indexes. Those come with Phase 10; until then each
//! table has hash maps of its keys, built when first needed and dropped when its rows move.

use super::expression_executor::execute;
use crate::catalog::{ForeignKey, TableCatalogEntry, TableConstraint, UniqueKeys};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{OrderModifiers, create_sort_keys};
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::sql_quoted_identifier;
use std::collections::HashSet;
use std::sync::Arc;

fn violation(message: String) -> Error {
    Error::new(ErrorKind::Constraint, message)
}

/// The key of each row in `columns`: `None` for a row with a NULL in it, which never conflicts.
fn row_keys(chunk: &DataChunk, columns: &[usize]) -> Result<Vec<Option<Vec<u8>>>> {
    let vectors: Vec<_> = columns
        .iter()
        .map(|c| (chunk.column(*c), OrderModifiers::ASC_NULLS_LAST))
        .collect();
    let keys = create_sort_keys(&vectors, chunk.len())?;
    let unified: Vec<_> = columns
        .iter()
        .map(|c| chunk.column(*c).to_unified(chunk.len()))
        .collect();
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(row, key)| unified.iter().all(|u| u.is_valid(row)).then_some(key))
        .collect())
}

/// `ART::GenerateErrorKeyName`.
fn key_name(
    table: &TableCatalogEntry,
    chunk: &DataChunk,
    values: &[usize],
    names: &[usize],
    row: usize,
) -> Result<String> {
    let mut parts = Vec::with_capacity(values.len());
    for (value, name) in values.iter().zip(names) {
        let value = chunk.column(*value).get_value(row);
        let text = if value.is_null() {
            "NULL".to_string()
        } else {
            value.try_to_string()?
        };
        parts.push(format!("{}: {text}", table.columns[*name].name));
    }
    Ok(parts.join(", "))
}

fn duplicate_key(
    table: &TableCatalogEntry,
    chunk: &DataChunk,
    columns: &[usize],
    primary_key: bool,
    row: usize,
    local: bool,
) -> Result<Error> {
    if local {
        // the error of merging the indexes of a transaction into the table's, `ART::Append`
        let mut values = Vec::with_capacity(columns.len());
        for column in columns {
            values.push(chunk.column(*column).get_value(row).try_to_string()?);
        }
        let key = values.join(", ");
        return Ok(violation(format!(
            "PRIMARY KEY or UNIQUE constraint violation: duplicate key \"{key}\""
        )));
    }
    let name = key_name(table, chunk, columns, columns, row)?;
    let kind = if primary_key { "primary key" } else { "unique" };
    Ok(violation(format!(
        "Duplicate key \"{name}\" violates {kind} constraint."
    )))
}

fn still_referenced(name: &str) -> Error {
    violation(format!(
        "Violates foreign key constraint because key \"{name}\" is still referenced by a foreign key in a different \
         table. If this is an unexpected constraint violation, please refer to our foreign key limitations in the \
         documentation"
    ))
}

/// `VerifyCheckConstraint` for each CHECK of `table` over rows of the whole table.
pub fn verify_checks(table: &TableCatalogEntry, chunk: &DataChunk) -> Result<()> {
    for constraint in &table.constraints {
        let TableConstraint::Check { expression, text } = constraint else {
            continue;
        };
        let result = execute(expression, chunk).map_err(|error| {
            violation(format!(
                "CHECK constraint failed on table {} with expression {text} (Error: {})",
                sql_quoted_identifier(&table.name),
                error.message
            ))
        })?;
        for row in 0..chunk.len() {
            if result.get_value(row).as_i128() == Some(0) {
                return Err(violation(format!(
                    "CHECK constraint failed on table {} with expression {text}",
                    sql_quoted_identifier(&table.name)
                )));
            }
        }
    }
    Ok(())
}

/// The keys of `rows`, for each UNIQUE and PRIMARY KEY in the order of `table.constraints`. The
/// rows from `first_new_row` on are the ones of the statement that is running.
fn build_keys(table: &TableCatalogEntry, rows: &ColumnDataCollection, first_new_row: usize) -> Result<Vec<UniqueKeys>> {
    let mut indexes = Vec::new();
    for constraint in &table.constraints {
        let TableConstraint::Unique { columns, primary_key } = constraint else {
            continue;
        };
        let mut index = UniqueKeys::new();
        let mut position = 0;
        for chunk in rows.chunks() {
            for (row, key) in row_keys(chunk, columns)?.into_iter().enumerate() {
                if let Some(key) = key
                    && let Some(existing) = index.insert(key, position + row)
                {
                    let local = existing >= first_new_row;
                    return Err(duplicate_key(table, chunk, columns, *primary_key, row, local)?);
                }
            }
            position += chunk.len();
        }
        indexes.push(index);
    }
    Ok(indexes)
}

/// Which of the indexes of [`build_keys`] holds exactly `columns`.
fn index_of(table: &TableCatalogEntry, columns: &[usize]) -> usize {
    table
        .constraints
        .iter()
        .filter_map(|c| match c {
            TableConstraint::Unique { columns, .. } => Some(columns),
            _ => None,
        })
        .position(|unique| unique == columns)
        .expect("the binder found this key")
}

/// That `chunk` finds the rows it references in the table `foreign_key` names.
fn verify_referenced(
    foreign_key: &ForeignKey,
    keys: &UniqueKeys,
    chunk: &DataChunk,
    parent: &TableCatalogEntry,
) -> Result<()> {
    for (row, key) in row_keys(chunk, &foreign_key.columns)?.into_iter().enumerate() {
        if key.is_some_and(|key| !keys.contains_key(&key)) {
            let name = key_name(
                parent,
                chunk,
                &foreign_key.columns,
                &foreign_key.referenced_columns,
                row,
            )?;
            return Err(violation(format!(
                "Violates foreign key constraint because key \"{name}\" does not exist in the referenced table"
            )));
        }
    }
    Ok(())
}

fn check_foreign_keys(table: &Arc<TableCatalogEntry>, chunk: &DataChunk, own: &[UniqueKeys]) -> Result<()> {
    for constraint in &table.constraints {
        let TableConstraint::ForeignKey(foreign_key) = constraint else {
            continue;
        };
        let position = |parent: &TableCatalogEntry| index_of(parent, &foreign_key.referenced_columns);
        match &foreign_key.referenced {
            None => verify_referenced(foreign_key, &own[position(table)], chunk, table)?,
            Some(parent) => {
                let rows = parent.storage.read();
                let mut keys = parent.unique_keys.lock();
                if keys.is_none() {
                    *keys = Some(build_keys(parent, &rows, usize::MAX)?);
                }
                let keys = keys.as_ref().expect("just built");
                verify_referenced(foreign_key, &keys[position(parent)], chunk, parent)?;
            }
        }
    }
    Ok(())
}

/// `DataTable::VerifyAppendConstraints`, for `chunk` about to join `rows`, the rows of `table`.
/// The statement has added the rows from `first_new_row` on.
pub fn verify_append(
    table: &Arc<TableCatalogEntry>,
    rows: &ColumnDataCollection,
    chunk: &DataChunk,
    first_new_row: usize,
) -> Result<()> {
    if table.constraints.is_empty() {
        return Ok(());
    }
    verify_checks(table, chunk)?;
    let mut keys = table.unique_keys.lock();
    if keys.is_none() {
        *keys = Some(build_keys(table, rows, usize::MAX)?);
    }
    let next_row = rows.len();
    let result = append_keys(
        table,
        keys.as_mut().expect("just built"),
        chunk,
        next_row,
        first_new_row,
    );
    if result.is_err() {
        // it may have added the keys of the rows before the one that failed
        *keys = None;
    }
    result
}

fn append_keys(
    table: &Arc<TableCatalogEntry>,
    keys: &mut [UniqueKeys],
    chunk: &DataChunk,
    next_row: usize,
    first_new_row: usize,
) -> Result<()> {
    // before the keys: a row can't reference the rows of its own chunk
    check_foreign_keys(table, chunk, keys)?;
    let uniques = table.constraints.iter().filter_map(|c| match c {
        TableConstraint::Unique { columns, primary_key } => Some((columns, *primary_key)),
        _ => None,
    });
    for ((columns, primary_key), index) in uniques.zip(keys.iter_mut()) {
        for (row, key) in row_keys(chunk, columns)?.into_iter().enumerate() {
            if let Some(key) = key
                && let Some(existing) = index.insert(key, next_row + row)
            {
                let local = existing >= first_new_row;
                return Err(duplicate_key(table, chunk, columns, primary_key, row, local)?);
            }
        }
    }
    Ok(())
}

/// A foreign key to a table, the table that has it, and the keys of its rows.
type ReferencingKeys = (Arc<TableCatalogEntry>, ForeignKey, HashSet<Vec<u8>>);

/// The foreign keys that reference `table`. `rows` are `table`'s, which the caller has locked.
fn referencing_keys(table: &Arc<TableCatalogEntry>, rows: &ColumnDataCollection) -> Result<Vec<ReferencingKeys>> {
    let mut found = Vec::new();
    let mut gather =
        |child: &Arc<TableCatalogEntry>, foreign_key: &ForeignKey, rows: &ColumnDataCollection| -> Result<()> {
            let mut keys = HashSet::new();
            for chunk in rows.chunks() {
                keys.extend(row_keys(chunk, &foreign_key.columns)?.into_iter().flatten());
            }
            found.push((child.clone(), foreign_key.clone(), keys));
            Ok(())
        };
    for constraint in &table.constraints {
        if let TableConstraint::ForeignKey(foreign_key) = constraint
            && foreign_key.referenced.is_none()
        {
            gather(table, foreign_key, rows)?;
        }
    }
    let children: Vec<_> = table
        .referenced_by
        .lock()
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    for child in &children {
        for constraint in &child.constraints {
            if let TableConstraint::ForeignKey(foreign_key) = constraint
                && foreign_key
                    .referenced
                    .as_ref()
                    .is_some_and(|parent| Arc::ptr_eq(parent, table))
            {
                let child_rows = child.storage.read();
                gather(child, foreign_key, &child_rows)?;
            }
        }
    }
    Ok(found)
}

/// `DataTable::VerifyDeleteConstraints`: nothing may reference the rows `deleted` from `rows`.
pub fn verify_delete(
    table: &Arc<TableCatalogEntry>,
    rows: &ColumnDataCollection,
    deleted: &ColumnDataCollection,
) -> Result<()> {
    for (child, foreign_key, keys) in referencing_keys(table, rows)? {
        for chunk in deleted.chunks() {
            for (row, key) in row_keys(chunk, &foreign_key.referenced_columns)?
                .into_iter()
                .enumerate()
            {
                if key.is_some_and(|key| keys.contains(&key)) {
                    let columns = &foreign_key.referenced_columns;
                    let name = key_name(&child, chunk, columns, &foreign_key.columns, row)?;
                    return Err(still_referenced(&name));
                }
            }
        }
    }
    Ok(())
}

/// UPDATE of key columns, which C++ turns into a DELETE and an INSERT: `rewritten` replaces
/// `rows`, with `changed` being the rows it updated.
pub fn verify_key_update(
    table: &Arc<TableCatalogEntry>,
    rows: &ColumnDataCollection,
    rewritten: &ColumnDataCollection,
    changed: &ColumnDataCollection,
) -> Result<()> {
    // the updated rows are the last ones: they moved there
    let keys = build_keys(table, rewritten, rewritten.len() - changed.len())?;
    for chunk in changed.chunks() {
        check_foreign_keys(table, chunk, &keys)?;
    }
    // a key that went away must not be referenced
    let remaining = |columns: &[usize]| -> Result<HashSet<Vec<u8>>> {
        let mut remaining = HashSet::new();
        for chunk in rewritten.chunks() {
            remaining.extend(row_keys(chunk, columns)?.into_iter().flatten());
        }
        Ok(remaining)
    };
    for (child, foreign_key, referencing) in referencing_keys(table, rewritten)? {
        let columns = &foreign_key.referenced_columns;
        let remaining = remaining(columns)?;
        for chunk in rows.chunks() {
            for (row, key) in row_keys(chunk, columns)?.into_iter().enumerate() {
                if key.is_some_and(|key| referencing.contains(&key) && !remaining.contains(&key)) {
                    let name = key_name(&child, chunk, columns, &foreign_key.columns, row)?;
                    return Err(still_referenced(&name));
                }
            }
        }
    }
    *table.unique_keys.lock() = Some(keys);
    Ok(())
}
