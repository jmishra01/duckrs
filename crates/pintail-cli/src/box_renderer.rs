// Ported from duckdb/src/common/box_renderer.cpp @ rust-port-baseline
//
//! The box renderer (`BoxRenderer`): a result as a table drawn with box characters, the shell's
//! default "duckbox" mode.
//!
//! # What the shell sets
//!
//! [`BoxRendererConfig::default`] has the C++ defaults of the struct (`max_rows` 20, `max_width` 0).
//! `ModeDuckBoxRenderer` (tools/shell/shell_renderer.cpp) overrides them; the shell should do the same:
//!
//! - `max_rows` = the shell's `.maxrows` (40 by default), `max_width` = its `.maxwidth` (0 by default).
//! - if `max_width` is 0 and stdout is not a terminal (e.g. `--batch` piped into a file), set
//!   `max_width = usize::MAX` (no width limit); when the output goes to a file (`.output file`, not a
//!   pipe) also set `max_rows = usize::MAX`. With `max_width` 0 the renderer uses the terminal's width
//!   (120 when stdout is not a terminal).
//! - `max_analyze_rows` = `.maxrows`' second argument if given, else 100 000 when stdout is a console,
//!   else unlimited (`usize::MAX`, the default).
//! - `large_number_rendering` = `.large_number_rendering` (`Footer` when it is "default" and stdout is a
//!   console, `None` otherwise).
//! - `null_value` = `.nullvalue`, `render_mode` = [`RenderMode::Columns`] for `.columns`,
//!   `decimal_separator` / `thousand_separator` = `.decimal_sep` / `.thousand_sep`.
//! - `hidden_rows_hint` = "use .last to show entire result" unless the pager is off.
//! - `.last` (`RemoveRenderLimits`) renders with `max_rows` and `max_width` at `usize::MAX`.
//!
//! Highlighting is not ported: [`ResultRenderer`] gets the pieces with their roles (the hook C++'s
//! `DuckBoxRenderer` colours), but the JSON highlighting annotations are not computed, so a nested value
//! arrives as one piece.

mod json;
use pintail_common::render_width as unicode;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};

use pintail::{LogicalType, LogicalTypeId, QueryResult};

pub(crate) use unicode::render_width;
use unicode::{next_grapheme_cluster, render_width_at};

/// The rows of a chunk (`STANDARD_VECTOR_SIZE`): results are fetched and rendered in chunks of these.
const CHUNK_SIZE: usize = 2048;

/// `ValueRenderAlignment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRenderAlignment {
    Left,
    Middle,
    Right,
}

/// `RenderMode`: a row per row, or a row per column (`.columns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    #[default]
    Rows,
    Columns,
}

/// `ResultRenderType`: the role of a piece of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultRenderType {
    Layout,
    ColumnName,
    ColumnType,
    Value,
    NullValue,
    Footer,
    /// a string inside a nested value (from highlighting, which is not ported)
    #[allow(dead_code)]
    StringLiteral,
}

/// `LargeNumberRendering`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LargeNumberRendering {
    /// render all numbers as they are
    #[default]
    None,
    /// with a single row, add a row with a readable form of large numbers (e.g. "(1.23 million)")
    Footer,
    /// render all large numbers in their readable form
    All,
}

/// The characters the box is drawn with (the constants of C++'s `BoxRendererConfig`).
#[derive(Debug, Clone)]
pub struct BoxChars {
    pub ltcorner: &'static str,
    pub rtcorner: &'static str,
    pub ldcorner: &'static str,
    pub rdcorner: &'static str,
    pub middle: &'static str,
    pub tmiddle: &'static str,
    pub lmiddle: &'static str,
    pub rmiddle: &'static str,
    pub dmiddle: &'static str,
    pub vertical: &'static str,
    pub horizontal: &'static str,
    pub dotdotdot: &'static str,
    pub dot: &'static str,
    pub dotdotdot_length: usize,
}

impl BoxChars {
    /// The `DUCKDB_ASCII_TREE_RENDERER` set.
    #[allow(dead_code)]
    pub fn ascii() -> Self {
        Self {
            ltcorner: "<",
            rtcorner: ">",
            ldcorner: "<",
            rdcorner: ">",
            middle: "+",
            tmiddle: "+",
            lmiddle: "+",
            rmiddle: "+",
            dmiddle: "+",
            vertical: "|",
            horizontal: "-",
            dotdotdot: "...",
            dot: ".",
            dotdotdot_length: 3,
        }
    }
}

impl Default for BoxChars {
    fn default() -> Self {
        Self {
            ltcorner: "┌",
            rtcorner: "┐",
            ldcorner: "└",
            rdcorner: "┘",
            middle: "┼",
            tmiddle: "┬",
            lmiddle: "├",
            rmiddle: "┤",
            dmiddle: "┴",
            vertical: "│",
            horizontal: "─",
            dotdotdot: "…",
            dot: "·",
            dotdotdot_length: 1,
        }
    }
}

/// `BoxRendererConfig`.
#[derive(Debug, Clone)]
pub struct BoxRendererConfig {
    /// the width to fit in; 0 is the terminal's (120 when stdout is not a terminal); at least 80
    pub max_width: usize,
    /// the rows to render at most
    pub max_rows: usize,
    /// the rows to look at to work out the column widths
    pub max_analyze_rows: usize,
    /// the limit applied before rendering: with exactly `limit` rows, the row count is "?"
    pub limit: usize,
    /// the width a column is shortened to (only if the result does not fit)
    pub max_col_width: usize,
    /// how NULL is rendered
    pub null_value: String,
    /// the decimal separator (if any)
    pub decimal_separator: Option<char>,
    /// the thousand separator (if any)
    pub thousand_separator: Option<char>,
    /// row-wise or column-wise
    pub render_mode: RenderMode,
    /// how large numbers are rendered
    pub large_number_rendering: LargeNumberRendering,
    /// shown in the footer when rows or columns are hidden
    pub hidden_rows_hint: String,
    /// the box characters
    pub chars: BoxChars,
}

impl Default for BoxRendererConfig {
    fn default() -> Self {
        Self {
            max_width: 0,
            max_rows: 20,
            max_analyze_rows: usize::MAX,
            limit: 0,
            max_col_width: 20,
            null_value: "NULL".to_string(),
            decimal_separator: None,
            thousand_separator: None,
            render_mode: RenderMode::Rows,
            large_number_rendering: LargeNumberRendering::None,
            hidden_rows_hint: String::new(),
            chars: BoxChars::default(),
        }
    }
}

/// `BaseResultRenderer`: where the rendered pieces go, with their roles.
pub trait ResultRenderer {
    fn render_layout(&mut self, text: &str);
    fn render_column_name(&mut self, text: &str);
    fn render_type(&mut self, text: &str);
    /// `ty` is the value's column type (INVALID if there is none)
    fn render_value(&mut self, text: &str, ty: &LogicalType);
    fn render_null(&mut self, text: &str, ty: &LogicalType);
    fn render_string_literal(&mut self, text: &str, ty: &LogicalType) {
        self.render_value(text, ty);
    }
    fn render_footer(&mut self, text: &str);
}

/// `StringResultRenderer`: everything into one string.
#[derive(Debug, Default)]
pub struct StringResultRenderer {
    pub result: String,
}

impl ResultRenderer for StringResultRenderer {
    fn render_layout(&mut self, text: &str) {
        self.result.push_str(text);
    }
    fn render_column_name(&mut self, text: &str) {
        self.result.push_str(text);
    }
    fn render_type(&mut self, text: &str) {
        self.result.push_str(text);
    }
    fn render_value(&mut self, text: &str, _ty: &LogicalType) {
        self.result.push_str(text);
    }
    fn render_null(&mut self, text: &str, _ty: &LogicalType) {
        self.result.push_str(text);
    }
    fn render_footer(&mut self, text: &str) {
        self.result.push_str(text);
    }
}

/// The part of `BaseResultRenderer` that knows the column types: a [`ResultRenderer`] plus the column
/// of the value being rendered.
struct Sink<'r> {
    out: &'r mut dyn ResultRenderer,
    column_types: Vec<LogicalType>,
    invalid_type: LogicalType,
    column_idx: Option<usize>,
}

impl Sink<'_> {
    fn layout(&mut self, text: &str) {
        self.out.render_layout(text);
    }

    fn spaces(&mut self, count: usize) {
        if count > 0 {
            self.out.render_layout(&" ".repeat(count));
        }
    }

    fn value_type(&self) -> &LogicalType {
        self.column_idx
            .and_then(|c| self.column_types.get(c))
            .unwrap_or(&self.invalid_type)
    }

    /// `BaseResultRenderer::Render`.
    fn render(&mut self, mode: ResultRenderType, text: &str) {
        match mode {
            ResultRenderType::Layout => self.out.render_layout(text),
            ResultRenderType::ColumnName => self.out.render_column_name(text),
            ResultRenderType::ColumnType => self.out.render_type(text),
            ResultRenderType::Value => {
                let ty = self.value_type().clone();
                self.out.render_value(text, &ty);
            }
            ResultRenderType::NullValue => {
                let ty = self.value_type().clone();
                self.out.render_null(text, &ty);
            }
            ResultRenderType::StringLiteral => {
                let ty = self.value_type().clone();
                self.out.render_string_literal(text, &ty);
            }
            ResultRenderType::Footer => self.out.render_footer(text),
        }
    }
}

/// `BoxRenderValue`.
#[derive(Debug, Clone)]
struct BoxRenderValue {
    text: String,
    render_mode: ResultRenderType,
    alignment: ValueRenderAlignment,
    column_idx: Option<usize>,
    render_width: Option<usize>,
    decomposed: bool,
}

impl BoxRenderValue {
    fn new(
        text: String,
        render_mode: ResultRenderType,
        alignment: ValueRenderAlignment,
        column_idx: Option<usize>,
        render_width: Option<usize>,
    ) -> Self {
        Self {
            text,
            render_mode,
            alignment,
            column_idx,
            render_width,
            decomposed: false,
        }
    }
}

/// `RenderRowType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderRowType {
    RowValues,
    Separator,
}

/// `BoxRenderRow`.
#[derive(Debug, Clone)]
struct BoxRenderRow {
    row_type: RenderRowType,
    values: Vec<BoxRenderValue>,
}

impl BoxRenderRow {
    fn new(row_type: RenderRowType) -> Self {
        Self {
            row_type,
            values: Vec::new(),
        }
    }
}

/// `BoxRendererFooter`.
#[derive(Debug, Default)]
struct BoxRendererFooter {
    row_count_str: String,
    readable_rows_str: String,
    shown_str: String,
    column_count_str: String,
    render_length: usize,
    must_show_footer: bool,
    has_hidden_rows: bool,
    has_hidden_columns: bool,
}

/// A rendered cell: the text (`None` for NULL) and its render width.
#[derive(Debug, Clone)]
struct RenderCell {
    text: Option<String>,
    width: usize,
}

/// `RenderDataCollection`: the cells to render, row by row.
type RenderCollection = Vec<Vec<RenderCell>>;

/// `BoxRendererState` / `BoxRendererImplementation`: a result laid out, ready to render.
pub struct BoxRendererState<'a> {
    config: BoxRendererConfig,
    column_names: Vec<String>,
    /// the types of the result
    original_types: Vec<LogicalType>,
    /// the types of what is rendered (all VARCHAR in columns mode)
    result_types: Vec<LogicalType>,
    result: &'a QueryResult,
    column_widths: Vec<usize>,
    column_boundary_positions: Vec<usize>,
    total_render_length: usize,
    header_rows: Vec<BoxRenderRow>,
    footer: BoxRendererFooter,
    pruned_columns: HashSet<usize>,
    column_map: Vec<Option<usize>>,
    expand_rows: bool,
    is_first_row: bool,
    max_rows_per_row: usize,
    render_collections: Vec<RenderCollection>,
    top_rows: usize,
    bottom_rows: usize,
    /// if the result was not fully fetched, the row to resume from
    current_row_idx: Option<usize>,
}

/// `BoxRenderer::Prepare`: lays out `result` (whose columns are called `names` and have `types`).
pub fn prepare<'a>(
    config: &BoxRendererConfig,
    names: &[String],
    types: &[LogicalType],
    result: &'a QueryResult,
) -> BoxRendererState<'a> {
    BoxRendererState::new(config.clone(), names, types, result)
}

/// `BoxRenderer::ToString`: the result drawn as C++ draws it.
pub fn render(config: &BoxRendererConfig, names: &[String], types: &[LogicalType], result: &QueryResult) -> String {
    let mut out = StringResultRenderer::default();
    prepare(config, names, types, result).render(&mut out);
    out.result
}

/// `Printer::TerminalWidth`: the width of the terminal, 120 if it is unknown.
pub fn terminal_width() -> usize {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Winsize {
            ws_row: u16,
            ws_col: u16,
            ws_xpixel: u16,
            ws_ypixel: u16,
        }
        #[cfg(target_os = "macos")]
        const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
        #[cfg(target_os = "linux")]
        const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
        unsafe extern "C" {
            fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
        }
        let mut w = Winsize::default();
        // SAFETY: TIOCGWINSZ writes a `struct winsize`, which `Winsize` mirrors, into the pointer.
        unsafe {
            ioctl(0, TIOCGWINSZ, &mut w as *mut Winsize);
        }
        if w.ws_col == 0 { 120 } else { usize::from(w.ws_col) }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        120
    }
}

/// `BoxRenderer::TruncateValue`: the part of `value` from `pos` that fits in `column_width` (counting
/// from `current_render_width`), stopping after a newline. Moves `pos` and `current_render_width`.
fn truncate_value(value: &str, column_width: usize, pos: &mut usize, current_render_width: &mut usize) -> String {
    let start_pos = *pos;
    let bytes = value.as_bytes();
    while *pos < value.len() {
        if bytes[*pos] == b'\n' {
            // stop at a newline, and skip it
            let render_pos = *pos;
            *pos += 1;
            return value[start_pos..render_pos].to_string();
        }
        let char_size = render_width_at(value, *pos);
        if *current_render_width + char_size > column_width {
            break;
        }
        *current_render_width += char_size;
        *pos = next_grapheme_cluster(value, *pos);
    }
    value[start_pos..*pos].to_string()
}

/// `LowerTypeStringPreservingQuotes`.
fn lower_type_string_preserving_quotes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_single_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            result.push(c);
            if in_single_quote && chars.peek() == Some(&'\'') {
                result.push(chars.next().unwrap_or('\''));
                continue;
            }
            in_single_quote = !in_single_quote;
        } else if in_single_quote {
            result.push(c);
        } else {
            result.push(c.to_ascii_lowercase());
        }
    }
    result
}

/// `BoxRendererImplementation::RenderType`: the type name in the header.
pub fn render_type(ty: &LogicalType) -> String {
    if ty.alias().is_some() {
        return ty.to_string().to_ascii_lowercase();
    }
    match ty.id() {
        LogicalTypeId::TinyInt => "int8".into(),
        LogicalTypeId::SmallInt => "int16".into(),
        LogicalTypeId::Integer => "int32".into(),
        LogicalTypeId::BigInt => "int64".into(),
        LogicalTypeId::HugeInt => "int128".into(),
        LogicalTypeId::UTinyInt => "uint8".into(),
        LogicalTypeId::USmallInt => "uint16".into(),
        LogicalTypeId::UInteger => "uint32".into(),
        LogicalTypeId::UBigInt => "uint64".into(),
        LogicalTypeId::UHugeInt => "uint128".into(),
        LogicalTypeId::List if ty.child_type().is_some() => {
            format!("{}[]", render_type(ty.child_type().unwrap_or(ty)))
        }
        _ => lower_type_string_preserving_quotes(&ty.to_string()),
    }
}

fn is_numeric_id(id: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    matches!(
        id,
        TinyInt
            | SmallInt
            | Integer
            | BigInt
            | HugeInt
            | UTinyInt
            | USmallInt
            | UInteger
            | UBigInt
            | UHugeInt
            | Decimal
            | Float
            | Double
    )
}

/// `BoxRendererImplementation::TypeAlignment`.
fn type_alignment(ty: &LogicalType) -> ValueRenderAlignment {
    if is_numeric_id(ty.id()) {
        ValueRenderAlignment::Right
    } else {
        ValueRenderAlignment::Left
    }
}

/// `BoxRenderer::TryFormatLargeNumber`: e.g. "1234567" as "1.23 million"; empty if it is not large.
pub fn try_format_large_number(numeric: &str, decimal_sep: Option<char>) -> String {
    // only numbers of a million or more are summarized
    if numeric.len() <= 5 {
        return String::new();
    }
    let bytes = numeric.as_bytes();
    let mut number: u64 = 0;
    let mut negative = false;
    let mut i = 0;
    if bytes[0] == b'-' {
        negative = true;
        i += 1;
    }
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'.' {
            break;
        }
        if !c.is_ascii_digit() {
            // not a number, or something like 1.23e7
            return String::new();
        }
        if number >= 1_000_000_000_000_000_000 {
            return String::new();
        }
        number = number * 10 + u64::from(c - b'0');
        i += 1;
    }
    const BASES: [(u64, &str); 5] = [
        (1_000_000, "million"),
        (1_000_000_000, "billion"),
        (1_000_000_000_000, "trillion"),
        (1_000_000_000_000_000, "quadrillion"),
        (1_000_000_000_000_000_000, "quintillion"),
    ];
    let mut base = 0;
    let mut unit = "";
    for (b, name) in BASES {
        let rounded_number = number + ((b / 100) / 2);
        if rounded_number >= b {
            base = b;
            unit = name;
        }
    }
    if unit.is_empty() {
        return String::new();
    }
    number += (base / 100) / 2;
    let decimal_unit = number / (base / 100);
    let decimal_str = decimal_unit.to_string();
    let mut result = String::new();
    if negative {
        result.push('-');
    }
    let split = decimal_str.len() - 2;
    result.push_str(&decimal_str[..split]);
    result.push(decimal_sep.unwrap_or('.'));
    result.push_str(&decimal_str[split..]);
    result.push(' ');
    result.push_str(unit);
    result
}

/// `BoxRendererImplementation::ConvertRenderValue(input)`: control characters escaped.
fn convert_render_value(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for c in input.chars() {
        let byte_value = c as u32;
        if byte_value < 32 {
            result.push('\\');
            match byte_value {
                7 => result.push('a'),
                8 => result.push('b'),
                9 => result.push('t'),
                10 => result.push('n'),
                11 => result.push('v'),
                12 => result.push('f'),
                13 => result.push('r'),
                27 => result.push('e'),
                _ => result.push_str(&byte_value.to_string()),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Whether a value of `ty` is pretty-printed / highlighted as JSON (`IsJSONType() || IsNested()`).
fn is_json_or_nested(ty: &LogicalType) -> bool {
    (ty.id() == LogicalTypeId::Varchar && ty.alias() == Some("JSON")) || ty.is_nested()
}

impl<'a> BoxRendererState<'a> {
    fn new(config: BoxRendererConfig, names: &[String], types: &[LogicalType], result: &'a QueryResult) -> Self {
        let mut state = Self {
            config,
            column_names: names.to_vec(),
            original_types: types.to_vec(),
            result_types: types.to_vec(),
            result,
            column_widths: Vec::new(),
            column_boundary_positions: Vec::new(),
            total_render_length: 0,
            header_rows: Vec::new(),
            footer: BoxRendererFooter::default(),
            pruned_columns: HashSet::new(),
            column_map: Vec::new(),
            expand_rows: false,
            is_first_row: true,
            max_rows_per_row: 1,
            render_collections: Vec::new(),
            top_rows: 0,
            bottom_rows: 0,
            current_row_idx: None,
        };
        state.initialize();
        state
    }

    /// `TotalRenderWidth`: the width of the rendered box (the shell's pager check uses it).
    #[allow(dead_code)]
    pub fn total_render_width(&self) -> usize {
        self.total_render_length
    }

    fn column_count(&self) -> usize {
        self.original_types.len()
    }

    fn row_count(&self) -> usize {
        self.result.row_count()
    }

    /// `ComputeRowFooter`.
    fn compute_row_footer(&mut self, row_count: usize, rendered_rows: usize) {
        let column_count = self.column_count();
        self.footer.column_count_str = format!("{column_count} column");
        if column_count > 1 {
            self.footer.column_count_str.push('s');
        }
        self.footer.row_count_str = self.format_number(&row_count.to_string()) + " rows";
        let has_limited_rows = self.config.limit > 0 && row_count == self.config.limit;
        if has_limited_rows {
            self.footer.row_count_str = "? rows".into();
        }
        if self.config.large_number_rendering == LargeNumberRendering::Footer && !has_limited_rows {
            let readable_str = self.try_format_large_number(&row_count.to_string());
            if !readable_str.is_empty() {
                self.footer.readable_rows_str = format!("{row_count} total");
                self.footer.row_count_str = readable_str + " rows";
            }
        }
        self.footer.has_hidden_rows = rendered_rows < row_count;
        if self.footer.has_hidden_rows {
            if has_limited_rows {
                let limit = self.format_number(&(self.config.limit - 1).to_string());
                self.footer.shown_str.push_str(&format!(">{limit} rows, "));
            }
            let shown = self.format_number(&rendered_rows.to_string());
            self.footer.shown_str.push_str(&format!("{shown} shown"));
        }
        self.footer.must_show_footer = has_limited_rows || self.footer.has_hidden_rows || row_count == 0;
        self.footer.render_length = self
            .footer
            .row_count_str
            .len()
            .max(self.footer.shown_str.len() + 2)
            .max(self.footer.readable_rows_str.len() + 2)
            + 4;
    }

    /// `UpdateColumnCountFooter`.
    fn update_column_count_footer(&mut self, column_count: usize) {
        if self.pruned_columns.is_empty() {
            return;
        }
        let shown = column_count - self.pruned_columns.len();
        if self.config.render_mode == RenderMode::Columns {
            // in columns mode, pruned columns are pruned rows
            self.footer.has_hidden_rows = true;
            self.footer.shown_str = format!("{} shown", shown.saturating_sub(2));
        } else {
            self.footer.has_hidden_columns = true;
            self.footer.column_count_str.push_str(&format!(" ({shown} shown)"));
        }
    }

    /// `Initialize`.
    fn initialize(&mut self) {
        let mut max_width = self.config.max_width;
        if max_width == 0 {
            use std::io::IsTerminal;
            max_width = if std::io::stdout().is_terminal() {
                terminal_width()
            } else {
                120
            };
        }
        // widths under 80 are not supported
        let max_width = max_width.max(80);

        // which rows to render
        let row_count = self.row_count();
        let mut rows_to_render = row_count.min(self.config.max_rows);
        if row_count <= self.config.max_rows.saturating_add(3) {
            // hiding rows adds 3 lines, so render all rows when only slightly over the limit
            rows_to_render = row_count;
        }
        if rows_to_render == row_count {
            self.top_rows = row_count;
            self.bottom_rows = 0;
        } else {
            self.top_rows = rows_to_render / 2 + rows_to_render % 2;
            self.bottom_rows = rows_to_render - self.top_rows;
        }
        self.compute_row_footer(row_count, self.top_rows + self.bottom_rows);

        let mut collections = self.fetch_render_collections(self.top_rows, self.bottom_rows);
        if self.config.render_mode == RenderMode::Columns && rows_to_render > 0 {
            collections = self.pivot_collections(collections, row_count);
        }

        let min_width = if self.footer.must_show_footer {
            self.footer.render_length
        } else {
            0
        };
        self.compute_render_widths(&collections, min_width, max_width);
        self.render_collections = collections;

        // the positions of the column boundaries
        for c in 0..self.column_widths.len() {
            let boundary = if c == 0 {
                self.column_widths[c] + 2
            } else {
                self.column_boundary_positions[c - 1] + self.column_widths[c] + 3
            };
            self.column_boundary_positions.push(boundary);
        }
    }

    /// `Render`: the box into `out`.
    pub fn render(mut self, out: &mut dyn ResultRenderer) {
        let mut ss = Sink {
            out,
            column_types: self.original_types.clone(),
            invalid_type: LogicalType::new(LogicalTypeId::Invalid),
            column_idx: None,
        };
        self.render_header(&mut ss);
        let mut collections = std::mem::take(&mut self.render_collections);
        loop {
            self.render_values(&mut ss, &collections);
            let Some(row_idx) = self.current_row_idx else {
                break;
            };
            // fetch the next rows
            collections = vec![
                self.fetch_top_collection(row_idx, self.top_rows, self.bottom_rows),
                Vec::new(),
            ];
        }
        let column_count = self.result_types.len();
        self.render_footer(&mut ss, self.row_count(), column_count);
    }

    /// `RenderValue`.
    #[allow(clippy::too_many_arguments)]
    fn render_value(
        &self,
        ss: &mut Sink<'_>,
        value: &str,
        column_width: usize,
        render_mode: ResultRenderType,
        alignment: ValueRenderAlignment,
        render_width_input: Option<usize>,
        vertical: Option<&str>,
    ) {
        let mut render_width = render_width_input.unwrap_or_else(|| render_width(value));
        let mut small_value;
        let mut render_value = value;
        if render_width > column_width {
            // too wide for the column (which was shortened): render what fits, then "…"
            let mut pos = 0;
            let mut current_render_width = self.config.chars.dotdotdot_length;
            small_value = truncate_value(value, column_width, &mut pos, &mut current_render_width);
            small_value.push_str(self.config.chars.dotdotdot);
            render_width = current_render_width;
            render_value = &small_value;
        }
        let padding_count = column_width.saturating_sub(render_width) + 2;
        let (lpadding, rpadding) = match alignment {
            ValueRenderAlignment::Left => (1, padding_count - 1),
            ValueRenderAlignment::Middle => (padding_count / 2, padding_count - padding_count / 2),
            ValueRenderAlignment::Right => (padding_count - 1, 1),
        };
        ss.layout(vertical.unwrap_or(self.config.chars.vertical));
        ss.spaces(lpadding);
        ss.render(render_mode, render_value);
        ss.spaces(rpadding);
    }

    /// `TryFormatLargeNumber`.
    fn try_format_large_number(&self, numeric: &str) -> String {
        try_format_large_number(numeric, self.config.decimal_separator)
    }

    /// `FormatNumber`: large-number rendering and the separators.
    fn format_number(&self, input: &str) -> String {
        if self.config.large_number_rendering == LargeNumberRendering::All {
            let number = self.try_format_large_number(input);
            if !number.is_empty() {
                return number;
            }
        }
        if self.config.decimal_separator.is_none() && self.config.thousand_separator.is_none() {
            return input.to_string();
        }
        // the digits before any decimal point
        let bytes = input.as_bytes();
        let character_count = bytes.iter().take_while(|c| c.is_ascii_digit()).count();
        let mut separator_position = if character_count % 3 == 0 {
            3
        } else {
            character_count % 3
        };
        let mut result = String::new();
        for (c, &digit) in bytes[..character_count].iter().enumerate() {
            if c == separator_position
                && let Some(sep) = self.config.thousand_separator
            {
                result.push(sep);
                separator_position += 3;
            }
            result.push(char::from(digit));
        }
        for c in input[character_count..].chars() {
            match (c, self.config.decimal_separator) {
                ('.', Some(sep)) => result.push(sep),
                _ => result.push(c),
            }
        }
        result
    }

    /// `ConvertRenderValue(input, type)`.
    fn convert_render_value_typed(&self, input: &str, ty: &LogicalType) -> String {
        if is_numeric_id(ty.id()) {
            self.format_number(input)
        } else {
            convert_render_value(input)
        }
    }

    /// A value cast to VARCHAR and converted for rendering (`CastToVarchar` + `ConvertRenderVector`).
    fn render_cell(&self, column: usize, row: usize, null_render_length: usize) -> RenderCell {
        let value = self.result.value(column, row);
        let text = if value.is_null() {
            None
        } else {
            value.try_to_string().ok()
        };
        match text {
            None => RenderCell {
                text: None,
                width: null_render_length,
            },
            Some(text) => {
                let text = self.convert_render_value_typed(&text, &self.original_types[column]);
                RenderCell {
                    width: render_width(&text),
                    text: Some(text),
                }
            }
        }
    }

    /// `FetchTopCollection`: the rows from `row_idx` up to `top_rows`, in chunks; stops after
    /// `max_analyze_rows` rows (when there are no bottom rows), remembering where.
    fn fetch_top_collection(&mut self, mut row_idx: usize, top_rows: usize, bottom_rows: usize) -> RenderCollection {
        let column_count = self.column_count();
        let row_count = self.row_count();
        let null_render_length = render_width(&self.config.null_value);
        let mut collection = RenderCollection::new();
        self.current_row_idx = None;
        while row_idx < top_rows {
            let chunk_size = CHUNK_SIZE.min(row_count - row_idx);
            let insert_count = chunk_size.min(top_rows - row_idx);
            for r in row_idx..row_idx + insert_count {
                collection.push(
                    (0..column_count)
                        .map(|c| self.render_cell(c, r, null_render_length))
                        .collect(),
                );
            }

            if self.config.large_number_rendering == LargeNumberRendering::Footer {
                // a row with a readable form of the values (i.e. "1234567" becomes "(1.23 million)")
                let mut readable_numbers = Vec::with_capacity(column_count);
                let mut all_readable = true;
                let first_row = &collection[collection.len() - insert_count];
                for (ty, cell) in self.original_types.iter().zip(first_row) {
                    if !ty.is_numeric() {
                        all_readable = false;
                        break;
                    }
                    let readable = self.try_format_large_number(cell.text.as_deref().unwrap_or("NULL"));
                    if readable.is_empty() {
                        all_readable = false;
                        break;
                    }
                    readable_numbers.push(format!("({readable})"));
                }
                if all_readable {
                    collection.push(
                        readable_numbers
                            .into_iter()
                            .map(|text| RenderCell {
                                width: render_width(&text),
                                text: Some(text),
                            })
                            .collect(),
                    );
                } else {
                    self.config.large_number_rendering = LargeNumberRendering::None;
                }
            }

            row_idx += chunk_size;
            if bottom_rows == 0
                && row_idx >= self.config.max_analyze_rows
                && self.config.render_mode == RenderMode::Rows
            {
                // stop for now: resume from here
                self.current_row_idx = Some(row_idx);
                break;
            }
        }
        collection
    }

    /// `FetchBottomCollection`: the last `bottom_rows` rows.
    fn fetch_bottom_collection(&self, bottom_rows: usize) -> RenderCollection {
        let column_count = self.column_count();
        let row_count = self.row_count();
        let null_render_length = render_width(&self.config.null_value);
        (row_count - bottom_rows..row_count)
            .map(|r| {
                (0..column_count)
                    .map(|c| self.render_cell(c, r, null_render_length))
                    .collect()
            })
            .collect()
    }

    /// `FetchRenderCollections`: the top and the bottom rows.
    fn fetch_render_collections(&mut self, top_rows: usize, bottom_rows: usize) -> Vec<RenderCollection> {
        if self.config.large_number_rendering == LargeNumberRendering::Footer
            && (self.config.render_mode != RenderMode::Rows || self.row_count() != 1)
        {
            // the large number footer is only for a single row in rows mode
            self.config.large_number_rendering = LargeNumberRendering::None;
        }
        let top = self.fetch_top_collection(0, top_rows, bottom_rows);
        let bottom = self.fetch_bottom_collection(bottom_rows);
        vec![top, bottom]
    }

    /// `PivotCollections`: a row per column, with its name, type and values.
    fn pivot_collections(&mut self, input: Vec<RenderCollection>, row_count: usize) -> Vec<RenderCollection> {
        let top_count = input[0].len();
        let bottom_count = input[1].len();
        let mut new_names = vec!["Column".to_string(), "Type".to_string()];
        for r in 0..top_count {
            new_names.push(format!("Row {}", r + 1));
        }
        for r in 0..bottom_count {
            new_names.push(format!("Row {}", row_count - bottom_count + r + 1));
        }
        let mut result = RenderCollection::new();
        for c in 0..self.column_names.len() {
            let column_name = self.column_names[c].clone();
            let type_name = render_type(&self.original_types[c]);
            let mut row = vec![
                RenderCell {
                    width: render_width(&column_name),
                    text: Some(column_name),
                },
                RenderCell {
                    width: render_width(&type_name),
                    text: Some(type_name),
                },
            ];
            for collection in &input {
                for input_row in collection {
                    row.push(input_row[c].clone());
                }
            }
            result.push(row);
        }
        self.result_types = vec![LogicalType::new(LogicalTypeId::Varchar); new_names.len()];
        self.column_names = new_names;
        vec![result]
    }

    /// `CanPrettyPrint` / `CanHighlight`.
    fn can_pretty_print(&self, value: &BoxRenderValue) -> bool {
        value
            .column_idx
            .and_then(|c| self.original_types.get(c))
            .is_some_and(is_json_or_nested)
    }

    /// `PotentiallyExpandRow`: a shortened value spread over extra lines.
    fn potentially_expand_row(&self, mut row: BoxRenderRow, rows: &mut Vec<BoxRenderRow>, is_first_row: bool) {
        let max_rows_per_row = self.max_rows_per_row;
        let mut extra_rows: Vec<BoxRenderRow> = Vec::new();
        for c in 0..row.values.len() {
            let column_width = self.column_widths[c];
            if self.can_pretty_print(&row.values[c]) {
                if let Some(text) = json::format_value(&row.values[c].text, max_rows_per_row, column_width) {
                    row.values[c].text = text;
                }
                // always decompose a pretty-printed value
                row.values[c].render_width = Some(column_width + 1);
            }
            let value_width = row.values[c]
                .render_width
                .unwrap_or_else(|| render_width(&row.values[c].text));
            if value_width <= column_width {
                continue;
            }
            // shortened: stretch it out over extra rows, starting with what fits on the first
            let mut current_row = 0;
            let mut current_pos = 0;
            let mut current_render_width = 0;
            let full_value = std::mem::take(&mut row.values[c].text);
            row.values[c].text = truncate_value(&full_value, column_width, &mut current_pos, &mut current_render_width);
            row.values[c].render_width = Some(current_render_width);
            row.values[c].decomposed = true;
            while current_pos < full_value.len() {
                if current_row >= extra_rows.len() {
                    if extra_rows.len() > max_rows_per_row {
                        // no space for another row
                        break;
                    }
                    let mut extra_row = BoxRenderRow::new(RenderRowType::RowValues);
                    for current_val in &row.values {
                        extra_row.values.push(BoxRenderValue::new(
                            String::new(),
                            current_val.render_mode,
                            current_val.alignment,
                            current_val.column_idx,
                            None,
                        ));
                    }
                    extra_rows.push(extra_row);
                }
                let can_add_extra_row = current_row + 1 < extra_rows.len() || extra_rows.len() < max_rows_per_row;
                let extra_value = &mut extra_rows[current_row].values[c];
                current_row += 1;
                current_render_width = 0;
                if can_add_extra_row {
                    extra_value.text =
                        truncate_value(&full_value, column_width, &mut current_pos, &mut current_render_width);
                } else {
                    // no row after this one: all the rest goes here
                    extra_value.text = full_value[current_pos..].to_string();
                    current_render_width = render_width(&extra_value.text);
                    current_pos = full_value.len();
                }
                extra_value.render_width = Some(current_render_width);
                extra_value.decomposed = true;
            }
        }
        // a separator before all but the first row
        if !is_first_row {
            rows.push(BoxRenderRow::new(RenderRowType::Separator));
        }
        rows.push(row);
        rows.extend(extra_rows);
    }

    /// `ComputeRenderWidths`: the column widths, shortening and pruning columns to fit `max_width`.
    fn compute_render_widths(&mut self, collections: &[RenderCollection], min_width: usize, max_width: usize) {
        let column_count = self.result_types.len();

        // the header: names and types
        let mut header_row = BoxRenderRow::new(RenderRowType::RowValues);
        let mut type_row = BoxRenderRow::new(RenderRowType::RowValues);
        for c in 0..column_count {
            let column_name = convert_render_value(&self.column_names[c]);
            let column_name_width = render_width(&column_name);
            let mut column_type_width = 0;
            header_row.values.push(BoxRenderValue::new(
                column_name,
                ResultRenderType::ColumnName,
                ValueRenderAlignment::Middle,
                None,
                Some(column_name_width),
            ));
            if self.config.render_mode == RenderMode::Rows {
                let column_type = render_type(&self.result_types[c]);
                column_type_width = render_width(&column_type);
                type_row.values.push(BoxRenderValue::new(
                    column_type,
                    ResultRenderType::ColumnType,
                    ValueRenderAlignment::Middle,
                    None,
                    Some(column_type_width),
                ));
            }
            self.column_widths.push(column_name_width.max(column_type_width));
        }
        self.header_rows.push(header_row);
        if self.config.render_mode == RenderMode::Rows {
            self.header_rows.push(type_row);
        }

        // the widest value of each column
        for collection in collections {
            for row in collection {
                for (c, cell) in row.iter().enumerate().take(column_count) {
                    if cell.width > self.column_widths[c] {
                        self.column_widths[c] = cell.width;
                    }
                }
            }
        }

        let mut shortened_columns = false;
        // a pipe at the start, and per column a space before and a space and a pipe after
        self.total_render_length = 1 + self.column_widths.iter().map(|w| w + 3).sum::<usize>();
        if self.total_render_length < min_width && !self.column_widths.is_empty() {
            // the footer must show: stretch the first column so that it fits
            self.column_widths[0] += min_width - self.total_render_length;
            self.total_render_length = min_width;
        }
        if self.total_render_length > max_width {
            let original_widths = self.column_widths.clone();
            // first try shortening the columns
            let mut max_shorten_amount = Vec::with_capacity(column_count);
            let mut total_max_shorten_amount = 0;
            for &w in &self.column_widths {
                let diff = w.saturating_sub(self.config.max_col_width);
                max_shorten_amount.push(diff);
                total_max_shorten_amount += diff;
            }
            let mut shorten_amount_required = self.total_render_length - max_width;
            if total_max_shorten_amount >= shorten_amount_required {
                // shorten the widest columns first, towards the same width
                let mut shorten_map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
                for (col_idx, &amount) in max_shorten_amount.iter().enumerate() {
                    shorten_map.entry(amount).or_default().push(col_idx);
                }
                let mut actual_shorten_amounts = vec![0usize; max_shorten_amount.len()];
                while shorten_amount_required > 0 {
                    let Some((&largest_width, _)) = shorten_map.iter().next_back() else {
                        break;
                    };
                    let second_largest_width = shorten_map.range(..largest_width).next_back().map(|(&k, _)| k);
                    let max_shorten_width = largest_width - second_largest_width.unwrap_or(0);
                    let column_list = shorten_map.remove(&largest_width).unwrap_or_default();
                    let total_potential_shorten_width = max_shorten_width * column_list.len();
                    if total_potential_shorten_width >= shorten_amount_required {
                        // shortening these columns is enough: shorten them equally
                        let per_column = shorten_amount_required / column_list.len();
                        for &column_idx in &column_list {
                            actual_shorten_amounts[column_idx] += per_column;
                            shorten_amount_required -= per_column;
                        }
                        // the rest of the division, one each from the back
                        for &column_idx in column_list.iter().rev() {
                            if shorten_amount_required == 0 {
                                break;
                            }
                            actual_shorten_amounts[column_idx] += 1;
                            shorten_amount_required -= 1;
                        }
                        break;
                    }
                    let Some(second_largest_width) = second_largest_width else {
                        break;
                    };
                    // shorten these to the next width, and continue with them in that group
                    for &column_idx in &column_list {
                        actual_shorten_amounts[column_idx] += max_shorten_width;
                    }
                    shorten_map.entry(second_largest_width).or_default().extend(column_list);
                    shorten_amount_required -= total_potential_shorten_width;
                }
                for (c, &amount) in actual_shorten_amounts.iter().enumerate() {
                    if amount == 0 {
                        continue;
                    }
                    self.column_widths[c] -= amount;
                    self.total_render_length -= amount;
                    shortened_columns = true;
                }
            } else {
                // shortening is not enough: shorten all to the max col width, then prune columns
                for w in &mut self.column_widths {
                    if *w <= self.config.max_col_width {
                        continue;
                    }
                    self.total_render_length -= *w - self.config.max_col_width;
                    *w = self.config.max_col_width;
                    shortened_columns = true;
                }
            }

            if self.total_render_length > max_width {
                // still too wide: remove columns, making space for the "…" column
                self.total_render_length += 3 + self.config.chars.dotdotdot_length;
                // in zig-zag order from the middle: of 10 columns, #5, then #4, then #6, #3, #7, ...
                let mut offset: i64 = 0;
                while self.total_render_length > max_width {
                    let c = column_count as i64 / 2 + offset;
                    if c < 0 || c as usize >= column_count {
                        break;
                    }
                    let c = c as usize;
                    self.total_render_length -= self.column_widths[c] + 3;
                    self.pruned_columns.insert(c);
                    offset = if offset >= 0 { -offset - 1 } else { -offset };
                }
                // give space that is left back to shortened columns
                let mut space_left = max_width.saturating_sub(self.total_render_length);
                for (c, (width, &original_width)) in self.column_widths.iter_mut().zip(&original_widths).enumerate() {
                    if space_left == 0 {
                        break;
                    }
                    if self.pruned_columns.contains(&c) || *width >= original_width {
                        continue;
                    }
                    let increase_amount = space_left.min(original_width - *width);
                    *width += increase_amount;
                    space_left -= increase_amount;
                    self.total_render_length += increase_amount;
                }
            }
        }

        self.update_column_count_footer(column_count);

        // the visible columns, with the "…" column in place of the pruned ones
        let mut added_split_column = false;
        let mut new_widths = Vec::new();
        for c in 0..column_count {
            if !self.pruned_columns.contains(&c) {
                self.column_map.push(Some(c));
                new_widths.push(self.column_widths[c]);
            } else if !added_split_column {
                self.column_map.push(None);
                new_widths.push(self.config.chars.dotdotdot_length);
                added_split_column = true;
            }
        }
        self.column_widths = new_widths;

        // prune the header rows
        for header_row in &mut self.header_rows {
            let mut values = std::mem::take(&mut header_row.values)
                .into_iter()
                .map(Some)
                .collect::<Vec<_>>();
            header_row.values = self
                .column_map
                .iter()
                .flatten()
                .filter_map(|&c| values[c].take())
                .collect();
        }

        let row_count: usize = collections.iter().map(Vec::len).sum();
        // spread shortened values over several lines: only in rows mode, without hidden columns
        if shortened_columns
            && self.config.render_mode == RenderMode::Rows
            && row_count > 0
            && row_count + 5 < self.config.max_rows
            && self.pruned_columns.is_empty()
        {
            let per_row = if self.config.max_rows <= 5 {
                0
            } else {
                (self.config.max_rows - 5) / row_count
            };
            self.max_rows_per_row = per_row.max(1);
            if self.max_rows_per_row > 1 {
                self.expand_rows = true;
            }
        }
    }

    /// `RenderLayoutLine`.
    fn render_layout_line(&self, ss: &mut Sink<'_>, layout: &str, boundary: &str, left: &str, right: &str) {
        let mut line = String::from(left);
        let mut column_index = 0;
        for k in 0..self.total_render_length.saturating_sub(2) {
            if column_index < self.column_boundary_positions.len() && k == self.column_boundary_positions[column_index]
            {
                line.push_str(boundary);
                column_index += 1;
            } else {
                line.push_str(layout);
            }
        }
        line.push_str(right);
        line.push('\n');
        ss.layout(&line);
    }

    /// `RenderDivider`: the three "·" lines between the top and the bottom rows.
    fn render_divider(&self, ss: &mut Sink<'_>, prev_row: &BoxRenderRow, next_row: &BoxRenderRow) {
        const DIVIDER_ROW_COUNT: usize = 3;
        let mut divider_rows = vec![BoxRenderRow::new(RenderRowType::RowValues); DIVIDER_ROW_COUNT];
        let dot = self.config.chars.dot;
        for c in 0..prev_row.values.len() {
            let prev_value = &prev_row.values[c];
            let next_value = &next_row.values[c];
            let mut alignment = prev_value.alignment;
            let text = if alignment == ValueRenderAlignment::Middle {
                dot.to_string()
            } else {
                // in the middle of the values above and below
                // (C++ indexes the widths by the value index, which skips the "…" column)
                let column_width = self.column_widths[c];
                let top_length = column_width.min(render_width(&prev_value.text));
                let bottom_length = column_width.min(render_width(&next_value.text));
                let mut dot_length = top_length.min(bottom_length);
                if top_length == 0 {
                    dot_length = bottom_length;
                } else if bottom_length == 0 {
                    dot_length = top_length;
                }
                if dot_length > 1 {
                    let padding = dot_length - 1;
                    let (left_padding, right_padding) = if alignment == ValueRenderAlignment::Left {
                        (padding / 2, padding - padding / 2)
                    } else {
                        (padding - padding / 2, padding / 2)
                    };
                    format!("{}{dot}{}", " ".repeat(left_padding), " ".repeat(right_padding))
                } else {
                    if dot_length == 0 {
                        // everything is empty
                        alignment = ValueRenderAlignment::Middle;
                    }
                    dot.to_string()
                }
            };
            for divider in &mut divider_rows {
                divider.values.push(BoxRenderValue::new(
                    text.clone(),
                    ResultRenderType::Layout,
                    alignment,
                    None,
                    None,
                ));
            }
        }
        for divider in &divider_rows {
            self.render_row(ss, divider);
        }
    }

    /// `RenderRow`.
    fn render_row(&self, ss: &mut Sink<'_>, row: &BoxRenderRow) {
        if row.row_type == RenderRowType::Separator {
            let chars = &self.config.chars;
            self.render_layout_line(ss, chars.horizontal, chars.middle, chars.lmiddle, chars.rmiddle);
            return;
        }
        let split_value = BoxRenderValue::new(
            self.config.chars.dotdotdot.to_string(),
            ResultRenderType::Layout,
            ValueRenderAlignment::Middle,
            None,
            Some(self.config.chars.dotdotdot_length),
        );
        let mut value_idx = 0;
        for (column_idx, &column_width) in self.column_widths.iter().enumerate() {
            let render_value = if self.column_map[column_idx].is_some() {
                value_idx += 1;
                &row.values[value_idx - 1]
            } else {
                &split_value
            };
            if matches!(
                render_value.render_mode,
                ResultRenderType::NullValue | ResultRenderType::Value
            ) {
                ss.column_idx = render_value.column_idx;
            }
            self.render_value(
                ss,
                &render_value.text,
                column_width,
                render_value.render_mode,
                render_value.alignment,
                render_value.render_width,
                None,
            );
        }
        ss.layout(self.config.chars.vertical);
        ss.layout("\n");
    }

    /// `RenderHeader`.
    fn render_header(&mut self, ss: &mut Sink<'_>) {
        let chars = self.config.chars.clone();
        self.render_layout_line(ss, chars.horizontal, chars.tmiddle, chars.ltcorner, chars.rtcorner);
        let mut rows_to_render = Vec::new();
        for row in std::mem::take(&mut self.header_rows) {
            if self.expand_rows {
                self.potentially_expand_row(row, &mut rows_to_render, self.is_first_row);
            } else {
                rows_to_render.push(row);
            }
        }
        for row in &rows_to_render {
            self.render_row(ss, row);
        }
        if self.row_count() > 0 {
            self.render_row(ss, &BoxRenderRow::new(RenderRowType::Separator));
        }
    }

    /// `RenderValues`.
    fn render_values(&mut self, ss: &mut Sink<'_>, collections: &[RenderCollection]) {
        let columns_to_scan: Vec<usize> = self.column_map.iter().flatten().copied().collect();
        let mut render_divider = false;
        let mut last_rendered_rows: Vec<BoxRenderRow> = Vec::new();
        let mut row_idx = 0;
        let mut is_first_collection = true;
        for collection in collections {
            if collection.is_empty() {
                continue;
            }
            if !is_first_collection {
                render_divider = true;
            }
            is_first_collection = false;

            for chunk in collection.chunks(CHUNK_SIZE) {
                let mut chunk_rows = vec![BoxRenderRow::new(RenderRowType::RowValues); chunk.len()];
                for (c, &scan_column) in columns_to_scan.iter().enumerate() {
                    for (r, cells) in chunk.iter().enumerate() {
                        let cell = &cells[scan_column];
                        let (text, mut render_type) = match &cell.text {
                            None => (self.config.null_value.clone(), ResultRenderType::NullValue),
                            Some(text) => (text.clone(), ResultRenderType::Value),
                        };
                        let mut column_idx = None;
                        let mut alignment;
                        if self.config.render_mode == RenderMode::Rows {
                            column_idx = Some(scan_column);
                            alignment = type_alignment(&self.result_types[scan_column]);
                        } else {
                            // in columns mode the name and type are left-aligned, the values right-aligned
                            match c {
                                0 => {
                                    render_type = ResultRenderType::ColumnName;
                                    alignment = ValueRenderAlignment::Left;
                                }
                                1 => {
                                    render_type = ResultRenderType::ColumnType;
                                    alignment = ValueRenderAlignment::Left;
                                }
                                _ => {
                                    render_type = ResultRenderType::Value;
                                    alignment = ValueRenderAlignment::Right;
                                    // the type of the value is that of the original column of this row
                                    column_idx = Some(row_idx + r);
                                }
                            }
                        }
                        if self.config.large_number_rendering == LargeNumberRendering::Footer {
                            // the large number footer is centered, and rendered as NULL
                            alignment = ValueRenderAlignment::Middle;
                            if row_idx + r == 1 {
                                render_type = ResultRenderType::NullValue;
                            }
                        }
                        chunk_rows[r].values.push(BoxRenderValue::new(
                            text,
                            render_type,
                            alignment,
                            column_idx,
                            Some(cell.width),
                        ));
                    }
                }
                row_idx += chunk.len();
                let mut rows_to_render = Vec::new();
                for row in chunk_rows {
                    if render_divider {
                        if let Some(last) = last_rendered_rows.last() {
                            self.render_divider(ss, last, &row);
                        }
                        render_divider = false;
                    }
                    if self.expand_rows {
                        self.potentially_expand_row(row, &mut rows_to_render, self.is_first_row);
                    } else {
                        rows_to_render.push(row);
                    }
                    self.is_first_row = false;
                }
                for row in &rows_to_render {
                    self.render_row(ss, row);
                }
                last_rendered_rows = rows_to_render;
            }
        }
    }

    /// `RenderFooter`: the bottom line, and the row and column counts.
    fn render_footer(&mut self, ss: &mut Sink<'_>, row_count: usize, column_count: usize) {
        let total_render_length = self.total_render_length;
        let footer = &mut self.footer;
        let has_hidden_columns = footer.has_hidden_columns;
        let has_hidden_rows = footer.has_hidden_rows;
        // can the row count, the readable row count and the shown count go on one line?
        let minimum_length = footer.row_count_str.len() + footer.column_count_str.len() + 6;
        let render_rows_and_columns = total_render_length >= minimum_length
            && ((has_hidden_columns && row_count > 0) || (row_count >= 10 && column_count > 1));
        let render_rows = total_render_length >= footer.render_length && (row_count == 0 || row_count >= 10);
        let render_anything = render_rows || render_rows_and_columns;

        let chars = self.config.chars.clone();
        let mut line = String::from(chars.ldcorner);
        let mut column_index = 0;
        for k in 0..total_render_length.saturating_sub(2) {
            if column_index < self.column_boundary_positions.len() && k == self.column_boundary_positions[column_index]
            {
                line.push_str(chars.dmiddle);
                column_index += 1;
            } else {
                line.push_str(chars.horizontal);
            }
        }
        line.push_str(chars.rdcorner);
        line.push('\n');
        ss.layout(&line);
        if !render_anything {
            return;
        }
        let footer = &mut self.footer;
        let mut padding = total_render_length.saturating_sub(footer.row_count_str.len() + 4);
        if render_rows_and_columns {
            padding = padding.saturating_sub(footer.column_count_str.len());
        }
        let mut extra_render_str = String::new();
        // is there space for the shown count on the same line?
        let shown_size = footer.readable_rows_str.len()
            + footer.shown_str.len()
            + if footer.readable_rows_str.is_empty() { 3 } else { 5 };
        if has_hidden_rows && padding >= shown_size {
            extra_render_str = format!(" ({}", footer.shown_str);
            if !footer.readable_rows_str.is_empty() {
                extra_render_str.push_str(&format!(", {}", footer.readable_rows_str));
            }
            extra_render_str.push(')');
            padding -= shown_size;
            footer.readable_rows_str.clear();
            footer.shown_str.clear();
        }

        ss.layout("  ");
        if render_rows_and_columns {
            ss.render(ResultRenderType::Footer, &footer.row_count_str);
            if !extra_render_str.is_empty() {
                ss.render(ResultRenderType::Footer, &extra_render_str);
            }
            // is there space for the hidden rows hint on this line?
            let hint = &self.config.hidden_rows_hint;
            if (has_hidden_columns || has_hidden_rows) && !hint.is_empty() && padding >= hint.len() + 10 {
                padding -= hint.len();
                let lpadding = padding / 2;
                ss.spaces(lpadding);
                ss.render(ResultRenderType::Footer, hint);
                ss.spaces(padding - lpadding);
            } else {
                ss.spaces(padding);
            }
            ss.render(ResultRenderType::Footer, &footer.column_count_str);
        } else if render_rows {
            let lpadding = padding / 2;
            ss.spaces(lpadding);
            ss.render(ResultRenderType::Footer, &footer.row_count_str);
            if !extra_render_str.is_empty() {
                ss.render(ResultRenderType::Footer, &extra_render_str);
            }
            ss.spaces(padding - lpadding);
        }
        ss.layout("\n");
        let mut readable_rows_str = std::mem::take(&mut footer.readable_rows_str);
        let mut shown_str = std::mem::take(&mut footer.shown_str);
        if !readable_rows_str.is_empty() || !shown_str.is_empty() {
            // can the two go on one line?
            let combined_shown_length = readable_rows_str.len() + shown_str.len() + 4;
            if !readable_rows_str.is_empty() && !shown_str.is_empty() && combined_shown_length <= total_render_length {
                ss.layout("  ");
                ss.render(ResultRenderType::Footer, &shown_str);
                ss.spaces(total_render_length - combined_shown_length);
                ss.render(ResultRenderType::Footer, &readable_rows_str);
                ss.layout("\n");
                readable_rows_str.clear();
                shown_str.clear();
            }
            let alignment = if render_rows_and_columns {
                ValueRenderAlignment::Left
            } else {
                ValueRenderAlignment::Middle
            };
            let width = total_render_length.saturating_sub(4);
            if !shown_str.is_empty() {
                let text = format!("({shown_str})");
                self.render_value(ss, &text, width, ResultRenderType::Footer, alignment, None, Some(" "));
                ss.layout("\n");
            }
            if !readable_rows_str.is_empty() {
                let text = format!("({readable_rows_str})");
                self.render_value(ss, &text, width, ResultRenderType::Footer, alignment, None, Some(" "));
                ss.layout("\n");
            }
        }
    }
}
