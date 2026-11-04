// Ported from duckdb/src/execution/operator/join/{physical_cross_product,physical_hash_join,
// physical_blockwise_nl_join}.cpp and duckdb/src/execution/physical_plan/plan_comparison_join.cpp
// @ rust-port-baseline
//
//! Joins. The right child is the build side: it is gathered first (sink), and then the left
//! child's chunks stream through the join (operator). RIGHT and FULL joins emit the build rows
//! nothing matched once the left child is done (source).
//!
//! Execution is single-threaded and in memory for now. The build side is looked up by the sort
//! key of its join keys, like grouping; `JoinHashTable` over the row format replaces that when
//! performance gets its turn (Phase 13).

use super::expression_executor::{execute, select};
use super::physical::{
    OperatorResult, OperatorState, PhysicalOperator, SourceState, check_buffered, check_buffered_rows, group_keys,
};
use crate::planner::expression::BoundExpr;
use crate::planner::logical::JoinCondition;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::vector::{DataChunk, SelectionVector, StringCopy, Vector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::JoinType;
use parking_lot::Mutex;
use std::collections::HashMap;

/// A chunk without rows, for when an operator has nothing to output this time.
fn empty_chunk() -> DataChunk {
    DataChunk::from_columns(Vec::new(), 0)
}

/// `PhysicalCrossProduct`.
#[derive(Debug)]
pub struct PhysicalCrossProduct {
    pub types: Vec<LogicalType>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub right: Mutex<ColumnDataCollection>,
}

/// `CrossProductExecutor`: walks one chunk row by row against the whole of the other one.
#[derive(Default)]
struct CrossProductState {
    /// the current chunk of the right side, and the next one
    current: Option<usize>,
    next: usize,
    position: usize,
    /// walk the input against the right chunk, not the other way around
    scan_input: bool,
}

impl PhysicalCrossProduct {
    pub fn new(types: Vec<LogicalType>, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        let right = ColumnDataCollection::new(children[1].types().to_vec());
        Self {
            types,
            children,
            right: Mutex::new(right),
        }
    }
}

impl PhysicalOperator for PhysicalCrossProduct {
    fn name(&self) -> &'static str {
        "CROSS_PRODUCT"
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
        let mut right = self.right.lock();
        right.append(chunk);
        check_buffered(&right)
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(CrossProductState::default())
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let state = state.downcast_mut::<CrossProductState>().expect("cross product state");
        let right = self.right.lock();
        // NextValue
        state.position += 1;
        let size = match state.current {
            Some(_) if state.scan_input => input.len(),
            Some(current) => right.chunk(current).len(),
            None => 0,
        };
        if state.position >= size {
            if state.next >= right.chunk_count() {
                *state = CrossProductState::default();
                return Ok((empty_chunk(), OperatorResult::NeedMoreInput));
            }
            state.current = Some(state.next);
            state.next += 1;
            state.position = 0;
            // the larger chunk is the one output whole, for larger output chunks
            state.scan_input = input.len() < right.chunk(state.next - 1).len();
        }
        let right_chunk = right.chunk(state.current.expect("just set"));
        let constants = |chunk: &DataChunk| -> Vec<Vector> {
            (0..chunk.column_count())
                .map(|column| Vector::constant(&chunk.get_value(column, state.position)))
                .collect()
        };
        let (mut columns, right_columns, count) = if state.scan_input {
            (constants(input), right_chunk.columns().to_vec(), right_chunk.len())
        } else {
            (input.columns().to_vec(), constants(right_chunk), input.len())
        };
        columns.extend(right_columns);
        Ok((DataChunk::from_columns(columns, count), OperatorResult::HaveMoreOutput))
    }
}

/// `PhysicalHashJoin` with join keys, and `PhysicalBlockwiseNLJoin` without: every pair of a
/// left row and the build rows with its keys (all of them, without keys) is checked against the
/// predicate.
#[derive(Debug)]
pub struct PhysicalJoin {
    pub types: Vec<LogicalType>,
    pub join_type: JoinType,
    pub conditions: Vec<JoinCondition>,
    pub predicate: Option<BoundExpr>,
    pub children: Vec<Box<dyn PhysicalOperator>>,
    pub build: Mutex<JoinBuild>,
}

/// The build side.
#[derive(Debug)]
pub struct JoinBuild {
    /// the build rows, in one chunk once they are all there
    columns: Vec<Vector>,
    count: usize,
    data: Option<DataChunk>,
    /// key -> index in `rows`; rows with a NULL key that is not null-safe are not in there
    groups: HashMap<Vec<u8>, usize>,
    /// the build rows of each key
    rows: Vec<Vec<u32>>,
    /// whether each build row found a match
    matched: Vec<bool>,
}

/// Where probing the current input chunk is.
struct Probe {
    /// per input row, the index in `JoinBuild::rows` of its key (empty without keys)
    groups: Vec<Option<usize>>,
    row: usize,
    /// the next candidate of `row`
    candidate: usize,
    /// whether each input row found a match
    found: Vec<bool>,
}

impl PhysicalJoin {
    pub fn new(
        types: Vec<LogicalType>,
        join_type: JoinType,
        conditions: Vec<JoinCondition>,
        predicate: Option<BoundExpr>,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        let columns = children[1]
            .types()
            .iter()
            .map(|t| Vector::new(t.clone(), STANDARD_VECTOR_SIZE))
            .collect();
        let build = JoinBuild {
            columns,
            count: 0,
            data: None,
            groups: HashMap::new(),
            rows: Vec::new(),
            matched: Vec::new(),
        };
        Self {
            types,
            join_type,
            conditions,
            predicate,
            children,
            build: Mutex::new(build),
        }
    }

    /// The key of each row of `chunk`, or `None` for a row that can match nothing because a key
    /// that is not null-safe is NULL.
    fn keys(&self, chunk: &DataChunk, side: impl Fn(&JoinCondition) -> &BoundExpr) -> Result<Vec<Option<Vec<u8>>>> {
        let vectors = self
            .conditions
            .iter()
            .map(|c| execute(side(c), chunk))
            .collect::<Result<Vec<_>>>()?;
        let references: Vec<&Vector> = vectors.iter().collect();
        let keys = group_keys(&references, chunk.len())?;
        let nulls: Vec<_> = self
            .conditions
            .iter()
            .zip(&vectors)
            .filter(|(c, _)| !c.null_safe)
            .map(|(_, v)| v.to_unified(chunk.len()))
            .collect();
        Ok(keys
            .into_iter()
            .enumerate()
            .map(|(row, key)| nulls.iter().all(|u| u.is_valid(row)).then_some(key))
            .collect())
    }

    /// Whether a left row is done at its first match.
    fn first_match_only(&self) -> bool {
        matches!(self.join_type, JoinType::Semi | JoinType::Anti)
    }

    /// The next pairs of a left and a build row with equal keys, at most a vector of them.
    fn next_key_pairs(&self, build: &JoinBuild, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        while left.len() < STANDARD_VECTOR_SIZE && probe.row < probe.found.len() {
            let row = probe.row;
            let chain: &[u32] = match probe.groups[row] {
                Some(group) if !(self.first_match_only() && probe.found[row]) => &build.rows[group],
                _ => &[],
            };
            let take = chain
                .len()
                .saturating_sub(probe.candidate)
                .min(STANDARD_VECTOR_SIZE - left.len());
            left.extend(std::iter::repeat_n(row as u32, take));
            if take > 0 {
                right.extend_from_slice(&chain[probe.candidate..probe.candidate + take]);
            }
            probe.candidate += take;
            if probe.candidate >= chain.len() {
                probe.row += 1;
                probe.candidate = 0;
            }
        }
        (left, right)
    }

    /// The next pairs without join keys. They come in the order of `CrossProductExecutor`, which
    /// `PhysicalBlockwiseNLJoin` runs on: per vector of the build side, a row of the smaller
    /// chunk against all of the larger one. `probe.row` counts those vectors.
    fn next_pairs(&self, build: &JoinBuild, probe: &mut Probe) -> (Vec<u32>, Vec<u32>) {
        let input_count = probe.found.len();
        loop {
            let start = probe.row * STANDARD_VECTOR_SIZE;
            if start >= build.count {
                return (Vec::new(), Vec::new());
            }
            let size = (build.count - start).min(STANDARD_VECTOR_SIZE);
            let scan_input = input_count < size;
            if probe.candidate >= if scan_input { input_count } else { size } {
                probe.row += 1;
                probe.candidate = 0;
                continue;
            }
            let position = probe.candidate;
            probe.candidate += 1;
            let skip = |row: usize| self.first_match_only() && probe.found[row];
            let (left, right): (Vec<u32>, Vec<u32>) = if scan_input {
                if skip(position) {
                    continue;
                }
                (
                    vec![position as u32; size],
                    (start..start + size).map(|row| row as u32).collect(),
                )
            } else {
                let left: Vec<u32> = (0..input_count)
                    .filter(|&row| !skip(row))
                    .map(|row| row as u32)
                    .collect();
                let right = vec![(start + position) as u32; left.len()];
                (left, right)
            };
            if !left.is_empty() {
                return (left, right);
            }
        }
    }

    /// The rows of `chunk` listed in `rows`.
    fn rows_of(chunk: &DataChunk, rows: Vec<u32>) -> DataChunk {
        let mut result = chunk.clone();
        let count = rows.len();
        result.slice(&SelectionVector::from_vec(rows), count);
        result
    }

    /// The columns of `chunk`, then a NULL column of each of `types`.
    fn with_nulls(chunk: DataChunk, types: &[LogicalType]) -> DataChunk {
        let mut columns = chunk.columns().to_vec();
        columns.extend(types.iter().map(|t| Vector::constant_null(t.clone())));
        DataChunk::from_columns(columns, chunk.len())
    }

    /// What the input chunk adds now that all its pairs are done (`NextLeftJoin` and friends).
    fn finish_probe(&self, input: &DataChunk, found: &[bool]) -> DataChunk {
        let rows = |wanted: bool| -> Vec<u32> {
            (0..found.len() as u32)
                .filter(|&r| found[r as usize] == wanted)
                .collect()
        };
        match self.join_type {
            JoinType::Left | JoinType::Outer => {
                Self::with_nulls(Self::rows_of(input, rows(false)), self.children[1].types())
            }
            JoinType::Semi => Self::rows_of(input, rows(true)),
            JoinType::Anti => Self::rows_of(input, rows(false)),
            _ => empty_chunk(),
        }
    }
}

impl PhysicalOperator for PhysicalJoin {
    fn name(&self) -> &'static str {
        if self.conditions.is_empty() {
            "BLOCKWISE_NL_JOIN"
        } else {
            "HASH_JOIN"
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
    fn sink_child(&self) -> usize {
        1
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        let keys = if self.conditions.is_empty() {
            Vec::new()
        } else {
            self.keys(chunk, |c| &c.right)?
        };
        let mut build = self.build.lock();
        let build = &mut *build;
        let offset = build.count;
        let sel = SelectionVector::from_vec((0..chunk.len() as u32).collect());
        for (target, source) in build.columns.iter_mut().zip(chunk.columns()) {
            target.copy_from(source, &sel, chunk.len(), offset, StringCopy::Deep);
        }
        build.count += chunk.len();
        check_buffered_rows(self.children[1].types(), build.count)?;
        for (row, key) in keys.into_iter().enumerate() {
            let Some(key) = key else { continue };
            let next = build.rows.len();
            let group = *build.groups.entry(key).or_insert(next);
            if group == next {
                build.rows.push(Vec::new());
            }
            build.rows[group].push((offset + row) as u32);
        }
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut build = self.build.lock();
        let columns = std::mem::take(&mut build.columns);
        let count = build.count;
        build.data = Some(DataChunk::from_columns(columns, count));
        build.matched = vec![false; count];
        Ok(())
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(None::<Probe>)
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let state = state.downcast_mut::<Option<Probe>>().expect("join state");
        let mut build = self.build.lock();
        let build = &mut *build;
        let probe = match state {
            Some(probe) => probe,
            None => {
                let groups = if self.conditions.is_empty() {
                    Vec::new()
                } else {
                    let keys = self.keys(input, |c| &c.left)?;
                    keys.iter()
                        .map(|key| key.as_ref().and_then(|key| build.groups.get(key).copied()))
                        .collect()
                };
                state.insert(Probe {
                    groups,
                    row: 0,
                    candidate: 0,
                    found: vec![false; input.len()],
                })
            }
        };
        let data = build.data.as_ref().expect("the build side is finished");
        let emit_pairs = matches!(
            self.join_type,
            JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Outer
        );
        loop {
            let (left, right) = if self.conditions.is_empty() {
                self.next_pairs(build, probe)
            } else {
                self.next_key_pairs(build, probe)
            };
            if left.is_empty() {
                let result = self.finish_probe(input, &probe.found);
                *state = None;
                return Ok((result, OperatorResult::NeedMoreInput));
            }
            let count = left.len();
            let mut columns = Self::rows_of(input, left.clone()).columns().to_vec();
            columns.extend_from_slice(Self::rows_of(data, right.clone()).columns());
            let mut pairs = DataChunk::from_columns(columns, count);
            let matches = match &self.predicate {
                Some(predicate) => {
                    let (sel, matched) = select(predicate, &pairs)?;
                    (matched != count).then_some((sel, matched))
                }
                None => None,
            };
            match &matches {
                Some((sel, matched)) => {
                    for i in 0..*matched {
                        probe.found[left[sel.get(i)] as usize] = true;
                        build.matched[right[sel.get(i)] as usize] = true;
                    }
                }
                None => {
                    for (left, right) in left.iter().zip(&right) {
                        probe.found[*left as usize] = true;
                        build.matched[*right as usize] = true;
                    }
                }
            }
            if !emit_pairs {
                continue;
            }
            if let Some((sel, matched)) = matches {
                pairs.slice(&sel, matched);
            }
            return Ok((pairs, OperatorResult::HaveMoreOutput));
        }
    }
    fn has_unmatched_source(&self) -> bool {
        matches!(
            self.join_type,
            JoinType::Right | JoinType::Outer | JoinType::RightSemi | JoinType::RightAnti
        )
    }
    fn source_state(&self) -> SourceState {
        Box::new(0usize)
    }
    /// The build rows nothing matched, or for RIGHT SEMI the ones something did.
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let next = state.downcast_mut::<usize>().expect("join source state");
        let build = self.build.lock();
        let wanted = self.join_type == JoinType::RightSemi;
        let mut rows = Vec::new();
        while *next < build.count && rows.len() < STANDARD_VECTOR_SIZE {
            if build.matched[*next] == wanted {
                rows.push(*next as u32);
            }
            *next += 1;
        }
        if rows.is_empty() {
            return Ok(None);
        }
        let count = rows.len();
        let right = Self::rows_of(build.data.as_ref().expect("the build side is finished"), rows);
        if matches!(self.join_type, JoinType::RightSemi | JoinType::RightAnti) {
            return Ok(Some(right));
        }
        let mut columns: Vec<Vector> = self.children[0]
            .types()
            .iter()
            .map(|t| Vector::constant_null(t.clone()))
            .collect();
        columns.extend_from_slice(right.columns());
        Ok(Some(DataChunk::from_columns(columns, count)))
    }
}
