// Ported from duckdb/src/execution/physical_operator.cpp and
// duckdb/src/execution/operator/{scan/physical_table_scan,scan/physical_dummy_scan,
// scan/physical_expression_scan,filter/physical_filter,projection/physical_projection,
// persistent/physical_insert,schema/physical_create_table,helper/physical_result_collector}.cpp
// @ rust-port-baseline
//
//! Physical operators. Like DuckDB, an operator can be a source (produces chunks), an
//! operator (transforms chunks as they stream through) and a sink (consumes all chunks),
//! and pipelines are built from those roles. Execution is single-threaded for now, so the
//! global and local states are one.

use super::expression_executor::{execute, select};
use crate::catalog::{ColumnDefinition, OnCreateConflict, SchemaCatalogEntry, TableCatalogEntry};
use crate::function::aggregates::{AggregateState, value_keys};
use crate::planner::expression::{BoundAggregate, BoundExpr};
use crate::planner::logical::{BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys, sort_collection};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::sql_identifier;
use parking_lot::Mutex;
use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// The state a source keeps between `get_data` calls (`GlobalSourceState`).
pub type SourceState = Box<dyn Any + Send>;

/// The state an operator keeps while it works through an input chunk (`OperatorState`).
pub type OperatorState = Box<dyn Any + Send>;

/// What an operator wants after producing a chunk (`OperatorResultType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorResult {
    NeedMoreInput,
    /// call it again with the same input
    HaveMoreOutput,
}

pub trait PhysicalOperator: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    fn types(&self) -> &[LogicalType];
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &[]
    }

    // Source interface
    fn is_source(&self) -> bool {
        false
    }
    fn source_state(&self) -> SourceState {
        Box::new(())
    }
    /// The next chunk, or `None` when the source is exhausted.
    fn get_data(&self, _state: &mut SourceState) -> Result<Option<DataChunk>> {
        unreachable!("{} is not a source", self.name())
    }

    // Operator interface: one input chunk becomes one output chunk
    fn execute(&self, _input: DataChunk) -> Result<DataChunk> {
        unreachable!("{} is not an operator", self.name())
    }

    fn operator_state(&self) -> OperatorState {
        Box::new(())
    }

    /// Like `execute`, for an operator that can turn one input chunk into many.
    fn execute_stateful(&self, input: &DataChunk, _state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        Ok((self.execute(input.clone())?, OperatorResult::NeedMoreInput))
    }

    /// Whether the operator needs no more input, so that its pipeline can stop early.
    fn is_finished(&self) -> bool {
        false
    }

    // Sink interface
    fn is_sink(&self) -> bool {
        false
    }
    fn sink(&self, _chunk: &DataChunk) -> Result<()> {
        unreachable!("{} is not a sink", self.name())
    }
    fn finalize(&self) -> Result<()> {
        Ok(())
    }
    /// Which child feeds the sink. Joins are sinks for their right child, and the left child
    /// streams through them.
    fn sink_child(&self) -> usize {
        0
    }

    /// Whether a join has rows to emit through `get_data` after its input is done
    /// (`IsSource` of RIGHT and FULL joins).
    fn has_unmatched_source(&self) -> bool {
        false
    }
}

/// `PhysicalTableScan` over a table's in-memory rows.
#[derive(Debug)]
pub struct PhysicalTableScan {
    pub table: Arc<TableCatalogEntry>,
    pub column_ids: Vec<usize>,
    pub types: Vec<LogicalType>,
}

impl PhysicalOperator for PhysicalTableScan {
    fn name(&self) -> &'static str {
        "TABLE_SCAN"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    /// The scan sees the rows the table had when it started, like a transaction's snapshot
    /// (so that `INSERT INTO t FROM t` ends): the next chunk and how many rows remain.
    fn source_state(&self) -> SourceState {
        Box::new((0usize, self.table.storage.read().len()))
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let (next, remaining) = state.downcast_mut::<(usize, usize)>().expect("table scan state");
        if *remaining == 0 {
            return Ok(None);
        }
        let storage = self.table.storage.read();
        let chunk = storage.chunk(*next);
        // all chunks but the last are full, so that a row's position gives its id
        let first_row = (*next * STANDARD_VECTOR_SIZE) as i64;
        *next += 1;
        let count = chunk.len().min(*remaining);
        *remaining -= count;
        let columns = self
            .column_ids
            .iter()
            .map(|&i| {
                if i == COLUMN_IDENTIFIER_ROW_ID {
                    Vector::sequence(LogicalType::BIGINT, first_row, 1)
                } else {
                    Ok(chunk.column(i).clone())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let mut result = DataChunk::from_columns(columns, chunk.len());
        if count < chunk.len() {
            result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
        }
        Ok(Some(result))
    }
}

/// How much a blocking operator or table may hold, roughly. Everything lives in memory until
/// out-of-core operators (Phase 8) and storage (Phase 9) arrive, so more than this fails
/// instead of exhausting the machine.
const MAX_BUFFERED_BYTES: usize = 2 << 30;

pub(super) fn check_buffered(collection: &ColumnDataCollection) -> Result<()> {
    check_buffered_rows(collection.types(), collection.len())
}

pub(super) fn check_buffered_rows(types: &[LogicalType], rows: usize) -> Result<()> {
    let row_size: usize = types.iter().map(|t| t.physical_type().size().max(8)).sum();
    if rows.saturating_mul(row_size) > MAX_BUFFERED_BYTES {
        return Err(crate::unported(
            "keeping more than 2 GiB of rows, which needs spilling to disk,",
        ));
    }
    Ok(())
}

/// A key per row that is equal exactly for rows with equal values, NULLs included.
pub(super) fn group_keys(columns: &[&Vector], count: usize) -> Result<Vec<Vec<u8>>> {
    let modifiers = OrderModifiers {
        order: OrderType::Ascending,
        nulls: NullOrder::NullsLast,
    };
    let columns: Vec<(&Vector, OrderModifiers)> = columns.iter().map(|&v| (v, modifiers)).collect();
    create_sort_keys(&columns, count)
}

/// `PhysicalEmptyResult`.
#[derive(Debug)]
pub struct PhysicalEmptyResult {
    pub types: Vec<LogicalType>,
}

impl PhysicalOperator for PhysicalEmptyResult {
    fn name(&self) -> &'static str {
        "EMPTY_RESULT"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn get_data(&self, _state: &mut SourceState) -> Result<Option<DataChunk>> {
        Ok(None)
    }
}

/// The `range` and `generate_series` table functions.
#[derive(Debug)]
pub struct PhysicalRange {
    pub types: Vec<LogicalType>,
    pub start: i128,
    pub increment: i128,
    pub count: u64,
}

impl PhysicalOperator for PhysicalRange {
    fn name(&self) -> &'static str {
        "RANGE"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0u64)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<u64>().expect("range state");
        if *done >= self.count {
            return Ok(None);
        }
        let count = (self.count - *done).min(STANDARD_VECTOR_SIZE as u64) as usize;
        let mut column = Vector::new(LogicalType::BIGINT, count);
        let first = self.start + *done as i128 * self.increment;
        for (i, slot) in column.data_mut::<i64>().iter_mut().enumerate().take(count) {
            *slot = (first + i as i128 * self.increment) as i64;
        }
        *done += count as u64;
        Ok(Some(DataChunk::from_columns(vec![column], count)))
    }
}

/// `PhysicalDummyScan`: one row, for a SELECT without FROM.
#[derive(Debug)]
pub struct PhysicalDummyScan {
    pub types: Vec<LogicalType>,
}

impl PhysicalOperator for PhysicalDummyScan {
    fn name(&self) -> &'static str {
        "DUMMY_SCAN"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("dummy scan state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let columns = self.types.iter().map(|t| Vector::constant_null(t.clone())).collect();
        Ok(Some(DataChunk::from_columns(columns, 1)))
    }
}

/// `PhysicalExpressionScan`: the rows of a VALUES list, evaluated in order.
#[derive(Debug)]
pub struct PhysicalExpressionScan {
    pub types: Vec<LogicalType>,
    pub expressions: Vec<Vec<BoundExpr>>,
}

impl PhysicalOperator for PhysicalExpressionScan {
    fn name(&self) -> &'static str {
        "EXPRESSION_SCAN"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let next = state.downcast_mut::<usize>().expect("expression scan state");
        if *next >= self.expressions.len() {
            return Ok(None);
        }
        let end = (*next + STANDARD_VECTOR_SIZE).min(self.expressions.len());
        let mut chunk = DataChunk::new(&self.types);
        let single_row = DataChunk::from_columns(Vec::new(), 1);
        for row in &self.expressions[*next..end] {
            let values = row
                .iter()
                .map(|e| Ok(execute(e, &single_row)?.get_value(0)))
                .collect::<Result<Vec<Value>>>()?;
            chunk.append_row(&values)?;
        }
        *next = end;
        Ok(Some(chunk))
    }
}

/// `PhysicalFilter`.
#[derive(Debug)]
pub struct PhysicalFilter {
    pub types: Vec<LogicalType>,
    pub condition: BoundExpr,
    pub children: Vec<Box<dyn PhysicalOperator>>,
}

impl PhysicalOperator for PhysicalFilter {
    fn name(&self) -> &'static str {
        "FILTER"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn execute(&self, mut input: DataChunk) -> Result<DataChunk> {
        let (sel, count) = select(&self.condition, &input)?;
        if count != input.len() {
            input.slice(&sel, count);
        }
        Ok(input)
    }
}

/// `PhysicalProjection`.
#[derive(Debug)]
pub struct PhysicalProjection {
    pub types: Vec<LogicalType>,
    pub expressions: Vec<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
}

impl PhysicalOperator for PhysicalProjection {
    fn name(&self) -> &'static str {
        "PROJECTION"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn execute(&self, input: DataChunk) -> Result<DataChunk> {
        let columns = self
            .expressions
            .iter()
            .map(|e| execute(e, &input))
            .collect::<Result<Vec<_>>>()?;
        Ok(DataChunk::from_columns(columns, input.len()))
    }
}

/// The input column a resolved ORDER BY or DISTINCT expression reads.
fn reference_index(expr: &BoundExpr) -> usize {
    match expr.kind {
        crate::planner::expression::BoundExprKind::Reference(i) => i,
        _ => unreachable!("ORDER BY and DISTINCT targets are columns of their input"),
    }
}

/// Emits the chunks of a collection one by one (the source half of a blocking operator).
fn scan_collection(collection: &Mutex<ColumnDataCollection>, state: &mut SourceState) -> Option<DataChunk> {
    let next = state.downcast_mut::<usize>().expect("collection scan state");
    let collection = collection.lock();
    if *next >= collection.chunk_count() {
        return None;
    }
    let chunk = collection.chunk(*next).clone();
    *next += 1;
    Some(chunk)
}

/// `PhysicalOrder`: gathers its input (sink), sorts it, and emits it in order (source).
#[derive(Debug)]
pub struct PhysicalOrder {
    pub types: Vec<LogicalType>,
    pub orders: Vec<(usize, OrderModifiers)>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalOrder {
    pub fn new(orders: Vec<BoundOrderByNode>, child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            orders: orders.iter().map(|o| (reference_index(&o.expr), o.modifiers)).collect(),
            data: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            children: vec![child],
        }
    }
}

impl PhysicalOperator for PhysicalOrder {
    fn name(&self) -> &'static str {
        "ORDER_BY"
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
        let mut data = self.data.lock();
        data.append(chunk);
        check_buffered(&data)
    }
    fn finalize(&self) -> Result<()> {
        let mut data = self.data.lock();
        *data = sort_collection(&data, &self.orders)?;
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        Ok(scan_collection(&self.data, state))
    }
}

/// `PhysicalStreamingLimit`: skips `offset` rows, then passes `limit` rows on.
#[derive(Debug)]
pub struct PhysicalLimit {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    /// rows still to skip, and rows still to pass (`None`: all)
    pub remaining: Mutex<(u64, Option<u64>)>,
}

impl PhysicalOperator for PhysicalLimit {
    fn name(&self) -> &'static str {
        "LIMIT"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn execute(&self, mut input: DataChunk) -> Result<DataChunk> {
        let mut remaining = self.remaining.lock();
        let (offset, limit) = &mut *remaining;
        let len = input.len() as u64;
        let skip = (*offset).min(len);
        *offset -= skip;
        let take = limit.map_or(len - skip, |l| l.min(len - skip));
        if let Some(l) = limit {
            *l -= take;
        }
        if skip > 0 || take < len {
            let sel = SelectionVector::from_vec((skip..skip + take).map(|i| i as u32).collect());
            input.slice(&sel, take as usize);
        }
        Ok(input)
    }
    fn is_finished(&self) -> bool {
        self.remaining.lock().1 == Some(0)
    }
}

/// `PhysicalHashAggregate` as used for DISTINCT: keeps the first row of each distinct value
/// of the targets.
#[derive(Debug)]
pub struct PhysicalDistinct {
    pub types: Vec<LogicalType>,
    pub targets: Vec<usize>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub seen: Mutex<std::collections::HashSet<Vec<u8>>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalDistinct {
    pub fn new(targets: &[BoundExpr], child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            targets: targets.iter().map(reference_index).collect(),
            seen: Mutex::new(std::collections::HashSet::new()),
            data: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            children: vec![child],
        }
    }
}

impl PhysicalOperator for PhysicalDistinct {
    fn name(&self) -> &'static str {
        "HASH_GROUP_BY"
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
        let columns: Vec<&Vector> = self.targets.iter().map(|&i| chunk.column(i)).collect();
        let keys = group_keys(&columns, chunk.len())?;
        let mut seen = self.seen.lock();
        let new_rows: Vec<u32> = keys
            .into_iter()
            .enumerate()
            .filter(|(_, key)| seen.insert(key.clone()))
            .map(|(i, _)| i as u32)
            .collect();
        if !new_rows.is_empty() {
            let mut rows = chunk.clone();
            let count = new_rows.len();
            rows.slice(&SelectionVector::from_vec(new_rows), count);
            let mut data = self.data.lock();
            data.append(&rows);
            check_buffered(&data)?;
        }
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        Ok(scan_collection(&self.data, state))
    }
}

/// `PhysicalHashAggregate` and `PhysicalUngroupedAggregate`: computes aggregates per group
/// (sink) and emits a row per group (source). Groups are found by the sort key of their
/// values, so NULLs group together and equal values compare like DuckDB's.
#[derive(Debug)]
pub struct PhysicalAggregate {
    pub types: Vec<LogicalType>,
    pub groups: Vec<BoundExpr>,
    pub aggregates: Vec<BoundAggregate>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub state: Mutex<AggregateSinkState>,
}

/// The groups so far and their aggregate states.
#[derive(Debug)]
pub struct AggregateSinkState {
    group_ids: std::collections::HashMap<Vec<u8>, usize>,
    group_values: ColumnDataCollection,
    group_count: usize,
    /// per aggregate, the states of the groups
    states: Vec<Vec<AggregateState>>,
    /// per aggregate, per group, the values a DISTINCT aggregate has seen
    distinct_seen: Vec<std::collections::HashSet<(usize, Vec<u8>)>>,
    /// per aggregate with ORDER BY, its rows, which it sees once they can be put in order
    ordered: Vec<Vec<OrderedRow>>,
    output: Vec<DataChunk>,
}

/// A row an aggregate with ORDER BY has yet to see.
#[derive(Debug)]
struct OrderedRow {
    group: usize,
    key: Vec<u8>,
    /// the aggregate's input; `None` for `count(*)`
    value: Option<Value>,
}

impl PhysicalAggregate {
    pub fn new(
        types: Vec<LogicalType>,
        groups: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let group_types: Vec<LogicalType> = groups.iter().map(|g| g.return_type.clone()).collect();
        // an ungrouped aggregate has its one group from the start
        let group_count = usize::from(groups.is_empty());
        let state = AggregateSinkState {
            group_ids: std::collections::HashMap::new(),
            group_values: ColumnDataCollection::new(group_types),
            group_count,
            states: aggregates
                .iter()
                .map(|a| vec![a.function.kind.initial_state(); group_count])
                .collect(),
            distinct_seen: vec![std::collections::HashSet::new(); aggregates.len()],
            ordered: aggregates.iter().map(|_| Vec::new()).collect(),
            output: Vec::new(),
        };
        Self {
            types,
            groups,
            aggregates,
            children,
            state: Mutex::new(state),
        }
    }

    /// The group of each row of `chunk`, creating groups as needed.
    fn find_groups(&self, chunk: &DataChunk, state: &mut AggregateSinkState) -> Result<Vec<usize>> {
        if self.groups.is_empty() {
            return Ok(vec![0; chunk.len()]);
        }
        let vectors = self
            .groups
            .iter()
            .map(|g| execute(g, chunk))
            .collect::<Result<Vec<_>>>()?;
        let keys = group_keys(&vectors.iter().collect::<Vec<_>>(), chunk.len())?;
        let mut new_rows = Vec::new();
        let mut ids = Vec::with_capacity(keys.len());
        for (row, key) in keys.into_iter().enumerate() {
            let next = state.group_count;
            let id = *state.group_ids.entry(key).or_insert(next);
            if id == next {
                state.group_count += 1;
                new_rows.push(row as u32);
            }
            ids.push(id);
        }
        if !new_rows.is_empty() {
            let count = new_rows.len();
            let mut values = DataChunk::from_columns(vectors, chunk.len());
            values.slice(&SelectionVector::from_vec(new_rows), count);
            state.group_values.append(&values);
            check_buffered(&state.group_values)?;
            for (states, aggregate) in state.states.iter_mut().zip(&self.aggregates) {
                states.resize(state.group_count, aggregate.function.kind.initial_state());
            }
        }
        Ok(ids)
    }
}

impl PhysicalOperator for PhysicalAggregate {
    fn name(&self) -> &'static str {
        if self.groups.is_empty() {
            "UNGROUPED_AGGREGATE"
        } else {
            "HASH_GROUP_BY"
        }
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
        let mut state = self.state.lock();
        let state = &mut *state;
        let ids = self.find_groups(chunk, state)?;
        for (i, aggregate) in self.aggregates.iter().enumerate() {
            let mut rows: Vec<(usize, usize)> = match &aggregate.filter {
                Some(filter) => {
                    let (sel, count) = select(filter, chunk)?;
                    (0..count).map(|r| (sel.get(r), ids[sel.get(r)])).collect()
                }
                None => ids.iter().copied().enumerate().collect(),
            };
            let input = aggregate.children.first().map(|c| execute(c, chunk)).transpose()?;
            if let (true, Some(input)) = (aggregate.distinct, &input) {
                // each value counts once per group; NULLs are not values
                let keys = value_keys(input, chunk.len())?;
                let unified = input.to_unified(chunk.len());
                let seen = &mut state.distinct_seen[i];
                rows.retain(|&(row, group)| unified.is_valid(row) && seen.insert((group, keys[row].clone())));
            }
            if !aggregate.order_by.is_empty() {
                let vectors = aggregate
                    .order_by
                    .iter()
                    .map(|(expr, _)| execute(expr, chunk))
                    .collect::<Result<Vec<_>>>()?;
                let columns: Vec<(&Vector, OrderModifiers)> = vectors
                    .iter()
                    .zip(&aggregate.order_by)
                    .map(|(vector, (_, modifiers))| (vector, *modifiers))
                    .collect();
                let keys = create_sort_keys(&columns, chunk.len())?;
                let ordered = &mut state.ordered[i];
                for &(row, group) in &rows {
                    ordered.push(OrderedRow {
                        group,
                        key: keys[row].clone(),
                        value: input.as_ref().map(|input| input.get_value(row)),
                    });
                }
                check_buffered_rows(&[LogicalType::HUGEINT, LogicalType::HUGEINT], ordered.len())?;
                continue;
            }
            aggregate
                .function
                .update(input.as_ref(), chunk.len(), &rows, &mut state.states[i])?;
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut state = self.state.lock();
        let state = &mut *state;
        // what ORDER BY held back goes in now, a row at a time, in order (stably, like DuckDB)
        for (i, aggregate) in self.aggregates.iter().enumerate() {
            let mut rows = std::mem::take(&mut state.ordered[i]);
            rows.sort_by(|a, b| a.key.cmp(&b.key));
            for row in rows {
                let input = row.value.as_ref().map(Vector::constant);
                aggregate
                    .function
                    .update(input.as_ref(), 1, &[(0, row.group)], &mut state.states[i])?;
            }
        }
        let finalize = |group: usize, count: usize| -> Result<Vec<Vector>> {
            self.aggregates
                .iter()
                .zip(&state.states)
                .map(|(aggregate, states)| {
                    let mut column = Vector::new(aggregate.return_type.clone(), count);
                    let input = aggregate.children.first().map(|c| &c.return_type);
                    for (row, state) in states[group..group + count].iter().enumerate() {
                        let value = aggregate.function.finalize(state, &aggregate.return_type, input)?;
                        column.set_value(row, &value)?;
                    }
                    Ok(column)
                })
                .collect()
        };
        let mut output = Vec::new();
        if self.groups.is_empty() {
            output.push(DataChunk::from_columns(finalize(0, 1)?, 1));
        } else {
            let mut offset = 0;
            for chunk in state.group_values.chunks() {
                let mut columns = chunk.columns().to_vec();
                columns.extend(finalize(offset, chunk.len())?);
                output.push(DataChunk::from_columns(columns, chunk.len()));
                offset += chunk.len();
            }
        }
        state.output = output;
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let next = state.downcast_mut::<usize>().expect("aggregate scan state");
        let chunk = self.state.lock().output.get(*next).cloned();
        *next += 1;
        Ok(chunk)
    }
}

/// `PhysicalInsert`: appends its input to a table (sink), then returns the row count (source).
#[derive(Debug)]
pub struct PhysicalInsert {
    pub types: Vec<LogicalType>,
    pub table: Arc<TableCatalogEntry>,
    pub column_index_map: Vec<Option<usize>>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub inserted: Mutex<u64>,
}

impl PhysicalOperator for PhysicalInsert {
    fn name(&self) -> &'static str {
        "INSERT"
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
        // ResolveDefaults: each table column from its input column, or its default
        let columns = self
            .table
            .columns
            .iter()
            .zip(&self.column_index_map)
            .map(|(column, source)| match (source, &column.default_value) {
                (Some(i), _) => Ok(chunk.column(*i).clone()),
                (None, Some(default)) => execute(default, chunk),
                (None, None) => Ok(Vector::constant_null(column.ty.clone())),
            })
            .collect::<Result<Vec<_>>>()?;
        let row = DataChunk::from_columns(columns, chunk.len());
        for (i, column) in self.table.columns.iter().enumerate() {
            let unified = row.column(i).to_unified(row.len());
            if column.not_null && (0..row.len()).any(|r| !unified.is_valid(r)) {
                return Err(Error::new(
                    ErrorKind::Constraint,
                    format!(
                        "NOT NULL constraint failed: {}.{}",
                        sql_identifier(&self.table.name),
                        sql_identifier(&column.name)
                    ),
                ));
            }
        }
        let mut storage = self.table.storage.write();
        storage.append(&row);
        check_buffered(&storage)?;
        drop(storage);
        *self.inserted.lock() += chunk.len() as u64;
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("insert source state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let count = *self.inserted.lock() as i64;
        let mut chunk = DataChunk::new(&self.types);
        chunk.append_row(&[Value::bigint(count)])?;
        Ok(Some(chunk))
    }
}

/// `PhysicalCreateTable` with a query (`PhysicalBatchInsert`): gathers the rows, then creates
/// the table with them, so that the query may read a table it replaces.
#[derive(Debug)]
pub struct PhysicalCreateTableAs {
    pub types: Vec<LogicalType>,
    pub schema: Arc<SchemaCatalogEntry>,
    pub table_name: String,
    pub columns: Vec<ColumnDefinition>,
    pub on_conflict: OnCreateConflict,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalOperator for PhysicalCreateTableAs {
    fn name(&self) -> &'static str {
        "CREATE_TABLE_AS"
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
        let mut columns = chunk.columns().to_vec();
        for (vector, column) in columns.iter_mut().zip(&self.columns) {
            if vector.logical_type() != &column.ty {
                // an untyped NULL column
                *vector = Vector::constant_null(column.ty.clone());
            }
        }
        let mut data = self.data.lock();
        data.append(&DataChunk::from_columns(columns, chunk.len()));
        check_buffered(&data)
    }
    fn finalize(&self) -> Result<()> {
        let table = self
            .schema
            .create_table(&self.table_name, self.columns.clone(), self.on_conflict)?;
        if let Some(table) = table {
            let data = self.data.lock();
            let mut storage = table.storage.write();
            for chunk in data.chunks() {
                storage.append(chunk);
            }
        }
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("create table state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let mut chunk = DataChunk::new(&self.types);
        chunk.append_row(&[Value::bigint(self.data.lock().len() as i64)])?;
        Ok(Some(chunk))
    }
}

/// `PhysicalCreateTable`.
#[derive(Debug)]
pub struct PhysicalCreateTable {
    pub types: Vec<LogicalType>,
    pub schema: Arc<SchemaCatalogEntry>,
    pub table_name: String,
    pub columns: Vec<ColumnDefinition>,
    pub on_conflict: OnCreateConflict,
}

impl PhysicalOperator for PhysicalCreateTable {
    fn name(&self) -> &'static str {
        "CREATE_TABLE"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn get_data(&self, _state: &mut SourceState) -> Result<Option<DataChunk>> {
        self.schema
            .create_table(&self.table_name, self.columns.clone(), self.on_conflict)?;
        Ok(None)
    }
}

/// `PhysicalMaterializedCollector`: gathers the query result.
#[derive(Debug)]
pub struct PhysicalResultCollector {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub result: Mutex<ColumnDataCollection>,
}

impl PhysicalResultCollector {
    pub fn new(child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            result: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            children: vec![child],
        }
    }
}

impl PhysicalOperator for PhysicalResultCollector {
    fn name(&self) -> &'static str {
        "RESULT_COLLECTOR"
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
        let mut result = self.result.lock();
        result.append(chunk);
        check_buffered(&result)
    }
}
