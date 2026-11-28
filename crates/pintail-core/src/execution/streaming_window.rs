// Ported from duckdb/src/execution/operator/aggregate/physical_streaming_window.cpp and the streaming
// APIs of duckdb/src/function/window/*.cpp @ rust-port-baseline
//
//! `PhysicalStreamingWindow`: windows over all rows in the order they come, with no PARTITION BY
//! or ORDER BY, whose values depend only on the rows so far (`IsStreamingFunction`). They are
//! computed as the rows pass. LEAD holds rows back until the rows it reads have come
//! (`delayed`), and gives the rest the default at the end (`FinalExecute`).

use super::expression_executor::{evaluate_scalar, execute};
use super::physical::{OperatorResult, OperatorState, PhysicalOperator};
use crate::function::aggregates::{AggregateKind, AggregateState, value_keys};
use crate::function::holistic::Holistic;
use crate::planner::expression::{BoundExpr, BoundWindow, WindowFunction};
use pintail_common::types::{LogicalType, PhysicalType};
use pintail_common::value::Value;
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Result, STANDARD_VECTOR_SIZE};
use pintail_parser::ast::{WindowBoundary, WindowExclude};
use std::collections::{HashSet, VecDeque};

/// `StreamingWindowState::MAX_BUFFER`: how far LEAD and LAG can look.
const MAX_BUFFER: i64 = 2048;

/// Whether a state of the aggregate frees something when it goes (`HasStateDestructorCallback`):
/// the holistic ones, and those that keep a value that is not a fixed-size number.
fn has_state_destructor(kind: AggregateKind, window: &BoundWindow) -> bool {
    use Holistic::*;
    let keeps_value = |ty: &LogicalType| {
        !matches!(
            ty.physical_type(),
            PhysicalType::Bool
                | PhysicalType::Int8
                | PhysicalType::Int16
                | PhysicalType::Int32
                | PhysicalType::Int64
                | PhysicalType::Int128
                | PhysicalType::UInt8
                | PhysicalType::UInt16
                | PhysicalType::UInt32
                | PhysicalType::UInt64
                | PhysicalType::UInt128
                | PhysicalType::Float
                | PhysicalType::Double
                | PhysicalType::Interval
        )
    };
    let any_kept = || window.children.iter().any(|c| keeps_value(&c.return_type));
    match kind {
        AggregateKind::Min
        | AggregateKind::Max
        | AggregateKind::First
        | AggregateKind::Last
        | AggregateKind::AnyValue => any_kept(),
        AggregateKind::SumBignum => true,
        AggregateKind::Holistic(holistic) => match holistic {
            Mode | Median | QuantileCont | QuantileDisc | Mad | Entropy | VariantGroupObject | MinN | MaxN
            | ArgMinN | ArgMaxN | ArgMinNullsLastN | ArgMaxNullsLastN | ApproxTopK | BitstringAgg | CombineAggr
            | Histogram | HistogramBins | HistogramExact | ApproxQuantile | Sorted => true,
            ArgMin | ArgMax | ArgMinNull | ArgMaxNull | ArgMinNullsLast | ArgMaxNullsLast => any_kept(),
            _ => false,
        },
        _ => false,
    }
}

/// A constant argument, if it is one (`IsFoldable` and `TryEvaluateScalar`).
fn constant(expr: &BoundExpr) -> Option<Value> {
    if !expr.is_foldable() {
        return None;
    }
    evaluate_scalar(expr).ok()
}

/// `ComputeOffset`: how many rows back a LAG reads, or ahead (negative) a LEAD does.
fn lead_lag_offset(window: &BoundWindow) -> Option<i64> {
    let mut offset = 1;
    if let Some(expr) = window.children.get(1) {
        let value = constant(expr)?;
        offset = i64::try_from(value.as_i128()?).ok()?;
        if offset == i64::MIN {
            return None;
        }
    }
    Some(if window.function == WindowFunction::Lead {
        -offset
    } else {
        offset
    })
}

/// `ComputeDefault`.
fn lead_lag_default(window: &BoundWindow) -> Option<Value> {
    let Some(expr) = window.children.get(2) else {
        return Some(Value::null(window.return_type.clone()));
    };
    let value = constant(expr)?;
    if value.is_null() {
        return Some(Value::null(window.return_type.clone()));
    }
    (*value.logical_type() == window.return_type).then_some(value)
}

/// `ComputeNthIndex`: the one-based row, with 0 for none.
fn nth_index(window: &BoundWindow) -> Option<u64> {
    let value = constant(window.children.get(1)?)?;
    let n = value.as_i128()?;
    Some(if n > 0 { u64::try_from(n).ok()? } else { 0 })
}

/// `PhysicalStreamingWindow::IsStreamingFunction`.
pub fn is_streaming(window: &BoundWindow) -> bool {
    if !window.partitions.is_empty()
        || !window.orders.is_empty()
        || !window.arg_orders.is_empty()
        || window.exclude != WindowExclude::NoOther
    {
        return false;
    }
    let running = window.start == WindowBoundary::UnboundedPreceding && window.end == WindowBoundary::CurrentRowRows;
    let volatile = window.children.iter().any(BoundExpr::is_volatile);
    use WindowFunction::*;
    match window.function {
        Aggregate => {
            let Some(function) = &window.aggregate else {
                return false;
            };
            // the aggregate of a window here reads its first argument
            running
                && window.children.len() <= 1
                && function.export.is_none()
                && !has_state_destructor(function.kind, window)
        }
        RowNumber | Rank | DenseRank | PercentRank => true,
        Lag | Lead => {
            if window.ignore_nulls || lead_lag_default(window).is_none() {
                return false;
            }
            let Some(offset) = lead_lag_offset(window) else {
                return false;
            };
            if offset < 0 && volatile {
                return false;
            }
            offset.abs() < MAX_BUFFER
        }
        FirstValue => {
            if volatile || window.start != WindowBoundary::UnboundedPreceding {
                return false;
            }
            if window.ignore_nulls {
                return window.end == WindowBoundary::CurrentRowRows;
            }
            matches!(
                window.end,
                WindowBoundary::CurrentRowRows
                    | WindowBoundary::CurrentRowRange
                    | WindowBoundary::CurrentRowGroups
                    | WindowBoundary::UnboundedFollowing
            )
        }
        LastValue => !volatile && running,
        NthValue => !volatile && running && nth_index(window).is_some(),
        CumeDist | Ntile | Fill => false,
    }
}

#[derive(Debug)]
pub struct PhysicalStreamingWindow {
    types: Vec<LogicalType>,
    windows: Vec<BoundWindow>,
    children: Vec<Box<dyn PhysicalOperator>>,
    /// whether a LEAD holds rows back
    delays: bool,
}

impl PhysicalStreamingWindow {
    pub fn new(types: Vec<LogicalType>, windows: Vec<BoundWindow>, children: Vec<Box<dyn PhysicalOperator>>) -> Self {
        let delays = windows.iter().any(|w| {
            matches!(w.function, WindowFunction::Lag | WindowFunction::Lead)
                && lead_lag_offset(w).is_some_and(|o| o < 0)
        });
        Self {
            types,
            windows,
            children,
            delays,
        }
    }
}

/// What a window remembers of the rows so far.
enum WindowState {
    RowNumber,
    Constant(Value),
    Aggregate {
        state: AggregateState,
        seen: HashSet<Vec<u8>>,
    },
    /// the last `offset` values of the argument, the oldest first
    Lag {
        offset: usize,
        default: Value,
        history: VecDeque<Value>,
    },
    /// the rows ahead it reads
    Lead {
        offset: usize,
        default: Value,
    },
    First(Option<Value>),
    Last(Value),
    Nth {
        index: u64,
        count: u64,
        value: Value,
    },
}

/// A row held back for a LEAD: its values, and the results of its windows, of which the LEADs
/// come later.
struct Delayed {
    values: Vec<Value>,
    results: Vec<Option<Value>>,
}

struct State {
    /// the rows so far
    rows: u64,
    windows: Vec<WindowState>,
    /// the rows held back, the first of them row `delayed_from`
    delayed: VecDeque<Delayed>,
    delayed_from: u64,
    finished: bool,
}

impl PhysicalStreamingWindow {
    fn initial_state(&self) -> State {
        use WindowFunction::*;
        let windows = self
            .windows
            .iter()
            .map(|window| match window.function {
                RowNumber => WindowState::RowNumber,
                Rank | DenseRank => WindowState::Constant(Value::bigint(1)),
                PercentRank => WindowState::Constant(Value::double(0.0)),
                Aggregate => WindowState::Aggregate {
                    state: window
                        .aggregate
                        .as_ref()
                        .expect("an aggregate window has a function")
                        .kind
                        .initial_state(),
                    seen: HashSet::new(),
                },
                Lag | Lead => {
                    let offset = lead_lag_offset(window).expect("checked when planned");
                    let default = lead_lag_default(window).expect("checked when planned");
                    if offset < 0 {
                        WindowState::Lead {
                            offset: offset.unsigned_abs() as usize,
                            default,
                        }
                    } else {
                        WindowState::Lag {
                            offset: offset as usize,
                            default,
                            history: VecDeque::new(),
                        }
                    }
                }
                FirstValue => WindowState::First(None),
                LastValue => WindowState::Last(Value::null(window.return_type.clone())),
                NthValue => WindowState::Nth {
                    index: nth_index(window).expect("checked when planned"),
                    count: 0,
                    value: Value::null(window.return_type.clone()),
                },
                CumeDist | Ntile | Fill => unreachable!("not a streaming window"),
            })
            .collect();
        State {
            rows: 0,
            windows,
            delayed: VecDeque::new(),
            delayed_from: 0,
            finished: false,
        }
    }

    /// The results of the windows for the rows of `input`, a column per window; `None` for a
    /// LEAD, which [`Self::delay`] fills in.
    fn evaluate(&self, input: &DataChunk, state: &mut State) -> Result<Vec<Vec<Option<Value>>>> {
        let count = input.len();
        let mut columns = Vec::with_capacity(self.windows.len());
        for (window, window_state) in self.windows.iter().zip(&mut state.windows) {
            let argument = match window.children.first() {
                Some(child) if !matches!(window.function, WindowFunction::RowNumber) => Some(execute(child, input)?),
                _ => None,
            };
            let values = |argument: &Option<Vector>| -> Vec<Value> {
                let argument = argument.as_ref().expect("the function has an argument");
                (0..count).map(|row| argument.get_value(row)).collect()
            };
            let mut column = Vec::with_capacity(count);
            match window_state {
                WindowState::RowNumber => {
                    for row in 0..count {
                        column.push(Some(Value::bigint((state.rows + row as u64 + 1) as i64)));
                    }
                }
                WindowState::Constant(value) => column.resize(count, Some(value.clone())),
                WindowState::Aggregate { state: aggregate, seen } => {
                    let function = window.aggregate.as_ref().expect("an aggregate window has a function");
                    let filtered: Vec<bool> = match &window.filter {
                        Some(filter) => {
                            let filter = execute(filter, input)?;
                            (0..count)
                                .map(|row| filter.get_value(row) == Value::boolean(true))
                                .collect()
                        }
                        None => vec![true; count],
                    };
                    let compares = window.distinct || matches!(function.kind, AggregateKind::Min | AggregateKind::Max);
                    let keys = match &argument {
                        Some(argument) if compares => Some(value_keys(argument, count)?),
                        _ => None,
                    };
                    let input_type = window.children.first().map(|c| &c.return_type);
                    let mut states = [std::mem::replace(aggregate, function.kind.initial_state())];
                    for row in 0..count {
                        let mut add = filtered[row];
                        if add
                            && window.distinct
                            && let (Some(argument), Some(keys)) = (&argument, &keys)
                        {
                            add = !argument.get_value(row).is_null() && seen.insert(keys[row].clone());
                        }
                        if add {
                            function.update_keyed(
                                argument.as_ref(),
                                count,
                                &[(row, 0)],
                                &mut states,
                                keys.as_deref(),
                            )?;
                        }
                        column.push(Some(function.finalize(&states[0], &window.return_type, input_type)?));
                    }
                    *aggregate = std::mem::replace(&mut states[0], function.kind.initial_state());
                }
                WindowState::Lag {
                    offset,
                    default,
                    history,
                } => {
                    for value in values(&argument) {
                        if *offset == 0 {
                            column.push(Some(value));
                            continue;
                        }
                        column.push(Some(if history.len() == *offset {
                            history.front().expect("full").clone()
                        } else {
                            default.clone()
                        }));
                        if history.len() == *offset {
                            history.pop_front();
                        }
                        history.push_back(value);
                    }
                }
                WindowState::Lead { .. } => column.resize(count, None),
                WindowState::First(first) => {
                    for value in values(&argument) {
                        if first.is_none() && !(window.ignore_nulls && value.is_null()) {
                            *first = Some(value);
                        }
                        column.push(Some(
                            first.clone().unwrap_or_else(|| Value::null(window.return_type.clone())),
                        ));
                    }
                }
                WindowState::Last(last) => {
                    for value in values(&argument) {
                        if !(window.ignore_nulls && value.is_null()) {
                            *last = value;
                        }
                        column.push(Some(last.clone()));
                    }
                }
                WindowState::Nth {
                    index,
                    count: seen,
                    value: nth,
                } => {
                    for value in values(&argument) {
                        if *seen < *index && !(window.ignore_nulls && value.is_null()) {
                            *seen += 1;
                            if *seen == *index {
                                *nth = value;
                            }
                        }
                        column.push(Some(nth.clone()));
                    }
                }
            }
            columns.push(column);
        }
        Ok(columns)
    }

    /// Holds the rows of `input` back, and gives the LEADs of the rows before them what they read.
    fn delay(&self, input: &DataChunk, results: Vec<Vec<Option<Value>>>, state: &mut State) -> Result<()> {
        let count = input.len();
        let mut leads = Vec::new();
        for (at, (window, window_state)) in self.windows.iter().zip(&state.windows).enumerate() {
            if let WindowState::Lead { offset, .. } = window_state {
                let argument = execute(&window.children[0], input)?;
                leads.push((at, *offset, argument));
            }
        }
        let mut results: Vec<std::vec::IntoIter<Option<Value>>> = results.into_iter().map(Vec::into_iter).collect();
        for row in 0..count {
            let values = (0..input.column_count()).map(|c| input.get_value(c, row)).collect();
            let row_results = results
                .iter_mut()
                .map(|r| r.next().expect("a result per row"))
                .collect();
            state.delayed.push_back(Delayed {
                values,
                results: row_results,
            });
            let global = state.rows + row as u64;
            for (at, offset, argument) in &leads {
                // this row is what the row `offset` before it reads
                if let Some(target) = global.checked_sub(*offset as u64)
                    && target >= state.delayed_from
                {
                    let delayed = &mut state.delayed[(target - state.delayed_from) as usize];
                    delayed.results[*at] = Some(argument.get_value(row));
                }
            }
        }
        Ok(())
    }

    /// The rows held back whose windows all have their results, at most a chunk of them.
    fn release(&self, state: &mut State) -> Result<DataChunk> {
        let mut chunk = DataChunk::new(&self.types);
        while chunk.len() < STANDARD_VECTOR_SIZE
            && let Some(front) = state.delayed.front()
            && front.results.iter().all(Option::is_some)
        {
            let delayed = state.delayed.pop_front().expect("just looked");
            state.delayed_from += 1;
            let mut values = delayed.values;
            values.extend(delayed.results.into_iter().map(|r| r.expect("all there")));
            chunk.append_row(&values)?;
        }
        Ok(chunk)
    }
}

impl PhysicalOperator for PhysicalStreamingWindow {
    fn name(&self) -> &'static str {
        "STREAMING_WINDOW"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn operator_state(&self) -> OperatorState {
        Box::new(self.initial_state())
    }
    fn execute_stateful(&self, input: &DataChunk, state: &mut OperatorState) -> Result<(DataChunk, OperatorResult)> {
        let state = state.downcast_mut::<State>().expect("a streaming window state");
        let count = input.len();
        let results = self.evaluate(input, state)?;
        if !self.delays {
            state.rows += count as u64;
            let mut columns = input.columns().to_vec();
            for (window, values) in self.windows.iter().zip(results) {
                let mut vector = Vector::new(window.return_type.clone(), count.max(1));
                for (row, value) in values.into_iter().enumerate() {
                    vector.set_value(row, &value.expect("no window waits"))?;
                }
                columns.push(vector);
            }
            return Ok((DataChunk::from_columns(columns, count), OperatorResult::NeedMoreInput));
        }
        self.delay(input, results, state)?;
        state.rows += count as u64;
        Ok((self.release(state)?, OperatorResult::NeedMoreInput))
    }
    /// `FinalExecute`: the LEADs that read past the last row get their default.
    fn final_execute(&self, state: &mut OperatorState) -> Result<Option<DataChunk>> {
        let state = state.downcast_mut::<State>().expect("a streaming window state");
        if !self.delays {
            return Ok(None);
        }
        if !state.finished {
            state.finished = true;
            for (at, window_state) in state.windows.iter().enumerate() {
                if let WindowState::Lead { default, .. } = window_state {
                    for delayed in &mut state.delayed {
                        delayed.results[at].get_or_insert_with(|| default.clone());
                    }
                }
            }
        }
        let chunk = self.release(state)?;
        Ok((!chunk.is_empty()).then_some(chunk))
    }
}
