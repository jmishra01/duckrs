// Ported from duckdb/src/execution/operator/join/{physical_asof_join,physical_positional_join}.cpp
// and the MARK handling of physical_hash_join.cpp @ rust-port-baseline
//
//! The joins that are not about pairs of equal keys: ASOF, POSITIONAL and MARK. They keep the
//! right side's rows and work row by row: the morsel-wise versions come with Phase 8.

use super::expression_executor::execute;
use super::physical::{OperatorResult, OperatorState, PhysicalOperator, SourceState, check_buffered_rows, group_keys};
use crate::planner::expression::BoundExpr;
use crate::planner::logical::JoinCondition;
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{ComparisonOp, JoinType};
use parking_lot::Mutex;
use std::collections::HashMap;

fn row_values(chunk: &DataChunk, row: usize) -> Vec<Value> {
    chunk.columns().iter().map(|c| c.get_value(row)).collect()
}

fn nulls(types: &[LogicalType]) -> Vec<Value> {
    types.iter().map(|t| Value::null(t.clone())).collect()
}

/// `PhysicalAsOfJoin`: each left row meets the closest right row with equal `conditions`, on the
/// right side of the inequality, of the ones `predicate` lets through.
#[derive(Debug)]
pub struct PhysicalAsOfJoin {
    pub types: Vec<LogicalType>,
    pub join_type: JoinType,
    pub conditions: Vec<JoinCondition>,
    /// left `op` right
    pub inequality: (BoundExpr, ComparisonOp, BoundExpr),
    pub predicate: Option<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub build: Mutex<AsOfBuild>,
}

#[derive(Debug, Default)]
pub struct AsOfBuild {
    rows: Vec<Vec<Value>>,
    /// per key, the order key and row of the rows that can match, sorted
    groups: HashMap<Vec<u8>, Vec<(Vec<u8>, u32)>>,
    matched: Vec<bool>,
}

impl PhysicalAsOfJoin {
    /// The key of each row, `None` when a key column that is not null-safe is NULL.
    fn keys(&self, chunk: &DataChunk, left: bool) -> Result<Vec<Option<Vec<u8>>>> {
        if self.conditions.is_empty() {
            return Ok(vec![Some(Vec::new()); chunk.len()]);
        }
        let vectors = self
            .conditions
            .iter()
            .map(|c| execute(if left { &c.left } else { &c.right }, chunk))
            .collect::<Result<Vec<_>>>()?;
        let references: Vec<&Vector> = vectors.iter().collect();
        let keys = group_keys(&references, chunk.len())?;
        let checked: Vec<_> = self
            .conditions
            .iter()
            .zip(&vectors)
            .filter(|(c, _)| !c.null_safe)
            .map(|(_, v)| v.to_unified(chunk.len()))
            .collect();
        Ok(keys
            .into_iter()
            .enumerate()
            .map(|(row, key)| checked.iter().all(|u| u.is_valid(row)).then_some(key))
            .collect())
    }

    fn order_keys(&self, chunk: &DataChunk, expr: &BoundExpr) -> Result<Vec<Option<Vec<u8>>>> {
        let vector = execute(expr, chunk)?;
        let keys = group_keys(&[&vector], chunk.len())?;
        let unified = vector.to_unified(chunk.len());
        Ok(keys
            .into_iter()
            .enumerate()
            .map(|(row, key)| unified.is_valid(row).then_some(key))
            .collect())
    }

    /// The right rows `left_key` can pair with, closest first.
    fn candidates<'b>(&self, group: &'b [(Vec<u8>, u32)], key: &[u8]) -> Box<dyn Iterator<Item = u32> + 'b> {
        let (_, op, _) = &self.inequality;
        match op {
            ComparisonOp::GreaterThanOrEqual => {
                let end = group.partition_point(|e| e.0.as_slice() <= key);
                Box::new(group[..end].iter().rev().map(|e| e.1))
            }
            ComparisonOp::GreaterThan => {
                let end = group.partition_point(|e| e.0.as_slice() < key);
                Box::new(group[..end].iter().rev().map(|e| e.1))
            }
            ComparisonOp::LessThanOrEqual => {
                let start = group.partition_point(|e| e.0.as_slice() < key);
                Box::new(group[start..].iter().map(|e| e.1))
            }
            _ => {
                let start = group.partition_point(|e| e.0.as_slice() <= key);
                Box::new(group[start..].iter().map(|e| e.1))
            }
        }
    }

    fn passes(&self, left: &[Value], right: &[Value]) -> Result<bool> {
        let Some(predicate) = &self.predicate else {
            return Ok(true);
        };
        let mut chunk = DataChunk::new(&self.types);
        let mut row = left.to_vec();
        row.extend_from_slice(right);
        chunk.append_row(&row)?;
        let result = execute(predicate, &chunk)?;
        Ok(matches!(result.get_value(0).data(), ValueData::Bool(true)))
    }
}

impl PhysicalOperator for PhysicalAsOfJoin {
    fn explain_details(&self) -> Vec<String> {
        vec![format!("Join Type: {}", self.join_type.name())]
    }
    fn name(&self) -> &'static str {
        "ASOF_JOIN"
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
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let keys = self.keys(chunk, false)?;
        let orders = self.order_keys(chunk, &self.inequality.2)?;
        let mut build = self.build.lock();
        for (row, (key, order)) in keys.into_iter().zip(orders).enumerate() {
            let index = build.rows.len() as u32;
            build.rows.push(row_values(chunk, row));
            if let (Some(key), Some(order)) = (key, order) {
                build.groups.entry(key).or_default().push((order, index));
            }
        }
        check_buffered_rows(self.children[1].types(), build.rows.len())
    }
    fn finalize(&self) -> Result<()> {
        let mut build = self.build.lock();
        for group in build.groups.values_mut() {
            group.sort();
        }
        build.matched = vec![false; build.rows.len()];
        Ok(())
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(())
    }
    fn execute_stateful(&self, input: &DataChunk, _state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let keys = self.keys(input, true)?;
        let orders = self.order_keys(input, &self.inequality.0)?;
        let mut build = self.build.lock();
        let build = &mut *build;
        let right_types = self.children[1].types();
        let mut output = DataChunk::new(&self.types);
        for row in 0..input.len() {
            let left = row_values(input, row);
            let mut found = None;
            if let (Some(key), Some(order)) = (&keys[row], &orders[row])
                && let Some(group) = build.groups.get(key)
            {
                for candidate in self.candidates(group, order) {
                    if self.passes(&left, &build.rows[candidate as usize])? {
                        found = Some(candidate);
                        break;
                    }
                }
            }
            let right = match found {
                Some(candidate) => {
                    build.matched[candidate as usize] = true;
                    build.rows[candidate as usize].clone()
                }
                None if matches!(self.join_type, JoinType::Left | JoinType::Outer) => nulls(right_types),
                None => continue,
            };
            let mut values = left;
            values.extend(right);
            output.append_row(&values)?;
        }
        Ok((output, OperatorResult::NeedMoreInput))
    }
    fn has_unmatched_source(&self) -> bool {
        matches!(self.join_type, JoinType::Right | JoinType::Outer)
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let next = state.downcast_mut::<usize>().expect("join source state");
        let build = self.build.lock();
        let mut output = DataChunk::new(&self.types);
        while *next < build.rows.len() && output.len() < STANDARD_VECTOR_SIZE {
            if !build.matched[*next] {
                let mut values = nulls(self.children[0].types());
                values.extend_from_slice(&build.rows[*next]);
                output.append_row(&values)?;
            }
            *next += 1;
        }
        Ok((!output.is_empty()).then_some(output))
    }
}

/// `PhysicalPositionalJoin`: the rows of both sides, side by side, in order. The shorter side
/// runs out into NULLs.
#[derive(Debug)]
pub struct PhysicalPositionalJoin {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub rows: Mutex<Vec<Vec<Value>>>,
    pub position: Mutex<usize>,
}

impl PhysicalOperator for PhysicalPositionalJoin {
    fn name(&self) -> &'static str {
        "POSITIONAL_JOIN"
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
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let mut rows = self.rows.lock();
        rows.extend((0..chunk.len()).map(|row| row_values(chunk, row)));
        check_buffered_rows(self.children[1].types(), rows.len())
    }
    fn execute_stateful(&self, input: &DataChunk, _state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let rows = self.rows.lock();
        let mut position = self.position.lock();
        let right_types = self.children[1].types();
        let mut output = DataChunk::new(&self.types);
        for row in 0..input.len() {
            let mut values = row_values(input, row);
            match rows.get(*position) {
                Some(right) => values.extend_from_slice(right),
                None => values.extend(nulls(right_types)),
            }
            *position += 1;
            output.append_row(&values)?;
        }
        Ok((output, OperatorResult::NeedMoreInput))
    }
    fn has_unmatched_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(())
    }
    fn get_data(&self, _state: &mut SourceState) -> Result<Option<DataChunk>> {
        let rows = self.rows.lock();
        let mut position = self.position.lock();
        let mut output = DataChunk::new(&self.types);
        while *position < rows.len() && output.len() < STANDARD_VECTOR_SIZE {
            let mut values = nulls(self.children[0].types());
            values.extend_from_slice(&rows[*position]);
            output.append_row(&values)?;
            *position += 1;
        }
        Ok((!output.is_empty()).then_some(output))
    }
}

/// A MARK join: the left rows and whether the predicate holds for some right row: TRUE if for
/// one it does, NULL if for none it does but it is NULL for one, and FALSE otherwise.
#[derive(Debug)]
pub struct PhysicalMarkJoin {
    pub types: Vec<LogicalType>,
    pub predicate: Option<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub rows: Mutex<Vec<DataChunk>>,
}

impl PhysicalMarkJoin {
    fn mark(&self, input: &DataChunk, row: usize, right: &[DataChunk]) -> Result<Value> {
        let Some(predicate) = &self.predicate else {
            return Ok(Value::boolean(right.iter().any(|c| !c.is_empty())));
        };
        let mut has_null = false;
        for chunk in right {
            if chunk.is_empty() {
                continue;
            }
            let count = chunk.len();
            let mut left = input.clone();
            left.slice(&SelectionVector::from_vec(vec![row as u32; count]), count);
            let mut columns = left.columns().to_vec();
            columns.extend_from_slice(chunk.columns());
            let pairs = DataChunk::from_columns(columns, count);
            let result = execute(predicate, &pairs)?;
            for i in 0..count {
                match result.get_value(i).data() {
                    ValueData::Bool(true) => return Ok(Value::boolean(true)),
                    ValueData::Bool(false) => {}
                    _ => has_null = true,
                }
            }
        }
        Ok(if has_null {
            Value::null(LogicalType::BOOLEAN)
        } else {
            Value::boolean(false)
        })
    }
}

impl PhysicalOperator for PhysicalMarkJoin {
    fn explain_details(&self) -> Vec<String> {
        vec!["Join Type: MARK".to_string()]
    }
    fn name(&self) -> &'static str {
        "BLOCKWISE_NL_JOIN"
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
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        self.rows.lock().push(chunk.clone());
        Ok(())
    }
    fn execute_stateful(&self, input: &DataChunk, _state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let rows = self.rows.lock();
        let mut output = DataChunk::new(&self.types);
        for row in 0..input.len() {
            let mut values = row_values(input, row);
            values.push(self.mark(input, row, &rows)?);
            output.append_row(&values)?;
        }
        Ok((output, OperatorResult::NeedMoreInput))
    }
}
