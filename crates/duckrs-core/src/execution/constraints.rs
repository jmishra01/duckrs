// Ported from the constraint checks of duckdb/src/storage/data_table.cpp and the error messages
// of duckdb/src/execution/index/art/art.cpp @ rust-port-baseline
//
//! Checks that writes keep a table's constraints. PRIMARY KEY, UNIQUE and FOREIGN KEY look keys up
//! in an ART per key, whose row ids are positions in the table. Tables are rewritten when rows move,
//! so these are built when first needed and dropped when that happens.

use super::art::allocator::prefix_count;
use super::art::key::{ArtKey, chunk_keys};
use super::art::tree::{AppendMode, Art};
use super::expression_executor::execute;
use crate::catalog::{ForeignKey, IndexTree, TableCatalogEntry, TableConstraint, TableKey, UniqueKeys};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{OrderModifiers, create_sort_keys};
use duckrs_common::vector::DataChunk;
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{sql_identifier, sql_quoted_identifier};
use parking_lot::MutexGuard;
use std::collections::HashSet;
use std::sync::Arc;

fn violation(message: String) -> Error {
    Error::new(ErrorKind::Constraint, message)
}

/// The key of each row in `columns`: `None` for a row with a NULL in it, which never conflicts.
pub(super) fn row_keys(chunk: &DataChunk, columns: &[usize]) -> Result<Vec<Option<Vec<u8>>>> {
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

/// The ART key of each row, for a key of the table.
pub(crate) fn keys_of(key: &TableKey, chunk: &DataChunk) -> Result<Vec<Option<ArtKey>>> {
    let values = key_values(key, chunk)?;
    let columns: Vec<_> = values.columns().iter().collect();
    chunk_keys(&columns, values.len())
}

/// An empty ART for a key of `table`.
fn new_keys(table: &TableCatalogEntry, key: &TableKey) -> UniqueKeys {
    UniqueKeys::new(key_prefix_count(table, key))
}

/// `ART::SetPrefixCount` for a key of `table`.
pub(crate) fn key_prefix_count(table: &TableCatalogEntry, key: &TableKey) -> usize {
    let types: Vec<_> = match &key.expressions {
        None => key
            .columns
            .iter()
            .map(|c| table.columns[*c].ty.physical_type())
            .collect(),
        Some(expressions) => expressions.iter().map(|e| e.return_type.physical_type()).collect(),
    };
    prefix_count(&types)
}

/// What the expressions of a key give for `chunk`.
fn key_values(key: &TableKey, chunk: &DataChunk) -> Result<DataChunk> {
    let columns = match &key.expressions {
        None => key.columns.iter().map(|c| chunk.column(*c).clone()).collect(),
        Some(expressions) => expressions
            .iter()
            .map(|expression| execute(expression, chunk))
            .collect::<Result<Vec<_>>>()?,
    };
    Ok(DataChunk::from_columns(columns, chunk.len()))
}

fn duplicate_key(key: &TableKey, chunk: &DataChunk, row: usize, local: bool) -> Result<Error> {
    let values = key_values(key, chunk)?;
    let mut texts = Vec::with_capacity(key.names.len());
    for column in values.columns() {
        let value = column.get_value(row);
        texts.push(if value.is_null() {
            "NULL".to_string()
        } else {
            value.try_to_string()?
        });
    }
    if local {
        // the error of merging the indexes of a transaction into the table's, `ART::Append`
        let key = texts.join(", ");
        return Ok(violation(format!(
            "PRIMARY KEY or UNIQUE constraint violation: duplicate key \"{key}\""
        )));
    }
    let name = key
        .names
        .iter()
        .zip(&texts)
        .map(|(name, text)| format!("{name}: {text}"))
        .collect::<Vec<_>>()
        .join(", ");
    let kind = if key.primary_key { "primary key" } else { "unique" };
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

/// `DataTable::VerifyNotNullConstraint` for each NOT NULL.
pub fn verify_not_null(table: &TableCatalogEntry, chunk: &DataChunk) -> Result<()> {
    // `VerifyGeneratedExpressionSuccess`
    for column in &table.generated {
        execute(&column.bound, chunk).map_err(|error| {
            violation(format!(
                "Incorrect value for generated column '{} {} AS ({})' : {}",
                sql_identifier(&column.name),
                column.ty,
                column.expr,
                error.message
            ))
        })?;
    }
    for (i, column) in table.columns.iter().enumerate() {
        let unified = chunk.column(i).to_unified(chunk.len());
        if column.not_null && (0..chunk.len()).any(|r| !unified.is_valid(r)) {
            return Err(violation(format!(
                "NOT NULL constraint failed: {}.{}",
                sql_identifier(&table.name),
                sql_identifier(&column.name)
            )));
        }
    }
    Ok(())
}

/// `VerifyCheckConstraint` for each CHECK of `table` over rows of the whole table.
pub fn verify_checks(table: &TableCatalogEntry, chunk: &DataChunk) -> Result<()> {
    for constraint in &table.constraints {
        let TableConstraint::Check { expression, text, .. } = constraint else {
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
/// rows from `first_new_row` on are the ones of the statement that is running. A DEFERRED key only
/// fails when `deferred_too`, at a commit: until then its keys can repeat.
fn build_keys(
    table: &TableCatalogEntry,
    rows: &ColumnDataCollection,
    first_new_row: usize,
    deferred_too: bool,
) -> Result<Vec<UniqueKeys>> {
    let mut indexes = Vec::new();
    for key in table.keys() {
        let mut index = new_keys(table, &key);
        let mut position = 0;
        for chunk in rows.chunks() {
            for (row, row_key) in keys_of(&key, chunk)?.into_iter().enumerate() {
                if let Some(row_key) = row_key
                    && let Some(existing) = index.insert(&row_key, position + row)?
                    && (deferred_too || !key.deferred)
                {
                    return Err(duplicate_key(
                        &key,
                        chunk,
                        row,
                        key.deferred || existing >= first_new_row,
                    )?);
                }
            }
            position += chunk.len();
        }
        indexes.push(index);
    }
    Ok(indexes)
}

/// The tree of `key` over the rows of `table`, for a scan. It goes along with rows that are added.
pub(crate) fn index_tree(table: &TableCatalogEntry, key: &TableKey) -> Result<IndexTree> {
    let rows = table.rows()?;
    if let Some(origin) = table.same_as_origin(rows.len()) {
        let tree = index_tree(origin, key)?;
        // it can only have been the same rows if they still are
        if table.same_as_origin(rows.len()).is_some() {
            return Ok(tree);
        }
    }
    let name = format!(
        "{:?} {:?}",
        key.columns,
        key.expressions
            .as_ref()
            .map(|expressions| expressions.iter().map(ToString::to_string).collect::<Vec<_>>())
    );
    let rewrites = table.rewrite_count();
    let empty = || IndexTree {
        rewrites,
        frontier: 0,
        rows: 0,
        gone: 0,
        tree: Arc::new(Art::new(key_prefix_count(table, key), UniqueKeys::BLOCK_SIZE, false)),
    };
    let ids = table.row_ids(rows.len());
    let mut trees = table.index_trees.lock();
    let entry = trees.entry(name).or_insert_with(empty);
    if entry.rewrites != rewrites {
        *entry = empty();
    }
    let known = ids.first_from(entry.frontier);
    if known < rows.len() {
        let tree = Arc::make_mut(&mut entry.tree);
        let mut first_row = known / STANDARD_VECTOR_SIZE * STANDARD_VECTOR_SIZE;
        for chunk in rows.chunks().skip(known / STANDARD_VECTOR_SIZE) {
            for (row, row_key) in keys_of(key, chunk)?.into_iter().enumerate() {
                if first_row + row >= known
                    && let Some(row_key) = row_key
                {
                    tree.insert(&row_key, ids.id(first_row + row) as i64, AppendMode::Default)?;
                }
            }
            first_row += chunk.len();
        }
        entry.rows += rows.len() - known;
        entry.frontier = ids.next();
    }
    Ok(entry.clone())
}

/// Fails if `rows` break a key of `table`.
pub(crate) fn check_all_keys(table: &TableCatalogEntry, rows: &ColumnDataCollection) -> Result<()> {
    build_keys(table, rows, 0, true).map(drop)
}

/// That the DEFERRED keys of `table` hold among `rows`, as a commit checks them, or among those
/// whose position `among` takes.
pub(crate) fn check_deferred_keys(
    table: &TableCatalogEntry,
    rows: &ColumnDataCollection,
    among: impl Fn(usize) -> bool,
) -> Result<()> {
    for key in table.keys().into_iter().filter(|key| key.deferred) {
        let mut index = new_keys(table, &key);
        if let Some(deleted) = &*table.deleted_before_key.lock() {
            for chunk in deleted.chunks() {
                for row_key in keys_of(&key, chunk)?.into_iter().flatten() {
                    index.insert(&row_key, usize::MAX)?;
                }
            }
        }
        let mut position = 0;
        for chunk in rows.chunks() {
            for (row, row_key) in keys_of(&key, chunk)?.into_iter().enumerate() {
                if let Some(row_key) = row_key
                    && among(position + row)
                    && index.insert(&row_key, position + row)?.is_some()
                {
                    return Err(duplicate_key(&key, chunk, row, true)?);
                }
            }
            position += chunk.len();
        }
    }
    Ok(())
}

/// The key maps of `table`, of which `rows` are the rows.
pub(crate) fn current_keys<'a>(
    table: &'a TableCatalogEntry,
    rows: &ColumnDataCollection,
) -> Result<MutexGuard<'a, Option<Vec<UniqueKeys>>>> {
    let mut keys = table.unique_keys.lock();
    if keys.is_none() {
        *keys = match table.inherited_keys(rows.len()) {
            Some(inherited) => Some(inherited),
            None => Some(build_keys(table, rows, usize::MAX, false)?),
        };
    }
    Ok(keys)
}

/// Which of the indexes of [`build_keys`] holds exactly `columns`.
pub(super) fn index_of(table: &TableCatalogEntry, columns: &[usize]) -> usize {
    table
        .keys()
        .iter()
        .position(|key| !key.deferred && key.columns == columns)
        .expect("the binder found this key")
}

/// That `chunk` finds the rows it references in the table `foreign_key` names.
fn verify_referenced(
    foreign_key: &ForeignKey,
    keys: &UniqueKeys,
    chunk: &DataChunk,
    parent: &TableCatalogEntry,
) -> Result<()> {
    let columns: Vec<_> = foreign_key.columns.iter().map(|c| chunk.column(*c)).collect();
    for (row, key) in chunk_keys(&columns, chunk.len())?.into_iter().enumerate() {
        let Some(key) = key else { continue };
        if !keys.contains(&key)? {
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
            Some(reference) => {
                let parent = reference.table();
                let parent = &parent;
                let rows = parent.rows()?;
                let mut keys = parent.unique_keys.lock();
                if keys.is_none() {
                    *keys = Some(build_keys(parent, &rows, usize::MAX, false)?);
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
    if table.constraints.is_empty() && !table.indexes.read().iter().any(|index| index.unique) {
        return Ok(());
    }
    verify_checks(table, chunk)?;
    let mut keys = current_keys(table, rows)?;
    let next_row = rows.len();
    let result = append_keys(
        table,
        keys.as_mut().expect("just built"),
        chunk,
        next_row,
        table.first_local_row(first_new_row, next_row),
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
    for (key, index) in table.keys().iter().zip(keys.iter_mut()) {
        for (row, row_key) in keys_of(key, chunk)?.into_iter().enumerate() {
            if let Some(row_key) = row_key
                && let Some(existing) = index.insert(&row_key, next_row + row)?
                && !key.deferred
            {
                return Err(duplicate_key(key, chunk, row, existing >= first_new_row)?);
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
                    .is_some_and(|parent| parent.name.eq_ignore_ascii_case(&table.name))
            {
                let child_rows = child.rows()?;
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
/// `UPDATE ... RETURNING` of a key takes the rows chunk by chunk, each deleting its old keys and then
/// adding its new ones, so that a key that a later chunk gives up is still there. Postgres has the
/// same limitation, and C++ makes a point of it in a test.
pub fn verify_chunked_key_update(
    table: &TableCatalogEntry,
    rows: &ColumnDataCollection,
    batches: &[(DataChunk, DataChunk)],
) -> Result<()> {
    for key in table.keys().into_iter().filter(|key| !key.deferred) {
        let mut present = HashSet::new();
        for chunk in rows.chunks() {
            present.extend(keys_of(&key, chunk)?.into_iter().flatten());
        }
        for (old, new) in batches {
            for gone in keys_of(&key, old)?.into_iter().flatten() {
                present.remove(&gone);
            }
            for (row, value) in keys_of(&key, new)?.into_iter().enumerate() {
                if let Some(value) = value
                    && !present.insert(value)
                {
                    return Err(duplicate_key(&key, new, row, false)?);
                }
            }
        }
    }
    Ok(())
}

pub fn verify_key_update(
    table: &Arc<TableCatalogEntry>,
    rows: &ColumnDataCollection,
    rewritten: &ColumnDataCollection,
    changed: &ColumnDataCollection,
) -> Result<()> {
    // the updated rows are the last ones: they moved there
    let keys = build_keys(table, rewritten, rewritten.len() - changed.len(), false)?;
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
