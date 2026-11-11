//! The shell's table-metadata renderer, ported from DuckDB's `tools/shell/shell_render_table_metadata.cpp`
//! (`ShellState::RenderTableMetadata` and its helpers).
//!
//! It draws a box per table (name, columns with their types and constraints, estimated row count),
//! grouped under a `database` and `schema` heading and packed side by side as wide as the render width
//! allows. The shell uses it for `.tables`, `.databases` and the `DESCRIBE` render mode.
//!
//! The renderer is pure: the caller fills [`TableInfo`]s and a [`MetadataConfig`] and gets the text.
//! Callers:
//! - `.tables [pattern]` (`ShellState::DisplayTables`): one [`TableInfo`] per row of the query on
//!   `duckdb_columns()`/`duckdb_tables()`/`duckdb_constraints()`; `is_view` when `duckdb_tables()` has
//!   no row for it (the `table_oid` of the join is NULL), `estimated_size` from `duckdb_tables()`.
//! - `.databases` (`ShellState::ShowDatabases`): one table named `databases`, with empty database and
//!   schema names, a column per row of `SELECT name, file FROM pragma_database_list`: the name as the
//!   column name, the file (or `(memory)` when it is NULL) as the type.
//! - `DESCRIBE` (`ModeDescribeRenderer`): one table with empty database and schema names, named after the
//!   described table (or `Describe`), a column per result row: `column_name`, `column_type`,
//!   `null = 'NO'` as not null, `key = 'PRI'` as primary key, `key = 'UNI'` as unique, `default`.
//!
//! Highlighting is not ported: every piece of text goes through `Output::print` with the
//! `HighlightElementType` C++ colours it with, and is written as plain text (as when stdout is not a
//! terminal). The pager (`ShouldUsePager`/`SetupPager`) is left to the caller.
//!
//! Like C++, a table that fits next to two tables which do not fit
//! next to each other is drawn next to both.
//!
//! Where the port differs from C++:
//! - the tables of a schema are sorted by their line count with a stable sort; C++'s `std::sort` leaves
//!   the order of tables with the same line count to the standard library.
//! - C++ underflows (and crashes) or indexes out of bounds for a few inputs that the shell never builds
//!   (tables with and without constraint components in one call, split tables stretched by a narrower
//!   neighbour); the port saturates or treats a missing width as 0 instead.

use std::collections::{BTreeMap, HashSet};

use crate::box_renderer::{render_width, try_format_large_number};

/// `ShellColumnInfo`: a column of a table, or a row of `.databases`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: String,
    pub is_primary_key: bool,
    pub is_not_null: bool,
    pub is_unique: bool,
    /// The default expression; empty if there is none.
    pub default_value: String,
}

/// `ShellTableInfo`: a table to render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableInfo {
    /// The database heading; empty for none.
    pub database_name: String,
    /// The schema heading; empty for none.
    pub schema_name: String,
    pub table_name: String,
    /// The estimated row count, rendered as "N rows" at the bottom of the box.
    pub estimated_size: Option<u64>,
    pub is_view: bool,
    pub columns: Vec<ColumnInfo>,
}

/// What `RenderTableMetadata` reads from `ShellState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataConfig {
    /// `ShellState::GetMaxRenderWidth()`: see [`max_render_width`].
    pub max_width: usize,
    /// `agent_mode_active`: one line per table instead of boxes (`RenderTableMetadataCompact`).
    pub compact: bool,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            max_width: 120,
            compact: false,
        }
    }
}

/// `ShellState::GetMaxRenderWidth`: `.maxwidth` (`shell_max_width`), or the terminal width when it is 0,
/// at least 80.
///
/// Like C++ this asks the terminal width of stdin (`Printer::TerminalWidth` does `ioctl(0, ...)`), which
/// is 120 when stdin is not a terminal, whether or not stdout is one.
pub fn max_render_width(shell_max_width: usize) -> usize {
    let width = if shell_max_width == 0 {
        crate::box_renderer::terminal_width()
    } else {
        shell_max_width
    };
    width.max(80)
}

/// `HighlightElementType`: the roles C++ colours the pieces of the rendering with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HighlightElementType {
    Layout,
    TableLayout,
    ViewLayout,
    TableName,
    ColumnName,
    PrimaryKeyColumn,
    ColumnType,
    Comment,
    DatabaseName,
    SchemaName,
}

/// `ShellHighlight` writing to stdout: collects the text; this is where the colours would go.
#[derive(Default)]
struct Output {
    text: String,
}

impl Output {
    /// `ShellHighlight::PrintText`.
    fn print(&mut self, text: &str, element_type: HighlightElementType) {
        // plain text output: the element type would pick the colour
        let _ = element_type;
        self.text.push_str(text);
    }
}

/// `BoxRendererConfig` (unicode box characters).
const LTCORNER: &str = "┌";
const RTCORNER: &str = "┐";
const LDCORNER: &str = "└";
const RDCORNER: &str = "┘";
const VERTICAL: &str = "│";
const HORIZONTAL: &str = "─";
const DOTDOTDOT: &str = "…";
const DOTDOTDOT_LENGTH: usize = 1;

/// Tables with more columns than this are split into several columns of columns when they fit.
const SPLIT_THRESHOLD: usize = 20;
/// `Truncate` does not shorten a component below this width.
const MIN_COMPONENT_SIZE: usize = 5;

/// `RenderComponent`: a piece of a column line (the name, the type, or the constraints).
struct RenderComponent {
    text: String,
    render_width: usize,
    element_type: HighlightElementType,
}

impl RenderComponent {
    fn new(text: String, element_type: HighlightElementType) -> Self {
        let render_width = render_width(&text);
        Self {
            text,
            render_width,
            element_type,
        }
    }
}

/// `ShellColumnRenderInfo`.
struct ColumnRenderInfo {
    components: Vec<RenderComponent>,
}

/// `ColumnRenderRow`: a column of column lines inside a box.
#[derive(Default)]
struct ColumnRenderRow {
    columns: Vec<ColumnRenderInfo>,
}

/// `ShellTableRenderInfo`: a table measured for rendering.
struct TableRenderInfo {
    database_name: String,
    schema_name: String,
    table_name: String,
    is_view: bool,
    has_estimated_size: bool,
    table_name_length: usize,
    render_width: usize,
    max_component_widths: Vec<usize>,
    estimated_size_text: String,
    estimated_size_length: usize,
    column_renders: Vec<ColumnRenderRow>,
}

/// `FormatTableMetadataType`: lower case, and all decimals as `decimal`.
fn format_table_metadata_type(ty: &str) -> String {
    let result = ty.to_ascii_lowercase();
    if result.starts_with("decimal") {
        return "decimal".to_string();
    }
    result
}

/// `4 + sum(widths) + (count - 1)`: the width of a box column with these component widths.
fn per_column_width(max_component_widths: &[usize]) -> usize {
    (4 + max_component_widths.iter().sum::<usize>() + max_component_widths.len()).saturating_sub(1)
}

/// `BoxRenderer::TruncateValue(value, column_width, 0, width)`: the longest prefix of `value` that renders
/// in `column_width`, stopping at a newline, and its render width.
fn truncate_value(value: &str, column_width: usize) -> (&str, usize) {
    let line = value.split('\n').next().unwrap_or("");
    let mut fit = (0, 0);
    for (pos, c) in line.char_indices() {
        let end = pos + c.len_utf8();
        let width = render_width(&line[..end]);
        if width > column_width {
            break;
        }
        fit = (end, width);
    }
    (&line[..fit.0], fit.1)
}

/// `ShellTableRenderInfo::TruncateValueIfRequired`: shorten `value` with an ellipsis to fit `max_render_width`.
fn truncate_value_if_required(value: &mut String, render_length: &mut usize, max_render_width: usize) {
    if *render_length <= max_render_width {
        return;
    }
    let (prefix, prefix_width) = truncate_value(value, max_render_width.saturating_sub(DOTDOTDOT_LENGTH));
    *value = format!("{prefix}{DOTDOTDOT}");
    *render_length = prefix_width + DOTDOTDOT_LENGTH;
}

impl TableRenderInfo {
    /// `ShellTableRenderInfo::ShellTableRenderInfo`.
    fn new(table: &TableInfo) -> Self {
        let table_name_length = render_width(&table.table_name);
        // a constraint component is added if any column has a constraint
        let has_constraint_component = table
            .columns
            .iter()
            .any(|col| col.is_not_null || col.is_unique || !col.default_value.is_empty());
        let component_count = 2 + usize::from(has_constraint_component);

        let mut render_row = ColumnRenderRow::default();
        for col in &table.columns {
            let column_name_type = if col.is_primary_key {
                HighlightElementType::PrimaryKeyColumn
            } else {
                HighlightElementType::ColumnName
            };
            let mut components = vec![
                RenderComponent::new(col.name.clone(), column_name_type),
                RenderComponent::new(format_table_metadata_type(&col.ty), HighlightElementType::ColumnType),
            ];
            if has_constraint_component {
                let mut constraints = Vec::new();
                if col.is_not_null {
                    constraints.push("not null".to_string());
                }
                if col.is_unique {
                    constraints.push("unique".to_string());
                }
                if !col.default_value.is_empty() {
                    constraints.push(format!("default {}", col.default_value));
                }
                components.push(RenderComponent::new(
                    constraints.join(" "),
                    HighlightElementType::ColumnType,
                ));
            }
            render_row.columns.push(ColumnRenderInfo { components });
        }

        // the widest of each component
        let mut max_component_widths = vec![0; component_count];
        for row in &render_row.columns {
            for (max, component) in max_component_widths.iter_mut().zip(&row.components) {
                *max = (*max).max(component.render_width);
            }
        }

        let mut info = Self {
            database_name: table.database_name.clone(),
            schema_name: table.schema_name.clone(),
            table_name: table.table_name.clone(),
            is_view: table.is_view,
            has_estimated_size: table.estimated_size.is_some(),
            table_name_length,
            render_width: table_name_length + 4,
            max_component_widths,
            estimated_size_text: String::new(),
            estimated_size_length: 0,
            column_renders: vec![render_row],
        };
        info.render_width = info.render_width.max(info.per_column_width());
        if let Some(estimated_size) = table.estimated_size {
            let mut text = estimated_size.to_string();
            // `BoxRendererConfig().decimal_separator` is always '\0' here, so the separator is '.'
            let formatted = try_format_large_number(&text, None);
            if !formatted.is_empty() {
                text = formatted;
            }
            text.push_str(" rows");
            info.estimated_size_length = render_width(&text);
            info.estimated_size_text = text;
            info.render_width = info.render_width.max(info.estimated_size_length + 4);
        }
        info
    }

    /// `LineCount`: top, name, blank, columns, (blank, size), bottom.
    fn line_count(&self) -> usize {
        let constant_count = if self.has_estimated_size { 6 } else { 4 };
        self.column_lines() + constant_count
    }

    fn per_column_width(&self) -> usize {
        per_column_width(&self.max_component_widths)
    }

    fn column_lines(&self) -> usize {
        self.column_renders.first().map_or(0, |render| render.columns.len())
    }

    /// `ShellTableRenderInfo::Truncate`: shorten the name and components to fit `max_render_width`.
    fn truncate(&mut self, max_render_width: usize) {
        if self.render_width <= max_render_width {
            return;
        }
        truncate_value_if_required(&mut self.table_name, &mut self.table_name_length, max_render_width - 4);
        let mut total_column_length = self.per_column_width();
        if total_column_length > max_render_width {
            // truncate the column names or types; keep the name as long as possible, only truncating it
            // when it by itself almost exceeds the width
            let component_count = self.max_component_widths.len();
            let min_leftover_size = component_count * MIN_COMPONENT_SIZE;
            if self.max_component_widths[0] + min_leftover_size > max_render_width {
                self.max_component_widths[0] = max_render_width - min_leftover_size;
            }
            total_column_length = self.per_column_width();
            if total_column_length > max_render_width {
                // truncate the other components if required
                for i in 1..component_count {
                    if self.max_component_widths[i] <= MIN_COMPONENT_SIZE {
                        // cannot truncate below the min component size
                        continue;
                    }
                    let truncate_amount =
                        (total_column_length - max_render_width).min(self.max_component_widths[i] - MIN_COMPONENT_SIZE);
                    self.max_component_widths[i] -= truncate_amount;
                    total_column_length -= truncate_amount;
                    // C++ compares with the (untruncated) render width here, not the max render width
                    if total_column_length <= self.render_width {
                        break;
                    }
                }
            }
            // truncate all the components that no longer fit
            for column_render in &mut self.column_renders {
                for col in &mut column_render.columns {
                    for (component, &max_width) in col.components.iter_mut().zip(&self.max_component_widths) {
                        truncate_value_if_required(&mut component.text, &mut component.render_width, max_width);
                    }
                }
            }
        }
        self.render_width = max_render_width;
    }

    /// Split a table with many columns into several columns of columns, if they fit in the width.
    fn split(&mut self, max_render_width: usize) {
        let column_count = self.column_lines();
        if column_count <= SPLIT_THRESHOLD {
            // not that many columns - keep it as-is
            return;
        }
        let max_split_count = column_count.div_ceil(SPLIT_THRESHOLD);
        let width_per_split = self.per_column_width();
        let max_splits = max_render_width / width_per_split;
        if max_splits <= 1 {
            // too wide - cannot split
            return;
        }
        let split_count = max_split_count.min(max_splits);
        let mut new_renders: Vec<ColumnRenderRow> = (0..split_count).map(|_| ColumnRenderRow::default()).collect();
        let columns = std::mem::take(&mut self.column_renders[0].columns);
        for (split_idx, col) in columns.into_iter().enumerate() {
            new_renders[split_idx % split_count].columns.push(col);
        }
        self.column_renders = new_renders;
        self.render_width = self.render_width.max(split_count * width_per_split);
    }
}

/// `TableMetadataLine`: a column of tables, stacked vertically, in a display.
struct TableMetadataLine {
    render_height: usize,
    render_width: usize,
    max_component_widths: Vec<usize>,
    /// Indexes of the tables in the schema's list.
    tables: Vec<usize>,
}

impl TableMetadataLine {
    fn new(table_idx: usize, table: &TableRenderInfo) -> Self {
        Self {
            render_height: table.line_count(),
            render_width: table.render_width,
            max_component_widths: table.max_component_widths.clone(),
            tables: vec![table_idx],
        }
    }

    fn per_column_width(&self) -> usize {
        per_column_width(&self.max_component_widths)
    }

    /// `TableMetadataLine::RenderLine`: the part of output line `line_idx` that this column of tables draws.
    fn render_line(&self, out: &mut Output, table_list: &[TableRenderInfo], mut line_idx: usize, last_line: bool) {
        // find the table this line belongs to
        let mut render_table = None;
        for &table_idx in &self.tables {
            let line_count = table_list[table_idx].line_count();
            if line_idx < line_count {
                render_table = Some(&table_list[table_idx]);
                break;
            }
            line_idx -= line_count;
        }
        let Some(render_table) = render_table else {
            // below the last table: pad with spaces, unless this is the last column of the line
            if !last_line {
                out.print(&" ".repeat(self.render_width), HighlightElementType::Layout);
            }
            return;
        };
        let layout_type = if render_table.is_view {
            HighlightElementType::ViewLayout
        } else {
            HighlightElementType::TableLayout
        };
        let inner_width = self.render_width.saturating_sub(2);
        let column_lines = render_table.column_lines();
        if line_idx == 0 {
            // top line
            out.print(
                &format!("{LTCORNER}{}{RTCORNER}", HORIZONTAL.repeat(inner_width)),
                layout_type,
            );
        } else if line_idx == 1 {
            // the table name, centered
            let space_count = inner_width.saturating_sub(render_table.table_name_length);
            let lspace = space_count / 2;
            let rspace = space_count - lspace;
            out.print(&format!("{VERTICAL}{}", " ".repeat(lspace)), layout_type);
            out.print(&render_table.table_name, HighlightElementType::TableName);
            out.print(&format!("{}{VERTICAL}", " ".repeat(rspace)), layout_type);
        } else if line_idx > 2 && line_idx < column_lines + 3 {
            self.render_column_line(out, render_table, line_idx - 3, layout_type);
        } else if line_idx == 2 || (render_table.has_estimated_size && line_idx == column_lines + 3) {
            // blank line after the name, or before the size
            out.print(&format!("{VERTICAL}{}{VERTICAL}", " ".repeat(inner_width)), layout_type);
        } else if render_table.has_estimated_size && line_idx == column_lines + 4 {
            // the estimated size, centered
            let space_count = inner_width.saturating_sub(render_table.estimated_size_length);
            let lspace = space_count / 2;
            let rspace = space_count - lspace;
            out.print(&format!("{VERTICAL}{}", " ".repeat(lspace)), layout_type);
            out.print(&render_table.estimated_size_text, HighlightElementType::Comment);
            out.print(&format!("{}{VERTICAL}", " ".repeat(rspace)), layout_type);
        } else {
            // bottom line
            out.print(
                &format!("{LDCORNER}{}{RDCORNER}", HORIZONTAL.repeat(inner_width)),
                layout_type,
            );
        }
    }

    /// The column line `column_idx` of a table: the column of each column of columns.
    fn render_column_line(
        &self,
        out: &mut Output,
        render_table: &TableRenderInfo,
        column_idx: usize,
        layout_type: HighlightElementType,
    ) {
        // a long table name (or a wider neighbour) may have stretched the box: spread the extra width
        let render_count_total = render_table.column_renders.len();
        let per_column_render_width = self.per_column_width();
        let column_render_width = render_count_total * per_column_render_width;
        let mut extra_render_width = self.render_width.saturating_sub(column_render_width);
        let render_width_per_column = extra_render_width / render_count_total;

        for (render_idx, column_render) in render_table.column_renders.iter().enumerate() {
            let is_last = render_idx + 1 == render_count_total;
            if let Some(col) = column_render.columns.get(column_idx) {
                let lead = if render_idx == 0 { VERTICAL } else { " " };
                out.print(&format!("{lead} "), layout_type);
                for (component_idx, component) in col.components.iter().enumerate() {
                    out.print(&component.text, component.element_type);
                    // pad to the widest component, plus the separating space
                    let max_width = self.max_component_widths.get(component_idx).copied().unwrap_or(0);
                    let mut padding = (max_width + 1).saturating_sub(component.render_width);
                    if extra_render_width > 0 {
                        // the extra spacing
                        let render_count = if is_last {
                            extra_render_width
                        } else {
                            render_width_per_column
                        };
                        padding += render_count;
                        extra_render_width -= render_count;
                    }
                    out.print(&" ".repeat(padding), layout_type);
                }
            } else {
                // this column of columns has no more columns - pad with spaces (C++ drops the lead here)
                out.print(&" ".repeat(per_column_render_width.saturating_sub(1)), layout_type);
            }
            out.print(if is_last { VERTICAL } else { " " }, layout_type);
        }
    }
}

/// `TableMetadataDisplayInfo`: tables of one schema drawn side by side, under the schema's heading.
struct TableMetadataDisplayInfo {
    database_name: String,
    schema_name: String,
    render_height: usize,
    render_width: usize,
    display_lines: Vec<TableMetadataLine>,
}

/// `RenderLineDisplay`: a heading like ` ───── name ───── ` of `total_render_width`.
fn render_line_display(out: &mut Output, text: &str, total_render_width: usize, element_type: HighlightElementType) {
    let mut text = text.to_string();
    let mut render_size = render_width(&text);
    truncate_value_if_required(&mut text, &mut render_size, total_render_width.saturating_sub(4));
    let total_lines = total_render_width.saturating_sub(render_size + 4);
    let lline = total_lines / 2;
    let rline = total_lines - lline;
    let line = format!(" {} {text} {} \n", HORIZONTAL.repeat(lline), HORIZONTAL.repeat(rline));
    out.print(&line, element_type);
}

/// `RenderTableMetadataCompact`: one line per table, `db.schema.name (table, ~N rows): col TYPE, ...`.
fn render_table_metadata_compact(tables: &[TableInfo]) -> String {
    let mut out = String::new();
    for table in tables {
        out.push_str(&format!(
            "{}.{}.{}",
            table.database_name, table.schema_name, table.table_name
        ));
        out.push_str(if table.is_view { " (view" } else { " (table" });
        if let Some(estimated_size) = table.estimated_size {
            out.push_str(&format!(", ~{estimated_size} rows"));
        }
        out.push_str("):");
        for (c, column) in table.columns.iter().enumerate() {
            out.push_str(if c == 0 { " " } else { ", " });
            out.push_str(&column.name);
            out.push(' ');
            out.push_str(&column.ty);
            if column.is_primary_key {
                out.push_str(" PK");
            }
        }
        out.push('\n');
    }
    out
}

/// Pack the tables of one schema (sorted from tall to short) into displays, greedily.
fn build_displays(result: &[TableRenderInfo], max_render_width: usize) -> Vec<TableMetadataDisplayInfo> {
    let mut displays = Vec::new();
    let mut displayed_tables = HashSet::new();
    for (table_idx, initial_table) in result.iter().enumerate() {
        if !displayed_tables.insert(table_idx) {
            // already displayed
            continue;
        }
        // the first line always has only this table
        let initial_line = TableMetadataLine::new(table_idx, initial_table);
        let mut display = TableMetadataDisplayInfo {
            database_name: initial_table.database_name.clone(),
            schema_name: initial_table.schema_name.clone(),
            render_height: initial_line.render_height,
            render_width: initial_line.render_width,
            display_lines: vec![initial_line],
        };

        // check for each following table whether it can be co-located
        // (like C++ this does not skip tables an earlier display already placed, so a small table that
        // fits next to two big tables that do not fit next to each other is drawn twice)
        for (next_idx, current_table) in result.iter().enumerate().skip(table_idx + 1) {
            let render_width = current_table.render_width;
            let render_height = current_table.line_count();
            // a table taller than the display never fits
            if render_height > display.render_height {
                continue;
            }
            // prefer adding it below the tables of an existing line
            let mut added = false;
            for existing_line in &mut display.display_lines {
                if existing_line.render_height + render_height > display.render_height {
                    // does not fit
                    continue;
                }
                // it fits height-wise, but the line may need to be stretched
                let new_max_component_widths: Vec<usize> = existing_line
                    .max_component_widths
                    .iter()
                    .enumerate()
                    .map(|(i, &width)| width.max(current_table.max_component_widths.get(i).copied().unwrap_or(0)))
                    .collect();
                let new_column_render_width = 3 + new_max_component_widths.iter().map(|w| w + 1).sum::<usize>();
                let new_rendering_width = render_width
                    .max(existing_line.render_width)
                    .max(new_column_render_width);
                let extra_width = new_rendering_width - existing_line.render_width;
                if display.render_width + extra_width > max_render_width {
                    // the extra width exceeds the limit
                    continue;
                }
                existing_line.max_component_widths = new_max_component_widths;
                existing_line.render_width += extra_width;
                existing_line.render_height += render_height;
                existing_line.tables.push(next_idx);
                added = true;
                break;
            }
            // otherwise add a new line, if it fits width-wise
            if !added && display.render_width + render_width <= max_render_width {
                display
                    .display_lines
                    .push(TableMetadataLine::new(next_idx, current_table));
                display.render_width += render_width;
                added = true;
            }
            if added {
                displayed_tables.insert(next_idx);
            }
        }
        display
            .display_lines
            .sort_by_key(|line| std::cmp::Reverse(line.render_height));
        displays.push(display);
    }
    displays
}

/// `ShellState::RenderTableMetadata`: the tables drawn as boxes, grouped by database and schema.
pub fn render_table_metadata(config: &MetadataConfig, tables: &[TableInfo]) -> String {
    if config.compact {
        return render_table_metadata_compact(tables);
    }
    let max_render_width = config.max_width;
    // measure each table, truncating it if it exceeds the max render width, and split up large tables
    let mut table_list: Vec<TableRenderInfo> = tables.iter().map(TableRenderInfo::new).collect();
    for table in &mut table_list {
        table.truncate(max_render_width);
    }
    for table in &mut table_list {
        table.split(max_render_width);
    }

    // group the tables by database and schema
    let mut grouped_tables: BTreeMap<String, BTreeMap<String, Vec<TableRenderInfo>>> = BTreeMap::new();
    for entry in table_list {
        grouped_tables
            .entry(entry.database_name.clone())
            .or_default()
            .entry(entry.schema_name.clone())
            .or_default()
            .push(entry);
    }

    // for each schema, sort from the tallest to the shortest table and pack them into displays
    let mut metadata_displays = Vec::new();
    for schemas in grouped_tables.values_mut() {
        for result in schemas.values_mut() {
            result.sort_by_key(|table| std::cmp::Reverse(table.line_count()));
            metadata_displays.extend(build_displays(result, max_render_width));
        }
    }

    // (C++ sets up the pager here, if the sum of the displays' render heights calls for it)

    // the width of the database and schema headings
    let mut metadata_render_width = 0;
    for display in &metadata_displays {
        let name_width = render_width(&display.database_name).max(render_width(&display.schema_name));
        metadata_render_width = metadata_render_width
            .max(max_render_width.min(name_width + 6))
            .max(display.render_width);
    }

    let mut out = Output::default();
    let mut last_displayed_database = "";
    let mut last_displayed_schema = "";
    for display in &metadata_displays {
        // the database and/or schema heading for this batch of tables
        if !display.database_name.is_empty() && last_displayed_database != display.database_name {
            render_line_display(
                &mut out,
                &display.database_name,
                metadata_render_width,
                HighlightElementType::DatabaseName,
            );
            last_displayed_database = &display.database_name;
            last_displayed_schema = "";
        }
        if !display.schema_name.is_empty() && last_displayed_schema != display.schema_name {
            render_line_display(
                &mut out,
                &display.schema_name,
                metadata_render_width,
                HighlightElementType::SchemaName,
            );
            last_displayed_schema = &display.schema_name;
        }
        let group_table_list = &grouped_tables[&display.database_name][&display.schema_name];
        for line_idx in 0..display.render_height {
            for (table_line_idx, display_line) in display.display_lines.iter().enumerate() {
                let is_last = table_line_idx + 1 == display.display_lines.len();
                display_line.render_line(&mut out, group_table_list, line_idx, is_last);
            }
            out.print("\n", HighlightElementType::Layout);
        }
    }
    out.text
}

#[cfg(test)]
#[path = "table_metadata/tests.rs"]
mod tests;
