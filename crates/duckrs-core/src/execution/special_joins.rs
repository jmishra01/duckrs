// Ported from duckdb/src/execution/operator/join/{physical_asof_join,physical_positional_join}.cpp
// and the MARK handling of physical_hash_join.cpp @ rust-port-baseline
//
//! The joins that are not about pairs of equal keys: ASOF, POSITIONAL and MARK. They keep the
//! right side's rows and work row by row: the morsel-wise versions come with Phase 8.

use super::expression_executor::execute;
use super::join::PhysicalJoin;
use super::physical::{
    OperatorResult, OperatorState, PhysicalEmptyResult, PhysicalOperator, SourceState, check_buffered,
    check_buffered_rows, group_keys,
};
use crate::execution::LocalSinkState;
use crate::planner::expression::BoundExpr;
use crate::planner::logical::JoinCondition;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{RowKeySet, create_row_keys};
use duckrs_common::types::LogicalType;
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{ComparisonOp, JoinType};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

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
            .filter(|(c, _)| !c.null_safe())
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
        let result = super::expression_executor::as_join_condition(|| execute(predicate, &chunk))?;
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
    /// How a pair of rows reads `predicate`.
    pub semantics: MarkSemantics,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub rows: Mutex<Vec<DataChunk>>,
}

/// How `PhysicalMarkJoin` reads its predicate for a pair of rows: TRUE is a match, and where no
/// pair matches, NULL makes the marker NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkSemantics {
    /// a join condition: a UNION compares as the whole value, as the hash join's row matcher does
    JoinCondition,
    /// an SQL comparison: a UNION compares by `UnionComparator`, where a NULL member is NULL
    Sql,
    /// `JoinHashTable::ConstructMarkJoinResult` of an uncorrelated multi-column `=` MARK join: the
    /// pairs match as a join condition, and a left row without a match is refined with
    /// `MarkJoinRowComparison::CompareEquality`, the SQL comparison
    RowEquality,
}

impl PhysicalMarkJoin {
    /// `PhysicalNestedLoopJoin` and `PhysicalHashJoin` of a MARK join: picks how to read `predicate`
    /// the way the C++ plan's operator would.
    pub fn new(
        types: Vec<LogicalType>,
        predicate: Option<BoundExpr>,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let (predicate, semantics) = match predicate {
            Some(predicate) => {
                let (predicate, semantics) = mark_semantics(predicate);
                (Some(predicate), semantics)
            }
            None => (None, MarkSemantics::JoinCondition),
        };
        Self {
            types,
            predicate,
            semantics,
            children,
            rows: Mutex::default(),
        }
    }

    /// Whether `predicate` finds a right row for `row`: `None` for NULL.
    fn mark(&self, input: &DataChunk, row: usize, right: &[DataChunk]) -> Result<Option<bool>> {
        let Some(predicate) = &self.predicate else {
            return Ok(Some(right.iter().any(|c| !c.is_empty())));
        };
        let evaluate = |pairs: &DataChunk, sql: bool| -> Result<Vector> {
            if sql {
                execute(predicate, pairs)
            } else {
                super::expression_executor::as_join_condition(|| execute(predicate, pairs))
            }
        };
        let mut has_null = false;
        let mut refine = Vec::new();
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
            let result = evaluate(&pairs, self.semantics == MarkSemantics::Sql)?;
            let (found, null) = mark_pairs(&result, count);
            if found {
                return Ok(Some(true));
            }
            has_null |= null;
            if self.semantics == MarkSemantics::RowEquality {
                refine.push(pairs);
            }
        }
        // `MarkJoinChunkHasUnknown`: the SQL comparison decides whether a pair is UNKNOWN
        if !has_null {
            for pairs in &refine {
                if mark_pairs(&evaluate(pairs, true)?, pairs.len()).1 {
                    has_null = true;
                    break;
                }
            }
        }
        Ok((!has_null).then_some(false))
    }
}

/// Whether a BOOLEAN result of `count` pairs has a TRUE, and whether it has a NULL.
fn mark_pairs(result: &Vector, count: usize) -> (bool, bool) {
    let format = result.to_unified(count);
    let data = format.data::<bool>();
    let mut null = false;
    for i in 0..count {
        if !format.is_valid(i) {
            null = true;
        } else if data[format.sel.get(i)] {
            return (true, null);
        }
    }
    (false, null)
}

/// How the C++ plan's operator compares the rows of a MARK join's predicate.
///
/// `<>` of a row is planned as a nested loop join, whose `MarkJoinNested` compares a TUPLE with
/// `MarkJoinRowComparison::CompareEquality`: unequal if a field is, else UNKNOWN if a field is NULL.
/// That is `a <> x OR b <> y`, by field and into nested rows, as SQL comparisons.
///
/// `=` of several columns is planned as a hash join with `InitializeUncorrelatedMarkJoin`, which
/// refines a left row without a match with the SQL comparison. The two only differ for a UNION,
/// whose NULL member is UNKNOWN in SQL but a value to the row matcher.
fn mark_semantics(predicate: BoundExpr) -> (BoundExpr, MarkSemantics) {
    use crate::planner::expression::BoundExprKind;
    use duckrs_common::types::LogicalTypeId;
    if let BoundExprKind::Comparison {
        op: ComparisonOp::NotEqual,
        left,
        right,
    } = &predicate.kind
        && left.return_type.id() == LogicalTypeId::Tuple
        && let Some(row) = row_not_equal(left, right)
    {
        return (row, MarkSemantics::Sql);
    }
    let mut comparisons = Vec::new();
    and_children(&predicate, &mut comparisons);
    let union_equality = comparisons.len() > 1
        && comparisons.iter().all(|c| {
            matches!(
                &c.kind,
                BoundExprKind::Comparison {
                    op: ComparisonOp::Equal,
                    ..
                }
            )
        })
        && comparisons.iter().any(|c| {
            c.children()
                .iter()
                .any(|operand| operand.return_type.id() == LogicalTypeId::Union)
        });
    let semantics = if union_equality {
        MarkSemantics::RowEquality
    } else {
        MarkSemantics::JoinCondition
    };
    (predicate, semantics)
}

/// The operands of a conjunction of ANDs, flattened.
fn and_children<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &expr.kind {
        crate::planner::expression::BoundExprKind::Conjunction { and: true, children } => {
            for child in children {
                and_children(child, out);
            }
        }
        _ => out.push(expr),
    }
}

/// `CompareRowEqualityInternal` as `<>`: the fields of two rows of the same width compared
/// pairwise, ORed, into fields that are rows themselves.
fn row_not_equal(left: &BoundExpr, right: &BoundExpr) -> Option<BoundExpr> {
    use crate::planner::expression::BoundExprKind;
    use duckrs_common::types::LogicalTypeId;
    if left.return_type.id() != LogicalTypeId::Tuple {
        return Some(BoundExpr::new(
            BoundExprKind::Comparison {
                op: ComparisonOp::NotEqual,
                left: Box::new(left.clone()),
                right: Box::new(right.clone()),
            },
            LogicalType::BOOLEAN,
        ));
    }
    let width = left.return_type.struct_children().len();
    if right.return_type.id() != LogicalTypeId::Tuple
        || right.return_type.struct_children().len() != width
        || width == 0
    {
        return None;
    }
    let children = (0..width)
        .map(|i| {
            let l = crate::function::nested::struct_extract_at_index(left.clone(), i)?;
            let r = crate::function::nested::struct_extract_at_index(right.clone(), i)?;
            row_not_equal(&l, &r)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(BoundExpr::new(
        BoundExprKind::Conjunction { and: false, children },
        LogicalType::BOOLEAN,
    ))
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
            values.push(match self.mark(input, row, &rows)? {
                Some(found) => Value::boolean(found),
                None => Value::null(LogicalType::BOOLEAN),
            });
            output.append_row(&values)?;
        }
        Ok((output, OperatorResult::NeedMoreInput))
    }
}

/// `PhysicalLeftDelimJoin`: gathers the left side, hands the distinct values of its join keys to the
/// right side, and then runs `join` with its rows. The inputs are two pipelines in that order, and
/// the left rows come out as a source, since all of them have to be there before the right side
/// starts.
#[derive(Debug)]
pub struct PhysicalDelimJoin {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub join: PhysicalJoin,
    pub left: Mutex<ColumnDataCollection>,
    /// the left rows threads gathered, by the morsel they came from
    left_batches: Mutex<Vec<(usize, ColumnDataCollection)>>,
    pub delim: Arc<Mutex<ColumnDataCollection>>,
}

/// Where the source is: the left chunk `join` is at, and the state of `join` in that chunk.
struct DelimJoinSource {
    chunk: usize,
    state: OperatorState,
}

impl PhysicalDelimJoin {
    pub fn new(
        types: Vec<LogicalType>,
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        delim: Arc<Mutex<ColumnDataCollection>>,
        error_on_multiple_rows: bool,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        // `join` is fed from here, so it only needs to know the types of its children
        let stub = |child: &dyn PhysicalOperator| -> Box<dyn PhysicalOperator> {
            Box::new(PhysicalEmptyResult {
                types: child.types().to_vec(),
            })
        };
        let stubs = vec![stub(children[0].as_ref()), stub(children[1].as_ref())];
        let mut join = PhysicalJoin::new(types.clone(), join_type, conditions, predicate, stubs);
        join.error_on_multiple_rows = error_on_multiple_rows;
        let left = Mutex::new(ColumnDataCollection::new(children[0].types().to_vec()));
        Self {
            types,
            children,
            join,
            left,
            left_batches: Mutex::default(),
            delim,
        }
    }
}

impl PhysicalOperator for PhysicalDelimJoin {
    fn explain_details(&self) -> Vec<String> {
        vec![format!("Join Type: {}", self.join.join_type.name())]
    }
    fn name(&self) -> &'static str {
        "LEFT_DELIM_JOIN"
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
        unreachable!("the inputs of a delim join go through sink_input")
    }
    fn sink_input(&self, input: usize, chunk: &DataChunk) -> Result<()> {
        if input == 1 {
            return self.join.sink(chunk);
        }
        let mut left = self.left.lock();
        left.append(chunk);
        check_buffered(&left)
    }
    /// `PhysicalLeftDelimJoin::Sink` keeps a collection per thread for the left side, and the right
    /// side is the build of the join, which threads can do.
    fn parallel_sink_input(&self, input: usize) -> bool {
        input == 0 || self.join.parallel_sink()
    }
    fn local_sink_state_input(&self, input: usize) -> LocalSinkState {
        if input == 1 {
            return self.join.local_sink_state();
        }
        Box::new(Vec::<(usize, ColumnDataCollection)>::new())
    }
    fn sink_local_input(
        &self,
        input: usize,
        local: &mut LocalSinkState,
        batch: usize,
        chunk: &DataChunk,
    ) -> Result<()> {
        if input == 1 {
            return self.join.sink_local(local, batch, chunk);
        }
        let batches: &mut Vec<(usize, ColumnDataCollection)> = local.downcast_mut().expect("delim join local state");
        if batches.last().is_none_or(|(last, _)| *last != batch) {
            batches.push((batch, ColumnDataCollection::new(self.children[0].types().to_vec())));
        }
        batches.last_mut().expect("just pushed").1.append(chunk);
        Ok(())
    }
    fn combine_local_input(&self, input: usize, local: LocalSinkState) -> Result<()> {
        if input == 1 {
            return self.join.combine_local(local);
        }
        let batches = *local
            .downcast::<Vec<(usize, ColumnDataCollection)>>()
            .expect("delim join local state");
        self.left_batches.lock().extend(batches);
        Ok(())
    }
    fn finish_input(&self, input: usize) -> Result<()> {
        if input != 0 {
            return Ok(());
        }
        // the rows threads gathered, in the order of their morsels: the order of one thread
        let mut batches = std::mem::take(&mut *self.left_batches.lock());
        let mut left = self.left.lock();
        if !batches.is_empty() {
            batches.sort_by_key(|(batch, _)| *batch);
            for (_, rows) in batches {
                left.combine(rows);
            }
            check_buffered(&left)?;
        }
        drop(left);
        let left = self.left.lock();
        let types: Vec<LogicalType> = self
            .join
            .conditions
            .iter()
            .map(|c| c.left.return_type.clone())
            .collect();
        let mut distinct = ColumnDataCollection::new(types);
        let mut seen = RowKeySet::default();
        for index in 0..left.chunk_count() {
            let chunk = left.chunk(index);
            let columns = self
                .join
                .conditions
                .iter()
                .map(|c| execute(&c.left, chunk))
                .collect::<Result<Vec<_>>>()?;
            let references: Vec<&Vector> = columns.iter().collect();
            let rows: Vec<u32> = create_row_keys(&references, chunk.len())?
                .into_iter()
                .enumerate()
                .filter_map(|(row, key)| seen.insert(key).then_some(row as u32))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let count = rows.len();
            let mut keys = DataChunk::from_columns(columns, chunk.len());
            keys.slice(&SelectionVector::from_vec(rows), count);
            distinct.append(&keys);
        }
        *self.delim.lock() = distinct;
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        self.join.finalize()
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(DelimJoinSource {
            chunk: 0,
            state: self.join.operator_state(),
        })
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let source = state.downcast_mut::<DelimJoinSource>().expect("delim join state");
        let left = self.left.lock();
        while source.chunk < left.chunk_count() {
            let (output, result) = self
                .join
                .execute_stateful(left.chunk(source.chunk), &mut source.state)?;
            if result == OperatorResult::NeedMoreInput {
                source.chunk += 1;
            }
            if !output.is_empty() {
                return Ok(Some(output));
            }
        }
        Ok(None)
    }
}
