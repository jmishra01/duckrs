// Ported from the conflict handling of duckdb/src/execution/operator/persistent/physical_insert.cpp
// and the semantics of the MERGE INTO that
// duckdb/src/planner/binder/statement/bind_insert.cpp generates @ rust-port-baseline
//
//! INSERT ... ON CONFLICT. Rows with the same key in the input count once, the first of them;
//! each of the others then updates the row it conflicts with, or goes in as a new row.

use super::constraints::{self, current_keys, index_of, keys_of};
use super::expression_executor::execute;
use super::physical::check_buffered;
use crate::catalog::{TableCatalogEntry, TableKey};
use crate::planner::logical::OnConflict;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Where a row of the input is: its chunk and its row in there.
type InputRow = (usize, usize);
/// A row that conflicts: the position of the row of the table, then its chunk and row.
type MatchedRow = (usize, usize, usize);

/// Runs the INSERT of `input`. Returns the rows it changed, and with `returning` those rows.
pub fn upsert(
    table: &Arc<TableCatalogEntry>,
    action: &OnConflict,
    input: &ColumnDataCollection,
    returning: bool,
) -> Result<(u64, ColumnDataCollection)> {
    let mut storage = table.write_storage();
    let input_chunks: Vec<&DataChunk> = input.chunks().collect();
    let (matched, inserts) = split_input(table, action, &storage, &input_chunks)?;

    let types = table.types();
    let row_of = |chunk: &DataChunk, row: usize| -> Vec<Value> {
        (0..types.len()).map(|c| chunk.column(c).get_value(row)).collect()
    };
    let updates = if action.do_update {
        updated_rows(table, action, &storage, &input_chunks, &matched)?
    } else {
        HashMap::new()
    };

    let mut new_rows = ColumnDataCollection::new(types.clone());
    let mut results: Vec<(InputRow, Vec<Value>)> = Vec::new();
    for (chunk, rows) in group_by_chunk(&inserts) {
        let count = rows.len();
        let mut part = input_chunks[chunk].clone();
        part.slice(
            &SelectionVector::from_vec(rows.iter().map(|r| *r as u32).collect()),
            count,
        );
        new_rows.append(&part);
        if returning {
            results.extend(
                rows.iter()
                    .map(|row| ((chunk, *row), row_of(input_chunks[chunk], *row))),
            );
        }
    }
    let inserted = new_rows.len() as u64;
    let updated = updates.len() as u64;
    if updates.is_empty() {
        let first_new_row = storage.len();
        for chunk in new_rows.chunks() {
            constraints::verify_not_null(table, chunk)?;
            constraints::verify_append(table, &storage, chunk, first_new_row)?;
            storage.append(chunk);
            check_buffered(&storage)?;
        }
    } else {
        let mut rewritten = ColumnDataCollection::new(types.clone());
        let mut changed = ColumnDataCollection::new(types.clone());
        let mut updated_chunk = DataChunk::new(&types);
        for (index, chunk) in storage.chunks().enumerate() {
            let first_row = index * STANDARD_VECTOR_SIZE;
            if !(0..chunk.len()).any(|row| updates.contains_key(&(first_row + row))) {
                rewritten.append(chunk);
                continue;
            }
            let mut output = DataChunk::new(&types);
            for row in 0..chunk.len() {
                match updates.get(&(first_row + row)) {
                    Some(values) => {
                        output.append_row(values)?;
                        updated_chunk.append_row(values)?;
                        if updated_chunk.len() == STANDARD_VECTOR_SIZE {
                            verify_updated(table, &updated_chunk)?;
                            changed.append(&updated_chunk);
                            updated_chunk = DataChunk::new(&types);
                        }
                    }
                    None => output.append_row(&row_of(chunk, row))?,
                }
            }
            rewritten.append(&output);
        }
        if !updated_chunk.is_empty() {
            verify_updated(table, &updated_chunk)?;
            changed.append(&updated_chunk);
        }
        for chunk in new_rows.chunks() {
            constraints::verify_not_null(table, chunk)?;
            constraints::verify_checks(table, chunk)?;
            rewritten.append(chunk);
            changed.append(chunk);
        }
        constraints::verify_key_update(table, &storage, &rewritten, &changed)?;
        *storage = rewritten;
    }

    let mut output = ColumnDataCollection::new(types.clone());
    if returning {
        for (position, _, input_row) in matched.iter().map(|(p, c, r)| (p, c, (*c, *r))) {
            if let Some(values) = updates.get(position) {
                results.push((input_row, values.clone()));
            }
        }
        results.sort_by_key(|(input_row, _)| *input_row);
        for rows in results.chunks(STANDARD_VECTOR_SIZE) {
            let mut chunk = DataChunk::new(&types);
            for (_, values) in rows {
                chunk.append_row(values)?;
            }
            output.append(&chunk);
        }
    }
    Ok((inserted + updated, output))
}

fn verify_updated(table: &TableCatalogEntry, chunk: &DataChunk) -> Result<()> {
    constraints::verify_not_null(table, chunk)?;
    constraints::verify_checks(table, chunk)
}

/// The rows of the input by chunk, the order of `rows` being that of the input.
fn group_by_chunk(rows: &[InputRow]) -> Vec<(usize, Vec<usize>)> {
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for (chunk, row) in rows {
        match groups.last_mut() {
            Some((last, rows)) if last == chunk => rows.push(*row),
            _ => groups.push((*chunk, vec![*row])),
        }
    }
    groups
}

/// Which rows of the input conflict with a row of the table (its position, then where in the
/// input), and which don't. Later rows with the key of an earlier one are in neither.
fn split_input(
    table: &TableCatalogEntry,
    action: &OnConflict,
    rows: &ColumnDataCollection,
    input: &[&DataChunk],
) -> Result<(Vec<MatchedRow>, Vec<InputRow>)> {
    let keys = current_keys(table, rows)?;
    let keys = keys.as_ref().expect("current_keys built them");
    let indexes: Vec<usize> = action.targets.iter().map(|columns| index_of(table, columns)).collect();
    let table_keys = table.keys();
    let targets: Vec<&TableKey> = indexes.iter().map(|index| &table_keys[*index]).collect();
    let mut seen = vec![HashSet::<Vec<u8>>::new(); targets.len()];
    let mut matched = Vec::new();
    let mut inserts = Vec::new();
    for (index, chunk) in input.iter().enumerate() {
        let chunk_keys = targets
            .iter()
            .map(|key| keys_of(key, chunk))
            .collect::<Result<Vec<_>>>()?;
        for row in 0..chunk.len() {
            let row_keys: Vec<Option<&Vec<u8>>> = chunk_keys
                .iter()
                .map(|keys| keys.get(row).and_then(Option::as_ref))
                .collect();
            let repeated = row_keys
                .iter()
                .zip(&seen)
                .any(|(key, seen)| key.is_some_and(|k| seen.contains(k)));
            if repeated {
                continue;
            }
            let mut position = None;
            for (t, (key, seen)) in row_keys.iter().zip(&mut seen).enumerate() {
                if let Some(key) = key {
                    position = position.or_else(|| keys[indexes[t]].get(*key).copied());
                    seen.insert((*key).clone());
                }
            }
            match position {
                Some(position) => matched.push((position, index, row)),
                None => inserts.push((index, row)),
            }
        }
    }
    Ok((matched, inserts))
}

/// The new contents of the rows that DO UPDATE changes, by position. The expressions read the
/// row of the table followed by the input row.
fn updated_rows(
    table: &TableCatalogEntry,
    action: &OnConflict,
    rows: &ColumnDataCollection,
    input: &[&DataChunk],
    matched: &[MatchedRow],
) -> Result<HashMap<usize, Vec<Value>>> {
    let column_count = table.columns.len();
    let mut types = table.types();
    types.extend(table.types());
    let stored: Vec<&DataChunk> = rows.chunks().collect();
    let mut updates = HashMap::with_capacity(matched.len());
    for batch in matched.chunks(STANDARD_VECTOR_SIZE) {
        let mut chunk = DataChunk::new(&types);
        for (position, input_chunk, input_row) in batch {
            let row = position % STANDARD_VECTOR_SIZE;
            let existing = stored[position / STANDARD_VECTOR_SIZE];
            let mut values: Vec<Value> = (0..column_count).map(|c| existing.column(c).get_value(row)).collect();
            values.extend((0..column_count).map(|c| input[*input_chunk].column(c).get_value(*input_row)));
            chunk.append_row(&values)?;
        }
        let condition = action.condition.as_ref().map(|c| execute(c, &chunk)).transpose()?;
        let sets = action
            .set_expressions
            .iter()
            .map(|e| execute(e, &chunk))
            .collect::<Result<Vec<_>>>()?;
        for (row, (position, ..)) in batch.iter().enumerate() {
            if let Some(condition) = &condition
                && !matches!(condition.get_value(row).data(), ValueData::Bool(true))
            {
                continue;
            }
            let mut values: Vec<Value> = (0..column_count).map(|c| chunk.column(c).get_value(row)).collect();
            for (set, column) in sets.iter().zip(&action.set_columns) {
                values[*column] = set.get_value(row);
            }
            updates.insert(*position, values);
        }
    }
    Ok(updates)
}
