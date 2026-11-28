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
use super::zone_map::groups_to_scan;
use crate::catalog::{
    ColumnDefinition, GeneratedColumn, OnCreateConflict, RowIds, SchemaCatalogEntry, TableCatalogEntry,
    TableConstraint, TableCursor, TableSource,
};
use crate::execution::external_sort::SortSpill;
use crate::execution::group_table::GroupTable;
use crate::execution::run_plan;
use crate::execution::temporary::{self, PartitionedSpill, SpillFile};
use crate::function::aggregates::{AggregateState, combine_states, value_keys};
use crate::planner::expression::{BoundAggregate, BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, COLUMN_IDENTIFIER_ROW_NUMBER, CteKey, LogicalOperator, OnConflict,
};
use crate::storage::log_writer::can_serialize;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{
    NullOrder, OrderModifiers, OrderType, RowKey, RowKeyMap, RowKeySet, create_row_key_buffer, create_row_keys,
    create_sort_keys, sort_collection,
};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, StringCopy, Vector};
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
    /// The chunks of morsel `index`, in order: a morsel of a source whose morsels are larger than a
    /// chunk (a row group of a database file) gives several.
    fn parallel_chunks(
        &self,
        state: &SharedSourceState,
        index: usize,
        emit: &mut dyn FnMut(DataChunk) -> Result<()>,
    ) -> Result<()> {
        match self.parallel_chunk(state, index)? {
            Some(chunk) => emit(chunk),
            None => Ok(()),
        }
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

    /// `FinalExecute`: what the operator still has once its input is done, a chunk at a time.
    fn final_execute(&self, _state: &mut OperatorState) -> Result<Option<DataChunk>> {
        Ok(None)
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
    /// The thread of `local` sank all of morsel `batch` (`PhysicalOperator::NextBatch`), which may have had no rows.
    fn batch_done_local(&self, _local: &mut LocalSinkState, _batch: usize) -> Result<()> {
        Ok(())
    }
    /// [`Self::parallel_sink`] for one of the inputs of a sink with several (a delim join).
    fn parallel_sink_input(&self, _input: usize) -> bool {
        self.sink_children().len() == 1 && self.parallel_sink()
    }
    fn local_sink_state_input(&self, _input: usize) -> LocalSinkState {
        self.local_sink_state()
    }
    fn sink_local_input(
        &self,
        _input: usize,
        local: &mut LocalSinkState,
        batch: usize,
        chunk: &DataChunk,
    ) -> Result<()> {
        self.sink_local(local, batch, chunk)
    }
    fn combine_local_input(&self, _input: usize, local: LocalSinkState) -> Result<()> {
        self.combine_local(local)
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

    /// `Dynamic Filters`: what the joins above gave a table scan, once they did.
    fn dynamic_filters(&self) -> Option<String> {
        None
    }

    /// `query.total_rows_scanned` of a scan that counts the rows it reads itself, before its filters; without
    /// one, the rows it gives on count.
    fn rows_read(&self) -> Option<u64> {
        None
    }

    /// `ReportsBytesScanned`: a scan of files that counts the bytes it reads of them.
    fn reports_bytes_scanned(&self) -> bool {
        false
    }

    /// The filters a scan got when the plan was made physical that are not in its logical plan: those of a
    /// Top-N above it, as `Filters` shows them.
    fn planned_filters(&self) -> Vec<String> {
        Vec::new()
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
    /// A filtered scan of a table in its file, a row group at a time: the next group, and the chunks
    /// of the last one that are still to come.
    filtered: Option<(usize, Vec<usize>, std::collections::VecDeque<DataChunk>)>,
    /// `ScanSamplingInfo`: the scan's random engine, and the phase of a sample of rows
    sampling: Option<(super::random_engine::RandomEngine, f64)>,
    /// the positions of the VARIANT fields the cursor does not read (`variant_field`), and those of the
    /// row group of the last chunk
    fields: Vec<usize>,
    field_group: Option<(usize, Vec<(usize, Vector)>)>,
    /// whether the chunks of the cursor are those of the file's row groups, in order
    group_aligned: bool,
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
    /// what no row of a row group the scan skips meets: `filter`, and the filters on several columns above it
    pub prune: Option<BoundExpr>,
    /// the rows to read, in order, instead of all of them
    pub index_rows: Option<Arc<Vec<usize>>>,
    /// a SYSTEM sample of the vectors or rows the scan reads
    pub sample: Option<crate::planner::logical::TableSample>,
    /// per position of `column_ids`, the field of the column it outputs in its place, if any
    pub extracts: Vec<Option<Arc<crate::planner::logical::PushedExtract>>>,
    /// `TableScanState::rows_scanned`: the rows of the vectors it read, before its filter
    pub rows_read: std::sync::atomic::AtomicU64,
    /// `prune` with each STRUCT field it reads (`struct_extract` of a column) as a position past `column_ids`, and
    /// the column and field of each of those positions, worked out once
    #[allow(
        clippy::type_complexity,
        reason = "computed once: the filter and the extracts it reads"
    )]
    pub vector_prune: std::sync::OnceLock<Option<(BoundExpr, Vec<(usize, crate::planner::logical::PushedExtract)>)>>,
}

impl PhysicalTableScan {
    /// `RowGroup::CheckZonemapSegments`: whether no row of the vector of `count` rows from `first_row` can pass the
    /// filters of the scan, by the row ids it holds and the statistics of its columns' segments.
    fn vector_skips(&self, ids: &RowIds, first_row: usize, count: usize) -> bool {
        let Some(prune) = &self.prune else {
            return false;
        };
        if count == 0 {
            return false;
        }
        // a join filter on the row ids: the vector's row ids are known, so it is read only if one of them passes
        let conjuncts: Vec<&BoundExpr> = match self.filter.as_ref().map(|f| &f.kind) {
            Some(BoundExprKind::Conjunction { and: true, children }) => children.iter().collect(),
            Some(_) => self.filter.iter().collect(),
            None => Vec::new(),
        };
        for conjunct in conjuncts {
            if let BoundExprKind::JoinFilter { child, filter, .. } = &conjunct.kind
                && let BoundExprKind::Reference(position) = child.kind
                && self.column_ids.get(position) == Some(&COLUMN_IDENTIFIER_ROW_ID)
                && filter.excludes_range(
                    &child.return_type,
                    ids.id(first_row) as i64,
                    ids.id(first_row + count - 1) as i64,
                )
            {
                return true;
            }
        }
        let vector = first_row / STANDARD_VECTOR_SIZE;
        let Some((prune, fields)) = self
            .vector_prune
            .get_or_init(|| struct_fields_of(prune, &self.column_ids))
        else {
            return false;
        };
        let zone = |index: usize| -> Option<crate::catalog::ColumnZone> {
            if let Some((column, extract)) = index.checked_sub(self.column_ids.len()).and_then(|at| fields.get(at)) {
                let zones = super::zone_map::vector_zones_of(&self.table, *column, Some(extract));
                return zones.get(vector).filter(|zone| !zone.unknown).cloned();
            }
            match *self.column_ids.get(index)? {
                COLUMN_IDENTIFIER_ROW_ID => Some(crate::catalog::ColumnZone {
                    min: Some(duckrs_common::value::Value::bigint(ids.id(first_row) as i64)),
                    max: Some(duckrs_common::value::Value::bigint(ids.id(first_row + count - 1) as i64)),
                    ..crate::catalog::ColumnZone::default()
                }),
                COLUMN_IDENTIFIER_ROW_NUMBER => None,
                column => {
                    let extract = self.extracts.get(index).and_then(|e| e.as_deref());
                    let zones = super::zone_map::vector_zones_of(&self.table, column, extract);
                    zones.get(vector).filter(|zone| !zone.unknown).cloned()
                }
            }
        };
        super::zone_map::never_true(&zone, prune)
    }

    /// Whether a Top-N above has a boundary that no row of row group `group` sorts before, by the group's zone
    /// maps: the group is not read (`DynamicFilterData::CheckStatistics`).
    fn top_n_skips(&self, group: usize, rows: usize) -> bool {
        let conjuncts: Vec<&BoundExpr> = match self.filter.as_ref().map(|f| &f.kind) {
            Some(BoundExprKind::Conjunction { and: true, children }) => children.iter().collect(),
            Some(_) => self.filter.iter().collect(),
            None => return false,
        };
        conjuncts.into_iter().any(|conjunct| {
            let BoundExprKind::TopNFilter { child, filter } = &conjunct.kind else {
                return false;
            };
            filter.expression_on((**child).clone()).is_some_and(|passes| {
                super::zone_map::group_never_true(&self.table, &self.column_ids, group, rows, &passes)
            })
        })
    }

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
                if i == COLUMN_IDENTIFIER_ROW_NUMBER {
                    return Vector::sequence(LogicalType::BIGINT, first_row as i64 + 1, 1);
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
    fn finish_chunk(&self, result: DataChunk) -> Result<Option<DataChunk>> {
        self.finish_chunk_with(result, &[], None)
    }

    /// The VARIANT columns of a chunk of the columns at `positions` that row group `group` of `source` stores
    /// shredded, marked as such (`SHREDDED_VECTOR`).
    fn mark_shredded(
        &self,
        positions: &[usize],
        mut chunk: DataChunk,
        group: Option<(&dyn TableSource, usize)>,
    ) -> DataChunk {
        let Some((source, group)) = group else {
            return chunk;
        };
        for (at, &position) in positions.iter().enumerate() {
            let column = self.column_ids[position];
            let pushed = self.extracts.get(position).is_some_and(Option::is_some);
            // a VARIANT a row group gives is flat, as `ColumnData::Select` gives it, and shredded if stored so
            if !pushed
                && self
                    .table
                    .columns
                    .get(column)
                    .is_some_and(|c| c.ty.id() == duckrs_common::types::LogicalTypeId::Variant)
            {
                let rows = chunk.len();
                let vector = &mut chunk.columns_mut()[at];
                if vector.vector_type() == duckrs_common::vector::VectorType::Dictionary {
                    vector.flatten(rows);
                }
                if source.variant_group_shredded(group, column) {
                    vector.set_shredded();
                }
            }
        }
        chunk
    }

    /// The VARIANT fields at `positions` of row group `group` of `rows` rows, computed: read from the typed
    /// part, or from the column where some value is not there.
    fn group_fields(
        &self,
        source: &dyn TableSource,
        group: usize,
        positions: &[usize],
        rows: usize,
    ) -> Result<Vec<(usize, Vector)>> {
        let mut fields = Vec::with_capacity(positions.len());
        for &position in positions {
            let extract = self.extracts[position].as_ref().expect("a pushed field");
            let (keys, rest) = extract.variant_path.as_ref().expect("a field by keys");
            let column = self.column_ids[position];
            let (expression, input) = match source.variant_field(group, column, keys)? {
                Some(field) => (rest, field),
                None => (
                    &extract.expression,
                    source.read_group(group, &[column])?.column(0).clone(),
                ),
            };
            let chunk = DataChunk::from_columns(vec![input], rows);
            fields.push((
                position,
                crate::execution::expression_executor::execute(expression, &chunk)?,
            ));
        }
        Ok(fields)
    }

    /// The positions of the scan's outputs that are VARIANT fields by keys, which can be read for themselves.
    fn variant_fields(&self) -> Vec<usize> {
        (0..self.column_ids.len())
            .filter(|&p| {
                self.extracts
                    .get(p)
                    .is_some_and(|e| e.as_ref().is_some_and(|e| e.variant_path.is_some()))
                    && self
                        .projection
                        .as_ref()
                        .is_some_and(|projection| projection.contains(&p))
            })
            .collect()
    }

    /// [`Self::finish_chunk`] of a chunk whose positions `done` have their fields already.
    fn finish_chunk_with(
        &self,
        mut result: DataChunk,
        done: &[usize],
        group: Option<(&dyn TableSource, usize)>,
    ) -> Result<Option<DataChunk>> {
        self.rows_read
            .fetch_add(result.len() as u64, std::sync::atomic::Ordering::Relaxed);
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
        let positions: Vec<usize> = match &self.projection {
            Some(positions) => positions.clone(),
            None => (0..result.column_count()).collect(),
        };
        if self.projection.is_some() {
            let columns = positions.iter().map(|&p| result.column(p).clone()).collect();
            result = DataChunk::from_columns(columns, result.len());
        }
        if let Some((sel, matched)) = selected {
            result.slice(&sel, matched);
        }
        let result = self.extract_fields(&positions, result, done)?;
        Ok(Some(self.mark_shredded(&positions, result, group)))
    }

    /// The fields pushed into the scan, of the columns they are of, in a chunk of the columns at `positions`.
    fn extract_fields(&self, positions: &[usize], result: DataChunk, done: &[usize]) -> Result<DataChunk> {
        if self.extracts.iter().all(Option::is_none) {
            return Ok(result);
        }
        let mut columns = Vec::with_capacity(positions.len());
        for (at, &position) in positions.iter().enumerate() {
            let column = result.column(at).clone();
            columns.push(match self.extracts.get(position) {
                Some(Some(extract)) if !done.contains(&position) => {
                    let input = DataChunk::from_columns(vec![column], result.len());
                    crate::execution::expression_executor::execute(&extract.expression, &input)?
                }
                _ => column,
            });
        }
        Ok(DataChunk::from_columns(columns, result.len()))
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

    /// [`Self::file_columns`], with the positions `skip` read some other way.
    #[allow(clippy::type_complexity, reason = "the three parts of a FileScan")]
    fn file_columns_without(&self, skip: &[usize]) -> (Vec<usize>, Vec<LogicalType>, Vec<Result<usize, Vector>>) {
        let mut used = vec![self.projection.is_none(); self.column_ids.len()];
        for &position in self.projection.iter().flatten() {
            used[position] = !skip.contains(&position);
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
            if column == COLUMN_IDENTIFIER_ROW_ID || column == COLUMN_IDENTIFIER_ROW_NUMBER {
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
        (read, types, columns)
    }

    /// [`PhysicalOperator::parallel_source`] of a table still in its file: a morsel is a row group,
    /// when the groups of the file are those of the table (all full but the last).
    fn parallel_file_source(&self, source: Arc<dyn TableSource>) -> Option<(SharedSourceState, usize)> {
        let group_rows = self.table.row_group_size();
        let groups = source.group_count();
        if groups < MIN_PARALLEL_ROW_GROUPS {
            return None;
        }
        let rows = self.table.row_count();
        // the zone maps are per group of the table, which are those of the file only when they line up
        let aligned = group_rows.is_multiple_of(STANDARD_VECTOR_SIZE) && TableCursor::is_aligned(&*source, group_rows);
        let wanted = if aligned {
            groups_to_scan(&self.table, &self.column_ids, self.prune.as_ref(), rows)
        } else {
            Vec::new()
        };
        let starts = group_starts(&*source);
        let ids = self.table.row_ids(rows);
        // the fields of VARIANTs by keys, which can be read from the typed part
        let fields = self.variant_fields();
        let (read, types, columns) = self.file_columns_without(&fields);
        let scan = FileScan {
            cursor: TableCursor::new(source.clone(), read.clone(), types.clone(), group_rows),
            columns,
        };
        Some((
            Box::new(ParallelFileScan {
                source,
                read,
                types,
                scan,
                ids,
                rows,
                groups: wanted,
                starts,
                fields,
            }),
            groups,
        ))
    }
}

/// The positions in `column_ids` that an expression reads.
fn referenced_positions(expr: &BoundExpr) -> Vec<usize> {
    let mut positions = Vec::new();
    expr.clone().walk_mut(&mut |e| {
        if let BoundExprKind::Reference(position) = e.kind
            && !positions.contains(&position)
        {
            positions.push(position);
        }
    });
    positions
}

impl PhysicalTableScan {
    /// `TableScanState` with table filters, on a row group of a table in its file: the conjuncts of the
    /// filter one by one (those of one column first), each reading its columns only for the rows the ones
    /// before kept (`ColumnData::Select`), then the columns of the output for the rows left, in chunks.
    #[allow(clippy::too_many_arguments, reason = "the parts of a scan of one row group")]
    fn scan_group_filtered(
        &self,
        source: &dyn TableSource,
        group: usize,
        first_row: usize,
        group_rows: usize,
        ids: &RowIds,
        emit: &mut dyn FnMut(DataChunk) -> Result<()>,
    ) -> Result<()> {
        let filter = self.filter.as_ref().expect("a filtered scan has a filter");
        let mut conjuncts: Vec<(&BoundExpr, Vec<usize>)> = match &filter.kind {
            BoundExprKind::Conjunction { and: true, children } => {
                children.iter().map(|c| (c, referenced_positions(c))).collect()
            }
            _ => vec![(filter, referenced_positions(filter))],
        };
        // of those of as many columns, a join filter that has its keys goes first: it hashes an integer
        // and often drops most rows. Then come those of numbers, then those that read strings
        let cost = |conjunct: &BoundExpr, positions: &[usize]| match &conjunct.kind {
            BoundExprKind::JoinFilter { filter, .. } => {
                if filter.is_active() {
                    0
                } else {
                    3
                }
            }
            _ if positions.iter().all(|&position| {
                matches!(
                    self.column_ids[position],
                    COLUMN_IDENTIFIER_ROW_ID | COLUMN_IDENTIFIER_ROW_NUMBER
                ) || self.table.columns[self.column_ids[position]].ty.id()
                    != duckrs_common::types::LogicalTypeId::Varchar
            }) =>
            {
                1
            }
            _ => 2,
        };
        conjuncts.sort_by_key(|(conjunct, positions)| (positions.len(), cost(conjunct, positions)));
        // those of the same columns that follow each other are one pass over their rows, each seeing the rows the
        // ones before it kept (`SelectAnd`), as they would one after another
        let mut merged: Vec<(std::borrow::Cow<'_, BoundExpr>, Vec<usize>)> = Vec::with_capacity(conjuncts.len());
        for (conjunct, positions) in conjuncts {
            match merged.last_mut() {
                Some((last, last_positions)) if *last_positions == positions => {
                    let mut children = match &last.kind {
                        BoundExprKind::Conjunction { and: true, children }
                            if matches!(last, std::borrow::Cow::Owned(_)) =>
                        {
                            children.clone()
                        }
                        _ => vec![last.as_ref().clone()],
                    };
                    children.push(conjunct.clone());
                    *last = std::borrow::Cow::Owned(BoundExpr::new(
                        BoundExprKind::Conjunction { and: true, children },
                        LogicalType::BOOLEAN,
                    ));
                }
                _ => merged.push((std::borrow::Cow::Borrowed(conjunct), positions)),
            }
        }
        let type_of = |position: usize| -> LogicalType {
            match self.column_ids[position] {
                COLUMN_IDENTIFIER_ROW_ID | COLUMN_IDENTIFIER_ROW_NUMBER => LogicalType::BIGINT,
                column => self.table.columns[column].ty.clone(),
            }
        };
        let fetch = |position: usize, selection: &[u32]| -> Result<Vector> {
            match self.column_ids[position] {
                COLUMN_IDENTIFIER_ROW_ID => {
                    let mut ids = ids.vector(first_row, group_rows)?;
                    ids.slice(&SelectionVector::from_vec(selection.to_vec()), selection.len());
                    Ok(ids)
                }
                COLUMN_IDENTIFIER_ROW_NUMBER => {
                    let mut numbers = Vector::sequence(LogicalType::BIGINT, first_row as i64 + 1, 1)?;
                    numbers.slice(&SelectionVector::from_vec(selection.to_vec()), selection.len());
                    Ok(numbers)
                }
                column => source.fetch_rows(group, column, selection),
            }
        };
        // the vectors whose row ids the filters rule out are not read
        let mut rows: Vec<u32> = Vec::with_capacity(group_rows);
        for start in (0..group_rows).step_by(STANDARD_VECTOR_SIZE) {
            let count = STANDARD_VECTOR_SIZE.min(group_rows - start);
            if !self.vector_skips(ids, first_row + start, count) {
                rows.extend(start as u32..(start + count) as u32);
            }
        }
        self.rows_read
            .fetch_add(rows.len() as u64, std::sync::atomic::Ordering::Relaxed);
        if rows.is_empty() {
            return Ok(());
        }
        let mut selection: Arc<Vec<u32>> = Arc::new(rows);
        let mut fetched: Vec<Option<Vector>> = vec![None; self.column_ids.len()];
        // what stands in a chunk for a column no filter has read yet
        let unread: Vec<Vector> = (0..self.column_ids.len())
            .map(|position| Vector::constant_null(type_of(position)))
            .collect();
        for (conjunct, positions) in &merged {
            for &position in positions {
                if fetched[position].is_none() {
                    fetched[position] = Some(fetch(position, &selection)?);
                }
            }
            // comparisons of a column of integers with constants: the values it has are a range, which a loop over
            // them keeps (C++'s `ConstantFilter` in the scan)
            let range = match positions.as_slice() {
                [position] => constant_range(conjunct, *position),
                _ => None,
            };
            if let (Some(range), [position]) = (range, positions.as_slice()) {
                let vector = fetched[*position].as_ref().expect("fetched above");
                if let Some(kept) = select_range(vector, selection.len(), range) {
                    if kept.len() < selection.len() {
                        let kept = Arc::new(kept);
                        let keep = SelectionVector::Explicit(Arc::clone(&kept));
                        for vector in fetched.iter_mut().flatten() {
                            vector.slice(&keep, kept.len());
                        }
                        selection = narrowed(selection, kept, group_rows);
                    }
                    if selection.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
            }
            // a join filter of a column: one pass over the rows it has, without making chunks of them
            if let (
                BoundExprKind::JoinFilter {
                    child,
                    filter,
                    keep_null,
                },
                [position],
            ) = (&conjunct.kind, positions.as_slice())
                && matches!(child.kind, BoundExprKind::Reference(p) if p == *position)
            {
                let vector = fetched[*position].as_ref().expect("fetched above");
                if let Some(kept) = filter.select(vector, selection.len(), *keep_null)
                    && kept.len() < selection.len()
                {
                    let kept = Arc::new(kept);
                    let keep = SelectionVector::Explicit(Arc::clone(&kept));
                    for vector in fetched.iter_mut().flatten() {
                        vector.slice(&keep, kept.len());
                    }
                    selection = narrowed(selection, kept, group_rows);
                }
                if selection.is_empty() {
                    return Ok(());
                }
                continue;
            }
            let mut kept: Vec<u32> = Vec::with_capacity(selection.len());
            for start in (0..selection.len()).step_by(STANDARD_VECTOR_SIZE) {
                let count = STANDARD_VECTOR_SIZE.min(selection.len() - start);
                // the columns the conjunct reads, and in place of the others what stands for them
                let columns = (0..self.column_ids.len())
                    .map(|position| match &fetched[position] {
                        Some(vector) if positions.contains(&position) => vector.range(start, count),
                        _ => unread[position].clone(),
                    })
                    .collect();
                let chunk = DataChunk::from_columns(columns, count);
                let (sel, matched) = select(conjunct, &chunk)?;
                kept.extend((0..matched).map(|i| (start + sel.get(i)) as u32));
            }
            if kept.len() < selection.len() {
                let kept = Arc::new(kept);
                let keep = SelectionVector::Explicit(Arc::clone(&kept));
                for vector in fetched.iter_mut().flatten() {
                    vector.slice(&keep, kept.len());
                }
                selection = narrowed(selection, kept, group_rows);
            }
            if selection.is_empty() {
                return Ok(());
            }
        }
        let output: Vec<usize> = match &self.projection {
            Some(positions) => positions.clone(),
            None => (0..self.column_ids.len()).collect(),
        };
        // the VARIANT fields of the rows left, of the row group's
        let fields: Vec<usize> = self
            .variant_fields()
            .into_iter()
            .filter(|&p| fetched[p].is_none())
            .collect();
        for (position, mut field) in self.group_fields(source, group, &fields, group_rows)? {
            field.slice(&SelectionVector::from_vec(selection.to_vec()), selection.len());
            fetched[position] = Some(field);
        }
        for &position in &output {
            if fetched[position].is_none() {
                fetched[position] = Some(fetch(position, &selection)?);
            }
        }
        for start in (0..selection.len()).step_by(STANDARD_VECTOR_SIZE) {
            let count = STANDARD_VECTOR_SIZE.min(selection.len() - start);
            let columns = output
                .iter()
                .map(|&position| fetched[position].as_ref().expect("fetched above").range(start, count))
                .collect();
            let chunk = self.extract_fields(&output, DataChunk::from_columns(columns, count), &fields)?;
            emit(self.mark_shredded(&output, chunk, Some((source, group))))?;
        }
        Ok(())
    }
}

/// The rows of a group that `kept` (positions in `selection`) are: `kept` itself while `selection` is all
/// `group_rows` of the group in order.
fn narrowed(selection: Arc<Vec<u32>>, kept: Arc<Vec<u32>>, group_rows: usize) -> Arc<Vec<u32>> {
    if selection.len() == group_rows {
        return kept;
    }
    Arc::new(kept.iter().map(|&k| selection[k as usize]).collect())
}

/// The values of the column at `position` that `conjunct` keeps, as an inclusive range, if it is only comparisons
/// of the column, of a type held as an integer, with constants that are not NULL (`<`, `<=`, `>`, `>=`, `=` and
/// BETWEEN, joined by AND). `None` of anything else, which is evaluated as it is.
fn constant_range(conjunct: &BoundExpr, position: usize) -> Option<(i64, i64)> {
    fn integer(value: &Value) -> Option<i64> {
        match value.data() {
            ValueData::Int8(v) => Some(i64::from(*v)),
            ValueData::Int16(v) => Some(i64::from(*v)),
            ValueData::Int32(v) => Some(i64::from(*v)),
            ValueData::Int64(v) => Some(*v),
            _ => None,
        }
    }
    fn column_of(expr: &BoundExpr, position: usize) -> bool {
        matches!(expr.kind, BoundExprKind::Reference(p) if p == position)
            && matches!(
                expr.return_type.physical_type(),
                duckrs_common::types::PhysicalType::Int8
                    | duckrs_common::types::PhysicalType::Int16
                    | duckrs_common::types::PhysicalType::Int32
                    | duckrs_common::types::PhysicalType::Int64
            )
    }
    fn constant_of(expr: &BoundExpr, column: &BoundExpr) -> Option<i64> {
        match &expr.kind {
            BoundExprKind::Constant(value) if expr.return_type == column.return_type => integer(value),
            _ => None,
        }
    }
    fn narrow(expr: &BoundExpr, position: usize, range: &mut (i64, i64)) -> Option<()> {
        use duckrs_parser::ast::ComparisonOp as Op;
        if let BoundExprKind::Conjunction { and: true, children } = &expr.kind {
            // each child narrows the same range
            return children.iter().try_for_each(|child| narrow(child, position, range));
        }
        let (lower, upper) = (&mut range.0, &mut range.1);
        match &expr.kind {
            BoundExprKind::Between {
                input,
                lower: low,
                upper: high,
            } if column_of(input, position) => {
                *lower = (*lower).max(constant_of(low, input)?);
                *upper = (*upper).min(constant_of(high, input)?);
                Some(())
            }
            BoundExprKind::Comparison { op, left, right } => {
                let (op, value) = if column_of(left, position) {
                    (*op, constant_of(right, left)?)
                } else if column_of(right, position) {
                    let flipped = match op {
                        Op::LessThan => Op::GreaterThan,
                        Op::GreaterThan => Op::LessThan,
                        Op::LessThanOrEqual => Op::GreaterThanOrEqual,
                        Op::GreaterThanOrEqual => Op::LessThanOrEqual,
                        other => *other,
                    };
                    (flipped, constant_of(left, right)?)
                } else {
                    return None;
                };
                match op {
                    Op::Equal => {
                        *lower = (*lower).max(value);
                        *upper = (*upper).min(value);
                    }
                    Op::LessThan => *upper = (*upper).min(value.checked_sub(1)?),
                    Op::LessThanOrEqual => *upper = (*upper).min(value),
                    Op::GreaterThan => *lower = (*lower).max(value.checked_add(1)?),
                    Op::GreaterThanOrEqual => *lower = (*lower).max(value),
                    _ => return None,
                }
                Some(())
            }
            _ => None,
        }
    }
    let mut range = (i64::MIN, i64::MAX);
    narrow(conjunct, position, &mut range)?;
    Some(range)
}

/// The rows of the first `count` of `vector` whose values are in `lower..=upper`, if it holds integers.
fn select_range(vector: &Vector, count: usize, (lower, upper): (i64, i64)) -> Option<Vec<u32>> {
    let unified = vector.to_unified(count);
    let mut kept = Vec::with_capacity(count);
    let flat = unified.sel.is_identity() && unified.validity.all_valid();
    macro_rules! keep {
        ($t:ty) => {{
            let data = unified.data::<$t>();
            if flat {
                // every row is written, and the count moves past the ones in the range: no branch per row
                kept.resize(count, 0);
                let mut matched = 0;
                for (row, &value) in data[..count].iter().enumerate() {
                    let value = i64::from(value);
                    kept[matched] = row as u32;
                    matched += usize::from(value >= lower && value <= upper);
                }
                kept.truncate(matched);
                return Some(kept);
            }
            for row in 0..count {
                let index = unified.sel.get(row);
                let value = i64::from(data[index]);
                if value >= lower && value <= upper && unified.validity.is_valid(index) {
                    kept.push(row as u32);
                }
            }
        }};
    }
    use duckrs_common::types::PhysicalType;
    match vector.logical_type().physical_type() {
        PhysicalType::Int8 => keep!(i8),
        PhysicalType::Int16 => keep!(i16),
        PhysicalType::Int32 => keep!(i32),
        PhysicalType::Int64 => keep!(i64),
        _ => return None,
    }
    Some(kept)
}

/// What the threads of a [`PhysicalTableScan`] of a table in its file share.
struct ParallelFileScan {
    source: Arc<dyn TableSource>,
    /// the columns of the table that are read, and their types
    read: Vec<usize>,
    types: Vec<LogicalType>,
    /// where the chunks have each column (its cursor is not used)
    scan: FileScan,
    ids: Arc<RowIds>,
    rows: usize,
    groups: Vec<bool>,
    /// the first row of each group of the file
    starts: Vec<usize>,
    /// the positions of fields of VARIANTs that are read for themselves (`variant_field`)
    fields: Vec<usize>,
}

/// The first row of each group of a source: groups of a file written on threads need not be full.
fn group_starts(source: &dyn TableSource) -> Vec<usize> {
    let mut starts = Vec::with_capacity(source.group_count());
    let mut start = 0;
    for group in 0..source.group_count() {
        starts.push(start);
        start += source.group_rows(group);
    }
    starts
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
    fn rows_read(&self) -> Option<u64> {
        Some(self.rows_read.load(std::sync::atomic::Ordering::Relaxed))
    }
    fn parallel_source(&self) -> Option<(SharedSourceState, usize)> {
        self.rows_read.store(0, std::sync::atomic::Ordering::Relaxed);
        if self.index_rows.is_some() || self.sample.is_some() || !self.filter.as_ref().is_none_or(parallel_expression) {
            return None;
        }
        if let Some(source) = self.table.source() {
            return self.parallel_file_source(source);
        }
        let rows = self.table.row_count();
        let chunks = rows.div_ceil(STANDARD_VECTOR_SIZE);
        if chunks < MIN_PARALLEL_ROW_GROUPS * super::zone_map::CHUNKS_PER_ROW_GROUP {
            return None;
        }
        let groups = groups_to_scan(&self.table, &self.column_ids, self.prune.as_ref(), rows);
        let ids = self.table.row_ids(rows);
        Some((Box::new(ParallelTableScan { ids, rows, groups }), chunks))
    }
    fn parallel_chunks(
        &self,
        state: &SharedSourceState,
        index: usize,
        emit: &mut dyn FnMut(DataChunk) -> Result<()>,
    ) -> Result<()> {
        let Some(state) = state.downcast_ref::<ParallelFileScan>() else {
            return match self.parallel_chunk(state, index)? {
                Some(chunk) => emit(chunk),
                None => Ok(()),
            };
        };
        // a row group of the file, in chunks
        if !state.groups.get(index).is_none_or(|g| *g) {
            return Ok(());
        }
        let group_start = state.starts[index];
        if self.filter.is_some() {
            let group_rows = state.source.group_rows(index);
            let rows = group_rows.min(state.rows.saturating_sub(group_start));
            return self.scan_group_filtered(&*state.source, index, group_start, rows, &state.ids, emit);
        }
        let mut group = ColumnDataCollection::new(state.types.clone());
        group.append_shared(&state.source.read_group(index, &state.read)?);
        let fields = self.group_fields(&*state.source, index, &state.fields, group.len())?;
        for (local, chunk) in group.chunks().enumerate() {
            let first_row = group_start + local * STANDARD_VECTOR_SIZE;
            let count = chunk.len().min(state.rows.saturating_sub(first_row));
            if count == 0 {
                break;
            }
            if self.vector_skips(&state.ids, first_row, count) {
                continue;
            }
            let mut columns = self.scan_columns(chunk, &state.ids, first_row, Some(&state.scan))?;
            for (position, field) in &fields {
                columns[*position] = field.range(local * STANDARD_VECTOR_SIZE, chunk.len());
            }
            let mut result = DataChunk::from_columns(columns, chunk.len());
            if count < chunk.len() {
                result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
            }
            if let Some(result) = self.finish_chunk_with(result, &state.fields, Some((&*state.source, index)))? {
                emit(result)?;
            }
        }
        Ok(())
    }
    fn parallel_chunk(&self, state: &SharedSourceState, index: usize) -> Result<Option<DataChunk>> {
        let state: &ParallelTableScan = state.downcast_ref().expect("parallel scan state");
        if !state
            .groups
            .get(index / self.table.chunks_per_row_group())
            .is_none_or(|g| *g)
        {
            return Ok(None);
        }
        let first_row = index * STANDARD_VECTOR_SIZE;
        let storage = self.table.rows()?;
        let chunk = storage.chunk(index);
        let count = chunk.len().min(state.rows.saturating_sub(first_row));
        if self.vector_skips(&state.ids, first_row, count) {
            return Ok(None);
        }
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
        self.rows_read.store(0, std::sync::atomic::Ordering::Relaxed);
        let remaining = self.table.row_count();
        let groups = groups_to_scan(&self.table, &self.column_ids, self.prune.as_ref(), remaining);
        let source = self.table.source();
        // the VARIANT fields are read by row group, where the chunks are those of the file's groups
        let group_rows = self.table.row_group_size();
        let group_aligned = source.as_ref().is_some_and(|source| {
            group_rows.is_multiple_of(STANDARD_VECTOR_SIZE) && TableCursor::is_aligned(&**source, group_rows)
        });
        let fields = if group_aligned && self.filter.is_none() {
            self.variant_fields()
        } else {
            Vec::new()
        };
        let cursor = source.clone().map(|source| {
            let (read, types, columns) = self.file_columns_without(&fields);
            FileScan {
                cursor: TableCursor::new(source, read, types, self.table.row_group_size()),
                columns,
            }
        });
        let ids = self.table.row_ids(remaining);
        // a filter on a table in its file is applied while its row groups are read
        let filtered = source
            .filter(|_| self.filter.is_some() && self.index_rows.is_none())
            .map(|source| (0, group_starts(&*source), std::collections::VecDeque::new()));
        // `TableScanState::Initialize`: a seeded sample seeds the scan's engine, and a sample of rows
        // starts at a random phase of its own engine
        let sampling = self.sample.as_ref().map(|sample| {
            let seed = sample.options.seed.map_or(-1, |seed| seed as i64);
            let engine = super::random_engine::RandomEngine::new(seed);
            let phase = if sample.options.is_percentage {
                0.0
            } else {
                super::random_engine::RandomEngine::new(seed).next_random()
            };
            (engine, phase)
        });
        Box::new(TableScanState {
            ids,
            next: 0,
            remaining,
            groups,
            cursor,
            next_index_row: 0,
            filtered,
            sampling,
            fields,
            field_group: None,
            group_aligned,
        })
    }
    fn dynamic_filters(&self) -> Option<String> {
        dynamic_filters_of(self.filter.as_ref())
    }
    fn planned_filters(&self) -> Vec<String> {
        top_n_filters_of(self.filter.as_ref())
    }
    fn row_groups_scanned(&self) -> Option<(usize, usize)> {
        if self.index_rows.is_some() {
            return None;
        }
        let rows = self.table.row_count();
        let groups = groups_to_scan(&self.table, &self.column_ids, self.prune.as_ref(), rows);
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
            filtered,
            sampling,
            fields,
            field_group,
            group_aligned,
        } = state.downcast_mut().expect("table scan state");
        if let Some(rows) = &self.index_rows {
            return self.next_index_chunk(rows, next_index_row);
        }
        if let Some((next_group, starts, pending)) = filtered {
            let source = self.table.source().expect("a filtered scan reads a file");
            let rows = *remaining;
            let group_rows = self.table.row_group_size();
            let aligned =
                group_rows.is_multiple_of(STANDARD_VECTOR_SIZE) && TableCursor::is_aligned(&*source, group_rows);
            loop {
                if let Some(chunk) = pending.pop_front() {
                    return Ok(Some(chunk));
                }
                if *next_group >= source.group_count() {
                    return Ok(None);
                }
                let group = *next_group;
                *next_group += 1;
                let first_row = starts[group];
                if first_row >= rows || (aligned && !groups.get(group).is_none_or(|g| *g)) {
                    continue;
                }
                if aligned && self.top_n_skips(group, rows) {
                    continue;
                }
                let group_rows = source.group_rows(group).min(rows - first_row);
                self.scan_group_filtered(&*source, group, first_row, group_rows, ids, &mut |chunk| {
                    pending.push_back(chunk);
                    Ok(())
                })?;
            }
        }
        while *remaining > 0 {
            // all chunks but the last are full, so that a chunk tells where its rows are
            let index = *next;
            let first_row = index * STANDARD_VECTOR_SIZE;
            let group = index / self.table.chunks_per_row_group();
            let wanted = groups.get(group).is_none_or(|g| *g) && !self.top_n_skips(group, self.table.row_count());
            *next += 1;
            // `RowGroup::Scan`: a SYSTEM sample skips whole vectors, or keeps rows of one at its rate
            let mut sample_selection = None;
            if let (Some(sample), Some((engine, phase))) = (&self.sample, sampling.as_mut()) {
                let count = STANDARD_VECTOR_SIZE.min(*remaining);
                if sample.options.is_percentage {
                    let rate = sample.options.sample_size.parse::<f64>().unwrap_or(0.0) / 100.0;
                    if engine.next_random() > rate {
                        *remaining -= count;
                        continue;
                    }
                } else if sample.rate <= 0.0 {
                    *remaining -= count;
                    continue;
                } else if sample.rate < 1.0 {
                    // `SystemRowsSelection`: a row is kept where the running count of the rate steps
                    let selected: Vec<u32> = (0..count)
                        .filter(|i| {
                            let row = (first_row + i) as f64;
                            (((row + 1.0) * sample.rate + *phase).floor()) > ((row * sample.rate + *phase).floor())
                        })
                        .map(|i| i as u32)
                        .collect();
                    if selected.is_empty() {
                        *remaining -= count;
                        continue;
                    }
                    sample_selection = Some(selected);
                }
            }
            if !wanted || self.vector_skips(ids, first_row, STANDARD_VECTOR_SIZE.min(*remaining)) {
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
            let mut columns = self.scan_columns(&chunk, ids, first_row, cursor.as_ref())?;
            if !fields.is_empty() {
                let per_group = self.table.chunks_per_row_group();
                let group = index / per_group;
                if field_group.as_ref().is_none_or(|(at, _)| *at != group) {
                    let source = self.table.source().expect("a scan of a file");
                    let rows = source.group_rows(group);
                    *field_group = Some((group, self.group_fields(&*source, group, fields, rows)?));
                }
                let (_, group_fields) = field_group.as_ref().expect("just set");
                let local = (index % per_group) * STANDARD_VECTOR_SIZE;
                for (position, field) in group_fields {
                    columns[*position] = field.range(local, chunk.len());
                }
            }
            let mut result = DataChunk::from_columns(columns, chunk.len());
            if let Some(selected) = sample_selection {
                let kept = selected.len();
                result.slice(&SelectionVector::from_vec(selected), kept);
            } else if count < chunk.len() {
                result.slice(&SelectionVector::from_vec((0..count as u32).collect()), count);
            }
            let source = cursor
                .as_ref()
                .and_then(|_| self.table.source())
                .filter(|_| *group_aligned);
            let group = source
                .as_ref()
                .map(|source| (&**source, index / self.table.chunks_per_row_group()));
            match self.finish_chunk_with(result, fields, group)? {
                Some(result) => return Ok(Some(result)),
                None => continue,
            }
        }
        Ok(None)
    }
}

/// Whether threads can evaluate `expr` at once. What is volatile gives other results in another
/// order, and a correlated subquery runs a plan for each row it gets.
pub(super) fn parallel_expression(expr: &BoundExpr) -> bool {
    use crate::planner::expression::BoundExprKind;
    fn has_subquery(expr: &BoundExpr) -> bool {
        // an uncorrelated subquery runs once and every thread reads what it gave
        let correlated = match &expr.kind {
            BoundExprKind::Subquery {
                subquery, correlated, ..
            } => subquery.volatile || !correlated.is_empty() || !subquery.correlated.is_empty(),
            _ => false,
        };
        correlated || expr.children().into_iter().any(has_subquery)
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
    fn reports_bytes_scanned(&self) -> bool {
        true
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let state = state.downcast_mut().expect("csv scan state");
        let chunk = super::csv::scan_chunk(&self.scan, state);
        let bytes = super::csv::take_bytes_scanned(state);
        if bytes > 0
            && let Some(profiler) = super::profiler::current()
        {
            profiler.add_bytes_scanned(self, bytes);
        }
        chunk
    }
    fn parallel_source(&self) -> Option<(SharedSourceState, usize)> {
        let morsels = super::csv::parallel_morsels(&self.scan)?;
        let count = morsels.len();
        Some((Box::new(morsels), count))
    }
    fn parallel_chunks(
        &self,
        state: &SharedSourceState,
        index: usize,
        emit: &mut dyn FnMut(DataChunk) -> Result<()>,
    ) -> Result<()> {
        let morsels: &super::csv::CsvMorsels = state.downcast_ref().expect("csv morsels");
        let bytes = super::csv::morsel_bytes(&self.scan, morsels, index);
        if bytes > 0
            && let Some(profiler) = super::profiler::current()
        {
            profiler.add_bytes_scanned(self, bytes);
        }
        super::csv::scan_morsel(&self.scan, morsels, index, emit)
    }
}

/// A file being written that logs its writes, and its closing when it goes away
/// (`DUCKDB_LOG_FILE_SYSTEM_WRITE` and `_CLOSE` of `LocalFileSystem`).
#[derive(Debug)]
struct LoggedFile {
    file: std::fs::File,
    path: String,
    position: u64,
    logger: Option<Arc<crate::logging::LogTarget>>,
}

impl std::io::Write for LoggedFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(bytes)?;
        crate::logging::log_file_system_to(
            self.logger.as_ref(),
            &self.path,
            "WRITE",
            Some((written as i64, self.position)),
        );
        self.position += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for LoggedFile {
    fn drop(&mut self) {
        crate::logging::log_file_system_to(self.logger.as_ref(), &self.path, "CLOSE", None);
    }
}

/// The file of a COPY TO, compressed or not.
enum CopyWriter {
    Plain(std::io::BufWriter<LoggedFile>),
    Gzip(Box<flate2::write::GzEncoder<std::io::BufWriter<LoggedFile>>>),
    Zstd(Box<zstd::stream::Encoder<'static, std::io::BufWriter<LoggedFile>>>),
}

impl std::fmt::Debug for CopyWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Plain(_) => "Plain",
            Self::Gzip(_) => "Gzip",
            Self::Zstd(_) => "Zstd",
        })
    }
}

impl std::io::Write for CopyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(writer) => writer.write(bytes),
            Self::Gzip(writer) => writer.write(bytes),
            Self::Zstd(writer) => writer.write(bytes),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(writer) => writer.flush(),
            Self::Gzip(writer) => writer.flush(),
            Self::Zstd(writer) => writer.flush(),
        }
    }
}

/// A file a CSV or JSON COPY writes: the JSON objects in it so far, and its bytes.
#[derive(Debug)]
pub struct CsvOutput {
    writer: CopyWriter,
    json_rows: std::sync::atomic::AtomicU64,
    bytes: usize,
    rows: usize,
}

/// `PhysicalCopyToFile`: writes the rows of its child, and reports how many there were.
#[derive(Debug)]
pub struct PhysicalCopyToFile {
    types: Vec<LogicalType>,
    options: super::csv::CsvWriteOptions,
    children: Vec<Box<dyn PhysicalOperator>>,
    path: String,
    names: Vec<String>,
    /// opened by the first row or at the end, so that planning alone (EXPLAIN) leaves the file be;
    /// by the directory of a partition, or `""` without partitions, with the JSON objects of the file so far
    writers: Mutex<super::parquet::OutputFiles<CsvOutput>>,
    compression: super::file_system::Compression,
    rows: std::sync::atomic::AtomicU64,
    /// the logger of the statement, for the file it writes
    logger: Option<Arc<crate::logging::LogTarget>>,
    buffers: Mutex<super::parquet::PartitionBuffers>,
}

impl PhysicalCopyToFile {
    pub fn new(
        types: Vec<LogicalType>,
        path: String,
        names: Vec<String>,
        options: super::csv::CsvWriteOptions,
        children: Vec<Box<dyn PhysicalOperator>>,
        compression: super::file_system::Compression,
    ) -> Self {
        Self {
            types,
            options,
            children,
            path,
            names,
            writers: Mutex::new(super::parquet::OutputFiles::default()),
            buffers: Mutex::new(super::parquet::PartitionBuffers::default()),
            compression,
            rows: std::sync::atomic::AtomicU64::new(0),
            logger: crate::logging::current_file_logger(),
        }
    }

    /// The columns that go in a file, by their position in the rows.
    fn written_columns(&self) -> Vec<usize> {
        match &self.options.partitioning {
            Some(partitioning) => partitioning.written_columns(self.names.len()),
            None => (0..self.names.len()).collect(),
        }
    }

    fn open(&self, path: &str) -> Result<CopyWriter> {
        let file = std::fs::File::create(path)
            .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
        crate::logging::log_file_system_to(self.logger.as_ref(), path, "OPEN", None);
        let file = std::io::BufWriter::new(LoggedFile {
            file,
            path: path.to_string(),
            position: 0,
            logger: self.logger.clone(),
        });
        let mut opened = match self.compression {
            super::file_system::Compression::Gzip => CopyWriter::Gzip(Box::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::default(),
            ))),
            super::file_system::Compression::Zstd => {
                CopyWriter::Zstd(Box::new(zstd::stream::Encoder::new(file, 0).map_err(|e| {
                    Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}"))
                })?))
            }
            _ => CopyWriter::Plain(file),
        };
        let mut header = Vec::new();
        self.options
            .write_header_of(&mut header, &self.names, &self.written_columns());
        write_all(&mut opened, &header)?;
        Ok(opened)
    }

    /// Writes `rows` of `chunk` to the file of `directory` (`""` without partitions).
    /// Buffered rows of a partition; with ORDER_BY, sorted and in a file of their own.
    fn write_pieces(&self, directory: &str, pieces: Vec<DataChunk>) -> Result<()> {
        let ordered = self.options.partitioning.as_ref().filter(|p| !p.order.is_empty());
        let pieces = match ordered {
            Some(partitioning) => partitioning.sorted(pieces)?,
            None => pieces,
        };
        for piece in &pieces {
            let rows: Vec<usize> = (0..piece.len()).collect();
            self.write_rows(directory, piece, &rows)?;
        }
        if ordered.is_some() {
            let key = super::parquet::OutputFiles::<CsvOutput>::key(directory, self.options.rotation.as_ref());
            let out = self.writers.lock().open.remove(&key);
            if let Some(out) = out {
                self.close(out)?;
            }
        }
        Ok(())
    }

    fn write_rows(&self, directory: &str, chunk: &DataChunk, rows: &[usize]) -> Result<()> {
        let rotation = self.options.rotation.as_ref();
        let key = super::parquet::OutputFiles::<CsvOutput>::key(directory, rotation);
        let mut files = self.writers.lock();
        // `PhysicalCopyRotateNow`, before the rows go to a file
        if let Some(rotation) = rotation
            && files
                .open
                .get(&key)
                .is_some_and(|out| rotation.rotate_now(out.bytes, 0, out.rows > 0))
        {
            let out = files.open.remove(&key).expect("open");
            self.close(out)?;
        }
        if !files.open.contains_key(&key) {
            let path = files.next_path(&self.path, directory, self.options.partitioning.as_ref(), rotation)?;
            let writer = self.open(&path)?;
            let limit = self.options.partitioning.as_ref().map(|p| p.max_open_files);
            let output = CsvOutput {
                writer,
                json_rows: std::sync::atomic::AtomicU64::new(0),
                bytes: 0,
                rows: 0,
            };
            for evicted in files.insert_limited(key.clone(), output, limit) {
                self.close(evicted)?;
            }
        }
        let out_file = files.open.get_mut(&key).expect("just opened");
        let mut out = Vec::new();
        self.options
            .write_rows(&mut out, chunk, rows, &self.written_columns(), &out_file.json_rows)?;
        out_file.rows += rows.len();
        write_all(&mut out_file.writer, &out)?;
        // what the file holds: for gzip, the compressed bytes so far
        out_file.bytes = match &out_file.writer {
            CopyWriter::Plain(writer) => writer.get_ref().position as usize + writer.buffer().len(),
            CopyWriter::Gzip(writer) => writer.get_ref().get_ref().position as usize + writer.get_ref().buffer().len(),
            CopyWriter::Zstd(writer) => writer.get_ref().get_ref().position as usize + writer.get_ref().buffer().len(),
        };
        Ok(())
    }

    /// The suffix of a file, and its end.
    fn close(&self, out: CsvOutput) -> Result<()> {
        let mut writer = out.writer;
        write_all(&mut writer, self.options.suffix.as_bytes())?;
        let finished = match writer {
            CopyWriter::Plain(mut writer) => std::io::Write::flush(&mut writer),
            CopyWriter::Gzip(writer) => writer.finish().and_then(|mut file| std::io::Write::flush(&mut file)),
            CopyWriter::Zstd(writer) => writer.finish().and_then(|mut file| std::io::Write::flush(&mut file)),
        };
        finished.map_err(|e| Error::new(ErrorKind::Io, format!("Could not write file: {e}")))
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
        match &self.options.partitioning {
            Some(partitioning) => {
                for (directory, rows) in partitioning.group_rows(&self.path, &self.names, chunk)? {
                    let ready = self
                        .buffers
                        .lock()
                        .add(&directory, chunk, &rows, partitioning.flush_threshold)?;
                    if let Some(pieces) = ready {
                        self.write_pieces(&directory, pieces)?;
                    }
                }
            }
            None => {
                let rows: Vec<usize> = (0..chunk.len()).collect();
                self.write_rows("", chunk, &rows)?;
            }
        }
        self.rows
            .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let buffered = self.buffers.lock().drain();
        for (directory, pieces) in buffered {
            self.write_pieces(&directory, pieces)?;
        }
        let mut files = self.writers.lock();
        let rotation = self.options.rotation.as_ref();
        if files.created == 0 && !self.options.output.write_empty_file {
            return Ok(());
        }
        if files.created == 0 {
            if self.options.partitioning.is_some() {
                return std::fs::create_dir_all(&self.path)
                    .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot create directory \"{}\": {e}", self.path)));
            }
            let path = files.next_path(&self.path, "", None, rotation)?;
            let writer = self.open(&path)?;
            files.open.insert(
                String::new(),
                CsvOutput {
                    writer,
                    json_rows: std::sync::atomic::AtomicU64::new(0),
                    bytes: 0,
                    rows: 0,
                },
            );
        }
        let mut keys: Vec<String> = files.open.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let out = files.open.remove(&key).expect("listed");
            self.close(out)?;
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
        let done = state.downcast_mut::<bool>().expect("copy state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let files = self.writers.lock();
        self.options
            .output
            .result(self.rows.load(std::sync::atomic::Ordering::Relaxed), &files.paths)
            .map(Some)
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

/// `PhysicalColumnDataScan`: chunks that are there already, as they are.
#[derive(Debug)]
pub struct PhysicalChunkScan {
    pub types: Vec<LogicalType>,
    pub chunks: Arc<Vec<DataChunk>>,
}

impl PhysicalOperator for PhysicalChunkScan {
    fn name(&self) -> &'static str {
        "COLUMN_DATA_SCAN"
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
        let next = state.downcast_mut::<usize>().expect("chunk scan state");
        let chunk = self.chunks.get(*next).cloned();
        *next += 1;
        Ok(chunk)
    }
}

// Ported from the `PhysicalTableScan` of a table function, as duckdb/src/main/capi/v1/table_function-c.cpp's
// `CTableFunctionInit`, `CTableFunctionLocalInit` and `CTableFunction` run in it @ rust-port-baseline

/// `PhysicalTableScan` of a table function the client registered: its init runs when the scan starts, and then
/// its function gives chunks until it gives an empty one.
#[derive(Debug)]
pub struct PhysicalUserTableScan {
    pub types: Vec<LogicalType>,
    pub scan: crate::function::udf::UserTableScan,
}

impl PhysicalOperator for PhysicalUserTableScan {
    fn name(&self) -> &'static str {
        "TABLE_SCAN"
    }
    fn explain_details(&self) -> Vec<String> {
        let mut details = vec![format!("Function: {}", self.scan.name)];
        if let Some((cardinality, _)) = self.scan.cardinality {
            details.push(format!("~{cardinality} rows"));
        }
        details
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        // the init runs with the first chunk, as an error of it is an error of the query
        Box::new(None::<Box<dyn crate::function::udf::TableUdfRun>>)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let run = state
            .downcast_mut::<Option<Box<dyn crate::function::udf::TableUdfRun>>>()
            .expect("table function scan state");
        if run.is_none() {
            let column_ids: Vec<usize> = (0..self.types.len()).collect();
            *run = Some(self.scan.scan.init(&column_ids)?);
        }
        let run = run.as_mut().expect("initialized above");
        let mut chunk = DataChunk::new(&self.types);
        run.next(&mut chunk)?;
        Ok((!chunk.is_empty()).then_some(chunk))
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
    key: CteKey,
    recursive: LogicalOperator,
    /// the rows of the current round
    data: Mutex<ColumnDataCollection>,
    /// what UNION has seen so far
    seen: Mutex<HashSet<Vec<u8>>>,
    /// whether the recursive part ran yet: it runs once even over an anchor of no rows, where an aggregate without
    /// groups still makes one
    recursed: std::sync::atomic::AtomicBool,
}

// The USING KEY recursion below is ported from
// duckdb/src/execution/operator/set/physical_recursive_cte_keyed.cpp and
// physical_recursive_cte_delta.cpp @ rust-port-baseline (`RecursiveCTEKeyedState`), without the
// pre-aggregation, the direct probes of the state and the reuse of candidate rows, which are
// ways of doing the same work faster.

/// The state of a USING KEY recursion (`RecursiveCTEKeyedState::ht`): a group per key, with a
/// state per aggregate, and what the states were finalized to by the last commit.
struct KeyedState {
    groups: HashMap<Vec<u8>, usize>,
    count: usize,
    /// per key column, the value of each group, by its id
    keys: Vec<Vector>,
    /// per aggregate, the state of each group
    states: Vec<Vec<AggregateState>>,
    /// per aggregate, the finalized state of each group as of the last commit
    /// (`RecursiveCTEFinalizedValues`), which `recurring.name` and the result read
    finalized: Vec<Vector>,
}

impl KeyedState {
    fn new(key: &CteKey) -> Self {
        let vector = |ty: &LogicalType| Vector::new(ty.clone(), STANDARD_VECTOR_SIZE);
        Self {
            groups: HashMap::new(),
            count: 0,
            keys: key.keys.iter().map(|&c| vector(&key.internal_types[c])).collect(),
            states: vec![Vec::new(); key.payload.len()],
            finalized: key.payload.iter().map(|(_, a)| vector(&a.return_type)).collect(),
        }
    }
}

impl PhysicalRecursiveCte {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        union_all: bool,
        working: Arc<Mutex<ColumnDataCollection>>,
        recurring: Arc<Mutex<ColumnDataCollection>>,
        key: CteKey,
        recursive: LogicalOperator,
    ) -> Self {
        // the anchor's rows, which are of the types of the rows a USING KEY aggregates
        let input_types = if key.is_empty() {
            types.clone()
        } else {
            key.internal_types.clone()
        };
        let data = Mutex::new(ColumnDataCollection::new(input_types));
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
            recursed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `CommitUsingKeyUpdates`: adds the candidate `rows` to the states of their keys, and
    /// finalizes the states. Returns the groups whose finalized row changed and the first new group
    /// (`FinalizeUsingKeyDelta`).
    fn commit(&self, rows: &ColumnDataCollection, state: &mut KeyedState) -> Result<(Vec<usize>, usize)> {
        let before = state.count;
        // the groups there were before, in the order the rows first touched them
        let mut touched = Vec::new();
        let mut is_touched = vec![false; before];
        for chunk in rows.chunks() {
            let count = chunk.len();
            // the keys group by their collated values
            let columns = self
                .key
                .keys
                .iter()
                .zip(&self.key.key_normalizers)
                .map(|(&c, normalizer)| match normalizer {
                    Some(normalizer) => execute(normalizer, chunk).map(Cow::Owned),
                    None => Ok(Cow::Borrowed(chunk.column(c))),
                })
                .collect::<Result<Vec<Cow<Vector>>>>()?;
            let columns: Vec<&Vector> = columns.iter().map(|c| c.as_ref()).collect();
            let encoded = group_keys(&columns, count)?;
            let first_new = state.count;
            let mut new_rows = Vec::new();
            let mut targets = Vec::with_capacity(count);
            for (row, key) in encoded.into_iter().enumerate() {
                let next = state.count;
                let id = *state.groups.entry(key).or_insert(next);
                if id == next {
                    state.count += 1;
                    new_rows.push(row as u32);
                } else if id < before && !std::mem::replace(&mut is_touched[id], true) {
                    touched.push(id);
                }
                targets.push((row, id));
            }
            if !new_rows.is_empty() {
                let new_count = new_rows.len();
                let sel = SelectionVector::from_vec(new_rows);
                for (values, &column) in state.keys.iter_mut().zip(&self.key.keys) {
                    values.copy_from(chunk.column(column), &sel, new_count, first_new, StringCopy::Deep);
                }
            }
            for ((_, aggregate), states) in self.key.payload.iter().zip(state.states.iter_mut()) {
                states.resize(state.count, aggregate.function.kind.initial_state());
                let input = aggregate.children.first().map(|c| execute(c, chunk)).transpose()?;
                aggregate.function.update(input.as_ref(), count, &targets, states)?;
            }
        }
        let mut finalized = Vec::with_capacity(self.key.payload.len());
        for ((_, aggregate), states) in self.key.payload.iter().zip(&state.states) {
            let mut column = Vector::new(aggregate.return_type.clone(), state.count.max(1));
            let input = aggregate.children.first().map(|c| &c.return_type);
            aggregate
                .function
                .finalize_into(states, &aggregate.return_type, input, &mut column)?;
            finalized.push(column);
        }
        // a group changed when one of its finalized values is distinct from what it was
        let mut changed = vec![false; touched.len()];
        if !touched.is_empty() {
            let sel = SelectionVector::from_vec(touched.iter().map(|&id| id as u32).collect());
            let count = touched.len();
            let compared = |values: &Vector, comparison: &Option<BoundExpr>| -> Result<Vec<Vec<u8>>> {
                let mut values = values.clone();
                values.slice(&sel, count);
                if let Some(comparison) = comparison {
                    values = execute(comparison, &DataChunk::from_columns(vec![values], count))?;
                }
                group_keys(&[&values], count)
            };
            for ((old, new), comparison) in state
                .finalized
                .iter()
                .zip(&finalized)
                .zip(&self.key.payload_comparisons)
            {
                let old_keys = compared(old, comparison)?;
                let new_keys = compared(new, comparison)?;
                for ((changed, old), new) in changed.iter_mut().zip(old_keys).zip(new_keys) {
                    *changed |= old != new;
                }
            }
        }
        state.finalized = finalized;
        let changed = touched
            .into_iter()
            .zip(changed)
            .filter_map(|(id, changed)| changed.then_some(id))
            .collect();
        Ok((changed, before))
    }

    /// The finalized rows of the groups `ids`, all of them for `None` (`AssembleStateRows`).
    fn state_rows(&self, state: &KeyedState, ids: Option<&[usize]>) -> ColumnDataCollection {
        let mut result = ColumnDataCollection::new(self.types.clone());
        let total = ids.map_or(state.count, <[usize]>::len);
        let sources = self
            .key
            .keys
            .iter()
            .zip(&state.keys)
            .chain(self.key.payload.iter().map(|(c, _)| c).zip(&state.finalized));
        let sources: Vec<(usize, &Vector)> = sources.map(|(&c, v)| (c, v)).collect();
        for start in (0..total).step_by(STANDARD_VECTOR_SIZE) {
            let count = (total - start).min(STANDARD_VECTOR_SIZE);
            let sel = ids
                .map(|ids| SelectionVector::from_vec(ids[start..start + count].iter().map(|&id| id as u32).collect()));
            let mut columns: Vec<Option<Vector>> = vec![None; self.types.len()];
            for &(column, source) in &sources {
                let mut vector = Vector::new(self.types[column].clone(), count);
                match &sel {
                    Some(sel) => vector.copy_from(source, sel, count, 0, StringCopy::Reference),
                    None => vector.copy_range_from(source, start, count, 0, StringCopy::Reference),
                }
                columns[column] = Some(vector);
            }
            let columns = columns.into_iter().map(|c| c.expect("a key or an aggregate")).collect();
            result.append(&DataChunk::from_columns(columns, count));
        }
        result
    }

    /// USING KEY: runs all rounds, and leaves the rows of all keys in `data`. Every round commits
    /// its rows to the states; the next one reads them all with UNION ALL, and with UNION the
    /// finalized rows of the keys that are new or whose row changed (`GetUsingKeyDataInternal`).
    fn run_keyed(&self) -> Result<()> {
        let mut state = KeyedState::new(&self.key);
        let mut rows = std::mem::replace(
            &mut *self.data.lock(),
            ColumnDataCollection::new(self.key.internal_types.clone()),
        );
        loop {
            let (changed, first_new) = self.commit(&rows, &mut state)?;
            *self.recurring.lock() = self.state_rows(&state, None);
            if self.key.once {
                // the recursive part does not recurse: its rows join the anchor's, once
                let mut plan = self.recursive.clone();
                plan.clear_subquery_results();
                let rows = run_plan(plan)?;
                self.commit(&rows, &mut state)?;
                break;
            }
            let next = if self.union_all {
                rows
            } else {
                let mut ids = changed;
                ids.extend(first_new..state.count);
                self.state_rows(&state, Some(&ids))
            };
            if next.is_empty() {
                break;
            }
            *self.working.lock() = next;
            let mut plan = self.recursive.clone();
            plan.clear_subquery_results();
            rows = run_plan(plan)?;
        }
        *self.data.lock() = self.state_rows(&state, None);
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
        "REC_CTE"
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
        // each run starts over: a plan run again, or a recursion inside a subquery run per outer row, must not see
        // the recurring rows or the seen rows of the last run
        self.recurring.lock().reset();
        self.seen.lock().clear();
        self.recursed.store(false, std::sync::atomic::Ordering::SeqCst);
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
            let first = !self.recursed.swap(true, std::sync::atomic::Ordering::SeqCst);
            if round.is_empty() && !first {
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

/// `PhysicalCTE`: the rows of its first child go into the CTE's table before its second child
/// runs, whose CTE scans read them; its own rows are those of the second child.
#[derive(Debug)]
pub struct PhysicalMaterializedCte {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub name: String,
    pub table_index: usize,
    /// what the CTE scans read
    pub data: Arc<Mutex<ColumnDataCollection>>,
    /// the rows of the first child until it is done
    definition: Mutex<ColumnDataCollection>,
    /// the rows of the second child
    output: Mutex<ColumnDataCollection>,
}

impl PhysicalMaterializedCte {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        name: String,
        table_index: usize,
        data: Arc<Mutex<ColumnDataCollection>>,
    ) -> Self {
        let definition = Mutex::new(ColumnDataCollection::new(children[0].types().to_vec()));
        let output = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            name,
            table_index,
            data,
            definition,
            output,
        }
    }
}

impl PhysicalOperator for PhysicalMaterializedCte {
    fn name(&self) -> &'static str {
        "CTE"
    }
    fn explain_details(&self) -> Vec<String> {
        vec![
            format!("CTE Name: {}", self.name),
            format!("Table Index: {}", self.table_index),
        ]
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
    fn sink_input(&self, input: usize, chunk: &DataChunk) -> Result<()> {
        let target = if input == 0 { &self.definition } else { &self.output };
        let mut data = target.lock();
        data.append(chunk);
        check_buffered(&data)
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        self.sink_input(1, chunk)
    }
    fn finish_input(&self, input: usize) -> Result<()> {
        if input == 0 {
            let types = self.children[0].types().to_vec();
            *self.data.lock() = std::mem::replace(&mut *self.definition.lock(), ColumnDataCollection::new(types));
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
        Ok(scan_collection(&self.output, state))
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
                    let (selection, selected) =
                        super::expression_executor::as_join_condition(|| select(predicate, &joined))?;
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
    /// what rows match by (their collated columns); empty for the rows themselves
    keys: Vec<BoundExpr>,
}

impl PhysicalExceptIntersect {
    pub fn new(
        types: Vec<LogicalType>,
        children: Vec<Box<dyn PhysicalOperator>>,
        except: bool,
        all: bool,
        keys: Vec<BoundExpr>,
    ) -> Self {
        let data = Mutex::new(ColumnDataCollection::new(types.clone()));
        Self {
            types,
            children,
            except,
            all,
            data,
            counts: Mutex::default(),
            keys,
        }
    }

    /// The match keys of the rows of `chunk`.
    fn row_keys(&self, chunk: &DataChunk) -> Result<Vec<Vec<u8>>> {
        if self.keys.is_empty() {
            let columns: Vec<&Vector> = chunk.columns().iter().collect();
            return group_keys(&columns, chunk.len());
        }
        let vectors = self
            .keys
            .iter()
            .map(|key| super::expression_executor::execute(key, chunk))
            .collect::<Result<Vec<_>>>()?;
        let columns: Vec<&Vector> = vectors.iter().collect();
        group_keys(&columns, chunk.len())
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
        let keys = self.row_keys(chunk)?;
        let mut counts = self.counts.lock();
        for key in keys {
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
            let keys = self.row_keys(&chunk)?;
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

/// `PhysicalSecureView`: the boundary of a secure view, which passes its rows on as they are. Plans show it in
/// place of the operators inside the view.
#[derive(Debug)]
pub struct PhysicalSecureView {
    pub types: Vec<LogicalType>,
    pub view_name: String,
    pub pushed_filters: Vec<String>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
}

impl PhysicalOperator for PhysicalSecureView {
    fn name(&self) -> &'static str {
        "SECURE_VIEW"
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
        Ok(input)
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
    /// a Top-N's boundary (`TopNHeap`'s boundary value): the sort key of the last row it keeps, once
    /// it has as many rows as it needs, so that rows that sort after it are not kept
    boundary: Mutex<Option<Vec<u8>>>,
    /// the filter of the boundary that the scans below apply (`PhysicalTopN::dynamic_filter`)
    pub dynamic: Option<Arc<super::join_filter::TopNFilter>>,
}

/// A Top-N sorts what it has when it has twice what it needs, and at least this.
const TOP_N_MIN_BUFFERED_ROWS: u64 = 1 << 12;

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
            boundary: Mutex::default(),
            dynamic: None,
            types,
            children: vec![child],
        }
    }

    /// The sort key of each row of `chunk`.
    fn sort_keys(&self, chunk: &DataChunk) -> Result<Vec<Vec<u8>>> {
        let columns: Vec<(&Vector, OrderModifiers)> = self
            .orders
            .iter()
            .map(|&(column, m)| (chunk.column(column), m))
            .collect();
        create_sort_keys(&columns, chunk.len())
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
        let Some((limit, offset)) = self.top else {
            let mut data = self.data.lock();
            data.append(chunk);
            if let Some(spill) = &self.spill {
                return spill.lock().sink(&mut data, chunk, &self.orders);
            }
            return check_buffered(&data);
        };
        let keep = limit.saturating_add(offset);
        // rows that sort after the boundary never get in: a tie comes after the rows kept, as the
        // sort keeps ties in order
        let mut boundary = self.boundary.lock();
        let mut rows = std::borrow::Cow::Borrowed(chunk);
        if let Some(bound) = boundary.as_ref() {
            let columns: Vec<(&Vector, OrderModifiers)> = self
                .orders
                .iter()
                .map(|&(column, m)| (chunk.column(column), m))
                .collect();
            let selected = duckrs_common::sort_key::rows_sorting_before(&columns, chunk.len(), bound)?;
            if selected.is_empty() {
                return Ok(());
            }
            if selected.len() < chunk.len() {
                let count = selected.len();
                let mut sliced = chunk.clone();
                sliced.slice(&SelectionVector::from_vec(selected), count);
                rows = std::borrow::Cow::Owned(sliced);
            }
        }
        let mut data = self.data.lock();
        data.append(&rows);
        // what is behind the first rows now never gets back in. The first time it has the rows it keeps it has
        // a boundary, as `TopNHeap` does, for the rows to come and the scans below
        let first_boundary = boundary.is_none() && data.len() as u64 >= keep;
        if keep > 0 && (first_boundary || data.len() as u64 > keep.saturating_mul(2).max(TOP_N_MIN_BUFFERED_ROWS)) {
            *data = rows_of_collection(&sort_collection(&data, &self.orders)?, 0, keep);
            if data.len() as u64 == keep {
                let last = data.chunk(data.chunk_count() - 1);
                let row = last.len() - 1;
                let mut one = last.clone();
                one.slice(&SelectionVector::from_vec(vec![row as u32]), 1);
                *boundary = self.sort_keys(&one)?.pop();
                // `TopNBoundaryValue::UpdateValue`: the scans get its first sort key
                if let (Some(dynamic), Some(&(column, _))) = (&self.dynamic, self.orders.first()) {
                    dynamic.set(one.get_value(column, 0));
                }
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
        // `GetDataInternal`: the percentage is of the rows with those of the offset, if any remain
        let count = if available > 0 { available + self.offset } else { 0 };
        let limit = (((count as f64) * self.percent / 100.0) as u64).min(count);
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
    /// of a SYSTEM sample of rows, the fraction of rows it keeps
    pub rate: f64,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalSample {
    pub fn new(options: SampleOptions, child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            options,
            rate: 1.0,
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
        if self.options.method != SampleMethod::Reservoir {
            // `PhysicalStreamingSample`: with the engine of the seed, a die per chunk (SYSTEM) or row
            // (BERNOULLI), or the rows where the running count of the rate steps (SYSTEM of rows)
            let seed = self.options.seed.map_or(-1, |seed| seed as i64);
            let mut engine = super::random_engine::RandomEngine::new(seed);
            let percentage = size / 100.0;
            let mut offset = 0;
            if self.options.method == SampleMethod::System && !self.options.is_percentage {
                let rate = self.rate.clamp(0.0, 1.0);
                let phase = super::random_engine::RandomEngine::new(seed).next_random();
                for (seen, row) in keep.iter_mut().enumerate() {
                    *row = rate >= 1.0
                        || (rate > 0.0
                            && ((seen + 1) as f64 * rate + phase).floor() > (seen as f64 * rate + phase).floor());
                }
            } else {
                for chunk in data.chunks() {
                    let rows = offset..offset + chunk.len();
                    offset += chunk.len();
                    if self.options.method == SampleMethod::System {
                        let kept = engine.next_random() <= percentage;
                        keep[rows].iter_mut().for_each(|row| *row = kept);
                    } else {
                        for row in &mut keep[rows] {
                            *row = engine.next_random() <= percentage;
                        }
                    }
                }
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
    /// The error of a LIMIT or OFFSET that did not fold, for the first row that reaches it.
    pub failure: Option<Error>,
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
        if let Some(failure) = &self.failure
            && !input.is_empty()
        {
            return Err(failure.clone());
        }
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
    /// DISTINCT ON keeps the first row of each group in this order (`first(x ORDER BY ...)`); without
    /// one, the first row it sees
    pub orders: Vec<(usize, duckrs_common::sort_key::OrderModifiers)>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub seen: Mutex<RowKeySet>,
    pub data: Mutex<ColumnDataCollection>,
}

impl PhysicalDistinct {
    pub fn new(targets: &[BoundExpr], orders: &[BoundOrderByNode], child: Box<dyn PhysicalOperator>) -> Self {
        let types = child.types().to_vec();
        Self {
            targets: targets.iter().map(reference_index).collect(),
            orders: orders.iter().map(|o| (reference_index(&o.expr), o.modifiers)).collect(),
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
        // in an order, the rows are sorted before the first of each group is kept
        if !self.orders.is_empty() {
            let mut data = self.data.lock();
            data.append(chunk);
            return check_buffered(&data);
        }
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
    fn finalize(&self) -> Result<()> {
        if self.orders.is_empty() {
            return Ok(());
        }
        let mut data = self.data.lock();
        let sorted = sort_collection(&data, &self.orders)?;
        let mut kept = ColumnDataCollection::new(self.types.clone());
        let mut seen = self.seen.lock();
        for chunk in sorted.chunks() {
            let columns: Vec<&Vector> = self.targets.iter().map(|&i| chunk.column(i)).collect();
            let keys = create_row_keys(&columns, chunk.len())?;
            let first: Vec<u32> = keys
                .into_iter()
                .enumerate()
                .filter_map(|(i, key)| seen.insert(key).then_some(i as u32))
                .collect();
            if !first.is_empty() {
                let mut rows = chunk.clone();
                let count = first.len();
                rows.slice(&SelectionVector::from_vec(first), count);
                kept.append(&rows);
            }
        }
        *data = kept;
        Ok(())
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
    /// the states of threads that are put together by partition when the aggregate finishes
    locals: Mutex<Vec<AggregateSinkState>>,
    /// whether the state of a thread went into `state` (one that spilled), so that `state` has more
    /// than what pipelines on one thread sank
    merged_locals: AtomicBool,
    output: Mutex<Vec<DataChunk>>,
    /// where rows go past the memory limit, for an aggregate that can spill
    spill: Option<AggregateSpill>,
    /// `PhysicalPerfectHashAggregate`, which C++ plans for groups of small integer ranges; here
    /// only its name, as the hash table does the same work
    pub perfect_hash: bool,
}

/// From how many groups the states of threads are put together by partition, on threads.
const PARTITIONED_MERGE_GROUPS: usize = 10_000;

/// How many groups the table of a thread's state holds before they go to their partitions and the table starts
/// again, so that the table rows look up stays in the caches (`RadixHTConfig::SinkCapacity`); the merge puts
/// together what partitions got.
const SINK_GROUPS: usize = 1 << 16;

/// How many partitions the groups of threads are merged in: some for each thread, so that threads that finish
/// theirs take others, a power of two.
fn merge_partitions() -> usize {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    (threads * 2).next_power_of_two()
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

    /// [`Self::partition`] of a group by its hash in a [`GroupTable`], for an aggregate whose groups are in tables.
    fn partition_of_hash(&self, hash: u64) -> usize {
        let mixed = duckrs_common::hash::murmur64(hash ^ u64::from(self.level).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        (mixed >> 32) as usize % AGGREGATE_PARTITIONS
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
    /// for a state split off another to be merged: the keys of its groups in their order, in
    /// place of `group_ids`, which nothing looks anything up in
    ordered_keys: Vec<RowKey>,
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
    /// the groups and their values, in place of `group_ids` and `group_values`, for an aggregate that has its
    /// groups in tables (`PhysicalAggregate::spills_by_hash`): every state of it has one or no group
    table: Option<GroupTable>,
    /// what the table took when `bytes` last counted it
    table_bytes: usize,
    /// for a thread's state, whether its table goes into `parts` when it has [`SINK_GROUPS`] groups
    may_flush: bool,
    /// by partition of their hash, the groups of tables that went out, with their states
    /// (`PartitionedTupleData` of `RadixPartitionedHashTable`): empty, or one per merge partition
    parts: Vec<PartData>,
    /// how many groups `parts` has, some of them the same group
    part_groups: usize,
}

impl AggregateSinkState {
    fn new(group_types: Vec<LogicalType>, aggregates: &[BoundAggregate]) -> Self {
        // a set without groups has its one group from the start, so it has a row for no input
        let group_count = usize::from(group_types.is_empty());
        Self {
            group_ids: RowKeyMap::default(),
            ordered_keys: Vec::new(),
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
            table: None,
            table_bytes: 0,
            may_flush: false,
            parts: Vec::new(),
            part_groups: 0,
        }
    }

    /// How many groups it has, in its table or by key and in its parts.
    fn groups(&self) -> usize {
        self.group_count + self.part_groups
    }
}

/// The groups of one merge partition that a table of a thread had: their values, hashes and states.
#[derive(Debug)]
struct PartData {
    values: Vec<DataChunk>,
    hashes: Vec<u64>,
    /// per aggregate, the states of the groups, in the order of the values
    states: Vec<Vec<AggregateState>>,
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
            perfect_hash: false,
            grouping_sets: sets,
            state: Mutex::new(state),
            locals: Mutex::default(),
            merged_locals: AtomicBool::new(false),
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
        2 * key + self.state_bytes_per_group()
    }

    /// Of [`Self::bytes_per_group`], what the states of a group take, which a group in a table has as well.
    fn state_bytes_per_group(&self) -> usize {
        // a state that holds a value has it behind a pointer, with its sort key for a minimum
        let apart: usize = self
            .aggregates
            .iter()
            .map(|a| match a.function.kind.initial_state() {
                AggregateState::Extreme(_) | AggregateState::Value(_) => {
                    2 * (std::mem::size_of::<Value>() + std::mem::size_of::<Vec<u8>>())
                }
                _ => 0,
            })
            .sum();
        self.aggregates.len() * (2 * std::mem::size_of::<AggregateState>() + 32) + apart
    }

    /// Roughly what the states hold for each input row, besides the groups: rows that an
    /// aggregate keeps, the values DISTINCT has seen, and the rows ORDER BY holds back.
    fn bytes_per_row(&self) -> usize {
        self.aggregates
            .iter()
            .filter(|a| a.distinct || !a.order_by.is_empty() || !a.function.kind.is_order_independent())
            .count()
            * (std::mem::size_of::<Value>() + 16)
    }

    /// What the aggregates that keep their rows' values take for `rows` rows of `inputs`: a value
    /// each, and the bytes of a string or BLOB (or BIT) past what a value holds inline.
    fn bytes_of(&self, inputs: &[AggregateInput], rows: usize) -> usize {
        let mut bytes = rows * self.bytes_per_row();
        for (aggregate, input) in self.aggregates.iter().zip(inputs) {
            let keeps =
                aggregate.distinct || !aggregate.order_by.is_empty() || !aggregate.function.kind.is_order_independent();
            let Some(vector) = input.input.as_ref().filter(|_| keeps) else {
                continue;
            };
            if vector.logical_type().physical_type() != duckrs_common::types::PhysicalType::Varchar {
                continue;
            }
            let count = |row: usize| vector.str_at(row).map_or(0, <[u8]>::len);
            bytes += match &input.filtered {
                Some(filtered) => filtered.iter().map(|&row| count(row)).sum::<usize>(),
                None => (0..rows).map(count).sum::<usize>(),
            };
        }
        bytes
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
            let mut known_rows = Vec::new();
            let mut spilled: Vec<Vec<u32>> = vec![Vec::new(); AGGREGATE_PARTITIONS];
            if self.spills_by_hash() {
                // the groups are in a table, by whose hash the rows of the others go to their partition
                let groups = DataChunk::from_columns(vectors, chunk.len());
                let hashes = GroupTable::hash(&groups)?;
                let found = match &states[0].table {
                    Some(table) => table.find(&groups, &hashes),
                    None => vec![None; chunk.len()],
                };
                for (row, found) in found.iter().enumerate() {
                    match found {
                        Some(_) => known_rows.push(row as u32),
                        None => spilled[target.partition_of_hash(hashes[row])].push(row as u32),
                    }
                }
            } else {
                let columns: Vec<&Vector> = vectors.iter().collect();
                let keys = create_row_keys(&columns, chunk.len())?;
                for (row, key) in keys.iter().enumerate() {
                    if states[0].group_ids.contains_key(key) {
                        known_rows.push(row as u32);
                    } else {
                        spilled[target.partition(key)].push(row as u32);
                    }
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
            state.bytes += self.bytes_of(&inputs.1, known.len());
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
    ) -> Result<Vec<Option<AggregateSinkState>>> {
        let types = state.group_values.types().to_vec();
        let new_state = || AggregateSinkState::new(types.clone(), &self.aggregates);
        let mut taken: Vec<Option<AggregateSinkState>> = (0..AGGREGATE_PARTITIONS).map(|_| None).collect();
        if !wanted.iter().any(|&w| w) {
            return Ok(taken);
        }
        if state.table.is_some() {
            // by the hash of each group in the table, which spilled rows went to their partition by
            let old = std::mem::replace(state, new_state());
            state.full = old.full;
            let table = old.table.expect("checked above");
            let destinations = AGGREGATE_PARTITIONS + 1;
            let mut ids: Vec<Vec<usize>> = vec![Vec::new(); destinations];
            let destination_of: Vec<usize> = table
                .hashes()
                .iter()
                .map(|&hash| {
                    let partition = target.partition_of_hash(hash);
                    if wanted[partition] {
                        partition
                    } else {
                        AGGREGATE_PARTITIONS
                    }
                })
                .collect();
            for (id, &destination) in destination_of.iter().enumerate() {
                ids[destination].push(id);
            }
            let mut states: Vec<Vec<Vec<AggregateState>>> = (0..destinations)
                .map(|_| self.aggregates.iter().map(|_| Vec::new()).collect())
                .collect();
            for (i, sourced) in old.states.into_iter().enumerate() {
                for (id, source) in sourced.into_iter().enumerate().take(table.len()) {
                    states[destination_of[id]][i].push(source);
                }
            }
            for (destination, (ids, states)) in ids.into_iter().zip(states).enumerate() {
                if ids.is_empty() {
                    continue;
                }
                let into = if destination == AGGREGATE_PARTITIONS {
                    &mut *state
                } else {
                    taken[destination].get_or_insert_with(new_state)
                };
                self.absorb(into, &table, &ids, states)?;
            }
            return Ok(taken);
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
        Ok(taken)
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
        let mut groups = self.take_partitions(state, target, &wanted)?;
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
        // a state that has no group yet finds them in a table, when nothing needs their keys while rows come in
        if state.table.is_none()
            && state.group_count == 0
            && state.group_ids.is_empty()
            && state.ordered_keys.is_empty()
            && self.groups_in_table()
        {
            let types = state.group_values.types().to_vec();
            if GroupTable::supports(&types) {
                state.table = Some(GroupTable::new(types)?);
            }
        }
        if state.may_flush && !state.full && state.table.as_ref().is_some_and(|table| table.len() >= SINK_GROUPS) {
            self.flush_table(state)?;
        }
        let columns: Vec<&Vector> = set.groups.iter().map(|&g| &vectors[g]).collect();
        if let Some(ids) = self.find_dictionary_groups(set, vectors, &columns, count, state)? {
            return Ok(ids);
        }
        if let Some(table) = &mut state.table {
            let groups = DataChunk::from_columns(columns.iter().map(|&column| column.clone()).collect(), count);
            let hashes = GroupTable::hash(&groups)?;
            let (ids, new_rows) = table.find_or_create(&groups, &hashes)?;
            state.group_count = table.len();
            self.add_groups(set, vectors, count, new_rows, state)?;
            return Ok(ids);
        }
        // the keys of the rows in one buffer: a row of a group there already makes no key of its own
        let keys = create_row_key_buffer(&columns, count)?;
        let mut new_rows = Vec::new();
        let mut ids = Vec::with_capacity(count);
        for row in 0..count {
            let key = keys.get(row);
            let id = match state.group_ids.get(&key) {
                Some(&id) => id,
                None => {
                    let id = state.group_count;
                    state.group_ids.insert(RowKey::from_bytes(key.0), id);
                    state.group_count += 1;
                    new_rows.push(row as u32);
                    id
                }
            };
            ids.push(id);
        }
        self.add_groups(set, vectors, count, new_rows, state)?;
        Ok(ids)
    }

    /// Moves the groups of the table of `state` to their partitions in `parts` with their states, and starts
    /// the table again (`AbandonInternal`): their values are read out while the table is still in the caches.
    fn flush_table(&self, state: &mut AggregateSinkState) -> Result<()> {
        let Some(table) = state.table.as_mut().filter(|table| !table.is_empty()) else {
            return Ok(());
        };
        let partitions = merge_partitions();
        if state.parts.is_empty() {
            state.parts = (0..partitions)
                .map(|_| PartData {
                    values: Vec::new(),
                    hashes: Vec::new(),
                    states: self.aggregates.iter().map(|_| Vec::new()).collect(),
                })
                .collect();
        }
        let partition_of: Vec<usize> = table
            .hashes()
            .iter()
            .map(|&hash| (hash >> 32) as usize & (partitions - 1))
            .collect();
        let mut ids: Vec<Vec<usize>> = vec![Vec::new(); partitions];
        for (id, &partition) in partition_of.iter().enumerate() {
            ids[partition].push(id);
        }
        for (part, ids) in state.parts.iter_mut().zip(&ids) {
            for batch in ids.chunks(STANDARD_VECTOR_SIZE) {
                part.values.push(table.gather(batch));
                part.hashes.extend(batch.iter().map(|&id| table.hashes()[id]));
            }
        }
        let states = std::mem::replace(&mut state.states, self.aggregates.iter().map(|_| Vec::new()).collect());
        for (i, states) in states.into_iter().enumerate() {
            for (s, &partition) in states.into_iter().zip(&partition_of) {
                state.parts[partition].states[i].push(s);
            }
        }
        state.part_groups += table.len();
        state.group_count = 0;
        // a new table as large as the old got, whose growth counts from there; what the groups take stays counted
        let mut fresh = GroupTable::new(state.group_values.types().to_vec())?;
        fresh.reserve(table.len());
        *table = fresh;
        state.table_bytes = table.size_in_bytes();
        Ok(())
    }

    /// Whether the groups of a state can be found in a [`GroupTable`] while rows come in: one grouping set, and
    /// aggregates that keep nothing by key apart (DISTINCT, ORDER BY).
    fn groups_in_table(&self) -> bool {
        self.partitioned_merge()
    }

    /// The values of the groups that rows `new_rows` of `vectors` made, and room for their states.
    fn add_groups(
        &self,
        set: &GroupingSet,
        vectors: &[Vector],
        count: usize,
        new_rows: Vec<u32>,
        state: &mut AggregateSinkState,
    ) -> Result<()> {
        if new_rows.is_empty() {
            return Ok(());
        }
        let new_count = new_rows.len();
        if let Some(table) = &state.table {
            // a table holds the values of its groups itself: what it grew by, and the states
            let size = table.size_in_bytes();
            state.bytes += size.saturating_sub(state.table_bytes) + new_count * self.state_bytes_per_group();
            state.table_bytes = size;
            if self.spill.is_none() {
                check_buffered_rows(state.group_values.types(), table.len())?;
            }
        } else {
            let columns = set.groups.iter().map(|&g| vectors[g].clone()).collect();
            let mut values = DataChunk::from_columns(columns, count);
            values.slice(&SelectionVector::from_vec(new_rows), new_count);
            state.bytes += temporary::estimated_bytes(&values) + new_count * self.bytes_per_group();
            state.group_values.append(&values);
            if self.spill.is_none() {
                check_buffered(&state.group_values)?;
            }
        }
        for (states, aggregate) in state.states.iter_mut().zip(&self.aggregates) {
            states.resize(state.group_count, aggregate.function.kind.initial_state());
        }
        Ok(())
    }

    /// [`Self::find_groups`] when every group column is a dictionary vector of a small dictionary
    /// (a string column read from its file): the entries of the dictionaries a row has name its group,
    /// so each combination in the chunk is looked up once, and the rows find theirs in a table.
    fn find_dictionary_groups(
        &self,
        set: &GroupingSet,
        vectors: &[Vector],
        columns: &[&Vector],
        count: usize,
        state: &mut AggregateSinkState,
    ) -> Result<Option<Vec<usize>>> {
        const MAX_COMBINATIONS: usize = 1 << 16;
        let mut dictionaries = Vec::with_capacity(columns.len());
        let mut combinations = 1usize;
        for column in columns {
            let Some((sel, child)) = column.dictionary() else {
                return Ok(None);
            };
            if child.vector_type() != duckrs_common::vector::VectorType::Flat {
                return Ok(None);
            }
            let entries = child.capacity().max(1);
            combinations = match combinations.checked_mul(entries) {
                Some(c) if c <= MAX_COMBINATIONS => c,
                _ => return Ok(None),
            };
            dictionaries.push((sel, entries));
        }
        // the combination of each row, and the first row of each combination
        let mut codes = vec![0u32; count];
        for (sel, entries) in &dictionaries {
            for (row, code) in codes.iter_mut().enumerate() {
                *code = *code * *entries as u32 + sel.get(row) as u32;
            }
        }
        let mut first_row = vec![u32::MAX; combinations];
        let mut representatives = Vec::new();
        for (row, &code) in codes.iter().enumerate() {
            if first_row[code as usize] == u32::MAX {
                first_row[code as usize] = row as u32;
                representatives.push(row as u32);
            }
        }
        // the groups of the combinations, as the rows that have them find theirs
        let mut sliced: Vec<Vector> = columns.iter().map(|c| (*c).clone()).collect();
        let selection = SelectionVector::from_vec(representatives.clone());
        for vector in &mut sliced {
            vector.slice(&selection, representatives.len());
        }
        let mut group_of = vec![0usize; combinations];
        let mut new_rows = Vec::new();
        if let Some(table) = &mut state.table {
            // a state whose groups are in a table finds those of the combinations there too
            let groups = DataChunk::from_columns(sliced, representatives.len());
            let hashes = GroupTable::hash(&groups)?;
            let (ids, new) = table.find_or_create(&groups, &hashes)?;
            state.group_count = table.len();
            for (&row, id) in representatives.iter().zip(ids) {
                group_of[codes[row as usize] as usize] = id;
            }
            new_rows.extend(new.into_iter().map(|index| representatives[index as usize]));
        } else {
            let references: Vec<&Vector> = sliced.iter().collect();
            let keys = create_row_keys(&references, representatives.len())?;
            for (&row, key) in representatives.iter().zip(keys) {
                let next = state.group_count;
                let id = *state.group_ids.entry(key).or_insert(next);
                if id == next {
                    state.group_count += 1;
                    new_rows.push(row);
                }
                group_of[codes[row as usize] as usize] = id;
            }
        }
        self.add_groups(set, vectors, count, new_rows, state)?;
        Ok(Some(codes.into_iter().map(|code| group_of[code as usize]).collect()))
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
        let distinct_input = match (&aggregate.distinct_key, aggregate.children.first(), &input) {
            // the shared input of `MultiStageAggregateRewriter`: a volatile argument is evaluated once per row, and
            // the key is computed from its values
            (Some(key), Some(child), Some(values)) if child.is_volatile() => {
                let mut over_values = key.clone();
                if replace_subtree(&mut over_values, child) {
                    let values = DataChunk::from_columns(vec![values.clone()], chunk.len());
                    Some(execute(&over_values, &values)?)
                } else {
                    Some(execute(key, chunk)?)
                }
            }
            (Some(key), ..) => Some(execute(key, chunk)?),
            _ => None,
        };
        let order_vectors = aggregate
            .order_by
            .iter()
            .map(|(expr, _)| execute(expr, chunk))
            .collect::<Result<Vec<_>>>()?;
        let distinct_keys = match (aggregate.distinct, distinct_input.as_ref().or(input.as_ref())) {
            (true, Some(input)) => {
                // NULL is one of the distinct values: the aggregate decides whether it counts
                let mut keys = value_keys(input, chunk.len())?;
                // the ORDER BY keys that are not arguments tell rows apart too
                for &at in &aggregate.distinct_order {
                    for (key, extra) in keys.iter_mut().zip(value_keys(&order_vectors[at], chunk.len())?) {
                        key.extend_from_slice(&(extra.len() as u64).to_le_bytes());
                        key.extend(extra);
                    }
                }
                Some(keys.into_iter().map(Some).collect())
            }
            _ => None,
        };
        let order_keys = if aggregate.order_by.is_empty() {
            None
        } else {
            let columns: Vec<(&Vector, OrderModifiers)> = order_vectors
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
                    aggregate.function.finalize_into(
                        &states[group..group + count],
                        &aggregate.return_type,
                        input,
                        &mut column,
                    )?;
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
        if let Some(table) = &state.table {
            for chunk in table.scan() {
                output.push(row_chunk(chunk.columns(), offset, chunk.len())?);
                offset += chunk.len();
            }
            return Ok(());
        }
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
        } else if self.perfect_hash {
            "PERFECT_HASH_GROUP_BY"
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
                let mut state = AggregateSinkState::new(types, &self.aggregates);
                state.may_flush = true;
                state
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
        let mut local = *local
            .downcast::<Vec<AggregateSinkState>>()
            .expect("aggregate local state");
        // a state that never spilled waits for the others, to be put together by partition
        if self.partitioned_merge() && local.len() == 1 && !local[0].full {
            // its groups go to their partitions here, on its thread
            self.flush_table(&mut local[0])?;
            self.locals.lock().push(local.remove(0));
            return Ok(());
        }
        self.merged_locals.store(true, Ordering::Relaxed);
        let mut states = self.state.lock();
        for (state, local) in states.iter_mut().zip(local) {
            self.merge_groups(state, local)?;
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut states = self.state.lock();
        let mut locals = std::mem::take(&mut *self.locals.lock());
        if !locals.is_empty() {
            // what a pipeline on one thread sank, such as the rows a RIGHT join found no match
            // for after its probe, is put together with the states of threads like one of them
            let serial = !states[0].full && !self.merged_locals.load(Ordering::Relaxed);
            if serial && states[0].group_count > 0 {
                let types = states[0].group_values.types().to_vec();
                let mut state = std::mem::replace(&mut states[0], AggregateSinkState::new(types, &self.aggregates));
                if state.table.is_some() {
                    self.flush_table(&mut state)?;
                }
                locals.push(state);
            }
            let groups: usize = locals.iter().map(AggregateSinkState::groups).sum();
            if serial && locals.len() > 1 && groups >= PARTITIONED_MERGE_GROUPS {
                let output = self.merge_partitioned(locals)?;
                if let Some(spill) = &self.spill {
                    spill.spilled.store(false, Ordering::Relaxed);
                    spill.pending.lock().clear();
                    temporary::current().release(spill.reserved.swap(0, Ordering::Relaxed));
                }
                *self.output.lock() = output;
                return Ok(());
            }
            for local in locals {
                self.merge_groups(&mut states[0], local)?;
            }
        }
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

    /// Whether the states of threads can wait to be put together by partition: one grouping set with
    /// groups, and aggregates that keep nothing apart (DISTINCT, ORDER BY).
    fn partitioned_merge(&self) -> bool {
        self.grouping_sets.len() == 1
            && !self.grouping_sets[0].groups.is_empty()
            && self.aggregates.iter().all(|a| !a.distinct && a.order_by.is_empty())
    }

    /// `RadixPartitionedHashTable::Finalize` on threads: the groups of each thread's state go into
    /// partitions by the hash of their key, then each partition is put together and gives its output
    /// on a thread of its own. The groups come out in partition order.
    fn merge_partitioned(&self, locals: Vec<AggregateSinkState>) -> Result<Vec<DataChunk>> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let partitions = (threads * 2).next_power_of_two();
        if locals.iter().all(|local| local.group_count == 0) {
            // the groups of every thread are in their partitions
            return self.merge_parts(locals);
        }
        // split each thread's state, on threads
        let split: Vec<Vec<AggregateSinkState>> = std::thread::scope(|scope| {
            let handles: Vec<_> = locals
                .into_iter()
                .map(|local| scope.spawn(move || self.split_state(local, partitions)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("an aggregate split thread"))
                .collect::<Result<Vec<_>>>()
        })?;
        // partition p of every thread, together
        let mut by_partition: Vec<Vec<AggregateSinkState>> = (0..partitions).map(|_| Vec::new()).collect();
        for parts in split {
            for (p, part) in parts.into_iter().enumerate() {
                if part.group_count > 0 {
                    by_partition[p].push(part);
                }
            }
        }
        let set = &self.grouping_sets[0];
        let outputs: Vec<Vec<DataChunk>> = std::thread::scope(|scope| {
            let handles: Vec<_> = by_partition
                .into_iter()
                .filter(|parts| !parts.is_empty())
                .map(|parts| {
                    scope.spawn(move || -> Result<Vec<DataChunk>> {
                        let types = parts[0].group_values.types().to_vec();
                        let mut merged = AggregateSinkState::new(types, &self.aggregates);
                        let groups = parts.iter().map(|part| part.group_count).sum();
                        merged.group_ids.reserve(groups);
                        merged.states.iter_mut().for_each(|states| states.reserve_exact(groups));
                        for part in parts {
                            self.merge_groups(&mut merged, part)?;
                        }
                        let mut output = Vec::new();
                        self.set_output(set, &merged, &mut output)?;
                        Ok(output)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("an aggregate merge thread"))
                .collect::<Result<Vec<_>>>()
        })?;
        Ok(outputs.into_iter().flatten().collect())
    }

    /// Whether the groups of this aggregate are in tables, and so go to spill partitions by their hash: they are
    /// for one grouping set, with aggregates that keep nothing by key, of types a table takes.
    fn spills_by_hash(&self) -> bool {
        self.groups_in_table() && {
            let set = &self.grouping_sets[0];
            let types: Vec<LogicalType> = set.groups.iter().map(|&g| self.groups[g].return_type.clone()).collect();
            GroupTable::supports(&types)
        }
    }

    /// Groups `ids` of `source`, whose states per aggregate are `states` in that order, into the table of `into`, each
    /// a group of it or with its state combined with the one of the group it is: the values of each copied once.
    fn absorb(
        &self,
        into: &mut AggregateSinkState,
        source: &GroupTable,
        ids: &[usize],
        states: Vec<Vec<AggregateState>>,
    ) -> Result<()> {
        let values: Vec<DataChunk> = ids
            .chunks(STANDARD_VECTOR_SIZE)
            .map(|part| source.gather(part))
            .collect();
        let hashes: Vec<u64> = ids.iter().map(|&id| source.hashes()[id]).collect();
        self.absorb_values(into, &values, &hashes, states)
    }

    /// [`Self::absorb`] of groups whose values are the rows of `values`, with `hashes` and `states` in that order.
    fn absorb_values(
        &self,
        into: &mut AggregateSinkState,
        values: &[DataChunk],
        hashes: &[u64],
        states: Vec<Vec<AggregateState>>,
    ) -> Result<()> {
        if into.table.is_none() {
            if into.group_count > 0 {
                return Err(Error::new(
                    ErrorKind::Internal,
                    "groups in a table can't join groups by key",
                ));
            }
            into.table = Some(GroupTable::new(into.group_values.types().to_vec())?);
        }
        let table = into.table.as_mut().expect("made above");
        let mut targets = Vec::with_capacity(hashes.len());
        let mut offset = 0;
        for chunk in values {
            let (found, _) = table.find_or_create(chunk, &hashes[offset..offset + chunk.len()])?;
            targets.extend(found);
            offset += chunk.len();
        }
        let before = into.group_count;
        into.group_count = table.len();
        // the groups new here that have a state yet: groups from several tables of one thread can be the same
        let mut placed = vec![false; into.group_count - before];
        for (i, (aggregate, sourced)) in self.aggregates.iter().zip(states).enumerate() {
            let kind = aggregate.function.kind;
            let merged = &mut into.states[i];
            merged.resize(into.group_count, kind.initial_state());
            placed.fill(false);
            for (&target, source) in targets.iter().zip(sourced) {
                let slot = &mut merged[target];
                if target >= before && !std::mem::replace(&mut placed[target - before], true) {
                    // a group new here has no state yet
                    *slot = source;
                    continue;
                }
                let current = std::mem::replace(slot, kind.initial_state());
                *slot = combine_states(kind, current, source)?;
            }
        }
        Ok(())
    }

    /// [`Self::merge_partitioned`] of states whose groups are in partitions (`RadixPartitionedHashTable::Finalize`):
    /// each partition puts what every thread has of it in a table of its own, and gives its output, on threads that
    /// take partitions until there are none.
    fn merge_parts(&self, locals: Vec<AggregateSinkState>) -> Result<Vec<DataChunk>> {
        let set = &self.grouping_sets[0];
        let types = locals[0].group_values.types().to_vec();
        let partitions = locals.iter().map(|local| local.parts.len()).max().unwrap_or(0);
        let mut by_partition: Vec<Vec<PartData>> = (0..partitions).map(|_| Vec::new()).collect();
        for local in locals {
            for (p, part) in local.parts.into_iter().enumerate() {
                if !part.hashes.is_empty() {
                    by_partition[p].push(part);
                }
            }
        }
        let work: Vec<Mutex<Vec<PartData>>> = by_partition.into_iter().map(Mutex::new).collect();
        let outputs: Vec<Mutex<Vec<DataChunk>>> = (0..partitions).map(|_| Mutex::default()).collect();
        let next = AtomicUsize::new(0);
        let merge = |parts: Vec<PartData>| -> Result<Vec<DataChunk>> {
            let mut merged = AggregateSinkState::new(types.clone(), &self.aggregates);
            // room for the most groups the partition can have, up to a few times its largest part
            let total: usize = parts.iter().map(|part| part.hashes.len()).sum();
            let largest = parts.iter().map(|part| part.hashes.len()).max().unwrap_or(0);
            let mut table = GroupTable::new(types.clone())?;
            table.reserve(total.min(largest * 4));
            merged.table = Some(table);
            for part in parts {
                self.absorb_values(&mut merged, &part.values, &part.hashes, part.states)?;
            }
            let mut output = Vec::new();
            self.set_output(set, &merged, &mut output)?;
            Ok(output)
        };
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(partitions);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| -> Result<()> {
                        loop {
                            let p = next.fetch_add(1, Ordering::Relaxed);
                            if p >= partitions {
                                return Ok(());
                            }
                            let parts = std::mem::take(&mut *work[p].lock());
                            if !parts.is_empty() {
                                *outputs[p].lock() = merge(parts)?;
                            }
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .try_for_each(|h| h.join().expect("an aggregate merge thread"))
        })?;
        Ok(outputs.into_iter().flat_map(Mutex::into_inner).collect())
    }

    /// The groups of `state` in `partitions` states, by the hash of their key.
    fn split_state(&self, state: AggregateSinkState, partitions: usize) -> Result<Vec<AggregateSinkState>> {
        use std::hash::{BuildHasher, BuildHasherDefault};
        let AggregateSinkState {
            group_ids,
            group_values,
            group_count,
            states,
            ..
        } = state;
        let types = group_values.types().to_vec();
        let mut parts: Vec<AggregateSinkState> = (0..partitions)
            .map(|_| {
                let mut part = AggregateSinkState::new(types.clone(), &self.aggregates);
                part.states.iter_mut().for_each(Vec::clear);
                part
            })
            .collect();
        let hasher = BuildHasherDefault::<duckrs_common::sort_key::KeyHasher>::default();
        let mut keys: Vec<Option<RowKey>> = vec![None; group_count];
        let mut partition_of = vec![0usize; group_count];
        let mut counts = vec![0usize; partitions];
        for (key, id) in group_ids {
            partition_of[id] = (hasher.hash_one(&key) >> 32) as usize & (partitions - 1);
            counts[partition_of[id]] += 1;
            keys[id] = Some(key);
        }
        // each part is as large as it gets from the start, so that nothing is copied to grow
        for (part, count) in parts.iter_mut().zip(&counts) {
            part.ordered_keys.reserve_exact(*count);
            part.states.iter_mut().for_each(|states| states.reserve_exact(*count));
        }
        // the values of the groups, chunk by chunk, into their partitions
        let mut first = 0;
        let mut rows: Vec<Vec<u32>> = vec![Vec::new(); partitions];
        for chunk in group_values.chunks() {
            for row in 0..chunk.len() {
                rows[partition_of[first + row]].push(row as u32);
            }
            for (p, rows) in rows.iter_mut().enumerate().filter(|(_, rows)| !rows.is_empty()) {
                let count = rows.len();
                let mut values = chunk.clone();
                values.slice(&SelectionVector::from_vec(std::mem::take(rows)), count);
                parts[p].group_values.append(&values);
            }
            first += chunk.len();
        }
        // the keys and states, in the order of the values
        for (id, key) in keys.into_iter().enumerate() {
            let part = &mut parts[partition_of[id]];
            part.ordered_keys.push(key.expect("a key per group"));
            part.group_count += 1;
        }
        for (i, sourced) in states.into_iter().enumerate() {
            for (id, source) in sourced.into_iter().enumerate() {
                parts[partition_of[id]].states[i].push(source);
            }
        }
        Ok(parts)
    }

    /// `CombineStates`: the groups of a thread go into `state`.
    fn merge_groups(&self, state: &mut AggregateSinkState, mut local: AggregateSinkState) -> Result<()> {
        for part in std::mem::take(&mut local.parts) {
            if !part.hashes.is_empty() {
                self.absorb_values(state, &part.values, &part.hashes, part.states)?;
            }
        }
        // a thread that found no group of a set with groups adds nothing (a set without them has its one group)
        if local.table.is_none() && local.group_count == 0 && !local.group_values.types().is_empty() {
            return Ok(());
        }
        if let Some(table) = local.table.take() {
            let ids: Vec<usize> = (0..table.len()).collect();
            return self.absorb(state, &table, &ids, local.states);
        }
        if state.table.is_some() {
            return Err(Error::new(
                ErrorKind::Internal,
                "groups by key can't join groups in a table",
            ));
        }
        let AggregateSinkState {
            group_ids,
            ordered_keys,
            group_values,
            group_count,
            states,
            ..
        } = local;
        let groups_before = state.group_count;
        // a set without groups has its one group on both sides
        let mut targets = vec![0; group_count];
        if !group_ids.is_empty() || !ordered_keys.is_empty() {
            let mut keys: Box<dyn Iterator<Item = RowKey>> = if ordered_keys.is_empty() {
                let mut keys: Vec<Option<RowKey>> = vec![None; group_count];
                for (key, id) in group_ids {
                    keys[id] = Some(key);
                }
                Box::new(keys.into_iter().map(|key| key.expect("a key per group")))
            } else {
                Box::new(ordered_keys.into_iter())
            };
            let mut first = 0;
            for chunk in group_values.chunks() {
                let mut new_rows = Vec::new();
                for row in 0..chunk.len() {
                    let key = keys.next().expect("a key per group");
                    let next = state.group_count;
                    let id = *state.group_ids.entry(key).or_insert(next);
                    if id == next {
                        state.group_count += 1;
                        new_rows.push(row as u32);
                    }
                    targets[first + row] = id;
                }
                if new_rows.len() == chunk.len() {
                    state.group_values.append(chunk);
                } else if !new_rows.is_empty() {
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
                if targets[group] >= groups_before && !group_values.types().is_empty() {
                    // a group that is new here has no state yet, and no other group of `local` is it
                    *slot = source;
                    continue;
                }
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
            // the rows of the aggregates without a filter are the same: made once
            let all_rows: Vec<(usize, usize)> = ids.iter().copied().enumerate().collect();
            for (i, (aggregate, input)) in self.aggregates.iter().zip(inputs).enumerate() {
                if input.filtered.is_none() && input.distinct_keys.is_none() && input.order_keys.is_none() {
                    aggregate
                        .function
                        .update(input.input.as_ref(), chunk.len(), &all_rows, &mut state.states[i])?;
                    continue;
                }
                let mut rows: Vec<(usize, usize)> = match &input.filtered {
                    Some(filtered) => filtered.iter().map(|&row| (row, ids[row])).collect(),
                    None => all_rows.clone(),
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
        if self.delete_in_file(&positions, count) {
            *self.output.lock() = changed_rows_output(
                &self.types,
                ColumnDataCollection::new(self.table.types()),
                positions.len() as u64,
                false,
            );
            return Ok(());
        }
        let mut storage = self.table.write_storage()?;
        let mut kept = ColumnDataCollection::new(storage.types().to_vec());
        let mut deleted = ColumnDataCollection::new(storage.types().to_vec());
        // with RETURNING, the `rowid` of each deleted row comes after its columns
        let ids = self.table.row_ids(count);
        let mut deleted_ids = Vec::new();
        for (index, chunk) in storage.chunks().enumerate() {
            let first_row = index * STANDARD_VECTOR_SIZE;
            let (gone, stay): (Vec<u32>, Vec<u32>) =
                (0..chunk.len() as u32).partition(|row| positions.contains(&(first_row + *row as usize)));
            if gone.is_empty() {
                kept.append(chunk);
                continue;
            }
            if self.returning {
                deleted_ids.extend(gone.iter().map(|row| ids.id(first_row + *row as usize) as i64));
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
        let deleted = if self.returning {
            let mut with_ids = ColumnDataCollection::new(self.types.clone());
            let mut at = 0;
            for chunk in deleted.chunks() {
                let mut column = Vector::new(LogicalType::BIGINT, chunk.len());
                column.data_mut::<i64>()[..chunk.len()].copy_from_slice(&deleted_ids[at..at + chunk.len()]);
                at += chunk.len();
                let mut columns = chunk.columns().to_vec();
                columns.push(column);
                with_ids.append(&DataChunk::from_columns(columns, chunk.len()));
            }
            with_ids
        } else {
            deleted
        };
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

impl PhysicalDelete {
    /// `DataTable::Delete` of a table in a file: the rows go from its version info, and nothing is read. `false`
    /// where the rows have to be in memory: for the keys and indexes that have them, a foreign key that references
    /// them, or RETURNING.
    fn delete_in_file(&self, positions: &HashSet<usize>, count: usize) -> bool {
        let table = &self.table;
        let Some(source) = table.source() else {
            return false;
        };
        if self.returning || !delete_leaves_in_file(table) {
            return false;
        }
        let mut removed: Vec<usize> = positions.iter().copied().collect();
        removed.sort_unstable();
        let mut layered = crate::storage::layered::LayeredTable::of(&source, table.types());
        layered.delete(&removed);
        table.rows_moved(&removed, count);
        table.keep_in_file(Arc::new(layered));
        *table.unique_keys.lock() = None;
        true
    }
}

/// Whether rows of a table in a file can go without reading them: nothing keeps their keys.
pub(crate) fn delete_leaves_in_file(table: &TableCatalogEntry) -> bool {
    table.indexes.read().is_empty()
        && table.referenced_by.lock().is_empty()
        && !table
            .constraints
            .iter()
            .any(|c| matches!(c, TableConstraint::Unique { .. }))
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

impl PhysicalUpdate {
    /// `ColumnData::Update` of a table in a file: the columns it sets change in the row groups it updates, read
    /// from the file for that, and nothing else is read. `false` where the rows have to be in memory: what a key,
    /// a foreign key, a CHECK or RETURNING needs of them.
    fn update_in_file(&self, updates: &HashMap<usize, &Vec<Value>>) -> Result<bool> {
        use crate::storage::layered::{Layer, LayeredTable};
        let table = &self.table;
        let Some(source) = table.source() else {
            return Ok(false);
        };
        let checks = table
            .constraints
            .iter()
            .any(|c| matches!(c, TableConstraint::Check { .. }));
        if self.returning || checks || !table.referenced_by.lock().is_empty() {
            return Ok(false);
        }
        let mut layered = LayeredTable::of(&source, table.types());
        let mut by_group: Vec<Vec<(usize, &Vec<Value>)>> = vec![Vec::new(); layered.group_count()];
        let locate = layered.locator();
        for (&position, values) in updates {
            let (group, row) = locate(position);
            by_group[group].push((row, values));
        }
        for (at, &column) in self.columns.iter().enumerate() {
            let (mut groups, from) = match &layered.layers()[column] {
                Layer::Base(from) => (vec![None; layered.group_count()], Some(*from)),
                Layer::Memory { groups, from } => ((**groups).clone(), *from),
            };
            for (group, rows) in by_group.iter().enumerate().filter(|(_, rows)| !rows.is_empty()) {
                let mut vector = layered.column(column, group)?;
                vector.flatten(layered.group_rows(group));
                for (row, values) in rows {
                    vector.set_value(*row, &values[at])?;
                }
                groups[group] = Some(vector);
            }
            layered.set_layer(
                column,
                Layer::Memory {
                    groups: Arc::new(groups),
                    from,
                },
            );
        }
        table.keep_in_file(Arc::new(layered));
        Ok(true)
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
        // C++ keeps statistics as rows are written, and an update only widens them: what is known
        // of the rows before has to be there for the updated values to add to
        if !moves_rows {
            crate::optimizer::statistics::warm_statistics(&self.table);
        }
        self.table.claim_rows(updates.keys().copied(), count, !moves_rows)?;
        if !moves_rows && self.update_in_file(&updates)? {
            self.table.rows_updated_in_place(updates.keys().copied());
            self.table.rows_updated(updates.keys().copied(), &self.columns, count);
            let changed = ColumnDataCollection::new(self.table.types());
            *self.output.lock() = changed_rows_output(&self.types, changed, updates.len() as u64, false);
            return Ok(());
        }
        let mut storage = self.table.write_storage()?;
        let mut rewritten = ColumnDataCollection::new(storage.types().to_vec());
        let mut changed = ColumnDataCollection::new(storage.types().to_vec());
        let referenced = !self.table.referenced_by.lock().is_empty();
        let key_update = moves_rows || referenced;
        // what a DELETE and an INSERT take away: the rows as they were
        let mut deleted = ColumnDataCollection::new(storage.types().to_vec());
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
                if moves_rows && referenced {
                    let mut old = chunk.clone();
                    old.slice(&SelectionVector::from_vec(picked.clone()), count);
                    deleted.append(&old);
                }
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
        if !deleted.is_empty() {
            // `VerifyDeleteConstraints` of the DELETE half: a referenced row can't be deleted, even
            // to come back as it was, which C++ calls over-eager and doesn't fix for foreign keys
            constraints::verify_delete(&self.table, &storage, &deleted)?;
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
    /// The morsels of a parallel input, until they are appended in their order.
    pub batches: InsertBatches,
}

/// `PhysicalBatchInsert`: of a parallel input, the morsels that threads sank and that wait for the ones before them
/// (the next to append, and those after it), and who appends them. They are appended in order, each as the `INSERT`
/// of one thread would, so that the rows and what is checked of them are those of one thread; the row groups that
/// they fill are encoded on threads at once (`write_ahead`).
#[derive(Debug, Default)]
pub struct InsertBatches {
    waiting: Mutex<Waiting>,
    appending: Mutex<()>,
}

/// The next morsel to append, and the morsels by their index.
type Waiting = (usize, std::collections::BTreeMap<usize, Vec<DataChunk>>);

/// Morsels that wait for one before them, past which the thread with one more waits for its turn to append.
const WAITING_BATCHES: usize = 8;

/// Whether `op` streams the rows of a CSV scan.
fn reads_csv(mut op: &dyn PhysicalOperator) -> bool {
    while !op.is_source() {
        match op.children().first() {
            Some(child) if !op.is_sink() => op = child.as_ref(),
            _ => return false,
        }
    }
    op.name() == "READ_CSV"
}

impl PhysicalInsert {
    /// `PhysicalInsert::Sink` of one chunk: its rows appended to the table, and with `write_ahead`, the row groups
    /// they fill written to blocks; threads that append at once write them after (`write_claimed`).
    fn append(&self, chunk: &DataChunk, write_ahead: bool) -> Result<()> {
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
        drop(storage);
        // `LocalStorage::Append`: full row groups go to blocks, and out of memory
        if write_ahead {
            self.table.write_ahead_when_full()?;
        }
        check_buffered_rows(&self.table.types(), self.table.rows_in_memory())?;
        *self.inserted.lock() += chunk.len() as u64;
        if self.returning {
            self.output.lock().append(&row);
        }
        Ok(())
    }

    /// The row groups that threads appended in full, each claimed by one thread, encoded on it, and placed in order.
    fn write_claimed(&self) -> Result<()> {
        while self
            .table
            .write_claimed_row_groups(crate::storage::writer::WRITE_BUFFER_ROW_GROUP_COUNT)?
        {}
        Ok(())
    }

    /// The morsels that are next, appended in order; `wait` for another thread that appends them now, or leave
    /// them to it.
    fn append_waiting(&self, wait: bool) -> Result<()> {
        loop {
            let appending = if wait {
                self.batches.appending.lock()
            } else {
                match self.batches.appending.try_lock() {
                    Some(appending) => appending,
                    None => return Ok(()),
                }
            };
            loop {
                let chunks = {
                    let mut waiting = self.batches.waiting.lock();
                    let next = waiting.0;
                    match waiting.1.remove(&next) {
                        Some(chunks) => {
                            waiting.0 += 1;
                            chunks
                        }
                        None => break,
                    }
                };
                for chunk in &chunks {
                    self.append(chunk, false)?;
                }
            }
            drop(appending);
            // the next may have come while this thread appended, and its thread left it to this one
            let waiting = self.batches.waiting.lock();
            if !waiting.1.contains_key(&waiting.0) {
                return Ok(());
            }
        }
    }
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
        self.append(chunk, true)
    }
    fn parallel_sink(&self) -> bool {
        reads_csv(self.children[0].as_ref())
    }
    fn local_sink_state(&self) -> LocalSinkState {
        Box::new(Vec::<DataChunk>::new())
    }
    fn sink_local(&self, local: &mut LocalSinkState, _batch: usize, chunk: &DataChunk) -> Result<()> {
        let local: &mut Vec<DataChunk> = local.downcast_mut().expect("insert local state");
        local.push(chunk.clone());
        Ok(())
    }
    fn batch_done_local(&self, local: &mut LocalSinkState, batch: usize) -> Result<()> {
        let local: &mut Vec<DataChunk> = local.downcast_mut().expect("insert local state");
        let waiting = {
            let mut waiting = self.batches.waiting.lock();
            waiting.1.insert(batch, std::mem::take(local));
            waiting.1.len()
        };
        self.append_waiting(waiting > WAITING_BATCHES)?;
        self.write_claimed()
    }
    fn combine_local(&self, _local: LocalSinkState) -> Result<()> {
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        self.append_waiting(true)?;
        self.write_claimed()?;
        self.table.end_claims();
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
    /// The row groups of `data` written to blocks as they filled (`OptimisticDataWriter`), out of memory, with the
    /// table they were written for, which takes their blocks.
    #[allow(
        clippy::type_complexity,
        reason = "the table, its written row groups and their rows, taken together"
    )]
    pub written: Mutex<
        Option<(
            Arc<TableCatalogEntry>,
            Vec<Arc<crate::storage::writer::WrittenGroup>>,
            usize,
        )>,
    >,
}

impl PhysicalCreateTableAs {
    /// `WriteNewRowGroup`: once `data` has `WRITE_BUFFER_ROW_GROUP_COUNT` full row groups and more, they go to blocks.
    fn write_full_row_groups(&self, data: &mut ColumnDataCollection) -> Result<()> {
        let Some(writer) = self.schema.ahead_writer() else {
            return Ok(());
        };
        let mut written = self.written.lock();
        let (table, groups, rows) = written.get_or_insert_with(|| {
            // the table as it will be, which only says how its rows are written
            let table = TableCatalogEntry::new(
                self.schema.name.clone(),
                self.table_name.clone(),
                self.columns.clone(),
                ColumnDataCollection::new(data.types().to_vec()),
                Vec::new(),
            );
            table
                .row_group_rows
                .store(self.schema.row_group_size(), std::sync::atomic::Ordering::Relaxed);
            (Arc::new(table), Vec::new(), 0)
        });
        let count = crate::storage::writer::WRITE_BUFFER_ROW_GROUP_COUNT * table.row_group_size();
        while data.len() > count {
            let full = crate::storage::appended::collection_between(data, 0, count);
            let new = writer.write(table, &full, 0, *rows as u64, false)?;
            if new.is_empty() {
                return Ok(());
            }
            groups.extend(new.into_iter().map(Arc::new));
            *rows += count;
            *data = crate::storage::appended::collection_between(data, count, data.len());
        }
        Ok(())
    }
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
        self.write_full_row_groups(&mut data)?;
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
        let written = self.written.lock().clone();
        if let Some(table) = table {
            table.from_query.store(true, std::sync::atomic::Ordering::Relaxed);
            // the row groups written so far are its first rows, and the blocks still open its own
            if let Some((scratch, groups, rows)) = written.filter(|(_, groups, _)| !groups.is_empty()) {
                let ids = crate::catalog::RowIds::of_ranges([(0, rows)], rows as u64);
                let parts = groups
                    .into_iter()
                    .map(crate::storage::appended::AppendedPart::Written)
                    .collect();
                let source =
                    crate::storage::appended::AppendedTable::new(parts, ids, table.types(), table.row_group_size());
                table.set_appended(Arc::new(source));
                *table.append_blocks.lock() = std::mem::take(&mut *scratch.append_blocks.lock());
            }
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
        let flushed = self.written.lock().as_ref().map_or(0, |(_, _, rows)| *rows);
        chunk.append_row(&[Value::bigint((self.data.lock().len() + flushed) as i64)])?;
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
    pub type_dependencies: Vec<(String, String)>,
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
        let table = self.schema.create_table(
            &self.table_name,
            self.columns.clone(),
            self.generated.clone(),
            self.constraints.clone(),
            self.on_conflict,
        )?;
        if let Some(table) = table {
            *table.type_dependencies.lock() = self.type_dependencies.clone();
        }
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

/// `Dynamic Filters` of a scan: the join filters among the conjuncts of its filter, once their joins gave them.
pub(crate) fn dynamic_filters_of(filter: Option<&BoundExpr>) -> Option<String> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    let mut visit = |expr: &BoundExpr| {
        // a join's bound of its build keys, once it has one
        if let BoundExprKind::TopNFilter { child, filter } = &expr.kind
            && filter.join_bound
            && let Some(bound) = filter.boundary()
        {
            let op = if filter.comparison == duckrs_parser::ast::ComparisonOp::GreaterThanOrEqual {
                ">="
            } else {
                "<="
            };
            let text = format!("optional: {child} {op} {}", BoundExpr::constant(bound));
            let name = child.to_string();
            match found.iter_mut().find(|(n, _)| *n == name) {
                Some((_, texts)) => texts.push(text),
                None => found.push((name, vec![text])),
            }
            return;
        }
        if let BoundExprKind::JoinFilter {
            child,
            filter,
            keep_null,
        } = &expr.kind
            && let Some(text) = filter.describe(child, *keep_null)
        {
            let name = child.to_string();
            match found.iter_mut().find(|(n, _)| *n == name) {
                Some((_, texts)) => texts.push(text),
                None => found.push((name, vec![text])),
            }
        }
    };
    match filter.map(|f| &f.kind) {
        Some(BoundExprKind::Conjunction { and: true, children }) => children.iter().for_each(&mut visit),
        Some(_) => visit(filter.expect("matched")),
        None => {}
    }
    // `TableFilterSet::PushFilter`: the filters of one column are one AND
    let lines: Vec<String> = found
        .into_iter()
        .map(|(_, texts)| match texts.as_slice() {
            [one] => one.clone(),
            _ => format!("({})", texts.join(" AND ")),
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The Top-N filters among the conjuncts of a scan's filter, as `Filters` shows them.
pub(crate) fn top_n_filters_of(filter: Option<&BoundExpr>) -> Vec<String> {
    let conjuncts: Vec<&BoundExpr> = match filter.map(|f| &f.kind) {
        Some(BoundExprKind::Conjunction { and: true, children }) => children.iter().collect(),
        Some(_) => filter.into_iter().collect(),
        None => Vec::new(),
    };
    conjuncts
        .into_iter()
        .filter(|c| matches!(&c.kind, BoundExprKind::TopNFilter { filter, .. } if !filter.join_bound))
        .map(|c| c.to_string())
        .collect()
}

/// `prune` with each `struct_extract` of a column of the scan, of constant fields, as a position past `column_ids`,
/// and the table column and field of each: the segments of a STRUCT's fields have statistics of their own.
fn struct_fields_of(
    prune: &BoundExpr,
    column_ids: &[usize],
) -> Option<(BoundExpr, Vec<(usize, crate::planner::logical::PushedExtract)>)> {
    // the column of the chain of extracts `expr` is, and the expression of the field on `Reference(0)`
    fn chain(expr: &BoundExpr) -> Option<(usize, BoundExpr, Vec<String>)> {
        let BoundExprKind::Function { function, children, .. } = &expr.kind else {
            return None;
        };
        if function.name != "struct_extract"
            || children.len() != 2
            || !matches!(children[1].kind, BoundExprKind::Constant(_))
        {
            return None;
        }
        let key = match &children[1].kind {
            BoundExprKind::Constant(value) => value.try_to_string().ok()?,
            _ => unreachable!("checked"),
        };
        let (position, base, mut names) = match &children[0].kind {
            BoundExprKind::Reference(position) => (
                *position,
                BoundExpr::new(BoundExprKind::Reference(0), children[0].return_type.clone()),
                Vec::new(),
            ),
            _ => chain(&children[0])?,
        };
        let mut field = expr.clone();
        if let BoundExprKind::Function { children, .. } = &mut field.kind {
            children[0] = base;
        }
        names.push(key);
        Some((position, field, names))
    }
    fn rewrite(
        expr: &mut BoundExpr,
        column_ids: &[usize],
        fields: &mut Vec<(usize, crate::planner::logical::PushedExtract)>,
    ) {
        if let Some((position, field, names)) = chain(expr)
            && let Some(&column) = column_ids.get(position)
            && column != COLUMN_IDENTIFIER_ROW_ID
            && column != COLUMN_IDENTIFIER_ROW_NUMBER
        {
            let ty = expr.return_type.clone();
            let at = match fields.iter().position(|(c, f)| *c == column && f.names == names) {
                Some(at) => at,
                None => {
                    fields.push((
                        column,
                        crate::planner::logical::PushedExtract {
                            names,
                            expression: field,
                            variant_path: None,
                        },
                    ));
                    fields.len() - 1
                }
            };
            *expr = BoundExpr::new(BoundExprKind::Reference(column_ids.len() + at), ty);
            return;
        }
        for child in expr.children_mut() {
            rewrite(child, column_ids, fields);
        }
    }
    let mut prune = prune.clone();
    let mut fields = Vec::new();
    rewrite(&mut prune, column_ids, &mut fields);
    Some((prune, fields))
}

/// Replaces the subtrees of `expr` equal to `subtree` with the first column of the input; whether there were any.
fn replace_subtree(expr: &mut BoundExpr, subtree: &BoundExpr) -> bool {
    if format!("{expr:?}") == format!("{subtree:?}") {
        *expr = BoundExpr {
            kind: BoundExprKind::Reference(0),
            return_type: subtree.return_type.clone(),
            alias: String::new(),
        };
        return true;
    }
    let mut replaced = false;
    for child in expr.children_mut() {
        replaced |= replace_subtree(child, subtree);
    }
    replaced
}
