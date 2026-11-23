// Ported from duckdb/src/common/tree_renderer/text_tree_renderer.cpp @ rust-port-baseline
//
//! `TextTreeRenderer`: the boxes of EXPLAIN. Each operator is a box with its name in the top border
//! and its details inside, its first child stacks under it and the others stand to the right of the
//! first, joined by a bus line (`├────╮`).
//!
//! C++ also folds operators that took little time into grouped boxes when `expand_all` is off and it
//! has timings, and colours the text. EXPLAIN has no timings, so only the full boxes are ported.

use duckrs_common::render_width::{char_width, render_width};

/// `ExplainTreeNode`.
pub(super) struct TreeNode {
    pub name: String,
    pub details: Vec<(String, Vec<String>)>,
    /// the rows the operator produced, which only EXPLAIN ANALYZE knows
    pub rows: Option<u64>,
    pub estimated_rows: Option<u64>,
    pub children: Vec<TreeNode>,
}

/// The `TextTreeRendererConfig` that `profiling_renderer_settings` can change.
pub(super) struct TextTreeConfig {
    /// `operator_casing = 'upper'`: HASH_JOIN instead of Hash Join
    pub upper_case: bool,
    pub thousand_separator: Option<char>,
}

impl Default for TextTreeConfig {
    fn default() -> Self {
        TextTreeConfig {
            upper_case: false,
            thousand_separator: Some(','),
        }
    }
}

/// `ExplainSpan`, without the type that colours it.
#[derive(Clone)]
struct Span {
    text: String,
    width: usize,
}

impl Span {
    fn new(text: impl Into<String>) -> Span {
        let text = text.into();
        let width = render_width(&text);
        Span { text, width }
    }

    fn set_text(&mut self, text: String) {
        self.width = render_width(&text);
        self.text = text;
    }
}

type Line = Vec<Span>;

/// `ExplainBlock`: lines of the same block; `spine` is where a parent connects down into it.
struct Block {
    lines: Vec<Line>,
    width: usize,
    spine: usize,
    output_rows: Option<u64>,
}

/// `RenderLength` of one code point.
fn code_point_width(c: char) -> usize {
    char_width(c as u32)
}

fn line_width(line: &Line) -> usize {
    line.iter().map(|span| span.width).sum()
}

fn offset_line(line: &mut Line, offset: usize) {
    if offset > 0 {
        line.insert(0, Span::new(" ".repeat(offset)));
    }
}

fn pad_line(line: &mut Line, width: usize) {
    let current = line_width(line);
    if current < width {
        line.push(Span::new(" ".repeat(width - current)));
    }
}

fn leading_spaces(line: &Line) -> usize {
    let mut count = 0;
    for span in line {
        for c in span.text.bytes() {
            if c != b' ' {
                return count;
            }
            count += 1;
        }
    }
    count
}

/// `AppendAt`: `src` after `dst`, with its first character that is not a space at `content_col`.
fn append_at(dst: &mut Line, src: &Line, content_col: usize) {
    pad_line(dst, content_col);
    let to_skip = leading_spaces(src);
    let mut skipped = 0;
    for span in src {
        if skipped >= to_skip {
            dst.push(span.clone());
            continue;
        }
        let bytes = span.text.as_bytes();
        let mut at = 0;
        while at < bytes.len() && bytes[at] == b' ' && skipped < to_skip {
            at += 1;
            skipped += 1;
        }
        if at < bytes.len() {
            dst.push(Span::new(&span.text[at..]));
        }
    }
}

/// `RenderCharEquals`: whether the character at render column `col` is `expected`.
fn char_equals(line: &Line, col: usize, expected: &str) -> bool {
    let mut offset = 0;
    for span in line {
        if col >= offset + span.width {
            offset += span.width;
            continue;
        }
        return span
            .text
            .chars()
            .nth(col - offset)
            .is_some_and(|c| expected.chars().eq(std::iter::once(c)));
    }
    false
}

/// `SetLayoutChar`: replaces the character at render column `col`.
fn set_layout_char(line: &mut Line, col: usize, replacement: &str) {
    let mut offset = 0;
    for span in line.iter_mut() {
        if col >= offset + span.width {
            offset += span.width;
            continue;
        }
        let target = col - offset;
        let (mut start, mut end) = (0, span.text.len());
        for (index, (byte, _)) in span.text.char_indices().enumerate() {
            if index == target {
                start = byte;
            } else if index == target + 1 {
                end = byte;
                break;
            }
        }
        let text = format!("{}{replacement}{}", &span.text[..start], &span.text[end..]);
        span.set_text(text);
        return;
    }
}

/// `ThickDownVariant`.
fn thick_down_variant(glyph: &'static str) -> &'static str {
    match glyph {
        "┬" => "┰",
        "╮" => "┒",
        "╭" => "┎",
        "├" => "┟",
        "┤" => "┧",
        "┼" => "╁",
        other => other,
    }
}

/// `TierCap`.
fn tier_cap(tier: usize, top: bool, right: bool) -> &'static str {
    match (tier, top, right) {
        (0, true, _) => "┬",
        (0, false, _) => "┴",
        (1, true, _) => "┰",
        (1, false, _) => "┸",
        (_, true, false) => "┒",
        (_, true, true) => "┎",
        (_, false, false) => "┚",
        (_, false, true) => "┖",
    }
}

/// `StringUtil::Title`.
fn title(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut first = true;
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            result.push(if first {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            });
            first = false;
        } else {
            first = true;
            result.push(c);
        }
    }
    result
}

/// `DisplayName`: HASH_JOIN is Hash Join, unless `operator_casing` asks for the name as it is.
fn display_name(name: &str, upper_case: bool) -> String {
    if upper_case {
        return name.to_string();
    }
    title(&name.replace('_', " "))
        .replace("Cte", "CTE")
        .replace("Ie Join", "IE Join")
}

/// `StripOuterParens`: "(a >= 1)" is "a >= 1".
pub(super) fn strip_outer_parens(mut text: String) -> String {
    loop {
        let bytes = text.as_bytes();
        if bytes.len() < 2 || bytes[0] != b'(' || bytes[bytes.len() - 1] != b')' {
            return text;
        }
        let mut depth = 0usize;
        let mut in_quote = false;
        let mut wraps = true;
        for (at, &c) in bytes.iter().enumerate() {
            if c == b'\'' {
                in_quote = !in_quote;
                continue;
            }
            if in_quote {
                continue;
            }
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                if depth == 0 {
                    wraps = false;
                    break;
                }
                depth -= 1;
                if depth == 0 && at + 1 != bytes.len() {
                    wraps = false;
                    break;
                }
            }
        }
        if !wraps || depth != 0 || in_quote {
            return text;
        }
        text = text[1..text.len() - 1].to_string();
    }
}

/// `StringUtil::Split(value, "\n")`: the lines that are not empty, or the value if there are none.
pub(super) fn split_lines(value: &str) -> Vec<String> {
    let lines: Vec<String> = value
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        vec![value.to_string()]
    } else {
        lines
    }
}

/// `FormatCount`.
fn format_count(count: u64, separator: Option<char>) -> String {
    let digits = count.to_string();
    let Some(separator) = separator else {
        return digits;
    };
    let mut result = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            result.push(separator);
        }
        result.push(digit);
    }
    result
}

/// `TruncateText`.
fn truncate_text(text: &str, max_width: usize) -> String {
    if render_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut width = 0;
    for c in text.chars() {
        let w = code_point_width(c);
        if width + w > max_width - 1 {
            break;
        }
        result.push(c);
        width += w;
    }
    result + "…"
}

/// `TruncateHead`: keeps the end, as the last part of a qualified name is the table.
fn truncate_head(text: &str, max_width: usize) -> String {
    if render_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let budget = max_width - 1;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut width = 0;
    let mut start = text.len();
    for &(byte, c) in chars.iter().rev() {
        let w = code_point_width(c);
        if width + w > budget {
            break;
        }
        width += w;
        start = byte;
    }
    format!("…{}", &text[start..])
}

/// `CutToWidth`.
fn cut_to_width(text: &str, width: usize) -> String {
    if render_width(text) <= width {
        return text.to_string();
    }
    let mut result = String::new();
    let mut rendered = 0;
    for c in text.chars() {
        let w = code_point_width(c);
        if rendered + w > width {
            break;
        }
        result.push(c);
        rendered += w;
    }
    result
}

/// `WordWrap`: breaks at spaces.
fn word_wrap(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(2);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for word in text.split(' ') {
        if word.is_empty() {
            continue;
        }
        let word = if render_width(word) > budget {
            truncate_text(word, budget)
        } else {
            word.to_string()
        };
        let word_width = render_width(&word);
        let needed = word_width + usize::from(!line.is_empty());
        if line_width + needed <= budget {
            if !line.is_empty() {
                line.push(' ');
                line_width += 1;
            }
            line.push_str(&word);
            line_width += word_width;
        } else {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            line = word;
            line_width = word_width;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `WrapValues`: `a, b, c` over as many lines of `budget` as they need, up to `max_lines`.
fn wrap_values(values: &[String], budget: usize, max_lines: usize) -> Vec<String> {
    let budget = budget.max(2);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for (at, value) in values.iter().enumerate() {
        let piece = if at + 1 < values.len() {
            format!("{value},")
        } else {
            value.clone()
        };
        let piece_width = render_width(&piece);
        let separator = usize::from(!current.is_empty());
        if current_width + separator + piece_width <= budget {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&piece);
            current_width += separator + piece_width;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if piece_width <= budget {
            current = piece;
            current_width = piece_width;
        } else {
            let mut wrapped = word_wrap(&piece, budget);
            current = wrapped.pop().unwrap_or_default();
            current_width = render_width(&current);
            lines.extend(wrapped);
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().expect("max_lines is not 0");
        if render_width(last) + 2 > budget {
            *last = cut_to_width(last, budget.saturating_sub(2));
        }
        last.push_str(if last.is_empty() { "…" } else { " …" });
    }
    lines
}

fn longest_value(values: &[String]) -> usize {
    values.iter().map(|v| render_width(v)).max().unwrap_or(0)
}

const HORIZONTAL_GAP: usize = 2;
const MAX_COMPACT_DETAIL: usize = 40;
const NARROW_MIN_WIDTH: usize = 30;
const MINIMUM_RENDER_WIDTH: usize = 30;
const MAX_TREE_WIDTH: usize = 400;
const MAXIMUM_RENDER_WIDTH: usize = 240;
const TIER1_FRACTION: f64 = 0.33;
const TIER2_FRACTION: f64 = 0.66;

/// `DetailIsNarrow`: a detail too wide for `Key: value` has its key on a line of its own.
fn detail_is_narrow(key: &str, values: &[String]) -> bool {
    if key == "Text" || key == "Table" {
        return false;
    }
    render_width(key) + 2 + longest_value(values) > MAX_COMPACT_DETAIL
}

fn detail_width(key: &str, values: &[String], max_content: usize) -> usize {
    let longest = longest_value(values) + usize::from(values.len() > 1);
    if !detail_is_narrow(key, values) {
        let key_width = if key == "Text" { 0 } else { render_width(key) + 2 };
        return key_width + longest;
    }
    render_width(key).max(longest.min(max_content)).max(NARROW_MIN_WIDTH)
}

fn max_node_rows(node: &TreeNode) -> u64 {
    node.children
        .iter()
        .map(max_node_rows)
        .fold(node.rows.unwrap_or(0), u64::max)
}

/// `ExplainBoxRenderer`, with `expand_all` set and the left alignment `TextTreeRenderer` uses.
struct BoxRenderer<'a> {
    config: &'a TextTreeConfig,
    layout_width: usize,
    max_edge_rows: u64,
}

impl BoxRenderer<'_> {
    fn max_content_width(&self) -> usize {
        if self.layout_width >= 4 {
            self.layout_width - 4
        } else {
            self.layout_width
        }
    }

    /// `EdgeTier`: how heavy the line is, as a share of the most rows any operator produced.
    fn edge_tier(&self, rows: Option<u64>) -> usize {
        let Some(rows) = rows else {
            return 0;
        };
        if self.max_edge_rows == 0 {
            return 0;
        }
        let fraction = rows as f64 / self.max_edge_rows as f64;
        if fraction >= TIER2_FRACTION {
            2
        } else if fraction >= TIER1_FRACTION {
            1
        } else {
            0
        }
    }

    fn tier_width(tier: usize) -> usize {
        if tier >= 2 { 2 } else { 1 }
    }

    fn draw_cap(border: &mut Line, col: usize, tier: usize, top: bool) {
        if char_equals(border, col, "─") {
            set_layout_char(border, col, tier_cap(tier, top, false));
        }
        if Self::tier_width(tier) == 2 && char_equals(border, col + 1, "─") {
            set_layout_char(border, col + 1, tier_cap(tier, top, true));
        }
    }

    /// `BuildMetrics`: the left side; EXPLAIN has no timings for the right.
    fn metrics(&self, node: &TreeNode) -> Option<String> {
        let unit = |count: u64| if count == 1 { " row" } else { " rows" };
        let separator = self.config.thousand_separator;
        if let Some(rows) = node.rows {
            Some(format_count(rows, separator) + unit(rows))
        } else {
            node.estimated_rows
                .map(|estimate| format!("~{}{}", format_count(estimate, separator), unit(estimate)))
        }
    }

    fn box_content_width(&self, node: &TreeNode) -> usize {
        let max_content = self.max_content_width();
        let mut width = render_width(&display_name(&node.name, self.config.upper_case)) + 2;
        for (key, values) in &node.details {
            width = width.max(detail_width(key, values, max_content));
        }
        width = width.max(self.metrics(node).map_or(0, |m| render_width(&m)));
        width.min(max_content).max(6)
    }

    /// `ChainWidth`: the widest box of a node and its first children, which all have that width.
    fn chain_width(&self, node: &TreeNode) -> usize {
        let mut width = 6;
        let mut current = node;
        loop {
            width = width.max(self.box_content_width(current));
            match current.children.first() {
                Some(child) => current = child,
                None => return width,
            }
        }
    }

    fn build_box(&self, node: &TreeNode, content_width: usize) -> Block {
        let mut content: Vec<Line> = Vec::new();
        for (key, values) in &node.details {
            let has_key = key != "Text";
            if key == "Table" && !values.is_empty() {
                let label = "Table: ";
                let table = truncate_head(&values[0], content_width.saturating_sub(render_width(label)));
                content.push(vec![Span::new(label), Span::new(table)]);
                continue;
            }
            if has_key && detail_is_narrow(key, values) {
                content.push(vec![Span::new(truncate_text(&format!("{key}:"), content_width))]);
                for line in wrap_values(values, content_width, usize::MAX) {
                    content.push(vec![Span::new(line)]);
                }
                continue;
            }
            let mut label = if has_key { format!("{key}: ") } else { String::new() };
            let mut key_width = render_width(&label);
            if has_key && key_width >= content_width {
                label = truncate_text(&label, if content_width >= 1 { content_width - 1 } else { 1 });
                key_width = render_width(&label);
            }
            let value_lines = wrap_values(values, content_width.saturating_sub(key_width), usize::MAX);
            for (at, value) in value_lines.into_iter().enumerate() {
                let mut line = Vec::new();
                if at == 0 && has_key {
                    line.push(Span::new(label.clone()));
                } else if has_key {
                    line.push(Span::new(" ".repeat(key_width)));
                }
                line.push(Span::new(value));
                content.push(line);
            }
        }
        if let Some(metrics) = self.metrics(node) {
            content.push(vec![Span::new(metrics)]);
        }

        let name = display_name(&node.name, self.config.upper_case);
        let title = truncate_text(&name, if content_width >= 2 { content_width - 2 } else { 1 });
        let title_width = render_width(&title);
        let box_width = content_width + 4;

        let mut lines = Vec::with_capacity(content.len() + 2);
        lines.push(vec![
            Span::new("╭─ "),
            Span::new(title),
            Span::new(format!(" {}╮", "─".repeat(box_width - 5 - title_width))),
        ]);
        for line in content {
            let width = line_width(&line);
            let mut boxed = vec![Span::new("│ ")];
            boxed.extend(line);
            let padding = content_width.saturating_sub(width);
            boxed.push(Span::new(format!("{}│", " ".repeat(padding + 1))));
            lines.push(boxed);
        }
        lines.push(vec![Span::new(format!("╰{}╯", "─".repeat(box_width - 2)))]);
        Block {
            lines,
            width: box_width,
            spine: box_width / 2,
            output_rows: None,
        }
    }

    /// `StackChain`: a box over its only child.
    fn stack_chain(&self, mut parent: Block, mut child: Block) -> Block {
        let target = parent.spine.max(child.spine);
        let parent_offset = target - parent.spine;
        let child_offset = target - child.spine;
        let tier = self.edge_tier(child.output_rows);
        if let Some(bottom) = parent.lines.last_mut() {
            Self::draw_cap(bottom, parent.spine, tier, true);
        }
        if let Some(top) = child.lines.first_mut() {
            Self::draw_cap(top, child.spine, tier, false);
        }
        let mut lines = Vec::with_capacity(parent.lines.len() + child.lines.len());
        for mut line in parent.lines {
            offset_line(&mut line, parent_offset);
            lines.push(line);
        }
        for mut line in child.lines {
            offset_line(&mut line, child_offset);
            lines.push(line);
        }
        Block {
            lines,
            width: (parent_offset + parent.width).max(child_offset + child.width),
            spine: target,
            output_rows: None,
        }
    }

    /// `LayoutRow`, compact: each child as far left as the lines of the ones before it allow.
    fn layout_row(children: &[Block]) -> (Vec<Line>, Vec<usize>, usize) {
        let mut offsets = Vec::with_capacity(children.len());
        let mut spines = Vec::with_capacity(children.len());
        let mut profile: Vec<usize> = Vec::new();
        for child in children {
            let mut offset = 0;
            for (row, line) in child.lines.iter().enumerate().take(profile.len()) {
                if profile[row] == 0 {
                    continue;
                }
                let need = profile[row] + HORIZONTAL_GAP;
                let left = leading_spaces(line);
                if need > left {
                    offset = offset.max(need - left);
                }
            }
            offsets.push(offset);
            spines.push(offset + child.spine);
            for (row, line) in child.lines.iter().enumerate() {
                if row >= profile.len() {
                    profile.resize(row + 1, 0);
                }
                profile[row] = profile[row].max(offset + line_width(line));
            }
        }
        let row_width = profile.iter().copied().max().unwrap_or(0);
        let mut row_lines = Vec::with_capacity(profile.len());
        for row in 0..profile.len() {
            let mut line = Vec::new();
            for (child, &offset) in children.iter().zip(&offsets) {
                if let Some(child_line) = child.lines.get(row) {
                    append_at(&mut line, child_line, offset + leading_spaces(child_line));
                }
            }
            row_lines.push(line);
        }
        (row_lines, spines, row_width)
    }

    /// `AttachHorizontalLeft`: a box over its children, side by side, under a bus from the first.
    fn attach_horizontal(&self, mut parent: Block, children: &[Block]) -> Block {
        let (mut row_lines, spines, row_width) = Self::layout_row(children);
        let front = spines[0];
        let wide_first = Self::tier_width(self.edge_tier(children[0].output_rows)) == 2 && parent.spine == front;
        let bottom = parent.lines.last_mut().expect("a box has borders");
        if wide_first {
            set_layout_char(bottom, parent.spine, "┰");
            if char_equals(bottom, parent.spine + 1, "─") {
                set_layout_char(bottom, parent.spine + 1, "┰");
            }
        } else {
            set_layout_char(bottom, parent.spine, "┬");
        }

        let mut bus_end = *spines.last().expect("there are children");
        for (child, &spine) in children.iter().zip(&spines) {
            let tier = self.edge_tier(child.output_rows);
            if let Some(top) = row_lines.first_mut() {
                Self::draw_cap(top, spine, tier, false);
            }
            bus_end = bus_end.max(spine + Self::tier_width(tier) - 1);
        }

        let mut bus = vec!["─"; bus_end - front + 1];
        for (at, (child, &spine)) in children.iter().zip(&spines).enumerate() {
            let tier = self.edge_tier(child.output_rows);
            let first = at == 0;
            let last = at + 1 == children.len();
            if Self::tier_width(tier) == 2 {
                let (left, right) = if first && wide_first {
                    ("┃", "┠")
                } else if last {
                    ("┰", "┒")
                } else {
                    ("┒", "┎")
                };
                bus[spine - front] = left;
                bus[spine + 1 - front] = right;
                continue;
            }
            let base = if first {
                "├"
            } else if last {
                "╮"
            } else {
                "┬"
            };
            bus[spine - front] = if tier >= 1 { thick_down_variant(base) } else { base };
        }
        let mut routing = vec![Span::new(bus.concat())];
        offset_line(&mut routing, front);

        let width = parent.width.max(row_width).max(bus_end + 1);
        let spine = parent.spine;
        let mut lines = parent.lines;
        lines.push(routing);
        lines.extend(row_lines);
        Block {
            lines,
            width,
            spine,
            output_rows: None,
        }
    }

    fn render_node(&self, node: &TreeNode, chain_width: usize) -> Block {
        let mut block = if node.children.len() == 1 {
            self.stack_chain(
                self.build_box(node, chain_width),
                self.render_node(&node.children[0], chain_width),
            )
        } else {
            let parent = self.build_box(node, chain_width);
            if node.children.is_empty() {
                parent
            } else {
                let children: Vec<Block> = node
                    .children
                    .iter()
                    .enumerate()
                    .map(|(at, child)| {
                        let chain = if at == 0 { chain_width } else { self.chain_width(child) };
                        self.render_node(child, chain)
                    })
                    .collect();
                self.attach_horizontal(parent, &children)
            }
        };
        block.output_rows = node.rows;
        block
    }
}

/// `TextTreeRenderer::ToStreamInternal`.
pub(super) fn render(root: &TreeNode, config: &TextTreeConfig) -> String {
    let max_width = MAXIMUM_RENDER_WIDTH.max(MINIMUM_RENDER_WIDTH);
    let renderer = BoxRenderer {
        config,
        layout_width: max_width.max(MAX_TREE_WIDTH),
        max_edge_rows: max_node_rows(root),
    };
    let block = renderer.render_node(root, renderer.chain_width(root));
    let mut out = String::new();
    for line in block.lines {
        for span in line {
            out.push_str(&span.text);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, details: &[(&str, &[&str])], estimate: u64, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            name: name.to_string(),
            details: details
                .iter()
                .map(|(key, values)| (key.to_string(), values.iter().map(|v| v.to_string()).collect()))
                .collect(),
            rows: None,
            estimated_rows: Some(estimate),
            children,
        }
    }

    /// What C++ renders for the plan of a join of two filtered scans.
    #[test]
    fn side_by_side_children() {
        let left = node(
            "PROJECTION",
            &[("Projections", &["CAST(#0 AS TINYINT)"])],
            20,
            vec![node(
                "FILTER",
                &[("Expression", &["x <= 9"])],
                20,
                vec![node(
                    "SEQ_SCAN",
                    &[
                        ("Table", &["memory.main.t2"]),
                        ("Type", &["Sequential Scan"]),
                        ("Projections", &["x"]),
                        ("Filters", &["y > 5"]),
                    ],
                    20,
                    vec![],
                )],
            )],
        );
        let right = node(
            "PROJECTION",
            &[("Projections", &["CAST(#0 AS TINYINT)", "CAST(#1 AS TINYINT)"])],
            2,
            vec![node(
                "SEQ_SCAN",
                &[
                    ("Table", &["memory.main.t1"]),
                    ("Type", &["Sequential Scan"]),
                    ("Projections", &["a", "b"]),
                    ("Filters", &["b < 3"]),
                ],
                2,
                vec![],
            )],
        );
        let join = node(
            "HASH_JOIN",
            &[("Join Type", &["INNER"]), ("Conditions", &["x = a"])],
            0,
            vec![left, right],
        );
        let casts = node(
            "PROJECTION",
            &[(
                "Projections",
                &["CAST(#0 AS BIGINT)", "CAST(#1 AS BIGINT)", "CAST(#2 AS BIGINT)"],
            )],
            0,
            vec![join],
        );
        let root = node(
            "PROJECTION",
            &[("Projections", &["#0", "CAST(#1 AS TINYINT)", "#2"])],
            0,
            vec![casts],
        );
        let expected = "\
╭─ Projection ──────────────────────╮
│ Projections: #0,                  │
│              CAST(#1 AS TINYINT), │
│              #2                   │
│ ~0 rows                           │
╰─────────────────┬─────────────────╯
╭─ Projection ────┴─────────────────╮
│ Projections: CAST(#0 AS BIGINT),  │
│              CAST(#1 AS BIGINT),  │
│              CAST(#2 AS BIGINT)   │
│ ~0 rows                           │
╰─────────────────┬─────────────────╯
╭─ Hash Join ─────┴─────────────────╮
│ Join Type: INNER                  │
│ Conditions: x = a                 │
│ ~0 rows                           │
╰─────────────────┬─────────────────╯
                  ├──────────────────────────────────────╮
╭─ Projection ────┴─────────────────╮  ╭─ Projection ────┴─────────────────╮
│ Projections: CAST(#0 AS TINYINT)  │  │ Projections: CAST(#0 AS TINYINT), │
│ ~20 rows                          │  │              CAST(#1 AS TINYINT)  │
╰─────────────────┬─────────────────╯  │ ~2 rows                           │
╭─ Filter ────────┴─────────────────╮  ╰─────────────────┬─────────────────╯
│ Expression: x <= 9                │  ╭─ Seq Scan ──────┴─────────────────╮
│ ~20 rows                          │  │ Table: memory.main.t1             │
╰─────────────────┬─────────────────╯  │ Type: Sequential Scan             │
╭─ Seq Scan ──────┴─────────────────╮  │ Projections: a, b                 │
│ Table: memory.main.t2             │  │ Filters: b < 3                    │
│ Type: Sequential Scan             │  │ ~2 rows                           │
│ Projections: x                    │  ╰───────────────────────────────────╯
│ Filters: y > 5                    │
│ ~20 rows                          │
╰───────────────────────────────────╯
";
        assert_eq!(render(&root, &TextTreeConfig::default()), expected);
    }

    #[test]
    fn strips_outer_parens() {
        assert_eq!(strip_outer_parens("((a >= 1))".to_string()), "a >= 1");
        assert_eq!(strip_outer_parens("(a) OR (b)".to_string()), "(a) OR (b)");
        assert_eq!(strip_outer_parens("(')')".to_string()), "')'");
    }
}
