// Ported from duckdb/src/execution/operator/csv_scanner/state_machine/csv_state_machine_cache.cpp,
// scanner/base_scanner, scanner/column_count_scanner.cpp, scanner/skip_scanner.cpp and
// scanner/string_value_scanner.cpp @ rust-port-baseline
//
//! The CSV state machine and the scanners over it. A file is in memory as a whole, so C++'s buffers
//! and boundaries are gone, but states, transitions and the order of events are its own.

use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Standard = 0,
    Delimiter = 1,
    DelimiterFirstByte = 2,
    DelimiterSecondByte = 3,
    DelimiterThirdByte = 4,
    RecordSeparator = 5,
    CarriageReturn = 6,
    Quoted = 7,
    Unquoted = 8,
    Escape = 9,
    Invalid = 10,
    NotSet = 11,
    QuotedNewLine = 12,
    EmptySpace = 13,
    Comment = 14,
    StandardNewline = 15,
    UnquotedEscape = 16,
    EscapedReturn = 17,
    MaybeQuoted = 18,
}

const STATES: usize = 19;

/// `NewLineIdentifier`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewLine {
    /// `\n`
    SingleN,
    /// `\r`
    SingleR,
    /// `\r\n`
    CarryOn,
    /// nobody said, so any of them ends a line
    NotSet,
}

impl NewLine {
    /// `NewLineIdentifierToString`
    pub fn shown(self) -> &'static str {
        match self {
            NewLine::SingleN | NewLine::NotSet => "\\n",
            NewLine::SingleR => "\\r",
            NewLine::CarryOn => "\\r\\n",
        }
    }
}

/// `CSVSniffer::DetectNewLineDelimiter`
pub fn detect_new_line(data: &[u8]) -> NewLine {
    let (mut carriage_return, mut n) = (false, false);
    for &c in data {
        if c == b'\r' {
            carriage_return = true;
        } else if c == b'\n' {
            n = true;
            break;
        } else if carriage_return {
            break;
        }
    }
    match (carriage_return, n) {
        (true, true) => NewLine::CarryOn,
        (true, false) => NewLine::SingleR,
        _ => NewLine::SingleN,
    }
}

/// The dialect of a file (`DialectOptions` and `CSVStateMachineOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: Vec<u8>,
    pub quote: Option<u8>,
    pub escape: Option<u8>,
    pub comment: Option<u8>,
    pub new_line: NewLine,
    pub strict: bool,
}

/// `StateMachine`: the next state, by byte and state.
#[derive(Debug)]
pub struct Transitions {
    next: Vec<[State; STATES]>,
    skip_standard: [bool; 256],
    skip_quoted: [bool; 256],
    skip_comment: [bool; 256],
}

impl Transitions {
    /// `CSVStateMachineCache::Insert`
    pub fn new(dialect: &Dialect) -> Self {
        use State::*;
        let strict = dialect.strict;
        let new_line = dialect.new_line;
        let quote = dialect.quote.unwrap_or(0) as usize;
        let escape = dialect.escape.unwrap_or(0) as usize;
        let comment = dialect.comment.unwrap_or(0) as usize;
        let delimiter = &dialect.delimiter;
        let first = delimiter.first().copied().unwrap_or(0) as usize;
        let multi_byte = delimiter.len() > 1;
        let (lf, cr, space) = (b'\n' as usize, b'\r' as usize, b' ' as usize);
        let quote_is_escape = dialect.quote.unwrap_or(0) == dialect.escape.unwrap_or(0);
        let enable_unquoted_escape = !strict && !quote_is_escape && escape != 0;
        let delimiter_state = if multi_byte { DelimiterFirstByte } else { Delimiter };

        let mut base = [Standard; STATES];
        base[MaybeQuoted as usize] = Quoted;
        base[Quoted as usize] = Quoted;
        base[QuotedNewLine as usize] = Quoted;
        base[Escape as usize] = Quoted;
        base[Unquoted as usize] = if strict { Invalid } else { Unquoted };
        base[Comment as usize] = Comment;
        base[CarriageReturn as usize] = if strict { Invalid } else { Standard };
        let mut t = vec![base; 256];

        // 1) Standard/Invalid State
        for state in [Standard, Invalid, StandardNewline] {
            let s = state as usize;
            t[first][s] = delimiter_state;
            match new_line {
                NewLine::CarryOn => {
                    t[cr][s] = CarriageReturn;
                    t[lf][s] = if state == StandardNewline {
                        Standard
                    } else if !strict {
                        RecordSeparator
                    } else {
                        Invalid
                    };
                }
                NewLine::SingleN => {
                    t[lf][s] = RecordSeparator;
                    t[cr][s] = if strict { Invalid } else { RecordSeparator };
                }
                NewLine::SingleR => {
                    t[cr][s] = RecordSeparator;
                    t[lf][s] = if strict { Invalid } else { RecordSeparator };
                }
                NewLine::NotSet => {
                    t[cr][s] = RecordSeparator;
                    t[lf][s] = RecordSeparator;
                }
            }
            if comment != 0 {
                t[comment][s] = Comment;
            }
            if enable_unquoted_escape {
                t[escape][s] = UnquotedEscape;
            }
        }
        let carriage = if new_line == NewLine::CarryOn {
            CarriageReturn
        } else {
            RecordSeparator
        };
        // 2) Field Separator State
        if quote != 0 {
            t[quote][Delimiter as usize] = Quoted;
        }
        if first != space {
            t[space][Delimiter as usize] = EmptySpace;
        }
        for state in [Delimiter, DelimiterFirstByte, DelimiterSecondByte, DelimiterThirdByte] {
            let s = state as usize;
            t[first][s] = delimiter_state;
            t[lf][s] = RecordSeparator;
            t[cr][s] = carriage;
            if comment != 0 {
                t[comment][Delimiter as usize] = Comment;
            }
        }
        // the other bytes of a delimiter
        let byte = |i: usize| delimiter[i] as usize;
        match delimiter.len() {
            2 => t[byte(1)][DelimiterFirstByte as usize] = Delimiter,
            3 => {
                if first == byte(1) {
                    t[byte(1)][DelimiterSecondByte as usize] = DelimiterSecondByte;
                }
                t[byte(1)][DelimiterFirstByte as usize] = DelimiterSecondByte;
                t[byte(2)][DelimiterSecondByte as usize] = Delimiter;
            }
            4 => {
                if first == byte(2) {
                    t[byte(1)][DelimiterThirdByte as usize] = DelimiterSecondByte;
                }
                if first == byte(1) && byte(1) == byte(2) {
                    t[byte(1)][DelimiterThirdByte as usize] = DelimiterThirdByte;
                }
                t[byte(1)][DelimiterFirstByte as usize] = DelimiterSecondByte;
                t[byte(2)][DelimiterSecondByte as usize] = DelimiterThirdByte;
                t[byte(3)][DelimiterThirdByte as usize] = Delimiter;
            }
            _ => {}
        }
        if enable_unquoted_escape {
            t[escape][Delimiter as usize] = UnquotedEscape;
        }
        // 3) Record Separator State
        let s = RecordSeparator as usize;
        t[first][s] = delimiter_state;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        if quote != 0 {
            t[quote][s] = Quoted;
        }
        if first != space {
            t[space][s] = EmptySpace;
        }
        if comment != 0 {
            t[comment][s] = Comment;
        }
        if enable_unquoted_escape {
            t[escape][s] = UnquotedEscape;
        }
        if strict {
            match new_line {
                NewLine::CarryOn | NewLine::SingleR => t[lf][s] = Invalid,
                NewLine::SingleN => t[cr][s] = Invalid,
                NewLine::NotSet => {}
            }
        }
        // 4) Carriage Return State
        let s = CarriageReturn as usize;
        t[lf][s] = RecordSeparator;
        t[cr][s] = if strict { Invalid } else { CarriageReturn };
        if quote != 0 {
            t[quote][s] = Quoted;
        }
        if first != space {
            t[space][s] = EmptySpace;
        }
        if comment != 0 {
            t[comment][s] = Comment;
        }
        if enable_unquoted_escape {
            t[escape][s] = UnquotedEscape;
        }
        // 5) Quoted State
        let s = Quoted as usize;
        t[quote][s] = Unquoted;
        t[lf][s] = QuotedNewLine;
        t[cr][s] = QuotedNewLine;
        if !quote_is_escape && escape != 0 {
            t[escape][s] = Escape;
        }
        // 6) Unquoted State
        let s = Unquoted as usize;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        t[first][s] = delimiter_state;
        if quote_is_escape {
            t[quote][s] = Quoted;
        }
        if strict {
            match new_line {
                NewLine::CarryOn | NewLine::SingleR => t[lf][s] = Invalid,
                NewLine::SingleN => t[cr][s] = Invalid,
                NewLine::NotSet => {}
            }
        } else if escape == 0 {
            // if an escape is defined, it limits how relaxed quotes can be in a reliable way
            t[quote][s] = MaybeQuoted;
        } else {
            t[quote][s] = Quoted;
        }
        if comment != 0 {
            t[comment][s] = Comment;
        }
        if first != space && quote != space && escape != space && comment != space {
            t[space][s] = Unquoted;
        }
        // 8) Not Set
        let s = NotSet as usize;
        t[first][s] = delimiter_state;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        if quote != 0 {
            t[quote][s] = Quoted;
        }
        if first != space {
            t[space][s] = EmptySpace;
        }
        if comment != 0 {
            t[comment][s] = Comment;
        }
        if enable_unquoted_escape {
            t[escape][s] = UnquotedEscape;
        }
        // 9) Quoted NewLine
        let s = QuotedNewLine as usize;
        t[quote][s] = Unquoted;
        if !quote_is_escape && escape != 0 {
            t[escape][s] = Escape;
        }
        // 10) Empty Value State (Not first value)
        let s = EmptySpace as usize;
        t[first][s] = delimiter_state;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        if quote != 0 {
            t[quote][s] = Quoted;
        }
        if comment != 0 {
            t[comment][s] = Comment;
        }
        if enable_unquoted_escape {
            t[escape][s] = UnquotedEscape;
        }
        // 11) Comment State
        let s = Comment as usize;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        // 12) and 13) Unquoted Escape State and Escaped Return State
        if enable_unquoted_escape {
            if new_line == NewLine::CarryOn {
                t[cr][UnquotedEscape as usize] = EscapedReturn;
            }
            let s = EscapedReturn as usize;
            t[first][s] = Delimiter;
            t[cr][s] = carriage;
            if comment != 0 {
                t[comment][s] = Comment;
            }
            t[escape][s] = UnquotedEscape;
        }
        // 14) Maybe quoted
        let s = MaybeQuoted as usize;
        t[quote][s] = MaybeQuoted;
        t[lf][s] = RecordSeparator;
        t[cr][s] = carriage;
        t[first][s] = delimiter_state;

        let mut skip_standard = [true; 256];
        let mut skip_quoted = [true; 256];
        let mut skip_comment = [true; 256];
        skip_standard[first] = false;
        skip_standard[lf] = false;
        skip_standard[cr] = false;
        skip_standard[comment] = false;
        if enable_unquoted_escape {
            skip_standard[escape] = false;
        }
        skip_quoted[quote] = false;
        skip_quoted[escape] = false;
        skip_quoted[lf] = false;
        skip_quoted[cr] = false;
        skip_comment[cr] = false;
        skip_comment[lf] = false;
        Self {
            next: t,
            skip_standard,
            skip_quoted,
            skip_comment,
        }
    }
}

/// A scanner's position, and what it saw: `BaseScanner`, without what a result is made of.
#[derive(Debug, Clone)]
pub struct Machine {
    transitions: Arc<Transitions>,
    pub dialect: Dialect,
    pub position: usize,
    pub states: [State; 2],
    pub ever_quoted: bool,
    pub ever_escaped: bool,
    pub used_unstrictness: bool,
    /// the end of the data was handled
    pub finished: bool,
}

/// `ScannerResult`, what `BaseScanner::Process` tells a scanner. A method that returns true stops
/// the scan.
pub trait Sink {
    fn add_value(&mut self, machine: &Machine, data: &[u8], position: usize);
    fn add_row(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool;
    fn empty_line(&mut self, machine: &Machine, position: usize) -> bool;
    fn invalid_state(&mut self);
    fn is_comment_set(&self) -> bool;
    fn set_comment(&mut self, machine: &Machine, position: usize);
    fn unset_comment(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool;
    fn set_quoted(&mut self, _position: usize) {}
    fn set_unquoted(&mut self, _machine: &Machine) {}
    fn set_escaped(&mut self) {}
}

impl Machine {
    pub fn new(dialect: Dialect, position: usize) -> Self {
        let transitions = Arc::new(Transitions::new(&dialect));
        Self::with_transitions(dialect, transitions, position)
    }

    pub fn with_transitions(dialect: Dialect, transitions: Arc<Transitions>, position: usize) -> Self {
        Self {
            transitions,
            dialect,
            position,
            states: [State::NotSet; 2],
            ever_quoted: false,
            ever_escaped: false,
            used_unstrictness: false,
            finished: false,
        }
    }

    pub fn transitions(&self) -> Arc<Transitions> {
        Arc::clone(&self.transitions)
    }

    fn empty_line_state(&self) -> bool {
        matches!(self.states[1], State::CarriageReturn | State::RecordSeparator)
            && matches!(self.states[0], State::RecordSeparator | State::NotSet)
    }

    fn new_row(&self) -> bool {
        !matches!(self.states[0], State::RecordSeparator | State::CarriageReturn)
            && matches!(self.states[1], State::RecordSeparator | State::CarriageReturn)
    }

    fn current_new_row(&self) -> bool {
        matches!(self.states[1], State::RecordSeparator | State::CarriageReturn)
    }

    fn quoted_current(&self) -> bool {
        matches!(self.states[1], State::Quoted | State::QuotedNewLine)
    }

    /// `CSVStates::EmptyLastValue`
    pub fn empty_last_value(&self) -> bool {
        (self.states[0] == State::Delimiter
            && matches!(
                self.states[1],
                State::RecordSeparator | State::CarriageReturn | State::Delimiter
            ))
            || (self.states[0] == State::Standard && self.states[1] == State::Delimiter)
    }

    /// `BaseScanner::Process`. Ends at the end of the data, or when `sink` stops the scan.
    pub fn process<S: Sink>(&mut self, data: &[u8], sink: &mut S) {
        let has_escape = self.dialect.escape.is_some();
        let transitions = Arc::clone(&self.transitions);
        while self.position < data.len() {
            let byte = data[self.position];
            self.states[0] = self.states[1];
            self.states[1] = transitions.next[byte as usize][self.states[0] as usize];
            let position = self.position;
            match self.states[1] {
                State::Invalid => {
                    sink.invalid_state();
                    self.position += 1;
                    return;
                }
                State::RecordSeparator => {
                    let mut done = false;
                    if matches!(self.states[0], State::RecordSeparator | State::NotSet) {
                        done = sink.empty_line(self, position);
                    } else if self.states[0] != State::CarriageReturn {
                        done = if sink.is_comment_set() {
                            sink.unset_comment(self, data, position)
                        } else {
                            sink.add_row(self, data, position)
                        };
                    }
                    self.position += 1;
                    if done {
                        return;
                    }
                }
                State::CarriageReturn => {
                    let mut done = false;
                    if matches!(self.states[0], State::RecordSeparator | State::NotSet) {
                        done = sink.empty_line(self, position);
                    } else if self.states[0] != State::CarriageReturn {
                        done = if sink.is_comment_set() {
                            sink.unset_comment(self, data, position)
                        } else {
                            sink.add_row(self, data, position)
                        };
                    }
                    self.position += 1;
                    if done {
                        return;
                    }
                }
                State::Delimiter => {
                    sink.add_value(self, data, position);
                    self.position += 1;
                }
                State::Quoted => {
                    if matches!(self.states[0], State::Unquoted | State::MaybeQuoted) && has_escape {
                        self.ever_escaped = true;
                        sink.set_escaped();
                    }
                    if matches!(
                        self.states[0],
                        State::Escape | State::EscapedReturn | State::UnquotedEscape
                    ) && (Some(byte) == self.dialect.quote || !self.dialect.strict)
                    {
                        self.ever_escaped = true;
                        if self.states[0] == State::UnquotedEscape {
                            self.used_unstrictness = true;
                        }
                    }
                    self.ever_quoted = true;
                    sink.set_quoted(position);
                    self.position += 1;
                    while self.position < data.len() && transitions.skip_quoted[data[self.position] as usize] {
                        self.position += 1;
                    }
                }
                State::Unquoted => {
                    if self.states[0] == State::MaybeQuoted {
                        self.ever_escaped = true;
                        sink.set_escaped();
                    }
                    sink.set_unquoted(self);
                    self.position += 1;
                }
                State::Escape | State::EscapedReturn => {
                    sink.set_escaped();
                    self.position += 1;
                }
                State::UnquotedEscape => {
                    sink.set_escaped();
                    self.position += 1;
                    self.used_unstrictness = true;
                }
                State::Standard => {
                    self.position += 1;
                    while self.position < data.len() && transitions.skip_standard[data[self.position] as usize] {
                        self.position += 1;
                    }
                }
                State::Comment => {
                    sink.set_comment(self, position);
                    self.position += 1;
                    while self.position < data.len() && transitions.skip_comment[data[self.position] as usize] {
                        self.position += 1;
                    }
                }
                _ => self.position += 1,
            }
        }
    }
}

/// `ColumnCount`: what the sniffer knows of a row.
#[derive(Debug, Clone)]
pub struct ColumnCount {
    pub columns: usize,
    pub last_value_always_empty: bool,
    pub is_comment: bool,
    pub is_mid_comment: bool,
    pub empty_lines: usize,
}

/// `ColumnCountResult`
#[derive(Debug, Default)]
struct CountSink {
    rows: Vec<ColumnCount>,
    limit: usize,
    current: usize,
    empty_lines: usize,
    comment: bool,
    starts_as_comment: bool,
    error: bool,
    /// the longest a line can be
    maximum_line_size: usize,
    /// a line that was longer: where it starts, and how long it is
    long_line: Option<(usize, usize)>,
    line_start: usize,
    per_count: std::collections::BTreeMap<usize, usize>,
}

impl Sink for CountSink {
    fn add_value(&mut self, _machine: &Machine, _data: &[u8], _position: usize) {
        self.current += 1;
    }

    fn add_row(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool {
        let line_end = (position + 1).min(data.len() + 1);
        if line_end.saturating_sub(self.line_start) > self.maximum_line_size && position < data.len() {
            // the first line that is too long is the one reported
            self.long_line
                .get_or_insert((self.line_start, line_end - self.line_start));
            self.error = true;
        }
        self.line_start = line_end;
        let columns = self.current + 1;
        self.rows.push(ColumnCount {
            columns,
            last_value_always_empty: true,
            is_comment: false,
            is_mid_comment: false,
            empty_lines: self.empty_lines,
        });
        *self.per_count.entry(columns).or_default() += 1;
        self.current = 0;
        if !machine.empty_last_value() {
            for row in self.rows.iter_mut().rev() {
                if !row.last_value_always_empty {
                    break;
                }
                row.last_value_always_empty = false;
            }
        }
        self.rows.len() >= self.limit
    }

    fn empty_line(&mut self, _machine: &Machine, _position: usize) -> bool {
        self.empty_lines += 1;
        false
    }

    fn invalid_state(&mut self) {
        self.rows.clear();
        self.error = true;
    }

    fn is_comment_set(&self) -> bool {
        self.comment
    }

    fn set_comment(&mut self, machine: &Machine, _position: usize) {
        if machine.states[0] != State::Standard && machine.states[0] != State::Delimiter {
            self.starts_as_comment = true;
        }
        self.comment = true;
    }

    fn unset_comment(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool {
        let done = self.add_row(machine, data, position);
        if let Some(row) = self.rows.last_mut() {
            if self.starts_as_comment {
                row.is_comment = true;
            } else {
                row.is_mid_comment = true;
            }
        }
        self.comment = false;
        self.starts_as_comment = false;
        done
    }
}

/// `ColumnCountScanner`
#[derive(Debug)]
pub struct ColumnCountScanner {
    pub machine: Machine,
    sink: CountSink,
    start: usize,
}

impl ColumnCountScanner {
    pub fn new(machine: Machine, limit: usize, maximum_line_size: usize) -> Self {
        let start = machine.position;
        Self {
            machine,
            sink: CountSink {
                limit,
                maximum_line_size,
                line_start: start,
                ..Default::default()
            },
            start,
        }
    }

    /// How long the line that was too long is.
    pub fn long_line(&self) -> Option<usize> {
        self.sink.long_line.map(|(_, size)| size)
    }

    pub fn rows(&self) -> &[ColumnCount] {
        &self.sink.rows
    }

    pub fn error(&self) -> bool {
        self.sink.error
    }

    pub fn finished(&self) -> bool {
        self.machine.finished
    }

    pub fn start(&self) -> usize {
        self.start
    }

    /// `GetMostFrequentColumnCount`
    pub fn most_frequent_column_count(&self) -> usize {
        let (mut count, mut most) = (0, 0);
        for (&columns, &rows) in &self.sink.per_count {
            if rows > most || (rows == most && columns > count) {
                (count, most) = (columns, rows);
            }
        }
        if most == 0 { 1 } else { count }
    }

    /// `ColumnCountScanner::ParseChunk`
    pub fn parse_chunk(&mut self, data: &[u8]) {
        self.sink.rows.clear();
        self.machine.process(data, &mut self.sink);
        if self.sink.rows.len() >= self.sink.limit || self.sink.error {
            return;
        }
        if self.machine.position >= data.len() && !self.machine.finished {
            self.machine.finished = true;
            if self.machine.quoted_current() && self.machine.states[0] != State::Unquoted {
                self.sink.error = true;
                return;
            }
            if self.machine.empty_line_state()
                || self.machine.new_row()
                || self.machine.current_new_row()
                || self.machine.states[1] == State::NotSet
            {
                return;
            }
            let position = data.len();
            if self.sink.comment {
                self.sink.unset_comment(&self.machine, data, position);
            } else {
                self.sink.add_row(&self.machine, data, position);
            }
        }
    }
}

/// `SkipResult`
#[derive(Debug, Default)]
struct SkipSink {
    wanted: usize,
    rows: usize,
    comment: bool,
    starts_as_comment: bool,
}

impl SkipSink {
    fn count(&mut self) -> bool {
        self.rows += 1;
        self.rows >= self.wanted
    }
}

impl Sink for SkipSink {
    fn add_value(&mut self, _machine: &Machine, _data: &[u8], _position: usize) {}

    fn add_row(&mut self, _machine: &Machine, _data: &[u8], _position: usize) -> bool {
        self.count()
    }

    fn empty_line(&mut self, _machine: &Machine, _position: usize) -> bool {
        self.count()
    }

    fn invalid_state(&mut self) {}

    fn is_comment_set(&self) -> bool {
        self.comment
    }

    fn set_comment(&mut self, machine: &Machine, _position: usize) {
        if machine.states[0] != State::Standard {
            self.starts_as_comment = true;
        }
        self.comment = true;
    }

    fn unset_comment(&mut self, _machine: &Machine, _data: &[u8], _position: usize) -> bool {
        let done = !self.starts_as_comment && self.count();
        self.comment = false;
        self.starts_as_comment = false;
        done
    }
}

/// `BaseScanner::SkipCSVRows`: where the rows after the first `rows` begin.
pub fn skip_rows(data: &[u8], dialect: &Dialect, transitions: &Arc<Transitions>, start: usize, rows: usize) -> usize {
    if rows == 0 {
        return start;
    }
    let mut machine = Machine::with_transitions(dialect.clone(), Arc::clone(transitions), start);
    let mut sink = SkipSink {
        wanted: rows,
        ..Default::default()
    };
    machine.process(data, &mut sink);
    // the line ended on its \r: what comes next is the \n of the same end
    if dialect.new_line == NewLine::CarryOn
        && machine.states[1] == State::CarriageReturn
        && data.get(machine.position) == Some(&b'\n')
    {
        machine.position += 1;
    }
    machine.position
}

/// A value of a row.
#[derive(Debug, Clone)]
pub struct Field<'a> {
    pub text: Cow<'a, [u8]>,
    pub quoted: bool,
    /// it went through `RemoveEscape`, or holds what a backslash left
    pub escaped: bool,
    /// where it is in the file, with its quotes
    pub offset: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    offset: usize,
    field_end: usize,
    quoted: bool,
    escaped: bool,
}

/// A row, or what a scanner found instead.
#[derive(Debug)]
pub enum Scanned<'a> {
    Row(Row<'a>),
    /// a line with nothing on it
    EmptyLine,
    Unterminated,
    Invalid,
}

#[derive(Debug)]
pub struct Row<'a> {
    pub fields: Vec<Field<'a>>,
    /// the line that holds it, without its end
    pub start: usize,
    pub end: usize,
}

/// `StringValueResult`, without vectors: rows are made of what a file says.
#[derive(Debug, Default)]
struct ValueSink {
    extra_delimiter_bytes: usize,
    ignore_empty_values: bool,
    last: usize,
    quoted: bool,
    escaped: bool,
    quoted_position: usize,
    comment: bool,
    position_before_comment: usize,
    spans: Vec<Span>,
    row_ready: bool,
    empty_line: bool,
    invalid: bool,
    row_start: usize,
    row_end: usize,
}

impl ValueSink {
    fn push(&mut self, start: usize, end: usize, field_end: usize, quoted: bool) {
        self.spans.push(Span {
            start,
            end,
            offset: self.last,
            field_end,
            quoted,
            escaped: self.escaped,
        });
        self.escaped = false;
    }

    /// `AddQuotedValue`
    fn add_quoted(&mut self, data: &[u8], position: usize) {
        let begin = self.quoted_position + 1;
        if position <= begin {
            self.push(begin, begin, position, true);
        } else {
            let mut length = position - begin;
            while length > 0 && self.ignore_empty_values && data[begin + length - 1] == b' ' {
                length -= 1;
            }
            let length = length.saturating_sub(1);
            self.push(begin, begin + length, position, true);
        }
        self.quoted = false;
    }

    /// `AddRow`, its row start and end
    fn finish(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool {
        if self.last <= position {
            if self.quoted {
                self.add_quoted(data, position);
            } else {
                let last = self.last;
                self.push(last, position, position, false);
            }
            self.last = if machine.dialect.new_line == NewLine::CarryOn && machine.states[1] != State::RecordSeparator {
                position + 2
            } else {
                position + 1
            };
        }
        self.row_end = position;
        self.row_ready = true;
        true
    }
}

impl Sink for ValueSink {
    fn add_value(&mut self, _machine: &Machine, data: &[u8], position: usize) {
        if self.last > position {
            return;
        }
        if self.quoted {
            self.add_quoted(data, position - self.extra_delimiter_bytes);
        } else {
            let last = self.last;
            let end = if position >= last + self.extra_delimiter_bytes {
                position - self.extra_delimiter_bytes
            } else {
                last
            };
            self.push(last, end, end, false);
        }
        self.last = position + 1;
    }

    fn add_row(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool {
        self.finish(machine, data, position)
    }

    fn empty_line(&mut self, machine: &Machine, position: usize) -> bool {
        self.last = position + 1;
        if machine.states[1] == State::CarriageReturn && machine.dialect.new_line == NewLine::CarryOn {
            self.last += 1;
        }
        self.empty_line = true;
        true
    }

    fn invalid_state(&mut self) {
        self.invalid = true;
    }

    fn is_comment_set(&self) -> bool {
        self.comment
    }

    fn set_comment(&mut self, _machine: &Machine, position: usize) {
        if !self.comment {
            self.position_before_comment = position;
            self.comment = true;
        }
    }

    fn unset_comment(&mut self, machine: &Machine, data: &[u8], position: usize) -> bool {
        let mut done = false;
        let before = self.position_before_comment;
        if self.last < before {
            if data[self.last..before].iter().any(|&c| c != b' ') {
                done = self.finish(machine, data, before);
            }
        } else if !self.spans.is_empty() {
            done = self.finish(machine, data, before);
        }
        self.comment = false;
        self.last = if machine.dialect.new_line == NewLine::CarryOn {
            position + 2
        } else {
            position + 1
        };
        if !done {
            self.row_start = self.last;
        }
        done
    }

    fn set_quoted(&mut self, position: usize) {
        if !self.quoted {
            self.quoted_position = position;
        }
        self.quoted = true;
    }

    fn set_unquoted(&mut self, machine: &Machine) {
        if machine.states == [State::Unquoted, State::Unquoted] && machine.dialect.escape.is_some() {
            self.escaped = true;
        }
        self.quoted = true;
    }

    fn set_escaped(&mut self) {
        self.escaped = true;
    }
}

/// `StringValueScanner::RemoveEscape`
fn remove_escape(text: &[u8], escape: u8, quote: u8, strict: bool) -> Vec<u8> {
    let mut result = Vec::with_capacity(text.len());
    let mut just_escaped = false;
    for &c in text {
        if c == escape && !just_escaped {
            just_escaped = true;
        } else if c == quote {
            if just_escaped || !strict {
                result.push(c);
            }
            just_escaped = false;
        } else {
            just_escaped = false;
            result.push(c);
        }
    }
    result
}

/// `StringValueScanner`: a row at a time.
#[derive(Debug)]
pub struct ValueScanner {
    pub machine: Machine,
    sink: ValueSink,
}

impl ValueScanner {
    pub fn new(machine: Machine) -> Self {
        let dialect = &machine.dialect;
        let space = Some(b' ');
        let sink = ValueSink {
            extra_delimiter_bytes: dialect.delimiter.len().saturating_sub(1),
            ignore_empty_values: dialect.delimiter.first().copied() != space
                && dialect.quote != space
                && dialect.escape != space,
            last: machine.position,
            row_start: machine.position,
            ..Default::default()
        };
        Self { machine, sink }
    }

    pub fn finished(&self) -> bool {
        self.machine.finished
    }

    /// The next row, or `None` at the end of the data.
    pub fn next<'a>(&mut self, data: &'a [u8]) -> Option<Scanned<'a>> {
        let sink = &mut self.sink;
        sink.spans.clear();
        sink.row_ready = false;
        sink.empty_line = false;
        sink.invalid = false;
        sink.row_start = sink.last;
        self.machine.process(data, sink);
        if !sink.row_ready && !sink.empty_line && !sink.invalid {
            if self.machine.finished {
                return None;
            }
            self.machine.finished = true;
            let machine = &self.machine;
            let strict = machine.dialect.strict;
            if machine.empty_line_state()
                || machine.new_row()
                || machine.current_new_row()
                || machine.states[1] == State::NotSet
            {
                return None;
            } else if machine.states[1] == State::Delimiter {
                sink.add_value(machine, data, data.len());
                if sink.comment {
                    sink.unset_comment(machine, data, data.len());
                } else {
                    sink.finish(machine, data, data.len());
                }
            } else if machine.quoted_current() && strict {
                return Some(Scanned::Unterminated);
            } else if sink.comment {
                sink.unset_comment(machine, data, data.len());
                if !sink.row_ready {
                    return None;
                }
            } else {
                if sink.quoted
                    && strict
                    && matches!(
                        machine.states[0],
                        State::DelimiterFirstByte | State::DelimiterSecondByte | State::DelimiterThirdByte
                    )
                {
                    return Some(Scanned::Unterminated);
                }
                sink.finish(machine, data, data.len());
            }
        }
        if sink.invalid {
            return Some(if sink.quoted {
                Scanned::Unterminated
            } else {
                Scanned::Invalid
            });
        }
        if sink.empty_line {
            return Some(Scanned::EmptyLine);
        }
        let dialect = &self.machine.dialect;
        let escape = dialect.escape.unwrap_or(0);
        let quote = dialect.quote.unwrap_or(0);
        let fields = sink
            .spans
            .iter()
            .map(|span| {
                let text = &data[span.start.min(data.len())..span.end.min(data.len())];
                Field {
                    text: if span.escaped {
                        Cow::Owned(remove_escape(text, escape, quote, dialect.strict))
                    } else {
                        Cow::Borrowed(text)
                    },
                    quoted: span.quoted,
                    escaped: span.escaped,
                    offset: span.offset,
                    end: span.field_end,
                }
            })
            .collect();
        Some(Scanned::Row(Row {
            fields,
            start: sink.row_start,
            end: sink.row_end.min(data.len()),
        }))
    }

    /// Goes on after the line of an error (`SkipUntilNewLine`).
    pub fn skip_line(&mut self, data: &[u8]) {
        let machine = &mut self.machine;
        let mut i = machine.position;
        while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
            i += 1;
        }
        if data.get(i) == Some(&b'\r') && data.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        machine.position = (i + 1).min(data.len());
        machine.states = [State::NotSet; 2];
        machine.finished = false;
        let sink = &mut self.sink;
        sink.last = machine.position;
        sink.quoted = false;
        sink.escaped = false;
        sink.comment = false;
        sink.spans.clear();
    }
}
