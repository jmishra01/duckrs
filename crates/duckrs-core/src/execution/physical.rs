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

use super::constraints;
use super::expression_executor::{execute, select};
use super::upsert::upsert;
use super::zone_map::{CHUNKS_PER_ROW_GROUP, groups_to_scan};
use crate::catalog::{
    ColumnDefinition, GeneratedColumn, OnCreateConflict, RowIds, SchemaCatalogEntry, TableCatalogEntry,
    TableConstraint, TableCursor, TableSource,
};
use crate::execution::external_sort::SortSpill;
use crate::execution::run_plan;
use crate::execution::temporary::{self, PartitionedSpill, SpillFile};
use crate::function::aggregates::{AggregateState, combine_states, value_keys};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, CteKeyColumn, LogicalOperator, OnConflict};
use crate::storage::log_writer::can_serialize;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{
    NullOrder, OrderModifiers, OrderType, RowKey, RowKeyMap, RowKeySet, create_row_keys, create_sort_keys,
    sort_collection,
};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{JoinType, SampleMethod, SampleOptions, sql_identifier};
use parking_lot::Mutex;
use std::any::Any;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The state a source keeps between `get_data` calls (`GlobalSourceState`).
pub type SourceState = Box<dyn Any + Send>;

/// What the threads of a parallel source share (`GlobalSourceState`).
pub type SharedSourceState = Box<dyn Any + Send + Sync>;

/// What a thread of a parallel sink gathers on its own (`LocalSinkState`).
pub type LocalSinkState = Box<dyn Any + Send>;

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
    /// Lines of `EXPLAIN` under the name of the operator (`ParamsToString`).
    fn explain_details(&self) -> Vec<String> {
        Vec::new()
    }
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

    /// A source that threads can read at once: what they share, and how many morsels it has.
    /// Morsels can be claimed in any order, so only an order-independent sink may read it.
    fn parallel_source(&self) -> Option<(SharedSourceState, usize)> {
        None
    }
    /// Morsel `index` of [`Self::parallel_source`]; `None` if it has no rows left.
    fn parallel_chunk(&self, _state: &SharedSourceState, _index: usize) -> Result<Option<DataChunk>> {
        unreachable!("{} is not a parallel source", self.name())
    }

    // Operator interface: one input chunk becomes one output chunk
    fn execute(&self, _input: DataChunk) -> Result<DataChunk> {
        unreachable!("{} is not an operator", self.name())
    }

    fn operator_state(&self) -> OperatorState {
        Box::new(())
    }

    /// Whether threads can run it at once on chunks in any order (`ParallelOperator`).
    fn parallel_operator(&self) -> bool {
        false
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
    /// Whether threads can sink at once, each into a state of its own, in any order
    /// (`ParallelSink`). The states come together in `combine_local`. `sink_local` is told
    /// which morsel of the source a chunk comes from (its batch index), for a sink that keeps
    /// the order of its input.
    fn parallel_sink(&self) -> bool {
        false
    }
    fn local_sink_state(&self) -> LocalSinkState {
        unreachable!("{} is not a parallel sink", self.name())
    }
    fn sink_local(&self, _local: &mut LocalSinkState, _batch: usize, _chunk: &DataChunk) -> Result<()> {
        unreachable!("{} is not a parallel sink", self.name())
    }
    fn combine_local(&self, _local: LocalSinkState) -> Result<()> {
        unreachable!("{} is not a parallel sink", self.name())
    }
    /// Which child feeds the sink. Joins are sinks for their right child, and the left child
    /// streams through them.
    fn sink_child(&self) -> usize {
        0
    }
    /// All the children that feed the sink.
    fn sink_children(&self) -> Vec<usize> {
        vec![self.sink_child()]
    }
    /// `sink`, for an operator that tells its inputs apart.
    fn sink_input(&self, _input: usize, chunk: &DataChunk) -> Result<()> {
        self.sink(chunk)
    }

    /// All of input `input` went through `sink_input`.
    fn finish_input(&self, _input: usize) -> Result<()> {
        Ok(())
    }

    /// How many of its row groups a table scan reads, of how many there are.
    fn row_groups_scanned(&self) -> Option<(usize, usize)> {
        None
    }

    /// Whether a join has rows to emit through `get_data` after its input is done
    /// (`IsSource` of RIGHT and FULL joins).
    fn has_unmatched_source(&self) -> bool {
        false
    }
}

/// Where a table scan is.
struct TableScanState {
    ids: Arc<RowIds>,
    next: usize,
    remaining: usize,
    /// per row group, whether its rows can pass the filter
    groups: Vec<bool>,
    /// Reads the table from its file, if its rows are still there.
    cursor: Option<FileScan>,
    /// How far an index scan got in `index_rows`.
    next_index_row: usize,
}

/// What a scan reads of a file. `column_ids` lists all columns for the filter and the projection to
/// refer to by position, and what neither of them refers to isn't read.
struct FileScan {
    cursor: TableCursor,
    /// Per entry of `column_ids`, where the chunks of `cursor` have it, or a vector that nothing reads.
    columns: Vec<Result<usize, Vector>>,
}

/// `PhysicalTableScan` over a table's in-memory rows.
#[derive(Debug)]
pub struct PhysicalTableScan {
    pub table: Arc<TableCatalogEntry>,
    pub column_ids: Vec<usize>,
    /// the positions in `column_ids` that it outputs, without one all of them
    pub projection: Option<Vec<usize>>,
    pub types: Vec<LogicalType>,
    /// what rows must meet, on `column_ids`
    pub filter: Option<BoundExpr>,
    /// the rows to read, in order, instead of all of them
    pub index_rows: Option<Arc<Vec<usize>>>,
}

impl PhysicalTableScan {
    /// What the columns of a scan are for rows of a chunk that starts at `first_row`.
    fn scan_columns(
        &self,
        chunk: &DataChunk,
        ids: &RowIds,
        first_row: usize,
        cursor: Option<&FileScan>,
    ) -> Result<Vec<Vector>> {
        self.column_ids
            .iter()
            .enumerate()
            .map(|(position, &i)| {
                if i == COLUMN_IDENTIFIER_ROW_ID {
                    return ids.vector(first_row, chunk.len());
                }
                Ok(match cursor.map(|scan| &scan.columns[position]) {
                    None => chunk.column(i).clone(),
                    Some(Ok(at)) => chunk.column(*at).clone(),
                    Some(Err(unread)) => unread.clone(),
                })
            })
            .collect()
    }

    /// The filter, then the columns it may be the only one to read, of a chunk that was scanned.
    fn finish_chunk(&self, mut result: DataChunk) -> Result<Option<DataChunk>> {
        // the filter reads what it scans, so the columns go after that and before the slice
        let mut selected = None;
        if let Some(filter) = &self.filter {
            let (sel, matched) = select(filter, &result)?;
            if matched == 0 {
                return Ok(None);
            }
            if matched != result.len() {
                selected = Some((sel, matched));
            }
        }
        if let Some(positions) = &self.projection {
            let columns = positions.iter().map(|&p| result.column(p).clone()).collect();
            result = DataChunk::from_columns(columns, result.len());
        }
        if let Some((sel, matched)) = selected {
            result.slice(&sel, matched);
        }
        Ok(Some(result))
    }

    /// The next rows of an index scan that have any left after the filter: those of one chunk.
    fn next_index_chunk(&self, rows: &[usize], next: &mut usize) -> Result<Option<DataChunk>> {
        let storage = self.table.rows()?;
        let ids = self.table.row_ids(storage.len());
        while *next < rows.len() {
            let index = rows[*next] / STANDARD_VECTOR_SIZE;
            let first_row = index * STANDARD_VECTOR_SIZE;
            let mut selected = Vec::new();
            while let Some(row) = rows.get(*next).filter(|row| **row / STANDARD_VECTOR_SIZE == index) {
                selected.push((*row - first_row) as u32);
                *next += 1;
            }
            let chunk = storage.chunk(index);
            let columns = self.scan_columns(chunk, &ids, first_row, None)?;
            let mut result = DataChunk::from_columns(columns, chunk.len());
            let count = selected.len();
            result.slice(&SelectionVector::from_vec(selected), count);
            if let Some(result) = self.finish_chunk(result)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    fn file_scan(&self, source: Arc<dyn TableSource>) -> FileScan {
        let mut used = vec![self.projection.is_none(); self.column_ids.len()];
        for &position in self.projection.iter().flatten() {
            used[position] = true;
        }
        if let Some(filter) = &self.filter {
            filter.clone().walk_mut(&mut |e| {
                if let BoundExprKind::Reference(position) = e.kind {
                    used[position] = true;
                }
            });
        }
        let mut read = Vec::new();
        let mut columns = Vec::with_capacity(self.column_ids.len());
        for (position, &column) in self.column_ids.iter().enumerate() {
            if column == COLUMN_IDENTIFIER_ROW_ID {
                // stands for the sequence that `get_data` makes
                columns.push(Ok(usize::MAX));
            } else if used[position] {
                read.push(column);
                columns.push(Ok(read.len() - 1));
            } else {
                columns.push(Err(Vector::new(
                    self.table.columns[column].ty.clone(),
                    STANDARD_VECTOR_SIZE,
                )));
            }
        }
        let types = read.iter().map(|&c| self.table.columns[c].ty.clone()).collect();
        FileScan {
            cursor: TableCursor::new(source, read, types),
            columns,
        }
    }
}

/// What the threads of a [`PhysicalTableScan`] share.
struct ParallelTableScan {
    ids: Arc<RowIds>,
    rows: usize,
    groups: Vec<bool>,
}

/// Fewer row groups than this aren't worth threads, and stay as they are: in order, and with
/// floating point sums that come out the same each time.
const MIN_PARALLEL_ROW_GROUPS: usize = 2;

impl PhysicalOperator for PhysicalTableScan {
    fn name(&self) -> &'static str {
        "TABLE_SCAN"
    }
    fn parallel_source(&self) -> Option<(SharedSourceState, usize)> {
        if self.index_rows.is_some() || self.table.source().is_some() {
            return None;
        }
        if !self.filter.as_ref().is_none_or(parallel_expression) {
            return None;
        }
        let rows = self.table.row_count();
        let chunks = rows.div_ceil(STANDARD_VECTOR_SIZE);
        if chunks < MIN_PARALLEL_ROW_GROUPS * CHUNKS_PER_ROW_GROUP {
            return None;
        }
        let groups = groups_to_scan(&self.table, &self.column_ids, self.filter.as_ref(), rows);
        let ids = self.table.row_ids(rows);
        Some((Box::new(ParallelTableScan { ids, rows, groups }), chunks))
    }
    fn parallel_chunk(&self, state: &SharedSourceState, index: usize) -> Result<Option<DataChunk>> {
        let state: &ParallelTableScan = state.downcast_ref().expect("parallel scan state");
        if !state.groups.get(index / CHUNKS_PER_ROW_GROUP).is_none_or(|g| *g) {
            return Ok(None);
        }
        let first_row = index * STANDARD_VECTOR_SIZE;
        let storage = self.table.rows()?;
        let chunk = storage.chunk(index);
        let count = chunk.len().min(state.rows.saturating_sub(first_row));
        let columns = self.scan_columns(chunk, &state.ids, first_row, None)?;
        let mut result = DataChunk::from_columns(columns, chunk.len());
        if count < chunk.len() {
            result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
        }
        self.finish_chunk(result)
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
        let remaining = self.table.row_count();
        let groups = groups_to_scan(&self.table, &self.column_ids, self.filter.as_ref(), remaining);
        let cursor = self.table.source().map(|source| self.file_scan(source));
        let ids = self.table.row_ids(remaining);
        Box::new(TableScanState {
            ids,
            next: 0,
            remaining,
            groups,
            cursor,
            next_index_row: 0,
        })
    }
    fn row_groups_scanned(&self) -> Option<(usize, usize)> {
        if self.index_rows.is_some() {
            return None;
        }
        let rows = self.table.row_count();
        let groups = groups_to_scan(&self.table, &self.column_ids, self.filter.as_ref(), rows);
        (!groups.is_empty()).then(|| (groups.iter().filter(|g| **g).count(), groups.len()))
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let TableScanState {
            ids,
            next,
            remaining,
            groups,
            cursor,
            next_index_row,
        } = state.downcast_mut().expect("table scan state");
        if let Some(rows) = &self.index_rows {
            return self.next_index_chunk(rows, next_index_row);
        }
        while *remaining > 0 {
            // all chunks but the last are full, so that a chunk tells where its rows are
            let index = *next;
            let first_row = index * STANDARD_VECTOR_SIZE;
            let wanted = groups.get(index / CHUNKS_PER_ROW_GROUP).is_none_or(|g| *g);
            *next += 1;
            if !wanted {
                *remaining -= STANDARD_VECTOR_SIZE.min(*remaining);
                continue;
            }
            let storage;
            let chunk = match cursor {
                Some(scan) => Cow::Owned(scan.cursor.chunk(index)?),
                None => {
                    storage = self.table.rows()?;
                    Cow::Borrowed(storage.chunk(index))
                }
            };
            let count = chunk.len().min(*remaining);
            *remaining -= count;
            let columns = self.scan_columns(&chunk, ids, first_row, cursor.as_ref())?;
            let mut result = DataChunk::from_columns(columns, chunk.len());
            if count < chunk.len() {
                result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
            }
            match self.finish_chunk(result)? {
                Some(result) => return Ok(Some(result)),
                None => continue,
            }
        }
        Ok(None)
    }
}

/// Whether threads can evaluate `expr` at once. What is volatile gives other results in another
/// order, and what runs a plan of its own is not known to be safe from several threads.
pub(super) fn parallel_expression(expr: &BoundExpr) -> bool {
    use crate::planner::expression::BoundExprKind;
    fn has_subquery(expr: &BoundExpr) -> bool {
        matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
    }
    !expr.is_volatile() && !has_subquery(expr)
}

/// How much a blocking operator or table may hold, roughly, unless it spills: then `memory_limit`
/// bounds it (see "Spilling" in `docs/ARCHITECTURE.md`). More than this fails instead of
/// exhausting the machine.
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
pub(crate) fn group_keys(columns: &[&Vector], count: usize) -> Result<Vec<Vec<u8>>> {
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

/// The `read_csv` table function.
#[derive(Debug)]
pub struct PhysicalCsvScan {
    pub types: Vec<LogicalType>,
    pub scan: Arc<super::csv::CsvScanData>,
}

impl PhysicalOperator for PhysicalCsvScan {
    fn name(&self) -> &'static str {
        "READ_CSV"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(super::csv::CsvScanState::default())
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let state = state.downcast_mut().expect("csv scan state");
        super::csv::scan_chunk(&self.scan, state)
    }
}

/// The file of a COPY TO, compressed or not.
#[derive(Debug)]
enum CopyWriter {
    Plain(std::io::BufWriter<std::fs::File>),
    Gzip(Box<flate2::write::GzEncoder<std::io::BufWriter<std::fs::File>>>),
}

impl std::io::Write for CopyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(writer) => writer.write(bytes),
            Self::Gzip(writer) => writer.write(bytes),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(writer) => writer.flush(),
            Self::Gzip(writer) => writer.flush(),
        }
    }
}

/// `PhysicalCopyToFile`: writes the rows of its child, and reports how many there were.
#[derive(Debug)]
pub struct PhysicalCopyToFile {
    types: Vec<LogicalType>,
    options: super::csv::CsvWriteOptions,
    children: Vec<Box<dyn PhysicalOperator>>,
    path: String,
    names: Vec<String>,
    /// opened by the first row or at the end, so that planning alone (EXPLAIN) leaves the file be
    writer: Mutex<Option<CopyWriter>>,
    gzip: bool,
    rows: std::sync::atomic::AtomicU64,
}

impl PhysicalCopyToFile {
    pub fn new(
        types: Vec<LogicalType>,
        path: String,
        names: Vec<String>,
        options: super::csv::CsvWriteOptions,
        children: Vec<Box<dyn PhysicalOperator>>,
        gzip: bool,
    ) -> Self {
        Self {
            types,
            options,
            children,
            path,
            names,
            writer: Mutex::new(None),
            gzip,
            rows: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn with_writer(&self, f: impl FnOnce(&mut CopyWriter) -> Result<()>) -> Result<()> {
        let mut writer = self.writer.lock();
        if writer.is_none() {
            let file = std::fs::File::create(&self.path)
                .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{}\": {e}", self.path)))?;
            let file = std::io::BufWriter::new(file);
            let mut opened = if self.gzip {
                CopyWriter::Gzip(Box::new(flate2::write::GzEncoder::new(
                    file,
                    flate2::Compression::default(),
                )))
            } else {
                CopyWriter::Plain(file)
            };
            let mut header = Vec::new();
            self.options.write_header(&mut header, &self.names);
            write_all(&mut opened, &header)?;
            *writer = Some(opened);
        }
        f(writer.as_mut().expect("just opened"))
    }
}

fn write_all(writer: &mut impl std::io::Write, bytes: &[u8]) -> Result<()> {
    writer
        .write_all(bytes)
        .map_err(|e| Error::new(ErrorKind::Io, format!("Could not write file: {e}")))
}

impl PhysicalOperator for PhysicalCopyToFile {
    fn name(&self) -> &'static str {
        "COPY_TO_FILE"
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
        let mut out = Vec::new();
        self.options.write_chunk(&mut out, chunk)?;
        self.with_writer(|writer| write_all(writer, &out))?;
        self.rows
            .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        self.with_writer(|writer| write_all(writer, self.options.suffix.as_bytes()))?;
        let finished = match self.writer.lock().take().expect("just opened") {
            CopyWriter::Plain(mut writer) => std::io::Write::flush(&mut writer),
            CopyWriter::Gzip(writer) => writer.finish().and_then(|mut file| std::io::Write::flush(&mut file)),
        };
        finished.map_err(|e| Error::new(ErrorKind::Io, format!("Could not write file: {e}")))
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("copy state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let rows = self.rows.load(std::sync::atomic::Ordering::Relaxed);
        let mut column = Vector::new(LogicalType::BIGINT, 1);
        column.data_mut::<i64>()[0] = rows as i64;
        Ok(Some(DataChunk::from_columns(vec![column], 1)))
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
    fn explain_details(&self) -> Vec<String> {
        vec![self.condition.to_string()]
    }
    fn name(&self) -> &'static str {
        "FILTER"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn parallel_operator(&self) -> bool {
        parallel_expression(&self.condition)
    }
    fn execute(&self, mut input: DataChunk) -> Result<DataChunk> {
        let (sel, count) = select(&self.condition, &input)?;
        if count != input.len() {
            input.slice(&sel, count);
        }
        Ok(input)
    }
}

/// `PhysicalUnnest`: the input's columns, repeated, and then the elements of the lists.
#[derive(Debug)]
pub struct PhysicalUnnest {
    pub types: Vec<LogicalType>,
    pub expressions: Vec<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
}

impl PhysicalOperator for PhysicalUnnest {
    fn name(&self) -> &'static str {
        "UNNEST"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn execute(&self, input: DataChunk) -> Result<DataChunk> {
        let lists = self
            .expressions
            .iter()
            .map(|e| execute(e, &input))
            .collect::<Result<Vec<_>>>()?;
        let mut rows: Vec<u32> = Vec::new();
        let mut elements: Vec<Vec<Value>> = vec![Vec::new(); lists.len()];
        let items = |list: &Vector, row: usize| match list.get_value(row).data() {
            ValueData::List(items) => items.clone(),
            _ => Vec::new(),
        };
        for row in 0..input.len() {
            let values: Vec<Vec<Value>> = lists.iter().map(|l| items(l, row)).collect();
            let length = values.iter().map(Vec::len).max().unwrap_or(0);
            rows.extend(std::iter::repeat_n(row as u32, length));
            for (column, mut value) in elements.iter_mut().zip(values) {
                let missing = length - value.len();
                value.extend((0..missing).map(|_| Value::sql_null()));
                column.extend(value);
            }
        }
        let mut output = input;
        let count = rows.len();
        output.slice(&SelectionVector::from_vec(rows), count);
        let mut columns: Vec<Vector> = output.columns().to_vec();
        let offset = columns.len();
        for (i, column) in elements.into_iter().enumerate() {
            let mut vector = Vector::new(self.types[offset + i].clone(), count.max(1));
            for (row, value) in column.iter().enumerate() {
                vector.set_value(row, value)?;
            }
            columns.push(vector);
        }
        Ok(DataChunk::from_columns(columns, count))
    }
}

/// `PhysicalRecursiveCTE`: gathers the child's rows, then runs the recursive part on the rows of
/// the last round until it adds nothing.
#[derive(Debug)]
pub struct PhysicalRecursiveCte {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    union_all: bool,
    working: Arc<Mutex<ColumnDataCollection>>,
    recurring: Arc<Mutex<ColumnDataCollection>>,
    key: Vec<CteKeyColumn>,
    recursive: LogicalOperator,
    /// the rows of the current round
    data: Mutex<ColumnDataCollection>,
    /// what UNION has seen so far
    seen: Mutex<HashSet<Vec<u8>>>,
}

/// The rows of a USING KEY recursion, one per key.
#[derive(Default)]
struct KeyedRows {
    positions: HashMap<Vec<u8>, usize>,
    rows: Vec<Vec<Value>>,
}

fn value_sort_key(value: &Value) -> Result<Vec<u8>> {
    let mut vector = Vector::new(value.logical_type().clone(), 1);
    vector.prepare_flat(1);
    vector.set_value(0, value)?;
    let mut keys = group_keys(&[&vector], 1)?;
    Ok(keys.remove(0))
}

impl PhysicalRecursiveCte {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        union_all: bool,
        working: Arc<Mutex<ColumnDataCollection>>,
        recurring: Arc<Mutex<ColumnDataCollection>>,
        key: Vec<CteKeyColumn>,
        recursive: LogicalOperator,
    ) -> Self {
        let data = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            union_all,
            working,
            recurring,
            key,
            recursive,
            data,
            seen: Mutex::default(),
        }
    }

    fn collection_of(&self, rows: impl Iterator<Item = Vec<Value>>) -> Result<ColumnDataCollection> {
        let mut result = ColumnDataCollection::new(self.types.clone());
        let mut chunk = DataChunk::new(&self.types);
        for row in rows {
            chunk.append_row(&row)?;
            if chunk.len() == STANDARD_VECTOR_SIZE {
                result.append(&chunk);
                chunk = DataChunk::new(&self.types);
            }
        }
        if !chunk.is_empty() {
            result.append(&chunk);
        }
        Ok(result)
    }

    /// Takes `rows` into `state`, and returns what the next round reads.
    fn apply_keyed(&self, rows: &ColumnDataCollection, state: &mut KeyedRows) -> Result<ColumnDataCollection> {
        let key_columns: Vec<usize> = (0..self.key.len())
            .filter(|c| self.key[*c] == CteKeyColumn::Key)
            .collect();
        let mut changed: Vec<usize> = Vec::new();
        let mut changed_set: HashSet<usize> = HashSet::new();
        for chunk in rows.chunks() {
            let vectors: Vec<&Vector> = key_columns.iter().map(|c| chunk.column(*c)).collect();
            let keys = group_keys(&vectors, chunk.len())?;
            for (row, key) in keys.into_iter().enumerate() {
                let values: Vec<Value> = chunk.columns().iter().map(|c| c.get_value(row)).collect();
                let position = match state.positions.get(&key) {
                    Some(position) => {
                        let existing = &mut state.rows[*position];
                        let mut updated = false;
                        for (column, mode) in self.key.iter().enumerate() {
                            let new = &values[column];
                            let old = &existing[column];
                            let take = match mode {
                                CteKeyColumn::Key | CteKeyColumn::First => false,
                                CteKeyColumn::Last => value_sort_key(new)? != value_sort_key(old)?,
                                CteKeyColumn::Min => {
                                    !new.is_null() && (old.is_null() || value_sort_key(new)? < value_sort_key(old)?)
                                }
                                CteKeyColumn::Max => {
                                    !new.is_null() && (old.is_null() || value_sort_key(new)? > value_sort_key(old)?)
                                }
                            };
                            if take {
                                existing[column] = new.clone();
                                updated = true;
                            }
                        }
                        if !updated {
                            continue;
                        }
                        *position
                    }
                    None => {
                        state.positions.insert(key, state.rows.len());
                        state.rows.push(values);
                        state.rows.len() - 1
                    }
                };
                if changed_set.insert(position) {
                    changed.push(position);
                }
            }
        }
        *self.recurring.lock() = self.collection_of(state.rows.iter().cloned())?;
        if self.union_all {
            let mut raw = ColumnDataCollection::new(self.types.clone());
            if !changed.is_empty() {
                for chunk in rows.chunks() {
                    raw.append(chunk);
                }
            }
            return Ok(raw);
        }
        self.collection_of(changed.iter().map(|p| state.rows[*p].clone()))
    }

    /// USING KEY: runs all rounds, and leaves the rows of all keys in `data`.
    fn run_keyed(&self) -> Result<()> {
        let mut state = KeyedRows::default();
        let anchor = std::mem::replace(&mut *self.data.lock(), ColumnDataCollection::new(self.types.clone()));
        let mut round = self.apply_keyed(&anchor, &mut state)?;
        while !round.is_empty() {
            *self.working.lock() = round;
            let mut plan = self.recursive.clone();
            plan.clear_subquery_results();
            let rows = run_plan(plan)?;
            round = self.apply_keyed(&rows, &mut state)?;
        }
        *self.data.lock() = self.collection_of(state.rows.into_iter())?;
        Ok(())
    }

    /// The rows of `rows` that UNION keeps: the ones not `seen` before.
    fn new_rows(&self, rows: ColumnDataCollection, seen: &mut HashSet<Vec<u8>>) -> Result<ColumnDataCollection> {
        if self.union_all {
            return Ok(rows);
        }
        let mut fresh = ColumnDataCollection::new(self.types.clone());
        for chunk in rows.chunks() {
            let columns: Vec<&Vector> = chunk.columns().iter().collect();
            let keys = group_keys(&columns, chunk.len())?;
            let keep: Vec<u32> = keys
                .into_iter()
                .enumerate()
                .filter(|(_, key)| seen.insert(key.clone()))
                .map(|(row, _)| row as u32)
                .collect();
            let count = keep.len();
            if count > 0 {
                let mut chunk = chunk.clone();
                chunk.slice(&SelectionVector::from_vec(keep), count);
                fresh.append(&chunk);
            }
        }
        Ok(fresh)
    }
}

impl PhysicalOperator for PhysicalRecursiveCte {
    fn name(&self) -> &'static str {
        "RECURSIVE_CTE"
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
        if !self.key.is_empty() {
            return self.run_keyed();
        }
        let mut data = self.data.lock();
        let anchor = std::mem::replace(&mut *data, ColumnDataCollection::new(self.types.clone()));
        *data = self.new_rows(anchor, &mut self.seen.lock())?;
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    /// A round runs when the rows of the last one have gone, so a LIMIT above can stop recursion
    /// that would never end.
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        loop {
            if let Some(chunk) = scan_collection(&self.data, state) {
                return Ok(Some(chunk));
            }
            if !self.key.is_empty() {
                return Ok(None);
            }
            let round = std::mem::replace(&mut *self.data.lock(), ColumnDataCollection::new(self.types.clone()));
            if round.is_empty() {
                return Ok(None);
            }
            {
                let mut recurring = self.recurring.lock();
                for chunk in round.chunks() {
                    recurring.append(chunk);
                }
            }
            *self.working.lock() = round;
            let mut plan = self.recursive.clone();
            plan.clear_subquery_results();
            let rows = run_plan(plan)?;
            *self.data.lock() = self.new_rows(rows, &mut self.seen.lock())?;
            *state.downcast_mut::<usize>().expect("collection scan state") = 0;
        }
    }
}

/// The rows of the last round of a recursive CTE.
#[derive(Debug)]
pub struct PhysicalWorkingTable {
    pub types: Vec<LogicalType>,
    pub working: Arc<Mutex<ColumnDataCollection>>,
}

impl PhysicalOperator for PhysicalWorkingTable {
    fn name(&self) -> &'static str {
        "CTE_SCAN"
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
        Ok(scan_collection(&self.working, state))
    }
}

/// A join whose right side runs once per combination of correlated values of the left side.
#[derive(Debug)]
pub struct PhysicalLateralJoin {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub join_type: JoinType,
    pub right: LogicalOperator,
    pub bindings: Vec<ColumnBinding>,
    pub correlated: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub results: Mutex<HashMap<Vec<u8>, Arc<ColumnDataCollection>>>,
}

impl PhysicalLateralJoin {
    /// The rows of the right side for a row of the left one.
    fn right_rows(&self, key: &[u8], values: Vec<Value>, volatile: bool) -> Result<Arc<ColumnDataCollection>> {
        if !volatile && let Some(rows) = self.results.lock().get(key) {
            return Ok(rows.clone());
        }
        let mut plan = self.right.clone();
        let columns: Vec<_> = self.bindings.iter().copied().zip(values).collect();
        plan.substitute_columns(&columns);
        let rows = Arc::new(run_plan(plan)?);
        if !volatile {
            self.results.lock().insert(key.to_vec(), rows.clone());
        }
        Ok(rows)
    }

    fn join_chunk(&self, input: &DataChunk) -> Result<VecDeque<DataChunk>> {
        let count = input.len();
        let left_count = input.column_count();
        let columns = self
            .correlated
            .iter()
            .map(|c| execute(c, input))
            .collect::<Result<Vec<_>>>()?;
        let keys = group_keys(&columns.iter().collect::<Vec<_>>(), count)?;
        let volatile = self.right.is_volatile();
        let mut output = VecDeque::new();
        for (row, key) in keys.iter().enumerate() {
            let values = columns.iter().map(|c| c.get_value(row)).collect();
            let rows = self.right_rows(key, values, volatile)?;
            let mut matched = false;
            for chunk in rows.chunks() {
                let mut left = input.clone();
                left.slice(&SelectionVector::from_vec(vec![row as u32; chunk.len()]), chunk.len());
                let mut joined = left.columns().to_vec();
                joined.extend(chunk.columns().iter().cloned());
                let mut joined = DataChunk::from_columns(joined, chunk.len());
                if let Some(predicate) = &self.predicate {
                    let (selection, selected) = select(predicate, &joined)?;
                    if selected != joined.len() {
                        joined.slice(&selection, selected);
                    }
                }
                if !joined.is_empty() {
                    matched = true;
                    output.push_back(joined);
                }
            }
            if !matched && self.join_type == JoinType::Left {
                let mut left = input.clone();
                left.slice(&SelectionVector::from_vec(vec![row as u32]), 1);
                let mut joined = left.columns().to_vec();
                joined.extend(
                    self.types[left_count..]
                        .iter()
                        .map(|t| Vector::constant_null(t.clone())),
                );
                output.push_back(DataChunk::from_columns(joined, 1));
            }
        }
        Ok(output)
    }
}

impl PhysicalOperator for PhysicalLateralJoin {
    fn name(&self) -> &'static str {
        "LATERAL_JOIN"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(VecDeque::<DataChunk>::new())
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let pending = state.downcast_mut::<VecDeque<DataChunk>>().expect("lateral join state");
        if pending.is_empty() {
            *pending = self.join_chunk(input)?;
        }
        let Some(chunk) = pending.pop_front() else {
            return Ok((DataChunk::from_columns(Vec::new(), 0), OperatorResult::NeedMoreInput));
        };
        let result = if pending.is_empty() {
            OperatorResult::NeedMoreInput
        } else {
            OperatorResult::HaveMoreOutput
        };
        Ok((chunk, result))
    }
}

/// `PhysicalUnion`: collects the rows of its children, and emits them in that order.
#[derive(Debug)]
pub struct PhysicalUnion {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    data: Mutex<ColumnDataCollection>,
}

impl PhysicalUnion {
    pub fn new(types: Vec<LogicalType>, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        let data = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self { types, children, data }
    }
}

impl PhysicalOperator for PhysicalUnion {
    fn name(&self) -> &'static str {
        "UNION"
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
    fn sink_children(&self) -> Vec<usize> {
        (0..self.children.len()).collect()
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let mut data = self.data.lock();
        data.append(chunk);
        check_buffered(&data)
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

/// EXCEPT and INTERSECT: C++ plans them as joins, with a window for the `ALL` forms.
#[derive(Debug)]
pub struct PhysicalExceptIntersect {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    except: bool,
    all: bool,
    /// the rows of the first child, and of the result once it is final
    data: Mutex<ColumnDataCollection>,
    /// how often each row of the second child occurs
    counts: Mutex<std::collections::HashMap<Vec<u8>, usize>>,
}

impl PhysicalExceptIntersect {
    pub fn new(types: Vec<LogicalType>, children: Vec<Box<dyn PhysicalOperator>>, except: bool, all: bool) -> Self {
        let data = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            except,
            all,
            data,
            counts: Mutex::default(),
        }
    }
}

impl PhysicalOperator for PhysicalExceptIntersect {
    fn name(&self) -> &'static str {
        if self.except { "EXCEPT" } else { "INTERSECT" }
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
    fn sink_children(&self) -> Vec<usize> {
        vec![0, 1]
    }
    fn sink(&self, _chunk: &DataChunk) -> Result<()> {
        unreachable!("the input is told apart")
    }
    fn sink_input(&self, input: usize, chunk: &DataChunk) -> Result<()> {
        if input == 0 {
            let mut data = self.data.lock();
            data.append(chunk);
            return check_buffered(&data);
        }
        let columns: Vec<&Vector> = chunk.columns().iter().collect();
        let mut counts = self.counts.lock();
        for key in group_keys(&columns, chunk.len())? {
            *counts.entry(key).or_default() += 1;
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut data = self.data.lock();
        let mut counts = self.counts.lock();
        let mut result = ColumnDataCollection::new(self.types.clone());
        for index in 0..data.chunk_count() {
            let mut chunk = data.chunk(index).clone();
            let columns: Vec<&Vector> = chunk.columns().iter().collect();
            let keys = group_keys(&columns, chunk.len())?;
            let mut keep = Vec::new();
            for (row, key) in keys.iter().enumerate() {
                let found = match counts.get_mut(key) {
                    Some(count) if *count > 0 => {
                        if self.all {
                            *count -= 1;
                        }
                        true
                    }
                    _ => false,
                };
                if found != self.except {
                    keep.push(row as u32);
                }
            }
            let count = keep.len();
            chunk.slice(&SelectionVector::from_vec(keep), count);
            if count > 0 {
                result.append(&chunk);
            }
        }
        *data = result;
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
    fn parallel_operator(&self) -> bool {
        self.expressions.iter().all(parallel_expression)
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

/// `LogicalOperatorKind::ColumnSelect`.
#[derive(Debug)]
pub struct PhysicalColumnSelect {
    pub types: Vec<LogicalType>,
    pub positions: Vec<usize>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
}

impl PhysicalOperator for PhysicalColumnSelect {
    fn name(&self) -> &'static str {
        "COLUMN_SELECT"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn parallel_operator(&self) -> bool {
        true
    }
    fn execute(&self, input: DataChunk) -> Result<DataChunk> {
        let columns = self.positions.iter().map(|&p| input.column(p).clone()).collect();
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
pub(super) fn scan_collection(collection: &Mutex<ColumnDataCollection>, state: &mut SourceState) -> Option<DataChunk> {
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
    /// `PhysicalTopN`: the limit and the offset of the rows it gives
    pub top: Option<(u64, u64)>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
    /// the runs of a sort past the memory limit; a Top-N keeps few rows and does not spill
    spill: Option<Mutex<SortSpill>>,
}

/// A Top-N sorts what it has when it has twice what it needs, and at least this.
const TOP_N_MIN_BUFFERED_ROWS: u64 = 1 << 16;

/// The rows of a collection after the first `offset`, at most `count` of them.
fn rows_of_collection(collection: &ColumnDataCollection, offset: u64, count: u64) -> ColumnDataCollection {
    let mut result = ColumnDataCollection::new(collection.types().to_vec());
    let (mut skip, mut left) = (offset as usize, usize::try_from(count).unwrap_or(usize::MAX));
    for index in 0..collection.chunk_count() {
        let chunk = collection.chunk(index);
        if skip >= chunk.len() {
            skip -= chunk.len();
            continue;
        }
        let take = left.min(chunk.len() - skip);
        if take == 0 {
            break;
        }
        let mut rows = chunk.clone();
        let selected: Vec<u32> = (skip..skip + take).map(|row| row as u32).collect();
        rows.slice(&SelectionVector::from_vec(selected), take);
        result.append(&rows);
        skip = 0;
        left -= take;
    }
    result
}

impl PhysicalOrder {
    pub fn new(orders: Vec<BoundOrderByNode>, top: Option<(u64, u64)>, child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            orders: orders.iter().map(|o| (reference_index(&o.expr), o.modifiers)).collect(),
            top,
            data: Mutex::new(ColumnDataCollection::new(types.clone())),
            spill: (top.is_none() && SortSpill::can_spill(&types)).then(Mutex::default),
            types,
            children: vec![child],
        }
    }
}

impl PhysicalOperator for PhysicalOrder {
    fn name(&self) -> &'static str {
        if self.top.is_some() { "TOP_N" } else { "ORDER_BY" }
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
        if let Some(spill) = &self.spill {
            return spill.lock().sink(&mut data, chunk, &self.orders);
        }
        // what is behind the first rows now never gets back in, as the sort keeps ties in order
        if let Some((limit, offset)) = self.top {
            let keep = limit.saturating_add(offset);
            if data.len() as u64 > keep.saturating_mul(2).max(TOP_N_MIN_BUFFERED_ROWS) {
                *data = rows_of_collection(&sort_collection(&data, &self.orders)?, 0, keep);
            }
        }
        check_buffered(&data)
    }
    fn finalize(&self) -> Result<()> {
        let mut data = self.data.lock();
        if let Some(spill) = &self.spill {
            let mut spill = spill.lock();
            if spill.spilled() {
                let rows = std::mem::replace(&mut *data, ColumnDataCollection::new(self.types.clone()));
                return spill.finish(rows, &self.orders);
            }
        }
        *data = sort_collection(&data, &self.orders)?;
        if let Some((limit, offset)) = self.top {
            *data = rows_of_collection(&data, offset, limit);
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
        if let Some(spill) = &self.spill {
            let mut spill = spill.lock();
            if spill.spilled() {
                return spill.next_chunk();
            }
        }
        Ok(scan_collection(&self.data, state))
    }
}

/// `PhysicalLimitPercent`: the first part of its input, once it knows how long that is.
#[derive(Debug)]
pub struct PhysicalLimitPercent {
    pub types: Vec<LogicalType>,
    pub percent: f64,
    pub offset: u64,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalOperator for PhysicalLimitPercent {
    fn name(&self) -> &'static str {
        "LIMIT_PERCENT"
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
        let total = data.len() as u64;
        let available = total.saturating_sub(self.offset);
        let limit = ((available as f64) * self.percent / 100.0) as u64;
        let mut kept = ColumnDataCollection::new(self.types.clone());
        let mut position = 0u64;
        for chunk in data.chunks() {
            let rows: Vec<u32> = (0..chunk.len() as u64)
                .filter(|row| {
                    let index = position + row;
                    index >= self.offset && index < self.offset + limit
                })
                .map(|row| row as u32)
                .collect();
            position += chunk.len() as u64;
            if !rows.is_empty() {
                let count = rows.len();
                let mut chunk = chunk.clone();
                chunk.slice(&SelectionVector::from_vec(rows), count);
                kept.append(&chunk);
            }
        }
        *data = kept;
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

/// `PhysicalReservoirSample` and `PhysicalStreamingSample`. The same seed gives the same sample, but
/// not the one C++ gives: its random engine is not ported. Rows come out in their input order.
#[derive(Debug)]
pub struct PhysicalSample {
    pub types: Vec<LogicalType>,
    pub options: SampleOptions,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalSample {
    pub fn new(options: SampleOptions, child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            options,
            data: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            children: vec![child],
        }
    }
}

/// A SplitMix64 stream.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl PhysicalOperator for PhysicalSample {
    fn name(&self) -> &'static str {
        match self.options.method {
            SampleMethod::Reservoir => "RESERVOIR_SAMPLE",
            _ => "STREAMING_SAMPLE",
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
        let mut data = self.data.lock();
        data.append(chunk);
        check_buffered(&data)
    }
    fn finalize(&self) -> Result<()> {
        let mut data = self.data.lock();
        let total = data.len();
        let mut random = self.options.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
        });
        let size: f64 = self.options.sample_size.parse().unwrap_or(0.0);
        let mut keep = vec![false; total];
        if self.options.is_percentage && self.options.method != SampleMethod::Reservoir {
            for row in &mut keep {
                *row = (next_random(&mut random) as f64 / u64::MAX as f64) * 100.0 < size;
            }
        } else {
            let wanted = if self.options.is_percentage {
                ((total as f64) * size / 100.0).round() as usize
            } else {
                size as usize
            };
            // the first `wanted` of a shuffle
            let mut rows: Vec<usize> = (0..total).collect();
            for i in 0..wanted.min(total) {
                let other = i + (next_random(&mut random) % (total - i) as u64) as usize;
                rows.swap(i, other);
                keep[rows[i]] = true;
            }
        }
        let mut sampled = ColumnDataCollection::new(self.types.clone());
        let mut offset = 0;
        for chunk in data.chunks() {
            let selected: Vec<u32> = (0..chunk.len())
                .filter(|i| keep[offset + i])
                .map(|i| i as u32)
                .collect();
            offset += chunk.len();
            let mut chunk = chunk.clone();
            let count = selected.len();
            chunk.slice(&SelectionVector::from_vec(selected), count);
            sampled.append(&chunk);
        }
        *data = sampled;
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
    pub seen: Mutex<RowKeySet>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalDistinct {
    pub fn new(targets: &[BoundExpr], child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            targets: targets.iter().map(reference_index).collect(),
            seen: Mutex::default(),
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
        let keys = create_row_keys(&columns, chunk.len())?;
        let mut seen = self.seen.lock();
        let new_rows: Vec<u32> = keys
            .into_iter()
            .enumerate()
            .filter_map(|(i, key)| seen.insert(key).then_some(i as u32))
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
/// values, so NULLs group together and equal values compare like DuckDB's. Each grouping set
/// aggregates on its own; together they cover the ungrouped case, as one set without groups.
#[derive(Debug)]
pub struct PhysicalAggregate {
    pub types: Vec<LogicalType>,
    pub groups: Vec<BoundExpr>,
    pub aggregates: Vec<BoundAggregate>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    grouping_sets: Vec<GroupingSet>,
    state: Mutex<Vec<AggregateSinkState>>,
    output: Mutex<Vec<DataChunk>>,
    /// where rows go past the memory limit, for an aggregate that can spill
    spill: Option<AggregateSpill>,
}

/// How many partitions the rows of an aggregate past the memory limit go into.
const AGGREGATE_PARTITIONS: usize = 64;

/// How many times a partition that is still too large is partitioned again.
const MAX_AGGREGATE_SPILL_LEVEL: u32 = 3;

/// What an aggregate past the memory limit spills: the input rows of the groups that it does not
/// hold, by the hash of their group, into a file per partition. Each partition is aggregated on its
/// own once the groups in memory are out (`RadixPartitionedHashTable` with external partitions).
/// A group is either in memory or in a partition when one thread aggregates, so its rows stay in
/// order; threads can each have a group, and then those in memory join their partition.
#[derive(Debug)]
struct AggregateSpill {
    target: SpillTarget,
    /// whether threads sank, so that groups in memory may also be in a partition
    threads: AtomicBool,
    /// whether any rows spilled
    spilled: AtomicBool,
    /// what states took of the memory limit
    reserved: AtomicUsize,
    /// the partitions to aggregate, with the groups in memory that belong to them
    pending: Mutex<Vec<PendingPartition>>,
}

/// The partition files of one level of spilling.
#[derive(Debug)]
struct SpillTarget {
    level: u32,
    files: PartitionedSpill,
}

impl SpillTarget {
    fn new(level: u32) -> Self {
        Self {
            level,
            files: PartitionedSpill::new(AGGREGATE_PARTITIONS),
        }
    }

    fn partition(&self, key: &RowKey) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = duckrs_common::sort_key::KeyHasher::default();
        self.level.hash(&mut hasher);
        key.hash(&mut hasher);
        (hasher.finish() >> 32) as usize % AGGREGATE_PARTITIONS
    }

    fn write(&self, partition: usize, chunk: &DataChunk) -> Result<()> {
        self.files.write(partition, chunk)
    }

    /// The files that rows went to, by partition.
    fn take(&self) -> Result<Vec<Option<SpillFile>>> {
        self.files.take()
    }
}

/// A partition of spilled rows still to aggregate.
#[derive(Debug)]
struct PendingPartition {
    file: SpillFile,
    level: u32,
    /// the groups in memory of this partition
    groups: Option<AggregateSinkState>,
}

/// A grouping set of a [`PhysicalAggregate`] (`HashAggregateGroupingData`).
#[derive(Debug)]
struct GroupingSet {
    /// the groups of the set; the others are NULL in its rows
    groups: Vec<usize>,
    /// the result of each GROUPING call in the rows of this set
    grouping_values: Vec<i64>,
}

impl GroupingSet {
    fn new(groups: Vec<usize>, grouping_functions: &[Vec<usize>]) -> Self {
        // `RadixPartitionedHashTable::SetGroupingValues`: a call has a bit per argument, the
        // first argument highest, set when the set does not group on it
        let grouping_values = grouping_functions
            .iter()
            .map(|function| {
                function
                    .iter()
                    .fold(0, |value, group| value << 1 | i64::from(!groups.contains(group)))
            })
            .collect();
        Self {
            groups,
            grouping_values,
        }
    }
}

/// The groups of a grouping set so far and their aggregate states.
#[derive(Debug)]
struct AggregateSinkState {
    group_ids: RowKeyMap<usize>,
    group_values: ColumnDataCollection,
    group_count: usize,
    /// per aggregate, the states of the groups
    states: Vec<Vec<AggregateState>>,
    /// per aggregate, per group, the values a DISTINCT aggregate has seen
    distinct_seen: Vec<std::collections::HashSet<(usize, Vec<u8>)>>,
    /// per aggregate with ORDER BY, its rows, which it sees once they can be put in order
    ordered: Vec<Vec<OrderedRow>>,
    /// roughly what the groups and states hold, and what of that the memory limit gave them
    bytes: usize,
    reserved: usize,
    /// whether the memory limit gave no more, so that the rows of new groups spill
    full: bool,
}

impl AggregateSinkState {
    fn new(group_types: Vec<LogicalType>, aggregates: &[BoundAggregate]) -> Self {
        // a set without groups has its one group from the start, so it has a row for no input
        let group_count = usize::from(group_types.is_empty());
        Self {
            group_ids: RowKeyMap::default(),
            group_values: ColumnDataCollection::new(group_types),
            group_count,
            states: aggregates
                .iter()
                .map(|a| vec![a.function.kind.initial_state(); group_count])
                .collect(),
            distinct_seen: vec![std::collections::HashSet::new(); aggregates.len()],
            ordered: aggregates.iter().map(|_| Vec::new()).collect(),
            bytes: 0,
            reserved: 0,
            full: false,
        }
    }
}

/// A row an aggregate with ORDER BY has yet to see.
#[derive(Debug)]
struct OrderedRow {
    group: usize,
    key: Vec<u8>,
    /// the aggregate's input; `None` for `count(*)`
    value: Option<Value>,
}

/// What an aggregate reads from a chunk; computed once, whatever the number of grouping sets.
struct AggregateInput {
    /// the rows its FILTER lets through
    filtered: Option<Vec<usize>>,
    input: Option<Vector>,
    /// with DISTINCT, the key of each row's value; `None` for NULLs, which are not values
    distinct_keys: Option<Vec<Option<Vec<u8>>>>,
    /// with ORDER BY, the sort key of each row
    order_keys: Option<Vec<Vec<u8>>>,
}

impl PhysicalAggregate {
    pub fn new(
        types: Vec<LogicalType>,
        groups: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        grouping_sets: &[BTreeSet<usize>],
        grouping_functions: &[Vec<usize>],
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let mut sets: Vec<GroupingSet> = grouping_sets
            .iter()
            .map(|set| GroupingSet::new(set.iter().copied().collect(), grouping_functions))
            .collect();
        if sets.is_empty() {
            sets.push(GroupingSet::new((0..groups.len()).collect(), grouping_functions));
        }
        let state = sets
            .iter()
            .map(|set| {
                let group_types = set.groups.iter().map(|&g| groups[g].return_type.clone()).collect();
                AggregateSinkState::new(group_types, &aggregates)
            })
            .collect();
        Self {
            types,
            groups,
            aggregates,
            children,
            spill: None,
            grouping_sets: sets,
            state: Mutex::new(state),
            output: Mutex::default(),
        }
        .with_spill()
    }

    /// Spilling needs one grouping set with groups, groups that are the same when computed again,
    /// and input that can go to a file.
    fn with_spill(mut self) -> Self {
        let can_spill = self.grouping_sets.len() == 1
            && !self.grouping_sets[0].groups.is_empty()
            && self.groups.iter().all(parallel_expression)
            && can_serialize(self.children[0].types());
        if can_spill {
            self.spill = Some(AggregateSpill {
                target: SpillTarget::new(0),
                threads: AtomicBool::new(false),
                spilled: AtomicBool::new(false),
                reserved: AtomicUsize::new(0),
                pending: Mutex::default(),
            });
        }
        self
    }

    /// Roughly what a group takes besides its values: its key and entry in the map, which has
    /// room to grow, and its states, whose vectors do too, with what a state may keep apart.
    fn bytes_per_group(&self) -> usize {
        let key = std::mem::size_of::<RowKey>() + std::mem::size_of::<usize>();
        2 * key + self.aggregates.len() * (2 * std::mem::size_of::<AggregateState>() + 32)
    }

    /// Roughly what the states hold for each input row, besides the groups: rows that an
    /// aggregate keeps, the values DISTINCT has seen, and the rows ORDER BY holds back.
    fn bytes_per_row(&self) -> usize {
        self.aggregates
            .iter()
            .filter(|a| a.distinct || !a.order_by.is_empty() || !a.function.kind.is_order_independent())
            .count()
            * 32
    }

    /// Sinks `chunk` into `states`. Past the memory limit, the rows of groups the state does not
    /// have go to `target` instead.
    fn sink_chunk(
        &self,
        chunk: &DataChunk,
        states: &mut [AggregateSinkState],
        target: Option<&SpillTarget>,
    ) -> Result<()> {
        let Some(target) = target else {
            let inputs = self.sink_inputs(chunk)?;
            return self.sink_into(chunk, &inputs, states);
        };
        let mut known = chunk.clone();
        if states[0].full {
            let vectors = self.grouping_sets[0]
                .groups
                .iter()
                .map(|&g| execute(&self.groups[g], chunk))
                .collect::<Result<Vec<_>>>()?;
            let columns: Vec<&Vector> = vectors.iter().collect();
            let keys = create_row_keys(&columns, chunk.len())?;
            let mut known_rows = Vec::new();
            let mut spilled: Vec<Vec<u32>> = vec![Vec::new(); AGGREGATE_PARTITIONS];
            for (row, key) in keys.iter().enumerate() {
                if states[0].group_ids.contains_key(key) {
                    known_rows.push(row as u32);
                } else {
                    spilled[target.partition(key)].push(row as u32);
                }
            }
            for (partition, rows) in spilled.into_iter().enumerate().filter(|(_, rows)| !rows.is_empty()) {
                let mut rows_of = chunk.clone();
                let count = rows.len();
                rows_of.slice(&SelectionVector::from_vec(rows), count);
                target.write(partition, &rows_of)?;
            }
            if known_rows.is_empty() {
                return Ok(());
            }
            if known_rows.len() < chunk.len() {
                let count = known_rows.len();
                known.slice(&SelectionVector::from_vec(known_rows), count);
            }
        }
        let inputs = self.sink_inputs(&known)?;
        self.sink_into(&known, &inputs, states)?;
        let state = &mut states[0];
        if !state.full {
            state.bytes += known.len() * self.bytes_per_row();
            let more = state.bytes.saturating_sub(state.reserved);
            if temporary::current().try_reserve(more) {
                state.reserved += more;
                if let Some(spill) = &self.spill {
                    spill.reserved.fetch_add(more, Ordering::Relaxed);
                }
            } else {
                state.full = true;
            }
        }
        Ok(())
    }

    /// Takes the groups of the partitions in `wanted` out of `state`, into a state per partition.
    fn take_partitions(
        &self,
        state: &mut AggregateSinkState,
        target: &SpillTarget,
        wanted: &[bool],
    ) -> Vec<Option<AggregateSinkState>> {
        let types = state.group_values.types().to_vec();
        let new_state = || AggregateSinkState::new(types.clone(), &self.aggregates);
        let mut taken: Vec<Option<AggregateSinkState>> = (0..AGGREGATE_PARTITIONS).map(|_| None).collect();
        if !wanted.iter().any(|&w| w) {
            return taken;
        }
        let old = std::mem::replace(state, new_state());
        state.full = old.full;
        let mut keys: Vec<Option<RowKey>> = vec![None; old.group_count];
        for (key, id) in old.group_ids {
            keys[id] = Some(key);
        }
        let mut old_states = old.states;
        let mut first = 0;
        for chunk in old.group_values.chunks() {
            // per destination (a partition, or what stays at the end), the rows of this chunk
            let mut rows: Vec<Vec<u32>> = vec![Vec::new(); AGGREGATE_PARTITIONS + 1];
            for row in 0..chunk.len() {
                let id = first + row;
                let key = keys[id].take().expect("a key per group");
                let partition = target.partition(&key);
                let destination = if wanted[partition] {
                    partition
                } else {
                    AGGREGATE_PARTITIONS
                };
                let into = if destination == AGGREGATE_PARTITIONS {
                    &mut *state
                } else {
                    taken[destination].get_or_insert_with(new_state)
                };
                into.group_ids.insert(key, into.group_count);
                into.group_count += 1;
                for ((states, old), aggregate) in
                    into.states.iter_mut().zip(old_states.iter_mut()).zip(&self.aggregates)
                {
                    states.push(std::mem::replace(&mut old[id], aggregate.function.kind.initial_state()));
                }
                rows[destination].push(row as u32);
            }
            for (destination, rows) in rows.into_iter().enumerate().filter(|(_, rows)| !rows.is_empty()) {
                let into = if destination == AGGREGATE_PARTITIONS {
                    &mut *state
                } else {
                    taken[destination].as_mut().expect("made above")
                };
                let mut values = chunk.clone();
                let count = rows.len();
                values.slice(&SelectionVector::from_vec(rows), count);
                into.group_values.append(&values);
            }
            first += chunk.len();
        }
        taken
    }

    /// What ORDER BY held back goes in now, a row at a time, in order (stably, like DuckDB).
    fn finish_ordered(&self, state: &mut AggregateSinkState) -> Result<()> {
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
        Ok(())
    }

    /// The partitions that rows spilled to at `target`, each with its groups in memory when
    /// threads may have put them there too.
    fn pending_partitions(
        &self,
        state: &mut AggregateSinkState,
        target: &SpillTarget,
        threads: bool,
    ) -> Result<Vec<PendingPartition>> {
        let files = target.take()?;
        let wanted: Vec<bool> = files.iter().map(|file| threads && file.is_some()).collect();
        let mut groups = self.take_partitions(state, target, &wanted);
        Ok(files
            .into_iter()
            .enumerate()
            .filter_map(|(partition, file)| {
                file.map(|file| PendingPartition {
                    file,
                    level: target.level + 1,
                    groups: groups[partition].take(),
                })
            })
            .collect())
    }

    /// Aggregates a spilled partition, whose own spills join `pending`.
    fn aggregate_partition(
        &self,
        partition: PendingPartition,
        pending: &mut Vec<PendingPartition>,
    ) -> Result<Vec<DataChunk>> {
        let set = &self.grouping_sets[0];
        let types = set.groups.iter().map(|&g| self.groups[g].return_type.clone()).collect();
        let mut states = vec![
            partition
                .groups
                .unwrap_or_else(|| AggregateSinkState::new(types, &self.aggregates)),
        ];
        let target = (partition.level <= MAX_AGGREGATE_SPILL_LEVEL).then(|| SpillTarget::new(partition.level));
        let mut reader = partition.file.into_reader()?;
        while let Some(chunk) = reader.next_chunk()? {
            self.sink_chunk(&chunk, &mut states, target.as_ref())?;
        }
        drop(reader);
        let mut state = states.pop().expect("one state");
        if let Some(target) = &target {
            pending.extend(self.pending_partitions(&mut state, target, false)?);
        }
        temporary::current().release(state.reserved);
        self.finish_ordered(&mut state)?;
        let mut output = Vec::new();
        self.set_output(set, &state, &mut output)?;
        Ok(output)
    }

    /// The group of each row of the chunk the `vectors` of the groups come from, creating
    /// groups as needed.
    fn find_groups(
        &self,
        set: &GroupingSet,
        vectors: &[Vector],
        count: usize,
        state: &mut AggregateSinkState,
    ) -> Result<Vec<usize>> {
        if set.groups.is_empty() {
            return Ok(vec![0; count]);
        }
        let columns: Vec<&Vector> = set.groups.iter().map(|&g| &vectors[g]).collect();
        let keys = create_row_keys(&columns, count)?;
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
            let new_count = new_rows.len();
            let columns = set.groups.iter().map(|&g| vectors[g].clone()).collect();
            let mut values = DataChunk::from_columns(columns, count);
            values.slice(&SelectionVector::from_vec(new_rows), new_count);
            state.bytes += temporary::estimated_bytes(&values) + new_count * self.bytes_per_group();
            state.group_values.append(&values);
            if self.spill.is_none() {
                check_buffered(&state.group_values)?;
            }
            for (states, aggregate) in state.states.iter_mut().zip(&self.aggregates) {
                states.resize(state.group_count, aggregate.function.kind.initial_state());
            }
        }
        Ok(ids)
    }

    fn aggregate_input(aggregate: &BoundAggregate, chunk: &DataChunk) -> Result<AggregateInput> {
        let filtered = match &aggregate.filter {
            Some(filter) => {
                let (sel, count) = select(filter, chunk)?;
                Some((0..count).map(|r| sel.get(r)).collect())
            }
            None => None,
        };
        let input = aggregate.children.first().map(|c| execute(c, chunk)).transpose()?;
        let distinct_keys = match (aggregate.distinct, &input) {
            (true, Some(input)) => {
                let unified = input.to_unified(chunk.len());
                let keys = value_keys(input, chunk.len())?.into_iter().enumerate();
                Some(keys.map(|(row, key)| unified.is_valid(row).then_some(key)).collect())
            }
            _ => None,
        };
        let order_keys = if aggregate.order_by.is_empty() {
            None
        } else {
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
            Some(create_sort_keys(&columns, chunk.len())?)
        };
        Ok(AggregateInput {
            filtered,
            input,
            distinct_keys,
            order_keys,
        })
    }

    /// The rows of a grouping set: its groups, NULL for the others, then the aggregates and the
    /// results of the GROUPING calls.
    fn set_output(&self, set: &GroupingSet, state: &AggregateSinkState, output: &mut Vec<DataChunk>) -> Result<()> {
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
        let row_chunk = |group_columns: &[Vector], group: usize, count: usize| -> Result<DataChunk> {
            let mut columns: Vec<Vector> = self
                .groups
                .iter()
                .enumerate()
                .map(|(i, expr)| match set.groups.iter().position(|&g| g == i) {
                    Some(position) => group_columns[position].clone(),
                    None => Vector::constant_null(expr.return_type.clone()),
                })
                .collect();
            columns.extend(finalize(group, count)?);
            columns.extend(set.grouping_values.iter().map(|&v| Vector::constant(&Value::bigint(v))));
            Ok(DataChunk::from_columns(columns, count))
        };
        if set.groups.is_empty() {
            output.push(row_chunk(&[], 0, 1)?);
            return Ok(());
        }
        let mut offset = 0;
        for chunk in state.group_values.chunks() {
            output.push(row_chunk(chunk.columns(), offset, chunk.len())?);
            offset += chunk.len();
        }
        Ok(())
    }
}

impl PhysicalOperator for PhysicalAggregate {
    fn name(&self) -> &'static str {
        if self.groups.is_empty() && self.grouping_sets.len() == 1 {
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
        if self.spill.is_some() {
            let mut states = self.state.lock();
            return self.sink_chunk(chunk, &mut states, self.spill.as_ref().map(|s| &s.target));
        }
        let inputs = self.sink_inputs(chunk)?;
        let mut states = self.state.lock();
        self.sink_into(chunk, &inputs, &mut states)
    }
    fn parallel_sink(&self) -> bool {
        self.grouping_sets.len() == 1
            && self.groups.iter().all(parallel_expression)
            && self.aggregates.iter().all(|a| {
                !a.distinct
                    && a.order_by.is_empty()
                    && a.function.kind.is_order_independent()
                    && a.children.iter().all(parallel_expression)
                    && a.filter.as_ref().is_none_or(parallel_expression)
            })
    }
    fn local_sink_state(&self) -> LocalSinkState {
        let states: Vec<AggregateSinkState> = self
            .grouping_sets
            .iter()
            .map(|set| {
                let types = set.groups.iter().map(|&g| self.groups[g].return_type.clone()).collect();
                AggregateSinkState::new(types, &self.aggregates)
            })
            .collect();
        if let Some(spill) = &self.spill {
            spill.threads.store(true, Ordering::Relaxed);
        }
        Box::new(states)
    }
    fn sink_local(&self, local: &mut LocalSinkState, _batch: usize, chunk: &DataChunk) -> Result<()> {
        let states: &mut Vec<AggregateSinkState> = local.downcast_mut().expect("aggregate local state");
        self.sink_chunk(chunk, states, self.spill.as_ref().map(|s| &s.target))
    }
    fn combine_local(&self, local: LocalSinkState) -> Result<()> {
        let local = *local
            .downcast::<Vec<AggregateSinkState>>()
            .expect("aggregate local state");
        let mut states = self.state.lock();
        for (state, local) in states.iter_mut().zip(local) {
            self.merge_groups(state, local)?;
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut states = self.state.lock();
        let mut output = Vec::new();
        if let Some(spill) = &self.spill {
            let threads = spill.threads.load(Ordering::Relaxed);
            let pending = self.pending_partitions(&mut states[0], &spill.target, threads)?;
            spill.spilled.store(!pending.is_empty(), Ordering::Relaxed);
            *spill.pending.lock() = pending;
            temporary::current().release(spill.reserved.swap(0, Ordering::Relaxed));
        }
        for (set, state) in self.grouping_sets.iter().zip(states.iter_mut()) {
            self.finish_ordered(state)?;
            self.set_output(set, state, &mut output)?;
            if self.spill.is_some() {
                // the output has what it held, and the partitions need the room
                let types = state.group_values.types().to_vec();
                *state = AggregateSinkState::new(types, &self.aggregates);
            }
        }
        *self.output.lock() = output;
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
        let mut output = self.output.lock();
        let Some(spill) = self
            .spill
            .as_ref()
            .filter(|spill| spill.spilled.load(Ordering::Relaxed))
        else {
            let chunk = output.get(*next).cloned();
            *next += 1;
            return Ok(chunk);
        };
        // spilled output is taken as it goes, so that it is not all in memory at once
        loop {
            if *next < output.len() {
                *next += 1;
                return Ok(Some(std::mem::replace(
                    &mut output[*next - 1],
                    DataChunk::from_columns(Vec::new(), 0),
                )));
            }
            let mut pending = spill.pending.lock();
            let Some(partition) = pending.pop() else {
                return Ok(None);
            };
            let chunks = self.aggregate_partition(partition, &mut pending)?;
            output.extend(chunks);
        }
    }
}

impl PhysicalAggregate {
    /// What a chunk gives the groups and the aggregates: the part threads need no lock for.
    fn sink_inputs(&self, chunk: &DataChunk) -> Result<(Vec<Vector>, Vec<AggregateInput>)> {
        let vectors = self
            .groups
            .iter()
            .map(|g| execute(g, chunk))
            .collect::<Result<Vec<_>>>()?;
        let inputs = self
            .aggregates
            .iter()
            .map(|a| Self::aggregate_input(a, chunk))
            .collect::<Result<Vec<_>>>()?;
        Ok((vectors, inputs))
    }

    /// `CombineStates`: the groups of a thread go into `state`.
    fn merge_groups(&self, state: &mut AggregateSinkState, local: AggregateSinkState) -> Result<()> {
        let AggregateSinkState {
            group_ids,
            group_values,
            group_count,
            states,
            ..
        } = local;
        // a set without groups has its one group on both sides
        let mut targets = vec![0; group_count];
        if !group_ids.is_empty() {
            let mut keys: Vec<Option<RowKey>> = vec![None; group_count];
            for (key, id) in group_ids {
                keys[id] = Some(key);
            }
            let mut first = 0;
            for chunk in group_values.chunks() {
                let mut new_rows = Vec::new();
                for row in 0..chunk.len() {
                    let key = keys[first + row].take().expect("a key per group");
                    let next = state.group_count;
                    let id = *state.group_ids.entry(key).or_insert(next);
                    if id == next {
                        state.group_count += 1;
                        new_rows.push(row as u32);
                    }
                    targets[first + row] = id;
                }
                if !new_rows.is_empty() {
                    let new_count = new_rows.len();
                    let mut values = chunk.clone();
                    values.slice(&SelectionVector::from_vec(new_rows), new_count);
                    state.group_values.append(&values);
                }
                first += chunk.len();
            }
            if self.spill.is_none() {
                check_buffered(&state.group_values)?;
            }
        }
        for (i, (aggregate, sourced)) in self.aggregates.iter().zip(states).enumerate() {
            let kind = aggregate.function.kind;
            let merged = &mut state.states[i];
            merged.resize(state.group_count, kind.initial_state());
            for (group, source) in sourced.into_iter().enumerate() {
                let slot = &mut merged[targets[group]];
                let current = std::mem::replace(slot, kind.initial_state());
                *slot = combine_states(kind, current, source)?;
            }
        }
        Ok(())
    }

    /// Takes what [`Self::sink_inputs`] made of a chunk into `states`.
    fn sink_into(
        &self,
        chunk: &DataChunk,
        (vectors, inputs): &(Vec<Vector>, Vec<AggregateInput>),
        states: &mut [AggregateSinkState],
    ) -> Result<()> {
        for (set, state) in self.grouping_sets.iter().zip(states.iter_mut()) {
            let ids = self.find_groups(set, vectors, chunk.len(), state)?;
            for (i, (aggregate, input)) in self.aggregates.iter().zip(inputs).enumerate() {
                let mut rows: Vec<(usize, usize)> = match &input.filtered {
                    Some(filtered) => filtered.iter().map(|&row| (row, ids[row])).collect(),
                    None => ids.iter().copied().enumerate().collect(),
                };
                if let Some(keys) = &input.distinct_keys {
                    // each value counts once per group
                    let seen = &mut state.distinct_seen[i];
                    rows.retain(|&(row, group)| {
                        keys[row].as_ref().is_some_and(|key| seen.insert((group, key.clone())))
                    });
                }
                if let Some(keys) = &input.order_keys {
                    let ordered = &mut state.ordered[i];
                    for &(row, group) in &rows {
                        ordered.push(OrderedRow {
                            group,
                            key: keys[row].clone(),
                            value: input.input.as_ref().map(|input| input.get_value(row)),
                        });
                    }
                    check_buffered_rows(&[LogicalType::HUGEINT, LogicalType::HUGEINT], ordered.len())?;
                    continue;
                }
                aggregate
                    .function
                    .update(input.input.as_ref(), chunk.len(), &rows, &mut state.states[i])?;
            }
        }
        Ok(())
    }
}

/// What a statement that changes rows gives back: their count, or with RETURNING the rows.
fn changed_rows_output(
    types: &[LogicalType],
    rows: ColumnDataCollection,
    count: u64,
    returning: bool,
) -> ColumnDataCollection {
    if returning {
        return rows;
    }
    let mut output = ColumnDataCollection::new(types.to_vec());
    let mut column = Vector::new(LogicalType::BIGINT, 1);
    column.data_mut::<i64>()[0] = count as i64;
    output.append(&DataChunk::from_columns(vec![column], 1));
    output
}

/// Where the rows with these ids are, for those that are there.
fn row_positions(table: &TableCatalogEntry, ids: impl Iterator<Item = i64>, count: usize) -> HashSet<usize> {
    let row_ids = table.row_ids(count);
    ids.filter_map(|id| row_ids.position(id as u64)).collect()
}

/// `PhysicalDelete`: gathers the ids of its input (sink), and removes those rows at the end, so
/// that the statement reads the table as it was.
#[derive(Debug)]
pub struct PhysicalDelete {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    table: Arc<TableCatalogEntry>,
    returning: bool,
    row_ids: Mutex<HashSet<i64>>,
    output: Mutex<ColumnDataCollection>,
}

impl PhysicalDelete {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        table: Arc<TableCatalogEntry>,
        returning: bool,
    ) -> Self {
        let output = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            table,
            returning,
            row_ids: Mutex::default(),
            output,
        }
    }
}

impl PhysicalOperator for PhysicalDelete {
    fn name(&self) -> &'static str {
        "DELETE"
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
        let mut ids = self.row_ids.lock();
        for row in 0..chunk.len() {
            if let Some(id) = chunk.column(0).get_value(row).as_i128() {
                ids.insert(id as i64);
            }
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let count = self.table.row_count();
        let positions = row_positions(&self.table, self.row_ids.lock().iter().copied(), count);
        self.table.claim_rows(positions.iter().copied(), count, false)?;
        let mut storage = self.table.write_storage()?;
        let mut kept = ColumnDataCollection::new(storage.types().to_vec());
        let mut deleted = ColumnDataCollection::new(storage.types().to_vec());
        for (index, chunk) in storage.chunks().enumerate() {
            let first_row = index * STANDARD_VECTOR_SIZE;
            let (gone, stay): (Vec<u32>, Vec<u32>) =
                (0..chunk.len() as u32).partition(|row| positions.contains(&(first_row + *row as usize)));
            if gone.is_empty() {
                kept.append(chunk);
                continue;
            }
            for (rows, target) in [(stay, &mut kept), (gone, &mut deleted)] {
                let count = rows.len();
                if count > 0 {
                    let mut part = chunk.clone();
                    part.slice(&SelectionVector::from_vec(rows), count);
                    target.append(&part);
                }
            }
        }
        constraints::verify_delete(&self.table, &storage, &deleted)?;
        *storage = kept;
        *self.table.unique_keys.lock() = None;
        let mut removed: Vec<usize> = positions.iter().copied().collect();
        removed.sort_unstable();
        self.table.rows_moved(&removed, count);
        *self.output.lock() = changed_rows_output(&self.types, deleted, removed.len() as u64, self.returning);
        Ok(())
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

/// `PhysicalUpdate`: gathers the new values (sink), and writes them at the end.
#[derive(Debug)]
pub struct PhysicalUpdate {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    table: Arc<TableCatalogEntry>,
    columns: Vec<usize>,
    returning: bool,
    /// the new values by row id; a row a join matches twice keeps its first ones
    updates: Mutex<HashMap<i64, Vec<Value>>>,
    output: Mutex<ColumnDataCollection>,
}

impl PhysicalUpdate {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        table: Arc<TableCatalogEntry>,
        columns: Vec<usize>,
        returning: bool,
    ) -> Self {
        let output = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            table,
            columns,
            returning,
            updates: Mutex::default(),
            output,
        }
    }
}

impl PhysicalOperator for PhysicalUpdate {
    fn name(&self) -> &'static str {
        "UPDATE"
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
        let id_column = self.columns.len();
        let mut updates = self.updates.lock();
        for row in 0..chunk.len() {
            let values: Vec<Value> = (0..id_column).map(|c| chunk.column(c).get_value(row)).collect();
            for (value, column) in values.iter().zip(&self.columns) {
                let column = &self.table.columns[*column];
                if column.not_null && value.is_null() {
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
            let id = chunk.column(id_column).get_value(row).as_i128().expect("a row id") as i64;
            updates.entry(id).or_insert(values);
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let count = self.table.row_count();
        let ids = self.table.row_ids(count);
        let by_id = self.updates.lock();
        let updates: HashMap<usize, &Vec<Value>> = by_id
            .iter()
            .filter_map(|(id, values)| Some((ids.position(*id as u64)?, values)))
            .collect();
        let moves_rows = self.table.update_moves_rows(&self.columns);
        self.table.claim_rows(updates.keys().copied(), count, !moves_rows)?;
        let mut storage = self.table.write_storage()?;
        let mut rewritten = ColumnDataCollection::new(storage.types().to_vec());
        let mut changed = ColumnDataCollection::new(storage.types().to_vec());
        let key_update = moves_rows || !self.table.referenced_by.lock().is_empty();
        let mut batches = Vec::new();
        for (index, chunk) in storage.chunks().enumerate() {
            let first_row = index * STANDARD_VECTOR_SIZE;
            let rows: Vec<(usize, &Vec<Value>)> = (0..chunk.len())
                .filter_map(|row| Some((row, *updates.get(&(first_row + row))?)))
                .collect();
            if rows.is_empty() {
                rewritten.append(chunk);
                continue;
            }
            let mut columns: Vec<Vector> = chunk.columns().to_vec();
            for (position, &column) in self.columns.iter().enumerate() {
                let mut target = Vector::new(columns[column].logical_type().clone(), chunk.len().max(1));
                let mut updated = rows.iter();
                let mut next = updated.next();
                for row in 0..chunk.len() {
                    let value = match next {
                        Some((at, values)) if *at == row => {
                            let value = values[position].clone();
                            next = updated.next();
                            value
                        }
                        _ => columns[column].get_value(row),
                    };
                    target.set_value(row, &value)?;
                }
                columns[column] = target;
            }
            let updated = DataChunk::from_columns(columns, chunk.len());
            if moves_rows {
                let stay: Vec<u32> = (0..chunk.len() as u32)
                    .filter(|row| rows.iter().all(|(updated, _)| *updated != *row as usize))
                    .collect();
                let count = stay.len();
                if count > 0 {
                    let mut part = updated.clone();
                    part.slice(&SelectionVector::from_vec(stay), count);
                    rewritten.append(&part);
                }
            } else {
                rewritten.append(&updated);
            }
            constraints::verify_checks(&self.table, &updated)?;
            if self.returning || key_update {
                let picked: Vec<u32> = rows.iter().map(|(row, _)| *row as u32).collect();
                let count = picked.len();
                let mut part = updated.clone();
                part.slice(&SelectionVector::from_vec(picked.clone()), count);
                changed.append(&part);
                if self.returning && moves_rows {
                    let mut old = chunk.clone();
                    old.slice(&SelectionVector::from_vec(picked), count);
                    batches.push((old, part));
                }
            }
        }
        if !batches.is_empty() {
            constraints::verify_chunked_key_update(&self.table, &storage, &batches)?;
        }
        if moves_rows {
            for chunk in changed.chunks() {
                rewritten.append(chunk);
            }
        }
        if key_update {
            constraints::verify_key_update(&self.table, &storage, &rewritten, &changed)?;
        }
        *storage = rewritten;
        if moves_rows {
            let mut removed: Vec<usize> = updates.keys().copied().collect();
            removed.sort_unstable();
            self.table.rows_moved(&removed, count);
        } else {
            self.table.rows_updated_in_place(updates.keys().copied());
            drop(storage);
            self.table.rows_updated(updates.keys().copied(), &self.columns, count);
        }
        *self.output.lock() = changed_rows_output(&self.types, changed, updates.len() as u64, self.returning);
        Ok(())
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

/// `PhysicalInsert`: appends its input to a table (sink), then returns the row count (source).
#[derive(Debug)]
pub struct PhysicalInsert {
    pub types: Vec<LogicalType>,
    pub table: Arc<TableCatalogEntry>,
    pub column_index_map: Vec<Option<usize>>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub inserted: Mutex<u64>,
    /// The rows of the table when this statement started to add to them.
    pub first_new_row: Mutex<Option<usize>>,
    pub returning: bool,
    pub on_conflict: Option<OnConflict>,
    /// With `on_conflict`, the rows wait here for the statement's end.
    pub pending: Mutex<ColumnDataCollection>,
    /// With `returning`, the rows to return.
    pub output: Mutex<ColumnDataCollection>,
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
        if self.on_conflict.is_some() {
            self.pending.lock().append(&row);
            return Ok(());
        }
        constraints::verify_not_null(&self.table, &row)?;
        let mut storage = self.table.append_storage()?;
        let first_new_row = *self.first_new_row.lock().get_or_insert(storage.len());
        constraints::verify_append(&self.table, &storage, &row, first_new_row)?;
        storage.append(&row);
        check_buffered(&storage)?;
        drop(storage);
        *self.inserted.lock() += chunk.len() as u64;
        if self.returning {
            self.output.lock().append(&row);
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        if let Some(action) = &self.on_conflict {
            let (count, rows) = upsert(&self.table, action, &self.pending.lock(), self.returning)?;
            *self.inserted.lock() = count;
            *self.output.lock() = rows;
        }
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        if self.returning {
            Box::new(0usize)
        } else {
            Box::new(false)
        }
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        if self.returning {
            return Ok(scan_collection(&self.output, state));
        }
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
        let table = self.schema.create_table(
            &self.table_name,
            self.columns.clone(),
            Vec::new(),
            Vec::new(),
            self.on_conflict,
        )?;
        if let Some(table) = table {
            let data = self.data.lock();
            let mut storage = table.append_storage()?;
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
    pub generated: Vec<GeneratedColumn>,
    pub constraints: Vec<TableConstraint>,
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
        self.schema.create_table(
            &self.table_name,
            self.columns.clone(),
            self.generated.clone(),
            self.constraints.clone(),
            self.on_conflict,
        )?;
        Ok(None)
    }
}

/// `PhysicalMaterializedCollector`, and `PhysicalBatchCollector` when threads gather the query
/// result: each keeps the rows of a morsel together, and they go into the result in the order
/// of the morsels, which is the order one thread would have given them.
#[derive(Debug)]
pub struct PhysicalResultCollector {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub result: Mutex<ColumnDataCollection>,
    /// the rows of each morsel, from threads
    batches: Mutex<Vec<(usize, ColumnDataCollection)>>,
    /// how many rows threads gathered
    parallel_rows: AtomicUsize,
}

/// The rows of each morsel a thread gathered, in the order it gathered them.
type LocalBatches = Vec<(usize, ColumnDataCollection)>;

impl PhysicalResultCollector {
    pub fn new(child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            result: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            children: vec![child],
            batches: Mutex::default(),
            parallel_rows: AtomicUsize::new(0),
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
    fn parallel_sink(&self) -> bool {
        true
    }
    fn local_sink_state(&self) -> LocalSinkState {
        Box::new(LocalBatches::new())
    }
    fn sink_local(&self, local: &mut LocalSinkState, batch: usize, chunk: &DataChunk) -> Result<()> {
        let local: &mut LocalBatches = local.downcast_mut().expect("collector local state");
        if local.last().is_none_or(|(last, _)| *last != batch) {
            local.push((batch, ColumnDataCollection::new(self.types.clone())));
        }
        local.last_mut().expect("just pushed").1.append(chunk);
        let rows = self.parallel_rows.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
        check_buffered_rows(&self.types, rows)
    }
    fn combine_local(&self, local: LocalSinkState) -> Result<()> {
        let local = *local.downcast::<LocalBatches>().expect("collector local state");
        self.batches.lock().extend(local);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut batches = std::mem::take(&mut *self.batches.lock());
        batches.sort_by_key(|(batch, _)| *batch);
        let mut result = self.result.lock();
        for (_, rows) in batches {
            result.combine(rows);
        }
        Ok(())
    }
}
