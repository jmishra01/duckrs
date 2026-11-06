// Ported from duckdb/src/execution/operator/persistent/physical_merge_into.cpp @ rust-port-baseline
//
//! `PhysicalMergeInto`: sends each row of the join to the first action that takes it. C++ runs
//! the actions as operators of their own; here their changes come together in one rewrite of
//! the table at the end of the statement.

use super::constraints;
use super::expression_executor::execute;
use super::physical::{PhysicalOperator, SourceState, check_buffered, scan_collection};
use crate::catalog::{TableCatalogEntry, TableConstraint};
use crate::planner::expression::BoundExpr;
use crate::planner::logical::BoundMergeAction;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{MergeActionType, MergeCondition};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug)]
pub struct PhysicalMergeInto {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub table: Arc<TableCatalogEntry>,
    pub actions: Vec<(MergeCondition, Vec<BoundMergeAction>)>,
    pub row_id: BoundExpr,
    pub marker: Option<BoundExpr>,
    pub returning: bool,
    pub input: Mutex<ColumnDataCollection>,
    pub output: Mutex<ColumnDataCollection>,
}

/// What the actions decided on, gathered over all rows.
#[derive(Default)]
struct MergeChanges {
    updates: HashMap<i64, Vec<(usize, Value)>>,
    deletes: HashSet<i64>,
    inserts: Vec<Vec<Value>>,
    matched: HashSet<i64>,
}

fn sliced(chunk: &DataChunk, rows: &[u32]) -> DataChunk {
    let mut part = chunk.clone();
    part.slice(&SelectionVector::from_vec(rows.to_vec()), rows.len());
    part
}

impl PhysicalMergeInto {
    fn apply_action(
        &self,
        condition: MergeCondition,
        action: &BoundMergeAction,
        input: &DataChunk,
        changes: &mut MergeChanges,
    ) -> Result<()> {
        let ids = execute(&self.row_id, input)?;
        let id_of = |row: usize| ids.get_value(row).as_i128().map(|id| id as i64);
        if condition == MergeCondition::WhenMatched
            && matches!(action.action_type, MergeActionType::Update | MergeActionType::Delete)
        {
            for row in 0..input.len() {
                if let Some(id) = id_of(row)
                    && !changes.matched.insert(id)
                {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "MERGE INTO command cannot affect the same target row more than once. A target row matched \
                         more than one source row; ensure the source rows are deduplicated or the ON condition is \
                         unique.",
                    ));
                }
            }
        }
        let values = action
            .expressions
            .iter()
            .map(|e| execute(e, input))
            .collect::<Result<Vec<_>>>()?;
        match action.action_type {
            MergeActionType::DoNothing => {}
            MergeActionType::Error => {
                let mut message = action.text.clone();
                if let Some(first) = values.first() {
                    let value = first.get_value(0);
                    let text = if value.is_null() {
                        "NULL".to_string()
                    } else {
                        value.try_to_string()?
                    };
                    message += &format!(": {text}");
                }
                return Err(Error::new(
                    ErrorKind::Constraint,
                    format!("Merge error condition {message}"),
                ));
            }
            MergeActionType::Delete => {
                for row in 0..input.len() {
                    if let Some(id) = id_of(row) {
                        changes.deletes.insert(id);
                    }
                }
            }
            MergeActionType::Update => {
                for row in 0..input.len() {
                    let Some(id) = id_of(row) else {
                        continue;
                    };
                    let row_values = action
                        .columns
                        .iter()
                        .zip(&values)
                        .map(|(column, vector)| (*column, vector.get_value(row)))
                        .collect();
                    changes.updates.insert(id, row_values);
                }
            }
            MergeActionType::Insert => {
                for row in 0..input.len() {
                    changes.inserts.push(values.iter().map(|v| v.get_value(row)).collect());
                }
            }
        }
        Ok(())
    }

    fn apply_chunk(&self, chunk: &DataChunk, changes: &mut MergeChanges) -> Result<()> {
        let ids = execute(&self.row_id, chunk)?;
        let marker = self.marker.as_ref().map(|m| execute(m, chunk)).transpose()?;
        for (condition, actions) in &self.actions {
            let mut rows: Vec<u32> = (0..chunk.len() as u32)
                .filter(|row| {
                    let has_target = !ids.get_value(*row as usize).is_null();
                    let has_source = marker.as_ref().is_none_or(|m| !m.get_value(*row as usize).is_null());
                    match condition {
                        MergeCondition::WhenMatched => has_target && has_source,
                        MergeCondition::WhenNotMatchedByTarget => !has_target,
                        MergeCondition::WhenNotMatchedBySource => !has_source,
                    }
                })
                .collect();
            for action in actions {
                if rows.is_empty() {
                    break;
                }
                let input = sliced(chunk, &rows);
                let (taken, rest): (Vec<u32>, Vec<u32>) = match &action.condition {
                    Some(expr) => {
                        let result = execute(expr, &input)?;
                        let mut taken = Vec::new();
                        let mut rest = Vec::new();
                        for (position, row) in rows.iter().enumerate() {
                            if matches!(result.get_value(position).data(), ValueData::Bool(true)) {
                                taken.push(*row);
                            } else {
                                rest.push(*row);
                            }
                        }
                        (taken, rest)
                    }
                    None => (rows.clone(), Vec::new()),
                };
                rows = rest;
                if !taken.is_empty() {
                    self.apply_action(*condition, action, &sliced(chunk, &taken), changes)?;
                }
            }
        }
        Ok(())
    }

    /// Rewrites the table. Rows that change keys move to the end, like a DELETE and an INSERT.
    fn apply_changes(&self, changes: MergeChanges) -> Result<()> {
        let table = &self.table;
        let types = table.types();
        let mut storage = table.write_storage()?;
        let count = storage.len();
        let row_ids = table.row_ids(count);
        let position = |id: i64| row_ids.position(id as u64);
        let deletes: HashSet<usize> = changes.deletes.iter().filter_map(|id| position(*id)).collect();
        let updates: HashMap<usize, _> = changes
            .updates
            .iter()
            .filter_map(|(id, update)| Some((position(*id)?, update)))
            .collect();
        table.claim_rows(deletes.iter().copied(), count, false)?;
        table.claim_rows(updates.keys().copied(), count, true)?;
        let has_keys = !table.indexes.read().is_empty()
            || !table.referenced_by.lock().is_empty()
            || table
                .constraints
                .iter()
                .any(|c| !matches!(c, TableConstraint::Check { .. }));
        let mut rewritten = ColumnDataCollection::new(types.clone());
        let mut changed = ColumnDataCollection::new(types.clone());
        let mut deleted = ColumnDataCollection::new(types.clone());
        let mut returned: Vec<(Vec<Value>, &'static str)> = Vec::new();
        let mut moved = Vec::new();
        for (index, chunk) in storage.chunks().enumerate() {
            let first_row = index * STANDARD_VECTOR_SIZE;
            let touched =
                (0..chunk.len()).any(|r| updates.contains_key(&(first_row + r)) || deletes.contains(&(first_row + r)));
            if !touched {
                rewritten.append(chunk);
                continue;
            }
            let mut output = DataChunk::new(&types);
            let mut gone = DataChunk::new(&types);
            for row in 0..chunk.len() {
                let id = first_row + row;
                let mut values: Vec<Value> = (0..types.len()).map(|c| chunk.column(c).get_value(row)).collect();
                if deletes.contains(&id) {
                    gone.append_row(&values)?;
                    returned.push((values, "DELETE"));
                    continue;
                }
                if let Some(update) = updates.get(&id) {
                    for (column, value) in update.iter() {
                        values[*column] = value.clone();
                    }
                    returned.push((values.clone(), "UPDATE"));
                    if has_keys {
                        moved.push(values);
                        continue;
                    }
                    let mut single = DataChunk::new(&types);
                    single.append_row(&values)?;
                    verify_written(table, &single)?;
                    changed.append(&single);
                }
                output.append_row(&values)?;
            }
            rewritten.append(&output);
            deleted.append(&gone);
        }
        for values in moved.into_iter().chain(changes.inserts.iter().cloned()) {
            let mut single = DataChunk::new(&types);
            single.append_row(&values)?;
            verify_written(table, &single)?;
            rewritten.append(&single);
            changed.append(&single);
        }
        for values in &changes.inserts {
            returned.push((values.clone(), "INSERT"));
        }
        constraints::verify_delete(table, &storage, &deleted)?;
        if has_keys {
            constraints::verify_key_update(table, &storage, &rewritten, &changed)?;
        }
        *storage = rewritten;
        check_buffered(&storage)?;
        *table.unique_keys.lock() = None;
        let updated = updates.keys().filter(|_| has_keys);
        let mut removed: Vec<usize> = deletes.iter().chain(updated).copied().collect();
        removed.sort_unstable();
        removed.dedup();
        table.rows_moved(&removed, count);
        if !has_keys {
            table.rows_updated_in_place(updates.keys().copied());
        }
        drop(storage);
        if !has_keys {
            let columns: Vec<usize> = updates
                .values()
                .flat_map(|update| update.iter().map(|(c, _)| *c))
                .collect();
            table.rows_updated(updates.keys().copied(), &columns, count);
        }

        let mut output = self.output.lock();
        if self.returning {
            for rows in returned.chunks(STANDARD_VECTOR_SIZE) {
                let mut chunk = DataChunk::new(&self.types);
                for (values, action) in rows {
                    let mut row = values.clone();
                    row.push(Value::varchar(*action));
                    chunk.append_row(&row)?;
                }
                output.append(&chunk);
            }
        } else {
            let mut chunk = DataChunk::new(&self.types);
            chunk.append_row(&[Value::bigint(returned.len() as i64)])?;
            output.append(&chunk);
        }
        Ok(())
    }
}

fn verify_written(table: &TableCatalogEntry, chunk: &DataChunk) -> Result<()> {
    constraints::verify_not_null(table, chunk)?;
    constraints::verify_checks(table, chunk)
}

impl PhysicalOperator for PhysicalMergeInto {
    fn name(&self) -> &'static str {
        "MERGE_INTO"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn is_sink(&self) -> bool {
        true
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        self.input.lock().append(chunk);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let input = self.input.lock();
        let mut changes = MergeChanges::default();
        for chunk in input.chunks() {
            self.apply_chunk(chunk, &mut changes)?;
        }
        self.apply_changes(changes)
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        Ok(scan_collection(&self.output, state))
    }
}
