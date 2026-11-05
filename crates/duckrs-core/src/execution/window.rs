// Ported from the semantics of duckdb/src/execution/operator/aggregate/physical_window.cpp and
// duckdb/src/function/window/*.cpp @ rust-port-baseline
//
//! `PhysicalWindow`. C++ answers frames with segment and merge sort trees, streams and works in
//! parallel. This operator gathers its input, sorts it per window and walks each frame row by
//! row, which is right in all cases and quadratic in the worst ones. Phases 8 and 13 bring the
//! fast paths; only frames that grow from a fixed start are incremental here.

use super::expression_executor::{evaluate_scalar, execute};
use super::physical::{PhysicalOperator, SourceState, scan_collection};
use crate::function::aggregates::{AggregateKind, value_keys};
use crate::planner::expression::{BoundExpr, BoundWindow, WindowFunction};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::{OrderModifiers, create_sort_keys};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use duckrs_parser::ast::{WindowBoundary, WindowExclude};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug)]
pub struct PhysicalWindow {
    types: Vec<LogicalType>,
    windows: Vec<BoundWindow>,
    children: Vec<Box<dyn PhysicalOperator>>,
    data: Mutex<ColumnDataCollection>,
    output: Mutex<ColumnDataCollection>,
}

impl PhysicalWindow {
    pub fn new(types: Vec<LogicalType>, windows: Vec<BoundWindow>, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        let input = types[..types.len() - windows.len()].to_vec();
        Self {
            output: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            windows,
            children,
            data: Mutex::new(ColumnDataCollection::new(input)),
        }
    }
}

impl PhysicalOperator for PhysicalWindow {
    fn name(&self) -> &'static str {
        "WINDOW"
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
        let data = self.data.lock();
        let chunks: Vec<&DataChunk> = data.chunks().collect();
        let count = data.len();
        if count == 0 {
            return Ok(());
        }
        // windows over the same partitions and orders share their sort
        let mut sorts: HashMap<String, Arc<Sorted>> = HashMap::new();
        let mut results = Vec::with_capacity(self.windows.len());
        let mut first_order = None;
        for window in &self.windows {
            let key = format!("{:?}{:?}", window.partitions, window.orders);
            let sorted = match sorts.get(&key) {
                Some(sorted) => sorted.clone(),
                None => {
                    let sorted = Arc::new(Sorted::new(window, &chunks, count)?);
                    sorts.insert(key, sorted.clone());
                    sorted
                }
            };
            let by_position = Evaluation::new(window, &sorted, &chunks, count)?.run()?;
            let mut by_row = vec![Value::sql_null(); count];
            for (position, value) in by_position.into_iter().enumerate() {
                by_row[sorted.rows[position]] = value;
            }
            results.push(by_row);
            first_order.get_or_insert_with(|| sorted.clone());
        }
        // the rows leave in the order of the first window, as they do in C++
        let first_order = first_order.expect("a window operator has windows");
        let mut output = self.output.lock();
        for rows in first_order.rows.chunks(STANDARD_VECTOR_SIZE) {
            let mut chunk = DataChunk::new(&self.types);
            for row in rows {
                let source = chunks[row / STANDARD_VECTOR_SIZE];
                let at = row % STANDARD_VECTOR_SIZE;
                let mut values: Vec<Value> = (0..source.columns().len())
                    .map(|c| source.column(c).get_value(at))
                    .collect();
                values.extend(results.iter().map(|result| result[*row].clone()));
                chunk.append_row(&values)?;
            }
            output.append(&chunk);
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

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

fn out_of_range(message: &str) -> Error {
    Error::new(ErrorKind::OutOfRange, message)
}

/// The value of `expr` for each of the rows, by row.
fn evaluate(expr: &BoundExpr, chunks: &[&DataChunk]) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for chunk in chunks {
        let column = execute(expr, chunk)?;
        values.extend((0..chunk.len()).map(|row| column.get_value(row)));
    }
    Ok(values)
}

/// The sort key of each row in `orders`, by row.
fn order_keys(orders: &[(&BoundExpr, OrderModifiers)], chunks: &[&DataChunk], count: usize) -> Result<Vec<Vec<u8>>> {
    if orders.is_empty() {
        return Ok(vec![Vec::new(); count]);
    }
    let mut keys = Vec::with_capacity(count);
    for chunk in chunks {
        let columns = orders
            .iter()
            .map(|(expr, _)| execute(expr, chunk))
            .collect::<Result<Vec<Vector>>>()?;
        let columns: Vec<(&Vector, OrderModifiers)> = columns.iter().zip(orders).map(|(c, (_, m))| (c, *m)).collect();
        keys.extend(create_sort_keys(&columns, chunk.len())?);
    }
    Ok(keys)
}

/// The rows of a window in order, with their partitions and groups of peers.
#[derive(Debug)]
struct Sorted {
    /// position -> row
    rows: Vec<usize>,
    /// by position
    order_keys: Vec<Vec<u8>>,
    /// where each partition is
    partitions: Vec<(usize, usize)>,
    /// the first and the end of the peers of each position
    peers: Vec<(usize, usize)>,
}

impl Sorted {
    fn new(window: &BoundWindow, chunks: &[&DataChunk], count: usize) -> Result<Self> {
        let partitions: Vec<(&BoundExpr, OrderModifiers)> = window
            .partitions
            .iter()
            .map(|p| (p, OrderModifiers::ASC_NULLS_LAST))
            .collect();
        let partition_keys = order_keys(&partitions, chunks, count)?;
        let orders: Vec<(&BoundExpr, OrderModifiers)> = window.orders.iter().map(|(e, m)| (e, *m)).collect();
        let keys = order_keys(&orders, chunks, count)?;
        let mut rows: Vec<usize> = (0..count).collect();
        rows.sort_by(|a, b| {
            partition_keys[*a]
                .cmp(&partition_keys[*b])
                .then_with(|| keys[*a].cmp(&keys[*b]))
        });
        let mut partitions = Vec::new();
        let mut peers = vec![(0, 0); count];
        let mut partition_start = 0;
        let mut peer_start = 0;
        for position in 1..=count {
            let new_partition =
                position == count || partition_keys[rows[position]] != partition_keys[rows[position - 1]];
            if new_partition || keys[rows[position]] != keys[rows[position - 1]] {
                peers[peer_start..position].fill((peer_start, position));
                peer_start = position;
            }
            if new_partition {
                partitions.push((partition_start, position));
                partition_start = position;
            }
        }
        let order_keys = rows.iter().map(|row| keys[*row].clone()).collect();
        Ok(Self {
            rows,
            order_keys,
            partitions,
            peers,
        })
    }
}

/// One boundary of the frame, ready to use: its value by position.
enum Boundary {
    /// `ROWS` and `GROUPS` offsets.
    Offsets(Vec<Value>),
    /// The sort keys of the values `RANGE` searches for; `None` where they are NULL.
    RangeKeys(Vec<Option<Vec<u8>>>),
    Fixed,
}

struct Evaluation<'a> {
    window: &'a BoundWindow,
    sorted: &'a Sorted,
    /// by child, by position
    children: Vec<Vec<Value>>,
    /// an aggregate's argument, by position: partitions share it, so it is built once
    input: Option<Vector>,
    /// `value_keys` of `input`, for what compares it: DISTINCT, `min` and `max`
    input_keys: Option<Vec<Vec<u8>>>,
    /// by position: whether FILTER lets the row in
    filtered_in: Vec<bool>,
    arg_keys: Option<Vec<Vec<u8>>>,
    start: Boundary,
    end: Boundary,
    /// by position, with RANGE: whether the order is NULL
    null_order: Vec<bool>,
}

impl<'a> Evaluation<'a> {
    fn new(window: &'a BoundWindow, sorted: &'a Sorted, chunks: &[&DataChunk], count: usize) -> Result<Self> {
        let by_position =
            |values: Vec<Value>| -> Vec<Value> { sorted.rows.iter().map(|r| values[*r].clone()).collect() };
        let mut children = Vec::with_capacity(window.children.len());
        for child in &window.children {
            children.push(by_position(evaluate(child, chunks)?));
        }
        let input = match (&window.aggregate, window.children.first()) {
            (Some(_), Some(child)) => {
                let mut vector = Vector::new(child.return_type.clone(), count.max(1));
                for (position, value) in children[0].iter().enumerate() {
                    vector.set_value(position, value)?;
                }
                Some(vector)
            }
            _ => None,
        };
        let compares = window.distinct
            || window
                .aggregate
                .as_ref()
                .is_some_and(|f| matches!(f.kind, AggregateKind::Min | AggregateKind::Max));
        let input_keys = match &input {
            Some(input) if compares => Some(value_keys(input, count)?),
            _ => None,
        };
        let filtered_in = match &window.filter {
            Some(filter) => by_position(evaluate(filter, chunks)?)
                .iter()
                .map(|v| matches!(v.data(), ValueData::Bool(true)))
                .collect(),
            None => vec![true; count],
        };
        let arg_keys = if window.arg_orders.is_empty() {
            None
        } else {
            let orders: Vec<(&BoundExpr, OrderModifiers)> = window.arg_orders.iter().map(|(e, m)| (e, *m)).collect();
            let keys = order_keys(&orders, chunks, count)?;
            Some(sorted.rows.iter().map(|r| keys[*r].clone()).collect())
        };
        let is_range = |b: WindowBoundary| {
            matches!(
                b,
                WindowBoundary::ExprPrecedingRange | WindowBoundary::ExprFollowingRange
            )
        };
        let boundary = |expr: &Option<BoundExpr>, kind: WindowBoundary| -> Result<Boundary> {
            let Some(expr) = expr else {
                return Ok(Boundary::Fixed);
            };
            if !is_range(kind) {
                return Ok(Boundary::Offsets(by_position(evaluate(expr, chunks)?)));
            }
            let modifiers = window.orders[0].1;
            let keys = order_keys(&[(expr, modifiers)], chunks, count)?;
            let values = evaluate(expr, chunks)?;
            Ok(Boundary::RangeKeys(
                sorted
                    .rows
                    .iter()
                    .map(|r| (!values[*r].is_null()).then(|| keys[*r].clone()))
                    .collect(),
            ))
        };
        let start = boundary(&window.start_expr, window.start)?;
        let end = boundary(&window.end_expr, window.end)?;
        let null_order = if is_range(window.start) || is_range(window.end) {
            by_position(evaluate(&window.orders[0].0, chunks)?)
                .iter()
                .map(Value::is_null)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            window,
            sorted,
            children,
            input,
            input_keys,
            filtered_in,
            arg_keys,
            start,
            end,
            null_order,
        })
    }

    fn run(&self) -> Result<Vec<Value>> {
        let count = self.sorted.rows.len();
        let mut result = Vec::with_capacity(count);
        for &(begin, end) in &self.sorted.partitions {
            let groups = self.peer_groups(begin, end);
            let frames = self.frames(begin, end, &groups)?;
            self.run_partition(begin, end, &groups, &frames, &mut result)?;
        }
        Ok(result)
    }

    /// The groups of peers of a partition.
    fn peer_groups(&self, begin: usize, end: usize) -> Vec<(usize, usize)> {
        let mut groups = Vec::new();
        let mut at = begin;
        while at < end {
            groups.push(self.sorted.peers[at]);
            at = self.sorted.peers[at].1;
        }
        groups
    }

    /// The frame of each position of a partition, `WindowBoundariesState`.
    fn frames(&self, begin: usize, end: usize, groups: &[(usize, usize)]) -> Result<Vec<(usize, usize)>> {
        let window = self.window;
        // RANGE only looks at the rows with an order
        let (valid_begin, valid_end) = if self.null_order.is_empty() {
            (begin, end)
        } else {
            let first = (begin..end).find(|p| !self.null_order[*p]).unwrap_or(end);
            let last = (begin..end).rfind(|p| !self.null_order[*p]).map_or(first, |p| p + 1);
            (first, last)
        };
        let keys = &self.sorted.order_keys;
        let mut frames = Vec::with_capacity(end - begin);
        let mut group = 0;
        for position in begin..end {
            let peers = self.sorted.peers[position];
            if peers.0 >= groups[group].1 {
                group += 1;
            }
            let offset = |boundary: &Boundary, name: &str, kind: &str| -> Result<i64> {
                let Boundary::Offsets(values) = boundary else {
                    unreachable!("the binder gives this boundary an expression");
                };
                let value = &values[position];
                if value.is_null() {
                    return Err(invalid_input(&format!(
                        "Window {kind} {name} expression cannot be NULL"
                    )));
                }
                let offset = value.as_i128().expect("cast to BIGINT") as i64;
                if offset < 0 {
                    return Err(invalid_input(&format!(
                        "Window {kind} {name} expression cannot be negative"
                    )));
                }
                Ok(offset)
            };
            let groups_offset = |boundary: &Boundary, name: &str| -> Result<Option<usize>> {
                let Boundary::Offsets(values) = boundary else {
                    unreachable!("the binder gives this boundary an expression");
                };
                let value = &values[position];
                if value.is_null() {
                    return Ok(None);
                }
                let offset = value.as_i128().expect("cast to BIGINT");
                if offset < 0 {
                    return Err(out_of_range(&format!("Invalid GROUPS {name} value")));
                }
                Ok(Some(offset as usize))
            };
            let search = |key: &Vec<u8>, after_equals: bool| -> usize {
                let rows = &keys[valid_begin..valid_end];
                valid_begin + rows.partition_point(|k| if after_equals { k <= key } else { k < key })
            };
            let frame_begin = match window.start {
                WindowBoundary::UnboundedPreceding | WindowBoundary::Invalid => begin,
                WindowBoundary::CurrentRowRows => position,
                WindowBoundary::CurrentRowRange | WindowBoundary::CurrentRowGroups => peers.0,
                WindowBoundary::ExprPrecedingRows => {
                    position.saturating_sub(offset(&self.start, "PRECEDING", "ROWS")? as usize)
                }
                WindowBoundary::ExprFollowingRows => {
                    position.saturating_add(offset(&self.start, "FOLLOWING", "ROWS")? as usize)
                }
                WindowBoundary::ExprPrecedingRange => match self.range_key(&self.start, position, true)? {
                    Some(key) => search(key, false),
                    None => peers.0,
                },
                WindowBoundary::ExprFollowingRange => match self.range_key(&self.start, position, false)? {
                    Some(key) => search(key, false),
                    None => peers.0,
                },
                WindowBoundary::ExprPrecedingGroups => match groups_offset(&self.start, "PRECEDING")? {
                    Some(n) => group.checked_sub(n).map_or(begin, |g| groups[g].0),
                    None => peers.0,
                },
                WindowBoundary::ExprFollowingGroups => match groups_offset(&self.start, "FOLLOWING")? {
                    Some(n) => groups.get(group.saturating_add(n)).map_or(end, |g| g.0),
                    None => peers.0,
                },
                WindowBoundary::UnboundedFollowing => end,
            };
            let frame_end = match window.end {
                WindowBoundary::UnboundedFollowing => end,
                WindowBoundary::CurrentRowRows => position + 1,
                WindowBoundary::CurrentRowRange | WindowBoundary::CurrentRowGroups | WindowBoundary::Invalid => peers.1,
                WindowBoundary::ExprPrecedingRows => {
                    (position + 1).saturating_sub(offset(&self.end, "PRECEDING", "ROWS")? as usize)
                }
                WindowBoundary::ExprFollowingRows => {
                    (position + 1).saturating_add(offset(&self.end, "FOLLOWING", "ROWS")? as usize)
                }
                WindowBoundary::ExprPrecedingRange => match self.range_key(&self.end, position, true)? {
                    Some(key) => search(key, true),
                    None => peers.1,
                },
                WindowBoundary::ExprFollowingRange => match self.range_key(&self.end, position, false)? {
                    Some(key) => search(key, true),
                    None => peers.1,
                },
                WindowBoundary::ExprPrecedingGroups => match groups_offset(&self.end, "PRECEDING")? {
                    Some(n) => group.checked_sub(n).map_or(begin, |g| groups[g].1),
                    None => peers.1,
                },
                WindowBoundary::ExprFollowingGroups => match groups_offset(&self.end, "FOLLOWING")? {
                    Some(n) => groups.get(group.saturating_add(n)).map_or(end, |g| g.1),
                    None => peers.1,
                },
                WindowBoundary::UnboundedPreceding => begin,
            };
            frames.push((frame_begin.clamp(begin, end), frame_end.clamp(begin, end)));
        }
        Ok(frames)
    }

    /// What a RANGE boundary searches for at `position`, `FindOrderedRangeBound`.
    fn range_key<'b>(&self, boundary: &'b Boundary, position: usize, preceding: bool) -> Result<Option<&'b Vec<u8>>> {
        let Boundary::RangeKeys(values) = boundary else {
            unreachable!("the binder gives this boundary an expression");
        };
        let key = values[position].as_ref();
        if let Some(key) = key {
            let own = &self.sorted.order_keys[position];
            if preceding && key > own {
                return Err(out_of_range("Invalid RANGE PRECEDING value"));
            }
            if !preceding && key < own {
                return Err(out_of_range("Invalid RANGE FOLLOWING value"));
            }
        }
        Ok(key)
    }

    /// The parts of a frame that EXCLUDE leaves, `WindowAggregator::EvaluateSubFrames`.
    fn sub_frames(&self, frame: (usize, usize), position: usize) -> Vec<(usize, usize)> {
        let (begin, end) = frame;
        if begin >= end {
            return Vec::new();
        }
        let peers = self.sorted.peers[position];
        let cut = |from: usize, to: usize| (from.clamp(begin, end), to.clamp(begin, end));
        let parts = match self.window.exclude {
            WindowExclude::NoOther => return vec![frame],
            WindowExclude::CurrentRow => vec![cut(begin, position), cut(position + 1, end)],
            WindowExclude::Group => vec![cut(begin, peers.0), cut(peers.1, end)],
            WindowExclude::Ties => vec![cut(begin, peers.0), cut(position, position + 1), cut(peers.1, end)],
        };
        parts.into_iter().filter(|(from, to)| from < to).collect()
    }

    /// The positions of a frame the function sees, in the order it sees them.
    fn frame_rows(&self, frame: (usize, usize), position: usize, skip_nulls: bool) -> Vec<usize> {
        let nulls = self.children.first().filter(|_| skip_nulls);
        let mut rows: Vec<usize> = self
            .sub_frames(frame, position)
            .into_iter()
            .flat_map(|(from, to)| from..to)
            .filter(|p| self.filtered_in[*p] && !nulls.is_some_and(|values| values[*p].is_null()))
            .collect();
        if let Some(keys) = &self.arg_keys {
            rows.sort_by(|a, b| keys[*a].cmp(&keys[*b]).then(a.cmp(b)));
        }
        rows
    }

    fn run_partition(
        &self,
        begin: usize,
        end: usize,
        groups: &[(usize, usize)],
        frames: &[(usize, usize)],
        result: &mut Vec<Value>,
    ) -> Result<()> {
        use WindowFunction::*;
        let window = self.window;
        let null = || Value::null(window.return_type.clone());
        match window.function {
            Aggregate => self.run_aggregate(begin, frames, result),
            Fill => self.run_fill(begin, end, result),
            RowNumber | Rank | DenseRank | PercentRank | CumeDist | Ntile if self.arg_keys.is_some() => {
                for position in begin..end {
                    let rows = self.frame_rows(frames[position - begin], position, false);
                    result.push(self.ranking_in_frame(position, &rows)?);
                }
                Ok(())
            }
            RowNumber => {
                result.extend((begin..end).map(|p| Value::bigint((p - begin + 1) as i64)));
                Ok(())
            }
            Ntile => {
                for position in begin..end {
                    result.push(ntile(&self.children[0][position], end - begin, position - begin)?);
                }
                Ok(())
            }
            Rank | DenseRank | PercentRank | CumeDist => {
                let count = (end - begin) as i64;
                let mut group = 0;
                for position in begin..end {
                    let peers = self.sorted.peers[position];
                    if peers.0 >= groups[group].1 {
                        group += 1;
                    }
                    let rank = (peers.0 - begin + 1) as i64;
                    result.push(match window.function {
                        Rank => Value::bigint(rank),
                        DenseRank => Value::bigint(group as i64 + 1),
                        PercentRank if count > 1 => Value::double((rank - 1) as f64 / (count - 1) as f64),
                        PercentRank => Value::double(0.0),
                        _ => Value::double((peers.1 - begin) as f64 / count as f64),
                    });
                }
                Ok(())
            }
            Lag | Lead => {
                for position in begin..end {
                    result.push(self.lead_lag(position, begin, end, frames[position - begin])?);
                }
                Ok(())
            }
            FirstValue | LastValue | NthValue => {
                for position in begin..end {
                    let rows = self.frame_rows(frames[position - begin], position, window.ignore_nulls);
                    let row = match window.function {
                        FirstValue => rows.first(),
                        LastValue => rows.last(),
                        _ => {
                            let n = self.children[1][position].as_i128().filter(|n| *n >= 1);
                            n.and_then(|n| rows.get(usize::try_from(n - 1).ok()?))
                        }
                    };
                    result.push(row.map_or_else(null, |row| self.children[0][*row].clone()));
                }
                Ok(())
            }
        }
    }

    /// RANK and its kind with ORDER BY among their arguments: over the frame, in that order.
    fn ranking_in_frame(&self, position: usize, rows: &[usize]) -> Result<Value> {
        use WindowFunction::*;
        let keys = self.arg_keys.as_ref().expect("checked by the caller");
        let own = &keys[position];
        let count = rows.len();
        let below = rows.iter().filter(|r| keys[**r] < *own).count();
        let own_or_below = rows.iter().filter(|r| keys[**r] <= *own).count();
        let row_number = below + rows.iter().filter(|r| keys[**r] == *own && **r < position).count() + 1;
        Ok(match self.window.function {
            RowNumber => Value::bigint(row_number as i64),
            Rank => Value::bigint(below as i64 + 1),
            DenseRank => {
                let distinct: HashSet<&Vec<u8>> = rows.iter().map(|r| &keys[*r]).filter(|k| **k < *own).collect();
                Value::bigint(distinct.len() as i64 + 1)
            }
            PercentRank if count > 1 => Value::double(below as f64 / (count - 1) as f64),
            PercentRank => Value::double(0.0),
            CumeDist if count > 0 => Value::double(own_or_below as f64 / count as f64),
            CumeDist => Value::double(0.0),
            _ if count == 0 => Value::null(LogicalType::BIGINT),
            _ => ntile(&self.children[0][position], count, (row_number - 1).min(count - 1))?,
        })
    }

    fn lead_lag(&self, position: usize, begin: usize, end: usize, frame: (usize, usize)) -> Result<Value> {
        let window = self.window;
        let lead = window.function == WindowFunction::Lead;
        let offset = match self.children.get(1) {
            Some(offsets) => match offsets[position].as_i128() {
                Some(offset) => offset,
                None => return Ok(Value::null(window.return_type.clone())),
            },
            None => 1,
        };
        let forward = if lead { offset } else { -offset };
        let default = || match self.children.get(2) {
            Some(defaults) => defaults[position].clone(),
            None => Value::null(window.return_type.clone()),
        };
        if let Some(keys) = &self.arg_keys {
            let rows = self.frame_rows(frame, position, window.ignore_nulls);
            let own_key = (&keys[position], position);
            // the rows before this one, and if it isn't there, the place where it would go
            let own_row = rows.iter().filter(|r| (&keys[**r], **r) < own_key).count() as i128;
            let mut target = own_row + forward;
            if frame.0 > position || position >= frame.1 {
                if offset == 0 {
                    return Ok(self.children[0][position].clone());
                }
                if lead == (offset > 0) {
                    target -= 1;
                }
            }
            return Ok(usize::try_from(target)
                .ok()
                .and_then(|at| rows.get(at))
                .map_or_else(default, |row| self.children[0][*row].clone()));
        }
        if forward == 0 {
            return Ok(self.children[0][position].clone());
        }
        let usable = |p: usize| self.filtered_in[p] && !(window.ignore_nulls && self.children[0][p].is_null());
        let mut remaining = forward.unsigned_abs();
        let target = if forward > 0 {
            (position + 1..end).find(|p| {
                if usable(*p) {
                    remaining -= 1;
                }
                remaining == 0
            })
        } else {
            (begin..position).rev().find(|p| {
                if usable(*p) {
                    remaining -= 1;
                }
                remaining == 0
            })
        };
        Ok(target.map_or_else(default, |row| self.children[0][row].clone()))
    }

    fn run_aggregate(&self, begin: usize, frames: &[(usize, usize)], result: &mut Vec<Value>) -> Result<()> {
        let window = self.window;
        let function = window.aggregate.as_ref().expect("an aggregate window has a function");
        let count = self.sorted.rows.len();
        let input = self.input.as_ref();
        let input_type = window.children.first().map(|c| &c.return_type);
        let keys = self.input_keys.as_deref();
        let incremental = window.exclude == WindowExclude::NoOther && !window.distinct && self.arg_keys.is_none();
        let mut states = [function.kind.initial_state()];
        let mut previous: Option<(usize, usize)> = None;
        for (index, frame) in frames.iter().enumerate() {
            let position = begin + index;
            let mut rows: Vec<(usize, usize)>;
            if incremental {
                let extends = previous.filter(|(from, to)| *from == frame.0 && *to <= frame.1 && *from < *to);
                let from = match extends {
                    Some((_, to)) => to,
                    None => {
                        states = [function.kind.initial_state()];
                        frame.0
                    }
                };
                rows = (from..frame.1)
                    .filter(|p| self.filtered_in[*p])
                    .map(|p| (p, 0))
                    .collect();
                previous = Some(*frame);
            } else {
                states = [function.kind.initial_state()];
                rows = self
                    .frame_rows(*frame, position, false)
                    .into_iter()
                    .map(|p| (p, 0))
                    .collect();
                if let Some(keys) = keys.filter(|_| window.distinct) {
                    let mut seen = HashSet::new();
                    let is_null = |p: usize| self.children[0][p].is_null();
                    rows.retain(|(p, _)| !is_null(*p) && seen.insert(&keys[*p]));
                }
            }
            if !rows.is_empty() {
                function.update_keyed(input, count, &rows, &mut states, keys)?;
            }
            result.push(function.finalize(&states[0], &window.return_type, input_type)?);
        }
        Ok(())
    }

    /// `WindowFillExecutor`: NULLs become what a line through their neighbours says.
    fn run_fill(&self, begin: usize, end: usize, result: &mut Vec<Value>) -> Result<()> {
        let window = self.window;
        let values = &self.children[0];
        // the order to measure along: the argument's, or else the window's
        let along = &self.children[1];
        let mut rows: Vec<usize> = (begin..end).collect();
        if let Some(keys) = &self.arg_keys {
            rows.sort_by(|a, b| keys[*a].cmp(&keys[*b]).then(a.cmp(b)));
        }
        let point = |p: usize| Some((number(&along[p])?, number(&values[p])?));
        let points: Vec<(usize, Option<(f64, f64)>)> = rows.iter().map(|p| (*p, point(*p))).collect();
        let start = result.len();
        result.extend(values[begin..end].iter().cloned());
        for (at, (position, _)) in points.iter().enumerate() {
            if !values[*position].is_null() {
                continue;
            }
            let Some(x) = number(&along[*position]) else {
                continue;
            };
            let before: Vec<(f64, f64)> = points[..at].iter().rev().filter_map(|(_, p)| *p).take(2).collect();
            let after: Vec<(f64, f64)> = points[at + 1..].iter().filter_map(|(_, p)| *p).take(2).collect();
            let line = match (before.as_slice(), after.as_slice()) {
                ([low, ..], [high, ..]) => Some((*low, *high)),
                ([last, previous], []) => Some((*previous, *last)),
                ([], [first, next]) => Some((*first, *next)),
                ([only], []) | ([], [only]) => Some((*only, *only)),
                _ => None,
            };
            let Some(((x0, y0), (x1, y1))) = line else {
                continue;
            };
            // halved, for orders whose span doesn't fit in a double
            let span = x1 / 2.0 - x0 / 2.0;
            let along_line = if span == 0.0 { 0.0 } else { (x / 2.0 - x0 / 2.0) / span };
            let filled = y0 + (y1 - y0) * along_line;
            result[start + (*position - begin)] = like(&window.return_type, filled)?;
        }
        Ok(())
    }
}

/// The number a value stands for, the unscaled one for DECIMAL.
fn number(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Float(v) => Some(f64::from(*v)),
        ValueData::Double(v) => Some(*v),
        _ => value.as_i128().map(|v| v as f64),
    }
}

/// `number` in reverse.
fn like(ty: &LogicalType, number: f64) -> Result<Value> {
    use duckrs_common::types::PhysicalType as P;
    let rounded = number.round();
    let data = match ty.physical_type() {
        P::Float => ValueData::Float(number as f32),
        P::Double => ValueData::Double(number),
        P::Int8 => ValueData::Int8(rounded as i8),
        P::Int16 => ValueData::Int16(rounded as i16),
        P::Int32 => ValueData::Int32(rounded as i32),
        P::Int64 => ValueData::Int64(rounded as i64),
        P::Int128 => ValueData::Int128(rounded as i128),
        P::UInt8 => ValueData::UInt8(rounded as u8),
        P::UInt16 => ValueData::UInt16(rounded as u16),
        P::UInt32 => ValueData::UInt32(rounded as u32),
        P::UInt64 => ValueData::UInt64(rounded as u64),
        P::UInt128 => ValueData::UInt128(rounded as u128),
        _ => return evaluate_scalar(&BoundExpr::constant(Value::double(number)).cast_to(ty, false)),
    };
    // times stop at the end of the day
    let day = match ty.id() {
        LogicalTypeId::Time => Some(86_400_000_000.0),
        LogicalTypeId::TimeNs => Some(86_400_000_000_000.0),
        _ => None,
    };
    if day.is_some_and(|day| !(0.0..=day).contains(&rounded)) {
        return Ok(Value::null(ty.clone()));
    }
    Value::new(ty.clone(), data)
}

/// `WindowNtileExecutor`, with thanks from SQLite's ntileValueFunc().
fn ntile(buckets: &Value, total: usize, row: usize) -> Result<Value> {
    let Some(buckets) = buckets.as_i128() else {
        return Ok(Value::null(LogicalType::BIGINT));
    };
    if buckets <= 0 {
        return Err(invalid_input("Argument for ntile must be greater than zero"));
    }
    let total = total as i128;
    let buckets = buckets.min(total);
    let size = total / buckets;
    let large = total - buckets * size;
    let small_start = large * (size + 1);
    let row = row as i128;
    let bucket = if row < small_start {
        1 + row / (size + 1)
    } else {
        1 + large + (row - small_start) / size
    };
    Ok(Value::bigint(bucket as i64))
}
