// Ported from duckdb/src/function/window/match_recognize_matcher.cpp and the semantics of
// duckdb/src/function/window/window_match_recognize.cpp @ rust-port-baseline
//
//! MATCH_RECOGNIZE. C++ plans it as a window function that reports the matches each row takes part in,
//! unnests them and computes the measures with window aggregates over the match frames. Here one
//! operator does all of that: it gathers its input, which the binder made the rows of the input with
//! every value a condition or a measure reads at a row computed alongside (the "slots" read them), sorts
//! it by partition and order, walks the pattern over each partition, and emits what ONE ROW or ALL ROWS
//! PER MATCH asks for.
//!
//! The pattern compiles to a small program that a depth-first search walks in the order of preference
//! the standard asks for, with C++'s memoisation (`PatternMemo`).

use super::expression_executor::execute;
use super::physical::{PhysicalOperator, SourceState, scan_collection};
use crate::function::aggregates::{AggregateFunction, value_keys};
use crate::planner::expression::BoundExpr;
use pintail_common::column_data::ColumnDataCollection;
use pintail_common::sort_key::{OrderModifiers, create_sort_keys};
use pintail_common::types::LogicalType;
use pintail_common::value::Value;
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use pintail_parser::ast::{AfterMatchSkip, RowsPerMatch};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

/// `MatchRecognizePattern`: the pattern, its variables by their index.
#[derive(Debug, Clone)]
pub enum PatternNode {
    Symbol(usize),
    /// `$` (true) or `^`
    Anchor(bool),
    Concatenation(Vec<PatternNode>),
    Alternation(Box<PatternNode>, Box<PatternNode>),
    Quantifier {
        child: Box<PatternNode>,
        min: u64,
        max: Option<u64>,
        reluctant: bool,
        excluded: bool,
    },
}

/// Which row of the partition a slot reads: one the match mapped to some variables (`FIRST`/`LAST`, or a
/// plain `A.x`, which is `LAST(A.x)`), or the row the expression is at, then rows before or after it
/// (`PREV`/`NEXT`).
#[derive(Debug, Clone)]
pub struct Navigation {
    /// the variables (none for all rows of the match), whether it is FIRST, and how many rows in
    pub logical: Option<(Vec<usize>, bool, usize)>,
    /// negative for PREV, positive for NEXT
    pub physical: i64,
}

#[derive(Debug, Clone)]
pub enum SlotKind {
    /// the value of a column of the input at the row the navigation reaches
    Column {
        navigation: Navigation,
        column: usize,
    },
    /// `CLASSIFIER()` of the row the navigation reaches
    Classifier {
        navigation: Navigation,
    },
    MatchNumber,
    /// an aggregate over the rows the match mapped to the variables (all of them for none), of a column
    /// of the input (none for `count(*)`)
    Aggregate {
        symbols: Vec<usize>,
        function: Arc<AggregateFunction>,
        return_type: LogicalType,
        operand: Option<AggregateOperand>,
        distinct: bool,
        /// the order the aggregate sees the rows in, by columns of the input
        order: Vec<(usize, OrderModifiers)>,
        /// which of the rows it sees
        filter: Option<Box<MatchExpression>>,
    },
    /// the value of an earlier measure, at the same row
    Measure(usize),
}

/// What an aggregate folds of each row: a column of the input, or an expression that reads the row's classifier.
#[derive(Debug, Clone)]
pub enum AggregateOperand {
    Column(usize),
    Expression(Box<MatchExpression>),
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub kind: SlotKind,
    pub ty: LogicalType,
    /// RUNNING (true) or FINAL (false) where the query says which
    pub running: Option<bool>,
}

/// A condition or a measure: an expression over the columns of the input, then the slots.
#[derive(Debug, Clone)]
pub struct MatchExpression {
    pub expr: BoundExpr,
    pub slots: Vec<Slot>,
}

impl MatchExpression {
    /// Whether what it says of a row depends on anything but that row and those next to it.
    fn reads_match(&self) -> bool {
        self.slots.iter().any(|slot| match &slot.kind {
            SlotKind::Column { navigation, .. } => navigation.logical.is_some(),
            _ => true,
        })
    }

    /// `RequiredMemo`'s reasons: whether it reads rows the match mapped to particular variables.
    fn reads_history(&self) -> bool {
        self.slots.iter().any(|slot| match &slot.kind {
            SlotKind::Column { navigation, .. } => navigation.logical.as_ref().is_some_and(|(s, _, _)| !s.is_empty()),
            SlotKind::Classifier { .. } => true,
            SlotKind::Aggregate { symbols, .. } => !symbols.is_empty(),
            _ => false,
        })
    }
}

/// One column of the output.
#[derive(Debug, Clone)]
pub enum OutputColumn {
    /// a column of the input, at the row
    Input(usize),
    Measure(usize),
}

/// What the binder settled.
#[derive(Debug)]
pub struct MatchRecognizeData {
    /// the columns of the input
    pub input_types: Vec<LogicalType>,
    pub partitions: Vec<usize>,
    pub orders: Vec<(usize, OrderModifiers)>,
    /// the variables, as CLASSIFIER() names them
    pub symbols: Vec<String>,
    /// by variable, its DEFINE (none: any row)
    pub defines: Vec<Option<MatchExpression>>,
    pub pattern: PatternNode,
    pub measures: Vec<MatchExpression>,
    pub rows_per_match: RowsPerMatch,
    pub after_match: AfterMatchSkip,
    /// the variables AFTER MATCH SKIP TO FIRST/LAST resumes at, and its name
    pub skip_symbols: Vec<usize>,
    pub skip_name: String,
    pub output: Vec<OutputColumn>,
    pub output_types: Vec<LogicalType>,
}

impl MatchRecognizeData {
    fn all_rows(&self) -> bool {
        !matches!(self.rows_per_match, RowsPerMatch::Default | RowsPerMatch::One)
    }
}

// ---------------------------------------------------------------------------------------------------
// The pattern program (`PatternProgram`)
// ---------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Symbol,
    Split,
    Jump,
    Anchor,
    Match,
}

#[derive(Debug, Clone, Copy)]
struct Instruction {
    op: Op,
    symbol: usize,
    excluded: bool,
    target: usize,
    alternative: usize,
    at_end: bool,
}

impl Instruction {
    fn new(op: Op) -> Self {
        Self {
            op,
            symbol: 0,
            excluded: false,
            target: 0,
            alternative: 0,
            at_end: false,
        }
    }
}

/// `PatternProgram::MAX_INSTRUCTIONS`
const MAX_INSTRUCTIONS: usize = 1 << 20;

struct Program {
    code: Vec<Instruction>,
}

/// `MinConsumption`: the fewest rows a part of the pattern takes.
fn min_consumption(node: &PatternNode) -> u64 {
    match node {
        PatternNode::Symbol(_) => 1,
        PatternNode::Anchor(_) => 0,
        PatternNode::Concatenation(children) => children.iter().fold(0u64, |a, c| a.saturating_add(min_consumption(c))),
        PatternNode::Alternation(left, right) => min_consumption(left).min(min_consumption(right)),
        PatternNode::Quantifier { child, min, .. } => min_consumption(child).saturating_mul(*min),
    }
}

impl Program {
    fn compile(pattern: &PatternNode, limit: u64) -> Result<Self> {
        let mut program = Self { code: Vec::new() };
        program.compile_node(pattern, limit, false)?;
        program.push(Instruction::new(Op::Match))?;
        Ok(program)
    }

    fn push(&mut self, instruction: Instruction) -> Result<usize> {
        if self.code.len() >= MAX_INSTRUCTIONS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "The MATCH_RECOGNIZE pattern compiles to more than {MAX_INSTRUCTIONS} instructions, which is more \
                     than can be matched. Repetition counts multiply, so nesting them is what usually gets here."
                ),
            ));
        }
        self.code.push(instruction);
        Ok(self.code.len() - 1)
    }

    fn compile_node(&mut self, node: &PatternNode, limit: u64, excluded: bool) -> Result<()> {
        match node {
            PatternNode::Symbol(symbol) => {
                let mut instruction = Instruction::new(Op::Symbol);
                instruction.symbol = *symbol;
                instruction.excluded = excluded;
                self.push(instruction)?;
            }
            PatternNode::Anchor(at_end) => {
                let mut instruction = Instruction::new(Op::Anchor);
                instruction.at_end = *at_end;
                self.push(instruction)?;
            }
            PatternNode::Concatenation(children) => {
                for child in children {
                    self.compile_node(child, limit, excluded)?;
                }
            }
            PatternNode::Alternation(left, right) => {
                let split = self.push(Instruction::new(Op::Split))?;
                self.code[split].target = self.code.len();
                self.compile_node(left, limit, excluded)?;
                let jump = self.push(Instruction::new(Op::Jump))?;
                self.code[split].alternative = self.code.len();
                self.compile_node(right, limit, excluded)?;
                self.code[jump].target = self.code.len();
            }
            PatternNode::Quantifier {
                child,
                min,
                max,
                reluctant,
                excluded: own,
            } => {
                let inner = excluded || *own;
                // one repetition past what the rows allow is already unsatisfiable, as every further one is
                let consumption = min_consumption(child);
                let reachable = limit.checked_div(consumption).map_or(limit + 1, |count| count + 1);
                let min_count = (*min).min(reachable);
                for _ in 0..min_count {
                    self.compile_node(child, limit, inner)?;
                }
                match max {
                    None => {
                        let looped = self.code.len();
                        let split = self.push(Instruction::new(Op::Split))?;
                        let again = self.code.len();
                        self.compile_node(child, limit, inner)?;
                        let jump = self.push(Instruction::new(Op::Jump))?;
                        self.code[jump].target = looped;
                        let leave = self.code.len();
                        self.code[split].target = if *reluctant { leave } else { again };
                        self.code[split].alternative = if *reluctant { again } else { leave };
                    }
                    Some(max) => {
                        let max_count = (*max).min(min_count.saturating_add(reachable));
                        let mut exits = Vec::new();
                        for _ in min_count..max_count {
                            let split = self.push(Instruction::new(Op::Split))?;
                            let again = self.code.len();
                            if *reluctant {
                                self.code[split].alternative = again;
                            } else {
                                self.code[split].target = again;
                            }
                            exits.push(split);
                            self.compile_node(child, limit, inner)?;
                        }
                        let end = self.code.len();
                        for exit in exits {
                            if *reluctant {
                                self.code[exit].target = end;
                            } else {
                                self.code[exit].alternative = end;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------
// The matcher (`PatternMatcher`)
// ---------------------------------------------------------------------------------------------------

/// `PatternMemo`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Memo {
    Partition,
    Attempt,
    History,
}

struct Pending {
    pc: usize,
    offset: usize,
    scope: usize,
    trail: usize,
}

/// Whether the row at an offset can be the variable, given the classifiers so far.
type SymbolTest<'t> = dyn FnMut(usize, usize, &[usize]) -> Result<bool> + 't;

struct Matcher<'a> {
    program: &'a Program,
    memo: Memo,
    rows: usize,
    explored: Vec<u8>,
    epoch: u8,
    attempt_marks: Vec<usize>,
    history_marks: Vec<usize>,
    history_scope: usize,
    history_trail: Vec<(usize, usize)>,
    pending: Vec<Pending>,
    match_end: usize,
}

impl<'a> Matcher<'a> {
    fn new(program: &'a Program, memo: Memo, rows: usize) -> Self {
        let explored = if memo == Memo::History {
            Vec::new()
        } else {
            vec![0; program.code.len() * (rows + 1)]
        };
        Self {
            program,
            memo,
            rows,
            explored,
            epoch: 0,
            attempt_marks: Vec::new(),
            history_marks: if memo == Memo::History {
                vec![0; program.code.len()]
            } else {
                Vec::new()
            },
            history_scope: 0,
            history_trail: Vec::new(),
            pending: Vec::new(),
            match_end: 0,
        }
    }

    fn next_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.explored.iter_mut().for_each(|e| *e = 0);
            self.epoch = 1;
        }
    }

    fn begin_partition(&mut self) {
        if self.memo != Memo::History {
            self.next_epoch();
        }
    }

    fn next_scope(&mut self) -> usize {
        if self.memo == Memo::History {
            self.history_scope += 1;
            self.history_scope
        } else {
            0
        }
    }

    fn unwind(&mut self, trail: usize) {
        while self.history_trail.len() > trail {
            let (pc, scope) = self.history_trail.pop().expect("longer than the trail");
            self.history_marks[pc] = scope;
        }
    }

    fn visit(&mut self, pc: usize, offset: usize, walk: usize) -> bool {
        if self.memo == Memo::History {
            let mark = &mut self.history_marks[pc];
            if *mark == walk {
                return false;
            }
            self.history_trail.push((pc, *mark));
            *mark = walk;
            return true;
        }
        let slot = pc * (self.rows + 1) + offset;
        if self.explored[slot] == self.epoch {
            return false;
        }
        self.explored[slot] = self.epoch;
        if self.memo == Memo::Partition {
            self.attempt_marks.push(slot);
        }
        true
    }

    /// `Match`: the first way through the pattern from `start`, within [`partition_start`, `end`).
    fn matches(
        &mut self,
        start: usize,
        partition_start: usize,
        end: usize,
        classifiers: &mut [usize],
        excluded: &mut [bool],
        test: &mut SymbolTest<'_>,
    ) -> Result<bool> {
        if self.memo == Memo::Attempt {
            self.next_epoch();
        }
        self.attempt_marks.clear();
        self.pending.clear();
        self.unwind(0);
        let scope = self.next_scope();
        self.pending.push(Pending {
            pc: 0,
            offset: start,
            scope,
            trail: 0,
        });
        while let Some(state) = self.pending.pop() {
            let (mut pc, mut offset) = (state.pc, state.offset);
            self.unwind(state.trail);
            let mut walk = state.scope;
            loop {
                if !self.visit(pc, offset, walk) {
                    break;
                }
                let instruction = self.program.code[pc];
                match instruction.op {
                    Op::Match => {
                        // a record only proves a dead end once its subtree was searched to exhaustion
                        for &mark in &self.attempt_marks {
                            self.explored[mark] = 0;
                        }
                        self.match_end = offset;
                        return Ok(true);
                    }
                    Op::Jump => {
                        pc = instruction.target;
                        continue;
                    }
                    Op::Split => {
                        self.pending.push(Pending {
                            pc: instruction.alternative,
                            offset,
                            scope: walk,
                            trail: self.history_trail.len(),
                        });
                        pc = instruction.target;
                        continue;
                    }
                    Op::Anchor => {
                        if offset != if instruction.at_end { end } else { partition_start } {
                            break;
                        }
                        pc += 1;
                        continue;
                    }
                    Op::Symbol => {}
                }
                if offset >= end {
                    break;
                }
                // the row is tentatively this variable while its condition runs, so LAST(X.c) sees it
                classifiers[offset] = instruction.symbol;
                if !test(instruction.symbol, offset, classifiers)? {
                    break;
                }
                excluded[offset] = instruction.excluded;
                pc += 1;
                offset += 1;
                walk = self.next_scope();
            }
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------------

/// The rows of one partition, in order, by column.
struct Partition<'a> {
    columns: &'a [Vector],
    /// the rows of the input in order, of which this partition is [start, end)
    order: &'a [usize],
    start: usize,
    end: usize,
}

/// Where an expression is evaluated: a match [start, start + length) of the partition, and the row it is at.
struct Frame<'a> {
    start: usize,
    /// the rows of the match the expression sees: up to the row it is at (RUNNING) or all of them
    running_end: usize,
    final_end: usize,
    /// the row it is at
    row: usize,
    classifiers: &'a [usize],
    match_number: i64,
}

impl Partition<'_> {
    fn value(&self, column: usize, row: usize) -> Value {
        self.columns[column].get_value(self.order[row])
    }

    /// The rows of the match that `symbols` (any for none) cover, up to `end` (exclusive).
    fn rows_of(symbols: &[usize], frame: &Frame<'_>, end: usize) -> Vec<usize> {
        (frame.start..end)
            .filter(|row| symbols.is_empty() || symbols.contains(&frame.classifiers[*row]))
            .collect()
    }

    /// The row a navigation reaches, if it is in the partition.
    fn navigate(&self, navigation: &Navigation, frame: &Frame<'_>, running: bool) -> Option<usize> {
        let end = if running { frame.running_end } else { frame.final_end };
        let base = match &navigation.logical {
            None => {
                if running {
                    frame.row
                } else {
                    end.checked_sub(1)
                        .filter(|row| *row >= frame.start)
                        .unwrap_or(frame.row)
                }
            }
            Some((symbols, first, offset)) => {
                let rows = Self::rows_of(symbols, frame, end);
                if *first {
                    *rows.get(*offset)?
                } else {
                    *rows.get(rows.len().checked_sub(offset + 1)?)?
                }
            }
        };
        let row = base as i64 + navigation.physical;
        (row >= self.start as i64 && row < self.end as i64).then_some(row as usize)
    }

    fn slot_value(
        &self,
        data: &MatchRecognizeData,
        slot: &Slot,
        frame: &Frame<'_>,
        default_running: bool,
        measures: &[Value],
    ) -> Result<Value> {
        let running = slot.running.unwrap_or(default_running);
        Ok(match &slot.kind {
            SlotKind::Column { navigation, column } => match self.navigate(navigation, frame, running) {
                Some(row) => self.value(*column, row),
                None => Value::null(slot.ty.clone()),
            },
            SlotKind::Classifier { navigation } => {
                // what any row of the match was mapped to is known, whichever of them the navigation reaches
                match self.navigate(navigation, frame, running) {
                    Some(row) if row >= frame.start && row < frame.final_end => {
                        Value::varchar(&data.symbols[frame.classifiers[row]])
                    }
                    _ => Value::null(LogicalType::VARCHAR),
                }
            }
            SlotKind::MatchNumber => Value::bigint(frame.match_number),
            SlotKind::Aggregate {
                symbols,
                function,
                return_type,
                operand,
                distinct,
                order,
                filter,
            } => {
                let end = if running { frame.running_end } else { frame.final_end };
                let mut rows = Self::rows_of(symbols, frame, end);
                let at_row = |row: usize| Frame {
                    start: frame.start,
                    running_end: end,
                    final_end: end,
                    row,
                    classifiers: frame.classifiers,
                    match_number: frame.match_number,
                };
                if let Some(filter) = filter {
                    let mut kept = Vec::with_capacity(rows.len());
                    for row in rows {
                        let value = self.evaluate(data, filter, &at_row(row), true, measures)?;
                        if value.data() == &pintail_common::value::ValueData::Bool(true) {
                            kept.push(row);
                        }
                    }
                    rows = kept;
                }
                if !order.is_empty() && rows.len() > 1 {
                    let mut keyed = Vec::with_capacity(order.len());
                    for (column, _) in order {
                        let ty = data.input_types[*column].clone();
                        let mut values = Vector::new(ty, rows.len());
                        for (at, row) in rows.iter().enumerate() {
                            values.set_value(at, &self.value(*column, *row))?;
                        }
                        keyed.push(values);
                    }
                    let columns: Vec<(&Vector, OrderModifiers)> = keyed
                        .iter()
                        .zip(order)
                        .map(|(values, (_, modifiers))| (values, *modifiers))
                        .collect();
                    let keys = create_sort_keys(&columns, rows.len())?;
                    let mut positions: Vec<usize> = (0..rows.len()).collect();
                    positions.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
                    rows = positions.into_iter().map(|at| rows[at]).collect();
                }
                let mut state = [function.kind.initial_state()];
                match operand {
                    Some(operand) => {
                        let ty = match operand {
                            AggregateOperand::Column(column) => data.input_types[*column].clone(),
                            AggregateOperand::Expression(expr) => expr.expr.return_type.clone(),
                        };
                        let mut input = Vector::new(ty.clone(), rows.len().max(1));
                        for (at, row) in rows.iter().enumerate() {
                            let value = match operand {
                                AggregateOperand::Column(column) => self.value(*column, *row),
                                AggregateOperand::Expression(expr) => {
                                    self.evaluate(data, expr, &at_row(*row), true, measures)?
                                }
                            };
                            input.set_value(at, &value)?;
                        }
                        let mut picked: Vec<(usize, usize)> = (0..rows.len()).map(|at| (at, 0)).collect();
                        if *distinct {
                            let keys = value_keys(&input, rows.len())?;
                            let mut seen = HashSet::new();
                            picked.retain(|(at, _)| seen.insert(keys[*at].clone()));
                        }
                        function.update(Some(&input), rows.len(), &picked, &mut state)?;
                        function.finalize(&state[0], return_type, Some(&ty))?
                    }
                    None => {
                        let picked: Vec<(usize, usize)> = (0..rows.len()).map(|at| (at, 0)).collect();
                        function.update(None, rows.len(), &picked, &mut state)?;
                        function.finalize(&state[0], return_type, None)?
                    }
                }
            }
            SlotKind::Measure(index) => measures[*index].clone(),
        })
    }

    /// The value of `expr` in `frame`.
    fn evaluate(
        &self,
        data: &MatchRecognizeData,
        expr: &MatchExpression,
        frame: &Frame<'_>,
        default_running: bool,
        measures: &[Value],
    ) -> Result<Value> {
        let mut columns = Vec::with_capacity(data.input_types.len() + expr.slots.len());
        let row = (frame.row < self.end).then_some(frame.row);
        for (column, ty) in data.input_types.iter().enumerate() {
            let value = match row {
                Some(row) => self.value(column, row),
                None => Value::null(ty.clone()),
            };
            columns.push(Vector::constant(&value));
        }
        for slot in &expr.slots {
            columns.push(Vector::constant(&self.slot_value(
                data,
                slot,
                frame,
                default_running,
                measures,
            )?));
        }
        let chunk = DataChunk::from_columns(columns, 1);
        Ok(execute(&expr.expr, &chunk)?.get_value(0))
    }
}

/// `SkipTo`: where the scan resumes after a match of rows [start, end].
fn skip_to(data: &MatchRecognizeData, start: usize, end: usize, classifiers: &[usize]) -> Result<usize> {
    let mut resume = end + 1;
    match data.after_match {
        AfterMatchSkip::NextRow => resume = start + 1,
        AfterMatchSkip::ToFirst | AfterMatchSkip::ToLast => {
            let first = data.after_match == AfterMatchSkip::ToFirst;
            let target = (0..=end - start)
                .map(|step| if first { start + step } else { end - step })
                .find(|row| data.skip_symbols.contains(&classifiers[*row]));
            let name = &data.skip_name;
            let Some(target) = target else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "AFTER MATCH SKIP TO {name} found a match with no row matched to {name}, so there is nowhere \
                         to resume from"
                    ),
                ));
            };
            if target == start {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "AFTER MATCH SKIP TO {name} resumes at the row the match started on, so matching cannot advance"
                    ),
                ));
            }
            resume = target;
        }
        AfterMatchSkip::PastLastRow | AfterMatchSkip::Default => {}
    }
    Ok(resume.max(start + 1))
}

/// A match found: [start, end) with what each row was matched to.
struct Found {
    start: usize,
    number: i64,
    classifiers: Vec<usize>,
    excluded: Vec<bool>,
}

fn memo_of(data: &MatchRecognizeData) -> Memo {
    let defines = data.defines.iter().flatten();
    if defines.clone().any(MatchExpression::reads_history) {
        Memo::History
    } else if defines.clone().any(MatchExpression::reads_match) {
        Memo::Attempt
    } else {
        Memo::Partition
    }
}

/// The record of what the matcher explored, counted against the memory limit for as long as it is kept.
struct MemoReservation {
    memory: Arc<super::temporary::QueryMemory>,
    bytes: usize,
}

impl MemoReservation {
    fn new(program: &Program, memo: Memo, rows: usize) -> Result<Self> {
        let memory = super::temporary::current();
        if memo == Memo::History {
            return Ok(Self { memory, bytes: 0 });
        }
        let Some(bytes) = program.code.len().checked_mul(rows + 1) else {
            return Err(Error::new(
                ErrorKind::OutOfMemory,
                format!(
                    "The MATCH_RECOGNIZE pattern needs a record per instruction per row, which does not fit in memory \
                     for {} instructions over {rows} rows",
                    program.code.len()
                ),
            ));
        };
        if !memory.try_reserve(bytes) {
            return Err(Error::new(
                ErrorKind::OutOfMemory,
                format!(
                    "could not allocate block of size {} ({} maximum)",
                    super::temporary::format_bytes(bytes as u64),
                    super::temporary::format_bytes(memory.limit() as u64)
                ),
            ));
        }
        Ok(Self { memory, bytes })
    }
}

impl Drop for MemoReservation {
    fn drop(&mut self) {
        self.memory.release(self.bytes);
    }
}

/// The matches of one partition, in the order they were found.
fn find_matches(data: &MatchRecognizeData, partition: &Partition<'_>, program: &Program) -> Result<Vec<Found>> {
    let rows = partition.end;
    let mut classifiers = vec![usize::MAX; rows];
    let mut excluded = vec![false; rows];
    let memo = memo_of(data);
    let _record = MemoReservation::new(program, memo, rows)?;
    let mut matcher = Matcher::new(program, memo, rows);
    matcher.begin_partition();
    // a condition that reads only its row says the same each time it is asked
    let mut settled: Vec<Vec<Option<bool>>> = vec![vec![None; rows]; data.symbols.len()];
    let mut found = Vec::new();
    let mut number = 0i64;
    let mut row = partition.start;
    while row < partition.end {
        let attempt = number + 1;
        let start = row;
        let mut test = |symbol: usize, at: usize, classifiers: &[usize]| -> Result<bool> {
            let Some(define) = &data.defines[symbol] else {
                return Ok(true);
            };
            let local = !define.reads_match();
            if local && let Some(known) = settled[symbol][at] {
                return Ok(known);
            }
            let frame = Frame {
                start,
                running_end: at + 1,
                final_end: at + 1,
                row: at,
                classifiers,
                match_number: attempt,
            };
            let value = partition.evaluate(data, define, &frame, true, &[])?;
            let holds = value.data() == &pintail_common::value::ValueData::Bool(true);
            if local {
                settled[symbol][at] = Some(holds);
            }
            Ok(holds)
        };
        if !matcher.matches(
            row,
            partition.start,
            partition.end,
            &mut classifiers,
            &mut excluded,
            &mut test,
        )? {
            row += 1;
            continue;
        }
        number += 1;
        let end = matcher.match_end;
        found.push(Found {
            start: row,
            number,
            classifiers: classifiers[row..end.max(row)].to_vec(),
            excluded: excluded[row..end.max(row)].to_vec(),
        });
        // an empty match covers no rows: the scan steps past it
        if end <= row {
            row += 1;
            continue;
        }
        row = skip_to(data, row, end - 1, &classifiers)?;
    }
    Ok(found)
}

/// `PhysicalMatchRecognize`
#[derive(Debug)]
pub struct PhysicalMatchRecognize {
    types: Vec<LogicalType>,
    data: Arc<MatchRecognizeData>,
    children: Vec<Box<dyn PhysicalOperator>>,
    input: Mutex<ColumnDataCollection>,
    output: Mutex<ColumnDataCollection>,
}

impl PhysicalMatchRecognize {
    pub fn new(
        types: Vec<LogicalType>,
        data: Arc<MatchRecognizeData>,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        Self {
            input: Mutex::new(ColumnDataCollection::new(data.input_types.clone())),
            output: Mutex::new(ColumnDataCollection::new(types.clone())),
            types,
            data,
            children,
        }
    }

    /// The rows of one match, or of one row of it, of the output.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        partition: &Partition<'_>,
        found: &Found,
        classifiers: &[usize],
        at: Option<usize>,
        rows: &mut Vec<Vec<Value>>,
    ) -> Result<()> {
        let data = &*self.data;
        let all_rows = data.all_rows();
        let end = found.start + found.classifiers.len();
        // ALL ROWS: the row it is at, and the match up to there; ONE ROW: the match's last row
        let (row, running_end) = match at {
            // an empty match covers no rows, not even the one it is reported at
            Some(row) => (row, (row + 1).min(end)),
            None => (
                end.checked_sub(1).filter(|r| *r >= found.start).unwrap_or(found.start),
                end,
            ),
        };
        let frame = Frame {
            start: found.start,
            running_end,
            final_end: end,
            row,
            classifiers,
            match_number: found.number,
        };
        let mut measures = Vec::with_capacity(data.measures.len());
        for measure in &data.measures {
            let value = partition.evaluate(data, measure, &frame, all_rows, &measures)?;
            measures.push(value);
        }
        let mut values = Vec::with_capacity(data.output.len());
        for column in &data.output {
            values.push(match column {
                OutputColumn::Input(column) => partition.value(*column, row.min(partition.end - 1)),
                OutputColumn::Measure(index) => measures[*index].clone(),
            });
        }
        rows.push(values);
        Ok(())
    }

    /// A row of the input that no match covers, as WITH UNMATCHED ROWS reports it.
    fn emit_unmatched(&self, partition: &Partition<'_>, row: usize, rows: &mut Vec<Vec<Value>>) {
        let data = &*self.data;
        rows.push(
            data.output
                .iter()
                .zip(&data.output_types)
                .map(|(column, ty)| match column {
                    OutputColumn::Input(column) => partition.value(*column, row),
                    OutputColumn::Measure(_) => Value::null(ty.clone()),
                })
                .collect(),
        );
    }
}

impl PhysicalOperator for PhysicalMatchRecognize {
    fn name(&self) -> &'static str {
        "MATCH_RECOGNIZE"
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
        let data = &*self.data;
        let input = self.input.lock();
        let count = input.len();
        // the input by column, as one vector each
        let mut columns: Vec<Vector> = data
            .input_types
            .iter()
            .map(|ty| Vector::new(ty.clone(), count.max(1)))
            .collect();
        let mut at = 0;
        for chunk in input.chunks() {
            for row in 0..chunk.len() {
                for (column, vector) in columns.iter_mut().enumerate() {
                    vector.set_value(at, &chunk.column(column).get_value(row))?;
                }
                at += 1;
            }
        }
        drop(input);
        // partitions, then the order within them
        let partition_keys = if data.partitions.is_empty() {
            vec![Vec::new(); count]
        } else {
            let keyed: Vec<(&Vector, OrderModifiers)> = data
                .partitions
                .iter()
                .map(|c| (&columns[*c], OrderModifiers::ASC_NULLS_LAST))
                .collect();
            create_sort_keys(&keyed, count)?
        };
        let order_keys = if data.orders.is_empty() {
            vec![Vec::new(); count]
        } else {
            let keyed: Vec<(&Vector, OrderModifiers)> = data.orders.iter().map(|(c, m)| (&columns[*c], *m)).collect();
            create_sort_keys(&keyed, count)?
        };
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|a, b| (&partition_keys[*a], &order_keys[*a], a).cmp(&(&partition_keys[*b], &order_keys[*b], b)));
        let program = Program::compile(&data.pattern, count as u64)?;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut start = 0;
        while start < count {
            let mut end = start + 1;
            while end < count && partition_keys[order[end]] == partition_keys[order[start]] {
                end += 1;
            }
            // rows are numbered within their partition from here on
            let partition = Partition {
                columns: &columns,
                order: &order[start..end],
                start: 0,
                end: end - start,
            };
            let found = find_matches(data, &partition, &program)?;
            let mut covered = vec![false; partition.end];
            for found in &found {
                // what each row of this match was matched to, by row of the partition
                let mut classifiers = vec![usize::MAX; partition.end];
                classifiers[found.start..found.start + found.classifiers.len()].copy_from_slice(&found.classifiers);
                let empty = found.classifiers.is_empty();
                if !data.all_rows() {
                    self.emit(&partition, found, &classifiers, None, &mut rows)?;
                    continue;
                }
                if empty {
                    if data.rows_per_match != RowsPerMatch::AllOmitEmpty {
                        self.emit(&partition, found, &classifiers, Some(found.start), &mut rows)?;
                        covered[found.start] = true;
                    }
                    continue;
                }
                for (offset, excluded) in found.excluded.iter().enumerate() {
                    let row = found.start + offset;
                    covered[row] = true;
                    if !excluded {
                        self.emit(&partition, found, &classifiers, Some(row), &mut rows)?;
                    }
                }
            }
            if data.rows_per_match == RowsPerMatch::AllUnmatched {
                for (row, _) in covered
                    .iter()
                    .enumerate()
                    .take(partition.end)
                    .filter(|(_, covered)| !**covered)
                {
                    self.emit_unmatched(&partition, row, &mut rows);
                }
            }
            start = end;
        }
        let mut output = self.output.lock();
        for batch in rows.chunks(STANDARD_VECTOR_SIZE) {
            let mut chunk = DataChunk::new(&self.types);
            for values in batch {
                chunk.append_row(values)?;
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
