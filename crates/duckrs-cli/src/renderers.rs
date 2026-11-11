// Ported from duckdb/tools/shell/shell_renderer.cpp @ rust-port-baseline
//
//! The shell's output modes other than duckbox: each draws a result from its values cast to text.

use crate::{RenderMode, Shell};
use duckrs::{LogicalType, LogicalTypeId, QueryResult, Value};

const BOX_24: &str = "─";
const BOX_23: &str = "┌";
const BOX_34: &str = "┐";
const BOX_12: &str = "└";
const BOX_14: &str = "┘";
const BOX_123: &str = "├";
const BOX_134: &str = "┤";
const BOX_234: &str = "┬";
const BOX_124: &str = "┴";
const BOX_1234: &str = "┼";

/// One cell of a result as the renderers see it: its text, or `None` for NULL.
type Cell = Option<String>;

/// `RenderingQueryResult` once converted: names, types and the cells of each row.
pub struct Converted {
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub rows: Vec<Vec<Cell>>,
}

/// `ShellRenderer::ConvertChunk`: each value as `CAST(value AS VARCHAR)` gives it.
fn cast_to_text(value: &Value) -> Cell {
    if value.is_null() {
        return None;
    }
    Some(value.try_to_string().unwrap_or_else(|e| e.message))
}

/// `ModeJsonRenderer::ConvertChunk`: nested and floating values are cast to JSON instead.
fn json_text(value: &Value, ty: &LogicalType) -> Cell {
    if value.is_null() {
        return None;
    }
    let nested = ty.is_nested();
    let floating = matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double);
    if floating {
        // `CastToJSON` of a double: the JavaScript spellings of infinity and NaN
        match value.try_to_string().as_deref() {
            Ok("inf") => return Some("Infinity".into()),
            Ok("-inf") => return Some("-Infinity".into()),
            Ok("nan" | "-nan") => return Some("NaN".into()),
            _ => {}
        }
    }
    if (nested || floating)
        && let Ok(json) = duckrs_core::function::json::value_to_json(value)
    {
        let mut out = String::new();
        json.write(&mut out);
        return Some(out);
    }
    cast_to_text(value)
}

pub fn convert(result: &QueryResult, json: bool) -> Converted {
    let mut rows = Vec::with_capacity(result.row_count());
    for row in 0..result.row_count() {
        let cells = (0..result.column_count())
            .map(|column| {
                let value = result.value(column, row);
                if json {
                    json_text(&value, &result.types[column])
                } else {
                    cast_to_text(&value)
                }
            })
            .collect();
        rows.push(cells);
    }
    Converted {
        names: result.names.clone(),
        types: result.types.clone(),
        rows,
    }
}

/// `ShellState::RenderLength`: the width of text on screen.
pub fn render_length(text: &str) -> usize {
    crate::box_renderer::render_width(text)
}

fn aligned(out: &mut String, text: &str, width: usize, right: bool) {
    let length = render_length(text);
    let padding = width.saturating_sub(length);
    if right {
        out.extend(std::iter::repeat_n(' ', padding));
        out.push_str(text);
    } else {
        out.push_str(text);
        out.extend(std::iter::repeat_n(' ', padding));
    }
}

/// `RenderAlignedValue` with its default, `TextAlignment::CENTER`.
fn centered(out: &mut String, text: &str, width: usize) {
    let padding = width.saturating_sub(render_length(text));
    out.extend(std::iter::repeat_n(' ', padding / 2));
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', padding.div_ceil(2)));
}

fn dashes(out: &mut String, count: usize, dash: &str) {
    for _ in 0..count {
        out.push_str(dash);
    }
}

/// `ShellState::EscapeCString`: bytes that are not printable ASCII become octal escapes.
pub fn escape_c_string(text: &str) -> String {
    let mut result = String::from("\"");
    for c in text.bytes() {
        match c {
            b'\\' => result.push_str("\\\\"),
            b'"' => result.push_str("\\\""),
            b'\t' => result.push_str("\\t"),
            b'\n' => result.push_str("\\n"),
            b'\r' => result.push_str("\\r"),
            0x20..=0x7e => result.push(c as char),
            c => result.push_str(&format!("\\{c:03o}")),
        }
    }
    result.push('"');
    result
}

/// `PrintStream::OutputQuotedString`: a SQL string literal.
pub fn quoted_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `PrintStream::OutputQuotedIdentifier`.
pub fn quoted_identifier(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !duckrs_parser::keywords::is_reserved(text);
    if plain {
        text.to_string()
    } else {
        format!("\"{}\"", text.replace('"', "\"\""))
    }
}

/// `ModeJsonRenderer::EscapeJSONString`.
fn escape_json(text: &str) -> String {
    let mut result = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' | '"' => {
                result.push('\\');
                result.push(c);
            }
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if (c as u32) <= 0x1f => result.push_str(&format!("\\u{:04x}", c as u32)),
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

fn html_escape(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        match c {
            '<' => escaped.push_str("&lt;"),
            '&' => escaped.push_str("&amp;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// `ModeInsertRenderer::EscapeNewlines`.
fn escape_newlines(text: &str) -> String {
    let needs_concat = text.contains(['\n', '\r', '\0']);
    let mut res = String::from(if needs_concat { "concat('" } else { "'" });
    for c in text.chars() {
        match c {
            '\n' => res.push_str("', chr(10), '"),
            '\r' => res.push_str("', chr(13), '"),
            '\0' => res.push_str("', chr(0), '"),
            '\'' => res.push_str("''"),
            c => res.push(c),
        }
    }
    res.push('\'');
    if needs_concat {
        res.push(')');
    }
    res
}

/// `ModeCsvRenderer::PrintEscapedCSV`.
fn escaped_csv(text: &str, separator: &str) -> String {
    let needs_quote = text.bytes().any(|c| c <= 31 || c == b'"' || c == b'\'' || c >= 127)
        || (!separator.is_empty() && text.contains(separator));
    if needs_quote {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn is_boolean(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Boolean
}

/// `ShellState::ColumnTypeIsInteger` on the type name.
fn is_integer_type(ty: &LogicalType) -> bool {
    matches!(
        ty.id(),
        LogicalTypeId::TinyInt | LogicalTypeId::SmallInt | LogicalTypeId::Integer | LogicalTypeId::BigInt
    )
}

/// Draws `result` in `mode` (anything but duckbox, describe and trash).
pub fn render(shell: &Shell, mode: RenderMode, result: &QueryResult) -> String {
    let converted = convert(result, matches!(mode, RenderMode::Json | RenderMode::JsonLines));
    match mode {
        RenderMode::Column | RenderMode::Table | RenderMode::Markdown | RenderMode::Box | RenderMode::Latex => {
            render_columns(shell, mode, converted)
        }
        _ => render_rows(shell, mode, &converted),
    }
}

fn render_columns(shell: &Shell, mode: RenderMode, mut c: Converted) -> String {
    let null_value = shell.null_value.clone();
    // `HasConvertValue` / `ConvertValue`
    let convert_text = |text: &str| -> String {
        match mode {
            RenderMode::Box => text.replace('\n', "\\n"),
            RenderMode::Markdown => text.replace('|', "\\|"),
            _ => text.to_string(),
        }
    };
    for name in &mut c.names {
        *name = convert_text(name);
    }
    for row in &mut c.rows {
        for text in row.iter_mut().flatten() {
            *text = convert_text(text);
        }
    }
    // `ColumnRenderer::Analyze`
    let column_count = c.names.len();
    let mut width = Vec::with_capacity(column_count);
    let mut right_align = Vec::with_capacity(column_count);
    for column in 0..column_count {
        let mut w = shell.col_width.get(column).copied().unwrap_or(0);
        right_align.push(w < 0);
        w = w.abs();
        width.push((w as usize).max(render_length(&c.names[column])));
    }
    for row in &c.rows {
        for (column, cell) in row.iter().enumerate() {
            let w = render_length(cell.as_deref().unwrap_or(&null_value));
            width[column] = width[column].max(w);
        }
    }
    let mut out = String::new();
    let (row_start, col_sep, row_sep) = match mode {
        RenderMode::Column => ("", "  ", "\n"),
        RenderMode::Table => ("| ", " | ", " |\n"),
        RenderMode::Markdown => ("| ", " | ", " |\n"),
        RenderMode::Box => ("│ ", " │ ", " │\n"),
        _ => ("", " & ", " \\\\\n"),
    };
    let separator_line = |out: &mut String, left: &str, middle: &str, right: &str, dash: &str| {
        if column_count > 0 {
            out.push_str(left);
            for (i, w) in width.iter().enumerate() {
                if i > 0 {
                    out.push_str(middle);
                }
                dashes(out, w + 2, dash);
            }
            out.push_str(right);
        }
        out.push('\n');
    };
    // the header
    match mode {
        RenderMode::Column => {
            if shell.show_header {
                for i in 0..column_count {
                    aligned(&mut out, &c.names[i], width[i], right_align[i]);
                    out.push_str(if i == column_count - 1 { "\n" } else { "  " });
                }
                for (i, w) in width.iter().enumerate() {
                    dashes(&mut out, *w, "-");
                    out.push_str(if i == column_count - 1 { "\n" } else { "  " });
                }
            }
        }
        RenderMode::Table => {
            separator_line(&mut out, "+", "+", "+", "-");
            out.push_str("| ");
            for (i, (name, w)) in c.names.iter().zip(&width).enumerate() {
                centered(&mut out, name, *w);
                out.push_str(if i == column_count - 1 { " |\n" } else { " | " });
            }
            separator_line(&mut out, "+", "+", "+", "-");
        }
        RenderMode::Markdown => {
            out.push_str(row_start);
            for (i, (name, w)) in c.names.iter().zip(&width).enumerate() {
                if i > 0 {
                    out.push_str(col_sep);
                }
                centered(&mut out, name, *w);
            }
            out.push_str(row_sep);
            if column_count > 0 {
                for (ty, w) in c.types.iter().zip(&width) {
                    out.push('|');
                    if ty.is_numeric() {
                        dashes(&mut out, w + 1, "-");
                        out.push(':');
                    } else {
                        dashes(&mut out, (w + 2).max(3), "-");
                    }
                }
                out.push('|');
            }
            out.push('\n');
        }
        RenderMode::Box => {
            separator_line(&mut out, BOX_23, BOX_234, BOX_34, BOX_24);
            out.push_str("│ ");
            for (i, (name, w)) in c.names.iter().zip(&width).enumerate() {
                centered(&mut out, name, *w);
                out.push_str(if i == column_count - 1 { " │\n" } else { " │ " });
            }
            separator_line(&mut out, BOX_123, BOX_1234, BOX_134, BOX_24);
        }
        _ => {
            out.push_str("\\begin{tabular}{|");
            for ty in &c.types {
                out.push(if is_integer_type(ty) { 'r' } else { 'l' });
            }
            out.push_str("|}\n\\hline\n");
            for (i, (name, w)) in c.names.iter().zip(&width).enumerate() {
                centered(&mut out, name, *w);
                out.push_str(if i == column_count - 1 { row_sep } else { col_sep });
            }
            out.push_str("\\hline\n");
        }
    }
    // the rows (`ColumnRenderer::RenderRow`)
    for row in &c.rows {
        out.push_str(row_start);
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str(col_sep);
            }
            aligned(
                &mut out,
                cell.as_deref().unwrap_or(&null_value),
                width[i],
                right_align[i],
            );
        }
        out.push_str(row_sep);
    }
    // the footer
    match mode {
        RenderMode::Table => separator_line(&mut out, "+", "+", "+", "-"),
        RenderMode::Box => separator_line(&mut out, BOX_12, BOX_124, BOX_14, BOX_24),
        RenderMode::Latex => out.push_str("\\hline\n\\end{tabular}\n"),
        _ => {}
    }
    out
}

fn render_rows(shell: &Shell, mode: RenderMode, c: &Converted) -> String {
    let mut out = String::new();
    let col_sep = shell.col_separator.as_str();
    let row_sep = shell.row_separator.as_str();
    let null_text = match mode {
        RenderMode::Quote => "NULL".to_string(),
        RenderMode::Json | RenderMode::JsonLines => "null".to_string(),
        _ => shell.null_value.clone(),
    };
    let text = |cell: &Cell| -> String { cell.clone().unwrap_or_else(|| null_text.clone()) };
    // the header
    let header: Option<Vec<String>> = match mode {
        RenderMode::List | RenderMode::Ascii => Some(c.names.clone()),
        RenderMode::Tcl => Some(c.names.iter().map(|n| escape_c_string(n)).collect()),
        RenderMode::Csv => Some(c.names.iter().map(|n| escaped_csv(n, col_sep)).collect()),
        RenderMode::Quote => Some(c.names.iter().map(|n| quoted_string(n)).collect()),
        _ => None,
    };
    if let Some(names) = header.filter(|_| shell.show_header) {
        out.push_str(&names.join(col_sep));
        out.push_str(row_sep);
    }
    if mode == RenderMode::Html && shell.show_header {
        out.push_str("<tr>");
        for name in &c.names {
            out.push_str(&format!("<th>{}</th>\n", html_escape(name)));
        }
        out.push_str("</tr>\n");
    }
    if mode == RenderMode::Json {
        out.push('[');
    }
    let header_width = c.names.iter().map(String::len).max().unwrap_or(0).max(5);
    for (index, row) in c.rows.iter().enumerate() {
        match mode {
            RenderMode::Line => {
                if index > 0 {
                    out.push_str(row_sep);
                }
                for (i, cell) in row.iter().enumerate() {
                    let name = &c.names[i];
                    out.extend(std::iter::repeat_n(' ', header_width.saturating_sub(name.len())));
                    out.push_str(name);
                    out.push_str(" = ");
                    out.push_str(&text(cell));
                    out.push_str(row_sep);
                }
            }
            RenderMode::List | RenderMode::Ascii => {
                let cells: Vec<String> = row.iter().map(&text).collect();
                out.push_str(&cells.join(col_sep));
                out.push_str(row_sep);
            }
            RenderMode::Html => {
                out.push_str("<tr>");
                for cell in row {
                    out.push_str(&format!("<td>{}</td>\n", html_escape(&text(cell))));
                }
                out.push_str("</tr>\n");
            }
            RenderMode::Tcl => {
                let cells: Vec<String> = row.iter().map(|cell| escape_c_string(&text(cell))).collect();
                out.push_str(&cells.join(col_sep));
                out.push_str(row_sep);
            }
            RenderMode::Csv => {
                let cells: Vec<String> = row.iter().map(|cell| escaped_csv(&text(cell), col_sep)).collect();
                out.push_str(&cells.join(col_sep));
                out.push_str(row_sep);
            }
            RenderMode::Quote => {
                let cells: Vec<String> = row
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| match cell {
                        Some(t) if !c.types[i].is_numeric() && !is_boolean(&c.types[i]) => quoted_string(t),
                        _ => text(cell),
                    })
                    .collect();
                out.push_str(&cells.join(col_sep));
                out.push_str(row_sep);
            }
            RenderMode::Json | RenderMode::JsonLines => {
                if index > 0 {
                    if mode == RenderMode::Json {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push('{');
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&escape_json(&c.names[i]));
                    out.push(':');
                    let ty = &c.types[i];
                    match cell {
                        None => out.push_str("null"),
                        Some(t) if ty.is_nested() || ty.alias() == Some("JSON") => out.push_str(t),
                        Some(t) if !json_requires_quotes(ty) => out.push_str(t),
                        Some(t) => out.push_str(&escape_json(t)),
                    }
                }
                out.push('}');
            }
            RenderMode::Insert => {
                let mut line = format!("INSERT INTO {}", shell.dest_table);
                if shell.show_header {
                    let names: Vec<String> = c.names.iter().map(|n| quoted_identifier(n)).collect();
                    line.push_str(&format!("({})", names.join(",")));
                }
                for (i, cell) in row.iter().enumerate() {
                    line.push_str(if i > 0 { "," } else { " VALUES(" });
                    match cell {
                        None => line.push_str("NULL"),
                        Some(t) if c.types[i].is_numeric() => line.push_str(t),
                        Some(t) if shell.newlines => line.push_str(&quoted_string(t)),
                        Some(t) => line.push_str(&escape_newlines(t)),
                    }
                }
                line.push_str(");\n");
                out.push_str(&line);
            }
            _ => {}
        }
    }
    // the footer
    match mode {
        RenderMode::Json => out.push_str("]\n"),
        RenderMode::JsonLines => out.push('\n'),
        _ => {}
    }
    out
}

/// `ModeJsonRenderer::RequiresQuotes`.
fn json_requires_quotes(ty: &LogicalType) -> bool {
    if is_boolean(ty) {
        return false;
    }
    if !ty.is_numeric() {
        return true;
    }
    matches!(
        ty.id(),
        LogicalTypeId::UBigInt | LogicalTypeId::HugeInt | LogicalTypeId::UHugeInt | LogicalTypeId::Decimal
    )
}
