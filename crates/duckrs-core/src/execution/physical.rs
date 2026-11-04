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
use crate::planner::expression::BoundExpr;
use crate::planner::logical::BoundOrderByNode;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType, create_sort_keys, sort_collection};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use parking_lot::Mutex;
use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// The state a source keeps between `get_data` calls (`GlobalSourceState`).
pub type SourceState = Box<dyn Any + Send>;

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
        *next += 1;
        let count = chunk.len().min(*remaining);
        *remaining -= count;
        let columns = self.column_ids.iter().map(|&i| chunk.column(i).clone()).collect();
        let mut result = DataChunk::from_columns(columns, chunk.len());
        if count < chunk.len() {
            result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
        }
        Ok(Some(result))
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
        self.data.lock().append(chunk);
        Ok(())
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
        // equal values (NULLs included) have equal sort keys
        let modifiers = OrderModifiers {
            order: OrderType::Ascending,
            nulls: NullOrder::NullsLast,
        };
        let columns: Vec<(&Vector, OrderModifiers)> =
            self.targets.iter().map(|&i| (chunk.column(i), modifiers)).collect();
        let keys = create_sort_keys(&columns, chunk.len())?;
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
            self.data.lock().append(&rows);
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
        // ResolveDefaults: each table column from its input column, or its default (NULL)
        let columns = self
            .table
            .columns
            .iter()
            .zip(&self.column_index_map)
            .map(|(column, source)| match source {
                Some(i) => chunk.column(*i).clone(),
                None => Vector::constant_null(column.ty.clone()),
            })
            .collect();
        let row = DataChunk::from_columns(columns, chunk.len());
        self.table.storage.write().append(&row);
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
        self.result.lock().append(chunk);
        Ok(())
    }
}
